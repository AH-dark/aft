import { mkdirSync, unlinkSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { getAftLspPackagesDir } from "../cache-paths.js";
import { createGithubInstaller } from "../lsp-github-install.js";
import { githubLayoutTests } from "./test-utils/github-layout.js";

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

githubLayoutTests(
  createGithubInstaller({
    log: () => {},
    warn: () => {},
    error: () => {},
    readVersionCheck: () => null,
    writeVersionCheck: () => {},
    shouldRecheckVersion: () => true,
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
  }),
);
