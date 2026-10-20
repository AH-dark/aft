import { afterEach, expect, test } from "bun:test";
import { Effect } from "effect";

import { projectV2Tool } from "../tools/definitions/v2.js";
import { __resetV2WakeRoutesForTests, createV2RuntimeConsumer } from "../wakes/runtime-consumer.js";
import { V2SessionDelivery } from "../wakes/session-delivery.js";

afterEach(__resetV2WakeRoutesForTests);

function flakyHost() {
  let attempts = 0;
  return {
    attempts: () => attempts,
    session: {
      get: () => Effect.succeed({}),
      prompt: () => Effect.void,
      synthetic: () =>
        Effect.suspend(() =>
          ++attempts === 1 ? Effect.fail(new Error("host unavailable")) : Effect.succeed({}),
        ),
    },
  };
}

test("V2 completion retries the same task after synthetic admission fails", async () => {
  const host = flakyHost();
  const delivery = new V2SessionDelivery(host.session);
  const wake = { sessionID: "s", taskIDs: ["task"], text: "done" };
  expect((await Effect.runPromiseExit(delivery.completion(wake)))._tag).toBe("Failure");
  expect(await Effect.runPromise(delivery.completion(wake))).toBe(true);
  expect(await Effect.runPromise(delivery.completion(wake))).toBe(false);
  expect(host.attempts()).toBe(2);
});

async function execute(consumer: ReturnType<typeof createV2RuntimeConsumer>, name: string) {
  const tool = projectV2Tool(
    name,
    { description: name, args: {}, execute: async () => "ok" },
    { directory: "/work" },
    consumer,
  );
  await Effect.runPromise(tool.execute({}, { sessionID: "s", progress: () => Effect.void }));
}

const completion = {
  type: "bash_completed" as const,
  session_id: "s",
  task_id: "task",
  status: "completed",
  command: "true",
  exit_code: 0,
  output_preview: "done",
};
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

test("V2 routing catches a failed synthetic admission and holds it for the next session registration", async () => {
  const host = flakyHost();
  const consumer = createV2RuntimeConsumer(host);
  const sends: string[] = [];
  const bridge = {
    send: async (command: string) => {
      sends.push(command);
      if (command === "bash_drain_completions")
        return { success: true, bg_completions: [completion], pending_matches: [] };
      return { success: true, acked_task_ids: [completion.task_id] };
    },
  };
  try {
    await execute(consumer, "bash");
    await expect(
      Promise.resolve(consumer.bridgeOptions.onBashCompletion?.(completion, bridge as never)),
    ).resolves.toBeUndefined();
    expect(sends).toEqual(["bash_drain_completions"]);
    await execute(consumer, "bash");
    await settle();
    expect(host.attempts()).toBe(2);
    // An admitted completion detaches the session's waits, then is acknowledged.
    expect(sends).toEqual([
      "bash_drain_completions",
      "bash_drain_completions",
      "bash_wait_detach",
      "bash_ack_completions",
    ]);
  } finally {
    consumer.dispose();
  }
});

test("V2 bash companions route held completions without launching bash", async () => {
  for (const name of ["bash_status", "bash_watch"]) {
    __resetV2WakeRoutesForTests();
    const synthetic: unknown[] = [];
    const consumer = createV2RuntimeConsumer({
      session: {
        get: () => Effect.succeed({}),
        prompt: () => Effect.void,
        synthetic: (input) => Effect.sync(() => synthetic.push(input)),
      },
    });
    const sends: string[] = [];
    const bridge = {
      send: async (command: string) => {
        sends.push(command);
        if (command === "bash_drain_completions")
          return { success: true, bg_completions: [completion], pending_matches: [] };
        return { success: true, acked_task_ids: [completion.task_id] };
      },
    };
    try {
      await consumer.bridgeOptions.onBashCompletion?.(completion, bridge as never);
      expect(synthetic).toEqual([]);
      await execute(consumer, name);
      await settle();
      expect(synthetic).toHaveLength(1);
      // An admitted completion detaches the session's waits, then is acknowledged.
      expect(sends).toEqual(["bash_drain_completions", "bash_wait_detach", "bash_ack_completions"]);
    } finally {
      consumer.dispose();
    }
  }
});

test("V2 routing withholds acknowledgement during a staged revert and retries after registration", async () => {
  let reverted = true;
  const synthetic: unknown[] = [];
  const consumer = createV2RuntimeConsumer({
    session: {
      get: () => Effect.sync(() => ({ revert: reverted ? { messageID: "boundary" } : undefined })),
      prompt: () => Effect.void,
      synthetic: (input) => Effect.sync(() => synthetic.push(input)),
    },
  });
  const sends: string[] = [];
  const bridge = {
    send: async (command: string) => {
      sends.push(command);
      if (command === "bash_drain_completions")
        return { success: true, bg_completions: [completion], pending_matches: [] };
      return { success: true, acked_task_ids: [completion.task_id] };
    },
  };
  try {
    consumer.registerSession?.("s");
    await consumer.bridgeOptions.onBashCompletion?.(completion, bridge as never);
    expect(synthetic).toEqual([]);
    expect(sends).toEqual(["bash_drain_completions"]);
    reverted = false;
    consumer.registerSession?.("s");
    await settle();
    expect(synthetic).toHaveLength(1);
    // An admitted completion detaches the session's waits, then is acknowledged.
    expect(sends).toEqual([
      "bash_drain_completions",
      "bash_drain_completions",
      "bash_wait_detach",
      "bash_ack_completions",
    ]);
  } finally {
    consumer.dispose();
  }
});
