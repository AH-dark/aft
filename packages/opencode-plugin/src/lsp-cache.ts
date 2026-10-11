/**
 * Disk layout and bookkeeping for AFT's auto-installed LSP cache.
 *
 * Layout under `<aft-cache-root>/lsp-packages/`:
 *
 *   <pkg>/
 *     node_modules/.bin/<binary>     ← actual installed binary
 *     node_modules/<pkg>/...
 *     package.json                   ← created by `bun add`
 *     .aft-version-check             ← JSON: { last_checked: ISO, latest: "X.Y.Z" }
 *     .aft-installing                ← presence = install in progress (lockfile)
 *
 * For scoped packages like `@vue/language-server`, the `@` is preserved in
 * the directory path. `<pkg>` is URL-encoded to keep filesystem-safe paths
 * for any future packages with unusual characters.
 */

import { randomUUID } from "node:crypto";
import {
  closeSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import {
  acquireLspInstallLease,
  getAftCacheRoot,
  getAftLspPackagesDir,
} from "@cortexkit/aft-bridge";
import { log, warn } from "./logger.js";

/** Keep aftCacheBase as an alias for getAftCacheRoot so existing callers using the older name continue to work. */
export function aftCacheBase(): string {
  return getAftCacheRoot();
}

/** Root directory for npm-installed LSP packages. */
export function lspCacheRoot(): string {
  return getAftLspPackagesDir();
}

/** Directory for one specific npm package's install. */
export function lspPackageDir(npmPackage: string): string {
  return join(lspCacheRoot(), encodeURIComponent(npmPackage));
}

/** Path to the binary inside that package's `node_modules/.bin/`. */
export function lspBinaryPath(npmPackage: string, binary: string): string {
  return join(lspPackageDir(npmPackage), "node_modules", ".bin", binary);
}

/** Directory passed to Rust as part of `lsp_paths_extra`. */
export function lspBinDir(npmPackage: string): string {
  return join(lspPackageDir(npmPackage), "node_modules", ".bin");
}

/** True when the cached binary file exists. */
export function isInstalled(npmPackage: string, binary: string): boolean {
  for (const candidate of lspBinaryCandidates(binary)) {
    try {
      if (statSync(join(lspBinDir(npmPackage), candidate)).isFile()) return true;
    } catch {
      // Try the next Windows shim extension.
    }
  }
  return false;
}

function lspBinaryCandidates(binary: string): string[] {
  if (process.platform !== "win32") return [binary];
  return [binary, `${binary}.cmd`, `${binary}.exe`, `${binary}.bat`];
}

/**
 * Per-install metadata recorded after a successful install.
 *
 * Persisting the installed version lets us detect a
 * `lsp.versions` pin change and trigger a transparent reinstall.
 *
 * Persisting the SHA-256 of the downloaded archive enables
 * Trust-On-First-Use verification — if the same tag is ever reinstalled
 * with a different hash, we reject it (the release was retroactively
 * rewritten or the download was tampered with). `sha256` is optional
 * because npm-installed packages don't have a single archive to hash,
 * and historic installs predating this field have no recorded hash.
 *
 * `version` is the resolved tag/semver string (npm package version or
 * GitHub release tag). `installedAt` is informational only.
 */
export interface InstalledMeta {
  version: string;
  installedAt: string;
  /** SHA-256 of the downloaded archive (GitHub installs only). */
  sha256?: string;
}

const INSTALLED_META_FILE = ".aft-installed";

/**
 * Write the installed-version record into `installDir`. Used by both the
 * npm install path (cache layout `<lspPackageDir>/<pkg>/`) and the GitHub
 * install path (cache layout `<ghPackageDir>/<spec.id>/`). Best-effort —
 * failures only logged.
 *
 * Pass `sha256` for GitHub installs so the next session can do TOFU
 * verification (audit v0.17 #1). npm installs leave it undefined since
 * there's no single archive to hash.
 */
export function writeInstalledMetaIn(installDir: string, version: string, sha256?: string): void {
  try {
    mkdirSync(installDir, { recursive: true });
    const meta: InstalledMeta = {
      version,
      installedAt: new Date().toISOString(),
      ...(sha256 ? { sha256 } : {}),
    };
    writeFileSync(join(installDir, INSTALLED_META_FILE), JSON.stringify(meta), "utf8");
  } catch (err) {
    log(`[lsp-cache] failed to write installed-meta in ${installDir}: ${err}`);
  }
}

/** Read the installed-version record from `installDir`, or null if missing/corrupt. */
export function readInstalledMetaIn(installDir: string): InstalledMeta | null {
  const path = join(installDir, INSTALLED_META_FILE);
  try {
    if (!statSync(path).isFile()) return null;
    const raw = readFileSync(path, "utf8");
    const parsed = JSON.parse(raw) as Partial<InstalledMeta>;
    if (typeof parsed.version !== "string" || parsed.version.length === 0) return null;
    return {
      version: parsed.version,
      installedAt: typeof parsed.installedAt === "string" ? parsed.installedAt : "",
      ...(typeof parsed.sha256 === "string" && parsed.sha256.length > 0
        ? { sha256: parsed.sha256 }
        : {}),
    };
  } catch {
    return null;
  }
}

/** npm install path: write installed metadata into the package cache dir.
 *
 * Pass `sha256` of the installed binary so the next
 * session can do TOFU verification on reinstalls of the same version.
 */
export function writeInstalledMeta(packageKey: string, version: string, sha256?: string): void {
  writeInstalledMetaIn(lspPackageDir(packageKey), version, sha256);
}

/** npm install path: read installed metadata from the package cache dir. */
export function readInstalledMeta(packageKey: string): InstalledMeta | null {
  return readInstalledMetaIn(lspPackageDir(packageKey));
}

export type InstallLockLease = import("@cortexkit/aft-bridge").LspInstallLease;

/** Pi and OpenCode coordinate installs through shared claims outside removable cache directories. */
export function acquireInstallLock(lockKey: string): InstallLockLease | null {
  return acquireLspInstallLease(lockKey, lspPackageDir(lockKey));
}

/**
 * Best-effort liveness check for a PID. Sending signal 0 doesn't actually
 * deliver anything; it just reports whether we'd be allowed to. Errors:
 * `ESRCH` = no such process (dead). `EPERM` = exists but not ours (alive).
 * Any other error: assume alive (safer for stealing).
 */
function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    const code = (err as NodeJS.ErrnoException).code;
    if (code === "ESRCH") return false;
    return true;
  }
}

/**
 * Run `task` while holding the install lock. The lock is released only
 * when `task` settles (resolves or rejects), not when it starts.
 *
 * Returns the task's resolved value, or `null` if the lock could not be
 * acquired (caller should treat as "another install in progress").
 */
export async function withInstallLock<T>(
  lockKey: string,
  task: (lease: InstallLockLease) => Promise<T>,
): Promise<T | null> {
  const lease = acquireInstallLock(lockKey);
  if (!lease) return null;
  try {
    return await task(lease);
  } finally {
    lease.release();
  }
}

const AUTO_INSTALL_STAMP_FILE = "last-lsp-autoinstall.json";
const AUTO_INSTALL_WINDOW_MS = 60 * 1000;
const AUTO_INSTALL_LOCK_STALE_MS = 30 * 60 * 1000;

export interface AutoInstallPassLease {
  release(): void;
}

interface AutoInstallLockOwner {
  pid: number;
  startedMs: number;
  token: string;
}

function autoInstallStampPath(): string {
  return join(aftCacheBase(), AUTO_INSTALL_STAMP_FILE);
}

function hasRecentAutoInstallStamp(path: string): boolean {
  try {
    const parsed = JSON.parse(readFileSync(path, "utf8")) as { lastRunMs?: unknown };
    return (
      typeof parsed.lastRunMs === "number" &&
      Number.isFinite(parsed.lastRunMs) &&
      Math.abs(Date.now() - parsed.lastRunMs) < AUTO_INSTALL_WINDOW_MS
    );
  } catch {
    return false;
  }
}

function readAutoInstallLockOwner(path: string): AutoInstallLockOwner | null {
  try {
    const parsed = JSON.parse(readFileSync(path, "utf8")) as Partial<AutoInstallLockOwner>;
    if (
      typeof parsed.pid !== "number" ||
      typeof parsed.startedMs !== "number" ||
      typeof parsed.token !== "string"
    ) {
      return null;
    }
    return parsed as AutoInstallLockOwner;
  } catch {
    return null;
  }
}

function autoInstallLockIsStale(path: string): boolean {
  const owner = readAutoInstallLockOwner(path);
  let age = Number.POSITIVE_INFINITY;
  try {
    age = Math.abs(Date.now() - statSync(path).mtimeMs);
  } catch {
    return true;
  }
  if (age >= AUTO_INSTALL_LOCK_STALE_MS) return true;
  if (process.platform === "win32" || !owner) return false;
  return !isProcessAlive(owner.pid);
}

/** Claim the machine-wide 60-second startup auto-install slot. */
export function claimLspAutoInstallPass(): AutoInstallPassLease | null {
  const stampPath = autoInstallStampPath();
  const lock = `${stampPath}.lock`;
  mkdirSync(aftCacheBase(), { recursive: true });
  if (hasRecentAutoInstallStamp(stampPath)) return null;

  const owner: AutoInstallLockOwner = {
    pid: process.pid,
    startedMs: Date.now(),
    token: randomUUID(),
  };
  const tryClaim = (): boolean => {
    try {
      const fd = openSync(lock, "wx");
      try {
        writeFileSync(fd, JSON.stringify(owner));
      } finally {
        closeSync(fd);
      }
      return true;
    } catch (err) {
      if ((err as NodeJS.ErrnoException).code === "EEXIST") return false;
      throw err;
    }
  };

  let claimed = false;
  try {
    claimed = tryClaim();
    if (!claimed && autoInstallLockIsStale(lock)) {
      rmSync(lock, { force: true });
      claimed = tryClaim();
    }
    if (!claimed) return null;

    const release = () => {
      const current = readAutoInstallLockOwner(lock);
      if (current?.pid === owner.pid && current.token === owner.token) {
        rmSync(lock, { force: true });
      }
    };
    if (hasRecentAutoInstallStamp(stampPath)) {
      release();
      return null;
    }

    const tempPath = `${stampPath}.tmp-${process.pid}-${owner.token}`;
    writeFileSync(tempPath, `${JSON.stringify({ lastRunMs: Date.now() })}\n`, {
      encoding: "utf8",
      mode: 0o600,
    });
    renameSync(tempPath, stampPath);
    return { release };
  } catch (err) {
    if (claimed) {
      const current = readAutoInstallLockOwner(lock);
      if (current?.pid === owner.pid && current.token === owner.token) {
        rmSync(lock, { force: true });
      }
    }
    warn(`[lsp] could not coordinate auto-install pass: ${err}`);
    return null;
  }
}

/** Last-checked metadata stored next to the install. */
interface VersionCheckRecord {
  /** ISO timestamp of the last `npm registry probe`. */
  last_checked: string;
  /** Version string that was eligible at last check (after grace filter). */
  latest_eligible: string | null;
}

const VERSION_CHECK_FILE = ".aft-version-check";

export function readVersionCheck(npmPackage: string): VersionCheckRecord | null {
  const file = join(lspPackageDir(npmPackage), VERSION_CHECK_FILE);
  try {
    const raw = readFileSync(file, "utf8");
    const parsed = JSON.parse(raw) as Partial<VersionCheckRecord>;
    if (typeof parsed.last_checked === "string") {
      return {
        last_checked: parsed.last_checked,
        latest_eligible: typeof parsed.latest_eligible === "string" ? parsed.latest_eligible : null,
      };
    }
    return null;
  } catch {
    return null;
  }
}

export function writeVersionCheck(npmPackage: string, latest: string | null): void {
  mkdirSync(lspPackageDir(npmPackage), { recursive: true });
  const file = join(lspPackageDir(npmPackage), VERSION_CHECK_FILE);
  const record: VersionCheckRecord = {
    last_checked: new Date().toISOString(),
    latest_eligible: latest,
  };
  writeFileSync(file, JSON.stringify(record, null, 2));
}

/** True if more than `graceDays` × 24h have elapsed since `last_checked`. */
export function shouldRecheckVersion(
  record: VersionCheckRecord | null,
  weeklyCheckIntervalMs = 7 * 24 * 60 * 60 * 1000,
): boolean {
  if (!record) return true;
  const age = Date.now() - new Date(record.last_checked).getTime();
  if (Number.isNaN(age) || age < 0) return true;
  return age >= weeklyCheckIntervalMs;
}
