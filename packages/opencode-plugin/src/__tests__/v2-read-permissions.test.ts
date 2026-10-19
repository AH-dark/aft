import { expect, test } from "bun:test";
import { realpathSync } from "node:fs";
import { mkdir, mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import { Effect } from "effect";
import { requestPermission } from "../permissions/v2.js";
import { projectV2Tool } from "../tools/definitions/v2.js";
import { hoistedV2ToolConsumers } from "../tools/hoisted/v2.js";
import { inspectTools } from "../tools/inspect.js";
import { navigationTools } from "../tools/navigation.js";
import { readingTools } from "../tools/reading.js";
import type { PluginContext } from "../types.js";
import { permissionHost } from "./v2-permission-host.js";

const context = {
  sessionID: "ses_test",
  messageID: "msg_test",
  id: "call_test",
  agent: "build",
  progress: () => Effect.void,
};
const request = {
  permission: "read",
  patterns: ["secret-dir/file.ts"],
  always: ["*"],
  metadata: {},
};

test("host observer sees allow deny and ask in order without prompting for allow", async () => {
  const observed: string[] = [];
  const { host, calls, prompts } = permissionHost({
    agentRules: [
      { action: "read", resource: "allowed.ts", effect: "allow" },
      { action: "read", resource: "denied.ts", effect: "deny" },
      { action: "read", resource: "ask.ts", effect: "ask" },
    ],
    evaluate: (input, effect) => {
      observed.push(`${input.resources[0]}:${effect}`);
      return effect;
    },
  });
  expect(observed).toEqual([]);
  const unrun = host.permission.assert({
    sessionID: "ses_test" as never,
    action: "read",
    resources: ["allowed.ts"],
    save: ["*"],
    metadata: {},
  });
  expect(Effect.isEffect(unrun)).toBe(true);
  expect(calls).toEqual([]);
  expect(observed).toEqual([]);
  for (const file of ["allowed.ts", "denied.ts", "ask.ts"]) {
    const pending = requestPermission(host, { ...request, patterns: [file] }, context);
    if (file === "denied.ts") await expect(pending).rejects.toThrow(/Permission denied/);
    else await pending;
  }
  expect(calls.map((input) => input.resources[0])).toEqual(["allowed.ts", "denied.ts", "ask.ts"]);
  expect(observed).toEqual(["allowed.ts:allow", "ask.ts:ask"]);
  expect(prompts.map((input) => input.resources)).toEqual([["ask.ts"]]);
});

function readers(
  evaluate?: NonNullable<Parameters<typeof permissionHost>[0]>["evaluate"],
  response?: (args: Record<string, unknown>) => Record<string, unknown>,
) {
  const host = permissionHost({
    agentRules: [
      { action: "*", resource: "*", effect: "allow" },
      { action: "read", resource: "secret-dir/*", effect: "deny" },
    ],
    evaluate,
  });
  const dispatched: Record<string, unknown>[] = [];
  const bridge = {
    toolCall: async (_session: unknown, _name: string, args: Record<string, unknown>) => {
      dispatched.push(args);
      return response?.(args) ?? { success: true, text: "export function allowed() {}" };
    },
  };
  const plugin = {
    pool: { getBridge: () => bridge } as unknown as BridgePool,
    client: {},
    config: {},
    storageDir: "/isolated/storage",
  } as PluginContext;
  const definitions = {
    ...readingTools(plugin),
    ...navigationTools(plugin),
    ...inspectTools(plugin),
  };
  const execute = (name: string, args: Record<string, unknown>) =>
    Effect.runPromise(
      projectV2Tool(
        name,
        definitions[name]!,
        { directory: process.cwd() },
        hoistedV2ToolConsumers(host.host),
      ).execute(args, context),
    );
  return { ...host, execute, dispatched };
}

test("canonical deny enforced only by a host evaluate hook blocks an AFT tool", async () => {
  const root = await mkdtemp(resolve(process.cwd(), ".aft-canonical-read-"));
  try {
    const canonical = resolve(root, "private.ts");
    const alias = resolve(root, "alias.ts");
    await writeFile(canonical, "export function privateSymbol() {}\n");
    await symlink(canonical, alias);
    const { execute, dispatched } = readers((input, effect) =>
      input.action === "read" &&
      realpathSync(resolve(process.cwd(), input.resources[0]!)) === realpathSync(canonical)
        ? "deny"
        : effect,
    );
    await expect(execute("aft_zoom", { path: alias, symbols: "privateSymbol" })).rejects.toThrow(
      "permission_denied: read",
    );
    expect(dispatched).toEqual([]);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("zoom refuses a read-denied file and still answers an allowed file", async () => {
  const { execute, calls, dispatched } = readers();
  await expect(
    execute("aft_zoom", { path: "secret-dir/file.ts", symbols: "secret" }),
  ).rejects.toThrow("permission_denied: read");
  expect(dispatched).toEqual([]);
  expect(await execute("aft_zoom", { path: "allowed.ts", symbols: "allowed" })).toMatchObject({
    content: "export function allowed() {}",
  });
  expect(calls.map((input) => input.resources)).toEqual([["secret-dir/file.ts"], ["allowed.ts"]]);
});

test("outline refuses a read-denied file and still answers an allowed file", async () => {
  const { execute, calls, dispatched } = readers();
  await expect(execute("aft_outline", { target: "secret-dir/file.ts" })).rejects.toThrow(
    "permission_denied: read",
  );
  expect(dispatched).toEqual([]);
  expect(await execute("aft_outline", { target: "allowed.ts" })).toMatchObject({
    content: "export function allowed() {}",
  });
  expect(calls.map((input) => input.resources)).toEqual([["secret-dir/file.ts"], ["allowed.ts"]]);
});

test("source readers preserve native read resources outside the project", async () => {
  const { execute, calls } = readers();
  const external = resolve(process.cwd(), "../external-source.ts");
  await execute("aft_zoom", { path: external, symbols: "allowed" });
  expect(calls.find((call) => call.action === "read")).toMatchObject({
    resources: [external],
    save: ["*"],
    source: { type: "tool", messageID: "msg_test", id: "call_test" },
  });
  expect(calls.find((call) => call.action === "read")).not.toHaveProperty("metadata");
});

test("read permissions use native slash-separated resources and omit empty metadata", async () => {
  const { host, calls } = permissionHost({
    agentRules: [{ action: "*", resource: "*", effect: "allow" }],
  });
  await requestPermission(host, { ...request, patterns: ["src\\file.ts"] }, context);
  expect(calls[0]?.resources).toEqual(["src/file.ts"]);
  expect(calls[0]).not.toHaveProperty("metadata");
});

test("zoom batch authorizes distinct files and keeps allowed lookups after a denial", async () => {
  const { execute, calls, dispatched } = readers();
  const result = await execute("aft_zoom", {
    path: "allowed.ts",
    symbols: "allowed",
    targets: [
      { path: "./allowed.ts", symbol: "other" },
      { path: "secret-dir/file.ts", symbol: "secret" },
      { path: "allowed.ts", symbol: "allowed" },
    ],
  });
  expect(calls.map((call) => call.resources)).toEqual([["allowed.ts"], ["secret-dir/file.ts"]]);
  expect(dispatched).toEqual([
    {
      targets: [
        { filePath: "./allowed.ts", symbol: "other" },
        { filePath: "allowed.ts", symbol: "allowed" },
      ],
      filePath: "allowed.ts",
      symbols: "allowed",
    },
  ]);
  expect(String(result.content)).toContain("export function allowed()");
  expect(String(result.content)).toContain("permission_denied: read");
  expect(JSON.stringify(dispatched)).not.toContain("secret-dir");
});

test("directory outline holds denied content and re-renders only allowed files", async () => {
  const root = await mkdtemp(resolve(process.cwd(), ".aft-directory-read-"));
  const allowed = resolve(root, "allowed.ts");
  const secret = resolve(root, "secret-dir/file.ts");
  try {
    await mkdir(resolve(root, "secret-dir"));
    await writeFile(allowed, "export function allowed() {}\n");
    await writeFile(secret, "export function secret() {}\n");
    for (const files of [false, true]) {
      const { execute, calls, dispatched } = readers(
        (input, effect) =>
          input.action === "read" &&
          input.resources.some((file) => file.endsWith("secret-dir/file.ts"))
            ? "deny"
            : effect,
        (args) =>
          args._exclude_read_paths
            ? { success: true, text: "allowed()", content_files: [allowed] }
            : { success: true, text: "allowed() SECRET_CONTENT", content_files: [allowed, secret] },
      );
      const result = await execute("aft_outline", { target: root, files });
      expect(result.content).toBe("allowed()\n1 files left out by read rules.");
      expect(dispatched).toHaveLength(2);
      expect(dispatched[1]?._exclude_read_paths).toEqual([secret]);
      expect(calls.filter((call) => call.action === "read")).toHaveLength(2);
      expect(String(result.content)).not.toContain("secret-dir");
    }
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("directory outline with every content file denied refuses like read", async () => {
  const root = await mkdtemp(resolve(process.cwd(), ".aft-denied-directory-"));
  try {
    const { execute, dispatched } = readers(
      (input, effect) => (input.action === "read" ? "deny" : effect),
      () => ({ success: true, text: "SECRET", content_files: [resolve(root, "secret.ts")] }),
    );
    await expect(execute("aft_outline", { target: root })).rejects.toThrow(
      "permission_denied: read",
    );
    expect(dispatched).toHaveLength(1);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("explicit-file callgraph and inspect scopes check read before returning snippets", async () => {
  const { execute, dispatched } = readers();
  await expect(
    execute("aft_callgraph", { path: "secret-dir/file.ts", symbol: "secret", op: "callers" }),
  ).rejects.toThrow("permission_denied: read");
  await expect(
    execute("aft_inspect", { scope: "secret-dir/file.ts", sections: ["todos"] }),
  ).rejects.toThrow("permission_denied: read");
  expect(dispatched).toEqual([]);
});
