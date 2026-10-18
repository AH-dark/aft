import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { statSync } from "node:fs";
import { join } from "node:path";

export interface StorageSnapshot {
  schemaVersion: number;
  objects: Array<{ type: "table" | "index"; name: string; sql: string | null }>;
}

export interface UnreachableStorageSnapshot {
  kind: "unreachable";
  reason: "EPERM" | "EACCES";
}

export type StorageSnapshotResult = StorageSnapshot | UnreachableStorageSnapshot | null;

type DatabaseStat = (path: string) => unknown;

// Preload checks run both before and after tests, so report each denial once.
const reportedUnreachableReasons = new Set<UnreachableStorageSnapshot["reason"]>();

/** Account lookup ignores the suite's replacement HOME/XDG variables. */
export function liveAftDatabasePath(): string {
  let dataHome: string;
  if (process.platform === "win32") {
    const result = spawnSync(
      "powershell.exe",
      [
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "[Environment]::GetFolderPath('LocalApplicationData')",
      ],
      { encoding: "utf8", windowsHide: true },
    );
    assert.equal(result.status, 0, `OS LocalAppData lookup failed: ${result.stderr}`);
    dataHome = result.stdout.trim();
    assert.ok(dataHome, "OS LocalAppData lookup must not return an empty path");
  } else {
    // Bun's os.userInfo().homedir reads HOME, unlike Node's implementation.
    // Query the native account database directly so suite isolation cannot
    // turn the live-store canary into a check of its own throwaway directory.
    const uid = spawnSync("/usr/bin/id", ["-u"], { encoding: "utf8" });
    assert.equal(uid.status, 0, `OS uid lookup failed: ${uid.stderr}`);
    const mac = process.platform === "darwin";
    const account = spawnSync(
      mac ? "/usr/bin/dscacheutil" : "/usr/bin/getent",
      mac ? ["-q", "user", "-a", "uid", uid.stdout.trim()] : ["passwd", uid.stdout.trim()],
      { encoding: "utf8" },
    );
    assert.equal(account.status, 0, `OS account lookup failed: ${account.stderr}`);
    const home = mac
      ? /^dir: (.+)$/m.exec(account.stdout)?.[1]
      : account.stdout.trim().split(":")[5];
    assert.ok(home?.startsWith("/"), "OS account lookup must return an absolute home");
    dataHome = join(home, ".local", "share");
  }
  return join(dataHome, "cortexkit", "aft", "aft.db");
}

/** The SQLite handle belongs to a separate, short-lived process. Opening a
 * second fd here could invalidate POSIX locks held by a bridge in this process.
 * readonly forbids creation/migration; never use AFT itself to inspect the store.
 */
export function snapshotAftDatabase(
  path: string,
  stat: DatabaseStat = statSync,
): StorageSnapshotResult {
  try {
    stat(path);
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code;
    if (code === "ENOENT") return null;
    if (code === "EPERM" || code === "EACCES") {
      if (!reportedUnreachableReasons.has(code)) {
        reportedUnreachableReasons.add(code);
        console.warn(`AFT live-store canary is inactive: database stat failed with ${code}`);
      }
      return { kind: "unreachable", reason: code };
    }
    throw error;
  }
  const result = spawnSync(
    process.execPath,
    [
      "-e",
      `
    const { Database } = require("bun:sqlite");
    const db = new Database(process.env.AFT_STORAGE_CANARY_DB, { readonly: true });
    try {
      db.exec("BEGIN");
      const row = db.query("SELECT COALESCE(MAX(version), 0) AS version FROM schema_version").get();
      const objects = db.query("SELECT type, name, sql FROM sqlite_master WHERE type IN ('table', 'index') ORDER BY type, name").all();
      process.stdout.write(JSON.stringify({ schemaVersion: row.version, objects }));
    } finally { db.close(); }
  `,
    ],
    { env: { ...process.env, AFT_STORAGE_CANARY_DB: path }, encoding: "utf8", windowsHide: true },
  );
  assert.equal(result.status, 0, `read-only storage canary failed: ${result.stderr}`);
  return JSON.parse(result.stdout) as StorageSnapshot;
}

export function assertAftDatabaseUnchanged(
  path: string,
  before: StorageSnapshotResult,
  stat: DatabaseStat = statSync,
): void {
  const after = snapshotAftDatabase(path, stat);
  if (isUnreachable(before) && isUnreachable(after)) return;
  assert.deepEqual(after, before, `live AFT database changed during tests: ${path}`);
}

function isUnreachable(snapshot: StorageSnapshotResult): snapshot is UnreachableStorageSnapshot {
  return snapshot !== null && "kind" in snapshot && snapshot.kind === "unreachable";
}
