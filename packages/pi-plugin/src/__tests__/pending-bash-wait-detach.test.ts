import { expect, test } from "bun:test";
import { clearPendingBashWaitDetach, interruptBashWaitsForInput } from "../bash-wait-detach.js";

test("Pi input remembers admission until turn delivery and orders the clear after admission", async () => {
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
  let opened = false;
  const pool = {
    getActiveBridgeForRoot: () => (opened ? bridge : null),
    getBridge: () => {
      opened = true;
      return bridge;
    },
  } as unknown as Parameters<typeof interruptBashWaitsForInput>[0];
  interruptBashWaitsForInput(pool, {}, "/repo", "pi", { text: "hello" });
  expect(calls).toEqual([{ session_id: "pi", pending: true }]);
  const delivered = clearPendingBashWaitDetach(pool, "/repo", "pi");
  expect(calls).toHaveLength(1);
  release();
  await delivered;
  expect(calls).toEqual([
    { session_id: "pi", pending: true },
    { session_id: "pi", clear_pending: true },
  ]);
});
