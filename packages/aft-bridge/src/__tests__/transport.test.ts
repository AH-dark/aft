/// <reference path="../bun-test.d.ts" />
import { describe, expect, test } from "bun:test";

import { BinaryBridge, type BridgeRequestOptions } from "../bridge.js";

class MockBridge extends BinaryBridge {
  readonly calls: Array<{
    command: string;
    params: Record<string, unknown>;
    options: BridgeRequestOptions | undefined;
  }> = [];

  constructor(private readonly response: Record<string, unknown>) {
    super("/tmp/aft-does-not-need-to-exist", process.cwd());
  }

  override async send(
    command: string,
    params: Record<string, unknown> = {},
    options?: BridgeRequestOptions,
  ): Promise<Record<string, unknown>> {
    this.calls.push({ command, params, options });
    return this.response;
  }
}

describe("BinaryBridge toolCall transport", () => {
  test("sends the tool_call envelope and returns the raw response", async () => {
    const rawResponse = {
      id: "42",
      success: true,
      text: "agent output",
      bg_completions: [{ task_id: "bg-1", status: "completed", exit_code: 0, command: "echo ok" }],
      preview_diff: "diff --git a/file b/file",
    };
    const bridge = new MockBridge(rawResponse);
    const options = { transportTimeoutMs: 1234 };

    const result = await bridge.toolCall(
      "session-123",
      "read",
      { filePath: "README.md", offset: 10 },
      options,
    );

    expect(bridge.calls).toEqual([
      {
        command: "tool_call",
        params: {
          name: "read",
          arguments: { filePath: "README.md", offset: 10 },
          session_id: "session-123",
        },
        options,
      },
    ]);
    expect(result).toBe(rawResponse);
    expect(result.text).toBe("agent output");
    expect(result).not.toHaveProperty("status_bar");
    expect(result.bg_completions).toEqual(rawResponse.bg_completions);
    expect(result.preview_diff).toBe(rawResponse.preview_diff);
  });

  test("puts the worker role in the tool_call envelope beside the session", async () => {
    const bridge = new MockBridge({ id: "1", success: true, text: "ok" });

    await bridge.toolCall("session-123", "bash", { command: "make" }, { workerSession: true });

    expect(bridge.calls[0]?.params).toEqual({
      name: "bash",
      arguments: { command: "make" },
      session_id: "session-123",
      worker_session: true,
    });
    // worker_session is part of the request, not a transport option.
    expect(bridge.calls[0]?.options).toBeUndefined();
  });

  test("puts the host's tool names in the tool_call envelope beside the session", async () => {
    const bridge = new MockBridge({ id: "1", success: true, text: "ok" });
    const names = { bash: "shell", apply_patch: "patch" };

    await bridge.toolCall("session-123", "edit", { patch: "p" }, { hostToolNames: names });
    await bridge.toolCall("session-456", "edit", { patch: "p" });

    expect(bridge.calls[0]?.params).toEqual({
      name: "edit",
      arguments: { patch: "p" },
      session_id: "session-123",
      host_tool_names: names,
    });
    expect(bridge.calls[0]?.options).toBeUndefined();
    // A call that names no tools sends none, and AFT keeps its own names.
    expect(bridge.calls[1]?.params).not.toHaveProperty("host_tool_names");
  });
});
