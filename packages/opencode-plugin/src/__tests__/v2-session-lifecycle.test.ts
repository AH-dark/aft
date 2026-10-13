import { expect, test } from "bun:test";
import { Effect, Exit, PubSub, Scope, Stream } from "effect";

import { _resetSubagentCacheForTest } from "../shared/subagent-detect.js";
import { createBashWatchTool } from "../tools/bash_watch.js";
import type { PluginContext } from "../types.js";
import {
  allowV2WatchWake,
  awaitV2ExecutionSettlement,
  noteV2ToolAbort,
  registerV2SessionLifecycle,
  trackV2Watch,
  v2SubagentStopped,
  v2WatchWakeAllowed,
} from "../v2-session-lifecycle.js";

async function rig(parentID: string | undefined = "parent") {
  _resetSubagentCacheForTest();
  const bus = await Effect.runPromise(
    PubSub.unbounded<{ type: string; data: { sessionID: string; reason?: string } }>(),
  );
  const scope = await Effect.runPromise(Scope.make());
  const calls: Array<{ name: string; args: Record<string, unknown> }> = [];
  const host = {
    location: { directory: process.cwd() },
    event: { subscribe: () => Stream.fromPubSub(bus) },
    session: { get: () => Effect.succeed({ parentID }) },
  };
  const bridge = {
    send: async (name: string, args: Record<string, unknown>) => {
      calls.push({ name, args });
      return { success: true };
    },
  };
  const ctx = {
    client: host,
    pool: { getBridge: () => bridge, getActiveBridgeForRoot: () => bridge },
    config: {},
  } as unknown as PluginContext;
  await Effect.runPromise(
    registerV2SessionLifecycle(host).pipe(Effect.provideService(Scope.Scope, scope)),
  );
  return {
    host,
    ctx,
    calls,
    async event(type: string, reason?: string) {
      await Effect.runPromise(PubSub.publish(bus, { type, data: { sessionID: "child", reason } }));
      await Bun.sleep(10);
    },
    async close() {
      await Effect.runPromise(Scope.close(scope, Exit.succeed(undefined)));
    },
  };
}

for (const reason of ["user", "inactivity"] as const) {
  test(`V2 deliberate ${reason} stop cancels registered and late subagent watches`, async () => {
    const r = await rig();
    try {
      const runtime = { sessionID: "child", directory: process.cwd() };
      await trackV2Watch(r.ctx, runtime, "bash-running", "watch-1");
      noteV2ToolAbort(r.host, "child");
      let settled = false;
      const pending = awaitV2ExecutionSettlement(r.host, "child").then(() => {
        settled = true;
      });
      await Bun.sleep(10);
      expect(settled).toBe(false);
      await r.event("session.execution.interrupted", reason);
      await pending;
      expect(v2SubagentStopped(r.host, "child")).toBe(true);
      expect(v2WatchWakeAllowed(r.host, "child", "bash-running")).toBe(false);
      expect(r.calls.map((call) => call.name)).toEqual(["bash_unnotify"]);
      await trackV2Watch(r.ctx, runtime, "bash-late", "watch-2");
      expect(r.calls.map((call) => call.name)).toEqual(["bash_unnotify", "bash_unnotify"]);
      await r.event("session.execution.started");
      expect(v2SubagentStopped(r.host, "child")).toBe(false);
      expect(v2WatchWakeAllowed(r.host, "child", "bash-running")).toBe(false);
      allowV2WatchWake(r.host, "child", "bash-resumed");
      expect(v2WatchWakeAllowed(r.host, "child", "bash-resumed")).toBe(true);
    } finally {
      await r.close();
    }
  });
}

for (const [type, reason] of [
  ["session.execution.interrupted", "shutdown"],
  ["session.execution.interrupted", "superseded"],
  ["session.execution.failed", undefined],
] as const) {
  test(`V2 ${type}/${reason ?? "provider error"} retains watch wakes`, async () => {
    const r = await rig();
    try {
      await trackV2Watch(
        r.ctx,
        { sessionID: "child", directory: process.cwd() },
        "bash-running",
        "watch-1",
      );
      noteV2ToolAbort(r.host, "child");
      await r.event(type, reason);
      await awaitV2ExecutionSettlement(r.host, "child");
      expect(v2SubagentStopped(r.host, "child")).toBe(false);
      expect(v2WatchWakeAllowed(r.host, "child", "bash-running")).toBe(true);
      expect(r.calls).toEqual([]);
    } finally {
      await r.close();
    }
  });
}

test("V2 main-session stop retains registered watch", async () => {
  const r = await rig("");
  try {
    await trackV2Watch(
      r.ctx,
      { sessionID: "child", directory: process.cwd() },
      "bash-running",
      "watch-1",
    );
    await r.event("session.execution.interrupted", "user");
    expect(v2SubagentStopped(r.host, "child")).toBe(false);
    expect(r.calls).toEqual([]);
  } finally {
    await r.close();
  }
});

test("cancelled worker watch does not acknowledge a racing inline completion", async () => {
  const r = await rig();
  try {
    const abort = new AbortController();
    const commands: string[] = [];
    const metadata: Array<Record<string, unknown>> = [];
    const bridge = r.ctx.pool.getBridge(process.cwd());
    bridge.send = async (command) => {
      commands.push(command);
      abort.abort();
      return {
        success: true,
        status: "completed",
        output: "racing-completion",
        task_id: "bash-race",
      };
    };
    await createBashWatchTool(r.ctx).execute({ taskId: "bash-race" }, {
      sessionID: "child",
      directory: process.cwd(),
      abort: abort.signal,
      metadata: (value: Record<string, unknown>) => metadata.push(value),
    } as never);
    expect(commands).toEqual(["bash_status"]);
    expect(metadata.at(-1)?.waited).toMatchObject({ reason: "aborted" });
  } finally {
    await r.close();
  }
});
