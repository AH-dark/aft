import { describe, expect, test } from "bun:test";
import {
  linkSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { getAftLspPackagesDir } from "../cache-paths.js";
import { createGithubInstaller } from "../lsp-github-install.js";
import { findGithubServerById } from "../lsp-github-table.js";
import { withEnv } from "./test-utils/env-guard.js";

type VersionCheck = { last_checked: string; latest_eligible: string | null };

function acquireInstallLock(repo: string) {
  const path = join(getAftLspPackagesDir(), encodeURIComponent(repo), ".aft-installing");
  mkdirSync(dirname(path), { recursive: true });
  try {
    writeFileSync(path, `${process.pid}\n`, { flag: "wx" });
    return { release: () => unlinkSync(path) };
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "EEXIST") return null;
    throw error;
  }
}

/** An installer whose version-check records live in `versionChecks`. */
function installerFixture(versionChecks = new Map<string, VersionCheck>()) {
  return createGithubInstaller({
    log: () => {},
    warn: () => {},
    error: () => {},
    readVersionCheck: (repo) => versionChecks.get(repo) ?? null,
    writeVersionCheck: (repo, latest) => {
      versionChecks.set(repo, { last_checked: new Date().toISOString(), latest_eligible: latest });
    },
    // Cached records stay fresh, so the cached-tag branch is taken when one exists.
    shouldRecheckVersion: (record) => record === null,
    acquireInstallLock,
    withInstallLock: async (repo, action) => {
      const lease = acquireInstallLock(repo);
      if (!lease) return null;
      try {
        return await action();
      } finally {
        lease.release();
      }
    },
    hasRootMarker: () => false,
    relevantExtensionsInProject: () => new Set(),
    reportInstallingBinaries: false,
  });
}

async function cacheFixture(
  fn: (root: string) => Promise<void> | void,
  env: Record<string, string> = {},
): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), "aft-github-install-regression-"));
  try {
    await withEnv({ ...env, AFT_CACHE_DIR: root }, () => fn(root));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

const config = { autoInstall: true, graceDays: 7, versions: {}, disabled: new Set<string>() };
const clangd = findGithubServerById("clangd");
if (!clangd) throw new Error("clangd fixture missing");

describe("GitHub install metadata", () => {
  test("github metadata replaces rather than truncates the previous inode", async () => {
    await cacheFixture((root) => {
      const pkg = join(root, "lsp-binaries", "fixture");
      mkdirSync(pkg, { recursive: true });
      const path = join(pkg, ".aft-installed");
      // A second name for the old inode shows whether the writer truncated
      // the file in place (a concurrent reader's view) or renamed over it.
      const old = join(root, "old-metadata");
      writeFileSync(path, "old-metadata");
      linkSync(path, old);
      installerFixture()._writeGithubInstalledMetaForTesting(pkg, "1.0.0", "0".repeat(64));
      expect(readFileSync(old, "utf8")).toBe("old-metadata");
      expect(JSON.parse(readFileSync(path, "utf8")).version).toBe("1.0.0");
      expect(readdirSync(pkg)).toEqual([".aft-installed"]);
    });
  });
});

describe("GitHub release lookups", () => {
  test("live release tags cannot persist unsafe path components", async () => {
    await cacheFixture(async () => {
      const versionChecks = new Map<string, VersionCheck>();
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
      const result = await installerFixture(versionChecks)._resolveTargetTagForTesting(
        clangd,
        config,
        fakeFetch,
      );
      expect(result.tag).toBeNull();
      expect(versionChecks.has(clangd.githubRepo)).toBe(false);
    });
  });
  test("tag lookup strips credentials on cross-origin redirects", async () => {
    await cacheFixture(
      async () => {
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
        const release = await installerFixture()._fetchReleaseByTagForTesting(
          "owner/repo",
          "1.0.0",
          fakeFetch,
        );
        expect(release?.tag).toBe("1.0.0");
        expect(authorizations).toEqual(["Bearer fixture-token-not-a-secret", null]);
      },
      { GITHUB_TOKEN: "fixture-token-not-a-secret" },
    );
  });

  for (const branch of ["cached", "live"] as const) {
    test(`${branch} release lookup propagates caller cancellation`, async () => {
      await cacheFixture(async () => {
        const versionChecks = new Map<string, VersionCheck>();
        if (branch === "cached") {
          versionChecks.set(clangd.githubRepo, {
            last_checked: new Date().toISOString(),
            latest_eligible: "1.0.0",
          });
        }
        const controller = new AbortController();
        // The cached branch looks up one tag; the live branch lists releases.
        // Record the signal each kind of request received separately so one
        // branch's signal cannot stand in for the other's.
        const signals = new Map<"tag" | "list", AbortSignal | null>();
        const fakeFetch = (async (url: unknown, init?: RequestInit) => {
          const kind = String(url).includes("/releases/tags/") ? "tag" : "list";
          if (!signals.has(kind)) signals.set(kind, init?.signal ?? null);
          controller.abort();
          return new Response("failure", { status: 500 });
        }) as typeof fetch;
        await installerFixture(versionChecks)._resolveTargetTagForTesting(
          clangd,
          config,
          fakeFetch,
          controller.signal,
        );
        const observed = signals.get(branch === "cached" ? "tag" : "list");
        expect(observed).toBeInstanceOf(AbortSignal);
        expect(observed?.aborted).toBe(true);
      });
    });
  }
});
