/// <reference path="../bun-test.d.ts" />

import { afterEach, beforeEach, describe, expect, mock, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __setMigrationStatusRunnerForTests, getMigrationStatus } from "../migration.js";
import { acquireEnv } from "./test-utils/env-guard.js";

describe("storage migration status", () => {
  let tempDir: string;
  let releaseEnv: (() => void) | undefined;

  beforeEach(async () => {
    tempDir = mkdtempSync(join(tmpdir(), "aft-migration-status-test-"));
    releaseEnv = await acquireEnv({
      XDG_DATA_HOME: tempDir,
      HOME: tempDir,
      AFT_STORAGE_DIR: undefined,
      // CI exports AFT_CACHE_DIR for the e2e suites; it outranks the XDG data
      // root in the storage ladder these tests pin.
      AFT_CACHE_DIR: undefined,
    });
  });

  afterEach(() => {
    __setMigrationStatusRunnerForTests(null);
    releaseEnv?.();
    releaseEnv = undefined;
    rmSync(tempDir, { recursive: true, force: true });
    mock.restore();
  });

  function setRunnerResult(result: unknown) {
    const statusRunner = mock(async () => result);
    __setMigrationStatusRunnerForTests(statusRunner as never);
    return statusRunner;
  }

  test("getMigrationStatus_returns_migrated_true_when_marker_exists", async () => {
    const payload = {
      harness: "opencode",
      target_root: join(tempDir, "cortexkit", "aft"),
      migrated: true,
      marker_path: join(tempDir, "cortexkit", "aft", "opencode", ".migrated_from_legacy"),
      migrated_at: "2026-05-19T15:00:00.123Z",
      source_path: "/legacy/aft",
      aft_version: "0.27.0",
    };
    const statusRunner = setRunnerResult({
      status: 0,
      signal: null,
      error: undefined,
      stdout: `${JSON.stringify(payload)}\n`,
      stderr: "",
    });

    await expect(
      getMigrationStatus({ harness: "opencode", binaryPath: "/bin/aft" }),
    ).resolves.toEqual(payload);
    expect(statusRunner).toHaveBeenCalledWith(
      "/bin/aft",
      [
        "migrate-storage",
        "--status",
        "--from",
        join(tempDir, "opencode", "storage", "plugin", "aft"),
        "--to",
        join(tempDir, "cortexkit", "aft"),
        "--harness",
        "opencode",
      ],
      30 * 60 * 1000,
      true,
    );
  });

  test("getMigrationStatus_returns_migrated_false_when_marker_absent", async () => {
    const payload = {
      harness: "opencode",
      target_root: join(tempDir, "cortexkit", "aft"),
      migrated: false,
    };
    setRunnerResult({
      status: 0,
      signal: null,
      error: undefined,
      stdout: `${JSON.stringify(payload)}\n`,
      stderr: "",
    });

    await expect(
      getMigrationStatus({ harness: "opencode", binaryPath: "/bin/aft" }),
    ).resolves.toEqual(payload);
  });

  test("getMigrationStatus_exposes_partial_state_fields", async () => {
    const payload = {
      harness: "opencode",
      target_root: join(tempDir, "cortexkit", "aft"),
      migrated: false,
      source_marker_path: join(
        tempDir,
        "opencode",
        "storage",
        "plugin",
        "aft",
        ".migrated_to_cortexkit",
      ),
      source_marker_present: true,
      partial_state: true,
    };
    setRunnerResult({
      status: 0,
      signal: null,
      error: undefined,
      stdout: `${JSON.stringify(payload)}\n`,
      stderr: "",
    });

    await expect(
      getMigrationStatus({ harness: "opencode", binaryPath: "/bin/aft" }),
    ).resolves.toEqual(payload);
  });

  test("getMigrationStatus_throws_on_invalid_json", async () => {
    setRunnerResult({
      status: 0,
      signal: null,
      error: undefined,
      stdout: "not-json\n",
      stderr: "",
    });

    await expect(
      getMigrationStatus({ harness: "opencode", binaryPath: "/bin/aft" }),
    ).rejects.toThrow(/invalid JSON/);
  });

  test("getMigrationStatus_handles_nonzero_exit", async () => {
    setRunnerResult({
      status: 1,
      signal: null,
      error: undefined,
      stdout: "",
      stderr: "failed",
    });

    await expect(
      getMigrationStatus({ harness: "opencode", binaryPath: "/bin/aft" }),
    ).rejects.toThrow(/exit 1.*failed/);
  });
});
