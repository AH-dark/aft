import { expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { writeStampedFileDigest } from "../../binary-identity.js";
import { execFileSync } from "../../child-process.js";
import { acquireEnv } from "./env-guard.js";

interface Installer {
  __githubBinaryHashCountForTests: () => number;
  _extractArchiveSafelyForTesting: (
    archive: string,
    destination: string,
    type: "tar.gz",
  ) => Promise<void>;
  runGithubAutoInstall: (
    servers: ReadonlySet<string>,
    config: {
      autoInstall: boolean;
      graceDays: number;
      versions: Readonly<Record<string, string>>;
      disabled: ReadonlySet<string>;
    },
    fetchImpl: typeof fetch,
  ) => {
    cachedBinDirs: string[];
    installsComplete: Promise<void>;
    getCachedBinDirs: () => Promise<string[]>;
  };
}

export function githubLayoutTests(installer: Installer): void {
  const withoutInstall = {
    autoInstall: false,
    graceDays: 7,
    versions: {},
    disabled: new Set<string>(),
  };
  async function withCachedServer(
    script: string,
    action: (bin: string, root: string) => Promise<void>,
  ) {
    const root = mkdtempSync(join(tmpdir(), "aft-smoke-verdict-"));
    const releaseEnv = await acquireEnv({ AFT_CACHE_DIR: root });
    try {
      const binary = join(root, "lsp-binaries", "texlab", "bin", "texlab");
      mkdirSync(dirname(binary), { recursive: true });
      writeFileSync(binary, script, { mode: 0o755 });
      writeFileSync(
        join(dirname(binary), "..", ".aft-installed"),
        JSON.stringify({
          version: "1.0.0",
          binarySha256: createHash("sha256").update(script).digest("hex"),
        }),
      );
      await action(binary, root);
    } finally {
      releaseEnv();
      rmSync(root, { recursive: true, force: true });
    }
  }
  test.skipIf(process.platform === "win32")(
    "recorded smoke verdict exposes the stamped cache synchronously without spawning",
    async () => {
      const script = '#!/bin/sh\nprintf x >> "${0}.runs"\nexit 0\n';
      await withCachedServer(script, async (binary, root) => {
        const caches = [
          { binary, packageDir: dirname(dirname(binary)), layout: "standalone" },
          {
            binary: join(root, "lsp-binaries", "lua-ls", "extracted", "bin", "lua-language-server"),
            packageDir: join(root, "lsp-binaries", "lua-ls"),
            layout: "distribution",
          },
          {
            binary: join(
              root,
              "lsp-binaries",
              "clangd",
              "extracted",
              "clangd_1.0.0",
              "bin",
              "clangd",
            ),
            packageDir: join(root, "lsp-binaries", "clangd"),
            layout: "distribution",
          },
        ];
        const digest = createHash("sha256").update(script).digest("hex");
        for (const cached of caches) {
          mkdirSync(dirname(cached.binary), { recursive: true });
          writeFileSync(cached.binary, script, { mode: 0o755 });
          writeStampedFileDigest(cached.binary, digest);
        }
        for (const verdict of ["passed", "timeout"]) {
          for (const cached of caches) {
            writeFileSync(
              join(cached.packageDir, ".aft-installed"),
              JSON.stringify({
                version: "1.0.0",
                installedAt: "now",
                sha256: digest,
                binarySha256: digest,
                layout: cached.layout,
                smoke: { binarySha256: digest, verdict },
              }),
            );
          }
          const hashes = installer.__githubBinaryHashCountForTests();
          const result = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
          for (const cached of caches) {
            expect(result.cachedBinDirs).toContain(dirname(cached.binary));
            expect(existsSync(`${cached.binary}.runs`)).toBe(false);
          }
          await result.installsComplete;
          const refreshed = await result.getCachedBinDirs();
          for (const cached of caches) {
            expect(refreshed).toContain(dirname(cached.binary));
            expect(existsSync(`${cached.binary}.runs`)).toBe(false);
          }
          expect(installer.__githubBinaryHashCountForTests()).toBe(hashes);
        }
      });
    },
  );
  test.skipIf(process.platform === "win32")(
    "cache without a smoke verdict is published only after asynchronous validation",
    async () => {
      const script = '#!/bin/sh\nprintf x >> "${0}.runs"\nexit 0\n';
      await withCachedServer(script, async (binary) => {
        const digest = createHash("sha256").update(script).digest("hex");
        writeStampedFileDigest(binary, digest);
        writeFileSync(
          join(dirname(binary), "..", ".aft-installed"),
          JSON.stringify({
            version: "1.0.0",
            installedAt: "now",
            sha256: digest,
            binarySha256: digest,
            layout: "standalone",
          }),
        );
        const result = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
        expect(result.cachedBinDirs).not.toContain(dirname(binary));
        await result.installsComplete;
        expect(result.cachedBinDirs).toContain(dirname(binary));
        expect(await result.getCachedBinDirs()).toContain(dirname(binary));
        expect(readFileSync(`${binary}.runs`, "utf8")).toBe("x");
      });
    },
  );
  test.skipIf(process.platform === "win32")(
    "runtime smoke yields to unrelated event-loop work",
    async () => {
      await withCachedServer("#!/bin/sh\nsleep 1\nexit 0\n", async (binary) => {
        let ticks = 0;
        const interval = setInterval(() => {
          ticks += 1;
        }, 10);
        try {
          const result = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
          await result.installsComplete;
          expect(ticks).toBeGreaterThan(2);
          expect(await result.getCachedBinDirs()).toContain(dirname(binary));
        } finally {
          clearInterval(interval);
        }
      });
    },
  );
  test.skipIf(process.platform === "win32")(
    "runtime smoke timeout keeps the cache without quarantine or download",
    async () => {
      await withCachedServer("#!/bin/sh\nexec sleep 60\n", async (binary, root) => {
        let downloads = 0;
        const arch = process.arch === "arm64" ? "aarch64" : "x86_64";
        const platform = process.platform === "darwin" ? "macos" : "linux";
        const asset = `texlab-${arch}-${platform}.tar.gz`;
        const fetchImpl = (async (input: string | URL | Request) => {
          if (String(input).includes("/releases/tags/"))
            return Response.json({
              tag_name: "1.0.0",
              assets: [
                {
                  name: asset,
                  browser_download_url: `https://github.com/latex-lsp/texlab/releases/download/1.0.0/${asset}`,
                },
              ],
            });
          downloads += 1;
          throw new Error("a slow smoke must not force a download");
        }) as typeof fetch;
        const result = installer.runGithubAutoInstall(
          new Set(["texlab"]),
          {
            ...withoutInstall,
            autoInstall: true,
            versions: { "latex-lsp/texlab": "1.0.0" },
          },
          fetchImpl,
        );
        await result.installsComplete;
        expect(await result.getCachedBinDirs()).toContain(dirname(binary));
        expect(existsSync(binary)).toBe(true);
        expect(existsSync(join(root, "lsp-binaries", ".quarantine"))).toBe(false);
        expect(downloads).toBe(0);
        const meta = JSON.parse(
          readFileSync(join(dirname(binary), "..", ".aft-installed"), "utf8"),
        );
        expect(meta.smoke).toEqual({ binarySha256: meta.binarySha256, verdict: "timeout" });
      });
    },
  );
  test.skipIf(process.platform === "win32")(
    "unchanged digest reuses the persisted smoke verdict without spawning",
    async () => {
      const script = '#!/bin/sh\nprintf x >> "${0}.runs"\nexit 0\n';
      await withCachedServer(script, async (binary) => {
        const first = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
        await first.installsComplete;
        expect(readFileSync(`${binary}.runs`, "utf8")).toBe("x");
        const second = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
        await second.installsComplete;
        expect(await second.getCachedBinDirs()).toContain(dirname(binary));
        expect(readFileSync(`${binary}.runs`, "utf8")).toBe("x");
        const metaPath = join(dirname(binary), "..", ".aft-installed");
        const meta = JSON.parse(readFileSync(metaPath, "utf8"));
        expect(meta.smoke).toEqual({ binarySha256: meta.binarySha256, verdict: "passed" });
        const changed = `${script}# changed trusted binary\n`;
        writeFileSync(binary, changed, { mode: 0o755 });
        meta.sha256 = meta.binarySha256 = createHash("sha256").update(changed).digest("hex");
        writeFileSync(metaPath, JSON.stringify(meta));
        const third = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
        await third.installsComplete;
        expect(readFileSync(`${binary}.runs`, "utf8")).toBe("xx");
        const migrated = JSON.parse(readFileSync(metaPath, "utf8"));
        delete migrated.smoke;
        writeFileSync(metaPath, JSON.stringify(migrated));
        const missing = installer.runGithubAutoInstall(new Set(), withoutInstall, fetch);
        await missing.installsComplete;
        expect(readFileSync(`${binary}.runs`, "utf8")).toBe("xxx");
      });
    },
  );
  const realArchive = process.env.AFT_TEST_LUALS_ARCHIVE;
  const hasRealArchive = !!realArchive && existsSync(realArchive) && process.platform !== "win32";
  if (!hasRealArchive)
    console.info(
      "SKIP real LuaLS 3.19.1 archive: no offline host archive at AFT_TEST_LUALS_ARCHIVE (POSIX tar.gz required)",
    );
  test.skipIf(!hasRealArchive)(
    "real offline LuaLS 3.19.1 distribution reports its version",
    async () => {
      const root = mkdtempSync(join(tmpdir(), "aft-real-luals-"));
      try {
        await installer._extractArchiveSafelyForTesting(
          realArchive!,
          join(root, "extracted"),
          "tar.gz",
        );
        expect(
          execFileSync(join(root, "extracted", "bin", "lua-language-server"), ["--version"], {
            encoding: "utf8",
            timeout: 2_000,
          }).trim(),
        ).toBe("3.19.1");
      } finally {
        rmSync(root, { recursive: true, force: true });
      }
    },
  );
  test.skipIf(process.platform === "win32")(
    "cached standalone server rejects definite runtime smoke failure",
    async () => {
      const root = mkdtempSync(join(tmpdir(), "aft-smoke-cache-"));
      const releaseEnv = await acquireEnv({ AFT_CACHE_DIR: root });
      try {
        const binDir = join(root, "lsp-binaries", "texlab", "bin");
        for (const script of ["#!/bin/sh\nexit 1\n"]) {
          mkdirSync(binDir, { recursive: true });
          writeFileSync(join(binDir, "texlab"), script, { mode: 0o755 });
          writeFileSync(
            join(dirname(binDir), ".aft-installed"),
            JSON.stringify({
              version: "1.0.0",
              binarySha256: createHash("sha256").update(script).digest("hex"),
            }),
          );
          const started = Date.now();
          const result = installer.runGithubAutoInstall(
            new Set(),
            { autoInstall: false, graceDays: 7, versions: {}, disabled: new Set() },
            fetch,
          );
          await result.installsComplete;
          expect(await result.getCachedBinDirs()).not.toContain(binDir);
          expect(Date.now() - started).toBeLessThan(4_000);
        }
      } finally {
        releaseEnv();
        rmSync(root, { recursive: true, force: true });
      }
    },
  );
  test.skipIf(process.platform === "win32")(
    "LuaLS distribution runs and repairs a legacy copy-out cache",
    async () => {
      const root = mkdtempSync(join(tmpdir(), "aft-luals-layout-"));
      const releaseEnv = await acquireEnv({ AFT_CACHE_DIR: root });
      try {
        const source = join(root, "fixture", "bin");
        mkdirSync(source, { recursive: true });
        const script = '#!/bin/sh\ncat "$(dirname "$0")/main.lua"\n';
        writeFileSync(join(source, "lua-language-server"), script, { mode: 0o755 });
        writeFileSync(join(source, "main.lua"), "3.19.1\n");
        const archive = join(root, "fixture.tar.gz");
        execFileSync("tar", ["-czf", archive, "-C", dirname(source), "bin"]);
        const asset = `lua-language-server-3.19.1-${process.platform}-${process.arch}.tar.gz`;
        let downloads = 0;
        const fetchImpl = (async (input: string | URL | Request) => {
          if (String(input).includes("/releases/tags/")) {
            return Response.json({
              tag_name: "3.19.1",
              assets: [
                {
                  name: asset,
                  browser_download_url: `https://github.com/LuaLS/lua-language-server/releases/download/3.19.1/${asset}`,
                },
              ],
            });
          }
          downloads += 1;
          return new Response(readFileSync(archive));
        }) as typeof fetch;
        const config = {
          autoInstall: true,
          graceDays: 7,
          versions: { "LuaLS/lua-language-server": "3.19.1" },
          disabled: new Set<string>(),
        };
        const pkg = join(root, "lsp-binaries", "lua-ls");
        const oldBin = join(pkg, "bin", "lua-language-server");
        mkdirSync(dirname(oldBin), { recursive: true });
        writeFileSync(oldBin, script, { mode: 0o755 });
        const digest = createHash("sha256").update(script).digest("hex");
        writeFileSync(
          join(pkg, ".aft-installed"),
          JSON.stringify({
            version: "3.19.1",
            sha256: digest,
            binarySha256: digest,
          }),
        );

        const result = installer.runGithubAutoInstall(new Set(["lua-ls"]), config, fetchImpl);
        expect(result.cachedBinDirs).not.toContain(dirname(oldBin));
        await result.installsComplete;
        const realBin = join(pkg, "extracted", "bin");
        expect(await result.getCachedBinDirs()).toContain(realBin);
        expect(
          execFileSync(join(realBin, "lua-language-server"), ["--version"], {
            encoding: "utf8",
          }).trim(),
        ).toBe("3.19.1");
        expect(existsSync(oldBin)).toBe(false);
        expect(downloads).toBe(1);
        const cached = installer.runGithubAutoInstall(new Set(["lua-ls"]), config, fetchImpl);
        await cached.installsComplete;
        expect(cached.cachedBinDirs).toContain(realBin);
        expect(downloads).toBe(1);

        rmSync(join(realBin, "main.lua"));
        // Simulate a pre-verdict cache: runtime resources are re-probed when no verdict exists.
        const metaPath = join(pkg, ".aft-installed");
        const meta = JSON.parse(readFileSync(metaPath, "utf8"));
        delete meta.smoke;
        writeFileSync(metaPath, JSON.stringify(meta));
        const damaged = installer.runGithubAutoInstall(new Set(["lua-ls"]), config, fetchImpl);
        expect(damaged.cachedBinDirs).not.toContain(realBin);
        await damaged.installsComplete;
        expect(await damaged.getCachedBinDirs()).toContain(realBin);
        expect(downloads).toBe(2);
      } finally {
        releaseEnv();
        rmSync(root, { recursive: true, force: true });
      }
    },
  );
}
