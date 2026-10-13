import { describe, expect, test } from "bun:test";
import { mkdtemp, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { loadScenarios, materializeParityScenarios } from "../../harness/scenario-loader.js";
import extension from "./read.extension.js";

describe("read OpenCode 2 scenarios", () => {
  for (const [fixture, count] of [["complete", 1], ["incomplete", 1001]] as const) {
    test(`T6 ${fixture} prepares a directory with ${count} entries`, async () => {
      const scenarios = await loadScenarios(resolve(import.meta.dir));
      const scenario = scenarios.find((candidate) => candidate.id === `read/T6/read-directory-payload-entries/${fixture}`)!;
      const project = await mkdtemp(join(tmpdir(), "read-t6-"));
      try {
        await extension.beforeScenario?.({
          scenario,
          project_root: project,
          run_root: project,
          forensic_dir: project,
          host_generation: "v2",
        });
        const turn = scenario.turns[0];
        if (turn.response.kind !== "tool_calls") throw new Error("expected directory read call");
        const directory = turn.response.calls[0].arguments.filePath as string;
        const entries = await readdir(join(project, directory));
        expect(entries).toHaveLength(count);
        expect(entries).toContain("entry-0000.txt");
        expect(scenario.metadata?.t6).toMatchObject({
          surface_id: "read.directory.payload.entries",
          owner: "read",
          fixture,
          ...(fixture === "incomplete" ? {
            triggered_reason: "cap",
            expected_trailer: "shown 1000 of 1001 items (cap) · narrow: path, offset, limit",
          } : {}),
        });
      } finally {
        await rm(project, { recursive: true, force: true });
      }
    });
  }

  test("load through the harness loader and satisfy the slice validator", async () => {
    const root = resolve(import.meta.dir);
    const scenarios = materializeParityScenarios(await loadScenarios(root));
    expect(scenarios.map((scenario) => scenario.id)).toEqual([
      "read/T1/edit-family/gpt",
      "read/T1/edit-family/non-gpt",
      "read/T1/happy",
      "read/T2/invalid_arguments",
      "read/T2/missing_target",
      "read/T3/read_ask_allow",
      "read/T3/read_ask_deny",
      "read/T3/read_config_deny",
      "read/T6/read-directory-payload-entries/complete",
      "read/T6/read-directory-payload-entries/incomplete",
      "read/T7/edit-family/gpt",
      "read/T7/edit-family/non-gpt",
      "read/T7/happy",
    ]);
    await extension.validate?.({
      repo_root: resolve(import.meta.dir, "../../../../../.."),
      platform: "linux",
      scenarios,
      matrix: {},
      pinned_host_version: "2.0.3",
    });
  });

  test("edit-family scenarios check real V2 requests through the loaded extension", async () => {
    const scenarios = await loadScenarios(resolve(import.meta.dir));
    const project = await mkdtemp(join(tmpdir(), "read-edit-family-"));
    try {
      for (const [suffix, model, names] of [
        ["non-gpt", "mock-model", ["read", "edit", "write"]],
        // OpenCode 2 offers AFT's apply_patch as `patch`.
        ["gpt", "gpt-5-mock", ["read", "patch"]],
      ] as const) {
        const scenario = scenarios.find((entry) => entry.id === `read/T1/edit-family/${suffix}`)!;
        const context = { scenario, project_root: project, run_root: project, forensic_dir: project, host_generation: "v2" as const };
        const request = { model, tools: names.map((name) => ({ function: { name } })) };
        const exchange = { index: 0, label: `check-${suffix}`, request, response: {}, observed_at: "test" };
        await extension.observe?.(context, { kind: "mock_exchange", exchange });
        const leaked = { ...request, tools: [...request.tools, { function: { name: suffix === "gpt" ? "edit" : "patch" } }] };
        await expect(extension.observe!(context, { kind: "mock_exchange", exchange: { ...exchange, request: leaked } })).rejects.toThrow("unexpectedly includes");
      }
    } finally {
      await rm(project, { recursive: true, force: true });
    }
  });
});
