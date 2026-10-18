import { createGithubInstaller } from "@cortexkit/aft-bridge";
import { error, log, warn } from "./logger.js";
import {
  acquireInstallLock,
  readVersionCheck,
  shouldRecheckVersion,
  withInstallLock,
  writeVersionCheck,
} from "./lsp-cache.js";
import { hasRootMarker, relevantExtensionsInProject } from "./lsp-project-relevance.js";

export type {
  Arch,
  GithubAutoInstallResult,
  GithubInstallConfig,
  GithubServerSpec,
  Platform,
} from "@cortexkit/aft-bridge";
export { detectHostPlatform, findGithubServerById, GITHUB_LSP_TABLE } from "@cortexkit/aft-bridge";

export const {
  ghBinaryPath,
  isGithubInstalled,
  __githubBinaryHashCountForTests,
  validateExtraction,
  precheckArchiveContents,
  abortInFlightGithubInstalls,
  runGithubAutoInstall,
  discoverRelevantGithubServers,
  _assertAllowedDownloadUrlForTesting,
  _downloadFileForTesting,
  _extractArchiveSafelyForTesting,
  _precheckArchiveSizeForTesting,
  _runArchiveToolForTesting,
} = createGithubInstaller({
  acquireInstallLock,
  log,
  warn,
  error,
  readVersionCheck,
  shouldRecheckVersion,
  withInstallLock,
  writeVersionCheck,
  hasRootMarker,
  relevantExtensionsInProject,
  reportInstallingBinaries: false,
});
