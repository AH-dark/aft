import { expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { execFileSync } from "../../child-process.js";
import { acquireEnv } from "./env-guard.js";

interface Installer {
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
    getCachedBinDirs: () => string[];
  };
}

export function githubLayoutTests(installer: Installer): void {
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
    "cached standalone server must pass a bounded runtime smoke check",
    async () => {
      const root = mkdtempSync(join(tmpdir(), "aft-smoke-cache-"));
      const releaseEnv = await acquireEnv({ AFT_CACHE_DIR: root });
      try {
        const binDir = join(root, "lsp-binaries", "texlab", "bin");
        for (const script of ["#!/bin/sh\nexit 1\n", "#!/bin/sh\nexec sleep 60\n"]) {
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
          expect(result.cachedBinDirs).not.toContain(binDir);
          expect(Date.now() - started).toBeLessThan(4_000);
          await result.installsComplete;
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
        expect(result.getCachedBinDirs()).toContain(realBin);
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
        const damaged = installer.runGithubAutoInstall(new Set(["lua-ls"]), config, fetchImpl);
        expect(damaged.cachedBinDirs).not.toContain(realBin);
        await damaged.installsComplete;
        expect(damaged.getCachedBinDirs()).toContain(realBin);
        expect(downloads).toBe(2);
      } finally {
        releaseEnv();
        rmSync(root, { recursive: true, force: true });
      }
    },
  );
}
