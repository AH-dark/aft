import { existsSync } from "node:fs";
import { join } from "node:path";
import { execFile, execFileSync } from "./child-process.js";

interface TarResolverOptions {
  platform?: NodeJS.Platform;
  env?: NodeJS.ProcessEnv;
  exists?: (path: string) => boolean;
}

/**
 * Resolve the archive extractor without allowing MSYS2 or Cygwin to shadow
 * Windows' drive-path-aware bsdtar executable.
 */
export function windowsTarExecutable(options: TarResolverOptions = {}): string {
  const platform = options.platform ?? process.platform;
  if (platform !== "win32") return "tar";

  const env = options.env ?? process.env;
  const systemTar = join(env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe");
  return (options.exists ?? existsSync)(systemTar) ? systemTar : "tar";
}

function extractionCommand(args: string[], privateStorage: boolean) {
  const executable = windowsTarExecutable();
  if (privateStorage && process.platform !== "win32") {
    // A child-only umask protects members without changing host/plugin writes.
    return {
      executable,
      file: "/bin/sh",
      args: [
        "-c",
        'umask 077; exec "$@"',
        "aft-extract",
        executable,
        "--no-same-permissions",
        ...args.map((arg, index) => (index === 0 && !arg.startsWith("-") ? `-${arg}` : arg)),
      ],
    };
  }
  return { executable, file: executable, args };
}

function extractionError(executable: string, cause: unknown): Error {
  const detail = cause instanceof Error ? cause.message : String(cause);
  return new Error(`tar extraction failed using ${executable}: ${detail}`, { cause });
}

/** Synchronous compatibility entry point for callers that explicitly need it. */
export function execTarExtractionSync(
  args: string[],
  timeout: number,
  privateStorage = false,
): void {
  const command = extractionCommand(args, privateStorage);
  try {
    execFileSync(command.file, command.args, { stdio: "pipe", timeout });
  } catch (cause) {
    throw extractionError(command.executable, cause);
  }
}

/** Async extraction keeps first-time installs from blocking the host event loop. */
export function execTarExtraction(
  args: string[],
  timeout: number,
  privateStorage = false,
): Promise<void> {
  const command = extractionCommand(args, privateStorage);
  return new Promise((resolve, reject) => {
    execFile(command.file, command.args, { timeout }, (error) => {
      if (error) reject(extractionError(command.executable, error));
      else resolve();
    });
  });
}
