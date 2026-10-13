import { expect, spyOn, test } from "bun:test";
import * as fs from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { BindIdentity, RouteHandle } from "@cortexkit/subc-client";
import { RevivableTransportPool } from "../revivable-transport.js";
import { type SubcClientLike, SubcTransportPool } from "../subc-transport.js";

function fixtureClient() {
  const opens: BindIdentity[] = [];
  const client = {
    routeOpen: async (_target: unknown, identity: BindIdentity) => {
      opens.push(identity);
      return { channel: opens.length, epoch: 1 } as RouteHandle;
    },
    request: async () => ({ structuredContent: { success: true, text: "ok" } }),
    subscribe: () => ({ unsubscribe() {} }),
    closeRouteChannel: async () => {},
    close() {},
  } satisfies SubcClientLike;
  return { client, opens };
}

function makePool(client: SubcClientLike) {
  return new SubcTransportPool({
    connectionFile: "/unused/connection.json",
    harness: "opencode",
    connect: async () => client,
  });
}

test("A14 dispatch canonicalizes once and retains fresh root stats", async () => {
  const root = fs.mkdtempSync(join(tmpdir(), "aft-a14-root-"));
  const { client, opens } = fixtureClient();
  const pool = new RevivableTransportPool(makePool(client), async () => makePool(client));
  const realpaths = spyOn(fs, "realpathSync");
  const stats = spyOn(fs, "statSync");
  try {
    for (let i = 0; i < 10; i++) {
      expect((await pool.toolCall(root, { sessionID: "resumed" }, "read")).text).toBe("ok");
    }
    expect(opens[0].project_root).toBe(root);
    expect(stats.mock.calls.length).toBeGreaterThanOrEqual(10);
    expect(realpaths.mock.calls.length).toBe(10);
    fs.rmSync(root, { recursive: true });
    await expect(pool.toolCall(root, { sessionID: "resumed" }, "read")).rejects.toThrow();
    fs.mkdirSync(root);
    expect((await pool.toolCall(root, { sessionID: "resumed" }, "read")).text).toBe("ok");
  } finally {
    stats.mockRestore();
    realpaths.mockRestore();
    await pool.shutdown();
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test("A14 session lookup visits only matching carriers at 1 100 1000 sessions", async () => {
  const root = fs.mkdtempSync(join(tmpdir(), "aft-a14-session-"));
  const second = join(root, "linked");
  fs.mkdirSync(second);
  const { client } = fixtureClient();
  const pool = makePool(client);
  type RecordView = { identity: BindIdentity };
  const view = pool as unknown as {
    sessions: Map<string, RecordView>;
    unscopedHostRecord(session: string): RecordView | undefined;
  };
  const counts: number[] = [];
  try {
    for (const size of [1, 100, 1000]) {
      for (let i = view.sessions.size; i < size; i++) {
        await pool.toolCall(root, { sessionID: `session-${i}` }, "read");
      }
      let visits = 0;
      for (const record of view.sessions.values()) {
        const session = record.identity.session;
        Object.defineProperty(record.identity, "session", {
          configurable: true,
          get: () => {
            visits++;
            return session;
          },
        });
      }
      const selected = view.unscopedHostRecord(`session-${size - 1}`);
      counts.push(visits);
      expect(selected?.identity.project_root).toBe(root);
    }
    console.log("A14 matching-carrier visits:", counts);
    expect(counts).toEqual([1, 1, 1]);
    const first = view.unscopedHostRecord("session-999");
    await pool.toolCall(second, { sessionID: "session-999" }, "read");
    expect(view.unscopedHostRecord("session-999")).toBe(first);
    await pool.closeSession(root, "session-999");
    expect(view.unscopedHostRecord("session-999")?.identity.project_root).toBe(second);
    await pool.closeSession(second, "session-999");
    expect(view.unscopedHostRecord("session-999")).toBeUndefined();
    await pool.toolCall(root, { sessionID: "session-999" }, "read");
    expect(view.unscopedHostRecord("session-999")?.identity.project_root).toBe(root);
    await pool.shutdown();
    expect(view.unscopedHostRecord("session-999")).toBeUndefined();
  } finally {
    await pool.shutdown();
    fs.rmSync(root, { recursive: true, force: true });
  }
});
