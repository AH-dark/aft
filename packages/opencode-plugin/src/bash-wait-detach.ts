import {
  type AftProjectTransport,
  type AftTransportPool,
  containsStandaloneDetachKeyword,
  shouldInterruptWaitsForMessage,
  stripDetachKeywordsAndTidyGap as stripStandaloneDetachKeywords,
} from "@cortexkit/aft-bridge";
import type { AftConfig } from "./config.js";
import { resolveBashConfig } from "./config.js";
import { debug, log, warn } from "./logger.js";
import { signalSyncWatchAbort } from "./sync-watch-abort.js";
import { BASH_TRANSPORT_TIMEOUT_MS } from "./tools/_shared.js";

type ActiveBridgePool = Pick<AftTransportPool, "getActiveBridgeForRoot" | "activeBridges"> &
  Partial<Pick<AftTransportPool, "getBridge">>;

const pendingOperations = new WeakMap<ActiveBridgePool, Map<string, Promise<void>>>();

/** OpenCode 1 transforms the message list before each model step, after queued input is included. */
export async function clearPendingBashWaitDetachForMessages(
  pool: ActiveBridgePool,
  output: { messages: { info: { sessionID: string } }[] },
): Promise<void> {
  await Promise.all(
    [...new Set(output.messages.map((message) => message.info.sessionID))].map((sessionID) =>
      clearPendingBashWaitDetach(pool, sessionID),
    ),
  );
}

function queuePendingOperation(
  pool: ActiveBridgePool,
  sessionID: string,
  run: () => Promise<void>,
): Promise<void> {
  let sessions = pendingOperations.get(pool);
  if (!sessions) pendingOperations.set(pool, (sessions = new Map()));
  const previous = sessions.get(sessionID);
  // Admission must not block the host, but its RPC must finish before the
  // next model step clears it; otherwise a late RPC could re-arm a stale turn.
  const operation = (previous ? previous.then(run, run) : run()).catch((error) => {
    warn(`[bash_wait_detach] request failed for session ${sessionID}: ${String(error)}`);
  });
  sessions.set(sessionID, operation);
  void operation.finally(() => {
    if (sessions.get(sessionID) === operation) sessions.delete(sessionID);
  });
  return operation;
}

/** Before the next model step, discard requests that no foreground wait has consumed. */
export async function clearPendingBashWaitDetach(
  pool: ActiveBridgePool,
  sessionID: string | undefined,
): Promise<void> {
  if (!sessionID) return;
  await queuePendingOperation(pool, sessionID, async () => {
    await Promise.all(
      pool.activeBridges().map(async (bridge) => {
        try {
          await bridge.send(
            "bash_wait_detach",
            { session_id: sessionID, clear_pending: true },
            { keepBridgeOnTimeout: true, transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS },
          );
        } catch (error) {
          warn(
            `[bash_wait_detach] delivery clear failed for session ${sessionID}: ${String(error)}`,
          );
        }
      }),
    );
  });
}

export { BASH_WAIT_DETACH_MAGIC_KEYWORD } from "@cortexkit/aft-bridge";

const EMPTY_DETACH_MESSAGE = "(requested background detach)";

type UserTextPart = { type?: unknown; text?: unknown; synthetic?: unknown; ignored?: unknown };

function userTextParts(output: unknown): Array<UserTextPart & { text: string }> {
  if (!output || typeof output !== "object") return [];
  const parts = (output as { parts?: unknown }).parts;
  if (!Array.isArray(parts)) return [];
  return parts.filter((part): part is UserTextPart & { text: string } => {
    if (!part || typeof part !== "object") return false;
    const textPart = part as UserTextPart;
    return (
      textPart.type === "text" &&
      typeof textPart.text === "string" &&
      textPart.synthetic !== true &&
      textPart.ignored !== true
    );
  });
}

/** Return the user-entered text parts from OpenCode's chat.message output. */
export function extractUserMessageText(output: unknown): string {
  return userTextParts(output)
    .map((part) => part.text)
    .join("\n");
}

/** Strip control tokens from mutable OpenCode text parts before model delivery. */
export function stripUserMessageDetachKeyword(output: unknown): string {
  const parts = userTextParts(output);
  const original = parts.map((part) => part.text).join("\n");
  if (!containsStandaloneDetachKeyword(original)) return original;

  const stripped = parts.map((part) => stripStandaloneDetachKeywords(part.text));
  if (stripped.join("\n").trim() === "") {
    if (parts[0]) parts[0].text = EMPTY_DETACH_MESSAGE;
    for (const part of parts.slice(1)) part.text = "";
    return EMPTY_DETACH_MESSAGE;
  }

  parts.forEach((part, index) => {
    part.text = stripped[index];
  });
  return stripped.join("\n");
}

/**
 * Decide whether a message should detach a foreground bash call and
 * abort a sync bash_watch. `messageText` is the operator-typed text from
 * `extractUserMessageText`, read before the keyword is stripped from it.
 */
export function shouldDetachBashWaitOnUserMessage(config: AftConfig, messageText: string): boolean {
  return shouldInterruptWaitsForMessage(
    resolveBashConfig(config).detach_on_user_message,
    messageText,
  );
}

/**
 * Handle OpenCode's chat.message output for the session's blocking waits:
 * decide, strip the `&detach` token from the mutable text parts, then (only
 * when the decision says so) abort any sync bash_watch and detach any
 * foreground bash. Both waits follow the same decision, so
 * `bash.detach_on_user_message: false` protects a sync bash_watch as well.
 * Returns whether the waits were interrupted.
 */
export function interruptBashWaitsForChatMessage(
  pool: ActiveBridgePool,
  config: AftConfig,
  projectRoot: string,
  sessionID: string | undefined,
  output: unknown,
  remember = true,
): boolean {
  // Decide before stripping: stripping removes the token the decision reads.
  const interrupt = shouldDetachBashWaitOnUserMessage(config, extractUserMessageText(output));
  stripUserMessageDetachKeyword(output);
  if (interrupt) {
    signalSyncWatchAbort(sessionID);
    if (sessionID && remember)
      void queuePendingOperation(pool, sessionID, () =>
        signalBashWaitDetachForProject(pool, projectRoot, sessionID, true),
      );
    else void signalBashWaitDetachForProject(pool, projectRoot, sessionID);
  }
  return interrupt;
}

async function sendBashWaitDetach(
  bridge: AftProjectTransport,
  sessionID: string,
  pending = false,
): Promise<boolean> {
  const response = await bridge.send(
    "bash_wait_detach",
    { session_id: sessionID, ...(pending ? { pending: true } : {}) },
    { keepBridgeOnTimeout: true, transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS },
  );
  if (response.success === false) {
    throw new Error(String(response.message ?? "bash_wait_detach failed"));
  }
  // success:true with detached:false means no active wait was found under
  // this session on that bridge — possibly the wrong bridge. Callers use
  // this to keep fanning out instead of treating delivery as done.
  return response.detached === true;
}

export async function signalBashWaitDetachForProject(
  pool: ActiveBridgePool,
  projectRoot: string,
  sessionID: string | undefined,
  pending = false,
): Promise<void> {
  if (!sessionID) return;
  // Try the exact root first, but keep fanning out while no bridge reports an
  // actually-detached wait: success:true + detached:false from the exact-root
  // bridge means the wait lives elsewhere (root-key mismatch), not "done".
  const exact =
    pool.getActiveBridgeForRoot(projectRoot) ??
    (pending ? pool.getBridge?.(projectRoot) : undefined);
  const all = pool.activeBridges();
  const targets = exact ? [exact, ...all.filter((bridge) => bridge !== exact)] : all;
  if (targets.length === 0) {
    // A wait lives inside a bridge, so with no bridge there is nothing to
    // detach. This is the normal state before the first tool call of a
    // session starts one, not a fault, so it is not logged as a warning.
    debug(
      `[bash_wait_detach] no bridge running yet for session ${sessionID} (root ${projectRoot}); nothing to detach`,
    );
    return;
  }
  let signaled = 0;
  let lastError: unknown = null;
  for (const bridge of targets) {
    try {
      signaled += 1;
      if (await sendBashWaitDetach(bridge, sessionID, pending)) {
        log(
          `[bash_wait_detach] detached wait for session ${sessionID} (bridge ${signaled}/${targets.length})`,
        );
        if (!pending) return;
      }
    } catch (err) {
      lastError = err;
    }
  }
  if (lastError !== null) {
    warn(
      `[bash_wait_detach] failed for session ${sessionID}: ${lastError instanceof Error ? lastError.message : String(lastError)}`,
    );
  } else {
    log(
      `[bash_wait_detach] ${pending ? "pending detach recorded" : "no active wait found"} for session ${sessionID} (signaled ${signaled} bridge(s))`,
    );
  }
}
