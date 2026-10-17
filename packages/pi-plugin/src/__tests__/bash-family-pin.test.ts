/**
 * Pi's bash family is pinned byte for byte.
 *
 * OpenCode 2 files AFT's bash permission requests under `shell` and can have
 * its own shell tool disabled by `aft setup`; neither change may reach Pi. The
 * fixture holds the name, label, description, parameter schema and prompt text
 * of every bash-family tool as Pi registered them before that work, so any
 * change to what a Pi agent is shown for these tools fails here.
 */
import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

import { registerBashCompanionTools, registerBashTool } from "../tools/bash.js";
import type { PluginContext } from "../types.js";

describe("Pi bash family registration", () => {
  test("matches the captured registration byte for byte", () => {
    const tools: Array<Record<string, unknown>> = [];
    const api = {
      registerTool: (tool: Record<string, unknown>) => tools.push(tool),
      on() {},
      registerCommand() {},
      registerMessageRenderer() {},
    };
    const ctx = {
      pool: {
        getBridge() {
          throw new Error("registration must not start a bridge");
        },
      },
      // The native sandbox is enabled so the `sandbox` parameter, offered
      // only then, stays pinned with the rest of the full surface.
      config: { sandbox: { profile: { head: "strict" } } },
      storageDir: "/tmp/test",
    } as unknown as PluginContext;

    registerBashTool(api as never, ctx);
    registerBashCompanionTools(api as never, ctx);

    const current = Object.fromEntries(
      tools.map((tool) => [
        tool.name,
        {
          label: tool.label,
          description: tool.description,
          parameters: tool.parameters,
          promptSnippet: tool.promptSnippet,
          promptGuidelines: tool.promptGuidelines,
        },
      ]),
    );
    const fixture = JSON.parse(
      readFileSync(join(import.meta.dir, "fixtures/pi-bash-family.json"), "utf8"),
    );
    expect(JSON.parse(JSON.stringify(current))).toEqual(fixture);
    expect(Object.keys(current)).toEqual([
      "bash",
      "bash_status",
      "bash_watch",
      "bash_write",
      "bash_kill",
    ]);
  });
});
