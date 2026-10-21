import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { BinaryBridge, type StatusSnapshot } from "../bridge.js";

function makeBridge(): BinaryBridge {
  return new BinaryBridge("/bin/false", process.cwd());
}

function pushStatus(bridge: BinaryBridge, snapshot: StatusSnapshot): void {
  (
    bridge as unknown as {
      onStdoutData(data: string): void;
    }
  ).onStdoutData(`${JSON.stringify({ type: "status_changed", session_id: null, snapshot })}\n`);
}

describe("BinaryBridge status cache", () => {
  test("root-local and compacted Rust status pushes stay fresh in cache and listeners", () => {
    for (const name of ["projected", "compacted"]) {
      const frame = JSON.parse(
        readFileSync(
          new URL(
            `../../../../crates/aft/tests/fixtures/status_push_consumer_${name}.json`,
            import.meta.url,
          ),
          "utf8",
        ),
      ) as { snapshot: StatusSnapshot };
      const bridge = makeBridge();
      const seen: StatusSnapshot[] = [];
      bridge.subscribeStatus((snapshot) => seen.push(snapshot));
      pushStatus(bridge, frame.snapshot);
      expect(bridge.getCachedStatus()).toEqual(frame.snapshot);
      expect(seen).toEqual([frame.snapshot]);
      expect(bridge.getCachedStatus()?.semantic_index?.entries_done).toBe(40_000);
      expect(bridge.getCachedStatus()?.runtime?.open_routes).toBe(90);
      expect(frame.snapshot).toMatchObject({ push_details: { read: "status" } });
      expect(frame.snapshot.memory).toBeUndefined();
      expect(Buffer.byteLength(JSON.stringify(frame))).toBeLessThanOrEqual(16 * 1024);
    }
  });

  test("bridge_caches_status_snapshot_on_push_frame", () => {
    const bridge = makeBridge();
    const snapshot: StatusSnapshot = { version: "0.24.0", cache_role: "main" };

    pushStatus(bridge, snapshot);

    expect(bridge.getCachedStatus()).toEqual(snapshot);
  });

  test("bridge_subscribe_returns_unsubscribe_function", () => {
    const bridge = makeBridge();
    let calls = 0;
    const unsubscribe = bridge.subscribeStatus(() => {
      calls++;
    });

    unsubscribe();
    pushStatus(bridge, { version: "0.24.0" });

    expect(typeof unsubscribe).toBe("function");
    expect(calls).toBe(0);
  });

  test("bridge_subscribe_fires_listener_immediately_when_cache_populated", () => {
    const bridge = makeBridge();
    const snapshot: StatusSnapshot = { version: "0.24.0", cache_role: "worktree" };
    bridge.cacheStatusSnapshot(snapshot);
    let observed: StatusSnapshot | null = null;

    bridge.subscribeStatus((next) => {
      observed = next;
    });

    expect(observed).toEqual(snapshot);
  });

  test("bridge_listener_errors_do_not_crash_other_listeners", () => {
    const bridge = makeBridge();
    let goodListenerCalls = 0;
    bridge.subscribeStatus(() => {
      throw new Error("boom");
    });
    bridge.subscribeStatus(() => {
      goodListenerCalls++;
    });

    pushStatus(bridge, { version: "0.24.0" });

    expect(goodListenerCalls).toBe(1);
  });
});
