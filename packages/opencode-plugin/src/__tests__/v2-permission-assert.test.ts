import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import { Error as ToolError } from "@opencode/plugin/promise/tool";
import { Cause, Effect } from "effect";

import { makeServerEffect } from "../entry/server-runtime.mjs";
import {
  decidePermission,
  PermissionDeniedError,
  PermissionRulesUnavailableError,
  requestPermission,
  type V2PermissionHostContext,
  type V2PermissionRule,
} from "../permissions/v2.js";
import { astTools } from "../tools/ast.js";
import { projectV2Tool, type V2DefinitionRuntime } from "../tools/definitions/v2.js";
import { hoistedV2ToolConsumers } from "../tools/hoisted/v2.js";
import { hoistedTools } from "../tools/hoisted.js";
import { inspectTools } from "../tools/inspect.js";
import { navigationTools } from "../tools/navigation.js";
import { classifyPermissionError } from "../tools/permissions.js";
import { readingTools } from "../tools/reading.js";
import { searchTools } from "../tools/search.js";
import type { PluginContext } from "../types.js";

const request = {
  permission: "edit",
  patterns: ["src/a.ts"],
  always: ["src/*"],
  metadata: { diff: "-old\n+new", reason: "Narrow the edit" },
};
const context = {
  sessionID: "ses_test",
  messageID: "msg_test",
  id: "call_test",
  agent: "build",
  progress: () => Effect.void,
};

const defaultRules: readonly V2PermissionRule[] = [
  { action: "*", resource: "*", effect: "allow" },
  { action: "external_directory", resource: "*", effect: "ask" },
  { action: "read", resource: "*.env", effect: "ask" },
  { action: "read", resource: "*.env.*", effect: "ask" },
  { action: "read", resource: "*.env.example", effect: "allow" },
];
const ruleRequest = {
  permission: "edit",
  patterns: ["file.ts"],
  always: ["file.ts"],
  metadata: { filepath: "/work/project/file.ts", diff: "@@ -1 +1 @@" },
};

interface FixtureOptions {
  agentRules?: readonly V2PermissionRule[];
  sessionRules?: readonly V2PermissionRule[];
  sessionAgent?: string;
  agentFailure?: Error;
  sessionFailure?: Error;
  rejection?: Error;
}

function fixture(options: FixtureOptions = {}) {
  const calls: unknown[] = [];
  const agents: string[] = [];
  const sessions: string[] = [];
  const agentRules = Object.hasOwn(options, "agentRules")
    ? options.agentRules
    : [{ action: "edit", resource: "src/a.ts", effect: "ask" as const }];
  const host: V2PermissionHostContext = {
    agent: {
      get: ({ agentID }) =>
        Effect.suspend(() => {
          agents.push(agentID);
          if (options.agentFailure) return Effect.fail(options.agentFailure);
          return Effect.succeed({
            data: agentRules === undefined ? {} : { permissions: agentRules },
          });
        }),
    },
    session: {
      get: ({ sessionID }) =>
        Effect.suspend(() => {
          sessions.push(sessionID);
          if (options.sessionFailure) return Effect.fail(options.sessionFailure);
          return Effect.succeed({
            ...(options.sessionAgent === undefined ? {} : { agent: options.sessionAgent }),
            ...(options.sessionRules === undefined ? {} : { permissions: options.sessionRules }),
          });
        }),
    },
    permission: {
      assert: (input) =>
        Effect.suspend(() => {
          calls.push(input);
          return options.rejection instanceof ToolError
            ? Effect.fail(options.rejection)
            : options.rejection
              ? Effect.die(options.rejection)
              : Effect.void;
        }),
    },
  };
  return { host, calls, agents, sessions };
}

function pluginContext(
  bridgeResponse: (name: string, preview: boolean) => Record<string, unknown>,
  bridgeSend?: (command: string, params: Record<string, unknown>) => Record<string, unknown>,
): PluginContext {
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      if (!bridgeSend) throw new Error(`unexpected ${command}`);
      return bridgeSend(command, params);
    },
    toolCall: async (
      _sessionID: string | undefined,
      name: string,
      _args: Record<string, unknown>,
      options?: { preview?: boolean },
    ) => bridgeResponse(name, options?.preview === true),
  };
  return {
    pool: { getBridge: () => bridge } as unknown as BridgePool,
    client: {},
    config: { tool_surface: "all", hoist_builtin_tools: true },
    hashlineEffective: false,
    storageDir: "/isolated/storage",
  } as PluginContext;
}

function projectedFilesystemTools(
  host: V2PermissionHostContext,
  bridgeResponse: (name: string, preview: boolean) => Record<string, unknown>,
) {
  const definitions = hoistedTools(pluginContext(bridgeResponse));
  const consumers = hoistedV2ToolConsumers(host);
  const location = { directory: process.cwd() };
  return {
    read: projectV2Tool("read", definitions.read!, location, consumers),
    write: projectV2Tool("write", definitions.write!, location, consumers),
    edit: projectV2Tool("edit", definitions.edit!, location, consumers),
    aft_delete: projectV2Tool("aft_delete", definitions.aft_delete!, location, consumers),
    aft_move: projectV2Tool("aft_move", definitions.aft_move!, location, consumers),
  };
}

async function executeProjected(
  tool: ReturnType<typeof projectV2Tool>,
  input: Record<string, unknown>,
) {
  return await Effect.runPromise(tool.execute(input, context));
}

function artifactReadTool(
  owned: boolean,
  name = "read",
  artifact = "/aft-v2-artifact-fixture/opencode/bash-tasks/bgb-0123abcd/stdout",
) {
  const ownershipQueries: Record<string, unknown>[] = [];
  const plugin = pluginContext(
    () => ({ success: true, text: "task output" }),
    (command, params) => {
      if (command !== "bash_artifact_owned") throw new Error(`unexpected ${command}`);
      ownershipQueries.push(params);
      return {
        success: true,
        owned: owned && params.path === artifact && params.session_id === "ses_test",
      };
    },
  );
  const { host, calls } = fixture({ agentRules: defaultRules });
  const definitions = {
    ...hoistedTools(plugin),
    ...searchTools(plugin),
    ...readingTools(plugin),
    ...navigationTools(plugin),
    ...inspectTools(plugin),
    ...astTools(plugin),
  };
  const definition = definitions[name];
  if (!definition) throw new Error(`the ${name} tool definition was not created`);
  const read = projectV2Tool(
    name,
    definition,
    { directory: process.cwd() },
    hoistedV2ToolConsumers(host),
  );
  return { read, artifact, calls, ownershipQueries };
}

describe("OpenCode 2 in-process permission assertion", () => {
  test("configured allow skips assert", async () => {
    const { host, calls } = fixture({
      agentRules: [{ action: "edit", resource: "src/a.ts", effect: "allow" }],
      rejection: new Error("must not ask"),
    });
    await requestPermission(host, request, context);
    expect(calls).toEqual([]);
  });

  test("configured deny skips assert and reports the rule", async () => {
    const { host, calls } = fixture({
      agentRules: [{ action: "edit", resource: "src/a.ts", effect: "deny" }],
    });
    await expect(requestPermission(host, request, context)).rejects.toThrow('"src/a.ts"');
    expect(calls).toEqual([]);
  });

  test("ask awaits assert and preserves edit diff and source mapping", async () => {
    const { host, calls } = fixture();
    let finish!: () => void;
    host.permission.assert = (input) =>
      Effect.promise(async () => {
        calls.push(input);
        await new Promise<void>((resolve) => {
          finish = resolve;
        });
      });
    let completed = false;
    const pending = requestPermission(host, request, context).then(() => {
      completed = true;
    });
    for (let turn = 0; turn < 20 && !finish; turn++) await Promise.resolve();
    expect(completed).toBe(false);
    expect(calls).toEqual([
      {
        sessionID: "ses_test",
        agent: "build",
        action: "edit",
        resources: ["src/a.ts"],
        save: ["src/*"],
        metadata: request.metadata,
        source: { type: "tool", messageID: "msg_test", id: "call_test" },
      },
    ]);
    finish();
    await pending;
    expect(completed).toBe(true);
  });

  test("allows requests under OpenCode's default rules without asserting", async () => {
    const { host, calls, agents, sessions } = fixture({ agentRules: defaultRules });
    await requestPermission(host, ruleRequest, context);

    expect(agents).toEqual(["build"]);
    expect(sessions).toEqual(["ses_test"]);
    expect(calls).toEqual([]);
  });

  test("reads inside the project root need no permission under the default ruleset", async () => {
    const { host, calls } = fixture({ agentRules: defaultRules });
    await requestPermission(
      host,
      { permission: "read", patterns: ["src/index.ts"], always: ["*"], metadata: {} },
      context,
    );

    expect(calls).toEqual([]);
  });

  test("keeps the host's dotenv ask rules and the example-file allow exception", async () => {
    const { host, calls } = fixture({ agentRules: defaultRules });
    const read = (resource: string) =>
      requestPermission(
        host,
        { permission: "read", patterns: [resource], always: ["*"], metadata: {} },
        context,
      );

    await read(".env");
    await read(".env.local");
    await read(".env.example");

    expect(calls).toHaveLength(2);
    expect(calls.map((call) => (call as { resources: string[] }).resources)).toEqual([
      [".env"],
      [".env.local"],
    ]);
  });

  test("denies with the deciding rule and never asserts", async () => {
    const { host, calls } = fixture({
      agentRules: [
        { action: "*", resource: "*", effect: "allow" },
        { action: "edit", resource: "file.ts", effect: "deny" },
      ],
    });
    const error = await requestPermission(host, ruleRequest, context).catch(
      (caught: unknown) => caught as Error,
    );

    expect(error).toBeInstanceOf(PermissionDeniedError);
    expect(error.message).toContain('"edit" on "file.ts" is "deny"');
    expect(calls).toEqual([]);
  });

  test("lets a session rule override the agent rule that precedes it", async () => {
    const { host, calls } = fixture({
      agentRules: [{ action: "*", resource: "*", effect: "allow" }],
      sessionRules: [{ action: "edit", resource: "*", effect: "deny" }],
    });

    await expect(requestPermission(host, ruleRequest, context)).rejects.toBeInstanceOf(
      PermissionDeniedError,
    );
    expect(calls).toEqual([]);
  });

  test("falls back to the session's agent when the call names none", async () => {
    const { host, agents } = fixture({
      agentRules: defaultRules,
      sessionAgent: "agent-from-session",
    });

    await requestPermission(host, ruleRequest, { ...context, agent: undefined });

    expect(agents).toEqual(["agent-from-session"]);
  });

  test("refuses with the reason when agent permission rules cannot be read", async () => {
    const unreadable = fixture({
      agentRules: defaultRules,
      agentFailure: new Error("agent lookup exploded"),
    });
    const agentFailure = await requestPermission(unreadable.host, ruleRequest, context).catch(
      (caught: unknown) => caught as Error,
    );
    expect(agentFailure).toBeInstanceOf(PermissionRulesUnavailableError);
    expect(agentFailure.message).toContain("could not read this session's OpenCode permission");
    expect(agentFailure.message).toContain("agent lookup exploded");

    const nameless = fixture({ agentRules: defaultRules });
    await expect(
      requestPermission(nameless.host, ruleRequest, { ...context, agent: undefined }),
    ).rejects.toBeInstanceOf(PermissionRulesUnavailableError);

    const ruleless = fixture({ agentRules: undefined });
    await expect(requestPermission(ruleless.host, ruleRequest, context)).rejects.toBeInstanceOf(
      PermissionRulesUnavailableError,
    );
  });

  test("refuses with the reason when session rules cannot be read", async () => {
    const { host } = fixture({
      agentRules: defaultRules,
      sessionFailure: new Error("session lookup exploded"),
    });

    await expect(requestPermission(host, ruleRequest, context)).rejects.toThrow(
      "session lookup exploded",
    );
  });

  test("matches actions and resources with the host's wildcard rules", () => {
    const rules: readonly V2PermissionRule[] = [
      { action: "*", resource: "*", effect: "allow" },
      { action: "bash", resource: "git push *", effect: "ask" },
    ];
    const decide = (permission: string, resource: string) =>
      decidePermission({ permission, patterns: [resource], always: [], metadata: {} }, rules)
        .effect;

    expect(decide("bash", "git push")).toBe("ask");
    expect(decide("bash", "git push --force origin main")).toBe("ask");
    expect(decide("bash", "git status")).toBe("allow");
  });

  test("denies a mixed-resource request when any resource is denied", async () => {
    const rules: readonly V2PermissionRule[] = [
      { action: "edit", resource: "a.ts", effect: "allow" },
      { action: "edit", resource: "b.ts", effect: "deny" },
    ];
    const { host, calls } = fixture({ agentRules: rules });
    const mixedRequest = { ...ruleRequest, patterns: ["a.ts", "b.ts"] };
    const decision = decidePermission(mixedRequest, rules);

    expect(decision).toMatchObject({ effect: "deny", resource: "b.ts" });
    await expect(requestPermission(host, mixedRequest, context)).rejects.toBeInstanceOf(
      PermissionDeniedError,
    );
    expect(calls).toEqual([]);
  });

  test("skips assert for a mixed-resource request whose resources are all allowed", async () => {
    const { host, calls } = fixture({
      agentRules: [
        { action: "edit", resource: "a.ts", effect: "allow" },
        { action: "edit", resource: "b.ts", effect: "allow" },
      ],
    });

    await requestPermission(host, { ...ruleRequest, patterns: ["a.ts", "b.ts"] }, context);

    expect(calls).toEqual([]);
  });

  test("asserts exactly once for a mixed request that resolves to ask", async () => {
    const { host, calls } = fixture({
      agentRules: [
        { action: "edit", resource: "a.ts", effect: "allow" },
        { action: "edit", resource: "b.ts", effect: "ask" },
      ],
    });

    await requestPermission(host, { ...ruleRequest, patterns: ["a.ts", "b.ts"] }, context);

    expect(calls).toHaveLength(1);
    expect(calls[0]).toMatchObject({ action: "edit", resources: ["a.ts", "b.ts"] });
  });

  test("hoisted edit preserves its generated PatchDiff metadata for the permission consumer", async () => {
    const seen: unknown[] = [];
    const { host } = fixture({ agentRules: defaultRules });
    const definitions = hoistedTools(
      pluginContext((_name, preview) =>
        preview
          ? { success: true, preview_diff: "@@ -1 +1 @@\n-old\n+new" }
          : { success: true, text: "edited" },
      ),
    );
    const definition = definitions.edit;
    if (!definition) throw new Error("the edit tool definition was not created");
    const consumers = hoistedV2ToolConsumers(host);
    const edit = projectV2Tool(
      "edit",
      definition,
      { directory: process.cwd() },
      {
        requestPermission: (permissionRequest, executionContext) => {
          seen.push(permissionRequest);
          return (
            consumers.requestPermission?.(permissionRequest, executionContext) ?? Promise.resolve()
          );
        },
      },
    );

    await executeProjected(edit, {
      path: "file.ts",
      edits: [{ oldString: "old", newString: "new" }],
    });

    expect(seen[0]).toMatchObject({
      permission: "edit",
      patterns: ["file.ts"],
      metadata: {
        filepath: resolve(process.cwd(), "file.ts"),
        diff: "@@ -1 +1 @@\n-old\n+new",
      },
    });
  });

  test("a session-owned bash artifact is read without asserting permission", async () => {
    const { read, artifact, calls, ownershipQueries } = artifactReadTool(true);

    await executeProjected(read, { path: artifact });

    expect(calls).toEqual([]);
    expect(ownershipQueries).toEqual([{ path: artifact, session_id: "ses_test" }]);
  });

  test("an artifact this session does not own asks for external_directory", async () => {
    const { read, artifact, calls } = artifactReadTool(false);

    await executeProjected(read, { path: artifact });

    expect(calls).toHaveLength(1);
    expect(calls.map((call) => (call as { action: string }).action)).toEqual([
      "external_directory",
    ]);
  });

  const readers = [
    { name: "grep", args: (p: string) => ({ path: p, pattern: "task" }) },
    { name: "glob", args: (p: string) => ({ path: p, pattern: "*" }) },
    { name: "aft_outline", args: (p: string) => ({ target: p }) },
    { name: "aft_zoom", args: (p: string) => ({ path: p, symbols: "task" }) },
    { name: "aft_callgraph", args: (p: string) => ({ path: p, op: "callers", symbol: "task" }) },
    { name: "aft_inspect", args: (p: string) => ({ scope: p, sections: ["todos"] }) },
    {
      name: "ast_grep_search",
      args: (p: string) => ({ paths: [p], lang: "rust", pattern: "fn $F() { $$$ }" }),
    },
  ];
  for (const scenario of ["own", "foreign", "directory", "pattern"] as const) {
    test(`exact-file readers ${scenario} artifact permission boundary`, async () => {
      const observations: Record<string, number> = {};
      for (const reader of readers) {
        const { read, artifact, calls } = artifactReadTool(scenario !== "foreign", reader.name);
        const target =
          scenario === "directory"
            ? artifact.slice(0, artifact.lastIndexOf("/"))
            : scenario === "pattern"
              ? artifact.replace("stdout", "*")
              : artifact;
        await executeProjected(read, reader.args(target));
        observations[reader.name] = calls.filter(
          (call) => (call as { action: string }).action === "external_directory",
        ).length;
      }
      expect(observations).toEqual(
        Object.fromEntries(readers.map((reader) => [reader.name, scenario === "own" ? 0 : 1])),
      );
    });
  }

  for (const owned of [true, false]) {
    for (const target of [
      "/aft-v2-scratch-fixture/.sandbox-tmp-task/scratch.rs",
      "/aft-v2-scratch-fixture/.sandbox-tmp-task",
    ]) {
      test(`scratch read capability ${owned ? "owned" : "foreign"} ${target}`, async () => {
        for (const reader of [{ name: "read", args: (p: string) => ({ path: p }) }, ...readers]) {
          const { read, calls, ownershipQueries } = artifactReadTool(owned, reader.name, target);
          await executeProjected(read, reader.args(target));
          expect(ownershipQueries).toEqual([{ path: target, session_id: "ses_test" }]);
          expect(
            calls.filter((call) => (call as { action: string }).action === "external_directory"),
          ).toHaveLength(owned ? 0 : 1);
        }
      });
    }
  }

  test("write to own artifact still asserts external_directory", async () => {
    const { read: write, artifact, calls } = artifactReadTool(true, "write");
    await executeProjected(write, { path: artifact, content: "overwrite" });
    expect(calls.map((call) => (call as { action: string }).action)).toContain(
      "external_directory",
    );
  });

  test("read, write, and edit surface a rule denial instead of a JSON envelope", async () => {
    for (const name of ["read", "write", "edit"] as const) {
      const { host } = fixture({
        agentRules: [{ action: "*", resource: "*", effect: "deny" }],
      });
      const tools = projectedFilesystemTools(host, (_tool, preview) =>
        preview ? { success: true, preview_diff: "diff" } : { success: true, text: "ok" },
      );
      const input =
        name === "read"
          ? { path: "file.ts" }
          : name === "write"
            ? { path: "file.ts", content: "new" }
            : { path: "file.ts", edits: [{ oldString: "old", newString: "new" }] };

      const failure = await executeProjected(tools[name], input).then(
        (result) => result,
        (error: unknown) => error as Error,
      );

      expect(failure, `${name} must fail the call`).toBeInstanceOf(Error);
      expect((failure as Error).message).toContain("permission_denied");
      expect((failure as Error).message).not.toContain('"success"');
    }
  });

  test("aft_delete and aft_move include the rule that denied them", async () => {
    for (const name of ["aft_delete", "aft_move"] as const) {
      const { host } = fixture({
        agentRules: [{ action: "edit", resource: "*", effect: "deny" }],
      });
      const tools = projectedFilesystemTools(host, () => ({ success: true, text: "ok" }));
      const input =
        name === "aft_delete"
          ? { files: ["file.ts"] }
          : { path: "file.ts", destination: "moved.ts" };
      const denied = await executeProjected(tools[name], input).catch(
        (error: unknown) => error as Error,
      );

      expect(denied).toBeInstanceOf(Error);
      expect((denied as Error).message).toContain("Permission denied");
      expect((denied as Error).message).toContain('"edit" on "*" is "deny"');
    }
  });

  for (const [label, message] of [
    ["deny", "Denied by host rule"],
    ["feedback", "Use a narrower path"],
  ]) {
    test(`host Tool.Error ${label} preserves deny-with-feedback`, async () => {
      const { host, calls } = fixture({ rejection: new ToolError({ message }) });
      const error = await requestPermission(host, request, context).catch((error) => error);
      expect(classifyPermissionError(error)).toEqual({ kind: "feedback", message });
      expect(calls).toHaveLength(1);
    });
  }

  test("plain host rejection escapes the projected tool unchanged even if shared code catches it", async () => {
    const abort = new Error("stop this step");
    const { host } = fixture({ rejection: abort });
    const projected = projectV2Tool(
      "edit",
      {
        description: "edit",
        args: {},
        execute: async (_input, runtime) => {
          try {
            await (runtime as unknown as V2DefinitionRuntime).ask(request);
          } catch {
            return "caught by shared tool";
          }
          return "incorrectly allowed";
        },
      },
      { directory: "/project" },
      hoistedV2ToolConsumers(host),
    );
    const outcome = await Effect.runPromiseExit(projected.execute({}, context));
    expect(outcome._tag).toBe("Failure");
    if (outcome._tag === "Failure") {
      expect(Cause.findDefect(outcome.cause)).toMatchObject({ _tag: "Success", success: abort });
    }
  });

  test("old V2 context refuses startup with the required version instead of disabling gated tools", () => {
    const { host } = fixture();
    const { permission: _permission, ...oldHost } = host;
    expect(() => hoistedV2ToolConsumers(oldHost)).toThrow("OpenCode 2.0.29");
    expect(() => makeServerEffect()({ ...oldHost, location: { directory: "/project" } })).toThrow(
      "OpenCode 2.0.29",
    );
  });

  test("capability probe accepts assert without a host version string", () => {
    expect(typeof hoistedV2ToolConsumers(fixture().host).requestPermission).toBe("function");
  });

  test("caller cancellation interrupts the host assertion rather than leaving a pending prompt", async () => {
    const { host } = fixture();
    const controller = new AbortController();
    let started = false;
    let interrupted = false;
    host.permission.assert = () =>
      Effect.suspend(() => {
        started = true;
        return Effect.never.pipe(
          Effect.onInterrupt(() =>
            Effect.sync(() => {
              interrupted = true;
            }),
          ),
        );
      });
    const projected = projectV2Tool(
      "edit",
      {
        description: "edit",
        args: {},
        execute: async (_input, runtime) => {
          await (runtime as unknown as V2DefinitionRuntime).ask(request);
          return "allowed";
        },
      },
      { directory: "/project" },
      hoistedV2ToolConsumers(host),
    );
    const pending = Effect.runPromiseExit(projected.execute({}, context), {
      signal: controller.signal,
    });
    for (let turn = 0; turn < 20 && !started; turn++) await Promise.resolve();
    expect(started).toBe(true);
    controller.abort();
    await pending;
    expect(interrupted).toBe(true);
  });
});
