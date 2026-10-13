import { expect, spyOn, test } from "bun:test";
import { readFile, writeFile } from "node:fs/promises";
import type { ToolContext } from "@opencode-ai/plugin";
import { hoistedTools } from "../../tools/hoisted.js";
import type { PluginContext } from "../../types.js";
import { createHarness, prepareBinary } from "./helpers.js";

test("A14 real hashline preflight and apply preserve validation move and undo", async () => {
  const h = await createHarness(await prepareBinary(), {
    fixtureNames: [],
    configOverrides: {
      edit_slot_survives: true,
      config: [
        {
          tier: "project",
          source: "a14-hashline.jsonc",
          doc: JSON.stringify({
            edit_mode: "hashline",
            indexes: { trigram: false, semantic: false },
          }),
        },
      ],
    },
  });
  const sessionID = "a14-real-hashline";
  const ctx = {
    pool: { getBridge: () => h.bridge },
    client: {},
    config: { edit_mode: "hashline" },
    hashlineEffective: true,
    storageDir: h.path(".aft-storage"),
  } as unknown as PluginContext;
  const permissions: Array<Record<string, unknown>> = [];
  const runtime = {
    sessionID,
    messageID: "a14",
    agent: "test",
    directory: h.tempDir,
    worktree: h.tempDir,
    abort: new AbortController().signal,
    metadata() {},
    ask: async (request: Record<string, unknown>) => {
      permissions.push(request);
    },
  } as unknown as ToolContext;
  const tool = hoistedTools(ctx).edit;
  const original = h.bridge.toolCall.bind(h.bridge);
  const responses: Record<string, unknown>[] = [];
  const calls = spyOn(h.bridge, "toolCall").mockImplementation(async (...args) => {
    const result = await original(...args);
    responses.push(result);
    return result;
  });
  async function tag() {
    const response = await h.bridge.toolCall(sessionID, "read", { path: "sample.txt" });
    expect(response.hashline_tag).toBeString();
    calls.mockClear();
    responses.length = 0;
    return response.hashline_tag;
  }
  function expectTwoCalls() {
    expect(calls.mock.calls.map((args) => [args[1], args[3]?.preview === true])).toEqual([
      ["hashline_preflight", false],
      ["edit", false],
    ]);
    expect(calls.mock.calls.map((args) => args[0])).toEqual([sessionID, sessionID]);
    expect(permissions.at(-1)?.permission).toBe("edit");
  }
  try {
    await writeFile(h.path("sample.txt"), "alpha\nbeta\ngamma\ndelta\nepsilon\n");
    let snapshot = await tag();
    await tool.execute({ patch: `[sample.txt#${snapshot}]\nPUT 1:\n+omega` }, runtime);
    expectTwoCalls();
    expect(await readFile(h.path("sample.txt"), "utf8")).toBe(
      "omega\nbeta\ngamma\ndelta\nepsilon\n",
    );
    expect(responses[1].op_id).toBeString();
    expect(responses[1].final_tags).toBeArray();
    const undo = await h.bridge.send("undo", { session_id: sessionID });
    expect(undo.success).toBe(true);
    expect(await readFile(h.path("sample.txt"), "utf8")).toBe(
      "alpha\nbeta\ngamma\ndelta\nepsilon\n",
    );

    snapshot = await tag();
    await writeFile(h.path("sample.txt"), "alpha\nbeta\ngamma\ndelta\nexternal\n");
    await expect(
      tool.execute(
        { patch: `[sample.txt#${snapshot}]\nCUT 1\n[sample.txt#${snapshot}]\nCUT 5` },
        runtime,
      ),
    ).rejects.toThrow();
    expectTwoCalls();
    expect(responses[1].code).toBe("hashline_stale_tag");
    expect(await readFile(h.path("sample.txt"), "utf8")).toBe(
      "alpha\nbeta\ngamma\ndelta\nexternal\n",
    );

    snapshot = await tag();
    await expect(
      tool.execute({ patch: `[sample.txt#${snapshot}]\nPUT 99:\n+invalid` }, runtime),
    ).rejects.toThrow();
    expectTwoCalls();
    expect(await readFile(h.path("sample.txt"), "utf8")).toBe(
      "alpha\nbeta\ngamma\ndelta\nexternal\n",
    );

    snapshot = await tag();
    await tool.execute(
      { patch: `[sample.txt#${snapshot}]\nPUT 1:\n+moved\n[sample.txt#${snapshot}]\nMV moved.txt` },
      runtime,
    );
    expectTwoCalls();
    expect(await readFile(h.path("moved.txt"), "utf8")).toBe(
      "moved\nbeta\ngamma\ndelta\nexternal\n",
    );
    const moveUndo = await h.bridge.send("undo", { session_id: sessionID });
    expect(moveUndo.success).toBe(true);
    expect(await readFile(h.path("sample.txt"), "utf8")).toBe(
      "alpha\nbeta\ngamma\ndelta\nexternal\n",
    );
  } finally {
    calls.mockRestore();
    await h.cleanup();
  }
}, 60_000);
