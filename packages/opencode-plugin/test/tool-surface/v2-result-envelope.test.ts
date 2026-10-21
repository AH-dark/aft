import { describe, expect, test } from "bun:test";
import type { ToolDefinition } from "@opencode-ai/plugin";
import { Effect, Exit } from "effect";
import { projectV2Tool } from "../../src/tools/definitions/v2.js";
import { safetyTools } from "../../src/tools/safety.js";
import type { PluginContext } from "../../src/types.js";

const LOCATION = {
  directory: process.cwd(),
  project: { directory: process.cwd(), canonical: process.cwd() },
};
const EXECUTION_CONTEXT = {
  sessionID: "session-envelope",
  messageID: "message-envelope",
  id: "call-envelope",
  agent: "agent-envelope",
  progress: () => Effect.succeed(undefined),
};

function toolReturning(output: unknown): ReturnType<typeof projectV2Tool> {
  if (output && typeof output === "object" && "success" in output) {
    const response = output as Record<string, unknown>;
    const bridge = { toolCall: async () => ({ text: JSON.stringify(response), ...response }) };
    const ctx = {
      config: {},
      client: {},
      pool: { getBridge: () => bridge },
    } as unknown as PluginContext;
    const definition = safetyTools(ctx).aft_safety as ToolDefinition;
    return projectV2Tool(
      "safety",
      { ...definition, execute: (_args, runtime) => definition.execute({ op: "list" }, runtime) },
      LOCATION,
    );
  }
  const definition = {
    description: "result envelope probe",
    args: {},
    execute: async () => output,
  } as unknown as ToolDefinition;
  return projectV2Tool("aft_probe", definition, LOCATION);
}

async function runExit(output: unknown) {
  return await Effect.runPromiseExit(toolReturning(output).execute({}, EXECUTION_CONTEXT));
}

describe("V2 tool result envelopes", () => {
  test("a structured engine failure fails the call instead of reporting success", async () => {
    const message = 'Permission denied (external_directory) for "/etc/hosts".';
    const exit = await runExit({
      success: false,
      code: "permission_denied",
      message,
      error: message,
    });

    expect(Exit.isFailure(exit)).toBe(true);
  });

  test("the failure text is the rendered message, with no JSON and no duplicated field", async () => {
    const message = "no undo history for: operation";
    const error = await Effect.runPromise(
      Effect.flip(
        toolReturning({ success: false, code: "no_history", message, error: message }).execute(
          {},
          EXECUTION_CONTEXT,
        ),
      ),
    );

    expect(error).toBeInstanceOf(Error);
    expect((error as Error).message).toBe(message);
    expect((error as Error).message).not.toContain('"success"');
    expect((error as Error).message).not.toContain('"error"');
  });

  test("leaves ordinary output alone, including JSON that is not a failure envelope", async () => {
    const succeeded = JSON.stringify({ success: true, text: "done" });
    const unrelated = JSON.stringify({ results: [{ success: false }] });

    expect(
      await Effect.runPromise(toolReturning(succeeded).execute({}, EXECUTION_CONTEXT)),
    ).toEqual({ content: succeeded });
    expect(
      await Effect.runPromise(toolReturning(unrelated).execute({}, EXECUTION_CONTEXT)),
    ).toEqual({ content: unrelated });
    expect(
      await Effect.runPromise(toolReturning("plain text output").execute({}, EXECUTION_CONTEXT)),
    ).toEqual({ content: "plain text output" });
  });

  test("structured tool output containing failure-shaped JSON remains data", async () => {
    const output = JSON.stringify({ success: false, message: "application data" });
    const result = await Effect.runPromise(
      toolReturning({ output, title: "read" }).execute({}, EXECUTION_CONTEXT),
    );
    expect(result.content).toBe(output);
  });
});
