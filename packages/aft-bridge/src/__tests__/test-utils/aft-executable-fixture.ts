import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  existsSync,
  linkSync,
  lstatSync,
  mkdirSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { delimiter, dirname, join } from "node:path";
import { cachedExecutable } from "./cached-executable.js";

export interface AftFixtureBehavior {
  stdout?: string;
  stderr?: string;
  exitCode?: number;
  sleepMs?: number;
}

const compilerSearchPath = process.env.PATH ?? "";
const nativeCompiler = process.platform === "win32" ? null : resolveNativeCompiler();

export function writeAftFixture(path: string, behavior: AftFixtureBehavior): string {
  mkdirSync(dirname(path), { recursive: true });

  if (process.platform === "win32") {
    writeShellFixture(path, behavior);
  } else {
    writeNativeFixture(path, behavior);
  }

  return path;
}

export function writeAftVersionFixture(path: string, version: string): string {
  return writeAftFixture(path, { stdout: `aft ${version}\n` });
}

function writeNativeFixture(path: string, behavior: AftFixtureBehavior): void {
  const source = nativeFixtureSource(behavior);

  // Compiling with cc at test time is the slow, flaky step on contended CI
  // runners (a single compile can blow a 5s test budget). Identical behaviors
  // produce identical sources, so compile each source once per machine into a
  // content-keyed cache and link to it for every later request.
  const cacheKey = createHash("sha256")
    .update(`${process.platform}\0${source}`)
    .digest("hex")
    .slice(0, 16);
  // macOS can assess freshly written executables in /var/folders before spawn
  // returns. Keep executable inodes in /tmp even when resolver symlinks use TMPDIR.
  const cacheDir =
    process.platform === "darwin"
      ? join("/tmp", `aft-fixture-cache-${process.getuid?.()}`)
      : join(tmpdir(), "aft-fixture-cache");
  mkdirSync(cacheDir, { recursive: true, mode: 0o700 });
  const directory = lstatSync(cacheDir);
  if (!directory.isDirectory() || (process.getuid && directory.uid !== process.getuid())) {
    throw new Error(`Native fixture cache is not an owned directory: ${cacheDir}`);
  }
  const cachedBinary = join(cacheDir, `aft-fixture-${cacheKey}`);

  if (!existsSync(cachedBinary)) {
    compileNativeFixture(source, cacheDir, cachedBinary);
  }

  if (process.platform === "darwin") {
    prepareMacOSFixture(cachedBinary);
  }

  // Keep each test's resolver-visible path, without copying a new executable inode.
  // A symlink can be unlinked and replaced by a test without changing the shared binary.
  symlinkSync(cachedBinary, path);
}

const preparedMacOSFixtures = new Set<string>();

function prepareMacOSFixture(path: string): void {
  if (preparedMacOSFixtures.has(path)) return;
  // Assess the signed inode in a separate runtime process. If posix_spawn blocks
  // its event loop, the supervising process can still enforce the deadline.
  const verified = spawnSync("/usr/bin/codesign", ["--verify", "--strict", path], {
    encoding: "utf8",
    timeout: 60_000,
  });
  if (verified.error || verified.status !== 0) {
    throw new Error(
      `macOS native fixture signature verification failed: ${verified.error?.message ?? verified.stderr}`,
    );
  }
  const warmed = probeNativeFixture(path);
  if (process.env.AFT_TEST_NATIVE_FIXTURE_TIMING === "1") {
    console.log(`NATIVE_FIXTURE_FIRST_BYTE ${JSON.stringify({ path, ...warmed })}`);
  }
  preparedMacOSFixtures.add(path);
}

export interface NativeFixtureTiming {
  spawnReturnMs: number;
  firstByteMs: number;
  closeMs: number;
}

function probeNativeFixture(path: string): NativeFixtureTiming {
  const script = `
    const { spawn } = require("node:child_process");
    const start = performance.now();
    let firstByteMs = null, stdout = "", stderr = "";
    const child = spawn(process.argv[1], ["--aft-fixture-warm"], { stdio: ["ignore", "pipe", "pipe"] });
    const spawnReturnMs = performance.now() - start;
    child.stdout.on("data", (data) => {
      if (firstByteMs === null) firstByteMs = performance.now() - start;
      stdout += data.toString();
    });
    child.stderr.on("data", (data) => { stderr += data.toString(); });
    child.on("error", (error) => { console.error(error); process.exitCode = 1; });
    child.on("close", (status) => {
      console.log(JSON.stringify({ spawnReturnMs, firstByteMs, closeMs: performance.now() - start, stdout, stderr, status }));
    });
  `;
  const probe = spawnSync(process.execPath, ["--eval", script, path], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout: 65_000,
  });
  if (probe.error || probe.status !== 0) {
    throw new Error(
      `Native fixture first-byte probe failed: ${probe.error?.message ?? probe.stderr}`,
    );
  }
  const measured = JSON.parse(probe.stdout.trim());
  if (
    measured.status !== 0 ||
    measured.stdout !== "aft-fixture-ready\n" ||
    measured.stderr !== "" ||
    typeof measured.firstByteMs !== "number"
  ) {
    throw new Error(`Native fixture assessment did not emit its ready byte: ${probe.stdout}`);
  }
  return {
    spawnReturnMs: measured.spawnReturnMs,
    firstByteMs: measured.firstByteMs,
    closeMs: measured.closeMs,
  };
}

/** Measure first execution and inode reuse without pre-warming either location. */
export function measureColdNativeFixture(
  directory: string,
  uniqueSource: string,
): {
  path: string;
  cold: NativeFixtureTiming;
  warm: NativeFixtureTiming;
} {
  const path = join(directory, "aft-cold-probe");
  compileNativeFixture(nativeFixtureSource({ stdout: uniqueSource }), directory, path);
  return { path, cold: probeNativeFixture(path), warm: probeNativeFixture(path) };
}

function compileNativeFixture(source: string, cacheDir: string, cachedBinary: string): void {
  if (!nativeCompiler)
    throw new Error("Native aft test fixtures are not available on this platform");

  mkdirSync(cacheDir, { recursive: true });
  // Unique staging names isolate compilers. Atomic hard-link publication keeps
  // the first complete signed inode, rather than replacing a warmed cache entry.
  const staging = join(
    cacheDir,
    `stage-${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2)}`,
  );
  const sourcePath = `${staging}.c`;
  writeFileSync(sourcePath, source, "utf8");

  const result = spawnSync(
    nativeCompiler.command,
    [...nativeCompiler.args, sourcePath, "-o", staging],
    {
      encoding: "utf8",
      env: { ...process.env, PATH: compilerSearchPath },
      stdio: ["ignore", "pipe", "pipe"],
      timeout: 60_000,
    },
  );

  if (result.error || result.status !== 0) {
    const detail = [
      result.error instanceof Error ? result.error.message : undefined,
      result.signal ? `signal: ${result.signal}` : undefined,
      String(result.stdout ?? "").trim(),
      String(result.stderr ?? "").trim(),
    ]
      .filter(Boolean)
      .join("\n");
    throw new Error(
      `Failed to compile native aft test fixture with ${nativeCompiler.label}: ${detail}`,
    );
  }

  chmodSync(staging, 0o755);
  if (process.platform === "darwin") {
    // Publish only signed bytes so concurrent users never rewrite a fixture
    // that another test is executing from the shared content-keyed cache.
    const signed = spawnSync("/usr/bin/codesign", ["-f", "-s", "-", staging], {
      encoding: "utf8",
      timeout: 60_000,
    });
    if (signed.error || signed.status !== 0) {
      throw new Error(
        `macOS native fixture signing failed: ${signed.error?.message ?? signed.stderr}`,
      );
    }
  }
  try {
    linkSync(staging, cachedBinary);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "EEXIST") throw error;
  } finally {
    rmSync(staging, { force: true });
    rmSync(sourcePath, { force: true });
  }
}

function nativeFixtureSource(behavior: AftFixtureBehavior): string {
  const stdout = bytesForC(behavior.stdout ?? "");
  const stderr = bytesForC(behavior.stderr ?? "");
  const sleepMs = Math.max(0, Math.trunc(behavior.sleepMs ?? 0));
  const exitCode = Math.trunc(behavior.exitCode ?? 0);

  return `#if !defined(_WIN32)
#define _POSIX_C_SOURCE 199309L
#endif
#include <stdio.h>
#include <string.h>
#if defined(_WIN32)
#include <windows.h>
#else
#include <errno.h>
#include <time.h>
#endif

static void aft_fixture_sleep(unsigned int milliseconds) {
  if (milliseconds == 0u) return;
#if defined(_WIN32)
  Sleep(milliseconds);
#else
  struct timespec requested;
  requested.tv_sec = milliseconds / 1000u;
  requested.tv_nsec = (long)(milliseconds % 1000u) * 1000000L;
  while (nanosleep(&requested, &requested) == -1 && errno == EINTR) {}
#endif
}

int main(int argc, char **argv) {
  if (argc == 2 && strcmp(argv[1], "--aft-fixture-warm") == 0) {
    fputs("aft-fixture-ready\\n", stdout);
    fflush(stdout);
    return 0;
  }
  static const unsigned char stdout_bytes[] = { ${stdout.literal} };
  static const unsigned char stderr_bytes[] = { ${stderr.literal} };

  aft_fixture_sleep(${sleepMs}u);
  if (${stdout.length}u > 0u) {
    fwrite(stdout_bytes, 1u, ${stdout.length}u, stdout);
    fflush(stdout);
  }
  if (${stderr.length}u > 0u) {
    fwrite(stderr_bytes, 1u, ${stderr.length}u, stderr);
    fflush(stderr);
  }

  return ${exitCode};
}
`;
}

function bytesForC(value: string): { literal: string; length: number } {
  const bytes = Buffer.from(value, "utf8");
  return {
    literal: bytes.length > 0 ? [...bytes].map((byte) => `0x${byte.toString(16)}`).join(", ") : "0",
    length: bytes.length,
  };
}

interface NativeCompiler {
  command: string;
  args: string[];
  label: string;
}

function resolveNativeCompiler(): NativeCompiler {
  const candidates = [process.env.CC, "cc", "clang", "gcc"].filter(
    (candidate): candidate is string => Boolean(candidate?.trim()),
  );

  for (const candidate of candidates) {
    const [command, ...args] = candidate.trim().split(/\s+/);
    const resolved = resolveCommand(command);
    if (resolved) return { command: resolved, args, label: candidate };
  }

  return { command: "cc", args: [], label: "cc" };
}

function resolveCommand(command: string): string | null {
  if (command.includes("/") || command.includes("\\")) return existsSync(command) ? command : null;

  for (const dir of compilerSearchPath.split(delimiter)) {
    if (!dir) continue;
    const path = join(dir, command);
    if (existsSync(path)) return path;
  }

  return null;
}

function writeShellFixture(path: string, behavior: AftFixtureBehavior): void {
  const sleepSeconds = Math.ceil(Math.max(0, behavior.sleepMs ?? 0) / 1000);
  const lines = ["#!/bin/sh"];

  if (sleepSeconds > 0) lines.push(`sleep ${sleepSeconds}`);
  if (behavior.stdout) lines.push(`printf '%s' ${shellQuote(behavior.stdout)}`);
  if (behavior.stderr) lines.push(`printf '%s' ${shellQuote(behavior.stderr)} >&2`);
  lines.push(`exit ${Math.trunc(behavior.exitCode ?? 0)}`, "");

  symlinkSync(cachedExecutable(lines.join("\n")), path);
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}
