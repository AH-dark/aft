import {
  type AftProjectTransport,
  type AftTransportPool,
  containsStandaloneDetachKeyword,
  shouldInterruptWaitsForMessage,
  stripDetachKeywordsAndTidyGap as stripStandaloneDetachKeywords,
} from "@cortexkit/aft-bridge";
import type { AftConfig } from "./config.js";
import { resolveBashConfig } from "./config.js";
import { warn } from "./logger.js";
import { signalSyncWatchAbort } from "./sync-watch-abort.js";

const BASH_TRANSPORT_TIMEOUT_MS = 30_000;

type ActiveBridgePool = Pick<AftTransportPool, "getActiveBridgeForRoot"> &
  Partial<Pick<AftTransportPool, "getBridge">>;

const pendingOperations = new WeakMap<ActiveBridgePool, Map<string, Promise<void>>>();

function queuePendingOperation(
  pool: ActiveBridgePool,
  sessionID: string,
  run: () => Promise<void>,
): Promise<void> {
  let sessions = pendingOperations.get(pool);
  if (!sessions) pendingOperations.set(pool, (sessions = new Map()));
  const previous = sessions.get(sessionID);
  // Serialize admission with turn_start clearing so a delayed RPC cannot
  // re-arm a detach request for an already-delivered message.
  const operation = (previous ? previous.then(run, run) : run()).catch((error) => {
    warn(`[bash_wait_detach] request failed for session ${sessionID}: ${String(error)}`);
  });
  sessions.set(sessionID, operation);
  void operation.finally(() => {
    if (sessions.get(sessionID) === operation) sessions.delete(sessionID);
  });
  return operation;
}

/** Pi starts the next turn after including messages queued while tools were running. */
export async function clearPendingBashWaitDetach(
  pool: ActiveBridgePool,
  projectRoot: string,
  sessionID: string | undefined,
): Promise<void> {
  if (!sessionID) return;
  await queuePendingOperation(pool, sessionID, async () => {
    const bridge = pool.getActiveBridgeForRoot(projectRoot);
    if (!bridge) return;
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
}

export { BASH_WAIT_DETACH_MAGIC_KEYWORD } from "@cortexkit/aft-bridge";

const EMPTY_DETACH_MESSAGE = "(requested background detach)";

/** Strip the `&detach` control token before Pi's input transform delivers the user message to the model. */
export function stripUserMessageDetachKeyword(messageText: string): string {
  if (!containsStandaloneDetachKeyword(messageText)) return messageText;
  const stripped = stripStandaloneDetachKeywords(messageText);
  return stripped.trim() === "" ? EMPTY_DETACH_MESSAGE : stripped;
}

/**
 * Decide whether a message should detach a foreground bash call and
 * abort a sync bash_watch. `messageText` is read before the keyword is
 * stripped from it.
 */
export function shouldDetachBashWaitOnUserMessage(config: AftConfig, messageText: string): boolean {
  return shouldInterruptWaitsForMessage(
    resolveBashConfig(config).detach_on_user_message,
    messageText,
  );
}

/** The fields of Pi's `input` event this module reads. */
export interface PiInputEvent {
  text: string;
}

/**
 * Handle Pi's `input` event for the session's blocking waits: decide before
 * the `&detach` token is stripped, then (only when the decision says so) abort
 * any sync bash_watch and detach any foreground bash. Both waits follow the same
 * decision, so `bash.detach_on_user_message: false` protects a sync bash_watch
 * as well. Returns the text to deliver to the model.
 */
export function interruptBashWaitsForInput(
  pool: ActiveBridgePool,
  config: AftConfig,
  projectRoot: string,
  sessionID: string | undefined,
  event: PiInputEvent,
): string {
  if (shouldDetachBashWaitOnUserMessage(config, event.text)) {
    signalSyncWatchAbort(sessionID);
    if (sessionID)
      void queuePendingOperation(pool, sessionID, () =>
        signalBashWaitDetachForProject(pool, projectRoot, sessionID, true),
      );
  }
  return stripUserMessageDetachKeyword(event.text);
}

async function sendBashWaitDetach(
  bridge: AftProjectTransport,
  sessionID: string,
  pending: boolean,
): Promise<void> {
  const response = await bridge.send(
    "bash_wait_detach",
    { session_id: sessionID, ...(pending ? { pending: true } : {}) },
    { keepBridgeOnTimeout: true, transportTimeoutMs: BASH_TRANSPORT_TIMEOUT_MS },
  );
  if (response.success === false) {
    throw new Error(String(response.message ?? "bash_wait_detach failed"));
  }
}

export async function signalBashWaitDetachForProject(
  pool: ActiveBridgePool,
  projectRoot: string,
  sessionID: string | undefined,
  pending = false,
): Promise<void> {
  if (!sessionID) return;
  const bridge =
    pool.getActiveBridgeForRoot(projectRoot) ??
    (pending ? pool.getBridge?.(projectRoot) : undefined);
  if (!bridge) return;
  try {
    await sendBashWaitDetach(bridge, sessionID, pending);
  } catch (err) {
    warn(
      `[bash_wait_detach] failed for session ${sessionID}: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
}
