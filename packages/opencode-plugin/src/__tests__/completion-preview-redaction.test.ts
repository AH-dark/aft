import { afterEach, expect, test } from "bun:test";
import { Effect } from "effect";
import { printedCredentialCompletion } from "../../../aft-bridge/src/__tests__/test-utils/completion-preview.js";
import {
  __resetBgNotificationStateForTests,
  appendInTurnBgCompletions,
  ingestBgCompletions,
  trackBgTask,
} from "../bg-notifications.js";
import { projectV2Tool } from "../tools/definitions/v2.js";
import { __resetV2WakeRoutesForTests, createV2RuntimeConsumer } from "../wakes/runtime-consumer.js";

afterEach(() => {
  __resetBgNotificationStateForTests();
  __resetV2WakeRoutesForTests();
});

test("OpenCode 1 completion wake masks a printed GitHub credential", async () => {
  const completion = await printedCredentialCompletion();
  trackBgTask("s", completion.task_id);
  ingestBgCompletions("s", [completion]);
  const output = { output: "ordinary tool result" };
  const bridge = { send: async () => ({ success: true, completions: [] }) };
  await appendInTurnBgCompletions(
    { directory: "/work", sessionID: "s", ctx: { pool: { getBridge: () => bridge } } } as never,
    output,
  );
  expect(output.output).toContain("<REDACTED_SECRET>");
  expect(output.output).not.toContain(`ghp_${"a".repeat(36)}`);
}, 30_000);

test("OpenCode 2 completion wake masks a printed GitHub credential", async () => {
  const completion = await printedCredentialCompletion();
  const delivered: unknown[] = [];
  const consumer = createV2RuntimeConsumer({
    session: {
      get: () => Effect.succeed({}),
      prompt: () => Effect.void,
      synthetic: (input) =>
        Effect.sync(() => {
          delivered.push(input);
        }),
    },
  });
  try {
    const tool = projectV2Tool(
      "bash",
      { description: "bash", args: {}, execute: async () => "ok" },
      { directory: "/work" },
      consumer,
    );
    await Effect.runPromise(tool.execute({}, { sessionID: "s", progress: () => Effect.void }));
    await consumer.bridgeOptions.onBashCompletion?.({ ...completion, session_id: "s" }, {
      send: async (command: string) =>
        command === "bash_drain_completions"
          ? {
              success: true,
              bg_completions: [{ ...completion, session_id: "s" }],
              pending_matches: [],
            }
          : { success: true, acked_task_ids: [completion.task_id] },
    } as never);
    expect(delivered).toHaveLength(1);
    expect(JSON.stringify(delivered)).toContain("<REDACTED_SECRET>");
    expect(JSON.stringify(delivered)).not.toContain(`ghp_${"a".repeat(36)}`);
  } finally {
    consumer.dispose();
  }
}, 30_000);
