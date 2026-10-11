import { describe, expect, test } from "bun:test";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  utimesSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { acquireInstallLock as acquireOpenCode } from "../../../opencode-plugin/src/lsp-cache.js";
import { acquireInstallLock as acquirePi } from "../../../pi-plugin/src/lsp-cache.js";
import { clearLspCaches } from "../lib/lsp-cache.js";

async function cacheFixture(fn: (root: string) => void): Promise<void> {
  const root = mkdtempSync(join(tmpdir(), "aft-shared-lsp-lease-"));
  try {
    await withEnv({ AFT_CACHE_DIR: root }, () => fn(root));
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

describe("LSP cache concurrency regressions awaiting protocol design", () => {
  // Design question: how can Pi and OpenCode recover a crashed install owner's
  // lock automatically without stale recovery deleting a live installer's lock?
  test.skip("OpenCode cannot reclaim an aged lease held by a live Pi writer", async () => {
    await cacheFixture((root) => {
      const lock = join(root, "lsp-packages", "fixture", ".aft-installing");
      const pi = acquirePi("fixture");
      expect(pi).not.toBeNull();
      const owner = readFileSync(lock, "utf8");
      const old = new Date(Date.now() - 24 * 60 * 60 * 1000);
      utimesSync(lock, old, old);
      let contender: ReturnType<typeof acquireOpenCode> = null;
      try {
        contender = acquireOpenCode("fixture");
        expect(contender).toBeNull();
        expect(readFileSync(lock, "utf8")).toBe(owner);
      } finally {
        contender?.release();
        pi?.release();
      }
    });
  });

  // Design question: which shared lock outside the deleted cache directories can
  // prevent install/clear overlap and automatically recover after its holder crashes?
  test.skip("external clear reports active entries without deleting a live install", async () => {
    await cacheFixture((root) => {
      const entry = join(root, "lsp-packages", "fixture");
      mkdirSync(entry, { recursive: true });
      writeFileSync(join(entry, "payload"), "keep this install");
      const pi = acquirePi("fixture");
      expect(pi).not.toBeNull();
      try {
        const result = clearLspCaches();
        expect(result.cleared).toEqual([]);
        expect(existsSync(join(entry, "payload"))).toBe(true);
        expect(
          (result as { skipped?: Array<{ path: string }> }).skipped?.some(
            (item) => item.path === entry,
          ),
        ).toBe(true);
      } finally {
        pi?.release();
      }
      expect(clearLspCaches().cleared.some((item) => item.path === entry)).toBe(true);
    });
  });
});
