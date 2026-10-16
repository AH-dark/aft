import { expect, test } from "bun:test";
import { Effect } from "effect";
import {
  clearPendingBashWaitDetachForMessages,
  interruptBashWaitsForChatMessage,
} from "../bash-wait-detach.js";
import { registerV2PromptDetachHook } from "../v2-prompt-detach.js";

function fixture() {
  const calls: Record<string, unknown>[] = [];
  let release!: () => void;
  const admitted = new Promise<void>((resolve) => {
    release = resolve;
  });
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      expect(command).toBe("bash_wait_detach");
      calls.push(params);
      if (params.pending) await admitted;
      return { success: true, detached: false };
    },
  };
  const pool = {
    getActiveBridgeForRoot: () => bridge,
    activeBridges: () => [bridge],
  } as unknown as Parameters<typeof interruptBashWaitsForChatMessage>[0];
  return { pool, calls, release };
}

test("V1 remembers admission and clears on model input after the admission RPC settles", async () => {
  const { pool, calls, release } = fixture();
  interruptBashWaitsForChatMessage(pool, {}, "/repo", "v1", {
    parts: [{ type: "text", text: "hello" }],
  });
  expect(calls).toEqual([{ session_id: "v1", pending: true }]);
  const delivered = clearPendingBashWaitDetachForMessages(pool, {
    messages: [{ info: { sessionID: "v1" } }],
  });
  expect(calls).toHaveLength(1);
  release();
  await delivered;
  expect(calls).toEqual([
    { session_id: "v1", pending: true },
    { session_id: "v1", clear_pending: true },
  ]);
});

test("V2 prompt admission persists until its context delivery hook", async () => {
  const { pool, calls, release } = fixture();
  const hooks = new Map<string, (event: never) => Effect.Effect<void>>();
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
      { pool, projectRoot: "/repo", getConfig: () => ({}) },
    ) as Effect.Effect<boolean>,
  );
  await Effect.runPromise(
    hooks.get("prompt")?.({
      sessionID: "v2",
      prompt: { text: "hello" },
    } as never) as Effect.Effect<void>,
  );
  expect(calls).toEqual([{ session_id: "v2", pending: true }]);
  const delivered = Effect.runPromise(
    hooks.get("context")?.({ sessionID: "v2" } as never) as Effect.Effect<void>,
  );
  expect(calls).toHaveLength(1);
  release();
  await delivered;
  expect(calls).toEqual([
    { session_id: "v2", pending: true },
    { session_id: "v2", clear_pending: true },
  ]);
});

test("admission opens a bridge before the session's first tool call", async () => {
  const { pool, calls, release } = fixture();
  const bridge = pool.getActiveBridgeForRoot("/repo");
  let opened = false;
  const lazy = {
    getActiveBridgeForRoot: () => (opened ? bridge : null),
    getBridge: () => {
      opened = true;
      return bridge;
    },
    activeBridges: () => (opened ? [bridge] : []),
  } as unknown as typeof pool;
  interruptBashWaitsForChatMessage(lazy, {}, "/repo", "new-session", {
    parts: [{ type: "text", text: "hello" }],
  });
  expect(calls).toEqual([{ session_id: "new-session", pending: true }]);
  release();
  await clearPendingBashWaitDetachForMessages(lazy, {
    messages: [{ info: { sessionID: "new-session" } }],
  });
});
