/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { linkCachedExecutable } from "../../../aft-bridge/src/__tests__/test-utils/cached-executable.js";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { registerShutdownCleanup } from "../shutdown-hooks.js";

type OpenCodePlugin = typeof import("../index.js").default;

// Match PLUGIN_VERSION from package.json so the fake binary the resolver finds
// satisfies the version-match expectation in findBinary. Without this, every
// release that bumps the plugin version breaks the test because the resolver
// rejects the cached fake at the old version path.
const PLUGIN_VERSION: string = (() => {
  try {
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    return (require("../../package.json") as { version: string }).version;
  } catch {
    return "0.0.0";
  }
})();

describe.serial("OpenCode migration bootstrap", () => {
  let tempDir: string;
  let releaseEnv: (() => void) | undefined;
  let argsLog: string;
  let aftPath: string;
  let cachedAft: string;

  function writeFakeAft(exitCode: number): void {
    const contents = `#!/bin/sh\nif [ "$1" = "--version" ]; then echo "aft ${PLUGIN_VERSION}"; exit 0; fi\nprintf "%s\\n" "$@" >> "$AFT_MIGRATION_ARGS_LOG"\nexit ${exitCode}\n`;
    rmSync(aftPath, { force: true });
    rmSync(cachedAft, { force: true });
    linkCachedExecutable(aftPath, contents);
    linkCachedExecutable(cachedAft, contents);
  }

  beforeEach(async () => {
    tempDir = mkdtempSync(join(tmpdir(), "aft-opencode-migration-bootstrap-"));

    const binDir = join(tempDir, "bin");
    mkdirSync(binDir, { recursive: true });
    argsLog = join(tempDir, "args.log");
    aftPath = join(binDir, "aft");

    const xdgCacheHome = join(tempDir, "cache");
    const opencodeConfigDir = join(tempDir, "opencode-config");
    releaseEnv = await acquireEnv({
      // CI exports an ambient AFT_CACHE_DIR; it outranks XDG_CACHE_HOME in the
      // shared cache resolver, so clear it for the sandbox to apply.
      AFT_CACHE_DIR: undefined,
      AFT_STORAGE_DIR: undefined,
      // CI also exports the built binary here, and since it became resolution
      // step 0 it would win over the fake `aft` this test plants on PATH.
      AFT_BINARY_PATH: undefined,
      PATH: `${binDir}:${process.env.PATH ?? ""}`,
      HOME: join(tempDir, "home"),
      XDG_DATA_HOME: join(tempDir, "data"),
      XDG_CONFIG_HOME: join(tempDir, "config"),
      XDG_STATE_HOME: join(tempDir, "state"),
      XDG_CACHE_HOME: xdgCacheHome,
      OPENCODE_CONFIG_DIR: opencodeConfigDir,
      AFT_MIGRATION_ARGS_LOG: argsLog,
    });

    cachedAft = join(xdgCacheHome, "aft", "bin", `v${PLUGIN_VERSION}`, "aft");
    mkdirSync(join(xdgCacheHome, "aft", "bin", `v${PLUGIN_VERSION}`), { recursive: true });
    writeFakeAft(0);

    mkdirSync(opencodeConfigDir, { recursive: true });
    writeFileSync(
      join(opencodeConfigDir, "aft.json"),
      JSON.stringify({ lsp: { auto_install: false }, indexes: { semantic: false } }),
      "utf8",
    );
  });

  afterEach(() => {
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(tempDir, { recursive: true, force: true });
  });

  function createLegacyRoot(): string {
    const legacyRoot = join(
      process.env.XDG_DATA_HOME as string,
      "opencode",
      "storage",
      "plugin",
      "aft",
    );
    mkdirSync(legacyRoot, { recursive: true });
    writeFileSync(join(legacyRoot, "warned_tools.json"), "{}", "utf8");
    return legacyRoot;
  }

  async function loadPlugin(): Promise<OpenCodePlugin> {
    const mod = await import(`../index.js?migration-bootstrap-${Date.now()}-${Math.random()}`);
    return mod.default;
  }

  test("opencode_plugin_calls_ensureStorageMigrated_with_opencode_harness", async () => {
    const legacyRoot = createLegacyRoot();
    const plugin = await loadPlugin();
    const hooks = (await plugin({
      directory: tempDir,
      client: {},
    } as Parameters<OpenCodePlugin>[0])) as {
      dispose?: () => Promise<void>;
    };

    const argv = readFileSync(argsLog, "utf8").trim().split("\n");
    expect(argv).toContain("migrate-storage");
    expect(argv).toContain("--harness");
    expect(argv).toContain("opencode");
    expect(argv).toContain("--from");
    expect(argv).toContain(legacyRoot);
    expect(argv).toContain("--to");
    expect(argv).toContain(join(process.env.XDG_DATA_HOME as string, "cortexkit", "aft"));

    await hooks.dispose?.();
  });

  test("opencode_plugin_dispose_does_not_run_unrelated_shutdown_cleanups", async () => {
    const plugin = await loadPlugin();
    const hooks = (await plugin({
      directory: tempDir,
      client: {},
    } as Parameters<OpenCodePlugin>[0])) as {
      dispose?: () => Promise<void>;
    };
    let unrelatedCleanupRan = false;
    const unrelated = registerShutdownCleanup(() => {
      unrelatedCleanupRan = true;
    });

    expect(typeof hooks.dispose).toBe("function");
    await hooks.dispose?.();

    expect(unrelatedCleanupRan).toBe(false);
    await unrelated.dispose("test teardown");
  });

  test("opencode_plugin_aborts_on_migration_error", async () => {
    createLegacyRoot();
    writeFakeAft(5);
    const plugin = await loadPlugin();

    await expect(
      plugin({ directory: tempDir, client: {} } as Parameters<OpenCodePlugin>[0]),
    ).rejects.toThrow(/AFT storage migration failed.*exit 5/);
  });

  for (const entry of ["dist/index.js", "dist/entry/server.js"]) {
    test(`bundled ${entry} migrates legacy storage in subc mode without a supplied binary`, async () => {
      const legacyRoot = createLegacyRoot();
      const connectionFile = join(tempDir, "subc.json");
      writeFileSync(connectionFile, "{}");
      writeFileSync(
        join(process.env.OPENCODE_CONFIG_DIR as string, "aft.json"),
        JSON.stringify({
          subc: { connection_file: connectionFile },
          lsp: { auto_install: false },
          indexes: { semantic: false },
        }),
      );
      const packageRoot = join(tempDir, "bundled-plugin");
      mkdirSync(packageRoot);
      writeFileSync(
        join(packageRoot, "package.json"),
        readFileSync(resolve(import.meta.dir, "../../package.json")),
      );
      symlinkSync(
        resolve(import.meta.dir, "../../node_modules"),
        join(packageRoot, "node_modules"),
        "junction",
      );
      const build = await Bun.build({
        entrypoints: [
          resolve(import.meta.dir, "../index.ts"),
          resolve(import.meta.dir, "../entry/server.mjs"),
        ],
        outdir: join(packageRoot, "dist"),
        target: "node",
        format: "esm",
        external: ["effect"],
      });
      expect(build.success).toBe(true);
      const runner = `
        const bundled = await import(${JSON.stringify(pathToFileURL(join(packageRoot, entry)).href)});
        const plugin = ${entry.endsWith("server.js") ? "bundled.default.server" : "bundled.default"};
        const hooks = await plugin({ directory: ${JSON.stringify(tempDir)}, client: {} });
        await hooks.dispose?.();
        process.exit(0);
      `;
      const child = spawnSync(process.execPath, ["--eval", runner], {
        encoding: "utf8",
        env: { ...process.env },
        timeout: 15_000,
      });
      expect(child.error).toBeUndefined();
      expect({ status: child.status, stderr: child.stderr }).toEqual({
        status: 0,
        stderr: expect.any(String),
      });
      const argv = readFileSync(argsLog, "utf8").trim().split("\n");
      expect(argv).toContain("migrate-storage");
      expect(argv).toContain(legacyRoot);
      expect(argv).toContain("opencode");
    });
  }
});
