import { expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { registerHoistedTools } from "../tools/hoisted.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

test("A14 Pi mutations make one apply and retain final full-text UI", async () => {
  const root = realpathSync(mkdtempSync(join(tmpdir(), "aft-a14-pi-")));
  mkdirSync(join(root, "src"));
  const before = "const value=1;\r\n";
  const after = "const value = 2;\r\n";
  const wire: string[] = [];
  const { api, tools } = makeMockApi();
  const { bridge, calls } = makeMockBridge(() => {
    const response = {
      success: true,
      text: "applied",
      diff: { before, after, additions: 1, deletions: 1 },
    };
    const serialized = JSON.stringify(response);
    wire.push(serialized);
    return JSON.parse(serialized);
  });
  registerHoistedTools(api, makePluginContext(bridge), {
    hoistRead: false,
    hoistWrite: true,
    hoistEdit: true,
    hoistGrep: false,
    restrictToProjectRoot: true,
  });
  try {
    for (const [name, args] of [
      ["write", { path: "src/value.ts", content: after }],
      ["edit", { path: "src/value.ts", oldString: "1", newString: "2" }],
      ["edit", { path: "src/value.ts", appendContent: "after" }],
      ["edit", { path: "src/value.ts", symbol: "value", content: after }],
      ["edit", { path: "src/value.ts", edits: [{ startLine: 1, endLine: 1, content: after }] }],
    ] as const) {
      calls.length = 0;
      const result = (await executeTool(
        tools.get(name)!,
        args,
        makeExtContext(root, "a14-pi"),
      )) as {
        details: { diff: string; firstChangedLine: number };
      };
      expect(calls.length).toBe(1);
      expect(calls[0].params.preview).toBeUndefined();
      expect(calls[0].params.session_id).toBe("a14-pi");
      expect(JSON.parse(wire.at(-1)!).diff).toEqual({ before, after, additions: 1, deletions: 1 });
      expect(result.details.firstChangedLine).toBe(1);
      expect(result.details.diff).toMatch(/^-\s*1 const value=1;$/m);
      expect(result.details.diff).toMatch(/^\+\s*1 const value = 2;$/m);
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
