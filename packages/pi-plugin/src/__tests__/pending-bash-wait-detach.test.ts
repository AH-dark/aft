import { expect, spyOn, test } from "bun:test";
import { clearPendingBashWaitDetach, interruptBashWaitsForInput } from "../bash-wait-detach.js";
import { bridgeFor, callBridge, callToolCall } from "../tools/_shared.js";

const projectRoot = process.cwd();

function fixture(open = true, aliases = false) {
  const calls: Array<{ command: string; params: Record<string, unknown> }> = [];
  const pending = new Set<unknown>();
  let opened = open;
  let creations = 0;
  let hold: Promise<void> | undefined;
  let release = () => {};
  let secondCalls = 0;
  const bridge = {
    getCwd: () => projectRoot,
    send: async (command: string, params: Record<string, unknown>) => {
      calls.push({ command, params });
      if (params.pending) pending.add(params.session_id);
      if (params.pending || params.clear_pending) await hold;
      if (params.clear_pending) pending.delete(params.session_id);
      const detached =
        (command === "bash" || (command === "tool_call" && params.name === "bash")) &&
        pending.delete(params.session_id);
      return { success: true, detached, text: "ok" };
    },
    toolCall: async (sessionID: string, name: string, args: Record<string, unknown>) =>
      bridge.send("tool_call", { session_id: sessionID, name, args }),
  };
  let current = aliases ? { ...bridge } : bridge;
  const second = {
    send: () => {
      secondCalls += 1;
      return new Promise<never>(() => {});
    },
  };
  const pool = {
    getActiveBridgeForRoot: (root: string) =>
      root === projectRoot ? (opened ? current : null) : second,
    getBridge: () => {
      if (!opened) creations += 1;
      opened = true;
      return current;
    },
    activeBridges: () => [bridge, second],
  } as unknown as Parameters<typeof interruptBashWaitsForInput>[0];
  const ctx = { pool } as unknown as Parameters<typeof bridgeFor>[0];
  const extCtx = (sessionID: string) =>
    ({ sessionManager: { getSessionId: () => sessionID } }) as unknown as NonNullable<
      Parameters<typeof callBridge>[3]
    >;
  return {
    pool,
    calls,
    creations: () => creations,
    secondCalls: () => secondCalls,
    useUnderlying: () => {
      current = bridge;
    },
    block: () => {
      hold = new Promise<void>((resolve) => {
        release = resolve;
      });
    },
    release: () => release(),
    admit: (sessionID = "pi") =>
      interruptBashWaitsForInput(pool, {}, projectRoot, sessionID, { text: "hello" }),
    step: (root = projectRoot, sessionID = "pi") =>
      clearPendingBashWaitDetach(pool, root, sessionID),
    tool: (sessionID = "pi") =>
      callBridge(
        bridgeFor(ctx, projectRoot),
        "bash",
        { command: "echo ready", block_to_completion: true },
        extCtx(sessionID),
      ),
    otherTool: (name: string, sessionID = "pi") =>
      callBridge(bridgeFor(ctx, projectRoot), name, { path: "README.md" }, extCtx(sessionID)),
    hoistedTool: (name = "read", sessionID = "pi") =>
      callToolCall(
        bridgeFor(ctx, projectRoot),
        name,
        { path: "README.md", wait: true },
        extCtx(sessionID),
      ),
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

test("Pi wrapper and underlying share one queue so a late clear cannot erase newer admission", async () => {
  const f = fixture(true, true);
  f.block();
  f.admit();
  await drain();
  try {
    await promptly(f.step());
    f.useUnderlying();
    f.admit();
  } finally {
    f.release();
  }
  await drain();
  expect((await f.tool()).detached).toBe(true);
  expect(f.calls.map(({ params }) => params)).toEqual([
    { session_id: "pi", pending: true },
    { session_id: "pi", clear_pending: true },
    { session_id: "pi", pending: true },
    { command: "echo ready", block_to_completion: true, session_id: "pi" },
  ]);
  await f.step();
});

test("Pi forwarding recognizes pending admission through wrapper and underlying", async () => {
  const f = fixture(true, true);
  f.admit();
  await drain();
  f.useUnderlying();
  expect((await f.tool()).detached).toBe(true);
  expect(f.calls.map(({ command }) => command)).toEqual(["bash_wait_detach", "bash"]);
  await f.step();
});

test.each([
  "command",
  "hoisted",
])("Pi next bash through underlying waits for wrapper clear and does not detach via %s", async (dispatch) => {
  const f = fixture(true, true);
  f.admit();
  await drain();
  f.block();
  let tool: ReturnType<typeof f.tool> | undefined;
  try {
    await f.step();
    f.useUnderlying();
    tool = dispatch === "hoisted" ? f.hoistedTool("bash") : f.tool();
    await drain();
    expect(f.calls.some(({ command }) => command === "bash" || command === "tool_call")).toBe(
      false,
    );
  } finally {
    f.release();
  }
  expect((await tool)?.detached).toBe(false);
});

for (const dispatch of ["command", "hoisted"] as const) {
  test.each([
    "read",
    "search",
    "bash",
  ])(`Pi session B %s ${dispatch} does not wait for session A clear on the same bridge`, async (name) => {
    const f = fixture();
    f.admit();
    await drain();
    f.block();
    try {
      await f.step();
      const result = await promptly(
        dispatch === "hoisted"
          ? f.hoistedTool(name, "other-pi")
          : name === "bash"
            ? f.tool("other-pi")
            : f.otherTool(name, "other-pi"),
      );
      expect(result.success).toBe(true);
    } finally {
      f.release();
    }
    await drain();
  });

  test.each([
    "read",
    "search",
  ])(`Pi session A %s ${dispatch} does not wait for its own clear without pending input`, async (name) => {
    const f = fixture();
    f.admit();
    await drain();
    f.block();
    try {
      await f.step();
      const result = await promptly(
        dispatch === "hoisted" ? f.hoistedTool(name) : f.otherTool(name),
      );
      expect(result.success).toBe(true);
    } finally {
      f.release();
    }
    await drain();
  });
}

test("Pi session B admission bypasses session A clear but A next bash still waits", async () => {
  const f = fixture();
  f.admit();
  await drain();
  f.block();
  let tool: ReturnType<typeof f.tool> | undefined;
  try {
    await f.step();
    f.admit("other-pi");
    await drain();
    expect(f.calls.some(({ params }) => params.session_id === "other-pi")).toBe(true);
    tool = f.tool();
    await drain();
    expect(f.calls.some(({ command }) => command === "bash")).toBe(false);
  } finally {
    f.release();
  }
  expect((await tool)?.detached).toBe(false);
  await f.step(projectRoot, "other-pi");
});

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

test.each([
  "admission",
  "clear",
])("Pi next bash waits for its slow delivery clear with %s in flight and does not detach", async (stage) => {
  const f = fixture();
  if (stage === "admission") f.block();
  f.admit();
  await drain();
  if (stage === "clear") f.block();
  let tool: ReturnType<typeof f.tool> | undefined;
  try {
    await promptly(f.step());
    tool = f.tool();
    await drain();
    expect(f.calls.map(({ command }) => command)).toEqual(
      stage === "admission" ? ["bash_wait_detach"] : ["bash_wait_detach", "bash_wait_detach"],
    );
  } finally {
    f.release();
  }
  expect((await tool)?.detached).toBe(false);
  expect(f.calls.map(({ params }) => params)).toEqual([
    { session_id: "pi", pending: true },
    { session_id: "pi", clear_pending: true },
    { command: "echo ready", block_to_completion: true, session_id: "pi" },
  ]);
});

test("Pi next bash waits only for its bridge while another project's admission hangs", async () => {
  const f = fixture();
  interruptBashWaitsForInput(f.pool, {}, "/another-project", "other-pi", { text: "hello" });
  expect(f.secondCalls()).toBe(1);
  await promptly(clearPendingBashWaitDetach(f.pool, "/another-project", "other-pi"));
  f.admit();
  await drain();
  await promptly(f.step());
  const result = await promptly(f.tool());
  expect(result.detached).toBe(false);
  expect(f.calls.map(({ params }) => params)).toEqual([
    { session_id: "pi", pending: true },
    { session_id: "pi", clear_pending: true },
    { command: "echo ready", block_to_completion: true, session_id: "pi" },
  ]);
});

test("Pi next bash bounds its wait for a hung delivery clear", async () => {
  const f = fixture();
  f.admit();
  await drain();
  f.block();
  await promptly(f.step());
  const setTimer = globalThis.setTimeout;
  const delays: Array<number | undefined> = [];
  const timer = spyOn(globalThis, "setTimeout").mockImplementation((callback, delay, ...args) => {
    delays.push(delay);
    // Exercise the real deadline without making the suite wait 30 seconds.
    return setTimer(callback, delay === 30_000 ? 0 : delay, ...args);
  });
  try {
    await promptly(
      expect(f.tool()).rejects.toThrow("Timed out waiting for bash wait detach delivery clear"),
    );
    expect(delays).toContain(30_000);
    expect(f.calls.some(({ command }) => command === "bash")).toBe(false);
  } finally {
    timer.mockRestore();
    f.release();
  }
  await drain();
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
