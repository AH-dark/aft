import { expect, spyOn, test } from "bun:test";
import { clearPendingBashWaitDetach, interruptBashWaitsForInput } from "../bash-wait-detach.js";
import { bridgeFor, callBridge, callToolCall } from "../tools/_shared.js";

const projectRoot = process.cwd();

function fixture(open = true) {
  const calls: Array<{ command: string; params: Record<string, unknown> }> = [];
  let opened = open;
  let creations = 0;
  let hold: Promise<void> | undefined;
  let release = () => {};
  let secondCalls = 0;
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      calls.push({ command, params });
      if (params.pending || params.clear_pending) await hold;
      return { success: true, detached: false, text: "ok" };
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
    getActiveBridgeForRoot: (root: string) =>
      root === projectRoot ? (opened ? bridge : null) : second,
    getBridge: () => {
      if (!opened) creations += 1;
      opened = true;
      return bridge;
    },
    activeBridges: () => [bridge, second],
  } as unknown as Parameters<typeof interruptBashWaitsForInput>[0];
  const ctx = { pool } as unknown as Parameters<typeof bridgeFor>[0];
  const extCtx = { sessionManager: { getSessionId: () => "pi" } } as unknown as NonNullable<
    Parameters<typeof callBridge>[3]
  >;
  return {
    pool,
    calls,
    creations: () => creations,
    secondCalls: () => secondCalls,
    block: () => {
      hold = new Promise<void>((resolve) => {
        release = resolve;
      });
    },
    release: () => release(),
    admit: () => interruptBashWaitsForInput(pool, {}, projectRoot, "pi", { text: "hello" }),
    step: (root = projectRoot) => clearPendingBashWaitDetach(pool, root, "pi"),
    tool: () =>
      callBridge(
        bridgeFor(ctx, projectRoot),
        "bash",
        { command: "echo ready", block_to_completion: true },
        extCtx,
      ),
    hoistedTool: () =>
      callToolCall(bridgeFor(ctx, projectRoot), "read", { path: "README.md" }, extCtx),
  };
}

async function promptly(operation: Promise<unknown>) {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([
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

test("Pi cold admission creates no bridge and forwards pending before the first bash", async () => {
  const f = fixture(false);
  f.admit();
  expect(f.creations()).toBe(0);
  expect(f.calls).toEqual([]);
  f.block();
  const tool = f.tool();
  try {
    await drain();
    expect(f.calls.map(({ command }) => command)).toEqual(["bash_wait_detach"]);
  } finally {
    f.release();
  }
  await tool;
  expect(f.creations()).toBe(1);
  expect(f.calls.map(({ command }) => command)).toEqual(["bash_wait_detach", "bash"]);
  expect(f.calls[0].params).toEqual({ session_id: "pi", pending: true });
});

test("Pi cold pending is also forwarded before a hoisted tool call", async () => {
  const f = fixture(false);
  f.admit();
  await f.hoistedTool();
  expect(f.calls.map(({ command }) => command)).toEqual(["bash_wait_detach", "tool_call"]);
});

test("Pi model step with no pending request makes zero bridge calls", async () => {
  const f = fixture();
  await f.step();
  expect(f.calls).toEqual([]);
  expect(f.secondCalls()).toBe(0);
});

test("Pi model step clears its recorded recipient, never a hung second project", async () => {
  const f = fixture();
  f.admit();
  await drain();
  await promptly(f.step("/another-project"));
  await drain();
  expect(f.calls.map(({ params }) => params)).toEqual([
    { session_id: "pi", pending: true },
    { session_id: "pi", clear_pending: true },
  ]);
  expect(f.secondCalls()).toBe(0);
});

test("Pi model step never waits for admission and preserves later admission order", async () => {
  const f = fixture();
  f.block();
  f.admit();
  try {
    await promptly(f.step());
    f.admit();
    expect(f.calls).toHaveLength(1);
  } finally {
    f.release();
  }
  await drain();
  expect(f.calls.map(({ params }) => params)).toEqual([
    { session_id: "pi", pending: true },
    { session_id: "pi", clear_pending: true },
    { session_id: "pi", pending: true },
  ]);
});

test("Pi model step never waits for a hung clear", async () => {
  const f = fixture();
  f.admit();
  await drain();
  f.block();
  try {
    await promptly(f.step());
    f.admit();
    expect(f.calls.map(({ params }) => params)).toEqual([
      { session_id: "pi", pending: true },
      { session_id: "pi", clear_pending: true },
    ]);
  } finally {
    f.release();
  }
  await drain();
  expect(f.calls[2].params).toEqual({ session_id: "pi", pending: true });
});

test("Pi delivery clears cold pending without opening a bridge", async () => {
  const f = fixture(false);
  f.admit();
  await f.step();
  await f.tool();
  expect(f.creations()).toBe(1);
  expect(f.calls.map(({ command }) => command)).toEqual(["bash"]);
});

test("Pi cold pending expires after 120 seconds", async () => {
  const f = fixture(false);
  const now = Date.now();
  const clock = spyOn(Date, "now").mockReturnValue(now);
  try {
    f.admit();
    clock.mockReturnValue(now + 120_000);
    await f.tool();
    expect(f.calls.map(({ command }) => command)).toEqual(["bash"]);
    await f.step();
  } finally {
    clock.mockRestore();
  }
});

test("Pi pending is session-scoped and not re-armed for every tool", async () => {
  const f = fixture(false);
  f.admit();
  await clearPendingBashWaitDetach(f.pool, projectRoot, "another-session");
  await f.tool();
  await f.tool();
  expect(f.calls.map(({ command }) => command)).toEqual(["bash_wait_detach", "bash", "bash"]);
});
