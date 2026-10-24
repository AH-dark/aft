import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { withEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { runAutoInstall } from "../lsp-auto-install.js";
import { NPM_LSP_TABLE } from "../lsp-npm-table.js";

async function recordInstall(pinned: boolean, npmVersion = "11.6.0") {
  const root = mkdtempSync(join(tmpdir(), "aft-npm-argv-"));
  const bin = join(root, "bin");
  const project = join(root, "project");
  const trace = join(root, "argv.jsonl");
  mkdirSync(bin);
  mkdirSync(project);
  writeFileSync(join(project, "main.py"), "pass\n");
  writeFileSync(trace, "");
  const script = join(bin, "npm.mjs");
  const source = `import { appendFileSync } from "node:fs";
const args = process.argv.slice(2);
appendFileSync(${JSON.stringify(trace)}, JSON.stringify(args) + "\\n");
if (args[0] === "--version") console.log(${JSON.stringify(npmVersion)});
`;
  writeFileSync(script, source);
  writeFileSync(
    join(bin, process.platform === "win32" ? "npm.cmd" : "npm"),
    process.platform === "win32"
      ? `@echo off\r\n"${process.execPath}" "${script}" %*\r\n`
      : `#!${process.execPath}\n${source}`,
    { mode: 0o755 },
  );
  try {
    return await withEnv(
      { AFT_CACHE_DIR: join(root, "cache"), PATH: `${bin}${delimiter}${process.env.PATH}` },
      async () => {
        const started = Date.now();
        const result = runAutoInstall(
          project,
          {
            autoInstall: true,
            graceDays: 3,
            versions: pinned ? { pyright: "1.2.3" } : {},
            disabled: new Set(
              NPM_LSP_TABLE.filter((spec) => spec.npm !== "pyright").map((s) => s.id),
            ),
          },
          (async () =>
            new Response(
              JSON.stringify({
                time: { "1.2.3": "2020-01-01T00:00:00Z" },
                "dist-tags": { latest: "1.2.3" },
              }),
            )) as typeof fetch,
        );
        await result.installsComplete;
        const calls = readFileSync(trace, "utf8")
          .trim()
          .split("\n")
          .filter(Boolean)
          .map((line) => JSON.parse(line) as string[]);
        return { calls, started, finished: Date.now() };
      },
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

describe("Pi npm installer policy", () => {
  test("real unpinned install carries the publication cutoff and suppresses scripts", async () => {
    const { calls, started, finished } = await recordInstall(false);
    expect(calls).toHaveLength(2);
    expect(calls[0]).toEqual(["--version"]);
    const install = calls[1];
    const cutoff = install?.[4];
    expect(cutoff).toMatch(/^--before=/);
    const timestamp = Date.parse(cutoff.slice("--before=".length));
    expect(timestamp).toBeGreaterThanOrEqual(started - 3 * 86_400_000);
    expect(timestamp).toBeLessThanOrEqual(finished - 3 * 86_400_000);
    expect(install).toEqual([
      "install",
      "--no-save",
      "--ignore-scripts",
      "--silent",
      cutoff,
      "pyright@1.2.3",
    ]);
  });

  test("real explicit pin bypasses the cutoff and npm version probe", async () => {
    const { calls } = await recordInstall(true, "6.14.0");
    expect(calls).toEqual([
      ["install", "--no-save", "--ignore-scripts", "--silent", "pyright@1.2.3"],
    ]);
  });

  test("real unpinned install refuses npm without cutoff support", async () => {
    const { calls } = await recordInstall(false, "6.14.0");
    expect(calls).toEqual([["--version"]]);
  });
});
