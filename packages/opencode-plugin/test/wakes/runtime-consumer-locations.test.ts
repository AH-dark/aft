import { afterEach, describe, expect, spyOn, test } from "bun:test";
import { fileURLToPath } from "node:url";
import { Effect } from "effect";

import * as logger from "../../src/logger.js";
import { __resetV2WakeRoutesForTests } from "../../src/wakes/runtime-consumer.js";
import type {
  V2SessionPromptInput,
  V2SessionSyntheticInput,
} from "../../src/wakes/session-delivery.js";

type RuntimeConsumerModule = typeof import("../../src/wakes/runtime-consumer.js");

/**
 * OpenCode 2 builds a runtime per Location and can load a separate copy of the
 * plugin's module graph for each. Importing the module by path with distinct
 * query strings gives two independent module instances in Bun, which is what
 * two Locations in one host process see.
 */
async function loadLocationCopy(label: string): Promise<RuntimeConsumerModule> {
  const path = fileURLToPath(new URL("../../src/wakes/runtime-consumer.ts", import.meta.url));
  return (await import(`${path}?location=${label}`)) as RuntimeConsumerModule;
}

function recordingSession() {
  const prompts: V2SessionPromptInput[] = [];
  const synthetic: V2SessionSyntheticInput[] = [];
  return {
    prompts,
    synthetic,
    session: {
      get: () => Effect.succeed({}),
      prompt: (input: V2SessionPromptInput) =>
        Effect.sync(() => {
          prompts.push(input);
          return { id: "wake" };
        }),
      synthetic: (input: V2SessionSyntheticInput) =>
        Effect.sync(() => {
          synthetic.push(input);
          return { id: "status" };
        }),
    },
  };
}

function recordingBridge(completions: ReturnType<typeof completion>[]) {
  const pending = new Map(completions.map((entry) => [entry.task_id, entry]));
  const sends: Array<{ command: string; params: unknown }> = [];
  return {
    sends,
    bridge: {
      getCwd: () => "/work/first",
      send: async (command: string, params: unknown) => {
        sends.push({ command, params });
        if (command === "bash_drain_completions")
          return { success: true, bg_completions: [...pending.values()], pending_matches: [] };
        const ids = (params as { task_ids?: string[] }).task_ids ?? [];
        return { success: true, acked_task_ids: ids.filter((id) => pending.delete(id)) };
      },
    },
  };
}

/** Run one background bash call, which is how a session registers with its Location. */
async function runBash(
  consumer: ReturnType<RuntimeConsumerModule["createV2RuntimeConsumer"]>,
  sessionID: string,
  directory: string,
): Promise<void> {
  const abort = new AbortController().signal;
  await consumer.executeBash?.({
    name: "bash",
    input: { command: "sleep 1", background: true },
    context: {
      sessionID,
      directory,
      worktree: directory,
      abort,
      effectAbort: abort,
      metadata: () => {},
      ask: async () => {},
      progress: () => Effect.succeed(undefined),
    },
    definition: {
      description: "background bash",
      args: {},
      execute: async () => "bash-1",
    },
  });
}

function completion(sessionID: string, taskID: string) {
  return {
    type: "bash_completed" as const,
    session_id: sessionID,
    task_id: taskID,
    status: "completed",
    exit_code: 0,
    command: "sleep 1",
    output_preview: "done",
  };
}

async function until(condition: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 100 && !condition(); attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}

afterEach(() => {
  __resetV2WakeRoutesForTests();
});

describe("V2 wake routing across Locations", () => {
  test("a completion pushed through the first Location's callback reaches a session of a second Location's module copy", async () => {
    const first = await loadLocationCopy("first");
    const second = await loadLocationCopy("second");
    expect(first).not.toBe(second);

    const firstHost = recordingSession();
    const secondHost = recordingSession();
    const firstConsumer = first.createV2RuntimeConsumer({ session: firstHost.session });
    const secondConsumer = second.createV2RuntimeConsumer({ session: secondHost.session });
    await runBash(secondConsumer, "session-second", "/work/second");

    // The shared bridge pool keeps the callbacks of the Location that created
    // it, so every push arrives through the first copy.
    const { bridge, sends } = recordingBridge([completion("session-second", "bash-second")]);
    await firstConsumer.bridgeOptions.onBashCompletion?.(
      completion("session-second", "bash-second"),
      bridge as never,
    );

    expect(firstHost.prompts).toEqual([]);
    expect(secondHost.prompts).toEqual([]);
    expect(secondHost.synthetic).toHaveLength(1);
    expect(secondHost.synthetic[0]).toMatchObject({
      sessionID: "session-second",
      metadata: { kind: "bash_completion", task_ids: ["bash-second"] },
    });
    expect(sends).toEqual([
      { command: "bash_drain_completions", params: { session_id: "session-second" } },
      { command: "bash_wait_detach", params: { session_id: "session-second" } },
      {
        command: "bash_ack_completions",
        params: { session_id: "session-second", task_ids: ["bash-second"] },
      },
    ]);

    await firstConsumer.bridgeOptions.onBashLongRunning?.(
      {
        type: "bash_long_running",
        session_id: "session-second",
        task_id: "bash-second",
        command: "sleep 60",
        elapsed_ms: 45_000,
      },
      bridge as never,
    );
    expect(firstHost.synthetic).toEqual([]);
    expect(secondHost.synthetic).toHaveLength(2);

    firstConsumer.dispose();
    secondConsumer.dispose();
  });

  test("disposing one Location removes only its own sessions from the shared table", async () => {
    const first = await loadLocationCopy("dispose-first");
    const second = await loadLocationCopy("dispose-second");
    const firstHost = recordingSession();
    const secondHost = recordingSession();
    const firstConsumer = first.createV2RuntimeConsumer({ session: firstHost.session });
    const secondConsumer = second.createV2RuntimeConsumer({ session: secondHost.session });
    await runBash(firstConsumer, "session-first", "/work/first");
    await runBash(secondConsumer, "session-second", "/work/second");

    firstConsumer.dispose();
    const { bridge, sends } = recordingBridge([completion("session-second", "bash-after-dispose")]);
    await firstConsumer.bridgeOptions.onBashCompletion?.(
      completion("session-second", "bash-after-dispose"),
      bridge as never,
    );

    expect(secondHost.synthetic).toHaveLength(1);
    expect(sends.map((send) => send.command)).toEqual([
      "bash_drain_completions",
      "bash_wait_detach",
      "bash_ack_completions",
    ]);
    secondConsumer.dispose();
  });

  test("an unrouted completion is logged once, left unacknowledged, and delivered when its session registers", async () => {
    const first = await loadLocationCopy("late-first");
    const second = await loadLocationCopy("late-second");
    const firstConsumer = first.createV2RuntimeConsumer({ session: recordingSession().session });
    const warn = spyOn(logger, "warn");
    try {
      const { bridge, sends } = recordingBridge([
        completion("session-late", "bash-late-1"),
        completion("session-late", "bash-late-2"),
      ]);
      await firstConsumer.bridgeOptions.onBashCompletion?.(
        completion("session-late", "bash-late-1"),
        bridge as never,
      );
      await firstConsumer.bridgeOptions.onBashCompletion?.(
        completion("session-late", "bash-late-2"),
        bridge as never,
      );
      await firstConsumer.bridgeOptions.onBashLongRunning?.(
        {
          type: "bash_long_running",
          session_id: "session-late",
          task_id: "bash-late-3",
          command: "sleep 60",
          elapsed_ms: 45_000,
        },
        bridge as never,
      );

      const unrouted = warn.mock.calls.filter(([message]) =>
        String(message).includes("session-late"),
      );
      expect(unrouted).toHaveLength(1);
      expect(String(unrouted[0]?.[0])).toContain("bash-late-1");
      expect(sends).toEqual([]);

      // The session's Location registers it on its next bash call; both held
      // completions are then delivered and acknowledged on the pushing bridge.
      const secondHost = recordingSession();
      const secondConsumer = second.createV2RuntimeConsumer({ session: secondHost.session });
      await runBash(secondConsumer, "session-late", "/work/second");
      // Each delivered completion detaches the session's waits, then is acknowledged.
      await until(
        () => sends.filter((send) => send.command === "bash_ack_completions").length === 2,
      );

      expect(secondHost.synthetic.map((entry) => entry.metadata?.task_ids)).toEqual([
        ["bash-late-1"],
        ["bash-late-2"],
      ]);
      expect(sends.filter((send) => send.command === "bash_ack_completions")).toEqual([
        {
          command: "bash_ack_completions",
          params: { session_id: "session-late", task_ids: ["bash-late-1"] },
        },
        {
          command: "bash_ack_completions",
          params: { session_id: "session-late", task_ids: ["bash-late-2"] },
        },
      ]);
      expect(sends.filter((send) => send.command === "bash_wait_detach")).toEqual([
        { command: "bash_wait_detach", params: { session_id: "session-late" } },
        { command: "bash_wait_detach", params: { session_id: "session-late" } },
      ]);

      // Held completions are handed over once; a second bash call delivers nothing new.
      await runBash(secondConsumer, "session-late", "/work/second");
      await new Promise((resolve) => setTimeout(resolve, 20));
      expect(secondHost.synthetic).toHaveLength(2);
      secondConsumer.dispose();
    } finally {
      warn.mockRestore();
      firstConsumer.dispose();
    }
  });
});
