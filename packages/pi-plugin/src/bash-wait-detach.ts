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

function bridgeIdentity(pool: ActiveBridgePool, bridge: AftProjectTransport): object {
  // Root lookups can return a facade while tool dispatch uses its underlying
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
  pool: ActiveBridgePool,
  bridge: AftProjectTransport,
  sessionID: string | undefined,
  bash: boolean,
): Promise<void> {
  if (!sessionID) return;
  const cached = pendingRequests.get(pool)?.get(sessionID);
  // Unrelated tools only participate when they must forward cached input to a
  // newly opened bridge. They cannot consume a stale foreground-bash detach.
  if (!bash && (!cached || cached.expiresAt <= Date.now())) return;
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
    const identity = bridgeIdentity(pool, bridge);
    request?.bridges.set(identity, bridge);
    try {
      const send = () => sendBashWaitDetach(bridge, sessionID, pending);
      if (request) await queuePendingOperation(identity, sessionID, send);
      else await send();
    } catch (err) {
      warn(
        `[bash_wait_detach] failed for session ${sessionID}: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
  };
  await run();
}
