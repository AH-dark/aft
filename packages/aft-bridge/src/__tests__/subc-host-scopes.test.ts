import { afterEach, expect, spyOn, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  type BindIdentity,
  type ControlPush,
  type RouteHandle,
  type RouteOpenOptions,
  type RouteScope,
  type RouteTarget,
  SubcClient,
  SubcError,
} from "@cortexkit/subc-client";
import { getActiveLogger, setActiveLogger } from "../active-logger.js";
import type { Logger } from "../logger.js";
import { type SubcClientLike, SubcTransportPool } from "../subc-transport.js";
import { createAftTransportPool } from "../transport-factory.js";
import { TEST_OTHER_ROOT, TEST_PROJECT_ROOT } from "./subc-test-roots.js";

const scope: RouteScope = {
  owner: { kind: "reserved", module_id: "prefrontal-core" },
  ref: "head-scope",
  scopeEpoch: 7,
};

class HostClient implements SubcClientLike {
  readonly opens: Array<{ target: RouteTarget; identity: BindIdentity; opts?: RouteOpenOptions }> =
    [];
  readonly reads: unknown[] = [];
  readonly toolBodies: unknown[] = [];
  readonly closedRoutes: number[] = [];
  readonly modules = new Map<number, string>();
  answer: unknown = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
  refusal: string | undefined;
  requestError: SubcError | undefined;
  scopeOpenError: SubcError | undefined;
  scopeOpenGate: Promise<void> | undefined;
  subscriptionChannels: number[] = [];

  async routeOpen(target: RouteTarget, identity: BindIdentity, opts?: RouteOpenOptions) {
    this.opens.push({ target, identity, opts });
    const channel = this.opens.length;
    if (target.module_id === "prefrontal-core" && this.scopeOpenError) throw this.scopeOpenError;
    if (target.module_id === "prefrontal-core") await this.scopeOpenGate;
    if (opts?.scope && this.refusal) throw new SubcError("carrier refused", this.refusal);
    this.modules.set(channel, target.module_id);
    return { channel, epoch: 1 } as RouteHandle;
  }

  async request(route: RouteHandle, body: unknown) {
    if (this.modules.get(route.channel) === "prefrontal-core") {
      this.reads.push(body);
      if (this.answer instanceof Error) throw this.answer;
      return this.answer;
    }
    this.toolBodies.push(body);
    if (this.requestError) {
      const error = this.requestError;
      this.requestError = undefined;
      throw error;
    }
    return { structuredContent: { success: true, text: "ok" } };
  }

  subscribe(route: RouteHandle) {
    this.subscriptionChannels.push(route.channel);
    let finish!: () => void;
    return {
      closed: new Promise<void>((resolve) => {
        finish = resolve;
      }),
      unsubscribe: () => finish(),
    };
  }

  async closeRouteChannel(route: RouteHandle) {
    this.closedRoutes.push(route.channel);
  }

  close() {}
}

const pools: SubcTransportPool[] = [];
const originalLogger = getActiveLogger();
const restoreClocks: Array<() => void> = [];
afterEach(async () => {
  await Promise.all(pools.splice(0).map((pool) => pool.shutdown()));
  for (const restore of restoreClocks.splice(0).reverse()) restore();
  setActiveLogger(originalLogger);
});

function fakeClock() {
  let now = Date.now();
  const clock = spyOn(Date, "now").mockImplementation(() => now);
  restoreClocks.push(() => clock.mockRestore());
  return {
    advance: (ms: number) => {
      now += ms;
    },
  };
}

function scopeLogs() {
  const lines: string[] = [];
  setActiveLogger({
    log: (line: string) => {
      if (line.startsWith("subc head scope:")) lines.push(line);
    },
    error() {},
    warn() {},
  } satisfies Logger);
  return lines;
}

function setup(harness = "opencode", background = false) {
  const client = new HostClient();
  let push!: (event: ControlPush) => void;
  const pool = new SubcTransportPool({
    connectionFile: "/fake/connection.json",
    harness,
    hostScopes: true,
    consumerIdentity: null,
    connect: async (opts) => {
      push = opts.onControlPush!;
      return client;
    },
    routeRetrySleep: async () => {},
    ...(background ? { onBgEventsNudge: () => {} } : {}),
  });
  pools.push(pool);
  return { client, pool, push: (event: ControlPush) => push(event) };
}

test("OpenCode and Pi direct carriers bind scoped heads once per host session", async () => {
  for (const harness of ["opencode", "pi"]) {
    const { client, pool } = setup(harness);
    await Promise.all([
      pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {}),
      pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {}),
    ]);
    expect(client.reads).toEqual([
      { op: "session.scope_identity", params: { harness, session: "head" } },
    ]);
    const tools = client.opens.filter((open) => open.target.module_id === "aft");
    expect(tools).toHaveLength(2);
    for (const open of tools) {
      expect(open.opts).toEqual({ consumerIdentity: null, scope });
    }
    expect(client.toolBodies).toEqual([
      { name: "read", arguments: {}, preset: "head" },
      { name: "read", arguments: {}, preset: "head" },
    ]);
    expect(client.closedRoutes).toHaveLength(1);
  }
});

test("both plugin factory harnesses enable host scopes on tool and background routes", async () => {
  const dir = mkdtempSync(join(tmpdir(), "aft-host-scopes-"));
  const connectionFile = join(dir, "connection.json");
  writeFileSync(connectionFile, "{}");
  const connect = spyOn(SubcClient, "connect");
  try {
    for (const harness of ["opencode", "pi"]) {
      const client = new HostClient();
      connect.mockResolvedValue(client as unknown as SubcClient);
      const pool = await createAftTransportPool({
        harness,
        binaryPath: null,
        subcConnectionFile: connectionFile,
        poolOptions: {},
        configOverrides: {},
        subcConsumerIdentity: null,
        onBgEventsNudge: () => {},
      });
      try {
        await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
        for (let turn = 0; turn < 32; turn++) await Promise.resolve();
        expect(client.reads).toHaveLength(1);
        expect(client.subscriptionChannels).toHaveLength(1);
        const tools = client.opens.filter((open) => open.target.module_id === "aft");
        expect(tools).toHaveLength(2);
        expect(tools.map((open) => open.opts?.scope)).toEqual([scope, scope]);
      } finally {
        await pool.shutdown();
      }
    }
  } finally {
    connect.mockRestore();
    rmSync(dir, { recursive: true, force: true });
  }
});

test("absent prefrontal module refuses only the lookup route, not the head bind", async () => {
  const { client, pool } = setup();
  client.scopeOpenError = new SubcError("module absent", "target_unavailable");
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  expect(client.reads).toHaveLength(0);
  expect(client.opens.at(-1)?.target.module_id).toBe("aft");
  expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
});

test("a stalled scope lookup falls back and closes its late management route", async () => {
  const { client, pool } = setup();
  let release!: () => void;
  client.scopeOpenGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
  release();
  for (let turn = 0; turn < 32; turn++) await Promise.resolve();
  expect(client.reads).toHaveLength(0);
  expect(client.closedRoutes).toEqual([1]);
});

test("a timed out head scope lookup retries and binds scoped after backoff", async () => {
  const clock = fakeClock();
  const lines = scopeLogs();
  const { client, pool } = setup();
  let release!: () => void;
  client.scopeOpenGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
  release();
  for (let turn = 0; turn < 32; turn++) await Promise.resolve();
  expect(client.closedRoutes).toEqual([1]);
  clock.advance(5_000);
  await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
  expect(client.opens.at(-1)?.opts?.scope).toEqual(scope);
  expect(client.reads).toHaveLength(1);
  expect(lines[0]).toMatch(/^subc head scope: session=head result=timeout after=\d+$/);
  expect(lines[1]).toBe("subc head scope: session=head result=scope ref=head-sco epoch=7");
});

test("head scope lookup errors are retried rather than cached", async () => {
  for (const source of ["route", "request"]) {
    const clock = fakeClock();
    const { client, pool } = setup();
    if (source === "route")
      client.scopeOpenError = new SubcError("unavailable", "target_unavailable");
    else client.answer = new Error("lookup failed");
    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
    expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
    client.scopeOpenError = undefined;
    client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
    clock.advance(5_000);
    await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
    expect(client.opens.at(-1)?.opts?.scope).toEqual(scope);
  }
});

test("explicit no_scope is cached across head binds", async () => {
  const clock = fakeClock();
  const lines = scopeLogs();
  const { client, pool } = setup();
  client.answer = { answer: "no_scope" };
  await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
  client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
  clock.advance(300_000);
  await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
  expect(client.reads).toHaveLength(1);
  expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
  expect(lines).toEqual(["subc head scope: session=head result=no_scope"]);
});

test("malformed head scope replies are retried rather than cached", async () => {
  for (const answer of [
    undefined,
    { answer: "unknown" },
    { owner: scope.owner, ref: scope.ref, scope_epoch: -1 },
  ]) {
    const clock = fakeClock();
    const { client, pool } = setup();
    client.answer = answer;
    await pool.getBridge(TEST_PROJECT_ROOT).toolCall("head", "read", {});
    expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
    client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
    clock.advance(5_000);
    await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
    expect(client.reads).toHaveLength(2);
    expect(client.opens.at(-1)?.opts?.scope).toEqual(scope);
  }
});

test("head scope retries back off exponentially to five minutes without delaying binds", async () => {
  const clock = fakeClock();
  const { client, pool } = setup();
  client.answer = new Error("lookup failed");
  let bind = 0;
  const rebind = () => {
    const root = join(TEST_PROJECT_ROOT, `retry-${bind++}`);
    mkdirSync(root, { recursive: true });
    return pool.getBridge(root).toolCall("head", "read", {});
  };
  await rebind();
  let reads = 1;
  for (const delay of [5_000, 10_000, 20_000, 40_000, 80_000, 160_000, 300_000, 300_000]) {
    clock.advance(delay - 1);
    await rebind();
    expect(client.reads).toHaveLength(reads);
    clock.advance(1);
    await rebind();
    expect(client.reads).toHaveLength(++reads);
  }
  client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: scope.scopeEpoch };
  clock.advance(300_000);
  await rebind();
  expect(client.opens.at(-1)?.opts?.scope).toEqual(scope);
});

test("head scope logs once per session outcome change without error messages or full refs", async () => {
  const clock = fakeClock();
  const lines = scopeLogs();
  const { client, pool, push } = setup();
  const request = client.request.bind(client);
  let elapsed = 0;
  spyOn(client, "request").mockImplementation(async (route, body) => {
    clock.advance(++elapsed);
    return request(route, body);
  });
  let bind = 0;
  const rebind = () => {
    const root = join(TEST_PROJECT_ROOT, `log-${bind++}`);
    mkdirSync(root, { recursive: true });
    return pool.getBridge(root).toolCall("head", "read", {});
  };
  client.answer = new SubcError("secret message", "target_unavailable");
  await rebind();
  clock.advance(5_000);
  await rebind();
  expect(lines).toHaveLength(1);
  expect(lines[0]).toMatch(
    /^subc head scope: session=head result=error code=target_unavailable after=\d+$/,
  );
  client.answer = new Error("another secret");
  clock.advance(10_000);
  await rebind();
  expect(lines[1]).toMatch(/^subc head scope: session=head result=error code=Error after=\d+$/);
  client.answer = {};
  clock.advance(20_000);
  await rebind();
  clock.advance(40_000);
  await rebind();
  expect(lines).toHaveLength(3);
  expect(lines[2]).toBe("subc head scope: session=head result=malformed");
  client.answer = { owner: scope.owner, ref: "private-ref-123456", scope_epoch: 7 };
  clock.advance(80_000);
  await rebind();
  expect(lines[3]).toBe("subc head scope: session=head result=scope ref=private- epoch=7");
  const channel = client.opens.length;
  push({ op: "route.closed", body: { channels: [channel], reason: "scope_ended" } });
  await rebind();
  expect(lines).toHaveLength(4);
  client.answer = { owner: scope.owner, ref: "private-ref-123456", scope_epoch: 8 };
  push({ op: "route.closed", body: { channels: [client.opens.length], reason: "scope_ended" } });
  await rebind();
  expect(lines[4]).toBe("subc head scope: session=head result=scope ref=private- epoch=8");
  client.answer = { answer: "no_scope" };
  push({ op: "route.closed", body: { channels: [client.opens.length], reason: "scope_ended" } });
  await rebind();
  expect(lines[5]).toBe("subc head scope: session=head result=no_scope");
  await pool.getBridge(TEST_OTHER_ROOT).toolCall("other-head", "read", {});
  expect(lines[6]).toBe("subc head scope: session=other-head result=no_scope");
  expect(lines).toHaveLength(7);
});

test("no_scope, missing prefrontal and lookup errors preserve an unscoped bind", async () => {
  for (const answer of [
    { answer: "no_scope" },
    new SubcError("module absent", "target_unavailable"),
    new Error("lookup failed"),
    { owner: scope.owner, ref: scope.ref, scope_epoch: -1 },
  ]) {
    const { client, pool } = setup();
    client.answer = answer;
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    await bridge.toolCall("head", "read", {});
    await bridge.toolCall("head", "read", {});
    expect(client.reads).toHaveLength(1);
    expect(client.opens.at(-1)?.opts?.scope).toBeUndefined();
  }
});

test("a scoped open carrier refusal falls back unscoped and logs its code once", async () => {
  const warnings: string[] = [];
  setActiveLogger({
    log() {},
    error() {},
    warn: (line: string) => warnings.push(line),
  } satisfies Logger);
  const { client, pool } = setup();
  client.refusal = "scope_carrier_denied";
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  await pool.getBridge(TEST_OTHER_ROOT).toolCall("head", "read", {});
  const tools = client.opens.filter((open) => open.target.module_id === "aft");
  expect(tools.map((open) => open.opts?.scope)).toEqual([scope, undefined, undefined]);
  expect(warnings).toHaveLength(1);
  expect(warnings[0]).toContain("scope_carrier_denied");
});

test("scope end and carrier removal invalidate an idle head cache and re-read its epoch", async () => {
  for (const reason of ["scope_ended", "scope_carrier_removed", "scope_delegation_changed"]) {
    const { client, pool, push } = setup();
    const bridge = pool.getBridge(TEST_PROJECT_ROOT);
    await bridge.toolCall("head", "read", {});
    const old = client.opens.length;
    client.answer = { owner: scope.owner, ref: "successor", scope_epoch: 8 };
    push({ op: "route.closed", body: { channels: [old], reason } });
    await bridge.toolCall("head", "read", {});
    expect(client.reads).toHaveLength(2);
    expect(client.opens.at(-1)?.opts?.scope).toEqual({ ...scope, ref: "successor", scopeEpoch: 8 });
  }
});

test("stale route epoch re-reads the host scope before retrying", async () => {
  const { client, pool } = setup();
  const bridge = pool.getBridge(TEST_PROJECT_ROOT);
  await bridge.toolCall("head", "read", {});
  client.answer = { owner: scope.owner, ref: scope.ref, scope_epoch: 8 };
  client.requestError = new SubcError("epoch changed", "stale_route_epoch");
  await bridge.toolCall("head", "read", {});
  expect(client.reads).toHaveLength(2);
  expect(client.opens.at(-1)?.opts?.scope?.scopeEpoch).toBe(8);
});
