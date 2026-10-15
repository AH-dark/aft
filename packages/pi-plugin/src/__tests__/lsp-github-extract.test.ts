/**
 * Tests for `validateExtraction` in lsp-github-install.ts.
 *
 * Total uncompressed bytes capped at MAX_EXTRACT_BYTES (1 GiB).
 * Mirrors the OpenCode plugin tests.
 */

import { afterEach, describe, expect, spyOn, test } from "bun:test";
import * as childProcess from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "@cortexkit/aft-bridge";
import {
  _extractArchiveSafelyForTesting as extractArchiveSafely,
  _precheckArchiveSizeForTesting as precheckArchiveSize,
  validateExtraction,
} from "../lsp-github-install.js";

const tempRoots = new Set<string>();

function createStagingFixture(): string {
  const root = mkdtempSync(join(tmpdir(), "aft-pi-extract-tests-"));
  tempRoots.add(root);
  return root;
}

afterEach(() => {
  for (const root of tempRoots) rmSync(root, { recursive: true, force: true });
  tempRoots.clear();
});

describe("precheckArchiveSize", () => {
  test("rejects an archive TOC with a sparse-sized member before extraction", async () => {
    const root = createStagingFixture();
    const archivePath = join(root, "bomb.zip");
    writeFakeZipWithUncompressedSize(archivePath, 1024 * 1024 * 1024 + 1);

    await expect(precheckArchiveSize(archivePath, "zip")).rejects.toThrow(
      /archive uncompressed size/,
    );
  });
});

describe("extractArchiveSafely", () => {
  // Hold each archive-tool callback until a timer has run. This measures the
  // host's event loop, not how quickly tar exits or Bun delivers pipe EOF.
  // An unbounded setImmediate spin also competes with the I/O being observed.
  test("extracts without blocking the event loop", async () => {
    const root = createStagingFixture();
    const source = join(root, "source");
    mkdirSync(join(source, "bin"), { recursive: true });
    writeFileSync(join(source, "bin", "server"), "binary");
    const archivePath = join(root, "server.tar.gz");
    execFileSync("tar", ["-czf", archivePath, "-C", source, "bin"]);
    const dest = join(root, "extract");

    type PendingTool = { command: string; args: string[]; complete: () => void };
    let announce: (tool: PendingTool) => void;
    const nextTool = (): Promise<PendingTool> =>
      new Promise((resolve) => {
        announce = resolve;
      });
    let started = nextTool();
    let draining = false;
    let releasePending: (() => void) | undefined;
    const toolMock = spyOn(childProcess, "execFile").mockImplementation(((
      command: string,
      args: string[],
      _options: unknown,
      callback: (
        error: childProcess.ExecFileException | null,
        stdout: string,
        stderr: string,
      ) => void,
    ) => {
      const complete = (): void => {
        releasePending = undefined;
        // Complete the controlled tool without waiting for a native child.
        // The separate integration case exercises real tar and its pipes.
        try {
          if (args[0] === "-tvf")
            callback(null, "-rw-r--r-- 0 user group 6 Jan 1 00:00 bin/server\n", "");
          else if (args[0] === "-tf") callback(null, "bin/server\n", "");
          else if (args[0] === "-xzf") {
            const staging = args[args.indexOf("-C") + 1];
            mkdirSync(join(staging, "bin"), { recursive: true });
            writeFileSync(join(staging, "bin", "server"), "binary");
            callback(null, "", "");
          } else throw new Error(`unexpected archive tool arguments: ${args.join(" ")}`);
        } catch (error) {
          callback(error as childProcess.ExecFileException, "", "");
        }
      };
      releasePending = complete;
      if (draining) complete();
      else announce({ command, args, complete });
      // runArchiveTool consumes the callback, not the ChildProcess handle.
      return {} as childProcess.ChildProcess;
    }) as typeof childProcess.execFile);
    let settled = false;
    const extraction = extractArchiveSafely(archivePath, dest, "tar.gz").finally(() => {
      settled = true;
    });
    const finished = extraction.then(() => null);
    try {
      for (const flag of ["-tvf", "-tf", "-xzf"]) {
        const pending = await Promise.race([started, finished]);
        expect(pending, `extraction completed before the asynchronous ${flag} call`).not.toBeNull();
        if (!pending) return;
        expect(pending.args[0]).toBe(flag);
        await new Promise<void>((resolve) => setTimeout(resolve, 0));
        expect(settled, `extraction settled before a timer ran during ${flag}`).toBe(false);
        started = nextTool();
        pending.complete();
      }
      await extraction;
    } finally {
      draining = true;
      releasePending?.();
      await extraction.catch(() => {});
      toolMock.mockRestore();
    }

    expect(readFileSync(join(dest, "bin", "server"), "utf8")).toBe("binary");
  });

  test("a failed extraction leaves no staging directory behind", async () => {
    const root = createStagingFixture();
    const archivePath = join(root, "broken.tar.gz");
    writeFileSync(archivePath, "not an archive");
    const dest = join(root, "extract");

    await expect(extractArchiveSafely(archivePath, dest, "tar.gz")).rejects.toThrow();
    expect(existsSync(dest)).toBe(false);
    const leftovers = readdirSync(root).filter((entry) => entry.startsWith("extract.staging-"));
    expect(leftovers).toEqual([]);
  });
});

function writeFakeZipWithUncompressedSize(path: string, uncompressedSize: number): void {
  const name = Buffer.from("sparse.bin");
  const local = Buffer.alloc(30 + name.length);
  local.writeUInt32LE(0x04034b50, 0);
  local.writeUInt16LE(20, 4);
  local.writeUInt32LE(0, 14);
  local.writeUInt32LE(uncompressedSize, 22);
  local.writeUInt16LE(name.length, 26);
  name.copy(local, 30);

  const central = Buffer.alloc(46 + name.length);
  central.writeUInt32LE(0x02014b50, 0);
  central.writeUInt16LE(20, 4);
  central.writeUInt16LE(20, 6);
  central.writeUInt32LE(0, 16);
  central.writeUInt32LE(uncompressedSize, 24);
  central.writeUInt16LE(name.length, 28);
  name.copy(central, 46);

  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(1, 8);
  end.writeUInt16LE(1, 10);
  end.writeUInt32LE(central.length, 12);
  end.writeUInt32LE(local.length, 16);
  writeFileSync(path, Buffer.concat([local, central, end]));
}

describe("validateExtraction (Pi)", () => {
  test("accepts a normal extraction", () => {
    const staging = createStagingFixture();
    mkdirSync(join(staging, "bin"), { recursive: true });
    writeFileSync(join(staging, "bin", "lsp-binary"), "binary");
    expect(() => validateExtraction(staging)).not.toThrow();
  });

  test("rejects symlinks (zip-slip defense)", () => {
    const staging = createStagingFixture();
    writeFileSync(join(staging, "real.txt"), "real");
    symlinkSync("real.txt", join(staging, "link.txt"));
    expect(() => validateExtraction(staging)).toThrow(/symlink.*zip-slip defense/);
  });

  test("rejects sparse file > 1 GiB cap (decompression bomb)", async () => {
    const staging = createStagingFixture();
    const fs = await import("node:fs");
    const fh = fs.openSync(join(staging, "sparse.bin"), "w");
    try {
      fs.ftruncateSync(fh, 1024 * 1024 * 1024 + 1);
    } finally {
      fs.closeSync(fh);
    }
    expect(() => validateExtraction(staging)).toThrow(/decompression bomb defense/);
  });

  test("rejects accumulated bytes across files exceeding cap", async () => {
    const staging = createStagingFixture();
    const fs = await import("node:fs");
    for (const name of ["a.bin", "b.bin"]) {
      const fh = fs.openSync(join(staging, name), "w");
      try {
        fs.ftruncateSync(fh, 600 * 1024 * 1024);
      } finally {
        fs.closeSync(fh);
      }
    }
    expect(() => validateExtraction(staging)).toThrow(/decompression bomb defense/);
  });
});
