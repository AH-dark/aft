import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import { loadScenarios, materializeParityScenarios } from "../../harness/scenario-loader.js";
import extension from "./inspect.extension.js";
describe("inspect OpenCode 2 scenarios", () => {
  test("load through the harness loader and satisfy the slice validator", async () => {
    const scenarios = materializeParityScenarios(await loadScenarios(resolve(import.meta.dir)));
    expect(scenarios.length).toBe(6);
    expect(scenarios.find((scenario) => scenario.id === "inspect/T4/abort")?.env).toEqual({ AFT_TEST_INSPECT_DELAY_MS: "30000" });
    // The interrupt is held until the host has started the inspect call; sent
    // as soon as the turn is served it can land before the call exists.
    expect(scenarios.find((scenario) => scenario.id === "inspect/T4/abort")?.controls?.[0]?.wait_for_tool_call).toEqual({ call_id: "inspect-abort", timeout_ms: 20000 });
    expect(scenarios.filter((scenario) => scenario.id !== "inspect/T4/abort").every((scenario) => scenario.env === undefined)).toBe(true);
    await extension.validate?.({ repo_root: resolve(import.meta.dir, "../../../../../.."), platform: "linux", scenarios, matrix: {}, pinned_host_version: "2.0.3" });
  });
});
