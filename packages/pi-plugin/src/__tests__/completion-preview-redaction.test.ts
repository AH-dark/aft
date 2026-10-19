import { afterEach, expect, test } from "bun:test";
import { printedCredentialCompletion } from "../../../aft-bridge/src/__tests__/test-utils/completion-preview.js";
import {
  __resetBgNotificationStateForTests,
  appendToolResultBgCompletions,
  ingestBgCompletions,
  trackBgTask,
} from "../bg-notifications.js";

afterEach(__resetBgNotificationStateForTests);

test("Pi completion wake masks a printed GitHub credential", async () => {
  const completion = await printedCredentialCompletion();
  trackBgTask("s", completion.task_id);
  ingestBgCompletions("s", [completion]);
  const content = [{ type: "text" as const, text: "ordinary tool result" }];
  const bridge = { send: async () => ({ success: true, completions: [] }) };
  const output = await appendToolResultBgCompletions(
    { directory: "/work", sessionID: "s", ctx: { pool: { getBridge: () => bridge } } } as never,
    content,
  );
  expect(JSON.stringify(output)).toContain("<REDACTED_SECRET>");
  expect(JSON.stringify(output)).not.toContain(`ghp_${"a".repeat(36)}`);
}, 30_000);
