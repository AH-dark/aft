import { afterEach, expect, test } from "bun:test";
import type { BashPatternMatchFrame } from "@cortexkit/aft-bridge";
import { Effect, Exit, PubSub, Scope, Stream } from "effect";

import { __resetBgNotificationStateForTests } from "../bg-notifications.js";
import { _resetSubagentCacheForTest } from "../shared/subagent-detect.js";
import { __resetSyncWatchAbortForTests } from "../sync-watch-abort.js";
import { createBashWatchTool } from "../tools/bash_watch.js";
import { projectV2Tool } from "../tools/definitions/v2.js";
import type { PluginContext } from "../types.js";
import { registerV2SessionLifecycle } from "../v2-session-lifecycle.js";
import { __resetV2WakeRoutesForTests, createV2RuntimeConsumer } from "../wakes/runtime-consumer.js";
import {
  MAX_ADMITTED_WAKES,
  V2SessionDelivery,
  type V2SessionSyntheticInput,
  WakeDeliveryOverflow,
} from "../wakes/session-delivery.js";

afterEach(() => {
  __resetV2WakeRoutesForTests();
  _resetSubagentCacheForTest();
  __resetBgNotificationStateForTests();
  __resetSyncWatchAbortForTests();
});

async function rig(parentID?: string) {
  _resetSubagentCacheForTest();
  const synthetic: V2SessionSyntheticInput[] = [];
  const task = { id: "bash-running", status: "running" };
  const statusAtWake: string[] = [];
  const bus = await Effect.runPromise(
    PubSub.unbounded<{ type: string; data: { sessionID: string; reason?: string } }>(),
  );
  const scope = await Effect.runPromise(Scope.make());
  let reverted = false;
  let failAdmission = false;
  let failAck = false;
  let ignoreAck = false;
  const host = {
    location: { directory: process.cwd() },
    event: { subscribe: () => Stream.fromPubSub(bus) },
    session: {
      get: () =>
        Effect.sync(() => ({ parentID, revert: reverted ? { messageID: "boundary" } : undefined })),
      prompt: () => Effect.die("pattern wakes must use session.synthetic"),
      synthetic: (input: V2SessionSyntheticInput) =>
        Effect.suspend(() => {
          if (failAdmission) {
            failAdmission = false;
            return Effect.fail(new Error("host unavailable"));
          }
          return Effect.sync(() => {
            statusAtWake.push(task.status);
            return synthetic.push(input);
          });
        }),
    },
  };
  const sends: Array<{ command: string; params: Record<string, unknown> }> = [];
  let watch: Record<string, unknown> | undefined;
  let pending: BashPatternMatchFrame | undefined;
  let offset = 0;
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      sends.push({ command, params });
      if (command === "bash_notify") {
        if (params.task_id !== task.id) return { success: false, message: "task not found" };
        watch = params;
        return { success: true, watch_id: "watch-ready" };
      }
      if (command === "bash_drain_completions")
        return { success: true, pending_matches: pending ? [pending] : [] };
      if (command === "bash_ack_completions") {
        if (ignoreAck) {
          ignoreAck = false;
          return { success: true, acked_task_ids: [] };
        }
        if (failAck) {
          failAck = false;
          return { success: false, message: "ack unavailable" };
        }
        if (pending?.once) watch = undefined;
        pending = undefined;
        return { success: true, acked_matches: params.matches ?? [] };
      }
      return { success: true };
    },
  };
  const ctx = {
    client: host,
    config: { bash: { subagent_background: true } },
    pool: { getBridge: () => bridge, getActiveBridgeForRoot: () => bridge },
  } as unknown as PluginContext;
  const consumer = createV2RuntimeConsumer(
    host,
    () => ctx.config,
    () => bridge as never,
  );
  await Effect.runPromise(
    registerV2SessionLifecycle(host).pipe(Effect.provideService(Scope.Scope, scope)),
  );
  const tool = projectV2Tool(
    "bash_watch",
    createBashWatchTool(ctx),
    { directory: process.cwd() },
    consumer,
  );
  return {
    consumer,
    host,
    task,
    statusAtWake,
    synthetic,
    sends,
    bridge,
    pending: () => pending,
    watching: () => watch !== undefined,
    revert: (value: boolean) => {
      reverted = value;
    },
    failAdmission: () => {
      failAdmission = true;
    },
    failAck: () => {
      failAck = true;
    },
    ignoreAck: () => {
      ignoreAck = true;
    },
    async stop() {
      await Effect.runPromise(
        PubSub.publish(bus, {
          type: "session.execution.interrupted",
          data: { sessionID: "session-v2", reason: "user" },
        }),
      );
      await Bun.sleep(0);
    },
    async close() {
      consumer.dispose();
      await Effect.runPromise(Scope.close(scope, Exit.succeed(undefined)));
    },
    async register(once = true) {
      await Effect.runPromise(
        tool.execute(
          { taskId: "bash-running", background: true, pattern: "READY", once },
          { sessionID: "session-v2", progress: () => Effect.void },
        ),
      );
      expect(watch).toMatchObject({ task_id: "bash-running", pattern: "READY", once });
    },
    async print(line: string, push = true) {
      // Model the bridge's stdout push, not a host-side shortcut to session input.
      expect(task.status).toBe("running");
      if (!watch || !line.includes(String(watch.pattern))) return;
      const frame: BashPatternMatchFrame = {
        type: "bash_pattern_match",
        session_id: "session-v2",
        task_id: "bash-running",
        watch_id: "watch-ready",
        match_text: "READY",
        match_offset: offset,
        context: line,
        once: watch.once as boolean,
      };
      offset += line.length + 1;
      pending = frame;
      if (push) await consumer.bridgeOptions.onBashPatternMatch?.(frame, bridge as never);
    },
  };
}

test("V2 async bash_watch wakes on matching output before task completion", async () => {
  const r = await rig();
  try {
    await r.register();
    await r.print("READY: server is listening");
    expect(r.synthetic).toHaveLength(1);
    expect(r.statusAtWake).toEqual(["running"]);
    expect(r.synthetic[0]).toMatchObject({
      sessionID: "session-v2",
      delivery: "steer",
      metadata: { source: "aft", kind: "bash_pattern_match" },
    });
    expect(r.synthetic[0]?.text).toContain("[BG BASH NOTIFY]");
    expect(r.synthetic[0]?.text).toContain("READY: server is listening");
    expect(r.synthetic[0]).not.toHaveProperty("resume");
    expect(r.sends.filter((send) => send.command === "bash_ack_completions")).toEqual([
      {
        command: "bash_ack_completions",
        params: {
          session_id: "session-v2",
          matches: [{ task_id: "bash-running", watch_id: "watch-ready", match_offset: 0 }],
        },
      },
    ]);
  } finally {
    await r.close();
  }
});

test("V2 sticky watch retires 10000 admitted match keys and drops an acknowledged replay", async () => {
  const r = await rig();
  const deliveries = new Set<V2SessionDelivery>();
  const original = V2SessionDelivery.prototype.patternMatch;
  V2SessionDelivery.prototype.patternMatch = function (input) {
    deliveries.add(this);
    return original.call(this, input);
  };
  try {
    await r.register(false);
    for (let i = 0; i < 10000; i++) await r.print("READY");
    expect(r.synthetic).toHaveLength(10000);
    const delivery = [...deliveries][0] as unknown as { admittedWakes: Set<string> };
    expect(delivery.admittedWakes.size).toBe(0);
    await r.consumer.bridgeOptions.onBashPatternMatch?.(
      {
        type: "bash_pattern_match",
        session_id: "session-v2",
        task_id: "bash-running",
        watch_id: "watch-ready",
        match_offset: 0,
        match_text: "READY",
        context: "READY",
        once: false,
      },
      r.bridge as never,
    );
    expect(r.synthetic).toHaveLength(10000);
  } finally {
    V2SessionDelivery.prototype.patternMatch = original;
    await r.close();
  }
});

test("V2 acknowledging one match leaves the other watch pending for the next delivery", async () => {
  const r = await rig();
  const first: BashPatternMatchFrame = {
    type: "bash_pattern_match",
    session_id: "session-v2",
    task_id: "bash-running",
    watch_id: "first",
    match_offset: 10,
    match_text: "READY",
    context: "first",
    once: true,
  };
  const second = { ...first, watch_id: "second", match_offset: 20, context: "second" };
  const pending = [first, second];
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      if (command === "bash_drain_completions")
        return { success: true, pending_matches: [...pending] };
      if (command === "bash_ack_completions") {
        const matches = params.matches as
          | Array<{ watch_id: string; match_offset: number }>
          | undefined;
        for (let i = pending.length - 1; i >= 0; i--) {
          if (
            params.task_ids ||
            matches?.some(
              (m) =>
                m.watch_id === pending[i]?.watch_id && m.match_offset === pending[i]?.match_offset,
            )
          )
            pending.splice(i, 1);
        }
        return { success: true, acked_matches: params.matches ?? [] };
      }
      return { success: true };
    },
  };
  try {
    r.consumer.registerSession?.("session-v2");
    await r.consumer.bridgeOptions.onBashPatternMatch?.(first, bridge as never);
    expect(pending).toEqual([second]);
    await r.consumer.bridgeOptions.onBashPatternMatch?.(second, bridge as never);
    expect(pending).toEqual([]);
    expect(r.synthetic.map((input) => input.text)).toEqual([
      expect.stringContaining("first"),
      expect.stringContaining("second"),
    ]);
  } finally {
    await r.close();
  }
});

test("V2 unretired wake overflow is typed and does not evict duplicate protection", async () => {
  let admitted = 0;
  const delivery = new V2SessionDelivery({
    get: () => Effect.succeed({}),
    prompt: () => Effect.void,
    synthetic: () => Effect.sync(() => admitted++),
  });
  const wake = {
    sessionID: "s",
    taskIDs: ["task"],
    watchID: "sticky",
    matchOffset: 0,
    text: "READY",
  };
  for (let i = 0; i < MAX_ADMITTED_WAKES; i++) {
    expect(await Effect.runPromise(delivery.patternMatch({ ...wake, matchOffset: i }))).toBe(true);
  }
  const overflow = await Effect.runPromise(
    delivery
      .patternMatch({ ...wake, matchOffset: MAX_ADMITTED_WAKES })
      .pipe(Effect.catch((error) => Effect.succeed(error))),
  );
  expect(overflow).toBeInstanceOf(WakeDeliveryOverflow);
  expect(overflow).toMatchObject({ type: "wake_delivery_overflow" });
  expect(await Effect.runPromise(delivery.patternMatch(wake))).toBe(false);
  expect(admitted).toBe(MAX_ADMITTED_WAKES);
  delivery.dispose();
});

test("V2 once matches remain pending until admission and acknowledgement succeed", async () => {
  const r = await rig();
  try {
    await r.register();
    r.failAdmission();
    await r.print("READY");
    expect(r.synthetic).toEqual([]);
    expect(r.pending()).toBeDefined();
    expect(r.watching()).toBe(true);
    r.failAck();
    r.consumer.registerSession?.("session-v2");
    await Bun.sleep(0);
    expect(r.synthetic).toHaveLength(1);
    expect(r.pending()).toBeDefined();
    r.consumer.registerSession?.("session-v2");
    await Bun.sleep(0);
    expect(r.synthetic).toHaveLength(1);
    expect(r.pending()).toBeUndefined();
    expect(r.watching()).toBe(false);
  } finally {
    await r.close();
  }
});

test("V2 unconfirmed match acknowledgement retains admission until durable retirement", async () => {
  const r = await rig();
  try {
    await r.register(false);
    r.ignoreAck();
    await r.print("READY");
    expect(r.synthetic).toHaveLength(1);
    expect(r.pending()).toBeDefined();
    r.consumer.registerSession?.("session-v2");
    await Bun.sleep(0);
    expect(r.pending()).toBeUndefined();
    expect(r.synthetic).toHaveLength(1);
  } finally {
    await r.close();
  }
});

test("V2 sticky matches clear on ack, can wake again, and do not consume completion", async () => {
  const r = await rig();
  try {
    await r.register(false);
    await r.print("READY first");
    const first = {
      type: "bash_pattern_match" as const,
      session_id: "session-v2",
      task_id: "bash-running",
      watch_id: "watch-ready",
      match_text: "READY",
      match_offset: 0,
      context: "READY first",
      once: false,
    };
    await r.consumer.bridgeOptions.onBashPatternMatch?.(first, r.bridge as never);
    expect(r.synthetic).toHaveLength(1);
    expect(r.pending()).toBeUndefined();
    expect(r.watching()).toBe(true);
    await r.print("READY second");
    expect(r.synthetic).toHaveLength(2);
    r.task.status = "completed";
    await r.consumer.bridgeOptions.onBashCompletion?.(
      {
        type: "bash_completed",
        session_id: "session-v2",
        task_id: "bash-running",
        status: "completed",
        command: "server",
        exit_code: 0,
        output_preview: "done",
      },
      r.bridge as never,
    );
    expect(r.synthetic).toHaveLength(3);
    expect(r.synthetic[2]?.metadata?.kind).toBe("bash_completion");
  } finally {
    await r.close();
  }
});

test("V2 missed pushes drain into pattern steer wakes and drop foreign-session matches", async () => {
  const r = await rig();
  try {
    await r.register();
    await r.print("READY recovered", false);
    expect(r.synthetic).toEqual([]);
    r.consumer.transportOptions.onBgEventsNudge?.(process.cwd(), "session-v2");
    await Bun.sleep(0);
    expect(r.synthetic).toHaveLength(1);
    expect(r.synthetic[0]?.text).toContain("READY recovered");
    expect(r.pending()).toBeUndefined();
    const count = r.sends.length;
    await r.consumer.bridgeOptions.onBashPatternMatch?.(
      {
        type: "bash_pattern_match",
        session_id: "foreign",
        task_id: "bash-other",
        watch_id: "watch-other",
        match_text: "READY",
        match_offset: 0,
        context: "foreign",
        once: true,
      },
      r.bridge as never,
    );
    expect(r.synthetic).toHaveLength(1);
    expect(r.sends).toHaveLength(count);
  } finally {
    await r.close();
  }
});

test("V2 staged revert defers pattern admission and ack until the next registration", async () => {
  const r = await rig();
  try {
    await r.register();
    r.revert(true);
    await r.print("READY");
    expect(r.synthetic).toEqual([]);
    expect(r.pending()).toBeDefined();
    expect(r.sends.some((send) => send.command === "bash_ack_completions")).toBe(false);
    r.revert(false);
    r.consumer.registerSession?.("session-v2");
    await Bun.sleep(0);
    expect(r.synthetic).toHaveLength(1);
    expect(r.pending()).toBeUndefined();
  } finally {
    await r.close();
  }
});

test("V2 session registration recovers a pending match after consumer restart", async () => {
  const r = await rig();
  let restarted: ReturnType<typeof createV2RuntimeConsumer> | undefined;
  try {
    await r.register();
    await r.print("READY after restart", false);
    r.consumer.dispose();
    restarted = createV2RuntimeConsumer(
      r.host,
      () => ({}),
      () => r.bridge as never,
    );
    restarted.registerSession?.("session-v2");
    await Bun.sleep(0);
    expect(r.synthetic).toHaveLength(1);
    expect(r.synthetic[0]?.text).toContain("READY after restart");
    expect(r.pending()).toBeUndefined();
  } finally {
    restarted?.dispose();
    await r.close();
  }
});

test("V2 subagent pattern wakes require an eligible watch and respect deliberate stop", async () => {
  const r = await rig("parent");
  try {
    r.consumer.registerSession?.("session-v2");
    await r.consumer.bridgeOptions.onBashPatternMatch?.(
      {
        type: "bash_pattern_match",
        session_id: "session-v2",
        task_id: "not-watched",
        watch_id: "other",
        match_text: "READY",
        match_offset: 0,
        context: "READY",
        once: true,
      },
      r.bridge as never,
    );
    expect(r.synthetic).toEqual([]);
    await r.register(false);
    await r.print("READY allowed");
    expect(r.synthetic).toHaveLength(1);
    await r.stop();
    const acks = r.sends.filter((send) => send.command === "bash_ack_completions").length;
    await r.print("READY stopped");
    expect(r.synthetic).toHaveLength(1);
    expect(r.sends.filter((send) => send.command === "bash_ack_completions")).toHaveLength(acks);
  } finally {
    await r.close();
  }
});
