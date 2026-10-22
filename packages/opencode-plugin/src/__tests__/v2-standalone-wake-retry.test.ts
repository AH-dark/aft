import { afterEach, expect, test } from "bun:test";
import type { BashCompletedPayload, BashPatternMatchFrame } from "@cortexkit/aft-bridge";
import { Effect, Exit, PubSub, Scope, Stream } from "effect";

import { __resetBgNotificationStateForTests } from "../bg-notifications.js";
import { _resetSubagentCacheForTest } from "../shared/subagent-detect.js";
import { registerV2SessionLifecycle } from "../v2-session-lifecycle.js";
import { __resetV2WakeRoutesForTests, createV2RuntimeConsumer } from "../wakes/runtime-consumer.js";
import type { V2SessionSyntheticInput } from "../wakes/session-delivery.js";

afterEach(() => {
  __resetV2WakeRoutesForTests();
  _resetSubagentCacheForTest();
  __resetBgNotificationStateForTests();
});

const SESSION = "session-v2";
const RETRY_MS = 5;

const match: BashPatternMatchFrame = {
  type: "bash_pattern_match",
  session_id: SESSION,
  task_id: "bash-running",
  watch_id: "watch-ready",
  match_text: "READY",
  match_offset: 0,
  context: "READY: listening",
  once: true,
};

const completion: BashCompletedPayload = {
  type: "bash_completed",
  session_id: SESSION,
  task_id: "bash-done",
  status: "completed",
  command: "make",
  exit_code: 0,
  output_preview: "built",
} as BashCompletedPayload;

/**
 * A standalone OpenCode 2 Location (no SubC pool) whose host refuses the
 * first `rejections` synthetic inputs, and a bridge that keeps the watch
 * match and the completion pending until they are acknowledged.
 */
async function rig(rejections: number, delays: readonly number[] = [RETRY_MS, RETRY_MS, RETRY_MS]) {
  _resetSubagentCacheForTest();
  let refuse = rejections;
  let attempts = 0;
  const delivered: V2SessionSyntheticInput[] = [];
  const bus = await Effect.runPromise(
    PubSub.unbounded<{ type: string; data: { sessionID: string } }>(),
  );
  const scope = await Effect.runPromise(Scope.make());
  const host = {
    location: { directory: process.cwd() },
    event: { subscribe: () => Stream.fromPubSub(bus) },
    session: {
      get: () => Effect.succeed({}),
      prompt: () => Effect.die("wakes must use session.synthetic"),
      synthetic: (input: V2SessionSyntheticInput) =>
        Effect.suspend(() => {
          attempts++;
          if (refuse > 0) {
            refuse--;
            return Effect.fail(new Error("host refused synthetic input"));
          }
          return Effect.sync(() => delivered.push(input));
        }),
    },
  };
  let pendingMatch: BashPatternMatchFrame | undefined;
  let pendingCompletion = false;
  const acks: Array<Record<string, unknown>> = [];
  const bridge = {
    getCwd: () => process.cwd(),
    send: async (command: string, params: Record<string, unknown>) => {
      if (command === "bash_drain_completions")
        return {
          success: true,
          pending_matches: pendingMatch ? [pendingMatch] : [],
          bg_completions: pendingCompletion
            ? [{ ...completion, session_id: undefined, type: undefined }]
            : [],
        };
      if (command === "bash_ack_completions") {
        acks.push(params);
        if (params.matches) pendingMatch = undefined;
        if (Array.isArray(params.task_ids) && params.task_ids.includes(completion.task_id))
          pendingCompletion = false;
        return {
          success: true,
          acked_matches: params.matches ?? [],
          acked_task_ids: params.task_ids ?? [],
        };
      }
      return { success: true };
    },
  };
  const consumer = createV2RuntimeConsumer(
    host,
    () => ({}),
    () => bridge as never,
    { standaloneRetryDelaysMs: delays },
  );
  await Effect.runPromise(
    registerV2SessionLifecycle(host).pipe(Effect.provideService(Scope.Scope, scope)),
  );
  return {
    consumer,
    bridge,
    delivered,
    acks,
    attempts: () => attempts,
    accept: () => {
      refuse = 0;
    },
    matchPending: () => pendingMatch !== undefined,
    completionPending: () => pendingCompletion,
    async deleteSession() {
      await Effect.runPromise(
        PubSub.publish(bus, { type: "session.deleted", data: { sessionID: SESSION } }),
      );
      await Bun.sleep(0);
    },
    /** Model the bridge's push of a match the daemon now holds pending. */
    async pushMatch() {
      pendingMatch = match;
      await consumer.bridgeOptions.onBashPatternMatch?.(match, bridge as never);
    },
    async pushCompletion() {
      pendingCompletion = true;
      await consumer.bridgeOptions.onBashCompletion?.(completion, bridge as never);
    },
    /** Register the session before anything is pending, so its drain finds nothing. */
    async register() {
      consumer.registerSession?.(SESSION);
      await Bun.sleep(RETRY_MS);
    },
    async close() {
      consumer.dispose();
      await Effect.runPromise(Scope.close(scope, Exit.succeed(undefined)));
    },
  };
}

async function waitFor(reason: string, ready: () => boolean): Promise<void> {
  const deadline = Date.now() + 5_000;
  while (!ready()) {
    if (Date.now() > deadline) throw new Error(`timed out: ${reason}`);
    await Bun.sleep(1);
  }
}

/** Longer than every remaining retry delay, so a retry that was still due has run. */
const settle = () => Bun.sleep(RETRY_MS * 20);

test("V2 standalone once-watch match retries a refused wake until the host accepts it, once", async () => {
  const r = await rig(2);
  try {
    await r.register();
    // Only the push: no later registration, drain or nudge follows it.
    await r.pushMatch();
    expect(r.attempts()).toBe(1);
    expect(r.delivered).toEqual([]);
    await waitFor("the retried match must reach the session", () => r.delivered.length > 0);
    await settle();
    expect(r.attempts()).toBe(3);
    expect(r.delivered).toHaveLength(1);
    expect(r.delivered[0]?.metadata).toMatchObject({ kind: "bash_pattern_match" });
    expect(r.acks.filter((ack) => ack.matches)).toHaveLength(1);
    expect(r.matchPending()).toBe(false);
  } finally {
    await r.close();
  }
});

test("V2 standalone completion retries a refused wake until the host accepts it, once", async () => {
  const r = await rig(2);
  try {
    await r.register();
    await r.pushCompletion();
    expect(r.attempts()).toBe(1);
    await waitFor("the retried completion must reach the session", () =>
      r.delivered.some((input) => input.metadata?.kind === "bash_completion"),
    );
    await settle();
    expect(r.delivered.filter((input) => input.metadata?.kind === "bash_completion")).toHaveLength(
      1,
    );
    expect(r.attempts()).toBe(3);
    expect(r.completionPending()).toBe(false);
    expect(
      r.acks.filter(
        (ack) => Array.isArray(ack.task_ids) && ack.task_ids.includes(completion.task_id),
      ),
    ).toHaveLength(1);
  } finally {
    await r.close();
  }
});

test("V2 standalone wake retries stop at the cap and leave the match for the next registration", async () => {
  const r = await rig(Number.POSITIVE_INFINITY, [RETRY_MS, RETRY_MS]);
  try {
    await r.register();
    await r.pushMatch();
    await waitFor("both retries must run", () => r.attempts() >= 3);
    await settle();
    // The first attempt and two retries; then the consumer gives up.
    expect(r.attempts()).toBe(3);
    expect(r.delivered).toEqual([]);
    expect(r.matchPending()).toBe(true);
    r.accept();
    r.consumer.registerSession?.(SESSION);
    await waitFor("registration must deliver the kept match", () => r.delivered.length > 0);
    await settle();
    expect(r.delivered).toHaveLength(1);
    expect(r.matchPending()).toBe(false);
  } finally {
    await r.close();
  }
});

test("V2 disposing the consumer cancels a waiting standalone wake retry", async () => {
  const r = await rig(1, [RETRY_MS * 4]);
  try {
    await r.register();
    await r.pushMatch();
    expect(r.attempts()).toBe(1);
    r.consumer.dispose();
    await settle();
    expect(r.attempts()).toBe(1);
    expect(r.delivered).toEqual([]);
    expect(r.acks).toEqual([]);
  } finally {
    await r.close();
  }
});

test("V2 deleting the session ends a waiting standalone wake retry without a wake", async () => {
  const r = await rig(1, [RETRY_MS * 4, RETRY_MS * 4]);
  try {
    await r.register();
    await r.pushMatch();
    expect(r.attempts()).toBe(1);
    await r.deleteSession();
    await settle();
    expect(r.attempts()).toBe(1);
    expect(r.delivered).toEqual([]);
    // The deleted session's match is acknowledged, as a late push for it is,
    // so the bridge does not deliver it again.
    expect(r.matchPending()).toBe(false);
    await settle();
    expect(r.attempts()).toBe(1);
  } finally {
    await r.close();
  }
});
