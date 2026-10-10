import { describe, expect, test } from "bun:test";
import { type ChildProcess, execFileSync, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { EventEmitter } from "node:events";
import {
  existsSync,
  linkSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { PassThrough } from "node:stream";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { _runInstallForTesting as runInstall } from "../lsp-auto-install.js";
import {
  acquireInstallLock,
  lspPackageDir,
  readVersionCheck,
  writeInstalledMetaIn,
  writeVersionCheck,
} from "../lsp-cache.js";
import {
  abortInFlightGithubInstalls,
  _fetchReleaseByTagForTesting as fetchReleaseByTag,
  _precheckTarForTesting as precheckArchive,
  _resolveTargetTagForTesting as resolveTargetTag,
  runGithubAutoInstall,
  _writeGithubInstalledMetaForTesting as writeGithubMeta,
} from "../lsp-github-install.js";
import { findGithubServerById } from "../lsp-github-table.js";
import { NPM_LSP_TABLE } from "../lsp-npm-table.js";

const config = { autoInstall: true, graceDays: 7, versions: {}, disabled: new Set<string>() };
const clangd = findGithubServerById("clangd");
if (!clangd) throw new Error("clangd fixture missing");

async function cacheFixture(fn: (root: string) => Promise<void> | void): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), "aft-install-regression-"));
  try {
    await withEnv({ AFT_CACHE_DIR: root }, () => fn(root));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

describe("installer regressions", () => {
  test("cache validation cannot quarantine a leased GitHub installation", async () => {
    await cacheFixture((root) => {
      const pkg = join(root, "lsp-binaries", "clangd");
      mkdirSync(join(pkg, "bin"), { recursive: true });
      writeFileSync(join(pkg, "bin", "clangd"), "partially published");
      writeFileSync(
        join(pkg, ".aft-installed"),
        JSON.stringify({ version: "1.0.0", binarySha256: "0".repeat(64) }),
      );
      const lease = acquireInstallLock(clangd.githubRepo);
      expect(lease).not.toBeNull();
      try {
        const result = runGithubAutoInstall(new Set(), { ...config, autoInstall: false });
        expect(result.cachedBinDirs).toEqual([]);
        expect(result.getCachedBinDirs()).toEqual([]);
        expect(existsSync(pkg)).toBe(true);
        expect(existsSync(join(root, "lsp-binaries", ".quarantine"))).toBe(false);
      } finally {
        lease?.release();
      }
    });
  });

  test("live release tags cannot persist unsafe path components", async () => {
    await cacheFixture(async () => {
      const fakeFetch = (async () =>
        new Response(
          JSON.stringify([
            {
              tag_name: "../../outside",
              published_at: "2020-01-01T00:00:00Z",
              assets: [],
              draft: false,
              prerelease: false,
            },
          ]),
        )) as typeof fetch;
      const result = await resolveTargetTag(clangd, config, fakeFetch);
      expect(result.tag).toBeNull();
      expect(readVersionCheck(clangd.githubRepo)).toBeNull();
    });
  });

  for (const branch of ["cached", "live"]) {
    test(`${branch} release lookup propagates caller cancellation`, async () => {
      await cacheFixture(async () => {
        if (branch === "cached") writeVersionCheck(clangd.githubRepo, "1.0.0");
        const controller = new AbortController();
        let receivedSignal: AbortSignal | null = null;
        const fakeFetch = (async (_url: unknown, init?: RequestInit) => {
          receivedSignal = init?.signal ?? null;
          controller.abort();
          return new Response("failure", { status: 500 });
        }) as typeof fetch;
        await resolveTargetTag(clangd, config, fakeFetch, controller.signal);
        expect(receivedSignal?.aborted).toBe(true);
      });
    });
  }

  test("tag lookup strips credentials on cross-origin redirects", async () => {
    await withEnv({ GITHUB_TOKEN: "fixture-token-not-a-secret" }, async () => {
      const authorizations: Array<string | null> = [];
      const fakeFetch = (async (_url: unknown, init?: RequestInit) => {
        authorizations.push(new Headers(init?.headers).get("authorization"));
        if (authorizations.length === 1)
          return new Response(null, {
            status: 302,
            headers: { location: "https://objects.githubusercontent.com/release.json" },
          });
        return new Response(JSON.stringify({ tag_name: "1.0.0", assets: [] }));
      }) as typeof fetch;
      await fetchReleaseByTag("owner/repo", "1.0.0", fakeFetch);
      expect(authorizations).toEqual(["Bearer fixture-token-not-a-secret", null]);
    });
  });

  for (const writer of ["npm", "github", "version"]) {
    test(`${writer} metadata replaces rather than truncates the previous inode`, async () => {
      await cacheFixture((root) => {
        const pkg = lspPackageDir("fixture");
        mkdirSync(pkg, { recursive: true });
        const path = join(pkg, writer === "version" ? ".aft-version-check" : ".aft-installed");
        const old = join(root, "old-metadata");
        writeFileSync(path, "old-metadata");
        linkSync(path, old);
        if (writer === "npm") writeInstalledMetaIn(pkg, "1.0.0", "0".repeat(64));
        if (writer === "github") writeGithubMeta(pkg, "1.0.0", "0".repeat(64));
        if (writer === "version") writeVersionCheck("fixture", "1.0.0");
        expect(readFileSync(old, "utf8")).toBe("old-metadata");
        expect(JSON.parse(readFileSync(path, "utf8"))).toBeDefined();
      });
    });
  }

  test("tar size precheck reads sizes rather than numeric filenames", async () => {
    await cacheFixture(async (root) => {
      const name = "5000000000-payload";
      writeFileSync(join(root, name), "four");
      const archive = join(root, "payload.tar");
      execFileSync("tar", ["-cf", archive, "-C", root, name]);
      expect(await precheckArchive(archive, "tar")).toBe(4);
    });
  });

  test("executable upgrade leaves the previous executable inode intact", async () => {
    await cacheFixture(async (root) => {
      const zls = findGithubServerById("zls");
      if (!zls) throw new Error("zls fixture missing");
      const asset = zls.resolveAsset("linux", "x64", "1.0.0");
      if (!asset) throw new Error("zls asset fixture missing");
      const tree = join(root, "tree");
      mkdirSync(tree);
      writeFileSync(join(tree, "zls"), "new-binary");
      const archive = join(root, "zls.tar.xz");
      execFileSync("tar", ["-cJf", archive, "-C", tree, "zls"]);
      const binaryDir = join(root, "lsp-binaries", "zls", "bin");
      mkdirSync(binaryDir, { recursive: true });
      const binary = join(binaryDir, "zls");
      writeFileSync(binary, "old-binary");
      writeGithubMeta(
        join(binaryDir, ".."),
        "0.9.0",
        createHash("sha256").update("old-binary").digest("hex"),
      );
      linkSync(binary, join(root, "old-binary"));
      const fakeFetch = (async (url: unknown) =>
        String(url).includes("/releases/tags/")
          ? new Response(
              JSON.stringify({
                tag_name: "1.0.0",
                assets: [
                  {
                    name: asset.name,
                    browser_download_url:
                      "https://github.com/zigtools/zls/releases/download/1.0.0/payload.tar.xz",
                  },
                ],
              }),
            )
          : new Response(readFileSync(archive))) as typeof fetch;
      const result = runGithubAutoInstall(
        new Set(["zls"]),
        { ...config, versions: { [zls.githubRepo]: "1.0.0" } },
        fakeFetch,
      );
      await result.installsComplete;
      expect(readFileSync(binary, "utf8")).toBe("new-binary");
      expect(readFileSync(join(root, "old-binary"), "utf8")).toBe("old-binary");
      await abortInFlightGithubInstalls();
    });
  });

  test("beforeExit does not suppress later signal cleanup", () => {
    const module = join(import.meta.dir, "..", "shutdown-hooks.ts");
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
      { encoding: "utf8", timeout: 10_000 },
    );
    expect(result.status).toBe(143);
    expect(result.stdout).toBe("late-cleanup\n");
  });
});

describe("npm deadline", () => {
  test("stalled npm settles only after deadline termination is confirmed", async () => {
    await cacheFixture(async (root) => {
      const spec = NPM_LSP_TABLE[0];
      if (!spec) throw new Error("npm fixture missing");
      const controller = new AbortController();
      const retained: boolean[] = [];
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
        spawn: (() => child) as typeof import("node:child_process").spawn,
        terminate: (() => {
          onTerminating();
          return new Promise<void>((resolve) => {
            confirmTermination = resolve;
          });
        }) as typeof import("@cortexkit/aft-bridge").terminateNpmProcessTree,
      };
      const rescue = setTimeout(() => controller.abort(), 1000);
      try {
        let settled = false;
        const installing = runInstall(
          spec,
          "1.0.0",
          root,
          { retain: () => retained.push(true), release: () => {} },
          controller.signal,
          0,
          runtime,
        );
        void installing.then(() => {
          settled = true;
        });
        await terminating;
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

describe("release platform and identity", () => {
  test("clangd declines Linux ARM64 instead of selecting an x64 executable", () => {
    expect(clangd.resolveAsset("linux", "arm64", "21.1.0")).toBeNull();
    expect(clangd.resolveAsset("linux", "x64", "21.1.0")?.name).toBe("clangd-linux-21.1.0.zip");
  });

  test("Pi tag lookups identify the Pi installer", async () => {
    const headers: Array<string | null> = [];
    const fakeFetch = (async (_url: unknown, init?: RequestInit) => {
      headers.push(new Headers(init?.headers).get("user-agent"));
      return new Response(JSON.stringify({ tag_name: "1.0.0", assets: [] }));
    }) as typeof fetch;
    await fetchReleaseByTag("owner/repo", "1.0.0", fakeFetch);
    expect(headers).toEqual(["aft-pi"]);
  });
});
