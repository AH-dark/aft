import { randomUUID } from "node:crypto";
import {
  closeSync,
  lstatSync,
  mkdirSync,
  openSync,
  readdirSync,
  readFileSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { hostname } from "node:os";
import { dirname, join } from "node:path";
import { getAftCacheRoot, getAftLspBinariesDir, getAftLspPackagesDir } from "./cache-paths.js";
import { spawnSync } from "./child-process.js";
import {
  type NpmInvocation,
  npmInvocation,
  npmSpawnEnv,
  type ResolvedNpm,
} from "./npm-resolver.js";

export interface LspInstallLease {
  retain(): void;
  release(): void;
}

function stateDir(): string {
  return join(getAftCacheRoot(), "lsp-install-leases");
}

function identity(path: string): string {
  const stat = lstatSync(path);
  if (!stat.isFile() || stat.isSymbolicLink()) throw new Error("invalid LSP lease file");
  return `${stat.dev}:${stat.ino}:${stat.size}:${stat.mtimeMs}:${stat.ctimeMs}`;
}

/** A live or uncertain owner is never displaced merely because its clock is old. */
function reclaimDeadOwner(path: string): boolean {
  try {
    const before = identity(path);
    const raw = readFileSync(path, "utf8");
    const lines = raw.split(/\r?\n/);
    const pid = Number(lines[0]);
    if (path.endsWith(".install") && (lines[3] !== "installing" || !lines[4])) return false;
    if (path.endsWith("clear.lock") && (lines[3] !== "clearing" || !lines[4])) return false;
    if (lines[4] && lines[4] !== hostname()) return false;
    if (!Number.isSafeInteger(pid) || pid <= 0) return false;
    // Windows PID-zero probes can be ambiguous; existing records need explicit recovery.
    if (process.platform === "win32") return false;
    try {
      process.kill(pid, 0);
      return false;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ESRCH") return false;
    }
    // A dead installer owner does not prove its npm descendants stopped.
    // Legacy records lack that proof too; recover these explicitly, not by age.
    if (
      lines[3] === "installing" ||
      path.endsWith("/.aft-installing") ||
      path.endsWith("\\.aft-installing")
    )
      return false;
    if (identity(path) !== before || readFileSync(path, "utf8") !== raw) return false;
    unlinkSync(path);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code === "ENOENT";
  }
}

/** Recovery itself is serialized outside the trees that cache clearing removes. */
function underRecoveryLease<T>(work: () => T, root = stateDir()): T | null {
  const gate = join(root, "recovery.lock");
  let fd: number;
  const owner = `${process.pid}\n${new Date().toISOString()}\n${randomUUID()}\nrecovering\n${hostname()}\n`;
  try {
    mkdirSync(root, { recursive: true });
    fd = openSync(gate, "wx");
  } catch {
    return null;
  }
  try {
    // Do not steal this short-lived gate. A crash here requires explicit recovery.
    writeFileSync(fd, owner);
    return work();
  } catch {
    return null;
  } finally {
    closeSync(fd);
    removeOwned(gate, owner);
  }
}

function publish(path: string, owner: string): void {
  mkdirSync(dirname(path), { recursive: true });
  const fd = openSync(path, "wx");
  try {
    writeFileSync(fd, owner);
  } finally {
    closeSync(fd);
  }
}

function removeOwned(path: string, owner: string): void {
  try {
    const before = identity(path);
    if (readFileSync(path, "utf8") !== owner || identity(path) !== before) return;
    unlinkSync(path);
  } catch {
    // Missing or changed records do not belong to this acquisition anymore.
  }
}

function lease(paths: string[], owner: string, root: string): LspInstallLease {
  let retained = false;
  let released = false;
  const release = (attempt = 0): void => {
    if (retained || released) return;
    const done = underRecoveryLease(() => {
      for (const path of paths) removeOwned(path, owner);
      released = true;
      return true;
    }, root);
    if (done === null && attempt < 100) {
      // A competing process holds the mutation gate; never unlink outside it.
      const retry = setTimeout(() => release(attempt + 1), 10);
      retry.unref?.();
    }
  };
  return {
    retain: () => {
      retained = true;
    },
    release: () => release(),
  };
}

/** Keep a legacy marker for older clients, but make the external lease authoritative. */
export function acquireLspInstallLease(
  key: string,
  legacyDir: string,
  beforePublish?: () => void,
): LspInstallLease | null {
  const root = stateDir();
  return underRecoveryLease(() => {
    if (!reclaimDeadOwner(join(root, "clear.lock"))) return null;
    const external = join(root, `${encodeURIComponent(key)}.install`);
    const legacy = join(legacyDir, ".aft-installing");
    if (!reclaimDeadOwner(external) || !reclaimDeadOwner(legacy)) return null;
    const owner = `${process.pid}\n${new Date().toISOString()}\n${randomUUID()}\ninstalling\n${hostname()}\n`;
    // Tests can interleave another admission at the inspection/publication boundary.
    beforePublish?.();
    publish(external, owner);
    try {
      publish(legacy, owner);
    } catch (error) {
      removeOwned(external, owner);
      throw error;
    }
    return lease([legacy, external], owner, root);
  }, root);
}

/** Refuse the entire clear if any current or legacy installer may still be writing. */
export function acquireLspCacheClearLease(): LspInstallLease | null {
  const root = stateDir();
  return underRecoveryLease(() => {
    const clear = join(root, "clear.lock");
    if (!reclaimDeadOwner(clear)) return null;
    for (const entry of readdirSync(root)) {
      if (entry.endsWith(".install") && !reclaimDeadOwner(join(root, entry))) return null;
    }
    for (const cacheRoot of [getAftLspPackagesDir(), getAftLspBinariesDir()]) {
      let entries: string[];
      try {
        entries = readdirSync(cacheRoot);
      } catch (error) {
        if ((error as NodeJS.ErrnoException).code === "ENOENT") continue;
        return null;
      }
      for (const entry of entries) {
        const stat = lstatSync(join(cacheRoot, entry));
        if (!stat.isDirectory() && !stat.isSymbolicLink()) continue;
        if (!reclaimDeadOwner(join(cacheRoot, entry, ".aft-installing"))) return null;
      }
    }
    const owner = `${process.pid}\n${new Date().toISOString()}\n${randomUUID()}\nclearing\n${hostname()}\n`;
    publish(clear, owner);
    return lease([clear], owner, root);
  }, root);
}

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
