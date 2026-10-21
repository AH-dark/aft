import { describe, expect, mock, test } from "bun:test";
import type { AftProjectTransport } from "@cortexkit/aft-bridge";
import type {
  ExtensionAPI,
  ExtensionContext,
  ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import { bindToolRegistrationFunnel } from "../tool-registration.js";
import { callBridge, callToolCallForSession, withOutsidePermissions } from "../tools/_shared.js";
import type { PluginContext } from "../types.js";

const ask = (id: string) => ({
  success: false,
  code: "permission_required",
  text: "outside access needed",
  asks: [
    {
      kind: "external_directory",
      grant_id: `outside:${id}`,
      path: "/outside/file",
      operation: "read",
    },
  ],
});
const context = (confirm: (...args: unknown[]) => Promise<boolean>, signal?: AbortSignal) =>
  ({ hasUI: true, ui: { confirm }, signal }) as unknown as ExtensionContext;

describe("engine outside-access asks", () => {
  test("registered Pi operation carries undo preview approval through apply", async () => {
    const calls: Array<{ name: string; args: Record<string, unknown> }> = [];
    const answer = (name: string, args: Record<string, unknown>) => {
      calls.push({ name, args: structuredClone(args) });
      const grants = args.permissions_granted as string[] | undefined;
      if (name === "undo_preview")
        return grants?.includes("outside:preview")
          ? { success: true, text: "preview", permissions_continuation: ["outside:apply"] }
          : ask("preview");
      return grants?.includes("outside:apply") || grants?.includes("outside:apply-new")
        ? { success: true, text: "applied" }
        : ask("apply-new");
    };
    const bridge = {
      send: async (name: string, args: Record<string, unknown>) => answer(name, args),
      toolCall: async (_session: unknown, name: string, args: Record<string, unknown>) =>
        answer(name, args),
    } as unknown as AftProjectTransport;
    const prompt = mock(async () => true);
    const extCtx = {
      ...context(prompt),
      sessionManager: { getSessionId: () => "captured" },
    } as unknown as ExtensionContext;
    let registered: ToolDefinition | undefined;
    const pi = bindToolRegistrationFunnel(
      {
        registerTool: (tool: ToolDefinition) => {
          registered = tool;
        },
      } as unknown as ExtensionAPI,
      { config: {} } as unknown as PluginContext,
      "pi",
    );
    pi.registerTool({
      name: "test_undo",
      label: "undo",
      description: "fixture",
      parameters: Type.Object({}),
      execute: async (_id, _params, _signal, _update, ctx) => {
        await callBridge(bridge, "undo_preview", { file: "/outside/a" }, ctx);
        const result = await callToolCallForSession(
          bridge,
          "safety",
          { op: "undo", path: "/outside/a" },
          "captured",
          ctx,
        );
        return { content: [{ type: "text" as const, text: result.text }], details: {} };
      },
    });
    if (!registered) throw new Error("tool not registered");
    await registered.execute("logical-call", {}, new AbortController().signal, undefined, extCtx);
    expect(prompt).toHaveBeenCalledTimes(1);
    expect(calls.find((call) => call.name === "safety")?.args.permissions_granted).toEqual([
      "outside:apply",
    ]);
  });
  test("path tool dispatch prompts and retries in the captured session", async () => {
    const calls: unknown[][] = [];
    const prompt = mock(async () => true);
    const bridge = {
      toolCall: async (...args: unknown[]) => {
        calls.push([args[0], args[1], structuredClone(args[2])]);
        return calls.length === 1
          ? ask("1")
          : calls.length === 2
            ? { ...ask("2"), permissions_continuation: ["outside:renewed-1"] }
            : { success: true, text: "allowed" };
      },
    } as unknown as AftProjectTransport;
    const result = await callToolCallForSession(
      bridge,
      "read",
      { path: "/outside/file" },
      "captured",
      context(prompt),
    );
    expect(result.text).toBe("allowed");
    expect(prompt).toHaveBeenCalledTimes(2);
    expect(calls.map((call) => call[0])).toEqual(["captured", "captured", "captured"]);
    expect(calls[2]?.[2]).toMatchObject({
      permissions_granted: ["outside:renewed-1", "outside:2"],
    });
  });

  test("rawArgs cannot seed forged or replayed permission tokens", async () => {
    const calls: Record<string, unknown>[] = [];
    const prompt = mock(async () => true);
    const bridge = {
      toolCall: async (_session: unknown, _name: string, args: Record<string, unknown>) => {
        calls.push(structuredClone(args));
        return calls.length === 1 ? ask("issued") : { success: true, text: "allowed" };
      },
    } as unknown as AftProjectTransport;
    await callToolCallForSession(
      bridge,
      "read",
      {
        path: "/outside/a",
        permissions_granted: [
          'outside:{"path":"/outside/a","kind":"file","mode":"read"}',
          "outside:old-issued-token",
        ],
      },
      "captured",
      context(prompt),
    );
    expect(calls[0]).not.toHaveProperty("permissions_granted");
    expect(calls[1]?.permissions_granted).toEqual(["outside:issued"]);
    expect(prompt).toHaveBeenCalledTimes(1);
  });

  test("a refusal or missing UI never sends approved IDs", async () => {
    const send = mock(async () => ask("no"));
    await expect(
      withOutsidePermissions(
        {},
        context(async () => false),
        send,
      ),
    ).rejects.toMatchObject({ code: "permission_denied" });
    expect(send).toHaveBeenCalledTimes(1);
    await expect(withOutsidePermissions({}, undefined, send)).rejects.toMatchObject({
      code: "permission_denied",
    });
    expect(send).toHaveBeenCalledTimes(2);
  });

  test("cancellation during confirmation never sends the approved retry", async () => {
    const abort = new AbortController();
    const send = mock(async () => ask("cancelled"));
    await expect(
      withOutsidePermissions(
        {},
        context(async () => {
          abort.abort();
          return true;
        }, abort.signal),
        send,
      ),
    ).rejects.toThrow();
    expect(send).toHaveBeenCalledTimes(1);
  });
});
