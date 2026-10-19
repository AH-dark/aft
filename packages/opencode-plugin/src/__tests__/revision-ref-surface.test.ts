/// <reference path="../bun-test.d.ts" />
/**
 * The `ref` (git revision) argument of read, aft_outline and aft_zoom: offered
 * only while `revision_reads` is on, and forwarded to the engine whenever a
 * caller sends one so a stale `ref` is refused by name.
 */

import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { type BridgePool, REVISION_REF_TOOL_NOTE } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { tool } from "@opencode-ai/plugin";
import { buildSubcToolSchemas } from "../subc-tool-schemas.js";
import { projectV2Tool } from "../tools/definitions/v2.js";
import { createReadTool } from "../tools/hoisted.js";
import { readingTools } from "../tools/reading.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers";

type ToolCallCall = { name: string; rawArgs: Record<string, unknown> };

const tempRoots: string[] = [];

afterEach(async () => {
  await Promise.all(tempRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

async function project(): Promise<string> {
  const root = await realpath(await mkdtemp(join(process.cwd(), ".aft-revision-ref-")));
  tempRoots.push(root);
  await mkdir(join(root, "src"));
  await writeFile(join(root, "src", "lib.ts"), "export const value = 1;\n");
  return root;
}

function harness(config: Record<string, unknown>) {
  const calls: ToolCallCall[] = [];
  const bridge = {
    send: async () => ({ success: true }),
    toolCall: async (_session: string | undefined, name: string, rawArgs = {}) => {
      calls.push({ name, rawArgs });
      return { success: true, text: `${name} reply` };
    },
  };
  const ctx = {
    pool: { getBridge: () => bridge } as unknown as BridgePool,
    client: { lsp: { status: async () => ({ data: [] }) }, find: {} },
    config,
    storageDir: "/tmp/aft-revision-ref-test",
  } as unknown as PluginContext;
  return { ctx, calls };
}

function sdkContext(directory: string): ToolContext {
  return {
    sessionID: "revision-ref",
    messageID: "message",
    agent: "test",
    directory,
    worktree: directory,
    abort: new AbortController().signal,
    metadata: () => {},
    ask: noopAsk,
  };
}

function argNames(def: { args: Record<string, unknown> }): string[] {
  return Object.keys(def.args);
}

describe("revision reads surface", () => {
  test("ref is offered on read, outline and zoom only while revision_reads is on", () => {
    const off = harness({});
    const on = harness({ revision_reads: true });
    const offTools = [createReadTool(off.ctx), ...Object.values(readingTools(off.ctx))];
    const onTools = [createReadTool(on.ctx), ...Object.values(readingTools(on.ctx))];
    for (const def of offTools) {
      expect(argNames(def)).not.toContain("ref");
      expect(def.description).not.toContain(REVISION_REF_TOOL_NOTE);
    }
    for (const def of onTools) {
      expect(argNames(def)).toContain("ref");
      expect(def.description).toContain(REVISION_REF_TOOL_NOTE);
      const schema = tool.schema.toJSONSchema(tool.schema.object(def.args), { io: "input" }) as {
        properties: Record<string, { type?: string }>;
        required?: string[];
      };
      expect(schema.properties.ref?.type).toBe("string");
      expect(schema.required ?? []).not.toContain("ref");
    }
    // The read examples stay last.
    const read = createReadTool(on.ctx).description;
    expect(read.indexOf(REVISION_REF_TOOL_NOTE)).toBeLessThan(read.indexOf("Examples:"));
  });

  test("OpenCode 2 projects the same gated argument", () => {
    for (const [config, offered] of [
      [{}, false],
      [{ revision_reads: true }, true],
    ] as const) {
      const { ctx } = harness({ ...config });
      const defs = { read: createReadTool(ctx), ...readingTools(ctx) };
      for (const [name, def] of Object.entries(defs)) {
        const projected = projectV2Tool(name, def, { directory: process.cwd() });
        const schema = tool.schema.toJSONSchema(projected.input, { io: "input" }) as {
          properties: Record<string, unknown>;
        };
        expect(Object.hasOwn(schema.properties, "ref")).toBe(offered);
      }
    }
  });

  test("the offer follows the live config", () => {
    const { ctx } = harness({});
    const read = createReadTool(ctx);
    expect(argNames(read)).not.toContain("ref");
    (ctx.config as Record<string, unknown>).revision_reads = true;
    expect(argNames(read)).toContain("ref");
  });

  test("the module catalog artifact advertises ref only in the enabled override", () => {
    const base = buildSubcToolSchemas();
    const enabled = buildSubcToolSchemas(false, true);
    for (const [name, schema] of Object.entries(base)) {
      const properties = schema.properties as Record<string, unknown>;
      expect(properties).not.toHaveProperty("ref");
      const withRef = enabled[name as keyof typeof enabled].properties as Record<string, unknown>;
      expect(Object.hasOwn(withRef, "ref")).toBe(["read", "outline", "zoom"].includes(name));
    }
  });

  test("read, outline and zoom forward ref; a blank ref is no revision", async () => {
    const root = await project();
    const { ctx, calls } = harness({ revision_reads: true });
    const context = sdkContext(root);
    const read = createReadTool(ctx);
    const { aft_outline: outline, aft_zoom: zoom } = readingTools(ctx);

    await read.execute({ filePath: "src/lib.ts", ref: "v1", startLine: 2 }, context);
    await read.execute({ filePath: "src/lib.ts", ref: "  " }, context);
    await outline.execute({ target: "src/lib.ts", ref: "main" }, context);
    await zoom.execute({ path: "src/lib.ts", symbols: "value", ref: "HEAD~1" }, context);
    await zoom.execute(
      { targets: [{ path: "src/lib.ts", symbol: "value" }], ref: "feature" },
      context,
    );

    expect(calls.map((call) => [call.name, call.rawArgs.ref])).toEqual([
      ["read", "v1"],
      ["read", undefined],
      ["outline", "main"],
      ["zoom", "HEAD~1"],
      ["zoom", "feature"],
    ]);
    expect(calls[0].rawArgs.startLine).toBe(2);
    expect(Object.hasOwn(calls[1].rawArgs, "ref")).toBe(false);
  });

  test("a ref sent while revision_reads is off still reaches the engine to be refused", async () => {
    const root = await project();
    const { ctx, calls } = harness({});
    await createReadTool(ctx).execute({ filePath: "src/lib.ts", ref: "v1" }, sdkContext(root));
    expect(calls).toEqual([{ name: "read", rawArgs: { filePath: "src/lib.ts", ref: "v1" } }]);
  });
});
