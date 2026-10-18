import { createGithubInstaller } from "../lsp-github-install.js";
import { githubLayoutTests } from "./test-utils/github-layout.js";

githubLayoutTests(
  createGithubInstaller({
    log: () => {},
    warn: () => {},
    error: () => {},
    readVersionCheck: () => null,
    writeVersionCheck: () => {},
    shouldRecheckVersion: () => true,
    withInstallLock: async (_repo, action) => action(),
    hasRootMarker: () => false,
    relevantExtensionsInProject: () => new Set(),
    reportInstallingBinaries: false,
  }),
);
