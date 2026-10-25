import { describe, expect, test } from "bun:test";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { linkCachedExecutable } from "../../../aft-bridge/src/__tests__/test-utils/cached-executable.js";
import { getAftBinaryName } from "../lib/paths.js";
import { getSelfVersion } from "../lib/self-version.js";

const commands = ["index", "cache", "backups"] as const;

function temporary<T>(fn: (root: string) => T): T {
  const root = mkdtempSync(join(tmpdir(), "aft-cli-approved-"));
  try {
    return fn(root);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

function binary(root: string, version: string, body: string): void {
  const dir = join(root, "cache", "bin", `v${version}`);
  mkdirSync(dir, { recursive: true });
  linkCachedExecutable(join(dir, getAftBinaryName()), `#!/bin/sh\n${body}\n`);
}

function forwarded(root: string, command: string, versionOverride?: string) {
  const module = join(import.meta.dir, "..", "commands", `${command}.ts`);
  const versionModule = join(import.meta.dir, "..", "lib", "self-version.ts");
  const empty = join(root, "empty");
  mkdirSync(empty, { recursive: true });
  const name = `run${command[0]?.toUpperCase()}${command.slice(1)}`;
  return spawnSync(
    process.execPath,
    [
      "-e",
      `
    import { mock } from 'bun:test';
    ${versionOverride ? `mock.module(${JSON.stringify(versionModule)}, () => ({ getSelfVersion: () => ${JSON.stringify(versionOverride)} }));` : ""}
    const command = await import(${JSON.stringify(module)});
    process.exit(command[${JSON.stringify(name)}](['fixture-argument']));
  `,
    ],
    {
      encoding: "utf8",
      timeout: 5000,
      env: { ...process.env, HOME: root, AFT_CACHE_DIR: join(root, "cache"), PATH: empty },
    },
  );
}

describe("approved native forwarding", () => {
  for (const command of commands) {
    test(`${command} prefers the CLI version rather than the newest cache`, () => {
      temporary((root) => {
        binary(root, getSelfVersion(), 'test "$2" = fixture-argument && exit 7\nexit 9');
        binary(root, "99.0.0", "exit 11");
        expect(forwarded(root, command).status).toBe(7);
      });
    });

    test(`${command} preserves native signal termination`, () => {
      temporary((root) => {
        binary(root, getSelfVersion(), "kill -TERM $$");
        expect(forwarded(root, command).status).toBe(143);
      });
    });

    test(`${command} preserves newest-cache fallback when CLI version is unknown`, () => {
      temporary((root) => {
        binary(root, "0.1.0", "exit 7");
        binary(root, "99.0.0", "exit 11");
        expect(forwarded(root, command, "unknown").status).toBe(11);
      });
    });

    test(`${command} does not silently use a mismatched cached version`, () => {
      temporary((root) => {
        binary(root, "99.0.0", "exit 11");
        const result = forwarded(root, command);
        expect(result.status).toBe(1);
        expect(result.stderr).toContain(`v${getSelfVersion()}`);
      });
    });
  }
});

describe("approved prompt cancellation", () => {
  for (const path of ["shared", "feature-list"]) {
    test(`${path} cancellation exits 130 instead of success`, () => {
      const prompts = join(import.meta.dir, "..", "lib", "prompts.ts");
      const wizard = join(import.meta.dir, "..", "setup", "feature-wizard.ts");
      const featureList = join(import.meta.dir, "..", "setup", "feature-list.ts");
      const result = spawnSync(
        process.execPath,
        [
          "-e",
          `
        import { mock } from 'bun:test';
        mock.module('@clack/prompts', () => ({
          confirm: async () => Symbol('cancel'), text: async () => Symbol('cancel'),
          isCancel: value => typeof value === 'symbol', log: { warn: () => {}, info: () => {} },
          intro: () => {}, note: () => {}, outro: () => {}, spinner: () => {},
          select: async () => Symbol('cancel'), multiselect: async () => Symbol('cancel'),
        }));
        ${
          path === "shared"
            ? `const { confirm } = await import(${JSON.stringify(prompts)}); await confirm('fixture');`
            : `
          mock.module(${JSON.stringify(featureList)}, () => ({ normalizeSelection: values => values, promptFeatureList: async (...args) => args[3]() }));
          const { runFeatureWizard } = await import(${JSON.stringify(wizard)});
          await runFeatureWizard({ features: [], warnings: [] });
        `
        }
      `,
        ],
        { encoding: "utf8", timeout: 5000, env: process.env },
      );
      expect(result.stderr).not.toContain("error:");
      expect(result.status).toBe(130);
    });
  }
});
