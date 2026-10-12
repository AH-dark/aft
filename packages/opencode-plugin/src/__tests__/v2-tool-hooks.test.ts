import { expect, test } from "bun:test";
import { Effect } from "effect";

import { currentSessionVisionCapability, rememberReadModel } from "../shared/read-vision.js";
import { bootV2Runtime } from "./helpers/v2-runtime.js";

test("V2 execute.before normalizes path aliases before the registered schema", async () => {
  const runtime = await bootV2Runtime();
  const before = runtime.toolHooks.get("execute.before");
  expect(before).toBeDefined();
  for (const name of ["read", "write", "edit"]) {
    const input =
      name === "write"
        ? { filePath: "file.ts", content: "new" }
        : name === "edit"
          ? { filePath: "file.ts", oldString: "old", newString: "new" }
          : { filePath: "file.ts" };
    const event = { tool: name, input };
    await Effect.runPromise(before!(event));
    expect(event.input).toHaveProperty("path", "file.ts");
    expect(runtime.tools.get(name).input.safeParse(event.input).success).toBe(true);
  }
  const foreign = { tool: "foreign", input: { filePath: "keep" } };
  await Effect.runPromise(before!(foreign));
  expect(foreign.input).toEqual({ filePath: "keep" });
  // Unknown edit fields must not be silently stripped by the host decoder.
  const invalid = {
    tool: "edit",
    input: { path: "file.ts", oldString: "old", newString: "new", unexpected: true },
  };
  expect((await Effect.runPromiseExit(before!(invalid)))._tag).toBe("Failure");
});

// OpenCode 2 names AFT's bash `shell` in its hook events.
test("V2 execute.after appends the conflicts hint only to completed shell text", async () => {
  const runtime = await bootV2Runtime();
  const after = runtime.toolHooks.get("execute.after");
  expect(after).toBeDefined();
  const text =
    "CONFLICT (content): file.ts\nAutomatic merge failed; fix conflicts and then commit the result.";
  const event = {
    tool: "shell",
    status: "completed",
    result: { content: [{ type: "text", text }], metadata: { exit: 1 } },
  };
  await Effect.runPromise(after!(event));
  expect(event.result.content[0]?.text).toContain("[Hint] Use aft_conflicts");
  expect(event.result.metadata).toEqual({ exit: 1 });
  const failure = { tool: "shell", status: "error", error: { message: text } };
  await Effect.runPromise(after!(failure));
  expect(failure.error.message).toBe(text);
});

test("V2 context model recording follows model switches for image reads", async () => {
  const runtime = await bootV2Runtime();
  const context = runtime.sessionHooks.get("context");
  expect(context).toBeDefined();
  for (const [id, expected] of [
    ["vision", true],
    ["text", false],
  ] as const) {
    await Effect.runPromise(
      context!({ sessionID: "s", model: { providerID: "p", id }, system: [], tools: {} }),
    );
    expect(await currentSessionVisionCapability(runtime.context, "s")).toBe(expected);
  }
});

test("V2 vision lookup runs provider.list and reads native capabilities", async () => {
  const runtime = await bootV2Runtime();
  rememberReadModel(runtime.context, "s", { providerID: "p", modelID: "vision" });
  expect(await currentSessionVisionCapability(runtime.context, "s")).toBe(true);
});

// The request's tool draft is keyed by the names the host registered, so AFT's
// apply_patch and bash appear as `patch` and `shell`.
for (const hook of ["context", "compaction", "generate"]) {
  for (const [id, expected] of [
    ["gpt-5.5", ["foreign", "patch", "shell"]],
    ["gpt-4o", ["edit", "foreign", "shell", "write"]],
    ["gpt-oss-120b", ["edit", "foreign", "shell", "write"]],
    ["claude-sonnet-4-5", ["edit", "foreign", "shell", "write"]],
  ] as const) {
    test(`V2 ${hook} gates AFT editing tools for ${id}`, async () => {
      const runtime = await bootV2Runtime();
      const callback = runtime.sessionHooks.get(hook);
      expect(callback).toBeDefined();
      const tools = Object.fromEntries(
        ["edit", "write", "patch", "shell", "foreign"].map((name) => [name, { name }]),
      );
      await Effect.runPromise(
        callback!({ sessionID: "s", model: { providerID: "p", id }, system: [], tools }),
      );
      expect(Object.keys(tools).sort()).toEqual(expected);
    });
  }

  test(`V2 ${hook} leaves host editing tools alone when AFT did not register them`, async () => {
    const runtime = await bootV2Runtime({ disabled_tools: ["edit", "write", "apply_patch"] });
    const callback = runtime.sessionHooks.get(hook);
    expect(callback).toBeDefined();
    const hostEdit = { description: "Host edit" };
    for (const id of ["gpt-5.5", "claude-sonnet-4-5"]) {
      const tools = { edit: hostEdit, write: {}, apply_patch: {}, patch: {} };
      await Effect.runPromise(
        callback!({ sessionID: "s", model: { providerID: "p", id }, system: [], tools }),
      );
      expect(Object.keys(tools).sort()).toEqual(["apply_patch", "edit", "patch", "write"]);
      expect(tools.edit).toBe(hostEdit);
    }
  });

  test(`V2 ${hook} keeps host edit while gating AFT write for GPT`, async () => {
    const runtime = await bootV2Runtime({ disabled_tools: ["edit"] });
    expect(runtime.tools.has("edit")).toBe(false);
    const hostEdit = { description: "Host edit" };
    const tools = { edit: hostEdit, write: {}, apply_patch: {}, patch: {} };
    await Effect.runPromise(
      runtime.sessionHooks.get(hook)!({
        sessionID: "s",
        model: { providerID: "p", id: "gpt-5.5" },
        system: [],
        tools,
      }),
    );
    expect(Object.keys(tools).sort()).toEqual(["apply_patch", "edit", "patch"]);
    expect(tools.edit).toBe(hostEdit);
  });
}

test("V2 apply_patch, registered as patch, uses the host edit permission action", async () => {
  const runtime = await bootV2Runtime();
  expect(runtime.tools.get("patch").options.permission).toBe("edit");
});
