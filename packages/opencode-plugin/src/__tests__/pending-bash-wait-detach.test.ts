import { expect, spyOn, test } from "bun:test";
import { Effect } from "effect";
import {
  clearPendingBashWaitDetachForMessages,
  interruptBashWaitsForChatMessage,
} from "../bash-wait-detach.js";
import { callBridge, callToolCall } from "../tools/_shared.js";
import { registerV2PromptDetachHook } from "../v2-prompt-detach.js";

const projectRoot = process.cwd();
let nextSession = 0;

function fixture(open = true) {
  const calls: Array<{ command: string; params: Record<string, unknown> }> = [];
  const pending = new Set<unknown>();
  let opened = open;
  let creations = 0;
  let hold: Promise<void> | undefined;
  let release = () => {};
  let secondOpen = false;
  let secondCalls = 0;
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      calls.push({ command, params });
      if (params.pending) pending.add(params.session_id);
      if (params.pending || params.clear_pending) await hold;
      if (params.clear_pending) pending.delete(params.session_id);
      const detached = command === "bash" && pending.delete(params.session_id);
      return { success: true, detached, text: "ok" };
    },
    toolCall: async (sessionID: string, name: string, args: Record<string, unknown>) =>
      bridge.send("tool_call", { session_id: sessionID, name, args }),
  };
  const second = {
    send: () => {
      secondCalls += 1;
      return new Promise<never>(() => {});
    },
  };
  const pool = {
    getActiveBridgeForRoot: () => (opened ? bridge : null),
    getBridge: () => {
      if (!opened) creations += 1;
      opened = true;
      return bridge;
    },
    activeBridges: () => [...(opened ? [bridge] : []), ...(secondOpen ? [second] : [])],
  } as unknown as Parameters<typeof interruptBashWaitsForChatMessage>[0];
  return {
    pool,
    calls,
    creations: () => creations,
    secondCalls: () => secondCalls,
    openSecond: () => {
      secondOpen = true;
    },
    block: () => {
      hold = new Promise<void>((resolve) => {
        release = resolve;
      });
    },
    release: () => release(),
  };
}

async function host(version: "V1" | "V2", pool: ReturnType<typeof fixture>["pool"]) {
  const sessionID = `pending-${version}-${++nextSession}`;
  const hooks = new Map<string, (event: never) => Effect.Effect<void>>();
  if (version === "V2") {
    await Effect.runPromise(
      registerV2PromptDetachHook(
        {
          session: {
            hook: (name: string, callback: (event: never) => Effect.Effect<void>) => {
              hooks.set(name, callback);
              return Effect.void;
            },
          },
        },
        { pool, projectRoot, getConfig: () => ({}) },
      ) as Effect.Effect<boolean>,
    );
  }
  const client = { session: { get: async () => ({ data: { directory: projectRoot } }) } };
  const ctx = { pool, client } as unknown as Parameters<typeof callBridge>[0];
  const runtime = { sessionID, directory: projectRoot, directoryIsSessionRoot: true };
  return {
    sessionID,
    admit: async () => {
      if (version === "V1")
        interruptBashWaitsForChatMessage(pool, {}, projectRoot, sessionID, {
          parts: [{ type: "text", text: "hello" }],
        });
      else
        await Effect.runPromise(
          hooks.get("prompt")?.({
            sessionID,
            prompt: { text: "hello" },
          } as never) as Effect.Effect<void>,
        );
    },
    step: () =>
      version === "V1"
        ? clearPendingBashWaitDetachForMessages(pool, { messages: [{ info: { sessionID } }] })
        : Effect.runPromise(hooks.get("context")?.({ sessionID } as never) as Effect.Effect<void>),
    tool: () =>
      callBridge(ctx, runtime, "bash", { command: "echo ready", block_to_completion: true }),
    hoistedTool: () => callToolCall(ctx, runtime, "read", { path: "README.md" }),
  };
}

async function promptly<T>(operation: Promise<T>): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      operation,
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error("model step waited for a bridge")), 200);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function drain() {
  for (let i = 0; i < 20; i += 1) await Promise.resolve();
}

for (const version of ["V1", "V2"] as const) {
  test(`${version} cold admission creates no bridge and forwards pending before the first bash`, async () => {
    const f = fixture(false);
    const h = await host(version, f.pool);
    await h.admit();
    expect(f.creations()).toBe(0);
    expect(f.calls).toEqual([]);
    f.block();
    const tool = h.tool();
    try {
      await drain();
      expect(f.calls.map(({ command }) => command)).toEqual(["bash_wait_detach"]);
    } finally {
      f.release();
    }
    await tool;
    expect(f.creations()).toBe(1);
    expect(f.calls.map(({ command }) => command)).toEqual(["bash_wait_detach", "bash"]);
    expect(f.calls[0].params).toEqual({ session_id: h.sessionID, pending: true });
  });

  test(`${version} cold pending is also forwarded before a hoisted tool call`, async () => {
    const f = fixture(false);
    const h = await host(version, f.pool);
    await h.admit();
    await h.hoistedTool();
    expect(f.calls.map(({ command }) => command)).toEqual(["bash_wait_detach", "tool_call"]);
  });

  test(`${version} model step with no pending request makes zero bridge calls`, async () => {
    const f = fixture();
    const h = await host(version, f.pool);
    await h.step();
    expect(f.calls).toEqual([]);
    expect(f.secondCalls()).toBe(0);
  });

  test(`${version} model step clears only its pending recipient, never a hung second project`, async () => {
    const f = fixture();
    const h = await host(version, f.pool);
    await h.admit();
    await drain();
    f.openSecond();
    await promptly(h.step());
    await drain();
    expect(f.calls.map(({ params }) => params)).toEqual([
      { session_id: h.sessionID, pending: true },
      { session_id: h.sessionID, clear_pending: true },
    ]);
    expect(f.secondCalls()).toBe(0);
  });

  test(`${version} model step never waits for admission and preserves later admission order`, async () => {
    const f = fixture();
    const h = await host(version, f.pool);
    f.block();
    await h.admit();
    try {
      await promptly(h.step());
      await h.admit();
      expect(f.calls).toHaveLength(1);
    } finally {
      f.release();
    }
    await drain();
    expect(f.calls.map(({ params }) => params)).toEqual([
      { session_id: h.sessionID, pending: true },
      { session_id: h.sessionID, clear_pending: true },
      { session_id: h.sessionID, pending: true },
    ]);
  });

  test(`${version} model step never waits for a hung clear`, async () => {
    const f = fixture();
    const h = await host(version, f.pool);
    await h.admit();
    await drain();
    f.block();
    try {
      await promptly(h.step());
      await h.admit();
      expect(f.calls.map(({ params }) => params)).toEqual([
        { session_id: h.sessionID, pending: true },
        { session_id: h.sessionID, clear_pending: true },
      ]);
    } finally {
      f.release();
    }
    await drain();
    expect(f.calls[2].params).toEqual({ session_id: h.sessionID, pending: true });
  });

  test.each([
    "admission",
    "clear",
  ])(`${version} next bash waits for its slow delivery clear with %s in flight and does not detach`, async (stage) => {
    const f = fixture();
    const h = await host(version, f.pool);
    if (stage === "admission") f.block();
    await h.admit();
    await drain();
    if (stage === "clear") f.block();
    let tool: ReturnType<typeof h.tool> | undefined;
    try {
      await promptly(h.step());
      tool = h.tool();
      await drain();
      expect(f.calls.map(({ command }) => command)).toEqual(
        stage === "admission" ? ["bash_wait_detach"] : ["bash_wait_detach", "bash_wait_detach"],
      );
    } finally {
      f.release();
    }
    expect((await tool)?.detached).toBe(false);
    expect(f.calls.map(({ command }) => command)).toEqual([
      "bash_wait_detach",
      "bash_wait_detach",
      "bash",
    ]);
  });

  test(`${version} next bash waits only for its bridge while another project's admission hangs`, async () => {
    const f = fixture();
    const h = await host(version, f.pool);
    f.openSecond();
    await h.admit();
    await drain();
    expect(f.secondCalls()).toBe(1);
    await promptly(h.step());
    const result = await promptly(h.tool());
    expect(result.detached).toBe(false);
    expect(f.calls.map(({ params }) => params)).toEqual([
      { session_id: h.sessionID, pending: true },
      { session_id: h.sessionID, clear_pending: true },
      { command: "echo ready", block_to_completion: true, session_id: h.sessionID },
    ]);
  });

  test(`${version} next bash bounds its wait for a hung delivery clear`, async () => {
    const f = fixture();
    const h = await host(version, f.pool);
    await h.admit();
    await drain();
    f.block();
    await promptly(h.step());
    const setTimer = globalThis.setTimeout;
    const delays: Array<number | undefined> = [];
    const timer = spyOn(globalThis, "setTimeout").mockImplementation((callback, delay, ...args) => {
      delays.push(delay);
      // Exercise the real deadline without making the suite wait 30 seconds.
      return setTimer(callback, delay === 30_000 ? 0 : delay, ...args);
    });
    try {
      await promptly(
        expect(h.tool()).rejects.toThrow("Timed out waiting for bash wait detach delivery clear"),
      );
      expect(delays).toContain(30_000);
      expect(f.calls.some(({ command }) => command === "bash")).toBe(false);
    } finally {
      timer.mockRestore();
      f.release();
    }
    await drain();
  });

  test(`${version} delivery clears cold pending without opening a bridge`, async () => {
    const f = fixture(false);
    const h = await host(version, f.pool);
    await h.admit();
    await h.step();
    await h.tool();
    expect(f.creations()).toBe(1);
    expect(f.calls.map(({ command }) => command)).toEqual(["bash"]);
  });

  test(`${version} cold pending expires after 120 seconds`, async () => {
    const f = fixture(false);
    const h = await host(version, f.pool);
    const now = Date.now();
    const clock = spyOn(Date, "now").mockReturnValue(now);
    try {
      await h.admit();
      clock.mockReturnValue(now + 120_000);
      await h.tool();
      expect(f.calls.map(({ command }) => command)).toEqual(["bash"]);
      await h.step();
    } finally {
      clock.mockRestore();
    }
  });

  test(`${version} pending is session-scoped and not re-armed for every tool`, async () => {
    const f = fixture(false);
    const admitted = await host(version, f.pool);
    const other = await host(version, f.pool);
    await admitted.admit();
    await other.step();
    await other.tool();
    await admitted.tool();
    await admitted.tool();
    expect(f.calls.map(({ command }) => command)).toEqual([
      "bash",
      "bash_wait_detach",
      "bash",
      "bash",
    ]);
    expect(f.calls[1].params).toEqual({ session_id: admitted.sessionID, pending: true });
  });
}
