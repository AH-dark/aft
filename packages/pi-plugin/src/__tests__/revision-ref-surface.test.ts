/// <reference path="../bun-test.d.ts" />
/**
 * The `ref` (git revision) argument on Pi's read, aft_outline and aft_zoom:
 * offered only while `revision_reads` is on, and forwarded to the engine
 * whenever a caller sends one so a stale `ref` is refused by name.
 */

import { describe, expect, test } from "bun:test";
import { REVISION_REF_TOOL_NOTE } from "@cortexkit/aft-bridge";
import { registerHoistedTools } from "../tools/hoisted.js";
import { registerReadingTools } from "../tools/reading.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

function registered(config: Record<string, unknown>) {
  const { api, tools } = makeMockApi();
  const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "reply" }));
  const ctx = makePluginContext(bridge, { config });
  registerHoistedTools(api, ctx, {
    hoistRead: true,
    hoistWrite: false,
    hoistEdit: false,
    hoistGrep: false,
    restrictToProjectRoot: false,
  });
  registerReadingTools(api, ctx, { outline: true, zoom: true });
  return { tools, calls };
}

function properties(tool: { parameters: unknown }): Record<string, unknown> {
  return (tool.parameters as { properties?: Record<string, unknown> }).properties ?? {};
}

const NAMES = ["read", "aft_outline", "aft_zoom"];

describe("Pi revision reads surface", () => {
  test("ref is offered only while revision_reads is on", () => {
    const off = registered({});
    const on = registered({ revision_reads: true });
    for (const name of NAMES) {
      const offTool = off.tools.get(name)!;
      const onTool = on.tools.get(name)!;
      expect(properties(offTool)).not.toHaveProperty("ref");
      expect(offTool.description).not.toContain(REVISION_REF_TOOL_NOTE);
      expect(properties(onTool)).toHaveProperty("ref");
      expect(onTool.description).toContain(REVISION_REF_TOOL_NOTE);
      const required = (onTool.parameters as { required?: string[] }).required ?? [];
      expect(required).not.toContain("ref");
    }
  });

  test("read, outline and zoom forward ref; a blank ref is no revision", async () => {
    const { tools, calls } = registered({ revision_reads: true });
    const context = makeExtContext(process.cwd(), "revision-ref");
    await executeTool(tools.get("read")!, { path: "package.json", ref: "v1" }, context);
    await executeTool(tools.get("read")!, { path: "package.json", ref: "" }, context);
    await executeTool(tools.get("aft_outline")!, { target: "package.json", ref: "main" }, context);
    await executeTool(
      tools.get("aft_zoom")!,
      { path: "package.json", symbols: "name", ref: "HEAD~1" },
      context,
    );
    const refs = calls.map((call) => (call.params.arguments as Record<string, unknown>).ref);
    expect(refs).toEqual(["v1", undefined, "main", "HEAD~1"]);
  });

  test("zoom keeps a flat batch schema with one gated top-level revision", () => {
    for (const enabled of [false, true]) {
      const zoom = registered({ revision_reads: enabled }).tools.get("aft_zoom")!;
      const schema = zoom.parameters as {
        properties: Record<string, { items?: { properties?: Record<string, unknown> } }>;
        required?: string[];
      };
      expect(Object.keys(schema.properties).sort()).toEqual(
        enabled
          ? ["batch", "callgraph", "contextLines", "ref"]
          : ["batch", "callgraph", "contextLines"],
      );
      expect(schema).not.toHaveProperty("anyOf");
      expect(schema).not.toHaveProperty("oneOf");
      expect(schema.required).toEqual(["batch"]);
      expect(Object.keys(schema.properties.batch.items!.properties!).sort()).toEqual([
        "symbols",
        "target",
      ]);
    }
  });

  test("zoom refuses a URL with a revision before dispatch", async () => {
    const { tools, calls } = registered({ revision_reads: true });
    await expect(
      executeTool(
        tools.get("aft_zoom")!,
        {
          batch: [{ target: "https://example.invalid/docs", symbols: "Overview" }],
          ref: "HEAD",
        },
        makeExtContext(process.cwd(), "revision-url"),
      ),
    ).rejects.toThrow("a URL has no revision");
    expect(calls).toHaveLength(0);
  });

  test("a ref sent while revision_reads is off still reaches the engine to be refused", async () => {
    const { tools, calls } = registered({});
    await executeTool(
      tools.get("read")!,
      { path: "package.json", ref: "v1" },
      makeExtContext(process.cwd(), "revision-ref-off"),
    );
    expect((calls[0].params.arguments as Record<string, unknown>).ref).toBe("v1");
  });
});
