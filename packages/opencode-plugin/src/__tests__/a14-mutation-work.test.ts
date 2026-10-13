import { expect, spyOn, test } from "bun:test";
import * as fs from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  RevivableTransportPool,
  SubcTransportPool,
  type ToolCallOptions,
} from "@cortexkit/aft-bridge";
import type { ToolContext } from "@opencode-ai/plugin";
import { callBridge, callToolCall } from "../tools/_shared.js";
import { hoistedTools } from "../tools/hoisted.js";
import type { PluginContext } from "../types.js";

type SubcClientLike = Awaited<
  ReturnType<NonNullable<ConstructorParameters<typeof SubcTransportPool>[0]["connect"]>>
>;

test("A14 OpenCode dispatch reuses the host root without caching symlink identity", async () => {
  const temp = fs.realpathSync(fs.mkdtempSync(join(tmpdir(), "aft-a14-host-")));
  const main = join(temp, "main");
  const linked = join(temp, "linked");
  const alias = join(temp, "alias");
  fs.mkdirSync(main);
  fs.mkdirSync(linked);
  fs.symlinkSync(linked, alias);
  const roots: string[] = [];
  const client = {
    routeOpen: async (_target: unknown, identity: { project_root: string }) => {
      roots.push(identity.project_root);
      return { channel: roots.length, epoch: 1 };
    },
    request: async () => ({ structuredContent: { success: true, text: "ok" } }),
    subscribe: () => ({ unsubscribe() {} }),
    closeRouteChannel: async () => {},
    close() {},
  } as SubcClientLike;
  const pool = new RevivableTransportPool(
    new SubcTransportPool({
      connectionFile: "/unused/a14-connection.json",
      harness: "opencode",
      connect: async () => client,
    }),
    async () => {
      throw new Error("unexpected revival");
    },
  );
  const ctx = { pool, client: {}, config: {}, storageDir: temp } as unknown as PluginContext;
  const runtime = { directory: alias, worktree: main, directoryIsSessionRoot: true };
  const realpaths = spyOn(fs, "realpathSync");
  try {
    expect((await callToolCall(ctx, runtime, "read", { path: "missing-parent/new.ts" })).text).toBe(
      "ok",
    );
    expect((await callBridge(ctx, runtime, "status")).text).toBe("ok");
    expect(roots).toEqual([linked]);
    expect(realpaths.mock.calls.length).toBe(2);
    fs.unlinkSync(alias);
    fs.symlinkSync(main, alias);
    await callToolCall(ctx, runtime, "read", { path: "new.ts" });
    expect(roots).toEqual([linked, main]);
    fs.rmSync(main, { recursive: true });
    await expect(callBridge(ctx, runtime, "status")).rejects.toThrow(
      "project directory no longer exists",
    );
    fs.mkdirSync(main);
    expect((await callBridge(ctx, runtime, "status")).text).toBe("ok");
  } finally {
    realpaths.mockRestore();
    await pool.shutdown();
    fs.rmSync(temp, { recursive: true, force: true });
  }
});

test("A14 preview without full texts keeps approval patch and post-format final UI", async () => {
  const root = fs.realpathSync(fs.mkdtempSync(join(tmpdir(), "aft-a14-ui-")));
  const before = "const value=1;\r\n";
  const after = "const value = 2;\r\n";
  const approvalPatch =
    "Index: value.ts\n--- value.ts\n+++ value.ts\n@@ -1 +1 @@\n-const value=1;\n+const value=2;\n";
  const requests: boolean[] = [];
  const wireBytes: number[] = [];
  const asks: Array<Record<string, unknown>> = [];
  const bridge = {
    async toolCall(
      _session: string | undefined,
      _name: string,
      _args: unknown,
      options?: ToolCallOptions,
    ) {
      requests.push(options?.preview === true);
      const response = options?.preview
        ? {
            success: true,
            text: "preview",
            preview_diff: approvalPatch,
            diff: { additions: 1, deletions: 1 },
          }
        : { success: true, text: "applied", diff: { before, after, additions: 1, deletions: 1 } };
      wireBytes.push(Buffer.byteLength(JSON.stringify(response)));
      return response;
    },
  };
  const ctx = {
    pool: { getBridge: () => bridge },
    client: {},
    config: {},
    storageDir: root,
  } as unknown as PluginContext;
  const runtime = {
    directory: root,
    worktree: root,
    sessionID: "a14-ui",
    messageID: "a14",
    agent: "a14",
    abort: new AbortController().signal,
    metadata() {},
    ask: async (request: Record<string, unknown>) => {
      asks.push(request);
    },
  } as unknown as ToolContext;
  try {
    const result = await hoistedTools(ctx).edit.execute(
      { path: "value.ts", edits: [{ oldString: "1", newString: "2" }] },
      runtime,
    );
    expect(requests).toEqual([true, false]);
    expect(wireBytes.length).toBe(2);
    expect(asks.find((request) => request.permission === "edit")?.metadata).toEqual({
      diff: approvalPatch,
      filepath: join(root, "value.ts"),
    });
    const patch = String(result.metadata.diff);
    expect(patch).toContain("+const value = 2;");
    expect(patch).not.toBe(approvalPatch);
    expect(result.output).toBe("applied");
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});
