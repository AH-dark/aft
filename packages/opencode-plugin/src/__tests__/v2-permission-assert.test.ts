import { describe, expect, test } from "bun:test";
import { Error as ToolError } from "@opencode/plugin/promise/tool";
import { Cause, Effect } from "effect";
import { makeServerEffect } from "../entry/server-runtime.mjs";
import { requestPermission, type V2PermissionHostContext } from "../permissions/v2.js";
import { projectV2Tool, type V2DefinitionRuntime } from "../tools/definitions/v2.js";
import { hoistedV2ToolConsumers } from "../tools/hoisted/v2.js";
import { classifyPermissionError } from "../tools/permissions.js";

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

function fixture(effect: "allow" | "deny" | "ask" = "ask", rejection?: Error) {
  const calls: unknown[] = [];
  const host: V2PermissionHostContext = {
    agent: {
      get: () =>
        Effect.succeed({ data: { permissions: [{ action: "*", resource: "*", effect }] } }),
    },
    session: { get: () => Effect.succeed(undefined) },
    permission: {
      assert: (input) =>
        Effect.suspend(() => {
          calls.push(input);
          return rejection instanceof ToolError
            ? Effect.fail(rejection)
            : rejection
              ? Effect.die(rejection)
              : Effect.void;
        }),
    },
  };
  return { host, calls };
}

describe("OpenCode 2 in-process permission assertion", () => {
  test("configured allow skips assert", async () => {
    const { host, calls } = fixture("allow", new Error("must not ask"));
    await requestPermission(host, request, context);
    expect(calls).toEqual([]);
  });

  test("configured deny skips assert and reports the rule", async () => {
    const { host, calls } = fixture("deny");
    await expect(requestPermission(host, request, context)).rejects.toThrow(
      "permission rules deny",
    );
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

  for (const [label, message] of [
    ["deny", "Denied by host rule"],
    ["feedback", "Use a narrower path"],
  ]) {
    test(`host Tool.Error ${label} preserves deny-with-feedback`, async () => {
      const { host, calls } = fixture("ask", new ToolError({ message }));
      const error = await requestPermission(host, request, context).catch((error) => error);
      expect(classifyPermissionError(error)).toEqual({ kind: "feedback", message });
      expect(calls).toHaveLength(1);
    });
  }

  test("plain host rejection escapes the projected tool unchanged even if shared code catches it", async () => {
    const abort = new Error("stop this step");
    const { host } = fixture("ask", abort);
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
