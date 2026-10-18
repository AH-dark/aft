import { afterEach, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __fileDigestWorkForTests, writeStampedFileDigest } from "@cortexkit/aft-bridge";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { writeSmokeBinary } from "../../../aft-bridge/src/__tests__/test-utils/smoke-binary.js";
import { runAutoInstall } from "../lsp-auto-install.js";
import { __githubBinaryHashCountForTests, runGithubAutoInstall } from "../lsp-github-install.js";

let root: string;
let release: (() => void) | undefined;
afterEach(() => {
  release?.();
  if (root) rmSync(root, { recursive: true, force: true });
});

test("stamped npm and GitHub startups hash zero bytes with auto-install off", async () => {
  root = mkdtempSync(join(tmpdir(), "aft-lsp-stamp-"));
  release = await acquireEnv({ AFT_CACHE_DIR: root });
  const npmDir = join(root, "lsp-packages", "pyright");
  const ghDir = join(root, "lsp-binaries", "texlab");
  const npmBin = join(npmDir, "node_modules", ".bin");
  const ghBin = join(ghDir, "bin");
  for (const [dir, bin, version, name] of [
    [npmDir, npmBin, "1.1.300", "pyright-langserver"],
    [ghDir, ghBin, "21.1.0", "texlab"],
  ]) {
    mkdirSync(bin, { recursive: true });
    const path = writeSmokeBinary(join(bin, name));
    const digest = createHash("sha256").update(readFileSync(path)).digest("hex");
    writeFileSync(
      join(dir, ".aft-installed"),
      JSON.stringify({ version, sha256: digest, binarySha256: digest }),
    );
    writeStampedFileDigest(path, digest);
  }
  const before = __fileDigestWorkForTests();
  const ghBefore = __githubBinaryHashCountForTests();
  const config = { autoInstall: false, graceDays: 7, versions: {}, disabled: new Set<string>() };
  for (let i = 0; i < 10; i++) {
    const npm = runAutoInstall(root, config);
    const gh = runGithubAutoInstall(new Set(), config);
    expect(npm.cachedBinDirs).toContain(npmBin);
    expect(npm.getCachedBinDirs()).toContain(npmBin);
    await gh.installsComplete;
    expect(gh.cachedBinDirs).toContain(ghBin);
    expect(await gh.getCachedBinDirs()).toContain(ghBin);
  }
  expect(__githubBinaryHashCountForTests() - ghBefore).toBe(0);
  expect(__fileDigestWorkForTests().syncHashes - before.syncHashes).toBe(0);
  expect(__fileDigestWorkForTests().bytesHashed - before.bytesHashed).toBe(0);
  writeFileSync(
    join(npmBin, process.platform === "win32" ? "pyright-langserver.exe" : "pyright-langserver"),
    "tampered!",
  );
  expect(runAutoInstall(root, config).cachedBinDirs).not.toContain(npmBin);
  expect(__fileDigestWorkForTests().syncHashes - before.syncHashes).toBe(1);
});
