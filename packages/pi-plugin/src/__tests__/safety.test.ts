/**
 * Unit tests for aft_safety argument shaping.
 */

/// <reference path="../bun-test.d.ts" />

import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import { registerSafetyTool } from "../tools/safety.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

describe("aft_safety adapter", () => {
  test("checkpoint forwards agent-facing filePath so server translation promotes it", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
    registerSafetyTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_safety")!, {
      op: "checkpoint",
      name: "before-edit",
      filePath: "src/app.ts",
    });

    expect(calls[0]).toMatchObject({
      command: "tool_call",
      params: {
        name: "safety",
        arguments: { op: "checkpoint", name: "before-edit", filePath: "src/app.ts" },
      },
    });
    expect(calls[0].params.arguments).not.toHaveProperty("file");
  });

  test("history requires path before bridge dispatch", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerSafetyTool(api, makePluginContext(bridge));

    await expect(executeTool(tools.get("aft_safety")!, { op: "history" })).rejects.toThrow(
      "requires 'path'",
    );
    expect(calls).toHaveLength(0);
  });

  test("undo without filePath previews then calls tool_call with operation args", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((command) =>
      command === "undo_preview"
        ? { success: true, paths: [] }
        : { success: true, operation: true, text: "restored operation" },
    );
    registerSafetyTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_safety")!, { op: "undo" });

    expect(calls.map((call) => call.command)).toEqual(["undo_preview", "tool_call"]);
    // The mock context is headless (no UI), which Pi treats as a worker
    // session, so every request also carries the caller's role.
    expect(calls[0].params).toEqual({ worker_session: true });
    expect(calls[1].params).toMatchObject({
      name: "safety",
      arguments: { op: "undo" },
    });
  });

  test("undo with filePath previews with file and forwards agent-facing filePath", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((command) =>
      command === "undo_preview"
        ? { success: true, paths: ["src/app.ts"] }
        : { success: true, backup_id: "b1", text: "restored src/app.ts" },
    );
    registerSafetyTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_safety")!, { op: "undo", filePath: "src/app.ts" });

    expect(calls[0].command).toBe("undo_preview");
    expect(calls[0].params).toMatchObject({ file: "src/app.ts" });
    expect(calls[1]).toMatchObject({
      command: "tool_call",
      params: {
        name: "safety",
        arguments: { op: "undo", filePath: "src/app.ts" },
      },
    });
  });

  test("restore previews checkpoint paths then forwards restore args through tool_call", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((command) =>
      command === "checkpoint_paths"
        ? { success: true, paths: ["src/a.ts"] }
        : { success: true, text: "checkpoint restored" },
    );
    registerSafetyTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_safety")!, {
      op: "restore",
      name: "before-edit",
      files: ["src/a.ts", "src/b.ts"],
    });

    expect(calls[0].command).toBe("checkpoint_paths");
    // The preview carries the same scope as the restore, so only the files
    // being restored are checked and prompted for.
    expect(calls[0].params).toMatchObject({
      name: "before-edit",
      files: ["src/a.ts", "src/b.ts"],
    });
    expect(calls[1]).toMatchObject({
      command: "tool_call",
      params: {
        name: "safety",
        arguments: {
          op: "restore",
          name: "before-edit",
          files: ["src/a.ts", "src/b.ts"],
        },
      },
    });
  });

  for (const restrictToProjectRoot of [true, false]) {
    test(`restore checks every preview path with restriction ${restrictToProjectRoot}`, async () => {
      const { api, tools } = makeMockApi();
      const externalFile = resolve(process.cwd(), "../outside/secrets/x");
      const { bridge, calls } = makeMockBridge((command) =>
        command === "checkpoint_paths"
          ? { success: true, paths: [resolve("public.txt"), externalFile] }
          : { success: true, text: "checkpoint restored" },
      );
      registerSafetyTool(
        api,
        makePluginContext(bridge, { config: { restrict_to_project_root: restrictToProjectRoot } }),
      );
      const restore = executeTool(
        tools.get("aft_safety")!,
        { op: "restore", name: "mixed" },
        makeExtContext(),
      );
      if (restrictToProjectRoot) {
        await expect(restore).rejects.toThrow(`'${externalFile}' is outside the project root`);
        expect(calls.map((call) => call.command)).toEqual(["checkpoint_paths"]);
      } else {
        await restore;
        expect(calls.map((call) => call.command)).toEqual(["checkpoint_paths", "tool_call"]);
      }
      expect(calls[0].params).toMatchObject({ name: "mixed" });
    });
  }

  test("restore permits all in-project preview paths without UI even under restriction", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge((command) =>
      command === "checkpoint_paths"
        ? { success: true, paths: [resolve("public.txt"), resolve("secrets/x")] }
        : { success: true, text: "checkpoint restored" },
    );
    registerSafetyTool(
      api,
      makePluginContext(bridge, { config: { restrict_to_project_root: true } }),
    );
    await executeTool(
      tools.get("aft_safety")!,
      { op: "restore", name: "internal" },
      makeExtContext(),
    );
    expect(calls.map((call) => call.command)).toEqual(["checkpoint_paths", "tool_call"]);
  });

  test("purge requires path before bridge dispatch", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge();
    registerSafetyTool(api, makePluginContext(bridge));

    await expect(executeTool(tools.get("aft_safety")!, { op: "purge" })).rejects.toThrow(
      "requires 'path'",
    );
    expect(calls).toHaveLength(0);
  });

  test("purge forwards the path through tool_call", async () => {
    const { api, tools } = makeMockApi();
    const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "purged" }));
    registerSafetyTool(api, makePluginContext(bridge));

    await executeTool(tools.get("aft_safety")!, { op: "purge", filePath: "secrets" });

    expect(calls.map((call) => call.command)).toEqual(["safety_purge_paths", "tool_call"]);
    expect(calls[0].params).toMatchObject({ file: "secrets" });
    expect(calls[1]).toMatchObject({
      command: "tool_call",
      params: { name: "safety", arguments: { op: "purge", filePath: "secrets" } },
    });
  });

  // A deleted directory is no longer on disk, so only the engine's list of
  // the saved paths it would remove shows what the purge reaches.
  for (const restrictToProjectRoot of [true, false]) {
    test(`purge checks every saved path the engine lists with restriction ${restrictToProjectRoot}`, async () => {
      const { api, tools } = makeMockApi();
      const externalFile = resolve(process.cwd(), "../outside/secrets/x");
      const { bridge, calls } = makeMockBridge((command) =>
        command === "safety_purge_paths"
          ? { success: true, paths: [resolve("deleted/x"), externalFile] }
          : { success: true, text: "purged" },
      );
      registerSafetyTool(
        api,
        makePluginContext(bridge, { config: { restrict_to_project_root: restrictToProjectRoot } }),
      );
      const purge = executeTool(
        tools.get("aft_safety")!,
        { op: "purge", filePath: "deleted" },
        makeExtContext(),
      );
      if (restrictToProjectRoot) {
        await expect(purge).rejects.toThrow(`'${externalFile}' is outside the project root`);
        expect(calls.map((call) => call.command)).toEqual(["safety_purge_paths"]);
      } else {
        await purge;
        expect(calls.map((call) => call.command)).toEqual(["safety_purge_paths", "tool_call"]);
      }
    });
  }

  for (const restrictToProjectRoot of [true, false]) {
    test(`purge outside the project root with restriction ${restrictToProjectRoot}`, async () => {
      const { api, tools } = makeMockApi();
      const externalDir = resolve(process.cwd(), "../outside/secrets");
      const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "purged" }));
      registerSafetyTool(
        api,
        makePluginContext(bridge, { config: { restrict_to_project_root: restrictToProjectRoot } }),
      );
      const purge = executeTool(
        tools.get("aft_safety")!,
        { op: "purge", filePath: externalDir },
        makeExtContext(),
      );
      if (restrictToProjectRoot) {
        await expect(purge).rejects.toThrow(`'${externalDir}' is outside the project root`);
        expect(calls).toHaveLength(0);
      } else {
        await purge;
        expect(calls.map((call) => call.command)).toEqual(["safety_purge_paths", "tool_call"]);
      }
    });
  }
});
