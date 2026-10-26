import { fileURLToPath } from "node:url";

/**
 * This module's own resolved entry path, same as every other AFT pi-plugin
 * module's `import.meta.url` once bundled into one ESM output file.
 * Declared explicitly as `sourcePath` on every OMP tool registration
 * (`tool-registration.ts`) instead of relying on the host to infer it, and
 * read back by the AFT interop contract (`aft-contract.ts`) to filter its
 * own live tool inventory to exactly what this extension registered.
 */
export const TOOL_SOURCE_PATH: string = fileURLToPath(import.meta.url);
