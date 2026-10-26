/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { projectAftCall, registerAftBinding, TOOL_SOURCE_PATH } from "../aft-contract.js";
import { setHarnessOverrideForTesting } from "../harness.js";
import type { OmpInternalUrlRouter } from "../omp-internal-urls.js";
import { makeExtContext, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

function tmpProjectDir(): string {
  return mkdtempSync(join(tmpdir(), "aft-contract-test-"));
}

interface MockHostApi {
  registerTool: () => void;
  on: (event: string, handler: (...args: unknown[]) => void) => void;
  events: {
    on: (event: string, handler: (payload: unknown) => void) => () => void;
    emit: (event: string, payload: unknown) => void;
  };
  getAllTools?: () => Array<{ name: string; sourceInfo?: { source?: string; path?: string } }>;
  getActiveTools?: () => string[];
}

interface MockHost {
  api: ExtensionAPI;
  /** Fire a `pi.on(event, handler)` lifecycle hook (e.g. `session_start`), forwarding every arg. */
  emitLifecycle: (event: string, ...args: unknown[]) => void;
  /** Fire a `pi.events.on(event, handler)` synchronous bus event (e.g. `aft:binding-request`). */
  emitBus: (event: string, payload: unknown) => void;
  /** Every event name a `pi.on(event, handler)` lifecycle hook was registered for. */
  registeredLifecycleEvents: () => string[];
}

function makeMockHost(overrides: Partial<MockHostApi> = {}): MockHost {
  const busHandlers = new Map<string, Array<(payload: unknown) => void>>();
  const lifecycleHandlers = new Map<string, Array<(...args: unknown[]) => void>>();
  const api: MockHostApi = {
    registerTool: () => {},
    on: (event, handler) => {
      const list = lifecycleHandlers.get(event) ?? [];
      list.push(handler);
      lifecycleHandlers.set(event, list);
    },
    events: {
      on: (event, handler) => {
        const list = busHandlers.get(event) ?? [];
        list.push(handler);
        busHandlers.set(event, list);
        return () => {
          busHandlers.set(
            event,
            (busHandlers.get(event) ?? []).filter((h) => h !== handler),
          );
        };
      },
      emit: (event, payload) => {
        for (const handler of busHandlers.get(event) ?? []) handler(payload);
      },
    },
    ...overrides,
  };
  return {
    api: api as unknown as ExtensionAPI,
    emitLifecycle: (event, ...args) => {
      for (const handler of lifecycleHandlers.get(event) ?? []) handler(...args);
    },
    emitBus: (event, payload) => api.events.emit(event, payload),
    registeredLifecycleEvents: () => [...lifecycleHandlers.keys()],
  };
}

/** A session_start-shaped ExtensionContext stub carrying the given sessionId. */
function sessionExtCtx(cwd: string, sessionId: string) {
  return { cwd, sessionManager: { getSessionId: () => sessionId } };
}

describe("registerAftBinding — harness gating", () => {
  afterEach(() => setHarnessOverrideForTesting(null));

  test("does not subscribe outside the OMP harness", () => {
    setHarnessOverrideForTesting("pi");
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const { api, emitLifecycle, emitBus } = makeMockHost();
    let answered = false;
    api.events.on("aft:binding", () => {
      answered = true;
    });
    registerAftBinding(api, ctx);
    emitLifecycle("session_start", undefined, sessionExtCtx(process.cwd(), "s1"));
    emitBus("aft:binding-request", { v: 1, sessionId: "s1", requestId: "r1" });
    expect(answered).toBe(false);
  });

  test("never registers a tool_call handler (it would disable OMP's speculative batch reads/tasks)", () => {
    setHarnessOverrideForTesting("omp");
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const { api, registeredLifecycleEvents } = makeMockHost();
    registerAftBinding(api, ctx);
    expect(registeredLifecycleEvents()).not.toContain("tool_call");
  });
});

describe("registerAftBinding — request admission (OMP)", () => {
  afterEach(() => setHarnessOverrideForTesting(null));

  function setup(toolEntries: Array<{ name: string; sourceInfo?: { path?: string } }>) {
    setHarnessOverrideForTesting("omp");
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const { api, emitLifecycle, emitBus } = makeMockHost({
      getAllTools: () => toolEntries,
      getActiveTools: () => toolEntries.map((entry) => entry.name),
    });
    const responses: unknown[] = [];
    api.events.on("aft:binding", (payload) => responses.push(payload));
    registerAftBinding(api, ctx);
    emitLifecycle("session_start", undefined, sessionExtCtx(process.cwd(), "session-1"));
    return { emitBus, responses };
  }

  test("ignores a request with the wrong version", () => {
    const { emitBus, responses } = setup([
      { name: "read", sourceInfo: { path: TOOL_SOURCE_PATH } },
    ]);
    emitBus("aft:binding-request", { v: 2, sessionId: "session-1", requestId: "r1" });
    expect(responses).toEqual([]);
  });

  test("ignores a request for a different session", () => {
    const { emitBus, responses } = setup([
      { name: "read", sourceInfo: { path: TOOL_SOURCE_PATH } },
    ]);
    emitBus("aft:binding-request", { v: 1, sessionId: "other-session", requestId: "r1" });
    expect(responses).toEqual([]);
  });

  test("ignores a request missing requestId", () => {
    const { emitBus, responses } = setup([
      { name: "read", sourceInfo: { path: TOOL_SOURCE_PATH } },
    ]);
    emitBus("aft:binding-request", { v: 1, sessionId: "session-1" });
    expect(responses).toEqual([]);
  });

  test("answers a well-formed request with only the live, exact-source tools", () => {
    const { emitBus, responses } = setup([
      { name: "read", sourceInfo: { path: TOOL_SOURCE_PATH } },
      { name: "aft_outline", sourceInfo: { path: TOOL_SOURCE_PATH } },
      // A same-named tool another extension registered later: excluded,
      // even though the name is one AFT would otherwise report.
      { name: "edit", sourceInfo: { path: "/some/other/extension/index.js" } },
      // Not a tool AFT ever registers: excluded regardless of source path.
      { name: "not_an_aft_tool", sourceInfo: { path: TOOL_SOURCE_PATH } },
    ]);
    emitBus("aft:binding-request", { v: 1, sessionId: "session-1", requestId: "r1" });
    expect(responses).toHaveLength(1);
    const response = responses[0] as {
      v: number;
      sessionId: string;
      requestId: string;
      tools: Array<{ name: string; sourcePath: string; capabilities: string[] }>;
      project: unknown;
    };
    expect(response.v).toBe(1);
    expect(response.sessionId).toBe("session-1");
    expect(response.requestId).toBe("r1");
    expect(typeof response.project).toBe("function");
    expect(response.tools.map((t) => t.name).sort()).toEqual(["aft_outline", "read"]);
    const read = response.tools.find((t) => t.name === "read")!;
    expect(read.sourcePath).toBe(TOOL_SOURCE_PATH);
    expect(read.capabilities.sort()).toEqual(["native-url", "observation"].sort());
  });

  test("stays silent when the host registry is unreadable (no answer, not an empty-tools answer)", () => {
    setHarnessOverrideForTesting("omp");
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const { api, emitLifecycle, emitBus } = makeMockHost(); // no getAllTools at all
    const responses: unknown[] = [];
    api.events.on("aft:binding", (payload) => responses.push(payload));
    registerAftBinding(api, ctx);
    emitLifecycle("session_start", undefined, sessionExtCtx(process.cwd(), "session-1"));
    emitBus("aft:binding-request", { v: 1, sessionId: "session-1", requestId: "r1" });
    expect(responses).toEqual([]);
  });
});

describe("projectAftCall — filesystem-mutating tools", () => {
  test("edit (non-hashline): exact single-file mutation target", async () => {
    const dir = tmpProjectDir();
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(
      ctx,
      "edit",
      { path: "a.ts", edits: [{ oldString: "x", newString: "y" }] },
      dir,
    );
    expect(projection.kind).toBe("mutation");
    expect(projection.paths).toEqual([join(dir, "a.ts")]);
    expect(projection.broad).toBe(false);
  });

  test("edit (hashline): reuses the real preflight parser for exact multi-file targets", async () => {
    const dir = tmpProjectDir();
    const { bridge } = makeMockBridge((command) => {
      if (command === "hashline_preflight") {
        return {
          success: true,
          affected_paths: ["a.ts"],
          mv_destinations: ["b.ts"],
        };
      }
      return { success: true };
    });
    const ctx = makePluginContext(bridge, { hashlineEffective: true });
    const projection = await projectAftCall(ctx, "edit", { patch: "[a.ts#AAAA]\nREM\n" }, dir);
    expect(projection.kind).toBe("mutation");
    expect(projection.paths.sort()).toEqual([join(dir, "a.ts"), join(dir, "b.ts")].sort());
    expect(projection.broad).toBe(false);
  });

  test("edit (hashline): a patch the preflight parser rejects is unknown, never empty-but-safe", async () => {
    const dir = tmpProjectDir();
    const { bridge } = makeMockBridge(() => ({ success: false, text: "malformed patch" }));
    const ctx = makePluginContext(bridge, { hashlineEffective: true });
    const projection = await projectAftCall(ctx, "edit", { patch: "garbage" }, dir);
    expect(projection.kind).toBe("unknown");
    expect(projection.paths).toEqual([]);
  });

  test("write: a GitHub comment target has no filesystem path", async () => {
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(
      ctx,
      "write",
      { path: "issue://42", content: "x" },
      "/repo",
    );
    expect(projection.kind).toBe("mutation");
    expect(projection.paths).toEqual([]);
  });

  test("aft_delete: recursive delete is a broad (ancestor) scope", async () => {
    const dir = tmpProjectDir();
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const recursive = await projectAftCall(
      ctx,
      "aft_delete",
      { files: ["sub"], recursive: true },
      dir,
    );
    expect(recursive.kind).toBe("mutation");
    expect(recursive.broad).toBe(true);
    const nonRecursive = await projectAftCall(ctx, "aft_delete", { files: ["a.ts"] }, dir);
    expect(nonRecursive.broad).toBe(false);
  });

  test("aft_delete: no files is unknown, never an empty-but-safe mutation", async () => {
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(ctx, "aft_delete", { files: [] }, "/repo");
    expect(projection.kind).toBe("unknown");
  });

  test("aft_move: destination is a real mutation target alongside the source", async () => {
    const dir = tmpProjectDir();
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(
      ctx,
      "aft_move",
      { path: "a.ts", destination: "sub/b.ts" },
      dir,
    );
    expect(projection.kind).toBe("mutation");
    expect(projection.paths).toEqual([join(dir, "a.ts"), join(dir, "sub/b.ts")]);
  });

  test("aft_move: moving a directory is a broad (ancestor) scope", async () => {
    const dir = tmpProjectDir();
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(
      ctx,
      "aft_move",
      { path: ".", destination: "renamed" },
      dir,
    );
    expect(projection.broad).toBe(true);
  });
});

describe("projectAftCall — aft_safety", () => {
  test("list/history are observation, never mutation", async () => {
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const list = await projectAftCall(ctx, "aft_safety", { op: "list" }, "/repo");
    expect(list.kind).toBe("observation");
    const history = await projectAftCall(
      ctx,
      "aft_safety",
      { op: "history", path: "a.ts" },
      "/repo",
    );
    expect(history.kind).toBe("observation");
  });

  test("restore without explicit targets reuses checkpoint_paths for exact targets, not unknown", async () => {
    const dir = tmpProjectDir();
    const { bridge } = makeMockBridge((command) => {
      if (command === "checkpoint_paths") {
        return { success: true, paths: ["a.ts", "b.ts"] };
      }
      return { success: true };
    });
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(ctx, "aft_safety", { op: "restore", name: "cp1" }, dir);
    expect(projection.kind).toBe("mutation");
    expect(projection.paths.sort()).toEqual([join(dir, "a.ts"), join(dir, "b.ts")].sort());
    expect(projection.broad).toBe(false);
  });

  test("restore with no checkpoint name is unknown", async () => {
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(ctx, "aft_safety", { op: "restore" }, "/repo");
    expect(projection.kind).toBe("unknown");
  });

  test("checkpoint without files/path is unknown (uninspectable full-project scope)", async () => {
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(
      ctx,
      "aft_safety",
      { op: "checkpoint", name: "cp1" },
      "/repo",
    );
    expect(projection.kind).toBe("unknown");
  });

  test("checkpoint with explicit files is a mutation, never an observation", async () => {
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(
      ctx,
      "aft_safety",
      { op: "checkpoint", name: "cp1", files: ["a.ts"] },
      "/repo",
    );
    expect(projection.kind).toBe("mutation");
    expect(projection.paths).toEqual(["/repo/a.ts"]);
  });

  test("undo: a failed preview is unknown, never a partial guess from the explicit path", async () => {
    const dir = tmpProjectDir();
    const { bridge } = makeMockBridge(() => ({ success: false, text: "preview unavailable" }));
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(ctx, "aft_safety", { op: "undo", path: "a.ts" }, dir);
    expect(projection.kind).toBe("unknown");
    expect(projection.paths).toEqual([]);
  });

  test("undo: an engine-confirmed empty scope is an honest empty mutation, not unknown", async () => {
    const dir = tmpProjectDir();
    const { bridge } = makeMockBridge((command) =>
      command === "undo_preview" ? { success: true, paths: [] } : { success: true },
    );
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(ctx, "aft_safety", { op: "undo" }, dir);
    expect(projection.kind).toBe("mutation");
    expect(projection.paths).toEqual([]);
  });

  test("restore: a failed preview is unknown, never a partial guess from explicit path/files", async () => {
    const dir = tmpProjectDir();
    const { bridge } = makeMockBridge(() => ({ success: false, text: "preview unavailable" }));
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(
      ctx,
      "aft_safety",
      { op: "restore", name: "cp1", path: "maybe-a-directory" },
      dir,
    );
    expect(projection.kind).toBe("unknown");
    expect(projection.paths).toEqual([]);
  });

  test("purge is a broad mutation on the purged path", async () => {
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(
      ctx,
      "aft_safety",
      { op: "purge", path: "secret.env" },
      "/repo",
    );
    expect(projection.kind).toBe("mutation");
    expect(projection.broad).toBe(true);
  });
});

describe("projectAftCall — AST dry-run versus apply", () => {
  test("ast_grep_replace with dryRun:true is observation", async () => {
    const dir = tmpProjectDir();
    writeFileSync(join(dir, "a.ts"), "const x = 1;");
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(
      ctx,
      "ast_grep_replace",
      { pattern: "$A", rewrite: "$A", lang: "typescript", paths: ["a.ts"], dryRun: true },
      dir,
    );
    expect(projection.kind).toBe("observation");
  });

  test("ast_grep_replace without dryRun applies, and is mutation", async () => {
    const dir = tmpProjectDir();
    writeFileSync(join(dir, "a.ts"), "const x = 1;");
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(
      ctx,
      "ast_grep_replace",
      { pattern: "$A", rewrite: "$A", lang: "typescript", paths: ["a.ts"] },
      dir,
    );
    expect(projection.kind).toBe("mutation");
  });

  test("ast_grep_search is always observation, regardless of scope breadth", async () => {
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(
      ctx,
      "ast_grep_search",
      { pattern: "$A", lang: "typescript" },
      "/repo",
    );
    expect(projection.kind).toBe("observation");
    expect(projection.broad).toBe(true);
  });
});

describe("projectAftCall — observation scope", () => {
  test("grep without a path scope is broad over cwd", async () => {
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(ctx, "grep", { pattern: "foo" }, "/repo");
    expect(projection.kind).toBe("observation");
    expect(projection.paths).toEqual(["/repo"]);
    expect(projection.broad).toBe(true);
  });

  test("aft_search path never narrows scope: always broad", async () => {
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(ctx, "aft_search", { query: "foo" }, "/repo");
    expect(projection.kind).toBe("observation");
    expect(projection.broad).toBe(true);
  });

  test("aft_callgraph requires a path; without one it is unknown", async () => {
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(
      ctx,
      "aft_callgraph",
      { op: "callers", symbol: "foo" },
      "/repo",
    );
    expect(projection.kind).toBe("unknown");
  });
});

describe("projectAftCall — native-URL delegation and unknown tools", () => {
  test("an OMP internal-URL read target is classified as native, not observation", async () => {
    const { bridge } = makeMockBridge();
    const router: OmpInternalUrlRouter = {
      canHandle: (input: string) => input.startsWith("proc://"),
      canResolve: (input: string) => input.startsWith("proc://"),
      writeTarget: () => undefined,
    };
    const ctx = makePluginContext(bridge, { ompRouter: router });
    const projection = await projectAftCall(ctx, "read", { path: "proc://123" }, "/repo");
    expect(projection.kind).toBe("native");
    expect(projection.paths).toEqual([]);
  });

  test("an unrecognized tool name is unknown", async () => {
    const { bridge } = makeMockBridge();
    const ctx = makePluginContext(bridge);
    const projection = await projectAftCall(ctx, "not_a_real_tool", { path: "a.ts" }, "/repo");
    expect(projection.kind).toBe("unknown");
  });
});

describe("session-owned mutation scope", () => {
  afterEach(() => setHarnessOverrideForTesting(null));

  test("undo projects the issuing session backup stack, not another session scope", async () => {
    const dir = tmpProjectDir();
    const { bridge } = makeMockBridge((command, params) =>
      command === "undo_preview"
        ? {
            success: true,
            paths: [params.session_id === "session-a" ? "docs/adr/private.md" : "ordinary.ts"],
          }
        : { success: true },
    );
    const ctx = makePluginContext(bridge);
    const first = await projectAftCall(
      ctx,
      "aft_safety",
      { op: "undo" },
      dir,
      makeExtContext(dir, "session-a"),
    );
    const second = await projectAftCall(
      ctx,
      "aft_safety",
      { op: "undo" },
      dir,
      makeExtContext(dir, "session-b"),
    );
    expect(first.paths).toEqual([join(dir, "docs/adr/private.md")]);
    expect(second.paths).toEqual([join(dir, "ordinary.ts")]);
  });

  test("a binding projects hashline targets in the session that requested it", async () => {
    setHarnessOverrideForTesting("omp");
    const dir = tmpProjectDir();
    const { bridge } = makeMockBridge();
    bridge.toolCall = async (sessionId) => ({
      success: true,
      text: "",
      affected_paths: [sessionId === "session-1" ? "docs/adr/private.md" : "ordinary.ts"],
      mv_destinations: [],
    });
    const ctx = makePluginContext(bridge, { hashlineEffective: true });
    const { api, emitLifecycle, emitBus } = makeMockHost({
      getAllTools: () => [{ name: "edit", sourceInfo: { path: TOOL_SOURCE_PATH } }],
      getActiveTools: () => ["edit"],
    });
    let response:
      | {
          project: (
            name: string,
            input: Record<string, unknown>,
            cwd: string,
          ) => Promise<{ paths: string[] }>;
        }
      | undefined;
    api.events.on("aft:binding", (payload) => {
      response = payload as typeof response;
    });
    registerAftBinding(api, ctx);
    emitLifecycle("session_start", undefined, sessionExtCtx(dir, "session-1"));
    emitBus("aft:binding-request", { v: 1, sessionId: "session-1", requestId: "r1" });
    const projection = await response?.project("edit", { patch: "[a.ts#AAAA]\nREM\n" }, dir);
    expect(projection?.paths).toEqual([join(dir, "docs/adr/private.md")]);
  });
});
