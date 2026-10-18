import { copyFileSync, writeFileSync } from "node:fs";

/** A real executable for cache tests: matching bytes alone no longer establish runtime health. */
export function writeSmokeBinary(path: string): string {
  if (process.platform === "win32") {
    const executable = `${path}.exe`;
    copyFileSync(process.execPath, executable);
    return executable;
  }
  writeFileSync(path, "#!/bin/sh\nexit 0\n", { mode: 0o755 });
  return path;
}
