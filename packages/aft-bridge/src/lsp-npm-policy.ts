import { spawn } from "./child-process.js";
import {
  type NpmInvocation,
  npmInvocation,
  npmSpawnEnv,
  type ResolvedNpm,
  terminateNpmProcessTree,
} from "./npm-resolver.js";

// Keep the in-flight promise and its settled result: all packages and later
// install passes using the same npm executable share one capability check.
const npmCutoffSupport = new Map<string, Promise<boolean>>();
const NPM_PROBE_TIMEOUT_MS = 5_000;
const NPM_PROBE_OUTPUT_LIMIT = 1_048_576;

function probeNpmCutoffSupport(npm: ResolvedNpm): Promise<boolean> {
  const invocation = npmInvocation(npm, ["--version"]);
  return new Promise((resolve) => {
    const child = spawn(invocation.command, invocation.args, {
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...npmSpawnEnv(npm), ...invocation.env },
      windowsVerbatimArguments: invocation.windowsVerbatimArguments,
    });
    child.unref();
    let stdout = "";
    let stdoutBytes = 0;
    let stderrBytes = 0;
    let settled = false;
    let stopping = false;
    const finish = (supported: boolean) => {
      if (settled) return;
      settled = true;
      clearTimeout(deadline);
      child.stdout?.destroy();
      child.stderr?.destroy();
      resolve(supported);
    };
    const stop = () => {
      if (settled || stopping) return;
      stopping = true;
      // Kill the spawned process immediately; Windows command shims need their
      // whole process tree terminated before their inherited pipes close.
      if (!invocation.windowsCmdShim) {
        try {
          child.kill("SIGKILL");
        } catch {
          // The child may have exited just before the deadline.
        }
      }
      void terminateNpmProcessTree(child, invocation, process.env, 500).then(
        () => finish(false),
        () => finish(false),
      );
    };
    const deadline = setTimeout(stop, NPM_PROBE_TIMEOUT_MS);
    child.stdout?.setEncoding("utf8");
    child.stdout?.on("data", (chunk: string) => {
      if (settled || stopping) return;
      stdoutBytes += Buffer.byteLength(chunk);
      if (stdoutBytes > NPM_PROBE_OUTPUT_LIMIT) stop();
      else stdout += chunk;
    });
    child.stderr?.setEncoding("utf8");
    child.stderr?.on("data", (chunk: string) => {
      stderrBytes += Buffer.byteLength(chunk);
      if (stderrBytes > NPM_PROBE_OUTPUT_LIMIT) stop();
    });
    child.once("error", () => finish(false));
    child.once("close", (code) => {
      const major = Number(/^([0-9]+)\.[0-9]+\.[0-9]+(?:\s|$)/.exec(stdout.trim())?.[1]);
      finish(!stopping && code === 0 && Number.isSafeInteger(major) && major >= 7);
    });
  });
}

function npmSupportsCutoff(npm: ResolvedNpm): Promise<boolean> {
  const existing = npmCutoffSupport.get(npm.command);
  if (existing) return existing;
  const pending = Promise.resolve()
    .then(() => probeNpmCutoffSupport(npm))
    .catch(() => false);
  npmCutoffSupport.set(npm.command, pending);
  return pending;
}

/** Unpinned installs exclude dependencies published inside the grace window; explicit pins bypass that filter. */
export function lspPublicationCutoff(
  graceDays: number,
  pinned: boolean,
  now = Date.now(),
): string | undefined {
  return pinned ? undefined : new Date(now - graceDays * 86_400_000).toISOString();
}

/** Suppress lifecycle scripts. Publication-date filtering requires npm 7+; unknown or older clients resolve to null rather than install. */
export async function lspNpmInstallInvocation(
  npm: ResolvedNpm,
  target: string,
  before?: string,
): Promise<NpmInvocation | null> {
  if (before !== undefined && !(await npmSupportsCutoff(npm))) return null;
  return npmInvocation(npm, [
    "install",
    "--no-save",
    "--ignore-scripts",
    "--silent",
    ...(before ? [`--before=${before}`] : []),
    target,
  ]);
}
