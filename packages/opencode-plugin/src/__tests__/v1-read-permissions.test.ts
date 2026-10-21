import { expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { relative, resolve } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { createReadTool } from "../tools/hoisted.js";
import { readingTools } from "../tools/reading.js";
import type { PluginContext } from "../types.js";
import { toolResultText } from "./test-helpers.js";

function readers(response?: (args: Record<string, unknown>) => Record<string, unknown>) {
  const asks: Parameters<ToolContext["ask"]>[0][] = [];
  const dispatched: Record<string, unknown>[] = [];
  const plugin = {
    pool: {
      getBridge: () => ({
        toolCall: async (_session: unknown, _name: string, args: Record<string, unknown>) => {
          dispatched.push(args);
          return {
            content_files: [],
            ...(response?.(args) ?? { success: true, text: "export function allowed() {}" }),
          };
        },
      }),
    } as unknown as BridgePool,
    client: {},
    config: {},
    storageDir: "/isolated/storage",
  } as PluginContext;
  // OC1 supplies only the SDK's one-argument Promise ask, not OC2's cancellable
  // permission adapter. Execute the same definitions OC1 registers directly.
  const context: ToolContext = {
    sessionID: "ses_v1_read",
    messageID: "msg_v1_read",
    agent: "build",
    directory: process.cwd(),
    worktree: process.cwd(),
    abort: new AbortController().signal,
    metadata: () => {},
    ask: async (...requests) => {
      expect(requests).toHaveLength(1);
      const [request] = requests;
      asks.push(request);
      if (
        request.permission === "read" &&
        request.patterns.some((file) => file === "secret.ts" || file.endsWith("/secret.ts"))
      ) {
        throw new Error(
          "The user has specified a rule which prevents you from using this specific tool call. Here are some of the relevant rules [{permission:read,pattern:**/secret.ts,action:deny}]",
        );
      }
    },
  };
  const tools = { ...readingTools(plugin), read: createReadTool(plugin) };
  const execute = async (name: keyof typeof tools, args: Record<string, unknown>) =>
    toolResultText(await tools[name]!.execute(args, context));
  return { execute, asks, dispatched };
}

for (const [name, args] of [
  ["read", { path: "secret.ts" }],
  ["aft_outline", { target: "secret.ts" }],
  ["aft_zoom", { batch: [{ target: "secret.ts", symbols: "secret" }], callgraph: true }],
] as const) {
  test(`OC1 ${name} refuses a read-denied file before dispatch`, async () => {
    for (const ref of [undefined, "HEAD~1"]) {
      const { execute, asks, dispatched } = readers();
      const output = await execute(name, { ...args, ...(ref ? { ref } : {}) });
      expect(JSON.parse(output)).toMatchObject({
        success: false,
        code: "permission_denied",
        message: expect.stringContaining("permission_denied: read"),
      });
      expect(dispatched).toEqual([]);
      expect(asks).toEqual([
        { permission: "read", patterns: ["secret.ts"], always: ["*"], metadata: {} },
      ]);
    }
  });
}

for (const name of ["aft_outline", "aft_zoom"] as const) {
  test(`OC1 ${name} batch keeps allowed entries and names the read denial`, async () => {
    const { execute, asks, dispatched } = readers();
    const output = await execute(
      name,
      name === "aft_outline"
        ? { target: ["secret.ts", "allowed.ts", "./allowed.ts"] }
        : {
            batch: [
              { target: "secret.ts", symbols: "secret" },
              { target: "allowed.ts", symbols: ["allowed", "other"] },
              { target: "./allowed.ts", symbols: "allowed" },
              { target: "https://example.test/docs", symbols: "Overview" },
            ],
            callgraph: true,
          },
    );
    expect(output).toContain("export function allowed()");
    expect(output).toContain("secret.ts: permission_denied: read");
    expect(JSON.stringify(dispatched)).not.toContain("secret.ts");
    expect(asks.filter((ask) => ask.permission === "read").map((ask) => ask.patterns)).toEqual([
      ["secret.ts"],
      ["allowed.ts"],
    ]);
  });

  test(`OC1 ${name} asks once for an allowed revision file`, async () => {
    const { execute, asks, dispatched } = readers();
    const output = await execute(
      name,
      name === "aft_outline"
        ? { target: "allowed.ts", ref: "HEAD~1" }
        : {
            batch: [
              { target: "allowed.ts", symbols: "allowed" },
              { target: "./allowed.ts", symbols: "other" },
            ],
            callgraph: true,
            ref: "HEAD~1",
          },
    );
    expect(output).toContain("export function allowed()");
    expect(asks).toEqual([
      { permission: "read", patterns: ["allowed.ts"], always: ["*"], metadata: {} },
    ]);
    expect(dispatched[0]?.ref).toBe("HEAD~1");
  });

  test(`OC1 ${name} does not ask read for a URL`, async () => {
    const { execute, asks, dispatched } = readers();
    await execute(
      name,
      name === "aft_outline"
        ? { target: "https://example.test/docs" }
        : { batch: [{ target: "https://example.test/docs", symbols: "Overview" }] },
    );
    expect(dispatched).toHaveLength(1);
    expect(asks).toEqual([]);
  });
}

for (const files of [false, true]) {
  test(`OC1 directory outline files:${files} authorizes every rendered file once`, async () => {
    const root = await mkdtemp(resolve(process.cwd(), ".aft-v1-read-"));
    const allowed = resolve(root, "allowed.ts");
    const secret = resolve(root, "secret.ts");
    const revealed = resolve(root, "revealed.ts");
    try {
      await mkdir(resolve(root, "nested"));
      await Promise.all(
        [allowed, secret, revealed].map((file) => writeFile(file, "export const x = 1;")),
      );
      const { execute, asks, dispatched } = readers((args) =>
        args._exclude_read_paths
          ? { success: true, text: "allowed() revealed()", content_files: [allowed, revealed] }
          : { success: true, text: "allowed() SECRET_CONTENT", content_files: [allowed, secret] },
      );
      const output = await execute("aft_outline", { target: root, files });
      expect(output).toContain("allowed() revealed()");
      expect(output).not.toContain("SECRET_CONTENT");
      expect(output).toContain("secret.ts: permission_denied: read");
      expect(dispatched).toHaveLength(2);
      expect(dispatched[1]?._exclude_read_paths).toEqual([secret]);
      expect(asks.filter((ask) => ask.permission === "read").map((ask) => ask.patterns)).toEqual(
        [allowed, secret, revealed].map((file) => [relative(process.cwd(), file)]),
      );
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
}

for (const name of ["aft_outline", "aft_zoom"] as const) {
  test(`OC1 ${name} refuses missing or malformed content manifests with an AFT upgrade instruction`, async () => {
    for (const content_files of [undefined, [42], [""]]) {
      const { execute } = readers(() => ({ success: true, text: "HELD_SOURCE", content_files }));
      await expect(
        execute(
          name,
          name === "aft_outline"
            ? { target: "." }
            : { batch: [{ target: "allowed.ts", symbols: "allowed" }] },
        ),
      ).rejects.toThrow(
        /requires server content_files.*AFT binary or daemon.*older than this plugin.*Update AFT.*restart the daemon/,
      );
    }
  });
}

test("OC1 zoom refuses a server that repeats read-denied source", async () => {
  const { execute, dispatched } = readers(() => ({
    success: true,
    text: "HELD_SOURCE",
    content_files: [resolve("secret.ts")],
  }));
  await expect(
    execute("aft_zoom", { batch: [{ target: "allowed.ts", symbols: "allowed" }] }),
  ).rejects.toThrow("Zoom server did not exclude a read-denied file");
  expect(dispatched).toHaveLength(2);
});
