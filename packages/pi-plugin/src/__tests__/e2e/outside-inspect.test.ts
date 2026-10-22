/**
 * E2E: inspecting a file outside the project asks through Pi's outside-access
 * prompt, runs once approved, and ends with the typed denial when refused.
 */

/// <reference path="../../bun-test.d.ts" />

import { describe, expect, mock, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import { callToolCall } from "../../tools/_shared.js";
import { createHarness, prepareBinary } from "./helpers.js";

const binary = await prepareBinary();

describe.skipIf(!binary.binaryPath)("outside-file inspect (real engine)", () => {
  test("Pi asks before inspecting an outside file, runs once approved, and a denial ends it", async () => {
    const h = await createHarness(binary, { noFixtures: true });
    // Outside the project and outside the system temp folder, so the engine
    // asks instead of allowing it as scratch.
    const cache = join(homedir(), ".cache");
    await mkdir(cache, { recursive: true });
    const outside = await mkdtemp(join(cache, "aft-pi-outside-inspect-"));
    try {
      const file = join(outside, "probe.ts");
      await writeFile(file, "// TODO: outside inspect\nexport const probe = 1;\n");
      // The aft_inspect tool sends exactly this tool call.
      const args = { scope: file, sections: "todos" };
      const withConfirm = (confirm: () => Promise<boolean>) =>
        ({ ...h.extCtx, hasUI: true, mode: "tui", ui: { confirm } }) as unknown as ExtensionContext;

      const allow = mock(async () => true);
      const result = await callToolCall(h.bridge, "inspect", args, withConfirm(allow));
      expect(result.success).not.toBe(false);
      expect(result.code).toBeUndefined();
      expect(allow).toHaveBeenCalledTimes(1);

      const deny = mock(async () => false);
      await expect(
        callToolCall(h.bridge, "inspect", args, withConfirm(deny)),
      ).rejects.toMatchObject({ code: "permission_denied" });
      expect(deny).toHaveBeenCalledTimes(1);
    } finally {
      await h.cleanup();
      await rm(outside, { recursive: true, force: true });
    }
  }, 60_000);
});
