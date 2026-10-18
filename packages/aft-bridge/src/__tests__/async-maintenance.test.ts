import { expect, test } from "bun:test";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { getMigrationStatus } from "../migration.js";
import { __test__ } from "../onnx-runtime.js";
import { execTarExtraction, windowsTarExecutable } from "../tar-executable.js";
import { acquireEnv } from "./test-utils/env-guard.js";

test("migration status subprocess yields without blocking the host", async () => {
  const dir = mkdtempSync(join(tmpdir(), "aft-status-async-"));
  let releaseEnv: (() => void) | undefined;
  try {
    const script = join(dir, "status.cjs");
    writeFileSync(
      script,
      'console.log(JSON.stringify({ harness: "pi", migrated: false })); process.exit(0);\n',
    );
    const node = spawnSync("node", ["-p", "process.execPath"], { encoding: "utf8" });
    expect(node.status).toBe(0);
    releaseEnv = await acquireEnv({ NODE_OPTIONS: `--require ${JSON.stringify(script)}` });
    const status = getMigrationStatus({ harness: "pi", binaryPath: node.stdout.trim() });
    let settled = false;
    void status.then(() => {
      settled = true;
    });
    for (let i = 0; i < 10; i++) await Promise.resolve();
    expect(settled).toBe(false);
    expect(await status).toEqual({ harness: "pi", migrated: false });
  } finally {
    releaseEnv?.();
    rmSync(dir, { recursive: true, force: true });
  }
});

for (const bytes of [8000, 1024 * 1024]) {
  test(`migration status captures bounded complete JSON with ${bytes} padding bytes`, async () => {
    const dir = mkdtempSync(join(tmpdir(), "aft-status-output-"));
    let releaseEnv: (() => void) | undefined;
    try {
      const script = join(dir, "status.cjs");
      writeFileSync(
        script,
        `require("node:fs").writeFileSync(1, JSON.stringify({ harness: "pi", migrated: false, padding: "x".repeat(${bytes}) })); process.exit(0);\n`,
      );
      const node = spawnSync("node", ["-p", "process.execPath"], { encoding: "utf8" });
      expect(node.status).toBe(0);
      releaseEnv = await acquireEnv({ NODE_OPTIONS: `--require ${JSON.stringify(script)}` });
      const status = getMigrationStatus({ harness: "pi", binaryPath: node.stdout.trim() });
      if (bytes === 8000) {
        expect(await status).toEqual({
          harness: "pi",
          migrated: false,
          padding: "x".repeat(bytes),
        });
      } else {
        await expect(status).rejects.toThrow(
          "migration status exceeded the subprocess output limit",
        );
      }
    } finally {
      releaseEnv?.();
      rmSync(dir, { recursive: true, force: true });
    }
  });
}

test("ONNX zip extraction yields while the extractor owns the work", async () => {
  const dir = mkdtempSync(join(tmpdir(), "aft-zip-async-"));
  try {
    const file = join(dir, "library.txt");
    const archive = join(dir, "ort.zip");
    // A stored ZIP containing library.txt, independent of a zip writer on PATH.
    writeFileSync(
      archive,
      Buffer.from(
        "UEsDBBQAAAAAAOcIS131Lkh4DQAAAA0AAAALAAAAbGlicmFyeS50eHRsaWJyYXJ5IGJ5dGVzUEsBAhQDFAAAAAAA5whLXfUuSHgNAAAADQAAAAsAAAAAAAAAAAAAAIABAAAAAGxpYnJhcnkudHh0UEsFBgAAAAABAAEAOQAAADYAAAAAAA==",
        "base64",
      ),
    );
    const extraction = __test__.extractZipArchive(archive, dir);
    let settled = false;
    void extraction.then(() => {
      settled = true;
    });
    for (let i = 0; i < 10; i++) await Promise.resolve();
    expect(settled).toBe(false);
    await extraction;
    expect(readFileSync(file, "utf8")).toBe("library bytes");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("private ONNX tar extraction yields and preserves private member permissions", async () => {
  const dir = mkdtempSync(join(tmpdir(), "aft-tar-async-"));
  try {
    const file = join(dir, "library.txt");
    const archive = join(dir, "ort.tar.gz");
    writeFileSync(file, "library bytes", { mode: 0o644 });
    execFileSync(windowsTarExecutable(), ["czf", archive, "-C", dir, "library.txt"]);
    rmSync(file);
    const extraction = execTarExtraction(["xzf", archive, "-C", dir], 120_000, true);
    let settled = false;
    void extraction.then(() => {
      settled = true;
    });
    for (let i = 0; i < 10; i++) await Promise.resolve();
    expect(settled).toBe(false);
    await extraction;
    expect(readFileSync(file, "utf8")).toBe("library bytes");
    if (process.platform !== "win32") expect(statSync(file).mode & 0o077).toBe(0);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
