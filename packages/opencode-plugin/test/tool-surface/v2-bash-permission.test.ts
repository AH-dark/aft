/**
 * OpenCode 2 evaluates shell-command permission rules under the action
 * `shell`: its own command tool asks under that name, and it rewrites a legacy
 * `bash` key in a user's config to `shell`. AFT's command tool keeps the name
 * `bash` on every host, but on V2 its permission requests and its declared
 * permission use `shell`, so the user's allow/deny/ask rules for shell commands
 * apply to it. OpenCode 1 keeps `bash`, pinned here byte for byte.
 */
import { describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DEFAULT_DISABLED_TOOLS, translateConfigDocument } from "@cortexkit/aft-bridge";
import { type ToolDefinition, tool } from "@opencode-ai/plugin";
import { Effect } from "effect";

import type { AftConfig } from "../../src/config.js";
import {
  buildAftToolDefinitions,
  buildOpenCodeToolMap,
  registerAftTools,
  type V2ToolEditor,
} from "../../src/tool-registration.js";
import {
  projectV2Tool,
  type V2PermissionRequest,
  type V2ProviderTool,
} from "../../src/tools/definitions/v2.js";

const ALL_TOOLS_CONFIG: AftConfig = {
  tool_surface: "all",
  hoist_builtin_tools: true,
  backup: { enabled: true },
  bash: true,
  search_index: true,
  semantic_search: true,
};
const PROJECT = mkdtempSync(join(tmpdir(), "aft-v2-bash-permission-"));
const LOCATION = { directory: PROJECT, project: { directory: PROJECT, canonical: PROJECT } };
const BASH_FAMILY = ["bash", "bash_status", "bash_watch", "bash_kill", "bash_write"];

function resolved(config: AftConfig): AftConfig {
  const doc = structuredClone(config) as Record<string, unknown>;
  translateConfigDocument(doc, "user");
  doc.disabled_tools ??= [...DEFAULT_DISABLED_TOOLS];
  return doc as AftConfig;
}

function definitions(send?: (command: string, params: Record<string, unknown>) => unknown) {
  const config = resolved(ALL_TOOLS_CONFIG);
  const bridge = {
    send: async (command: string, params: Record<string, unknown> = {}) => {
      if (!send) throw new Error("this test must not reach a bridge");
      return send(command, params);
    },
  };
  return buildAftToolDefinitions(
    {
      pool: { getBridge: () => bridge },
      client: {},
      config,
      hashlineEffective: false,
      storageDir: join(PROJECT, ".storage"),
    } as never,
    config,
  );
}

const executionContext = {
  sessionID: "ses-v2",
  messageID: "msg-v2",
  id: "call-v2",
  agent: "build",
  progress: () => Effect.void,
};

describe("OpenCode 2 files AFT's bash permissions under shell", () => {
  // OpenCode 2 renders its native command row only for a tool named `shell`,
  // so AFT's bash is registered under that name there (issue #406); the
  // companions are AFT's own tools and keep theirs.
  test("bash registers as shell and every companion keeps its name", async () => {
    const added: V2ProviderTool[] = [];
    const removed: string[] = [];
    await Effect.runPromise(
      registerAftTools(
        {
          tool: {
            transform: (transform: (editor: V2ToolEditor) => void) =>
              Effect.sync(() =>
                transform({
                  add: (definition) => added.push(definition),
                  remove: (name) => removed.push(name),
                }),
              ),
          },
        },
        LOCATION,
        definitions(),
      ) as Effect.Effect<void>,
    );
    const names = added.map((definition) => definition.name);
    for (const name of BASH_FAMILY.filter((name) => name !== "bash")) {
      expect(names).toContain(name);
    }
    expect(names.filter((name) => name === "shell")).toHaveLength(1);
    expect(names).not.toContain("bash");
    // Registering under the host's name replaces its built-in; nothing is
    // removed first.
    expect(removed).not.toContain("shell");
  });

  test("the bash tool declares the shell permission action", () => {
    const bash = projectV2Tool("bash", definitions().bash as ToolDefinition, LOCATION);
    expect(bash.name).toBe("shell");
    expect(bash.options.permission).toBe("shell");
  });

  test("a command ask from the permission loop reaches the host as shell", async () => {
    const sends: Array<Record<string, unknown>> = [];
    const defs = definitions((_command, params) => {
      sends.push(params);
      if (sends.length === 1) {
        return {
          success: false,
          code: "permission_required",
          asks: [
            { kind: "bash", patterns: ["rm -rf build"], always: ["rm *"] },
            { kind: "external_directory", patterns: ["/elsewhere/*"], always: ["/elsewhere/*"] },
          ],
        };
      }
      return { success: true, status: "completed", exit_code: 0, output: "done" };
    });
    const asked: V2PermissionRequest[] = [];
    const bash = projectV2Tool("bash", defs.bash as ToolDefinition, LOCATION, {
      requestPermission: async (request) => {
        asked.push(request);
      },
    });

    await Effect.runPromise(bash.execute({ command: "rm -rf build" }, executionContext));

    expect(asked.map((request) => request.permission)).toEqual(["shell", "external_directory"]);
    expect(asked[0]?.patterns).toEqual(["rm -rf build"]);
    // The command itself carries nothing host-specific.
    expect(sends[0]).not.toHaveProperty("command_tool_name");
  });

  test("a rule denying shell refuses the command before it runs", async () => {
    let probes = 0;
    const defs = definitions(() => {
      probes += 1;
      return {
        success: false,
        code: "permission_required",
        asks: [{ kind: "bash", patterns: ["rm -rf build"], always: ["rm *"] }],
      };
    });
    const bash = projectV2Tool("bash", defs.bash as ToolDefinition, LOCATION, {
      requestPermission: async (request) => {
        if (request.permission === "shell") throw new Error("Permission denied: shell");
      },
    });

    await expect(
      Effect.runPromise(bash.execute({ command: "rm -rf build" }, executionContext)),
    ).rejects.toThrow("Permission denied: shell");
    expect(probes).toBe(1);
  });

  test("only the bash tool's own bash asks are translated", async () => {
    const asked: string[] = [];
    const asking = (permission: string) =>
      ({
        description: "probe",
        args: { value: tool.schema.string() },
        execute: async (_input: unknown, context: { ask(request: unknown): Promise<void> }) => {
          await context.ask({ permission, patterns: ["x"], always: [], metadata: {} });
          return "ok";
        },
      }) as unknown as ToolDefinition;
    const consumers = {
      requestPermission: async (request: V2PermissionRequest) => {
        asked.push(request.permission);
      },
    };

    await Effect.runPromise(
      projectV2Tool("bash", asking("bash"), LOCATION, consumers).execute(
        { value: "x" },
        executionContext,
      ),
    );
    await Effect.runPromise(
      projectV2Tool("aft_probe", asking("bash"), LOCATION, consumers).execute(
        { value: "x" },
        executionContext,
      ),
    );
    await Effect.runPromise(
      projectV2Tool("bash", asking("external_directory"), LOCATION, consumers).execute(
        { value: "x" },
        executionContext,
      ),
    );
    expect(asked).toEqual(["shell", "bash", "external_directory"]);
  });
});

describe("OpenCode 1 keeps its bash family byte-identical", () => {
  /**
   * Captured from the V1 registration before the OpenCode 2 permission work,
   * so any change to what an OpenCode 1 agent is shown for these tools, or to
   * their names, fails here. The native sandbox is enabled so the `sandbox`
   * argument, offered only then, stays pinned. The `bash` description is the
   * one an agent sees once `aft_search` is registered: the registration map
   * now carries that final wording instead of the factory default.
   */
  test("names, descriptions and schemas match the capture", () => {
    const config = resolved({ ...ALL_TOOLS_CONFIG, sandbox: { profile: { head: "strict" } } });
    const v1 = buildOpenCodeToolMap(
      {
        pool: {
          getBridge: () => {
            throw new Error("registration must not start a bridge");
          },
        },
        client: {},
        config,
        hashlineEffective: false,
        storageDir: "/x",
      } as never,
      config,
    );
    const fixture = JSON.parse(
      readFileSync(join(import.meta.dir, "fixtures/v1-bash-family.json"), "utf8"),
    );
    const current = Object.fromEntries(
      [...BASH_FAMILY, "aft_delete"].map((name) => {
        const definition = v1[name] as ToolDefinition & { options?: unknown };
        expect(definition.options).toBeUndefined();
        return [
          name,
          {
            description: definition.description,
            schema: tool.schema.toJSONSchema(tool.schema.object(definition.args), { io: "input" }),
          },
        ];
      }),
    );
    expect(current).toEqual(fixture);
  });
});
