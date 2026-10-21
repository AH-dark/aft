import { expect, test } from "bun:test";
import { Effect } from "effect";
import { createBashTool } from "../tools/bash.js";
import { createBashWatchTool } from "../tools/bash_watch.js";
import { projectV2Tool } from "../tools/definitions/v2.js";
import { createReadTool } from "../tools/hoisted.js";
import { safetyTools } from "../tools/safety.js";
import type { PluginContext } from "../types.js";

const directory = process.cwd();
const location = { directory };
const execution = { progress: () => Effect.void };
const json = JSON.stringify({ success: false, code: "fixture", message: "application data" });

function context(success: boolean): PluginContext {
  const bridge = {
    toolCall: async () => ({ success, text: json, message: "engine failure" }),
    send: async () => ({ success, status: "completed", output: json, message: "engine failure" }),
  };
  return { client: {}, config: {}, pool: { getBridge: () => bridge } } as unknown as PluginContext;
}

test("V2 read preserves failure-shaped JSON file contents as text", async () => {
  const tool = projectV2Tool("read", createReadTool(context(true)), location, {
    requestPermission: async () => {},
  });
  const result = await Effect.runPromise(tool.execute({ path: "fixture.json" }, execution));
  expect(result.content).toBe(json);
});

test("V2 shell preserves failure-shaped JSON stdout as text", async () => {
  const tool = projectV2Tool("bash", createBashTool(context(true)), location);
  const result = await Effect.runPromise(tool.execute({ command: "application" }, execution));
  expect(result.content).toBe(json);
});

test("V2 genuine engine failure is selected from the structured reply, not its text", async () => {
  const tool = projectV2Tool("safety", safetyTools(context(false)).aft_safety!, location);
  const error = await Effect.runPromise(Effect.flip(tool.execute({ op: "list" }, execution)));
  expect(error.message).toBe("engine failure");
});

test("V2 unknown role obeys the subagent foreground restriction", async () => {
  const calls: Record<string, unknown>[] = [];
  const bridge = {
    send: async (_command: string, args: Record<string, unknown>) => {
      calls.push(args);
      return { success: true, output: "done", status: "completed" };
    },
  };
  const ctx = {
    client: { location, session: { get: () => Effect.fail(new Error("unavailable")) } },
    config: { bash: { subagent_background: false } },
    pool: { getBridge: () => bridge },
  } as unknown as PluginContext;
  const tool = projectV2Tool("bash", createBashTool(ctx), location);
  await Effect.runPromise(
    tool.execute(
      { command: "build", background: true },
      { ...execution, sessionID: "unknown-worker" },
    ),
  );
  expect(calls[0]?.background).toBe(false);
  expect(calls[0]?.block_to_completion).toBe(true);
});

test("V2 unknown role turns an async watch into a wait, as for a subagent", async () => {
  const commands: string[] = [];
  const bridge = {
    send: async (command: string) => {
      commands.push(command);
      return { success: true, status: "completed", output: "done", exit_code: 0 };
    },
  };
  const ctx = {
    client: { location, session: { get: () => Effect.fail(new Error("unavailable")) } },
    config: { bash: { subagent_background: false } },
    pool: { getBridge: () => bridge },
  } as unknown as PluginContext;
  await createBashWatchTool(ctx).execute(
    { taskId: "bash-unknown", background: true, pattern: "done" },
    {
      sessionID: "unknown-watcher",
      directory,
      abort: new AbortController().signal,
      metadata: () => {},
    } as never,
  );
  expect(commands).not.toContain("bash_notify");
  expect(commands).toContain("bash_status");
});
