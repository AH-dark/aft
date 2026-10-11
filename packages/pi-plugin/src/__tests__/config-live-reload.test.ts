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
import { registerPiToolSurface, resolvePiToolSurface } from "../tool-registration.js";
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
  });
  cleanup.push(() => reload.stop());
  return { root, projectDir, projectPath, userPath, ctx, reload, notices, calls };
}

describe.serial("Pi live config reload", () => {
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

  test("deleting the project file removes its live settings", async () => {
    const f = await fixture(
      '{ "bash": { "watch_sync_max_ms": 120000 } }',
      '{ "bash": { "watch_sync_max_ms": 1000 } }',
    );
    expect(resolveBashConfig(f.ctx.config).watch_sync_max_ms).toBe(1000);
    unlinkSync(f.projectPath);

    f.reload.reload();

    expect(resolveBashConfig(f.ctx.config).watch_sync_max_ms).toBe(120000);
    expect(f.notices).toEqual([]);
  });

  test("an invalid edit keeps the last valid config", async () => {
    const f = await fixture('{ "restrict_to_project_root": true }');
    writeFileSync(f.userPath, '{ "restrict_to_project_root": "yes" }');
    f.reload.reload();
    expect(f.ctx.config.restrict_to_project_root).toBe(true);
    expect(f.notices).toHaveLength(1);
  });
});
