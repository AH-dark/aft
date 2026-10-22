/// <reference path="../bun-test.d.ts" />

/**
 * Live config reload in the Pi extension: an edit to either config file
 * reaches `ctx.config`, and the path restriction the hoisted tools enforce
 * follows it even though the tools captured their surface at registration.
 */

import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import {
  getConfigLoadSources,
  getConfigLoadTexts,
  loadAftConfig,
  resolveBashConfig,
} from "../config.js";
import { startPiLiveConfigReload } from "../config-live-reload.js";
import {
  refreshPiRevisionReadTools,
  registerPiToolSurface,
  resolvePiToolSurface,
} from "../tool-registration.js";
import {
  executeTool,
  makeExtContext,
  makeMockApi,
  makeMockBridge,
  makePluginContext,
} from "./tool-test-utils.js";

const cleanup: (() => void)[] = [];

afterEach(() => {
  for (const fn of cleanup.splice(0).reverse()) fn();
});

async function fixture(user: string, project?: string) {
  const root = mkdtempSync(join(tmpdir(), "aft-pi-live-config-"));
  const xdg = join(root, "xdg");
  const projectDir = join(root, "project");
  mkdirSync(join(xdg, "cortexkit"), { recursive: true });
  mkdirSync(projectDir, { recursive: true });
  mkdirSync(join(projectDir, ".cortexkit"), { recursive: true });
  const userPath = join(xdg, "cortexkit", "aft.jsonc");
  const projectPath = join(projectDir, ".cortexkit", "aft.jsonc");
  writeFileSync(userPath, user);
  if (project !== undefined) writeFileSync(projectPath, project);
  const releaseEnv = await acquireEnv({ HOME: join(root, "home"), XDG_CONFIG_HOME: xdg });
  cleanup.push(() => rmSync(root, { recursive: true, force: true }));
  cleanup.push(releaseEnv);
  const { bridge, calls } = makeMockBridge(() => ({ success: true, text: "ok" }));
  const ctx = makePluginContext(bridge, { config: loadAftConfig(projectDir) });
  const { api, tools } = makeMockApi();
  const surface = resolvePiToolSurface(ctx.config);
  registerPiToolSurface(api, ctx, surface);
  const notices: string[] = [];
  const reload = startPiLiveConfigReload({
    directory: projectDir,
    initialSources: [...getConfigLoadSources()],
    initialSourceTexts: Object.fromEntries(getConfigLoadTexts()),
    getConfig: () => ctx.config,
    setConfig: (next) => {
      ctx.config = next;
    },
    notify: (message) => notices.push(message),
    watch: false,
    refreshRevisionReadTools: () => refreshPiRevisionReadTools(api, ctx, surface),
  });
  cleanup.push(() => reload.stop());
  return { root, projectDir, projectPath, userPath, ctx, reload, notices, calls, tools };
}

describe.serial("Pi live config reload", () => {
  test("malformed hardening reload preserves the whole last-good candidate", async () => {
    const f = await fixture(
      '{"sandbox":{"profile":{"worker":"strict"},"read_deny":["private"]},"bash":{"watch_sync_max_ms":120000}}',
    );
    writeFileSync(
      f.userPath,
      '{"sandbox":{"profile":{"worker":5}},"bash":{"watch_sync_max_ms":1000}}',
    );
    f.reload.reload();
    expect(f.ctx.config.sandbox?.profile?.worker).toBe("strict");
    expect(f.ctx.config.sandbox?.read_deny).toEqual(["private"]);
    expect(resolveBashConfig(f.ctx.config).watch_sync_max_ms).toBe(120000);
    expect(f.notices[0]).toContain("invalid_security_config:sandbox");
  });
  test("refreshed read schemas retain later live path restrictions", async () => {
    const f = await fixture('{"revision_reads":false,"restrict_to_project_root":false}');
    writeFileSync(f.userPath, '{"revision_reads":true,"restrict_to_project_root":false}');
    f.reload.reload();
    const refreshed = f.tools.get("read")!;
    writeFileSync(f.userPath, '{"revision_reads":true,"restrict_to_project_root":true}');
    f.reload.reload();
    await expect(
      executeTool(refreshed, { path: join(f.root, "outside.txt") }, makeExtContext(f.projectDir)),
    ).rejects.toThrow("restrict_to_project_root");
  });
  test("revision_reads replaces captured host schemas in both directions", async () => {
    const f = await fixture('{"revision_reads":false}');
    for (const enabled of [true, false]) {
      const previous = f.tools.get("read");
      writeFileSync(f.userPath, JSON.stringify({ revision_reads: enabled }));
      expect(f.reload.reload()?.applied).toContain("revision_reads");
      expect(f.ctx.config.revision_reads).toBe(enabled);
      expect(f.tools.get("read")).not.toBe(previous);
      for (const name of ["read", "aft_outline", "aft_zoom"]) {
        const definition = f.tools.get(name)!;
        const schema = definition.parameters as { properties: Record<string, unknown> };
        expect(Object.hasOwn(schema.properties, "ref")).toBe(enabled);
        expect(definition.description.includes("git revision")).toBe(enabled);
      }
    }
  });
  test("a restrict_to_project_root edit reaches the registered hoisted tools", async () => {
    const f = await fixture('{ "restrict_to_project_root": false }');
    const { api, tools } = makeMockApi();
    registerPiToolSurface(api, f.ctx, resolvePiToolSurface(f.ctx.config));
    const outside = join(f.root, "elsewhere", "file.txt");
    const write = () =>
      executeTool(
        tools.get("write")!,
        { path: outside, content: "x" },
        makeExtContext(f.projectDir),
      );

    await expect(write()).resolves.toBeDefined();

    writeFileSync(f.userPath, '{ "restrict_to_project_root": true }');
    expect(f.reload.reload()?.applied).toEqual(["restrict_to_project_root"]);

    await expect(write()).rejects.toThrow("restrict_to_project_root");
  });

  test("a bash wait setting applies live and a restart-only one does not", async () => {
    const f = await fixture('{ "bash": { "foreground_wait_window_ms": 20000, "compress": true } }');
    writeFileSync(
      f.userPath,
      '{ "bash": { "foreground_wait_window_ms": 30000, "compress": false } }',
    );
    const result = f.reload.reload();
    expect(result?.applied).toEqual(["bash.foreground_wait_window_ms"]);
    const bash = resolveBashConfig(f.ctx.config);
    expect(bash.foreground_wait_window_ms).toBe(30_000);
    expect(bash.compress).toBe(true);
  });

  test("deleting a project file applies ordinary settings and holds security loosening", async () => {
    const project = '{ "bash": { "host_fallback": false, "watch_sync_max_ms": 1000 } }';
    const f = await fixture(
      '{ "bash": { "host_fallback": true, "watch_sync_max_ms": 120000 } }',
      project,
    );
    expect(resolveBashConfig(f.ctx.config).host_fallback).toBe(false);
    expect(resolveBashConfig(f.ctx.config).watch_sync_max_ms).toBe(1000);
    unlinkSync(f.projectPath);

    const deleted = f.reload.reload();

    expect(deleted?.held).toEqual(["bash.host_fallback"]);
    expect(resolveBashConfig(f.ctx.config).host_fallback).toBe(false);
    expect(resolveBashConfig(f.ctx.config).watch_sync_max_ms).toBe(120000);
    expect(f.notices).toEqual([]);

    writeFileSync(f.projectPath, project);
    const restored = f.reload.reload();
    expect(restored?.held).toEqual([]);
    expect(resolveBashConfig(f.ctx.config).host_fallback).toBe(false);
    expect(resolveBashConfig(f.ctx.config).watch_sync_max_ms).toBe(1000);
  });

  test("an invalid edit keeps the last valid config", async () => {
    const f = await fixture('{ "restrict_to_project_root": true }');
    writeFileSync(f.userPath, '{ "restrict_to_project_root": "yes" }');
    f.reload.reload();
    expect(f.ctx.config.restrict_to_project_root).toBe(true);
    expect(f.notices).toHaveLength(1);
  });
});
