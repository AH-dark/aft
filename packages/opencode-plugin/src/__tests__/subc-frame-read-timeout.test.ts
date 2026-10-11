/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import {
  BASH_FRAME_READ_TIMEOUT_DISPOSITION,
  BASH_TRANSPORT_DISPOSITION,
  SUBC_FRAME_READ_TIMEOUT_DISPOSITION,
  SubcFrameReadTimeoutError,
  SubcTransportPool,
} from "@cortexkit/aft-bridge";
import {
  decodeHeader,
  FrameType,
  HEADER_LEN,
  PROTOCOL_VERSION,
  SocketClosedError,
  SocketTimeoutError,
  SubcClient,
} from "@cortexkit/subc-client";
import type { ToolContext } from "@opencode-ai/plugin";
import { callBridge, callToolCall } from "../tools/_shared.js";
import { createBashTool } from "../tools/bash.js";
import { createReadTool } from "../tools/hoisted.js";
import type { PluginContext } from "../types.js";
import { noopAsk } from "./test-helpers.js";

class FakeStallingSocket {
  private readonly written: Uint8Array[] = [];
  private closed = false;

  bufferedBytes(): number {
    return 0;
  }

  localPort(): number {
    return 54321;
  }

  close(): void {
    this.closed = true;
  }

  async write(bytes: Uint8Array): Promise<void> {
    this.written.push(bytes);
  }

  writeTracked(bytes: Uint8Array): { queued: boolean; completed: Promise<void> } {
    this.written.push(bytes);
    return { queued: true, completed: Promise.resolve() };
  }

  async readFrame(
    _headerDeadlineMs: number,
    _bodyDeadline: number | { afterHeaderMs: number },
    onHeader?: () => void,
  ) {
    while (this.written.length === 0) {
      if (this.closed) throw new SocketClosedError("socket closed");
      await new Promise((resolve) => setTimeout(resolve, 5));
    }
    const chunk = this.written.shift()!;
    const header = decodeHeader(new Uint8Array(chunk.subarray(0, HEADER_LEN)));

    if (header.channel === 0) {
      const body = new TextEncoder().encode(
        JSON.stringify({ op: "route.open", route_channel: 1, route_epoch: 1 }),
      );
      return {
        header: {
          version: PROTOCOL_VERSION,
          flags: 0,
          ty: FrameType.Response,
          channel: 0,
          epoch: 0,
          corr: header.corr,
          len: body.length,
        },
        body,
      };
    }

    onHeader?.();
    await new Promise((resolve) => setTimeout(resolve, 10));
    throw new SocketTimeoutError("timed out waiting for 359 bytes");
  }
}

describe("OpenCode plugin subc frame read timeout handling", () => {
  const cleanups: Array<() => Promise<void> | void> = [];
  afterEach(async () => {
    while (cleanups.length > 0) await cleanups.pop()?.();
  });

  function createStallingPluginContext(options: { hostFallback?: boolean } = {}): {
    ctx: PluginContext;
    pool: SubcTransportPool;
  } {
    const socket = new FakeStallingSocket();
    const conn = {
      schema: 1,
      wire_version: 2,
      daemon_id: new Uint8Array(16).fill(1),
      endpoints: [{ host: "127.0.0.1", port: 12345 }],
    };
    const client = new (
      SubcClient as unknown as new (
        s: FakeStallingSocket,
        c: typeof conn,
        o: { connectionFile: string },
      ) => SubcClient
    )(socket, conn, { connectionFile: "/tmp/fake-conn.json" });

    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake-conn.json",
      harness: "opencode",
      consumerIdentity: null,
      connect: async () => client,
    });
    cleanups.push(() => pool.shutdown());

    const ctx: PluginContext = {
      pool: pool as unknown as PluginContext["pool"],
      client: {
        lsp: { status: async () => ({ data: [] }) },
        find: { symbols: async () => ({ data: [] }) },
      } as unknown as PluginContext["client"],
      config: {
        bash: {
          host_fallback: options.hostFallback ?? true,
        },
      } as unknown as PluginContext["config"],
      storageDir: "/tmp/aft-test",
    };
    return { ctx, pool };
  }

  function mockRuntime(): ToolContext {
    return {
      sessionID: "test-session",
      messageID: "test-message",
      agent: "test-agent",
      directory: process.cwd(),
      worktree: process.cwd(),
      abort: new AbortController().signal,
      metadata: () => {},
      ask: noopAsk,
      callID: "test-call",
    } as unknown as ToolContext;
  }

  test("callBridge for bash maps a frame-read timeout to typed outcome-unknown error", async () => {
    const { ctx } = createStallingPluginContext();
    const runtime = mockRuntime();

    let thrown: unknown;
    try {
      await callBridge(ctx, runtime, "bash", { command: "touch marker" });
    } catch (error) {
      thrown = error;
    }

    expect(thrown).toBeInstanceOf(SubcFrameReadTimeoutError);
    const err = thrown as SubcFrameReadTimeoutError;
    expect(err.kind).toBe("outcome_unknown");
    expect(err.code).toBe("frame_read_timeout");
    expect(err.message).toContain(BASH_FRAME_READ_TIMEOUT_DISPOSITION);
    expect(err.message).toContain("UNKNOWN");
    expect(err.message).toContain("bash_status");
    expect(err.message).toContain("do not re-run side-effecting commands blindly");
    expect(err.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
  });

  test("callToolCall for read maps a frame-read timeout to typed outcome-unknown error", async () => {
    const { ctx } = createStallingPluginContext();
    const runtime = mockRuntime();

    let thrown: unknown;
    try {
      await callToolCall(ctx, runtime, "read", { file: "foo.txt" });
    } catch (error) {
      thrown = error;
    }

    expect(thrown).toBeInstanceOf(SubcFrameReadTimeoutError);
    const err = thrown as SubcFrameReadTimeoutError;
    expect(err.kind).toBe("outcome_unknown");
    expect(err.code).toBe("frame_read_timeout");
    expect(err.message).toContain(SUBC_FRAME_READ_TIMEOUT_DISPOSITION);
    expect(err.message).toContain("UNKNOWN");
    expect(err.message).toContain("never blind-retry a mutation");
    expect(err.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
    expect(err.message).not.toContain(BASH_FRAME_READ_TIMEOUT_DISPOSITION);
  });

  test("createBashTool execute rejects with typed outcome-unknown without host fallback", async () => {
    // Even with host_fallback: true, a mid-call frame timeout must NEVER trigger host fallback
    const { ctx } = createStallingPluginContext({ hostFallback: true });
    let askCalled = false;
    const runtime = {
      ...mockRuntime(),
      ask: async () => {
        askCalled = true;
      },
    } as unknown as ToolContext;

    const bashTool = createBashTool(ctx);
    let thrown: unknown;
    try {
      await bashTool.execute({ command: "touch marker" }, runtime);
    } catch (error) {
      thrown = error;
    }

    expect(askCalled).toBe(false);
    expect(thrown).toBeInstanceOf(SubcFrameReadTimeoutError);
    const err = thrown as SubcFrameReadTimeoutError;
    expect(err.message).toContain(BASH_FRAME_READ_TIMEOUT_DISPOSITION);
    expect(err.message).not.toContain("timed out waiting for 359 bytes\n");
  });

  test("createReadTool rejects with typed outcome-unknown error", async () => {
    const { ctx } = createStallingPluginContext();
    const runtime = mockRuntime();
    const readTool = createReadTool(ctx);

    let thrown: unknown;
    try {
      await readTool.execute({ filePath: "foo.txt" }, runtime);
    } catch (error) {
      thrown = error;
    }

    expect(thrown).toBeInstanceOf(SubcFrameReadTimeoutError);
    const err = thrown as SubcFrameReadTimeoutError;
    expect(err.message).toContain(SUBC_FRAME_READ_TIMEOUT_DISPOSITION);
    expect(err.message).toContain("UNKNOWN");
  });
});
