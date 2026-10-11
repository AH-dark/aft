import { describe, expect, spyOn, test } from "bun:test";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { runAutoInstall } from "../lsp-auto-install.js";

async function installFixture(npmVersion: string, pinned: boolean): Promise<string | null> {
  const root = mkdtempSync(join(tmpdir(), "aft-npm-cutoff-"));
  const argv = join(root, "install-args");
  const bin = join(root, "bin");
  mkdirSync(bin);
  writeFileSync(
    join(bin, "npm"),
    `#!/bin/sh
if [ "$1" = --version ]; then printf '${npmVersion}\\n'; exit 0; fi
printf '%s\\n' "$@" > '${argv}'
/bin/mkdir -p node_modules/.bin
printf 'fixture binary' > node_modules/.bin/pyright-langserver
`,
    { mode: 0o755 },
  );
  writeFileSync(join(root, "main.py"), "print('fixture')");
  const clock = spyOn(Date, "now").mockReturnValue(Date.parse("2026-10-10T00:00:00Z"));
  try {
    return await withEnv(
      { AFT_CACHE_DIR: join(root, "cache"), PATH: `${bin}:${process.env.PATH}` },
      async () => {
        const fakeFetch = (async () =>
          new Response(
            JSON.stringify({
              versions: { "1.1.300": {} },
              time: { "1.1.300": "2020-01-01T00:00:00Z" },
              "dist-tags": { latest: "1.1.300" },
            }),
          )) as typeof fetch;
        const result = runAutoInstall(
          root,
          {
            autoInstall: true,
            graceDays: 7,
            versions: pinned ? { pyright: "1.1.300" } : {},
            disabled: new Set(),
          },
          fakeFetch,
        );
        await result.installsComplete;
        return existsSync(argv) ? readFileSync(argv, "utf8") : null;
      },
    );
  } finally {
    clock.mockRestore();
    rmSync(root, { recursive: true, force: true });
  }
}

describe("approved npm publication grace", () => {
  test("unpinned installs pass the grace cutoff to dependency resolution", async () => {
    const args = await installFixture("10.9.0", false);
    expect(args).toContain("--before=2026-10-03T00:00:00.000Z");
    expect(args).toContain("--ignore-scripts");
    expect(args).toContain("pyright@1.1.300");
  });
  test("unsupported npm fails closed before an unpinned install", async () => {
    expect(await installFixture("6.14.18", false)).toBeNull();
  });
  test("explicit pins retain their existing grace exemption", async () => {
    const args = await installFixture("6.14.18", true);
    expect(args).toContain("pyright@1.1.300");
    expect(args).toContain("--ignore-scripts");
    expect(args).not.toContain("--before");
  });
});
