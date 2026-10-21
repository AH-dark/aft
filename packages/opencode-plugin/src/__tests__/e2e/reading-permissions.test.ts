import { afterEach, describe, expect, test } from "bun:test";
import { writeFile } from "node:fs/promises";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { Effect } from "effect";
import { projectV2Tool } from "../../tools/definitions/v2.js";
import { hoistedV2ToolConsumers } from "../../tools/hoisted/v2.js";
import { readingTools } from "../../tools/reading.js";
import type { PluginContext } from "../../types.js";
import { toolResultText } from "../test-helpers.js";
import { permissionHost } from "../v2-permission-host.js";
import { cleanupHarnesses, createHarness, type E2EHarness, prepareBinary } from "./helpers.js";

const binary = await prepareBinary();
const harnesses: E2EHarness[] = [];
afterEach(() => cleanupHarnesses(harnesses));

describe.skipIf(!binary.binaryPath)("reading permissions through real tool_call", () => {
  for (const version of [1, 2]) {
    test(`OC${version} zoom drops read-denied re-export bodies and member menus`, async () => {
      const h = await createHarness(binary, { fixtureNames: [], timeoutMs: 20_000 });
      harnesses.push(h);
      await writeFile(
        h.path("secret.ts"),
        'export function hidden() { return "SECRET_BODY_MUST_NOT_ESCAPE"; }\n' +
          "export class SecretService {\n" +
          Array.from(
            { length: 200 },
            (_, index) => `  privateMember${index}(): string { return 'secret'; }`,
          ).join("\n") +
          "\n}\n",
      );
      await writeFile(h.path("barrel.ts"), 'export { hidden, SecretService } from "./secret";\n');
      await writeFile(
        h.path("allowed.ts"),
        'export function visible() { return "PUBLIC_BODY"; }\n',
      );
      const plugin = {
        pool: { getBridge: () => h.bridge } as unknown as BridgePool,
        client: {},
        config: {},
        storageDir: h.path(".aft-storage"),
      } as PluginContext;
      const readPatterns: string[][] = [];
      const v1: ToolContext = {
        sessionID: "ses_read_permissions",
        messageID: "msg_read_permissions",
        agent: "build",
        directory: h.tempDir,
        worktree: h.tempDir,
        abort: new AbortController().signal,
        metadata: () => {},
        ask: async (...requests) => {
          expect(requests).toHaveLength(1);
          const [request] = requests;
          if (request.permission !== "read") return;
          readPatterns.push(request.patterns);
          if (request.patterns.includes("secret.ts")) {
            throw new Error(
              "The user has specified a rule which prevents you from using this specific tool call. Here are some of the relevant rules []",
            );
          }
        },
      };
      const host = permissionHost({
        agentRules: [
          { action: "*", resource: "*", effect: "allow" },
          { action: "read", resource: "secret.ts", effect: "deny" },
        ],
      });
      const definition = readingTools(plugin).aft_zoom;
      const args = {
        batch: [
          { target: "barrel.ts", symbols: ["hidden", "SecretService"] },
          { target: "allowed.ts", symbols: "visible" },
        ],
        callgraph: true,
      };
      const output =
        version === 1
          ? toolResultText(await definition.execute(args, v1))
          : String(
              (
                await Effect.runPromise(
                  projectV2Tool(
                    "aft_zoom",
                    definition,
                    { directory: h.tempDir },
                    hoistedV2ToolConsumers(host.host),
                  ).execute(args, {
                    sessionID: v1.sessionID,
                    messageID: v1.messageID,
                    id: "call_read_permissions",
                    agent: "build",
                    progress: () => Effect.void,
                  }),
                )
              ).content,
            );
      expect(output).toContain("PUBLIC_BODY");
      expect(output).toContain("secret.ts");
      expect(output).toContain("permission_denied: read");
      expect(output).not.toContain("SECRET_BODY_MUST_NOT_ESCAPE");
      expect(output).not.toContain("privateMember");
      expect(output).not.toContain("content_files");
      const patterns =
        version === 1
          ? readPatterns
          : host.calls.filter((call) => call.action === "read").map((call) => call.resources);
      expect(patterns.sort()).toEqual([["allowed.ts"], ["barrel.ts"], ["secret.ts"]]);
    }, 60_000);
  }
});
