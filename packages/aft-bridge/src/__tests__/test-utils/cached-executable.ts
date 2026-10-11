import { createHash, randomUUID } from "node:crypto";
import {
  chmodSync,
  linkSync,
  mkdirSync,
  readFileSync,
  statSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

const cacheRoot = join(tmpdir(), "aft-test-executables");

export function cachedExecutable(source: string): string {
  return cachedExecutableBytes(Buffer.from(source));
}

export function cachedExecutableBytes(
  bytes: Uint8Array,
  root = cacheRoot,
  name = process.platform === "win32" ? "executable.exe" : "executable",
): string {
  const digest = createHash("sha256").update("mode:755\0").update(bytes).digest("hex");
  const path = join(root, digest, name);
  try {
    const stat = statSync(path);
    if (!stat.isFile() || (stat.mode & 0o777) !== 0o755 || !readFileSync(path).equals(bytes)) {
      throw new Error(`content-addressed test executable is invalid: ${path}`);
    }
    return path;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
  }

  mkdirSync(dirname(path), { recursive: true });
  // Link publication is atomic and never replaces a path another process won.
  const pending = join(dirname(path), `.pending-${process.pid}-${randomUUID()}`);
  try {
    writeFileSync(pending, bytes, { flag: "wx" });
    chmodSync(pending, 0o755);
    try {
      linkSync(pending, path);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
      const stat = statSync(path);
      if (!stat.isFile() || (stat.mode & 0o777) !== 0o755 || !readFileSync(path).equals(bytes)) {
        throw new Error(`content-addressed test executable is invalid: ${path}`);
      }
    }
  } finally {
    try {
      unlinkSync(pending);
    } catch {
      // A failed best-effort cleanup must not hide the original publication error.
    }
  }
  return path;
}

export function linkCachedExecutable(path: string, source: string): string {
  mkdirSync(dirname(path), { recursive: true });
  symlinkSync(cachedExecutable(source), path);
  return path;
}
