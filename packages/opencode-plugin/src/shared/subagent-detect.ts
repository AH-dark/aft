/**
 * Subagent detection via OpenCode SDK.
 *
 * AFT bash auto-promotes anything that takes more than ~5s to a background
 * task. For the main agent (Alfonso), promotion is fine — the completion
 * reminder fires back into the conversation and the agent picks it up on
 * its next idle. For SUBAGENTS, promotion is fatal: a subagent waiting for
 * a background-bash completion must end its turn to receive the reminder,
 * which closes the subagent session permanently. The subagent never gets
 * to commit its work.
 *
 * This module detects whether a session is a subagent (has a non-empty
 * `parentID`) so the bash tool can:
 *   1. Refuse `background: true` outright for subagents.
 *   2. Disable auto-promotion: the foreground poll window extends to the
 *      task's full hard-kill timeout instead of the default 5s.
 *
 * Detection is via the OpenCode SDK `client.session.get` on OpenCode 1 and
 * via the plugin context's `session.get` Effect on OpenCode 2. The result is
 * cached per sessionID for the lifetime of the plugin process — subagent
 * identity is sticky (parentID never changes after session creation), so
 * the cache never needs invalidation.
 *
 * Every bridge call asks for the role (AFT words its replies by it), so the
 * lookup must stay cheap and bounded:
 * - concurrent first calls for one session share a single lookup;
 * - a lookup that has not answered within LOOKUP_TIMEOUT_MS is abandoned and
 *   that call proceeds with an unknown role, so a slow host delays a read by
 *   at most LOOKUP_TIMEOUT_MS;
 * - a failed or abandoned lookup is cached as unknown for
 *   FAILED_LOOKUP_RETRY_MS, then retried, so a struggling host is not asked on
 *   every call but the real answer is still picked up later.
 *
 * Mirrors the pattern in `session-directory.ts`.
 */
import { Effect } from "effect";

import { sessionLog, sessionWarn } from "../logger.js";

interface SessionInfo {
  parentID?: string;
}

interface OpenCodeClientShape {
  session?: {
    get?: (input: {
      path: { id: string };
      query?: { directory?: string };
      throwOnError?: boolean;
    }) => Promise<{ data?: SessionInfo } | SessionInfo | undefined>;
  };
}

export type SubagentStatus = boolean | "unknown";

interface CacheEntry {
  isSubagent: SubagentStatus;
  /**
   * When an unknown answer (recorded after a failed or timed-out
   * lookup) stops counting and the host is asked again. Absent for a real
   * answer, which never changes: a session's parent is fixed at creation.
   */
  retryAfter?: number;
}

const CACHE_MAX_ENTRIES = 200;
/** Longest a caller waits for the host's session lookup. */
export const LOOKUP_TIMEOUT_MS = 1_000;
/**
 * How long an unknown role (after a failed or timed-out lookup) is reused
 * before the host is asked again. Short, because an unknown role is treated
 * cautiously by the shell and stop handlers until the real answer arrives; not
 * zero, because a host that keeps timing out would otherwise add up to
 * LOOKUP_TIMEOUT_MS to every tool call.
 */
export const FAILED_LOOKUP_RETRY_MS = 5_000;
const cache = new Map<string, CacheEntry>();
/** One lookup per session at a time; concurrent first calls await it. */
const inflight = new Map<string, Promise<SubagentStatus>>();
let now: () => number = () => Date.now();

/**
 * Returns `true` only when the given session is known to have a non-empty
 * `parentID` (subagent). Returns `false` for primary sessions, when the SDK is
 * unavailable, and for an unknown role (the lookup failed or took longer than
 * LOOKUP_TIMEOUT_MS). Callers whose safe default for an unknown role is the
 * subagent behaviour use resolveSubagentStatus instead.
 *
 * A known answer is an O(1) cache hit. Otherwise at most one host lookup
 * per session is in flight, and it is bounded by LOOKUP_TIMEOUT_MS.
 */
export async function resolveIsSubagent(
  client: unknown,
  sessionId: string | undefined,
  fallbackDirectory?: string,
): Promise<boolean> {
  return (await resolveSubagentStatus(client, sessionId, fallbackDirectory)) === true;
}

/**
 * The session's role: `true` when a non-empty `parentID` marks a subagent,
 * `false` when the host says it has no parent (or there is no session or no
 * lookup API to ask), and "unknown" while the host lookup is failing or slow.
 * The shell tool, the watch tool and the OpenCode 2 stop handler treat
 * "unknown" like a subagent, so a slow lookup cannot strip a subagent of its
 * foreground rule or its stop cleanup.
 */
export async function resolveSubagentStatus(
  client: unknown,
  sessionId: string | undefined,
  _fallbackDirectory?: string,
): Promise<SubagentStatus> {
  if (!sessionId) {
    sessionLog(undefined, "[subagent-detect] no sessionId provided → primary");
    return false;
  }

  const cached = cache.get(sessionId);
  if (cached && (cached.retryAfter === undefined || now() < cached.retryAfter)) {
    // Refresh LRU position. Don't log the hit — it's pure noise; the
    // downstream call sites already log their effective gate decisions.
    cache.delete(sessionId);
    cache.set(sessionId, cached);
    return cached.isSubagent;
  }

  const pending = inflight.get(sessionId);
  if (pending) return pending;
  const lookup = boundedLookup(client, sessionId).finally(() => {
    inflight.delete(sessionId);
  });
  inflight.set(sessionId, lookup);
  return lookup;
}

/**
 * Runs the host lookup, giving up after LOOKUP_TIMEOUT_MS. A real answer is
 * cached for good; a failure or timeout is cached as "unknown" until
 * FAILED_LOOKUP_RETRY_MS has passed. On OpenCode 2 the lookup is an Effect, and
 * Effect.timeout interrupts it when the time is up, so no abandoned lookup
 * keeps running. An OpenCode 1 lookup is a promise that cannot be cancelled;
 * its late answer is ignored.
 */
async function boundedLookup(client: unknown, sessionId: string): Promise<SubagentStatus> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timedOut = new Promise<"timeout">((resolve) => {
    timer = setTimeout(() => resolve("timeout"), LOOKUP_TIMEOUT_MS);
  });
  let answer: boolean | "failed" | "timeout";
  try {
    answer = isV2PluginContext(client)
      ? await resolveV2(client, sessionId)
      : await Promise.race([lookupSession(client, sessionId), timedOut]);
  } finally {
    clearTimeout(timer);
  }
  if (typeof answer === "boolean") {
    setCache(sessionId, answer);
    return answer;
  }
  if (answer === "timeout") {
    sessionWarn(
      sessionId,
      `[subagent-detect] session lookup did not answer within ${LOOKUP_TIMEOUT_MS}ms → unknown for now`,
    );
  }
  setCache(sessionId, "unknown", now() + FAILED_LOOKUP_RETRY_MS);
  return "unknown";
}

/** The host's answer, or "failed" when the lookup threw. Never rejects. */
async function lookupSession(client: unknown, sessionId: string): Promise<boolean | "failed"> {
  if (isV2PluginContext(client)) return resolveV2(client, sessionId);

  const c = client as OpenCodeClientShape;
  const sessionApi = c?.session;
  if (!sessionApi || typeof sessionApi.get !== "function") {
    // SDK shape unavailable. Cache as not-subagent so we don't retry
    // every call when the host doesn't expose session.get.
    sessionLog(
      sessionId,
      `[subagent-detect] client.session.get unavailable (client=${typeof client}, session=${typeof sessionApi}, get=${typeof sessionApi?.get}) → caching as primary`,
    );
    return false;
  }

  sessionLog(
    sessionId,
    `[subagent-detect] cache miss, calling client.session.get(id=${sessionId})`,
  );

  try {
    // Call as a method so the SDK's `this._client` reference resolves
    // correctly. Extracting `sessionApi.get` into a local would lose
    // the binding and crash the SDK with "undefined is not an object".
    // SDK schema: SessionGetData uses `path: { id }`, NOT a flat `sessionID`.
    // We do NOT pass `directory` — looking up a session by ID is an identity
    // query, not a directory-scoped one. Passing the wrong shape returned a
    // different session whose `parentID` was undefined, defeating the gate.
    const result = await sessionApi.get({
      path: { id: sessionId },
    });
    // SDK responses come either as `{ data: Session }` or directly as
    // `Session` depending on `ThrowOnError`. Handle both shapes.
    const session: SessionInfo | undefined =
      (result as { data?: SessionInfo } | undefined)?.data ?? (result as SessionInfo | undefined);
    const parentIdRaw = session?.parentID;
    const isSubagent =
      session !== undefined && typeof session.parentID === "string" && session.parentID.length > 0;
    sessionLog(
      sessionId,
      `[subagent-detect] SDK returned session=${session !== undefined ? "present" : "undefined"}, parentID=${JSON.stringify(parentIdRaw)} → isSubagent=${isSubagent}`,
    );
    return isSubagent;
  } catch (err) {
    sessionWarn(
      sessionId,
      `[subagent-detect] SDK lookup failed: ${err instanceof Error ? err.message : String(err)}`,
    );
    return "failed";
  }
}

/**
 * The OpenCode 2 runtime passes its plugin context where the V1 plugin passes
 * the SDK client (see `entry/server-runtime.mjs`). That context has no SDK
 * client at all; its `session.get` takes `{ sessionID }` and returns an Effect
 * that resolves to the bare session record. Calling it the V1 way builds an
 * Effect that never runs, and awaiting that Effect yields the Effect itself, so
 * no `parentID` is ever read and every OpenCode 2 session would be cached as
 * primary. The context is recognised by its `location`, the same capability
 * the runtime checks before booting.
 */
interface V2PluginContextShape {
  location: { directory: string };
  session: {
    get(input: { sessionID: string }): Effect.Effect<SessionInfo | undefined, unknown>;
  };
}

function isV2PluginContext(client: unknown): client is V2PluginContextShape {
  if (!client || typeof client !== "object") return false;
  const candidate = client as { location?: { directory?: unknown }; session?: { get?: unknown } };
  return (
    typeof candidate.location?.directory === "string" &&
    typeof candidate.session?.get === "function"
  );
}

async function resolveV2(
  context: V2PluginContextShape,
  sessionId: string,
): Promise<boolean | "failed"> {
  let session: SessionInfo | undefined;
  try {
    session = await Effect.runPromise(
      Effect.timeout(context.session.get({ sessionID: sessionId }), LOOKUP_TIMEOUT_MS),
    );
  } catch (err) {
    sessionWarn(
      sessionId,
      `[subagent-detect] OpenCode 2 session lookup failed: ${err instanceof Error ? err.message : String(err)}`,
    );
    return "failed";
  }
  if (!session) return "failed";
  const isSubagent = typeof session?.parentID === "string" && session.parentID.length > 0;
  sessionLog(
    sessionId,
    `[subagent-detect] OpenCode 2 session parentID=${JSON.stringify(session?.parentID)} → isSubagent=${isSubagent}`,
  );
  return isSubagent;
}

function setCache(sessionId: string, isSubagent: SubagentStatus, retryAfter?: number): void {
  if (cache.has(sessionId)) cache.delete(sessionId);
  cache.set(sessionId, retryAfter === undefined ? { isSubagent } : { isSubagent, retryAfter });
  if (cache.size > CACHE_MAX_ENTRIES) {
    const oldest = cache.keys().next().value;
    if (oldest !== undefined) cache.delete(oldest);
  }
}

/** Test-only cache reset. Not exported from the public surface. */
export function _resetSubagentCacheForTest(): void {
  cache.clear();
  inflight.clear();
  now = () => Date.now();
}

/** Test-only clock override for the failed-lookup retry window. */
export function _setSubagentClockForTest(clock: () => number): void {
  now = clock;
}
