import { spawnSync } from "./child-process.js";
import {
  type NpmInvocation,
  npmInvocation,
  npmSpawnEnv,
  type ResolvedNpm,
} from "./npm-resolver.js";

/** Unpinned installs exclude dependencies published inside the grace window; explicit pins bypass that filter. */
export function lspPublicationCutoff(
  graceDays: number,
  pinned: boolean,
  now = Date.now(),
): string | undefined {
  return pinned ? undefined : new Date(now - graceDays * 86_400_000).toISOString();
}

/** Suppress lifecycle scripts. Publication-date filtering requires npm 7+; unknown or older clients return null rather than install. */
export function lspNpmInstallInvocation(
  npm: ResolvedNpm,
  target: string,
  before?: string,
): NpmInvocation | null {
  if (before !== undefined) {
    const probe = npmInvocation(npm, ["--version"]);
    const version = spawnSync(probe.command, probe.args, {
      stdio: ["ignore", "pipe", "pipe"],
      encoding: "utf8",
      timeout: 5000,
      env: { ...npmSpawnEnv(npm), ...probe.env },
      windowsVerbatimArguments: probe.windowsVerbatimArguments,
    });
    const major = Number(
      /^([0-9]+)\.[0-9]+\.[0-9]+(?:\s|$)/.exec(version.stdout?.trim() ?? "")?.[1],
    );
    if (version.status !== 0 || !Number.isSafeInteger(major) || major < 7) return null;
  }
  return npmInvocation(npm, [
    "install",
    "--no-save",
    "--ignore-scripts",
    "--silent",
    ...(before ? [`--before=${before}`] : []),
    target,
  ]);
}
