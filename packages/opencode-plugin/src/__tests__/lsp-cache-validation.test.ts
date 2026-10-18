import { afterEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { writeSmokeBinary } from "../../../aft-bridge/src/__tests__/test-utils/smoke-binary.js";
import { runAutoInstall } from "../lsp-auto-install.js";
import { __githubBinaryHashCountForTests, runGithubAutoInstall } from "../lsp-github-install.js";

const roots = new Set<string>();
let releaseEnv: (() => void) | undefined;

async function tempCache(): Promise<string> {
  const root = mkdtempSync(join(tmpdir(), "aft-lsp-cache-validation-"));
  roots.add(root);
  releaseEnv = await acquireEnv({ AFT_CACHE_DIR: root });
  return root;
}

afterEach(() => {
  releaseEnv?.();
  releaseEnv = undefined;
  for (const root of roots) rmSync(root, { recursive: true, force: true });
  roots.clear();
});

describe("cached LSP validation before lsp_paths_extra", () => {
  test("npm cached binary with mismatched sha is excluded and quarantined", async () => {
    const root = await tempCache();
    const pkgDir = join(root, "lsp-packages", "pyright");
    const binDir = join(pkgDir, "node_modules", ".bin");
    mkdirSync(binDir, { recursive: true });
    // pyright's binary in NPM_LSP_TABLE is `pyright-langserver` (the LSP),
    // not the `pyright` CLI. isInstalled only fires when the actual spec.binary
    // exists, so write that exact name.
    writeFileSync(join(binDir, "pyright-langserver"), "tampered");
    writeFileSync(
      join(pkgDir, ".aft-installed"),
      JSON.stringify({ version: "1.1.300", installedAt: "now", sha256: "0".repeat(64) }),
    );

    const result = runAutoInstall(root, {
      autoInstall: false,
      graceDays: 7,
      versions: {},
      disabled: new Set(),
    });

    expect(result.cachedBinDirs).not.toContain(binDir);
    const quarantine = join(root, "lsp-packages", ".quarantine", "pyright");
    expect(existsSync(quarantine)).toBe(true);
    expect(readdirSync(quarantine).length).toBeGreaterThan(0);
  });

  test("GitHub cached binary with mismatched sha is excluded and quarantined", async () => {
    const root = await tempCache();
    const pkgDir = join(root, "lsp-binaries", "clangd");
    const binDir = join(pkgDir, "bin");
    mkdirSync(binDir, { recursive: true });
    writeFileSync(join(binDir, "clangd"), "tampered");
    // Use binarySha256 (v0.27 metadata shape). Legacy sha256-only entries
    // trigger a TOFU migration that accepts the current binary hash, so a
    // tampered binary with a stale sha256 alone would no longer quarantine.
    // binarySha256 is the authoritative field and must match exactly.
    writeFileSync(
      join(pkgDir, ".aft-installed"),
      JSON.stringify({
        version: "21.1.0",
        installedAt: "now",
        binarySha256: "0".repeat(64),
      }),
    );

    const result = runGithubAutoInstall(new Set(), {
      autoInstall: false,
      graceDays: 7,
      versions: {},
      disabled: new Set(),
    });

    await result.installsComplete;
    expect(result.cachedBinDirs).not.toContain(binDir);
    const quarantine = join(root, "lsp-binaries", ".quarantine", "clangd");
    expect(existsSync(quarantine)).toBe(true);
    expect(readdirSync(quarantine).length).toBeGreaterThan(0);
  });

  // Startup validates each cached binary, and the refresh after background
  // installs settle validates them all again. A binary is tens to hundreds of
  // MB, so an unchanged one must be hashed once per process, not per check.
  test("an unchanged GitHub cached binary is hashed once across repeated validation", async () => {
    const root = await tempCache();
    const pkgDir = join(root, "lsp-binaries", "texlab");
    const binDir = join(pkgDir, "bin");
    mkdirSync(binDir, { recursive: true });
    const binary = writeSmokeBinary(join(binDir, "texlab"));
    writeFileSync(
      join(pkgDir, ".aft-installed"),
      JSON.stringify({
        version: "21.1.0",
        installedAt: "now",
        binarySha256: createHash("sha256").update(readFileSync(binary)).digest("hex"),
      }),
    );

    const before = __githubBinaryHashCountForTests();
    const result = runGithubAutoInstall(new Set(), {
      autoInstall: false,
      graceDays: 7,
      versions: {},
      disabled: new Set(),
    });
    await result.installsComplete;
    expect(result.cachedBinDirs).toContain(binDir);
    expect(await result.getCachedBinDirs()).toContain(binDir);
    expect(await result.getCachedBinDirs()).toContain(binDir);
    expect(__githubBinaryHashCountForTests() - before).toBe(1);

    // A changed binary is hashed again and caught by the recorded digest.
    writeFileSync(binary, "tampered-binary");
    expect(await result.getCachedBinDirs()).not.toContain(binDir);
    expect(__githubBinaryHashCountForTests() - before).toBe(2);
    expect(existsSync(join(root, "lsp-binaries", ".quarantine", "texlab"))).toBe(true);
  });
});
