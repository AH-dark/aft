import { afterEach, describe, expect, test } from "bun:test";
import { once } from "node:events";
import { mkdir, symlink, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import type { AddressInfo } from "node:net";
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

async function executeReader(
  h: E2EHarness,
  version: number,
  name: "aft_outline" | "aft_zoom",
  args: Record<string, unknown>,
  deniedPath = "secret.ts",
) {
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
      if (request.patterns.includes(deniedPath)) {
        throw new Error(
          "The user has specified a rule which prevents you from using this specific tool call. Here are some of the relevant rules []",
        );
      }
    },
  };
  const host = permissionHost({
    agentRules: [
      { action: "*", resource: "*", effect: "allow" },
      { action: "read", resource: deniedPath, effect: "deny" },
    ],
  });
  const definition = readingTools(plugin)[name];
  const run = async () =>
    version === 1
      ? toolResultText(await definition.execute(args, v1))
      : String(
          (
            await Effect.runPromise(
              projectV2Tool(
                name,
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
  // OpenCode 2 reports a whole-call permission refusal as a failed tool call
  // rather than as output text, so keep its message for the same assertions.
  const output = await run().catch((error: unknown) =>
    version === 2 ? String(error instanceof Error ? error.message : error) : Promise.reject(error),
  );
  const patterns =
    version === 1
      ? readPatterns
      : host.calls.filter((call) => call.action === "read").map((call) => call.resources);
  return { output, patterns };
}

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
      const args = {
        batch: [
          { target: "barrel.ts", symbols: ["hidden", "SecretService"] },
          { target: "allowed.ts", symbols: "visible" },
        ],
        callgraph: true,
      };
      const { output, patterns } = await executeReader(h, version, "aft_zoom", args);
      expect(output).toContain("PUBLIC_BODY");
      expect(output).toContain("secret.ts");
      expect(output).toContain("permission_denied: read");
      expect(output).not.toContain("SECRET_BODY_MUST_NOT_ESCAPE");
      expect(output).not.toContain("privateMember");
      expect(output).not.toContain("content_files");
      expect(patterns.sort()).toEqual([["allowed.ts"], ["barrel.ts"], ["secret.ts"]]);
    }, 60_000);
  }

  // A host rule names a file by its path under the project root. A request that
  // goes through a symlink must still be checked against the real location of
  // every file whose content comes back, or a rule on that location is bypassed.
  for (const version of [1, 2]) {
    for (const layout of ["deeper", "upward"] as const) {
      test(`OC${version} zoom through a ${layout} symlink checks the re-export source at its real path`, async () => {
        const h = await createHarness(binary, { fixtureNames: [], timeoutMs: 20_000 });
        harnesses.push(h);
        await mkdir(h.path("src"), { recursive: true });
        await writeFile(
          h.path("src/secret.ts"),
          'export function hidden() { return "SECRET_BODY_MUST_NOT_ESCAPE"; }\n',
        );
        let target: string;
        let barrel: string;
        if (layout === "deeper") {
          // `a/link` is one directory deeper than the `src` directory it names.
          await mkdir(h.path("a"));
          await symlink(h.path("src"), h.path("a/link"), "dir");
          await writeFile(h.path("src/barrel.ts"), 'export { hidden } from "./secret";\n');
          target = "a/link/barrel.ts";
          barrel = "src/barrel.ts";
        } else {
          // `src/up` names the project root itself.
          await symlink(h.tempDir, h.path("src/up"), "dir");
          await writeFile(h.path("barrel.ts"), 'export { hidden } from "./src/secret";\n');
          target = "src/up/barrel.ts";
          barrel = "barrel.ts";
        }
        const { output, patterns } = await executeReader(
          h,
          version,
          "aft_zoom",
          { batch: [{ target, symbols: "hidden" }] },
          "src/secret.ts",
        );
        expect(output).toContain("permission_denied: read");
        expect(output).not.toContain("SECRET_BODY_MUST_NOT_ESCAPE");
        // The requested spelling is checked before dispatch; the manifest then
        // names the barrel and its re-export source at their real locations.
        expect(patterns.sort()).toEqual([[barrel], ["src/secret.ts"], [target]].sort());
      }, 60_000);
    }

    test(`OC${version} outline through a symlink checks the file at its real path`, async () => {
      const h = await createHarness(binary, { fixtureNames: [], timeoutMs: 20_000 });
      harnesses.push(h);
      await mkdir(h.path("src"));
      await mkdir(h.path("a"));
      await symlink(h.path("src"), h.path("a/link"), "dir");
      await writeFile(
        h.path("src/secret.ts"),
        "export function SECRET_SIGNATURE_MUST_NOT_ESCAPE() {}\n",
      );
      const { output, patterns } = await executeReader(
        h,
        version,
        "aft_outline",
        { target: "a/link/secret.ts" },
        "src/secret.ts",
      );
      expect(output).toContain("permission_denied");
      expect(output).not.toContain("SECRET_SIGNATURE_MUST_NOT_ESCAPE");
      expect(patterns.sort()).toEqual([["a/link/secret.ts"], ["src/secret.ts"]]);
    }, 60_000);
  }

  for (const name of ["aft_outline", "aft_zoom"] as const) {
    test(`OC2 ${name} batch preserves allowed content after a read denial`, async () => {
      const h = await createHarness(binary, { fixtureNames: [], timeoutMs: 20_000 });
      harnesses.push(h);
      await writeFile(h.path("allowed.ts"), "export function visible() {}\n");
      await writeFile(
        h.path("secret.ts"),
        'export function SECRET_SIGNATURE_MUST_NOT_ESCAPE() { return "SECRET_BODY"; }\n',
      );
      const { output, patterns } = await executeReader(
        h,
        2,
        name,
        name === "aft_outline"
          ? { target: ["allowed.ts", "secret.ts"] }
          : {
              batch: [
                { target: "allowed.ts", symbols: "visible" },
                { target: "secret.ts", symbols: "SECRET_SIGNATURE_MUST_NOT_ESCAPE" },
              ],
            },
      );
      expect(output).toContain("visible");
      expect(output).toContain("secret.ts: permission_denied: read");
      expect(output).not.toContain("SECRET_SIGNATURE_MUST_NOT_ESCAPE");
      expect(output).not.toContain("SECRET_BODY");
      expect(patterns.sort()).toEqual([["allowed.ts"], ["secret.ts"]]);
    });
  }

  for (const version of [1, 2]) {
    for (const mixed of [false, true]) {
      test(`OC${version} capped ${mixed ? "mixed" : "URL-only"} zoom preserves its read manifest`, async () => {
        const server = createServer((_request, response) => {
          response.writeHead(200, { "content-type": "text/markdown" });
          response.end(`# Large\n\n${"URL_CONTENT ".repeat(6000)}\n`);
        });
        server.listen(0, "127.0.0.1");
        await once(server, "listening");
        const url = `http://127.0.0.1:${(server.address() as AddressInfo).port}/large.md`;
        try {
          const h = await createHarness(binary, { fixtureNames: [], timeoutMs: 20_000 });
          harnesses.push(h);
          const userConfig = h.path("aft-user.jsonc");
          await writeFile(userConfig, '{ "url_fetch_allow_private": true }\n');
          await h.bridge.send("configure", {
            project_root: h.tempDir,
            harness: "opencode",
            cortexkit_user_config_path: userConfig,
          });
          await writeFile(h.path("allowed.ts"), "export function visible() {}\n");
          const batch = mixed
            ? [
                { target: "allowed.ts", symbols: "visible" },
                { target: url, symbols: "Large" },
              ]
            : [{ target: url, symbols: "Large" }];
          const { output, patterns } = await executeReader(h, version, "aft_zoom", { batch });
          expect(output).toContain("URL_CONTENT");
          expect(output).toContain("truncated at");
          expect(Buffer.byteLength(output)).toBeLessThanOrEqual(50 * 1024);
          expect(output).not.toContain("Update AFT");
          expect(patterns).toEqual(mixed ? [["allowed.ts"]] : []);
          if (mixed) expect(output).toContain("visible");
          const response = await h.bridge.toolCall("ses_cap_manifest", "zoom", { batch });
          expect(response.content_files).toEqual(mixed ? [h.path("allowed.ts")] : []);
          expect(response.complete).toBe(false);
        } finally {
          await new Promise<void>((resolve, reject) =>
            server.close((error) => (error ? reject(error) : resolve())),
          );
        }
      }, 60_000);
    }
  }
});
