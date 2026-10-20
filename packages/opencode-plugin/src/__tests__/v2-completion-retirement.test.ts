import { afterEach, expect, test } from "bun:test";
import type { BashCompletedPayload } from "@cortexkit/aft-bridge";
import { Effect } from "effect";

import { __resetV2WakeRoutesForTests, createV2RuntimeConsumer } from "../wakes/runtime-consumer.js";
import { MAX_ADMITTED_WAKES, V2SessionDelivery } from "../wakes/session-delivery.js";

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
        return { success: true, bg_completions: [...pending.values()], pending_matches: [] };
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
