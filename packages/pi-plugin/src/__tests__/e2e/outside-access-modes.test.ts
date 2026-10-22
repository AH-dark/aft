/**
 * E2E: how Pi answers the real engine's outside-access asks in each session
 * mode. Print, JSON and RPC sessions read an outside file without an AFT
 * prompt (only Pi's own terminal UI is ever asked); floors (`sandbox.read_deny`) and a configured
 * `restrict_to_project_root: true` refuse in every mode, with no prompt.
 */

/// <reference path="../../bun-test.d.ts" />

import { describe, expect, mock, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import {
  callToolCall,
  type OutsideAccessPolicy,
  withOutsidePermissionOperation,
} from "../../tools/_shared.js";
import { createHarness, prepareBinary } from "./helpers.js";

const binary = await prepareBinary();

/** An outside folder that is neither in the project nor in the system temp folder. */
async function outsideFolder(): Promise<string> {
  const cache = join(homedir(), ".cache");
  await mkdir(cache, { recursive: true });
  return mkdtemp(join(cache, "aft-pi-outside-modes-"));
}

const BODY = "outside body 6f1c\n";

/** Every way a Pi session can run, with the confirm it would be asked through. */
function sessions() {
  const confirm = mock(async () => true);
  const contexts = (base: Record<string, unknown>) =>
    [
      ["tui", { ...base, hasUI: true, mode: "tui", ui: { confirm } }],
      ["print", { ...base, hasUI: false, mode: "print", ui: { confirm } }],
      ["json", { ...base, hasUI: false, mode: "json", ui: { confirm } }],
      ["rpc", { ...base, hasUI: true, mode: "rpc", ui: { confirm } }],
    ] as Array<[string, ExtensionContext]>;
  return { confirm, contexts };
}

const policies: OutsideAccessPolicy[] = [
  { harness: "pi", askOnPi: true },
  { harness: "pi", askOnPi: false },
  { harness: "omp", askOnPi: true },
];

describe.skipIf(!binary.binaryPath)("outside access by session mode (real engine)", () => {
  test("print, JSON and RPC sessions read an outside file without asking", async () => {
    const h = await createHarness(binary, { noFixtures: true });
    const outside = await outsideFolder();
    try {
      const file = join(outside, "notes.txt");
      await writeFile(file, BODY);
      const { confirm, contexts } = sessions();
      for (const [mode, extCtx] of contexts(h.extCtx as unknown as Record<string, unknown>)) {
        if (mode === "tui") continue;
        const result = await withOutsidePermissionOperation(
          () => callToolCall(h.bridge, "read", { path: file }, extCtx),
          undefined,
          { harness: "pi", askOnPi: true },
        );
        expect(result.success, mode).not.toBe(false);
        expect(JSON.stringify(result), mode).toContain("outside body 6f1c");
      }
      expect(confirm).not.toHaveBeenCalled();
      // The same read from Pi's terminal UI is asked about: the engine really
      // asks for this path, so the reads above were answered by AFT itself.
      const [, terminal] = contexts(h.extCtx as unknown as Record<string, unknown>)[0];
      const asked = await withOutsidePermissionOperation(
        () => callToolCall(h.bridge, "read", { path: file }, terminal),
        undefined,
        { harness: "pi", askOnPi: true },
      );
      expect(asked.success).not.toBe(false);
      expect(confirm).toHaveBeenCalledTimes(1);
    } finally {
      await h.cleanup();
      await rm(outside, { recursive: true, force: true });
    }
  }, 60_000);

  for (const [label, config] of [
    ["sandbox.read_deny", (outside: string) => ({ sandbox: { read_deny: [outside] } })],
    ["restrict_to_project_root: true", () => ({ restrict_to_project_root: true })],
  ] as const) {
    test(`${label} refuses in every mode and setting, without a prompt`, async () => {
      const outside = await outsideFolder();
      const h = await createHarness(binary, { noFixtures: true, config: config(outside) });
      try {
        const file = join(outside, "secret.txt");
        await writeFile(file, BODY);
        const { confirm, contexts } = sessions();
        for (const policy of policies) {
          for (const [mode, extCtx] of contexts(h.extCtx as unknown as Record<string, unknown>)) {
            const result = await withOutsidePermissionOperation(
              () => callToolCall(h.bridge, "read", { path: file }, extCtx),
              undefined,
              policy,
            ).catch((error: unknown) => ({ success: false, error: String(error) }));
            const where = `${mode} ${JSON.stringify(policy)}`;
            expect(result.success, where).toBe(false);
            expect(JSON.stringify(result), where).not.toContain("outside body 6f1c");
          }
        }
        expect(confirm).not.toHaveBeenCalled();
      } finally {
        await h.cleanup();
        await rm(outside, { recursive: true, force: true });
      }
    }, 60_000);
  }
});
