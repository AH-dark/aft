import { describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const packageRoot = join(import.meta.dir, "../..");

describe("V2 bootstrap fixture isolation", () => {
  for (const invalid of [false, true]) {
    test(`fake boots ignore ambient parser state: ${invalid ? "malformed config" : "missing config"}`, () => {
      const root = mkdtempSync(join(tmpdir(), "aft-v2-bootstrap-isolation-"));
      try {
        const project = join(root, "project");
        mkdirSync(join(project, ".cortexkit"), { recursive: true });
        if (invalid) writeFileSync(join(project, ".cortexkit/aft.jsonc"), "{");
        // Use a fresh child with private HOME/XDG directories, then preload a
        // real config load to test whether its retained errors contaminate fake boots.
        const result = spawnSync(
          process.execPath,
          [
            "test",
            "--preload",
            "./test/fixtures/bootstrap-config-state.ts",
            "./test/entry/server-effect.test.ts",
            "./test/tool-surface/v2-behavior.test.ts",
          ],
          {
            cwd: packageRoot,
            env: {
              PATH: process.env.PATH,
              HOME: join(root, "home"),
              XDG_CONFIG_HOME: join(root, "config"),
              XDG_CACHE_HOME: join(root, "cache"),
              XDG_DATA_HOME: join(root, "data"),
              XDG_STATE_HOME: join(root, "state"),
              AFT_BOOTSTRAP_FIXTURE_PROJECT: project,
              AFT_BOOTSTRAP_FIXTURE_INVALID: invalid ? "1" : "0",
            },
            encoding: "utf8",
            timeout: 20_000,
          },
        );
        expect(result.error).toBeUndefined();
        expect(`${result.stdout}\n${result.stderr}`).toContain("11 pass");
        expect(result.status).toBe(0);
      } finally {
        rmSync(root, { recursive: true, force: true });
      }
    });
  }
});
