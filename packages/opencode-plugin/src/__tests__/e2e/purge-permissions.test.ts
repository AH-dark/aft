import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, rm, writeFile } from "node:fs/promises";
import * as path from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { Effect } from "effect";
import { projectV2Tool, type V2PermissionRequest } from "../../tools/definitions/v2.js";
import { hoistedV2ToolConsumers } from "../../tools/hoisted/v2.js";
import { safetyTools } from "../../tools/safety.js";
import type { PluginContext } from "../../types.js";
import { permissionHost as createPermissionHost } from "../v2-permission-host.js";
import { cleanupHarnesses, createHarness, type E2EHarness, prepareBinary } from "./helpers.js";

const binary = await prepareBinary();
const sessionID = "ses_purge_permissions";

describe.skipIf(!binary.binaryPath)("aft_safety purge permissions", () => {
  const harnesses: E2EHarness[] = [];
  afterEach(async () => cleanupHarnesses(harnesses));

  for (const host of ["V1", "V2"] as const) {
    for (const effect of ["deny", "allow"] as const) {
      test(`${host} purge ${effect} asks edit for the path before removing anything`, async () => {
        const h = await createHarness(binary);
        harnesses.push(h);
        const secret = h.path("secrets/x");
        await mkdir(h.path("secrets"), { recursive: true });
        await writeFile(secret, "token=0\n");
        const write = await h.bridge.toolCall(sessionID, "write", {
          path: secret,
          content: "token=1\n",
        });
        expect(write.success).not.toBe(false);
        const checkpoint = await h.bridge.toolCall(sessionID, "safety", {
          op: "checkpoint",
          name: "with-secret",
          files: [secret],
        });
        expect(checkpoint.success).toBe(true);

        const { result, requests } = await executePurge(h, host, effect, "secrets/x");

        expect(requests).toEqual([{ permission: "edit", patterns: ["secrets/x"] }]);
        const list = await h.bridge.toolCall(sessionID, "safety", { op: "list" });
        const names = (list.checkpoints as Array<{ name: string }>).map((entry) => entry.name);
        if (effect === "deny") {
          expect(result.toLowerCase()).toContain("denied");
          expect(names).toEqual(["with-secret"]);
          const undo = await h.bridge.toolCall(sessionID, "safety", {
            op: "undo",
            path: secret,
          });
          expect(undo.success).not.toBe(false);
          expect(await Bun.file(secret).text()).toBe("token=0\n");
        } else {
          expect(result).toContain("This cannot be undone");
          expect(names).toEqual([]);
          const undo = await h.bridge.toolCall(sessionID, "safety", {
            op: "undo",
            path: secret,
          });
          expect(undo.success).toBe(false);
          expect(undo.code).toBe("no_undo_history");
          expect(await Bun.file(secret).text()).toBe("token=1\n");
        }
      }, 60_000);
    }

    test(`${host} purge of a deleted directory asks edit for each saved path under it`, async () => {
      const h = await createHarness(binary);
      harnesses.push(h);
      const secret = h.path("secrets/x");
      await mkdir(h.path("secrets"), { recursive: true });
      await writeFile(secret, "token=0\n");
      const write = await h.bridge.toolCall(sessionID, "write", {
        path: secret,
        content: "token=1\n",
      });
      expect(write.success).not.toBe(false);
      const checkpoint = await h.bridge.toolCall(sessionID, "safety", {
        op: "checkpoint",
        name: "with-secret",
        files: [secret],
      });
      expect(checkpoint.success).toBe(true);
      // Once the directory is gone, nothing on disk says it held `x`.
      await rm(h.path("secrets"), { recursive: true, force: true });

      const { result, requests } = await executePurge(h, host, "deny", "secrets");

      expect(result.toLowerCase()).toContain("denied");
      const editPatterns = requests
        .filter((request) => request.permission === "edit")
        .flatMap((request) => request.patterns);
      expect(editPatterns).toContain(path.join("secrets", "x"));
      const list = await h.bridge.toolCall(sessionID, "safety", { op: "list" });
      const names = (list.checkpoints as Array<{ name: string }>).map((entry) => entry.name);
      expect(names).toEqual(["with-secret"]);
    }, 60_000);
  }
});

async function executePurge(
  h: E2EHarness,
  host: "V1" | "V2",
  effect: "deny" | "allow",
  target: string,
): Promise<{ result: string; requests: Array<{ permission: string; patterns: string[] }> }> {
  const definition = safetyTools(pluginContext(h)).aft_safety;
  const requests: Array<{ permission: string; patterns: string[] }> = [];
  const args = { op: "purge" as const, path: target };

  if (host === "V1") {
    const ask: ToolContext["ask"] = async (input) => {
      requests.push({ permission: input.permission, patterns: [...input.patterns] });
      // Stands in for the rule `edit secrets/*`, which matches a path under
      // `secrets` but not `secrets` itself.
      const underSecrets = input.patterns.some((pattern) =>
        pattern.startsWith(`secrets${path.sep}`),
      );
      if (effect === "deny" && input.permission === "edit" && underSecrets) {
        throw Object.assign(new Error(`denied by rule edit secrets/*: ${input.patterns[0]}`), {
          _tag: "DeniedError",
        });
      }
    };
    const result = String(
      await definition.execute(args, {
        directory: h.tempDir,
        worktree: h.tempDir,
        sessionID,
        messageID: "msg_purge",
        agent: "build",
        abort: new AbortController().signal,
        metadata: () => {},
        ask,
      } as ToolContext),
    );
    return { result, requests };
  }

  const permissionHost = createPermissionHost({
    agentRules: [
      { action: "*", resource: "*", effect: "allow" },
      { action: "edit", resource: "secrets/*", effect },
    ],
  }).host;
  const consumers = hoistedV2ToolConsumers(permissionHost);
  const tool = projectV2Tool(
    "aft_safety",
    definition,
    { directory: h.tempDir },
    {
      requestPermission: async (request: V2PermissionRequest, context, signal) => {
        requests.push({ permission: request.permission, patterns: [...request.patterns] });
        await consumers.requestPermission!(request, context, signal);
      },
    },
  );
  const result = await Effect.runPromise(
    tool.execute(args, {
      sessionID,
      messageID: "msg_purge",
      id: "call_purge",
      agent: "build",
      progress: () => Effect.void,
    }),
  ).then(
    (value) => JSON.stringify(value),
    (error) => String(error),
  );
  return { result, requests };
}

function pluginContext(h: E2EHarness): PluginContext {
  return {
    pool: { getBridge: () => h.bridge } as unknown as BridgePool,
    client: {},
    config: { disabled_tools: [], lsp: { diagnostics_on_edit: false } },
    storageDir: h.path(".storage"),
  } as PluginContext;
}
