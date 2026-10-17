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

type ActiveBridgePool = Pick<AftTransportPool, "getActiveBridgeForRoot">;

const pendingOperations = new WeakMap<AftProjectTransport, Promise<void>>();
const pendingClears = new WeakMap<AftProjectTransport, Promise<void>>();
const pendingRequests = new WeakMap<ActiveBridgePool, Map<string, PendingRequest>>();
const PENDING_DETACH_TTL_MS = 120_000;

interface PendingRequest {
  expiresAt: number;
  bridges: Set<AftProjectTransport>;
  timer?: ReturnType<typeof setTimeout>;
}

function rememberPendingRequest(pool: ActiveBridgePool, sessionID: string): PendingRequest {
  let sessions = pendingRequests.get(pool);
  if (!sessions) pendingRequests.set(pool, (sessions = new Map()));
  const request: PendingRequest = sessions.get(sessionID) ?? { expiresAt: 0, bridges: new Set() };
  clearTimeout(request.timer);
  request.expiresAt = Date.now() + PENDING_DETACH_TTL_MS;
  request.timer = setTimeout(() => {
    if (sessions.get(sessionID) === request) sessions.delete(sessionID);
  }, PENDING_DETACH_TTL_MS);
  request.timer.unref();
  sessions.set(sessionID, request);
  return request;
}

function queuePendingOperation(
  bridge: AftProjectTransport,
  sessionID: string,
  run: () => Promise<void>,
): Promise<void> {
  const previous = pendingOperations.get(bridge);
  // Preserve admission/clear order on each bridge without letting another
  // project's slow admission hold up this bridge's delivery clear.
  const operation = (previous ? previous.then(run, run) : run()).catch((error) => {
    warn(`[bash_wait_detach] request failed for session ${sessionID}: ${String(error)}`);
  });
  pendingOperations.set(bridge, operation);
  void operation.finally(() => {
    if (pendingOperations.get(bridge) === operation) pendingOperations.delete(bridge);
  });
  return operation;
}

async function waitForPendingClear(bridge: AftProjectTransport): Promise<void> {
  const clear = pendingClears.get(bridge);
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

/** Pi starts the next turn after including messages queued while tools were running. */
export async function clearPendingBashWaitDetach(
  pool: ActiveBridgePool,
  _projectRoot: string,
  sessionID: string | undefined,
): Promise<void> {
  if (!sessionID) return;
  const sessions = pendingRequests.get(pool);
  const request = sessions?.get(sessionID);
  if (!request) return;
  sessions?.delete(sessionID);
  clearTimeout(request.timer);
  request.expiresAt = 0;
  for (const bridge of request.bridges) {
    const clear = queuePendingOperation(bridge, sessionID, async () => {
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
    // Retain the barrier after removing the cached request. Tools wait only for
    // their own bridge, while the model step never waits for any bridge RPC.
    pendingClears.set(bridge, clear);
    void clear.finally(() => {
      if (pendingClears.get(bridge) === clear) pendingClears.delete(bridge);
    });
  }
}

/** A real tool may open a cold bridge; deliver cached input before sending that tool. */
export async function forwardPendingBashWaitDetach(
  pool: ActiveBridgePool,
  bridge: AftProjectTransport,
  sessionID: string | undefined,
): Promise<void> {
  if (!sessionID) return;
  await waitForPendingClear(bridge);
  const request = pendingRequests.get(pool)?.get(sessionID);
  if (!request || request.expiresAt <= Date.now()) return;
  if (request.bridges.has(bridge)) {
    await pendingOperations.get(bridge);
    return;
  }
  request.bridges.add(bridge);
  await queuePendingOperation(bridge, sessionID, async () => {
    if (request.expiresAt <= Date.now()) return;
    await sendBashWaitDetach(bridge, sessionID, true);
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
    void signalBashWaitDetachForProject(pool, projectRoot, sessionID, true);
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
  const request = pending ? rememberPendingRequest(pool, sessionID) : undefined;
  const run = async () => {
    if (request && request.expiresAt <= Date.now()) return;
    // Message admission never creates a bridge. A later tool forwards cached
    // input if it opens this session's first bridge before delivery or expiry.
    const bridge = pool.getActiveBridgeForRoot(projectRoot);
    if (!bridge) return;
    request?.bridges.add(bridge);
    try {
      const send = () => sendBashWaitDetach(bridge, sessionID, pending);
      if (request) await queuePendingOperation(bridge, sessionID, send);
      else await send();
    } catch (err) {
      warn(
        `[bash_wait_detach] failed for session ${sessionID}: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
  };
  await run();
}
