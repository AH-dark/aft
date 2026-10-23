/// <reference path="../bun-test.d.ts" />

/**
 * The OpenCode `chat.message` hook needs the current config on every message.
 * A full load reads and parses both config files twice and validates the
 * result, so the hook reuses the last load while neither file has changed.
 */

import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { acquireEnv } from "../../../aft-bridge/src/__tests__/test-utils/env-guard.js";
import { createProjectAcceptance } from "../bridge-bootstrap.js";
import {
  type AftConfig,
  ConfigRejectedError,
  createUnchangedConfigLoader,
  loadAftConfig,
} from "../config.js";

const cleanup: (() => void)[] = [];

afterEach(() => {
  for (const fn of cleanup.splice(0).reverse()) fn();
});

async function fixture(user: string, project: string) {
  const root = mkdtempSync(join(tmpdir(), "aft-oc-unchanged-config-"));
  const xdg = join(root, "xdg");
  const projectDir = join(root, "project");
  mkdirSync(join(xdg, "cortexkit"), { recursive: true });
  mkdirSync(join(projectDir, ".cortexkit"), { recursive: true });
  const userPath = join(xdg, "cortexkit", "aft.jsonc");
  const projectPath = join(projectDir, ".cortexkit", "aft.jsonc");
  writeFileSync(userPath, user);
  writeFileSync(projectPath, project);
  const releaseEnv = await acquireEnv({ HOME: join(root, "home"), XDG_CONFIG_HOME: xdg });
  cleanup.push(() => rmSync(root, { recursive: true, force: true }));
  cleanup.push(releaseEnv);
  return { projectDir, userPath, projectPath };
}

function countingLoader() {
  let loads = 0;
  const load = (projectDirectory: string): AftConfig => {
    loads += 1;
    return loadAftConfig(projectDirectory);
  };
  return { load: createUnchangedConfigLoader(load), loads: () => loads };
}

describe("createUnchangedConfigLoader", () => {
  test("project acceptance revalidates boot roots and aliases after config edits", async () => {
    const { projectDir, projectPath, userPath } = await fixture("{}", "{}");
    let loads = 0;
    let reject = false;
    const accepts = createProjectAcceptance(projectDir, {
      loadConfig: () => {
        loads++;
        if (reject) throw new ConfigRejectedError(["invalid"]);
        return {} as AftConfig;
      },
    });
    expect(accepts(projectDir)).toBe(true);
    reject = true;
    writeFileSync(projectPath, '{"changed": true}');
    expect(accepts(projectDir)).toBe(false);
    expect(accepts(`${projectDir}/.`)).toBe(false);
    expect(loads).toBe(1);
    reject = false;
    writeFileSync(userPath, '{"changed": "again"}');
    expect(accepts(projectDir)).toBe(true);
    expect(loads).toBe(2);
  });

  test("unchanged config rejections reuse the error without reparsing and recover on edits", async () => {
    const { projectDir, projectPath } = await fixture("{}", "{}");
    let calls = 0;
    let reject = true;
    const load = createUnchangedConfigLoader(() => {
      calls++;
      if (reject) throw new ConfigRejectedError(["invalid"]);
      return {} as AftConfig;
    });
    const errors: unknown[] = [];
    for (let i = 0; i < 4; i++) {
      try {
        load(projectDir);
      } catch (error) {
        errors.push(error);
      }
    }
    expect(calls).toBe(1);
    expect(errors).toHaveLength(4);
    expect(errors.every((error) => error === errors[0])).toBe(true);
    reject = false;
    writeFileSync(projectPath, '{"changed": true}');
    expect(load(projectDir)).toEqual({});
    expect(calls).toBe(2);
  });

  test("repeated loads with unchanged files run the full load once", async () => {
    const { projectDir } = await fixture(
      '{ "bash": { "wait_detach_on_user_message": false } }',
      "{}",
    );
    const loader = countingLoader();

    const first = loader.load(projectDir);
    for (let i = 0; i < 9; i++) expect(loader.load(projectDir)).toBe(first);

    expect(loader.loads()).toBe(1);
    expect(first).toEqual(loadAftConfig(projectDir));
  });

  test("an edit, a new file or a deleted file triggers a fresh load", async () => {
    const { projectDir, userPath, projectPath } = await fixture("{}", "{}");
    const loader = countingLoader();
    loader.load(projectDir);

    writeFileSync(projectPath, '{ "bash": { "wait_detach_on_user_message": false } }');
    const edited = loader.load(projectDir);
    expect(loader.loads()).toBe(2);
    expect(edited).toEqual(loadAftConfig(projectDir));

    unlinkSync(userPath);
    loader.load(projectDir);
    expect(loader.loads()).toBe(3);

    writeFileSync(userPath, "{}");
    loader.load(projectDir);
    expect(loader.loads()).toBe(4);

    loader.load(projectDir);
    expect(loader.loads()).toBe(4);
  });

  test("a load that throws is not remembered", async () => {
    const { projectDir } = await fixture("{}", "{}");
    let calls = 0;
    const load = createUnchangedConfigLoader((): AftConfig => {
      calls += 1;
      throw new Error("rejected");
    });

    expect(() => load(projectDir)).toThrow("rejected");
    expect(() => load(projectDir)).toThrow("rejected");
    expect(calls).toBe(2);
  });
});
