/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import type { AftProjectTransport } from "@cortexkit/aft-bridge";
import type {
  ExtensionAPI,
  ExtensionContext,
  ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import type { AftConfig } from "../config.js";
import { resetRecordedExtensionApi, setHarnessOverrideForTesting } from "../harness.js";
import { OMP_TOOL_TIERS, type OmpToolTier, ompCallTier } from "../omp-approval.js";
import {
  bindToolRegistrationFunnel,
  registerPiToolSurface,
  resolvePiToolSurface,
} from "../tool-registration.js";
import { callToolCallForSession, recordSessionCwd } from "../tools/_shared.js";
import type { PluginContext } from "../types.js";
import { makeMockApi, makeMockBridge, makePluginContext } from "./tool-test-utils.js";

const ROOT = "/work/project";

beforeEach(() => {
  resetRecordedExtensionApi();
  setHarnessOverrideForTesting(null);
  recordSessionCwd(ROOT);
});
afterEach(() => recordSessionCwd(process.cwd()));

type Approval = ((args: unknown) => unknown) | string | undefined;
type ApprovalMode = "always-ask" | "write" | "yolo";

/**
 * OMP's approval gate for a call with no `tools.approval.<tool>` user policy,
 * as oh-my-pi `packages/coding-agent/src/tools/approval.ts` resolves it:
 * - `getToolDecision`: a function-valued `approval` is called with the call's
 *   arguments; a missing one is tier `exec` (`normalizeDecision`).
 * - `resolveApproval`: `yolo` allows any tier; otherwise the mode allows tiers
 *   up to its ceiling (`always-ask` → read, `write` → write) and prompts above.
 * - `resolveApprovalFromContext`: `--auto-approve` forces `yolo`.
 * It is no more capable than OMP: a decision object's `policy` is not modelled,
 * because AFT never returns one.
 */
function ompGate(
  approval: Approval,
  args: unknown,
  mode: ApprovalMode,
  autoApprove = false,
): "allow" | "prompt" {
  const decision = typeof approval === "function" ? approval(args) : approval;
  const tier: OmpToolTier =
    decision === "read" || decision === "write" || decision === "exec" ? decision : "exec";
  const effective = autoApprove ? "yolo" : mode;
  if (effective === "yolo") return "allow";
  const rank = { read: 0, write: 1, exec: 2 } as const;
  const ceiling = effective === "always-ask" ? "read" : "write";
  return rank[tier] <= rank[ceiling] ? "allow" : "prompt";
}

function registered(harness: "pi" | "omp") {
  const config: AftConfig = { disabled_tools: [], bash: true };
  const { api, tools } = makeMockApi();
  const { bridge } = makeMockBridge(() => ({ success: true, text: "ok" }));
  registerPiToolSurface(
    api,
    makePluginContext(bridge, { config }),
    resolvePiToolSurface(config),
    harness,
  );
  return tools as unknown as Map<string, { approval?: Approval }>;
}

const EXPECTED_TIERS: Record<string, OmpToolTier> = {
  read: "read",
  grep: "read",
  aft_search: "read",
  aft_outline: "read",
  aft_zoom: "read",
  aft_callgraph: "read",
  aft_inspect: "read",
  aft_conflicts: "read",
  ast_grep_search: "read",
  bash_status: "read",
  bash_watch: "read",
  write: "write",
  edit: "write",
  aft_delete: "write",
  aft_move: "write",
  aft_import: "write",
  aft_safety: "write",
  ast_grep_replace: "write",
  bash: "exec",
  bash_write: "exec",
  bash_kill: "exec",
};

describe("OMP approval tiers", () => {
  test("every AFT tool registered on OMP declares its tier", () => {
    const tools = registered("omp");
    const names = [...tools.keys()].sort();
    // The Pi adapter has no glob or apply_patch, and PowerShell only
    // registers when the host has it.
    expect(names).toEqual(Object.keys(EXPECTED_TIERS).sort());
    for (const [name, tool] of tools) {
      expect(typeof tool.approval).toBe("function");
      expect(tool.approval instanceof Function && tool.approval({})).toBe(EXPECTED_TIERS[name]);
      expect(OMP_TOOL_TIERS[name]).toBe(EXPECTED_TIERS[name]);
    }
  });

  test("upstream Pi gets no approval field", () => {
    for (const [, tool] of registered("pi")) expect("approval" in tool).toBe(false);
  });

  test("write mode: a read does not prompt, bash does", () => {
    const tools = registered("omp");
    const read = tools.get("read")?.approval;
    const bash = tools.get("bash")?.approval;
    expect(ompGate(read, { path: "src/a.ts" }, "write")).toBe("allow");
    expect(ompGate(bash, { command: "ls" }, "write")).toBe("prompt");
    expect(ompGate(tools.get("write")?.approval, { path: "src/a.ts" }, "write")).toBe("allow");
    // Without a declared tier OMP would have prompted for the read too.
    expect(ompGate(undefined, { path: "src/a.ts" }, "write")).toBe("prompt");
  });

  test("write mode prompts once for an outside read and not for an in-project one", () => {
    const read = registered("omp").get("read")?.approval;
    expect(ompGate(read, { path: "/elsewhere/notes.md" }, "write")).toBe("prompt");
    expect(ompGate(read, { path: "../sibling/notes.md" }, "write")).toBe("prompt");
    expect(ompGate(read, { path: "~/notes.md" }, "write")).toBe("prompt");
    expect(ompGate(read, { path: `${ROOT}/src/a.ts` }, "write")).toBe("allow");
    expect(ompGate(read, { path: "src/../README.md" }, "write")).toBe("allow");
  });

  test("yolo and --auto-approve never prompt, outside paths and bash included", () => {
    const tools = registered("omp");
    for (const [name, args] of [
      ["read", { path: "/elsewhere/notes.md" }],
      ["write", { path: "/elsewhere/notes.md", content: "x" }],
      ["bash", { command: "ls" }],
    ] as const) {
      const approval = tools.get(name)?.approval;
      expect(ompGate(approval, args, "yolo")).toBe("allow");
      expect(ompGate(approval, args, "write", true)).toBe("allow");
      expect(ompGate(approval, args, "always-ask", true)).toBe("allow");
    }
  });

  test("always-ask allows in-project reads and prompts for writes and outside reads", () => {
    const tools = registered("omp");
    expect(ompGate(tools.get("grep")?.approval, { path: "src" }, "always-ask")).toBe("allow");
    expect(ompGate(tools.get("grep")?.approval, { path: "/etc" }, "always-ask")).toBe("prompt");
    expect(ompGate(tools.get("edit")?.approval, { path: "src/a.ts" }, "always-ask")).toBe("prompt");
  });

  test("batch and multi-path arguments take the strictest tier", () => {
    const tools = registered("omp");
    const zoom = tools.get("aft_zoom")?.approval as (args: unknown) => unknown;
    expect(zoom({ batch: [{ target: "src/a.ts", symbols: "f" }] })).toBe("read");
    expect(
      zoom({
        batch: [
          { target: "src/a.ts", symbols: "f" },
          { target: "/elsewhere/b.ts", symbols: "g" },
        ],
      }),
    ).toBe("exec");
    // A URL target names no filesystem path.
    expect(zoom({ batch: [{ target: "https://example.com/docs", symbols: "Intro" }] })).toBe(
      "read",
    );
    const move = tools.get("aft_move")?.approval as (args: unknown) => unknown;
    expect(move({ path: "a.ts", destination: "b.ts" })).toBe("write");
    expect(move({ path: "a.ts", destination: "/elsewhere/b.ts" })).toBe("exec");
    const del = tools.get("aft_delete")?.approval as (args: unknown) => unknown;
    expect(del({ files: ["a.ts", "/elsewhere/b.ts"] })).toBe("exec");
    const inspect = tools.get("aft_inspect")?.approval as (args: unknown) => unknown;
    expect(inspect({ scope: ["src", "file:///elsewhere/x.ts"] })).toBe("exec");
  });

  test("a malformed call still gets a tier instead of throwing", () => {
    const read = registered("omp").get("read")?.approval as (args: unknown) => unknown;
    expect(read(null)).toBe("read");
    expect(read("not an object")).toBe("read");
  });
});

describe("OMP outside access end to end", () => {
  test("write mode asks once, through OMP's own gate, for an outside read", async () => {
    const sends: Array<Record<string, unknown>> = [];
    const bridge = {
      toolCall: async (_session: unknown, _name: string, args: Record<string, unknown>) => {
        sends.push(structuredClone(args));
        return sends.length === 1
          ? {
              success: false,
              code: "permission_required",
              asks: [
                {
                  kind: "external_directory",
                  grant_id: "outside:notes",
                  path: "/elsewhere/notes.md",
                  operation: "read",
                },
              ],
            }
          : { success: true, text: "notes body" };
      },
    } as unknown as AftProjectTransport;
    let tool: (ToolDefinition & { approval?: Approval }) | undefined;
    const pi = bindToolRegistrationFunnel(
      {
        registerTool: (definition: ToolDefinition) => {
          tool = definition;
        },
      } as unknown as ExtensionAPI,
      { config: { pi: { external_access_ask: true } } } as unknown as PluginContext,
      "omp",
    );
    pi.registerTool({
      name: "read",
      label: "read",
      description: "fixture",
      parameters: Type.Object({ path: Type.String() }),
      execute: async (_id, params, _signal, _update, ctx) => {
        const result = await callToolCallForSession(bridge, "read", params, "s", ctx);
        return { content: [{ type: "text" as const, text: result.text }], details: {} };
      },
    });
    if (!tool) throw new Error("tool not registered");
    const confirm = mock(async () => true);
    const extCtx = { hasUI: true, mode: "tui", ui: { confirm }, cwd: ROOT };
    const args = { path: "/elsewhere/notes.md" };

    // OMP's gate runs first and asks the user; AFT then runs without asking.
    if (ompGate(tool.approval, args, "write") === "prompt") await confirm();
    const outcome = await tool.execute(
      "call",
      args,
      new AbortController().signal,
      undefined,
      extCtx as unknown as ExtensionContext,
    );
    expect((outcome.content[0] as { text: string }).text).toBe("notes body");
    expect(confirm).toHaveBeenCalledTimes(1);
    expect(sends[1]?.permissions_granted).toEqual(["outside:notes"]);
  });
});

describe("OMP tiers for patch targets", () => {
  function hashlineEdit() {
    const config: AftConfig = { disabled_tools: [], edit_mode: "hashline" };
    const { api, tools } = makeMockApi();
    const { bridge } = makeMockBridge(() => ({ success: true, text: "ok" }));
    registerPiToolSurface(
      api,
      makePluginContext(bridge, { config, hashlineEffective: true }),
      resolvePiToolSurface(config),
      "omp",
    );
    const approval = (tools as unknown as Map<string, { approval?: Approval }>).get("edit")
      ?.approval as (args: unknown) => unknown;
    return (patch: string) => approval({ patch });
  }

  test("an MV from a project file to an outside path is exec", () => {
    const tier = hashlineEdit();
    expect(tier("[src/a.ts#CAFE]\nMV /elsewhere/a.ts\n")).toBe("exec");
    expect(tier("*** Begin Patch\n[src/a.ts#CAFE]\nMV '../sibling/a.ts'\n*** End Patch\n")).toBe(
      "exec",
    );
  });

  test("a section editing an outside file is exec", () => {
    const tier = hashlineEdit();
    expect(tier("[/elsewhere/a.ts#CAFE]\nPUT 1:\n+x\n")).toBe("exec");
    // The outside file is the second section, after an in-project one.
    expect(tier("[src/a.ts#CAFE]\nREM\n[~/notes.md#BEEF]\nCUT 2\n")).toBe("exec");
  });

  test("an unparseable patch is exec", () => {
    const tier = hashlineEdit();
    expect(tier("")).toBe("exec");
    expect(tier("PUT 1:\n+x\n")).toBe("exec");
    expect(tier("[src/a.ts]\nPUT 1:\n+x\n")).toBe("exec");
    expect(tier("[src/a.ts#CAFE]\nPUT 1:\nnot a body row\n")).toBe("exec");
    expect(tier("[src/a.ts#CAFE]\nMV\n")).toBe("exec");
    expect(tier("[src/a.ts#CAFE]\nMV two words\n")).toBe("exec");
  });

  test("an in-project patch stays write", () => {
    const tier = hashlineEdit();
    expect(
      tier(
        "*** Begin Patch\n[src/a.ts#CAFE]\nPUT >25:\n+inserted\n+\nCUT 13.=14 @fragment\nMV src/b.ts\n[src/c.ts#BEEF]\nPUT $ @fragment\n*** End Patch\n",
      ),
    ).toBe("write");
  });

  test("apply_patch file and move targets are checked the same way", () => {
    const tier = (patchText: string) => ompCallTier("apply_patch", { patchText }, ROOT);
    expect(tier("*** Begin Patch\n*** Add File: src/new.ts\n+x\n*** End Patch\n")).toBe("write");
    expect(tier("*** Begin Patch\n*** Add File: /elsewhere/new.ts\n+x\n*** End Patch\n")).toBe(
      "exec",
    );
    expect(
      tier(
        "*** Begin Patch\n*** Update File: src/a.ts\n*** Move to: ../out/a.ts\n@@\n-a\n+b\n*** End Patch\n",
      ),
    ).toBe("exec");
    expect(tier("*** Begin Patch\n*** Delete File: ~/notes.md\n*** End Patch\n")).toBe("exec");
    expect(tier("not a patch")).toBe("exec");
  });
});
