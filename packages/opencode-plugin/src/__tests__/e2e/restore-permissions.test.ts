import { afterEach, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, rm, writeFile } from "node:fs/promises";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { Effect } from "effect";
import type { V2PermissionHostContext } from "../../permissions/v2.js";
import { projectV2Tool, type V2PermissionRequest } from "../../tools/definitions/v2.js";
import { hoistedV2ToolConsumers } from "../../tools/hoisted/v2.js";
import { safetyTools } from "../../tools/safety.js";
import type { PluginContext } from "../../types.js";
import { cleanupHarnesses, createHarness, type E2EHarness, prepareBinary } from "./helpers.js";

const binary = await prepareBinary();
const sessionID = "ses_restore_permissions";
const checkpointFiles = ["public.txt", "secrets/x", "secrets/deleted"];

describe.skipIf(!binary.binaryPath)("checkpoint restore permissions", () => {
  const harnesses: E2EHarness[] = [];
  afterEach(async () => cleanupHarnesses(harnesses));

  for (const host of ["V1", "V2"] as const) {
    for (const effect of ["deny", "allow", "ask"] as const) {
      test(`${host} restore ${effect} uses exact checkpoint paths before any write`, async () => {
        const h = await createHarness(binary);
        harnesses.push(h);
        await mkdir(h.path("secrets"), { recursive: true });
        for (const file of [...checkpointFiles, "unrelated.txt"]) {
          await writeFile(h.path(file), `snapshot ${file}\n`);
        }
        const checkpoint = await h.bridge.toolCall(sessionID, "safety", {
          op: "checkpoint",
          name: "permissions",
          files: checkpointFiles.map((file) => h.path(file)),
        });
        expect(checkpoint.success).toBe(true);
        expect(checkpoint.file_count).toBe(3);
        await writeFile(h.path("public.txt"), "current public\n");
        await writeFile(h.path("secrets/x"), "current secret\n");
        await rm(h.path("secrets/deleted"));
        await writeFile(h.path("unrelated.txt"), "current unrelated\n");

        const definition = safetyTools(pluginContext(h)).aft_safety;
        const requests: Array<{ permission: string; patterns: string[] }> = [];
        const prompts: string[][] = [];
        const args = { op: "restore", name: "permissions" };
        let result: string;
        if (host === "V1") {
          const ask: ToolContext["ask"] = async (input) => {
            requests.push({ permission: input.permission, patterns: [...input.patterns] });
            const denied = input.patterns.find((path) => path.startsWith("secrets/"));
            if (input.permission === "edit" && denied) {
              if (effect === "deny") {
                throw Object.assign(new Error(`denied by rule edit secrets/*: ${denied}`), {
                  _tag: "DeniedError",
                });
              }
              if (effect === "ask") prompts.push([...input.patterns]);
            }
          };
          result = String(
            await definition.execute(args, {
              directory: h.tempDir,
              worktree: h.tempDir,
              sessionID,
              messageID: "msg_restore",
              agent: "build",
              abort: new AbortController().signal,
              metadata: () => {},
              ask,
            } as ToolContext),
          );
        } else {
          const permissionHost: V2PermissionHostContext = {
            agent: {
              get: () =>
                Effect.succeed({
                  data: {
                    permissions: [
                      { action: "*", resource: "*", effect: "allow" as const },
                      { action: "edit", resource: "secrets/*", effect },
                    ],
                  },
                }),
            },
            session: { get: () => Effect.succeed({}) },
            permission: {
              assert: (input) => {
                prompts.push([...input.resources]);
                return Effect.void;
              },
            },
          };
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
          result = await Effect.runPromise(
            tool.execute(args, {
              sessionID,
              messageID: "msg_restore",
              id: "call_restore",
              agent: "build",
              progress: () => Effect.void,
            }),
          ).then(
            (value) => JSON.stringify(value),
            (error) => String(error),
          );
        }

        if (effect === "deny") {
          expect(result).toMatch(/secrets\/(?:x|deleted)/);
          expect(result.toLowerCase()).toContain("denied");
          expect(await Bun.file(h.path("public.txt")).text()).toBe("current public\n");
          expect(await Bun.file(h.path("secrets/x")).text()).toBe("current secret\n");
          expect(existsSync(h.path("secrets/deleted"))).toBe(false);
        } else {
          expect(result).toContain("checkpoint restored permissions");
          for (const file of checkpointFiles) {
            expect(await Bun.file(h.path(file)).text()).toBe(`snapshot ${file}\n`);
          }
        }
        expect(await Bun.file(h.path("unrelated.txt")).text()).toBe("current unrelated\n");
        expect(requests).toHaveLength(1);
        expect(requests[0].permission).toBe("edit");
        expect([...requests[0].patterns].sort()).toEqual([...checkpointFiles].sort());
        expect(prompts).toEqual(effect === "ask" ? [requests[0].patterns] : []);
      }, 60_000);
    }

    test(`${host} scoped restore of public.txt asks only for public.txt`, async () => {
      const h = await createHarness(binary);
      harnesses.push(h);
      await prepareScopedCheckpoint(h);

      const { result, requests } = await executeScopedRestore(h, host, "public.txt");

      expect(result).toContain("checkpoint restored permissions");
      expect(await Bun.file(h.path("public.txt")).text()).toBe("snapshot public.txt\n");
      expect(await Bun.file(h.path("secrets/x")).text()).toBe("current secret\n");
      expect(existsSync(h.path("secrets/deleted"))).toBe(false);
      expect(await Bun.file(h.path("unrelated.txt")).text()).toBe("current unrelated\n");
      expect(requests).toEqual([{ permission: "edit", patterns: ["public.txt"] }]);
    }, 60_000);

    test(`${host} scoped denied restore of secrets/x writes nothing`, async () => {
      const h = await createHarness(binary);
      harnesses.push(h);
      await prepareScopedCheckpoint(h);

      const { result, requests } = await executeScopedRestore(h, host, "secrets/x");

      expect(result.toLowerCase()).toContain("denied");
      expect(await Bun.file(h.path("public.txt")).text()).toBe("current public\n");
      expect(await Bun.file(h.path("secrets/x")).text()).toBe("current secret\n");
      expect(existsSync(h.path("secrets/deleted"))).toBe(false);
      expect(await Bun.file(h.path("unrelated.txt")).text()).toBe("current unrelated\n");
      expect(requests).toEqual([{ permission: "edit", patterns: ["secrets/x"] }]);
    }, 60_000);
  }
});

async function prepareScopedCheckpoint(h: E2EHarness): Promise<void> {
  await mkdir(h.path("secrets"), { recursive: true });
  for (const file of [...checkpointFiles, "unrelated.txt"]) {
    await writeFile(h.path(file), `snapshot ${file}\n`);
  }
  const checkpoint = await h.bridge.toolCall(sessionID, "safety", {
    op: "checkpoint",
    name: "permissions",
    files: checkpointFiles.map((file) => h.path(file)),
  });
  expect(checkpoint.success).toBe(true);
  expect(checkpoint.file_count).toBe(3);
  await writeFile(h.path("public.txt"), "current public\n");
  await writeFile(h.path("secrets/x"), "current secret\n");
  await rm(h.path("secrets/deleted"));
  await writeFile(h.path("unrelated.txt"), "current unrelated\n");
}

async function executeScopedRestore(
  h: E2EHarness,
  host: "V1" | "V2",
  path: string,
): Promise<{ result: string; requests: Array<{ permission: string; patterns: string[] }> }> {
  const definition = safetyTools(pluginContext(h)).aft_safety;
  const requests: Array<{ permission: string; patterns: string[] }> = [];
  const args = { op: "restore" as const, name: "permissions", path };

  if (host === "V1") {
    const ask: ToolContext["ask"] = async (input) => {
      requests.push({ permission: input.permission, patterns: [...input.patterns] });
      const denied = input.patterns.find((pattern) => pattern.startsWith("secrets/"));
      if (input.permission === "edit" && denied) {
        throw Object.assign(new Error(`denied by rule edit secrets/*: ${denied}`), {
          _tag: "DeniedError",
        });
      }
    };
    const result = String(
      await definition.execute(args, {
        directory: h.tempDir,
        worktree: h.tempDir,
        sessionID,
        messageID: "msg_restore_scoped",
        agent: "build",
        abort: new AbortController().signal,
        metadata: () => {},
        ask,
      } as ToolContext),
    );
    return { result, requests };
  }

  const permissionHost: V2PermissionHostContext = {
    agent: {
      get: () =>
        Effect.succeed({
          data: {
            permissions: [
              { action: "*", resource: "*", effect: "allow" as const },
              { action: "edit", resource: "secrets/*", effect: "deny" as const },
            ],
          },
        }),
    },
    session: { get: () => Effect.succeed({}) },
    permission: { assert: () => Effect.void },
  };
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
      messageID: "msg_restore_scoped",
      id: "call_restore_scoped",
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
