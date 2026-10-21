import { afterEach, expect, spyOn, test } from "bun:test";
import type { BashCompletedPayload } from "@cortexkit/aft-bridge";
import { Effect } from "effect";

import type { PatternMatchEntry } from "../bg-notifications.js";
import { __resetV2WakeRoutesForTests, createV2RuntimeConsumer } from "../wakes/runtime-consumer.js";
import { V2SessionDelivery, type V2SessionSyntheticInput } from "../wakes/session-delivery.js";
import { drainEntry } from "./helpers/drain-wire.js";

afterEach(__resetV2WakeRoutesForTests);

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

type Kind = "completion" | "pattern";
type Stage = "drain" | "lookup" | "admission" | "ack";

function rig(kind: Kind, stage: Stage) {
  const reached = deferred();
  const resume = deferred();
  const acked = deferred();
  let paused = false;
  const pause = async (current: Stage) => {
    if (current !== stage || paused) return;
    paused = true;
    reached.resolve();
    await resume.promise;
  };
  const frame: BashCompletedPayload = {
    type: "bash_completed",
    session_id: "s",
    task_id: "task",
    status: "completed",
    command: "true",
    exit_code: 0,
    output_preview: "done",
  };
  const match: PatternMatchEntry = {
    session_id: "s",
    task_id: "task",
    watch_id: "watch",
    match_offset: 10,
    reason: "pattern_match",
    match_text: "ready",
    context: "ready",
    once: true,
  };
  let pending = true;
  const sends: string[] = [];
  const synthetic: V2SessionSyntheticInput[] = [];
  const bridge = {
    send: async (command: string) => {
      sends.push(command);
      if (command === "bash_drain_completions") {
        await pause("drain");
        return {
          success: true,
          bg_completions: kind === "completion" && pending ? [drainEntry(frame)] : [],
          pending_matches: kind === "pattern" && pending ? [match] : [],
        };
      }
      if (command === "bash_ack_completions") {
        // The engine commits the ACK before the transport returns its response.
        pending = false;
        await pause("ack");
        acked.resolve();
        return {
          success: true,
          acked_task_ids: [frame.task_id],
          acked_matches: [match],
        };
      }
      return { success: true };
    },
  };
  const open = (recover = false) => {
    const consumer = createV2RuntimeConsumer(
      {
        session: {
          get: () =>
            Effect.promise(async () => {
              await pause("lookup");
              return {};
            }),
          prompt: () => Effect.void,
          synthetic: (input) =>
            Effect.promise(async () => {
              await pause("admission");
              synthetic.push(input);
            }),
        },
      },
      undefined,
      recover ? () => bridge as never : undefined,
    );
    consumer.registerSession?.("s");
    return consumer;
  };
  const consumer = open();
  const push = () =>
    kind === "completion"
      ? consumer.bridgeOptions.onBashCompletion?.(frame, bridge as never)
      : consumer.bridgeOptions.onBashPatternMatch?.(match, bridge as never);
  return { consumer, open, push, reached, resume, acked, sends, synthetic, pending: () => pending };
}

for (const kind of ["completion", "pattern"] as const) {
  test(`V2 ${kind} refused admission stays pending without ack`, async () => {
    const r = rig(kind, "drain");
    r.resume.resolve();
    const refusal = spyOn(
      V2SessionDelivery.prototype,
      kind === "completion" ? "completion" : "patternMatch",
    ).mockReturnValueOnce(Effect.succeed("not-delivered"));
    try {
      await r.push();
      expect(r.synthetic).toHaveLength(0);
      expect(r.sends).not.toContain("bash_ack_completions");
      expect(r.pending()).toBe(true);
    } finally {
      refusal.mockRestore();
      r.consumer.dispose();
    }
    const reopened = r.open(true);
    try {
      await r.acked.promise;
      expect(r.synthetic).toHaveLength(1);
      expect(r.pending()).toBe(false);
    } finally {
      reopened.dispose();
    }
  });

  test(`V2 ${kind} disposed during drain stays pending for a reopened Location`, async () => {
    const r = rig(kind, "drain");
    const attempt = r.push();
    await r.reached.promise;
    r.consumer.dispose();
    r.resume.resolve();
    await attempt;

    expect(r.synthetic).toHaveLength(0);
    expect(r.sends).not.toContain("bash_ack_completions");
    expect(r.pending()).toBe(true);
    const reopened = r.open(true);
    try {
      await r.acked.promise;
      expect(r.synthetic).toHaveLength(1);
      expect(r.pending()).toBe(false);
      expect(r.sends.filter((command) => command === "bash_ack_completions")).toHaveLength(1);
    } finally {
      reopened.dispose();
    }
  });

  for (const stage of ["lookup", "admission"] as const) {
    test(`V2 ${kind} disposed during ${stage} is not acknowledged`, async () => {
      const r = rig(kind, stage);
      const attempt = r.push();
      await r.reached.promise;
      r.consumer.dispose();
      r.resume.resolve();
      await attempt;
      expect(r.synthetic).toHaveLength(stage === "lookup" ? 0 : 1);
      expect(r.sends).not.toContain("bash_wait_detach");
      expect(r.sends).not.toContain("bash_ack_completions");
      expect(r.pending()).toBe(true);
    });
  }

  test(`V2 ${kind} disposed during ack is not redelivered after reopen`, async () => {
    const r = rig(kind, "ack");
    const attempt = r.push();
    await r.reached.promise;
    expect(r.synthetic).toHaveLength(1);
    r.consumer.dispose();
    const reopened = r.open();
    try {
      await r.push();
      r.resume.resolve();
      await attempt;
      await r.push();
      expect(r.pending()).toBe(false);
      expect(r.synthetic).toHaveLength(1);
      expect(r.sends.filter((command) => command === "bash_ack_completions")).toHaveLength(1);
    } finally {
      r.resume.resolve();
      reopened.dispose();
    }
  });
}

for (const kind of ["completion", "patternMatch"] as const) {
  test(`V2 ${kind} distinguishes delivered, already delivered, and closed delivery`, async () => {
    let admitted = 0;
    const delivery = new V2SessionDelivery({
      get: () => Effect.succeed({}),
      prompt: () => Effect.void,
      synthetic: () => Effect.sync(() => admitted++),
    });
    const wake = {
      sessionID: "s",
      taskIDs: ["task"],
      text: "done",
      watchID: "watch",
      matchOffset: 0,
    };
    expect(await Effect.runPromise(delivery[kind](wake))).toBe("delivered");
    expect(await Effect.runPromise(delivery[kind](wake))).toBe("already-delivered");
    delivery.dispose();
    expect(await Effect.runPromise(delivery[kind](wake))).toBe("not-delivered");
    expect(admitted).toBe(1);
  });

  test(`V2 ${kind} does not confirm an in-flight reservation as delivered`, async () => {
    const reached = deferred();
    const resume = deferred();
    const delivery = new V2SessionDelivery({
      get: () =>
        Effect.promise(async () => {
          reached.resolve();
          await resume.promise;
          return {};
        }),
      prompt: () => Effect.void,
      synthetic: () => Effect.void,
    });
    const wake = {
      sessionID: "s",
      taskIDs: ["task"],
      text: "done",
      watchID: "watch",
      matchOffset: 0,
    };
    const attempt = Effect.runPromise(delivery[kind](wake));
    await reached.promise;
    expect(await Effect.runPromise(delivery[kind](wake))).toBe("not-delivered");
    resume.resolve();
    expect(await attempt).toBe("delivered");
    expect(await Effect.runPromise(delivery[kind](wake))).toBe("already-delivered");
    delivery.dispose();
  });
}
