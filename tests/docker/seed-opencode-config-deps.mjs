#!/usr/bin/env node
/**
 * Pre-installs the dependency OpenCode 1.x puts in every config directory, so
 * the first `opencode run` of an e2e job never contacts the npm registry.
 *
 * On every start, OpenCode 1.x checks each config directory it reads: its
 * global config directory, `~/.opencode` when that directory exists, and
 * `OPENCODE_CONFIG_DIR`. A directory is already set up when it has
 * `node_modules` and its package-lock.json root lists every dependency in
 * package.json plus `@opencode-ai/plugin`. Otherwise OpenCode installs
 * `@opencode-ai/plugin@<its own version>` there from the npm registry, and it
 * loads no external plugin, including AFT, until those installs finish. When
 * the registry or its audit service stalls, the first launch prints nothing
 * and runs past its time budget. This script leaves each directory set up.
 * The OpenCode 2 harness does the same with seedHostConfigDependencies in
 * tests/docker/opencode2/harness/isolation.ts.
 *
 * The version comes from `opencode --version`, which reports the same version
 * OpenCode installs, so an OpenCode upgrade cannot leave the seeded package
 * behind. The install runs with --no-audit --no-fund and gets three attempts
 * with growing pauses between them. If every attempt fails, the script exits
 * non-zero and names the registry, so the setup step fails visibly instead of
 * a later launch hanging.
 *
 * Usage: node seed-opencode-config-deps.mjs [extra-config-dir ...]
 *   OPENCODE_BIN  OpenCode command to ask for its version (default: opencode)
 */
import { spawnSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join, resolve, win32 } from "node:path";
import { fileURLToPath } from "node:url";

const PACKAGE = "@opencode-ai/plugin";
const ATTEMPTS = 3;
const BACKOFF_SECONDS = [5, 15];
const ATTEMPT_TIMEOUT_MS = 180_000;
const isWindows = process.platform === "win32";

function fail(message) {
  console.error(`seed-opencode-config-deps: ${message}`);
  process.exit(1);
}

/**
 * How to start npm with `args`, without a shell on any platform.
 *
 * On Windows, `npm` on PATH is a .cmd shim, and a .cmd file only starts
 * through cmd.exe. With `shell: true`, Node joins the arguments into one
 * cmd.exe command line, so a path containing a space (a staging directory
 * under `C:\Users\First Last\...`) would split into two arguments. Instead,
 * run npm's own JavaScript entry point with the Node binary directly, so each
 * argument reaches npm intact. The entry point is `npm_execpath` when that
 * names npm-cli.js (set when running under npm), otherwise the npm-cli.js the
 * Windows Node installer places next to node.exe.
 *
 * Returns `{ command, args, shell }`, or `{ error }` when npm-cli.js is missing.
 * Elsewhere `npm` is an executable on PATH and runs without a shell.
 */
export function npmSpawnPlan(
  args,
  { platform = process.platform, env = process.env, execPath = process.execPath, exists = existsSync } = {},
) {
  if (platform !== "win32") return { command: "npm", args: [...args], shell: false };
  const candidates = [];
  if (env.npm_execpath && win32.basename(env.npm_execpath).toLowerCase() === "npm-cli.js") {
    candidates.push(env.npm_execpath);
  }
  candidates.push(win32.join(win32.dirname(execPath), "node_modules", "npm", "bin", "npm-cli.js"));
  const cli = candidates.find((candidate) => exists(candidate));
  if (!cli) return { error: `npm-cli.js not found (looked in: ${candidates.join(", ")})` };
  return { command: execPath, args: [cli, ...args], shell: false };
}

function runNpm(args, options = {}) {
  const plan = npmSpawnPlan(args);
  if (plan.error) fail(`cannot start npm: ${plan.error}`);
  return spawnSync(plan.command, plan.args, { encoding: "utf8", shell: plan.shell, ...options });
}

function runOpencodeVersion(opencodeBin) {
  // The opencode command on Windows is a .cmd shim, which needs cmd.exe. The
  // only argument is the fixed `--version`, so cmd.exe's argument joining
  // cannot split anything.
  return spawnSync(opencodeBin, ["--version"], { encoding: "utf8", shell: isWindows });
}

function readJson(path) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return undefined;
  }
}

function dependencyNames(manifest) {
  const names = [];
  for (const field of ["dependencies", "devDependencies", "peerDependencies", "optionalDependencies"]) {
    const deps = manifest?.[field];
    if (deps && typeof deps === "object") names.push(...Object.keys(deps));
  }
  return names;
}

// The same test OpenCode applies before deciding to install (Npm.install in
// packages/core/src/npm.ts at opencode v1.19.3).
function opencodeWouldSkipInstall(dir) {
  if (!existsSync(join(dir, "node_modules"))) return false;
  const declared = new Set([...dependencyNames(readJson(join(dir, "package.json"))), PACKAGE]);
  const lockRoot = readJson(join(dir, "package-lock.json"))?.packages?.[""];
  const locked = new Set(dependencyNames(lockRoot));
  return [...declared].every((name) => locked.has(name));
}

function sleepSeconds(seconds) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, seconds * 1000);
}

function main() {
  // OpenCode's version, which is the version of the package it installs.
  const opencodeBin = process.env.OPENCODE_BIN || "opencode";
  const versionRun = runOpencodeVersion(opencodeBin);
  const version = (versionRun.stdout ?? "").trim().split(/\s+/).pop() ?? "";
  if (versionRun.status !== 0 || !/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(version)) {
    fail(
      `could not read the OpenCode version from \`${opencodeBin} --version\` ` +
        `(exit ${versionRun.status}, stdout ${JSON.stringify(versionRun.stdout)}, stderr ${JSON.stringify(versionRun.stderr)})`,
    );
  }
  const spec = `${PACKAGE}@${version}`;

  // The config directories OpenCode 1.x reads, following ConfigPaths.directories
  // in packages/opencode/src/config/paths.ts. The global directory comes from
  // xdg-basedir (XDG_CONFIG_HOME, else ~/.config, on every platform). OpenCode
  // creates it at startup, so it is created here too. `~/.opencode` only counts
  // when it exists; the curl installer creates it to hold the binary.
  const home = homedir();
  const dirs = new Set([join(process.env.XDG_CONFIG_HOME || join(home, ".config"), "opencode")]);
  if (existsSync(join(home, ".opencode"))) dirs.add(join(home, ".opencode"));
  if (process.env.OPENCODE_CONFIG_DIR) dirs.add(process.env.OPENCODE_CONFIG_DIR);
  for (const extra of process.argv.slice(2)) dirs.add(extra);
  const targets = [...dirs].map((dir) => resolve(dir));

  const pending = [];
  for (const dir of targets) {
    mkdirSync(dir, { recursive: true });
    if (opencodeWouldSkipInstall(dir)) {
      console.log(`seed-opencode-config-deps: ${dir} already has its dependencies installed; leaving it unchanged`);
      continue;
    }
    const others = dependencyNames(readJson(join(dir, "package.json"))).filter((name) => name !== PACKAGE);
    if (others.length > 0) {
      fail(`${dir}/package.json already declares ${others.join(", ")}; refusing to replace it`);
    }
    pending.push(dir);
  }
  if (pending.length === 0) return;

  const registryRun = runNpm(["config", "get", "registry"]);
  const registry = (registryRun.stdout ?? "").trim() || "https://registry.npmjs.org/";

  // Install once into a staging directory, then copy the result into each target.
  const stage = mkdtempSync(join(tmpdir(), "opencode-config-deps-"));
  let installed = false;
  for (let attempt = 1; attempt <= ATTEMPTS; attempt++) {
    rmSync(join(stage, "node_modules"), { recursive: true, force: true });
    console.log(
      `seed-opencode-config-deps: installing ${spec} from ${registry} into ${stage} (attempt ${attempt}/${ATTEMPTS})`,
    );
    const install = runNpm(
      ["install", "--prefix", stage, "--no-audit", "--no-fund", "--save-exact", "--ignore-scripts", spec],
      { stdio: "inherit", timeout: ATTEMPT_TIMEOUT_MS },
    );
    if (install.status === 0 && existsSync(join(stage, "node_modules", PACKAGE, "package.json"))) {
      installed = true;
      break;
    }
    const reason = install.error ? install.error.message : `exit ${install.status}`;
    console.error(`seed-opencode-config-deps: attempt ${attempt} failed (${reason})`);
    if (attempt < ATTEMPTS) sleepSeconds(BACKOFF_SECONDS[attempt - 1]);
  }
  if (!installed) {
    fail(`could not install ${spec} from the npm registry ${registry} after ${ATTEMPTS} attempts`);
  }

  const installedVersion = readJson(join(stage, "node_modules", PACKAGE, "package.json"))?.version;
  if (installedVersion !== version) {
    fail(`npm installed ${PACKAGE}@${installedVersion}, but OpenCode ${version} installs ${spec}`);
  }

  for (const dir of pending) {
    rmSync(join(dir, "node_modules"), { recursive: true, force: true });
    for (const name of ["package.json", "package-lock.json", "node_modules"]) {
      cpSync(join(stage, name), join(dir, name), { recursive: true, verbatimSymlinks: true });
    }
    if (!opencodeWouldSkipInstall(dir)) {
      fail(`${dir} was seeded but still does not meet OpenCode's skip-install check`);
    }
    console.log(`seed-opencode-config-deps: seeded ${dir} with ${spec}`);
  }
  rmSync(stage, { recursive: true, force: true });
}

// Run only when started as a script, so tests can import npmSpawnPlan.
// (import.meta.main is not available on every Node version CI runners ship.)
function startedAsScript() {
  try {
    return realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url));
  } catch {
    return false;
  }
}
if (startedAsScript()) main();
