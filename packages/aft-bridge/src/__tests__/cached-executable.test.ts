import { expect, test } from "bun:test";
import { randomUUID } from "node:crypto";
import { readFileSync, statSync } from "node:fs";
import { cachedExecutableBytes } from "./test-utils/cached-executable.js";

test("binary executable bytes reuse one immutable content-addressed path", () => {
  const bytes = Buffer.from(`\u0000AFT test executable ${randomUUID()}\n`);
  const first = cachedExecutableBytes(bytes);
  const before = statSync(first);
  const second = cachedExecutableBytes(bytes);
  const after = statSync(second);

  expect(second).toBe(first);
  expect(readFileSync(second)).toEqual(bytes);
  expect(after.mtimeMs).toBe(before.mtimeMs);
  expect(after.ino).toBe(before.ino);
  expect(second.split(/[\\/]/).at(-2)).toMatch(/^[a-f0-9]{64}$/);
});
