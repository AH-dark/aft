// Unit checks for how seed-opencode-config-deps.mjs starts npm.
// Run with: node --test tests/docker/seed-opencode-config-deps.test.mjs
//
// The Windows plan is checked on any host: npmSpawnPlan takes the platform,
// environment, node path and file-existence check as inputs. The execution
// test runs that same plan here, without a shell, and checks that a path
// containing spaces and a double quote reaches npm as one argument.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { npmSpawnPlan } from "./seed-opencode-config-deps.mjs";

const NODE = "C:\\Program Files\\nodejs\\node.exe";
const BUNDLED_CLI = "C:\\Program Files\\nodejs\\node_modules\\npm\\bin\\npm-cli.js";
const STAGE = 'C:\\Users\\First "Q" Last\\AppData\\Local\\Temp\\opencode-config-deps-ab cd';
const INSTALL_ARGS = ["install", "--prefix", STAGE, "--no-audit", "--no-fund", "@opencode-ai/plugin@1.19.3"];

const existsOnly =
  (...paths) =>
  (candidate) =>
    paths.includes(candidate);

test("Windows: runs npm-cli.js next to node.exe with node, no shell, arguments unchanged", () => {
  const plan = npmSpawnPlan(INSTALL_ARGS, { platform: "win32", env: {}, execPath: NODE, exists: existsOnly(BUNDLED_CLI) });
  assert.deepEqual(plan, { command: NODE, args: [BUNDLED_CLI, ...INSTALL_ARGS], shell: false });
});

test("Windows: prefers npm_execpath when it names npm-cli.js", () => {
  const cli = "D:\\tools\\npm 11\\bin\\npm-cli.js";
  const plan = npmSpawnPlan(INSTALL_ARGS, {
    platform: "win32",
    env: { npm_execpath: cli },
    execPath: NODE,
    exists: existsOnly(cli, BUNDLED_CLI),
  });
  assert.deepEqual(plan, { command: NODE, args: [cli, ...INSTALL_ARGS], shell: false });
});

test("Windows: ignores an npm_execpath that is another package manager", () => {
  const plan = npmSpawnPlan(INSTALL_ARGS, {
    platform: "win32",
    env: { npm_execpath: "C:\\pnpm\\bin\\pnpm.cjs" },
    execPath: NODE,
    exists: () => true,
  });
  assert.equal(plan.args[0], BUNDLED_CLI);
});

test("Windows: reports where it looked when npm-cli.js is missing", () => {
  const plan = npmSpawnPlan(INSTALL_ARGS, { platform: "win32", env: {}, execPath: NODE, exists: () => false });
  assert.match(plan.error, /npm-cli\.js not found/);
  assert.ok(plan.error.includes(BUNDLED_CLI));
});

test("Linux and macOS: runs npm from PATH without a shell", () => {
  const plan = npmSpawnPlan(INSTALL_ARGS, { platform: "linux", env: {} });
  assert.deepEqual(plan, { command: "npm", args: INSTALL_ARGS, shell: false });
});

test("the Windows plan delivers a path with spaces and a quote as one argument", () => {
  const root = mkdtempSync(join(tmpdir(), "seed plan test "));
  try {
    // A stand-in npm-cli.js that prints the arguments it received.
    const binDir = join(root, "npm 11", "bin");
    mkdirSync(binDir, { recursive: true });
    const fakeCli = join(binDir, "npm-cli.js");
    writeFileSync(fakeCli, "process.stdout.write(JSON.stringify(process.argv.slice(2)));\n");
    const args = ["install", "--prefix", join(root, 'stage "q" dir'), "--no-audit"];

    const plan = npmSpawnPlan(args, {
      platform: "win32",
      env: { npm_execpath: fakeCli },
      execPath: process.execPath,
    });
    // Checked before spawning, so a wrong plan fails here instead of running a real npm.
    assert.equal(plan.command, process.execPath);
    assert.equal(plan.shell, false);
    const result = spawnSync(plan.command, plan.args, { encoding: "utf8", shell: plan.shell });
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(JSON.parse(result.stdout), args);

    // The same arguments through a shell do not arrive intact, which is the
    // failure the shell-free plan exists to avoid.
    const viaShell = spawnSync(process.execPath, [fakeCli, ...args], { encoding: "utf8", shell: true });
    assert.notDeepEqual(viaShell.status === 0 ? JSON.parse(viaShell.stdout) : undefined, args);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
