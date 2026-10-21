import { describe, expect, test } from "bun:test";
import { Effect } from "effect";
import { drainEntry } from "../../src/__tests__/helpers/drain-wire.js";
import type { AftConfig } from "../../src/config.js";
import { __resetSyncWatchAbortForTests, isSyncWatchAborted } from "../../src/sync-watch-abort.js";
import { createV2RuntimeConsumer } from "../../src/wakes/runtime-consumer.js";
import {
  V2SessionDelivery,
  type V2SessionPromptInput,
  type V2SessionSyntheticInput,
} from "../../src/wakes/session-delivery.js";

function recordingSession() {
  const prompts: V2SessionPromptInput[] = [];
  const synthetic: V2SessionSyntheticInput[] = [];
  const modelHistory: string[] = [];
  return {
    prompts,
    synthetic,
    modelHistory,
    session: {
      get: () => Effect.succeed({}),
      prompt: (input: V2SessionPromptInput) =>
        Effect.sync(() => {
          prompts.push(input);
          modelHistory.push(input.text);
          return { id: "wake-1" };
        }),
      synthetic: (input: V2SessionSyntheticInput) =>
        Effect.sync(() => {
          synthetic.push(input);
          return { id: "status-1" };
        }),
    },
  };
}

describe("V2 session delivery", () => {
  test("admits one synthetic steer wake per completed task and uses the default resume behavior", async () => {
    const host = recordingSession();
    const delivery = new V2SessionDelivery(host.session);
    const wake = {
      sessionID: "session-v2",
      taskIDs: ["bash-aborted-then-completed"],
      text: "[BACKGROUND BASH COMPLETED] bash-aborted-then-completed",
    };
    const startedAt = Date.now();

    await Effect.runPromise(delivery.completion(wake));
    await Effect.runPromise(delivery.completion(wake));

    expect(Date.now() - startedAt).toBeLessThan(45_000);
    expect(host.synthetic).toEqual([
      {
        sessionID: "session-v2",
        text: wake.text,
        description: "Background bash completed",
        delivery: "steer",
        // Keep the wake's origin even when the caller passes no metadata.
        metadata: { source: "aft", task_ids: ["bash-aborted-then-completed"] },
      },
    ]);
    expect(host.synthetic[0]).not.toHaveProperty("resume");
    expect(host.prompts).toEqual([]);
  });

  test("records status-only updates as non-resuming synthetic entries", async () => {
    const host = recordingSession();
    const delivery = new V2SessionDelivery(host.session);

    await Effect.runPromise(
      delivery.status({
        sessionID: "session-v2",
        text: "AFT index is ready",
        description: "AFT status",
        metadata: { phase: "ready" },
      }),
    );

    expect(host.prompts).toEqual([]);
    expect(host.modelHistory).toEqual([]);
    expect(host.synthetic).toEqual([
      {
        sessionID: "session-v2",
        text: "AFT index is ready",
        description: "AFT status",
        metadata: { phase: "ready" },
        resume: false,
      },
    ]);
  });

  test("delivers and acknowledges an idle bridge completion exactly once", async () => {
    const host = recordingSession();
    const consumer = createV2RuntimeConsumer({
      location: { directory: "/work/project", project: { canonical: "/work/project" } },
      session: host.session,
    });
    const sends: Array<{ command: string; params: unknown }> = [];
    let pendingCompletion = true;
    const bridge = {
      getCwd: () => "/work/project",
      send: async (command: string, params: unknown) => {
        sends.push({ command, params });
        if (command === "bash_drain_completions")
          return {
            success: true,
            bg_completions: pendingCompletion ? [drainEntry(completion)] : [],
            pending_matches: [],
          };
        if (command === "bash_ack_completions") pendingCompletion = false;
        return { success: true, acked_task_ids: ["bash-1"] };
      },
    };
    const completion = {
      type: "bash_completed" as const,
      session_id: "session-v2",
      task_id: "bash-1",
      status: "completed",
      exit_code: 0,
      command: "sleep 1",
      output_preview: "done",
    };

    const execution = consumer.executeBash?.({
      name: "bash",
      input: { command: "sleep 1", background: true },
      context: {
        sessionID: "session-v2",
        directory: "/work/project",
        worktree: "/work/project",
        abort: new AbortController().signal,
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
    await execution;
    await consumer.bridgeOptions.onBashCompletion?.(completion, bridge as never);
    await consumer.bridgeOptions.onBashCompletion?.(completion, bridge as never);

    expect(host.prompts).toEqual([]);
    expect(host.synthetic).toHaveLength(1);
    expect(host.synthetic[0]).toMatchObject({
      sessionID: "session-v2",
      description: "Background bash completed",
      delivery: "steer",
      metadata: {
        source: "aft",
        kind: "bash_completion",
        status: "completed",
        task_ids: ["bash-1"],
      },
    });
    // The duplicate push only validates durable state; it cannot wake or ACK again.
    expect(sends).toEqual([
      { command: "bash_drain_completions", params: { session_id: "session-v2" } },
      { command: "bash_wait_detach", params: { session_id: "session-v2" } },
      {
        command: "bash_ack_completions",
        params: { session_id: "session-v2", task_ids: ["bash-1"] },
      },
      { command: "bash_drain_completions", params: { session_id: "session-v2" } },
    ]);
    consumer.dispose();
  });

  test("routes long-running status through synthetic without waking the model", async () => {
    const host = recordingSession();
    const consumer = createV2RuntimeConsumer({
      location: { directory: "/work/project", project: { canonical: "/work/project" } },
      session: host.session,
    });
    await consumer.executeBash?.({
      name: "bash",
      input: { command: "sleep 60", background: true },
      context: {
        sessionID: "session-v2",
        directory: "/work/project",
        worktree: "/work/project",
        abort: new AbortController().signal,
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

    await consumer.bridgeOptions.onBashLongRunning?.(
      {
        type: "bash_long_running",
        session_id: "session-v2",
        task_id: "bash-1",
        command: "sleep 60",
        elapsed_ms: 45_000,
      },
      { getCwd: () => "/work/project" } as never,
    );

    expect(host.prompts).toEqual([]);
    expect(host.modelHistory).toEqual([]);
    expect(host.synthetic).toHaveLength(1);
    expect(host.synthetic[0]).toMatchObject({
      sessionID: "session-v2",
      resume: false,
      metadata: { source: "aft", kind: "bash_long_running", task_id: "bash-1" },
    });
    consumer.dispose();
  });

  test("does not admit a wake or status update after disposal begins", async () => {
    const host = recordingSession();
    const delivery = new V2SessionDelivery(host.session);
    delivery.dispose();

    await Effect.runPromise(
      delivery.completion({
        sessionID: "session-v2",
        taskIDs: ["bash-1"],
        text: "completed",
      }),
    );
    await Effect.runPromise(delivery.status({ sessionID: "session-v2", text: "still indexing" }));

    expect(host.prompts).toEqual([]);
    expect(host.synthetic).toEqual([]);
  });
});

describe("V2 completion wakes end the session's waits", () => {
  const completion = (sessionID: string, taskID: string) => ({
    type: "bash_completed" as const,
    session_id: sessionID,
    task_id: taskID,
    status: "completed",
    exit_code: 0,
    command: "sleep 1",
    output_preview: "done",
  });

  function bridge(frame: ReturnType<typeof completion>) {
    const sends: Array<{ command: string; params: unknown }> = [];
    return {
      sends,
      bridge: {
        getCwd: () => "/work/project",
        send: async (command: string, params: unknown) => {
          sends.push({ command, params });
          if (command === "bash_drain_completions")
            return { success: true, bg_completions: [drainEntry(frame)], pending_matches: [] };
          return {
            success: true,
            detached: command === "bash_wait_detach",
            acked_task_ids: [frame.task_id],
          };
        },
      },
    };
  }

  async function deliver(config: AftConfig, sessionID: string) {
    const host = recordingSession();
    const consumer = createV2RuntimeConsumer({ session: host.session }, () => config);
    consumer.registerSession?.(sessionID);
    const pushed = bridge(completion(sessionID, "bash-wake"));
    await consumer.bridgeOptions.onBashCompletion?.(
      completion(sessionID, "bash-wake"),
      pushed.bridge as never,
    );
    consumer.dispose();
    return { host, sends: pushed.sends };
  }

  test("an AFT completion wake delivered as steer detaches its session's wait and sync watch", async () => {
    __resetSyncWatchAbortForTests();
    const { host, sends } = await deliver({}, "session-wake");
    expect(host.synthetic).toHaveLength(1);
    expect(host.synthetic[0]?.delivery).toBe("steer");
    expect(sends.filter((send) => send.command === "bash_wait_detach")).toEqual([
      { command: "bash_wait_detach", params: { session_id: "session-wake" } },
    ]);
    expect(isSyncWatchAborted("session-wake")).toBe(true);
    expect(isSyncWatchAborted("another-session")).toBe(false);
    __resetSyncWatchAbortForTests();
  });

  test("bash.detach_on_user_message: false keeps the wait blocking", async () => {
    __resetSyncWatchAbortForTests();
    const { host, sends } = await deliver(
      { bash: { detach_on_user_message: false } },
      "session-kept",
    );
    expect(host.synthetic).toHaveLength(1);
    expect(sends.map((send) => send.command)).toEqual([
      "bash_drain_completions",
      "bash_ack_completions",
    ]);
    expect(isSyncWatchAborted("session-kept")).toBe(false);
  });
});
