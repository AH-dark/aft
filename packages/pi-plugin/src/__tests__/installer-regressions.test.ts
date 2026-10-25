import { describe, expect, test } from "bun:test";
import { type ChildProcess, spawnSync } from "node:child_process";
import { EventEmitter } from "node:events";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { PassThrough } from "node:stream";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { _runInstallForTesting as runInstall } from "../lsp-auto-install.js";
import { NPM_LSP_TABLE } from "../lsp-npm-table.js";

/** Run `fn` with a private AFT cache and a fake `npm` first on PATH. */
async function cacheFixture(fn: (root: string) => Promise<void> | void): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), "aft-install-regression-"));
  const bin = join(root, "bin");
  mkdirSync(bin);
  // Only resolveNpm() sees this script; spawning is replaced where a test
  // needs npm to stall.
  writeFileSync(join(bin, "npm"), "#!/bin/sh\nexit 0\n", { mode: 0o755 });
  try {
    await withEnv(
      { AFT_CACHE_DIR: join(root, "cache"), PATH: `${bin}${delimiter}${process.env.PATH ?? ""}` },
      () => fn(root),
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

function settlesWithin<T>(promise: Promise<T>, ms: number): Promise<boolean> {
  return Promise.race([
    promise.then(() => true),
    new Promise<boolean>((resolve) => setTimeout(() => resolve(false), ms)),
  ]);
}

describe("shutdown hooks", () => {
  test("beforeExit does not suppress later signal cleanup", () => {
    const module = join(import.meta.dir, "..", "shutdown-hooks.ts");
    // beforeExit fires with no cleanups registered; a cleanup registered
    // afterwards must still run when SIGTERM arrives.
    const result = spawnSync(
      process.execPath,
      [
        "-e",
        `
      const { registerShutdownCleanup } = await import(${JSON.stringify(module)});
      registerShutdownCleanup(() => {})();
      process.emit('beforeExit', 0);
      await Promise.resolve();
      registerShutdownCleanup(() => process.stdout.write('late-cleanup\\n'));
      process.kill(process.pid, 'SIGTERM');
      setInterval(() => {}, 1000);
    `,
      ],
      { encoding: "utf8", timeout: 10_000, env: { ...process.env, NODE_ENV: "test" } },
    );
    expect(result.stdout).toBe("late-cleanup\n");
    expect(result.status).toBe(143);
  });
});

describe("npm deadline", () => {
  test("stalled npm settles only after deadline termination is confirmed", async () => {
    await cacheFixture(async (root) => {
      const spec = NPM_LSP_TABLE[0];
      if (!spec) throw new Error("npm fixture missing");
      const controller = new AbortController();
      const retained: boolean[] = [];
      // A child that never exits: only the deadline can end this install.
      const child = Object.assign(new EventEmitter(), {
        pid: 123,
        exitCode: null,
        signalCode: null,
        stdout: new PassThrough(),
        stderr: new PassThrough(),
        unref: () => {},
      }) as unknown as ChildProcess;
      let confirmTermination = () => {};
      let onTerminating = () => {};
      const terminating = new Promise<void>((resolve) => {
        onTerminating = resolve;
      });
      const runtime = {
        spawn: (() => child) as unknown as typeof import("@cortexkit/aft-bridge").spawn,
        terminate: (() => {
          onTerminating();
          return new Promise<void>((resolve) => {
            confirmTermination = resolve;
          });
        }) as typeof import("@cortexkit/aft-bridge").terminateNpmProcessTree,
      };
      // Bounds the test if the deadline never fires.
      const rescue = setTimeout(() => controller.abort(), 2_000);
      try {
        let settled = false;
        const installing = runInstall(
          spec,
          "1.0.0",
          root,
          { retain: () => retained.push(true), release: () => {} },
          undefined,
          controller.signal,
          0,
          runtime,
        );
        void installing.then(() => {
          settled = true;
        });
        expect(await settlesWithin(terminating, 1_000)).toBe(true);
        expect(settled).toBe(false);
        confirmTermination();
        expect(await installing).toBe(false);
        expect(controller.signal.aborted).toBe(false);
        expect(retained).toEqual([]);
      } finally {
        confirmTermination();
        clearTimeout(rescue);
      }
    });
  });
});
