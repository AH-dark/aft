/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, spyOn, test } from "bun:test";
import * as fs from "node:fs";
import type {
  BindIdentity,
  RequestOptions,
  RouteHandle,
  RouteTarget,
} from "@cortexkit/subc-client";
import { getActiveLogger, setActiveLogger } from "../active-logger.js";
import type { Logger } from "../logger.js";
import { RevivableTransportPool } from "../revivable-transport.js";
import {
  type SubcClientLike,
  type SubcSubscriptionLike,
  SubcTransportPool,
} from "../subc-transport.js";
import { TEST_PROJECT_ROOT } from "./subc-test-roots.js";

class FakeClient implements SubcClientLike {
  readonly routeOpens: BindIdentity[] = [];
  closed = 0;
  private nextChannel = 1;

  async routeOpen(_target: RouteTarget, identity: BindIdentity): Promise<RouteHandle> {
    this.routeOpens.push(identity);
    const channel = this.nextChannel++;
    return { channel, epoch: channel } as RouteHandle;
  }

  async request(_route: RouteHandle, _body: unknown, _options?: RequestOptions): Promise<unknown> {
    return {
      structuredContent: {
        success: true,
        text: "revived",
      },
    };
  }

  subscribe(
    _route: RouteHandle,
    _body: unknown,
    _onEvent: (event: Uint8Array) => void,
  ): SubcSubscriptionLike {
    return { unsubscribe: () => undefined };
  }

  async closeRouteChannel(_route: RouteHandle): Promise<void> {}

  close(): void {
    this.closed += 1;
  }
}

function makeSubcPool(client: FakeClient): SubcTransportPool {
  return new SubcTransportPool({
    connectionFile: "/tmp/fake-subc-connection.json",
    harness: "opencode",
    connect: async () => client,
  });
}

describe("RevivableTransportPool", () => {
  let previousLogger: Logger | undefined;

  beforeEach(() => {
    previousLogger = getActiveLogger();
  });

  afterEach(() => {
    const slot = globalThis as Record<symbol, unknown>;
    const key = Symbol.for("aft-bridge-active-logger");
    if (previousLogger) setActiveLogger(previousLogger);
    else delete slot[key];
  });

  test("dispatch refresh reuses the canonical facade without another realpath", async () => {
    const client = new FakeClient();
    const owner = new RevivableTransportPool(makeSubcPool(client), async () => {
      throw new Error("the live pool must not be revived");
    });
    const transport = owner.getBridge(TEST_PROJECT_ROOT);
    const realpaths = spyOn(fs, "realpathSync");
    try {
      for (let i = 0; i < 100; i++) {
        expect((await owner.toolCall(TEST_PROJECT_ROOT, { sessionID: "hot" }, "read")).text).toBe(
          "revived",
        );
      }
      // Resolve the project directory once per request; the underlying pool still checks that it exists.
      expect(realpaths.mock.calls.length).toBe(100);
      realpaths.mockClear();
      for (let i = 0; i < 100; i++) {
        expect((await transport.send("status", { session_id: "hot" })).text).toBe("revived");
      }
      expect(realpaths.mock.calls.length).toBe(100);
    } finally {
      realpaths.mockRestore();
      await owner.shutdown();
    }
  });

  test("revives a shut-down pool with fresh routes and repeats after the replacement shuts down", async () => {
    const initialClient = new FakeClient();
    const revivedClient = new FakeClient();
    const clients = [initialClient, revivedClient];
    let created = 0;
    const initialPool = makeSubcPool(initialClient);
    const owner = new RevivableTransportPool(initialPool, async () => {
      const client = clients[++created];
      if (!client) throw new Error("unexpected extra pool creation");
      return makeSubcPool(client);
    });
    const transport = owner.getBridge(TEST_PROJECT_ROOT);

    await transport.toolCall("before-shutdown", "read", {});
    await owner.shutdown();
    expect(owner.isShutdown()).toBe(true);
    expect(initialClient.closed).toBe(1);

    const revived = await transport.toolCall("after-shutdown", "read", {});
    expect(revived.text).toBe("revived");
    expect(created).toBe(1);
    expect(initialClient.routeOpens.map((identity) => identity.session)).toEqual([
      "before-shutdown",
    ]);
    expect(revivedClient.routeOpens.map((identity) => identity.session)).toEqual([
      "after-shutdown",
    ]);

    await owner.shutdown();
    expect(owner.isShutdown()).toBe(true);
    expect(revivedClient.closed).toBe(1);
  });

  test("a deferred pool initializes once on demand and preserves pre-start configuration", async () => {
    let created = 0;
    const client = new FakeClient();
    const overrides: Array<[string, unknown]> = [];
    const owner = new RevivableTransportPool(null, async () => {
      created += 1;
      const pool = makeSubcPool(client);
      pool.setConfigureOverride = (key, value) => {
        overrides.push([key, value]);
      };
      return pool;
    });
    const transport = owner.getBridge(TEST_PROJECT_ROOT);
    owner.setConfigureOverride("edit_slot_survives", true);
    await owner.reconfigure(TEST_PROJECT_ROOT, { lsp_paths_extra: ["/cache"] });
    await owner.closeSession(TEST_PROJECT_ROOT, "unused");
    expect(owner.activeBridges()).toEqual([]);
    expect(owner.getActiveBridgeForRoot(TEST_PROJECT_ROOT)).toBeNull();
    expect(owner.isShutdown()).toBe(false);
    expect(created).toBe(0);
    const results = await Promise.all([
      transport.toolCall("one", "read", {}),
      transport.toolCall("two", "read", {}),
    ]);
    expect(results.map((result) => result.text)).toEqual(["revived", "revived"]);
    expect(created).toBe(1);
    expect(overrides).toEqual([
      ["edit_slot_survives", true],
      ["lsp_paths_extra", ["/cache"]],
    ]);
    await owner.shutdown();
    expect(client.closed).toBe(1);
  });

  test("fresh-session hints reported before a deferred pool exists reach the pool it creates", async () => {
    const observed: Array<[string, string]> = [];
    let created = 0;
    const owner = new RevivableTransportPool(null, async () => {
      created += 1;
      const pool = makeSubcPool(new FakeClient());
      pool.observeSessionStart = (root: string, session: string) => {
        observed.push([root, session]);
      };
      return pool;
    });
    // Pi reports `session_start` before the first tool call creates the pool.
    // This must neither throw nor create the pool, and the hint must survive.
    owner.observeSessionStart(TEST_PROJECT_ROOT, "fresh");
    for (let i = 0; i < 300; i++) owner.observeSessionStart(TEST_PROJECT_ROOT, `extra-${i}`);
    expect(created).toBe(0);
    await owner.getBridge(TEST_PROJECT_ROOT).toolCall("extra-299", "read", {});
    expect(created).toBe(1);
    // Bounded: the oldest unused hints are dropped (that session then counts as
    // unobserved and is retained), the newest reach the pool exactly once.
    expect(observed.length).toBe(256);
    expect(observed.at(-1)).toEqual([TEST_PROJECT_ROOT, "extra-299"]);
    expect(observed.some(([, session]) => session === "fresh")).toBe(false);
    owner.observeSessionStart(TEST_PROJECT_ROOT, "after-create");
    expect(observed.at(-1)).toEqual([TEST_PROJECT_ROOT, "after-create"]);
    expect(observed.length).toBe(257);
    await owner.shutdown();
  });

  test("shutting down an unused deferred pool does not initialize it", async () => {
    let created = 0;
    const owner = new RevivableTransportPool(null, async () => {
      created += 1;
      return makeSubcPool(new FakeClient());
    });
    await owner.shutdown("validation finished");
    expect(created).toBe(0);
    expect(owner.isShutdown()).toBe(true);
  });

  test("reconfigure during or after shutdown records overrides without reviving the pool", async () => {
    let created = 0;
    const revivedOverrides: Array<[string, unknown]> = [];
    const owner = new RevivableTransportPool(makeSubcPool(new FakeClient()), async () => {
      created += 1;
      const pool = makeSubcPool(new FakeClient());
      const setOverride = pool.setConfigureOverride.bind(pool);
      pool.setConfigureOverride = (key: string, value: unknown) => {
        revivedOverrides.push([key, value]);
        setOverride(key, value);
      };
      return pool;
    });
    const transport = owner.getBridge(TEST_PROJECT_ROOT);

    // A reconfigure issued while the host's shutdown is still settling (the
    // shape of an aborted background install finishing mid-shutdown) must not
    // install a live replacement pool behind the completed shutdown.
    const shuttingDown = owner.shutdown("host quit");
    await owner.reconfigure(TEST_PROJECT_ROOT, { lsp_paths_extra: ["/cache/a"] });
    await shuttingDown;
    await owner.reconfigure(TEST_PROJECT_ROOT, { lsp_paths_extra: ["/cache/b"] });

    expect(created).toBe(0);
    expect(owner.isShutdown()).toBe(true);

    // Real demand still revives, and the replacement carries the latest paths.
    await transport.toolCall("after-shutdown", "read", {});
    expect(created).toBe(1);
    expect(revivedOverrides).toContainEqual(["lsp_paths_extra", ["/cache/b"]]);
    await owner.shutdown();
  });

  test("captures edit-slot registration once for subc-backed plugin pools", () => {
    const initialPool = makeSubcPool(new FakeClient());
    const owner = new RevivableTransportPool(initialPool, async () =>
      makeSubcPool(new FakeClient()),
    );

    owner.setConfigureOverride("edit_slot_survives", true);

    expect(initialPool.getEditSlotSurvives()).toBe(true);
    expect(() => owner.setConfigureOverride("edit_slot_survives", false)).toThrow(
      "edit_slot_survives is write-once",
    );
  });

  test("concurrent demand during revival shares one replacement instance", async () => {
    const initialClient = new FakeClient();
    const revivedClient = new FakeClient();
    let created = 0;
    let releaseRevival!: () => void;
    const revivalGate = new Promise<void>((resolve) => {
      releaseRevival = resolve;
    });
    const owner = new RevivableTransportPool(makeSubcPool(initialClient), async () => {
      created += 1;
      await revivalGate;
      return makeSubcPool(revivedClient);
    });
    const transport = owner.getBridge(TEST_PROJECT_ROOT);

    await owner.shutdown();
    const first = transport.toolCall("session-a", "read", {});
    const second = transport.toolCall("session-b", "read", {});
    await Promise.resolve();
    expect(created).toBe(1);
    releaseRevival();

    await Promise.all([first, second]);
    expect(created).toBe(1);
    expect(revivedClient.routeOpens.map((identity) => identity.session)).toEqual([
      "session-a",
      "session-b",
    ]);
  });

  test("parks failed revival attempts so concurrent terminal calls cannot hot-spin", async () => {
    const initialClient = new FakeClient();
    let attempts = 0;
    const owner = new RevivableTransportPool(makeSubcPool(initialClient), () => {
      attempts += 1;
      throw new Error("subc unavailable");
    });
    const transport = owner.getBridge(TEST_PROJECT_ROOT);

    await owner.shutdown();
    await Promise.all(
      Array.from({ length: 20 }, () =>
        transport.toolCall("session", "read", {}).catch(() => undefined),
      ),
    );
    expect(attempts).toBe(1);

    // The failed terminal pool has no immediate retry work. Many arrivals share
    // one delayed retry instead of each recursively starting a fresh connection.
    const queued = Array.from({ length: 20 }, () =>
      transport.toolCall("session", "read", {}).catch(() => undefined),
    );
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(attempts).toBe(1);
    await Promise.all(queued);
    expect(attempts).toBe(2);
  });

  test("logs the explicit shutdown reason when demand revives the transport", async () => {
    const messages: string[] = [];
    setActiveLogger({
      log: () => undefined,
      warn: (message) => messages.push(message),
      error: () => undefined,
    });
    const initialClient = new FakeClient();
    const revivedClient = new FakeClient();
    const owner = new RevivableTransportPool(makeSubcPool(initialClient), async () =>
      makeSubcPool(revivedClient),
    );

    await owner.shutdown("dispose");
    await owner.getBridge(TEST_PROJECT_ROOT).toolCall("session", "read", {});

    expect(messages).toContain(
      "transport was shut down (reason: dispose) but new demand arrived — reviving",
    );
  });
});
