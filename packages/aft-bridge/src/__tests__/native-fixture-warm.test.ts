/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { mkdtempSync, realpathSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { measureColdNativeFixture, writeAftFixture } from "./test-utils/aft-executable-fixture.js";

describe.skipIf(process.platform === "win32")("native fixture cold execution", () => {
  let root: string;
  beforeEach(() => {
    root = mkdtempSync(join(tmpdir(), "aft-native-warm-"));
  });
  afterEach(() => {
    rmSync(root, { recursive: true, force: true });
  });

  test("warm assessment mode never runs the configured failure or output behavior", () => {
    const executable = writeAftFixture(join(root, "aft"), {
      exitCode: 42,
      stdout: "normal stdout",
      stderr: "normal stderr",
    });
    const warmed = spawnSync(executable, ["--aft-fixture-warm"], {
      encoding: "utf8",
      timeout: 60_000,
    });
    expect(warmed.error).toBeUndefined();
    expect(warmed.status).toBe(0);
    expect(warmed.stdout).toBe("aft-fixture-ready\n");
    expect(warmed.stderr).toBe("");
    const normal = spawnSync(executable, [], { encoding: "utf8", timeout: 60_000 });
    expect(normal.status).toBe(42);
    expect(normal.stdout).toBe("normal stdout");
    expect(normal.stderr).toBe("normal stderr");
  }, 65_000);

  test("resolver symlinks keep the same prepared native executable inode", () => {
    const one = writeAftFixture(join(root, "one"), { exitCode: 0, stdout: "inode probe" });
    const two = writeAftFixture(join(root, "two"), { exitCode: 0, stdout: "inode probe" });
    expect(statSync(one).ino).toBe(statSync(two).ino);
    if (process.platform === "darwin") {
      expect(spawnSync("codesign", ["--verify", "--strict", one], { timeout: 60_000 }).status).toBe(
        0,
      );
    }
  }, 65_000);

  test.skipIf(process.platform !== "darwin")(
    "macOS native fixture executes from /tmp instead of TMPDIR",
    () => {
      const executable = writeAftFixture(join(root, "aft"), { stdout: "location probe" });
      expect(realpathSync(executable).startsWith(`${realpathSync("/tmp")}/`)).toBe(true);
    },
    65_000,
  );

  test("first-byte probe measures the actual native fixture and its warmed inode", () => {
    const measured = measureColdNativeFixture(join(root, "probe"), randomUUID());
    for (const timing of [measured.cold, measured.warm]) {
      expect(timing.firstByteMs).toBeGreaterThanOrEqual(0);
      expect(timing.closeMs).toBeGreaterThanOrEqual(timing.firstByteMs);
      expect(timing.spawnReturnMs).toBeGreaterThanOrEqual(0);
    }
  }, 140_000);

  test.skipIf(process.platform !== "darwin" || process.env.AFT_TEST_NATIVE_FIXTURE_TIMING !== "1")(
    "macOS cold TMPDIR and /tmp executables report time to first child byte",
    () => {
      const systemRoot = mkdtempSync(join(tmpdir(), "aft-cold-timing-"));
      const fastRoot = mkdtempSync("/tmp/aft-cold-timing-");
      try {
        // Distinct source bytes prevent an earlier code-signing assessment from
        // making a new executable at either location look artificially warm.
        for (const [location, directory] of [
          ["TMPDIR", systemRoot],
          ["/tmp", fastRoot],
        ] as const) {
          const measured = measureColdNativeFixture(directory, randomUUID());
          console.log(`COLD_FIXTURE_TIMING ${JSON.stringify({ location, ...measured })}`);
          expect(measured.cold.firstByteMs).toBeGreaterThanOrEqual(0);
          expect(measured.warm.firstByteMs).toBeGreaterThanOrEqual(0);
        }
      } finally {
        rmSync(systemRoot, { recursive: true, force: true });
        rmSync(fastRoot, { recursive: true, force: true });
      }
    },
    280_000,
  );
});
