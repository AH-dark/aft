import {
  type AftProjectTransport,
  type AftTransportPool,
  type CanonicalRootPath,
  containsStandaloneDetachKeyword,
  shouldInterruptWaitsForMessage,
  stripDetachKeywordsAndTidyGap as stripStandaloneDetachKeywords,
} from "@cortexkit/aft-bridge";
import type { AftConfig } from "./config.js";
import { resolveBashConfig } from "./config.js";
import { debug, log, warn } from "./logger.js";
import { signalSyncWatchAbort } from "./sync-watch-abort.js";

const BASH_TRANSPORT_TIMEOUT_MS = 30_000;

type ActiveBridgePool = Pick<AftTransportPool, "getActiveBridgeForRoot" | "activeBridges">;

const bridgeIdentities = new WeakMap<ActiveBridgePool, Map<string, object>>();
const pendingOperations = new WeakMap<object, Map<string, Promise<void>>>();
const pendingClears = new WeakMap<object, Map<string, Promise<void>>>();
const pendingRequests = new WeakMap<ActiveBridgePool, Map<string, PendingRequest>>();
const PENDING_DETACH_TTL_MS = 120_000;

interface PendingRequest {
  expiresAt: number;
  bridges: Map<object, AftProjectTransport>;
  timer?: ReturnType<typeof setTimeout>;
}

function rememberPendingRequest(pool: ActiveBridgePool, sessionID: string): PendingRequest {
  let sessions = pendingRequests.get(pool);
  if (!sessions) pendingRequests.set(pool, (sessions = new Map()));
  const request: PendingRequest = sessions.get(sessionID) ?? { expiresAt: 0, bridges: new Map() };
  clearTimeout(request.timer);
  request.expiresAt = Date.now() + PENDING_DETACH_TTL_MS;
  request.timer = setTimeout(() => {
    if (sessions.get(sessionID) === request) sessions.delete(sessionID);
  }, PENDING_DETACH_TTL_MS);
  request.timer.unref();
  sessions.set(sessionID, request);
  return request;
}

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

function bridgeIdentity(pool: ActiveBridgePool, bridge: AftProjectTransport): object {
  // Root lookups can return a facade while activeBridges returns its underlying
  // transport. Both must use the same queue and delivery barrier within this pool.
  const root = bridge.getCwd?.();
  if (root === undefined) return bridge;
  let roots = bridgeIdentities.get(pool);
  if (!roots) bridgeIdentities.set(pool, (roots = new Map()));
  let identity = roots.get(root);
  if (!identity) roots.set(root, (identity = {}));
  return identity;
}

function queuePendingOperation(
  identity: object,
  sessionID: string,
  run: () => Promise<void>,
): Promise<void> {
  let sessions = pendingOperations.get(identity);
  if (!sessions) pendingOperations.set(identity, (sessions = new Map()));
  const previous = sessions.get(sessionID);
  // Preserve admission/clear order for one session without delaying other
  // sessions that share the project's bridge.
  const operation = (previous ? previous.then(run, run) : run()).catch((error) => {
    warn(`[bash_wait_detach] request failed for session ${sessionID}: ${String(error)}`);
  });
  sessions.set(sessionID, operation);
  void operation.finally(() => {
    if (sessions.get(sessionID) === operation) sessions.delete(sessionID);
  });
  return operation;
}

async function waitForPendingClear(identity: object, sessionID: string): Promise<void> {
  const clear = pendingClears.get(identity)?.get(sessionID);
  if (!clear) return;
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([
      clear,
      new Promise<never>((_, reject) => {
        timer = setTimeout(
          () => reject(new Error("Timed out waiting for bash wait detach delivery clear")),
          BASH_TRANSPORT_TIMEOUT_MS,
        );
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

/** Before the next model step, discard requests that no foreground wait has consumed. */
export async function clearPendingBashWaitDetach(
  pool: ActiveBridgePool,
  sessionID: string | undefined,
): Promise<void> {
  if (!sessionID) return;
  const sessions = pendingRequests.get(pool);
  const request = sessions?.get(sessionID);
  if (!request) return;
  sessions?.delete(sessionID);
  clearTimeout(request.timer);
  // Stop in-flight fanout from arming bridges after the model has seen the input.
  request.expiresAt = 0;
  for (const [identity, bridge] of request.bridges) {
    const clear = queuePendingOperation(identity, sessionID, async () => {
      try {
        await bridge.send(
          "bash_wait_detach",
          { session_id: sessionID, clear_pending: true },
          { keepBridgeOnTimeout: true, transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS },
        );
      } catch (error) {
        warn(`[bash_wait_detach] delivery clear failed for session ${sessionID}: ${String(error)}`);
      }
    });
    // Retain the barrier after removing the cached request. Only this session's
    // bash calls need it; the model step never waits for any bridge RPC.
    let clears = pendingClears.get(identity);
    if (!clears) pendingClears.set(identity, (clears = new Map()));
    clears.set(sessionID, clear);
    void clear.finally(() => {
      if (clears.get(sessionID) === clear) clears.delete(sessionID);
    });
  }
}

/** A real tool may open a cold bridge; deliver cached input before sending that tool. */
export async function forwardPendingBashWaitDetach(
  pool: AftTransportPool,
  projectRoot: CanonicalRootPath,
  sessionID: string | undefined,
  bash: boolean,
): Promise<void> {
  if (!sessionID) return;
  const cached = pendingRequests.get(pool)?.get(sessionID);
  // Unrelated tools only participate when they must forward cached input to a
  // newly opened bridge. They cannot consume a stale foreground-bash detach.
  if (!bash && (!cached || cached.expiresAt <= Date.now())) return;
  const bridge =
    cached && cached.expiresAt > Date.now()
      ? pool.getBridgeForCanonicalRoot
        ? pool.getBridgeForCanonicalRoot(projectRoot)
        : pool.getBridge(projectRoot)
      : pool.getActiveBridgeForRoot?.(projectRoot);
  if (!bridge) return;
  const identity = bridgeIdentity(pool, bridge);
  if (bash) await waitForPendingClear(identity, sessionID);
  const request = pendingRequests.get(pool)?.get(sessionID);
  if (!request || request.expiresAt <= Date.now()) return;
  if (request.bridges.has(identity)) {
    await pendingOperations.get(identity)?.get(sessionID);
    return;
  }
  request.bridges.set(identity, bridge);
  await queuePendingOperation(identity, sessionID, async () => {
    if (request.expiresAt <= Date.now()) return;
    await sendBashWaitDetach(bridge, sessionID, true);
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
    void signalBashWaitDetachForProject(pool, projectRoot, sessionID, remember);
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
  const request = pending ? rememberPendingRequest(pool, sessionID) : undefined;
  const run = () => signalActiveBridges(pool, projectRoot, sessionID, request);
  await run();
}

async function signalActiveBridges(
  pool: ActiveBridgePool,
  projectRoot: string,
  sessionID: string,
  request?: PendingRequest,
): Promise<void> {
  if (request && request.expiresAt <= Date.now()) return;
  // Try the exact root first, but keep fanning out while no bridge reports an
  // actually-detached wait: success:true + detached:false from the exact-root
  // bridge means the wait lives elsewhere (root-key mismatch), not "done".
  const exact = pool.getActiveBridgeForRoot(projectRoot);
  const all = pool.activeBridges();
  const targets = new Map<object, AftProjectTransport>();
  for (const bridge of exact ? [exact, ...all] : all) {
    const identity = bridgeIdentity(pool, bridge);
    if (!targets.has(identity)) targets.set(identity, bridge);
  }
  if (targets.size === 0) {
    // Input must not defeat lazy startup. The cached request will be forwarded
    // if a real tool opens a bridge before model delivery or expiry.
    debug(
      `[bash_wait_detach] no bridge running yet for session ${sessionID} (root ${projectRoot}); nothing to detach`,
    );
    return;
  }
  let signaled = 0;
  let lastError: unknown = null;
  for (const [identity, bridge] of targets) {
    if (request && request.expiresAt <= Date.now()) break;
    try {
      signaled += 1;
      request?.bridges.set(identity, bridge);
      let detached = false;
      const send = async () => {
        detached = await sendBashWaitDetach(bridge, sessionID, request !== undefined);
      };
      if (request) await queuePendingOperation(identity, sessionID, send);
      else await send();
      if (detached) {
        log(
          `[bash_wait_detach] detached wait for session ${sessionID} (bridge ${signaled}/${targets.size})`,
        );
        if (!request) return;
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
    const message = request
      ? `[bash_wait_detach] pending detach recorded for session ${sessionID} (signaled ${signaled} bridge(s))`
      : `[bash_wait_detach] no active wait found for session ${sessionID} (signaled ${signaled} bridge(s))`;
    if (request) log(message);
    else debug(message);
  }
}
