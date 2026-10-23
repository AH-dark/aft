import { afterEach, expect, test } from "bun:test";
import type { BashCompletedPayload } from "@cortexkit/aft-bridge";
import { Effect } from "effect";

import { __resetV2WakeRoutesForTests, createV2RuntimeConsumer } from "../wakes/runtime-consumer.js";
import { MAX_ADMITTED_WAKES, V2SessionDelivery } from "../wakes/session-delivery.js";
import { capturedDrainReply, drainEntry } from "./helpers/drain-wire.js";

afterEach(__resetV2WakeRoutesForTests);

function rig() {
  const synthetic: unknown[] = [];
  const pending = new Map<string, BashCompletedPayload>();
  let failAck: "throw" | "refuse" | undefined;
  const consumer = createV2RuntimeConsumer({
    session: {
      get: () => Effect.succeed({}),
      prompt: () => Effect.void,
      synthetic: (input) => Effect.sync(() => synthetic.push(input)),
    },
  });
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      if (command === "bash_drain_completions")
        return {
          success: true,
          bg_completions: [...pending.values()].map(drainEntry),
          pending_matches: [],
        };
      if (command === "bash_ack_completions") {
        if (failAck) {
          const failure = failAck;
          failAck = undefined;
          if (failure === "throw") throw new Error("ack transport unavailable");
          return { success: false, message: "ack unavailable" };
        }
        const acked = (params.task_ids as string[]).filter((id) => pending.delete(id));
        return { success: true, acked_task_ids: acked };
      }
      return { success: true };
    },
  };
  consumer.registerSession?.("s");
  return {
    consumer,
    synthetic,
    pending,
    failAck: (mode: "throw" | "refuse") => {
      failAck = mode;
    },
    push: (completion: BashCompletedPayload) =>
      consumer.bridgeOptions.onBashCompletion?.(completion, bridge as never),
  };
}

const completion = (id: string): BashCompletedPayload => ({
  type: "bash_completed",
  session_id: "s",
  task_id: id,
  status: "completed",
  command: "true",
  exit_code: 0,
  output_preview: "done",
});

test("V2 one Location delivers beyond 4096 confirmed completions and rejects a late replay", async () => {
  const deliveries = new Set<V2SessionDelivery>();
  const original = V2SessionDelivery.prototype.completion;
  V2SessionDelivery.prototype.completion = function (input) {
    deliveries.add(this);
    return original.call(this, input);
  };
  const r = rig();
  try {
    for (let i = 0; i <= MAX_ADMITTED_WAKES; i++) {
      const frame = completion(`task-${i}`);
      r.pending.set(frame.task_id, frame);
      await r.push(frame);
    }
    expect(r.synthetic).toHaveLength(MAX_ADMITTED_WAKES + 1);
    expect(r.pending.size).toBe(0);
    const delivery = [...deliveries][0] as unknown as { admittedWakes: Set<string> };
    expect(delivery.admittedWakes.size).toBe(0);
    await r.push(completion("task-0"));
    expect(r.synthetic).toHaveLength(MAX_ADMITTED_WAKES + 1);
  } finally {
    V2SessionDelivery.prototype.completion = original;
    r.consumer.dispose();
  }
});

test.each([
  "throw",
  "refuse",
] as const)("V2 completion retries a %s acknowledgement without readmitting the wake", async (failure) => {
  const r = rig();
  try {
    const frame = completion("task");
    r.pending.set(frame.task_id, frame);
    r.failAck(failure);
    await r.push(frame);
    expect(r.synthetic).toHaveLength(1);
    expect(r.pending.size).toBe(1);
    r.consumer.registerSession?.("s");
    await Bun.sleep(0);
    expect(r.synthetic).toHaveLength(1);
    expect(r.pending.size).toBe(0);
    await r.push(frame);
    expect(r.synthetic).toHaveLength(1);
  } finally {
    r.consumer.dispose();
  }
});

// The drain doubles below answer in the daemon's wire shape (see drainEntry):
// entries carry no `session_id`, because the reply is scoped by its request.
function wireRig() {
  const synthetic: unknown[] = [];
  const pending = new Map<string, BashCompletedPayload>();
  const consumer = createV2RuntimeConsumer(
    {
      session: {
        get: () => Effect.succeed({}),
        prompt: () => Effect.void,
        synthetic: (input) => Effect.sync(() => synthetic.push(input)),
      },
    },
    () => ({}),
    () => bridge as never,
  );
  const bridge = {
    getCwd: () => "/work",
    send: async (command: string, params: Record<string, unknown>) => {
      if (command === "bash_drain_completions")
        return {
          success: true,
          bg_completions: [...pending.values()]
            .filter((frame) => frame.session_id === params.session_id)
            .map(drainEntry),
          pending_matches: [],
        };
      if (command === "bash_ack_completions") {
        const acked = (params.task_ids as string[]).filter((id) => pending.delete(id));
        return { success: true, acked_task_ids: acked, acked_matches: [] };
      }
      return { success: true };
    },
  };
  return { consumer, synthetic, pending, bridge };
}

test("V2 pushed completion wakes the session when the drain reply omits session_id", async () => {
  const r = wireRig();
  try {
    r.consumer.registerSession?.("s");
    await Bun.sleep(0);
    const frame = completion("task");
    r.pending.set(frame.task_id, frame);
    await r.consumer.bridgeOptions.onBashCompletion?.(frame, r.bridge as never);
    expect(r.synthetic).toHaveLength(1);
    expect(r.pending.size).toBe(0);
  } finally {
    r.consumer.dispose();
  }
});

test("V2 registration drain delivers a queued completion whose entry omits session_id", async () => {
  const r = wireRig();
  try {
    const frame = completion("queued");
    r.pending.set(frame.task_id, frame);
    // A completion for another session must stay with that session.
    const foreign = { ...completion("foreign"), session_id: "other" };
    r.pending.set(foreign.task_id, foreign);
    r.consumer.registerSession?.("s");
    for (let attempt = 0; attempt < 20 && r.synthetic.length === 0; attempt++) await Bun.sleep(0);
    expect(r.synthetic).toHaveLength(1);
    expect([...r.pending.keys()]).toEqual(["foreign"]);
  } finally {
    r.consumer.dispose();
  }
});

test("V2 pushed completion wakes the session against a drain reply captured from the daemon", async () => {
  const reply = capturedDrainReply();
  const [entry] = reply.bg_completions;
  if (!entry) throw new Error("the captured drain reply lists no completion");
  // The capture is the daemon's real shape; if this ever fails, recapture it.
  expect("session_id" in entry).toBe(false);
  const synthetic: unknown[] = [];
  let acked = false;
  const consumer = createV2RuntimeConsumer({
    session: {
      get: () => Effect.succeed({}),
      prompt: () => Effect.void,
      synthetic: (input) => Effect.sync(() => synthetic.push(input)),
    },
  });
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      if (command === "bash_drain_completions")
        return acked ? { ...reply, bg_completions: [] } : reply;
      if (command === "bash_ack_completions") {
        acked = (params.task_ids as string[]).includes(entry.task_id as string);
        return { success: true, acked_task_ids: acked ? [entry.task_id] : [], acked_matches: [] };
      }
      return { success: true };
    },
  };
  try {
    consumer.registerSession?.("captured-session");
    await consumer.bridgeOptions.onBashCompletion?.(
      { ...entry, type: "bash_completed", session_id: "captured-session" } as BashCompletedPayload,
      bridge as never,
    );
    expect(synthetic).toHaveLength(1);
    expect(acked).toBe(true);
  } finally {
    consumer.dispose();
  }
});
