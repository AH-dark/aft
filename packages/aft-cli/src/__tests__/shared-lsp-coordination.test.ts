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
import { hostname, tmpdir } from "node:os";
import { join } from "node:path";
import { acquireLspCacheClearLease, acquireLspInstallLease } from "@cortexkit/aft-bridge";
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

describe("approved shared cache coordination", () => {
  test("OpenCode cannot reclaim an aged lease held by a live Pi writer", async () => {
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

  test("external clear reports active entries without deleting a live install", async () => {
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

describe("shared lease fail-closed recovery", () => {
  test("a dead installer owner does not authorize stealing from unknown descendants", async () => {
    await cacheFixture((root) => {
      const entry = join(root, "lsp-packages", "fixture");
      mkdirSync(entry, { recursive: true });
      const lock = join(entry, ".aft-installing");
      const owner = "99999999\n2020-01-01T00:00:00Z\nuncertain-token\ninstalling\n";
      writeFileSync(lock, owner);
      const contender = acquireOpenCode("fixture");
      try {
        expect(contender).toBeNull();
        expect(readFileSync(lock, "utf8")).toBe(owner);
      } finally {
        contender?.release();
      }
    });
  });

  test("foreign-host clear ownership is not reclaimed using local PID evidence", async () => {
    await cacheFixture((root) => {
      const state = join(root, "lsp-install-leases");
      mkdirSync(state, { recursive: true });
      writeFileSync(
        join(state, "clear.lock"),
        "99999999\n2020-01-01T00:00:00Z\nforeign-token\nclearing\nforeign-fixture-host\n",
      );
      const entry = join(root, "lsp-packages", "fixture");
      mkdirSync(entry, { recursive: true });
      writeFileSync(join(entry, "payload"), "keep");
      expect(clearLspCaches().cleared).toEqual([]);
      expect(existsSync(join(entry, "payload"))).toBe(true);
    });
  });

  test("lease release cannot erase a replacement ownership token", async () => {
    await cacheFixture((root) => {
      const first = acquirePi("fixture");
      expect(first).not.toBeNull();
      const lock = join(root, "lsp-packages", "fixture", ".aft-installing");
      const replacement = `${process.pid}\n2026-10-10T00:00:00Z\nreplacement-token\n`;
      writeFileSync(lock, replacement);
      first?.release();
      expect(readFileSync(lock, "utf8")).toBe(replacement);
    });
  });
});

describe("shared lease admission ordering", () => {
  test("clear admission cannot interleave installation publication", async () => {
    await cacheFixture((root) => {
      let clearer: ReturnType<typeof acquireLspCacheClearLease> = null;
      const writer = acquireLspInstallLease(
        "fixture",
        join(root, "lsp-packages", "fixture"),
        () => {
          clearer = acquireLspCacheClearLease();
        },
      );
      try {
        expect(writer).not.toBeNull();
        expect(clearer).toBeNull();
      } finally {
        writer?.release();
        clearer?.release();
      }
    });
  });
});

describe("shared lease platform safety", () => {
  test("Windows clear does not treat PID-zero failure as dead-owner proof", async () => {
    await cacheFixture((root) => {
      const state = join(root, "lsp-install-leases");
      mkdirSync(state, { recursive: true });
      writeFileSync(
        join(state, "clear.lock"),
        `99999999\n2020-01-01T00:00:00Z\nwindows-token\nclearing\n${hostname()}\n`,
      );
      const entry = join(root, "lsp-packages", "fixture");
      mkdirSync(entry, { recursive: true });
      writeFileSync(join(entry, "payload"), "keep");
      const descriptor = Object.getOwnPropertyDescriptor(process, "platform");
      Object.defineProperty(process, "platform", { configurable: true, value: "win32" });
      try {
        expect(clearLspCaches().cleared).toEqual([]);
        expect(existsSync(join(entry, "payload"))).toBe(true);
      } finally {
        if (descriptor) Object.defineProperty(process, "platform", descriptor);
      }
    });
  });
});

describe("shared clear enumeration", () => {
  test("ordinary cache-root files do not masquerade as installer leases", async () => {
    await cacheFixture((root) => {
      const packages = join(root, "lsp-packages");
      const entry = join(packages, "fixture");
      mkdirSync(entry, { recursive: true });
      writeFileSync(join(entry, "payload"), "clear");
      writeFileSync(join(packages, ".DS_Store"), "not an install");
      expect(clearLspCaches().cleared.map((item) => item.path)).toEqual([entry]);
      expect(existsSync(join(packages, ".DS_Store"))).toBe(true);
    });
  });
});
