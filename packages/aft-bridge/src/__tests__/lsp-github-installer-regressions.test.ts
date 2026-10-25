import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { getAftLspPackagesDir } from "../cache-paths.js";
import { createGithubInstaller } from "../lsp-github-install.js";
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

describe("GitHub release lookups", () => {
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
});
