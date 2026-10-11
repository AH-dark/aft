/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, test } from "bun:test";
import {
  decodeHeader,
  FrameType,
  HEADER_LEN,
  PROTOCOL_VERSION,
  SocketClosedError,
  SocketTimeoutError,
  SubcClient,
  SubcError,
} from "@cortexkit/subc-client";
import {
  adaptToolError,
  BASH_FRAME_READ_TIMEOUT_DISPOSITION,
  BASH_TRANSPORT_DISPOSITION,
  classifyBashHostFallbackError,
  isBashTransportDeadError,
  isSubcFrameReadTimeoutError,
  SUBC_FRAME_READ_TIMEOUT_DISPOSITION,
  SUBC_MODULE_RESTART_DISPOSITION,
} from "../error-contract.js";
import { SubcFrameReadTimeoutError, SubcTransportPool } from "../subc-transport.js";

/**
 * A fake socket that answers route.open on channel 0, and on any data-plane call
 * delivers a frame header and then stalls before delivering body bytes, throwing
 * SocketTimeoutError("timed out waiting for 359 bytes").
 *
 * Test double contract: no more capable than the real client. It implements only
 * the raw socket surface that SubcClient consumes and delegates all dispatch and
 * error handling to the real SubcClient readLoop.
 */
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
      // Channel 0 route.open response: accept and establish route 1@1.
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

    // Data-plane tool request (bash or read tool):
    // Simulate: frame header arrived, then socket stalled waiting for body bytes.
    onHeader?.();
    await new Promise((resolve) => setTimeout(resolve, 10));
    throw new SocketTimeoutError("timed out waiting for 359 bytes");
  }
}

describe("subc frame read timeout mapping (real SubcClient, fake socket)", () => {
  const cleanups: Array<() => Promise<void> | void> = [];
  afterEach(async () => {
    while (cleanups.length > 0) await cleanups.pop()?.();
  });

  async function createStallingRig(): Promise<{
    pool: SubcTransportPool;
    socket: FakeStallingSocket;
    client: SubcClient;
  }> {
    const socket = new FakeStallingSocket();
    const conn = {
      schema: 1,
      wire_version: 2,
      daemon_id: new Uint8Array(16).fill(1),
      endpoints: [{ host: "127.0.0.1", port: 12345 }],
    };
    const opts = { connectionFile: "/tmp/fake-conn.json" };
    const client = new (
      SubcClient as unknown as new (
        s: FakeStallingSocket,
        c: typeof conn,
        o: typeof opts,
      ) => SubcClient
    )(socket, conn, opts);

    const pool = new SubcTransportPool({
      connectionFile: "/tmp/fake-conn.json",
      harness: "opencode",
      consumerIdentity: null,
      connect: async () => client,
    });
    cleanups.push(() => pool.shutdown());
    return { pool, socket, client };
  }

  test("a frame-read timeout on bash yields the typed outcome-unknown reply", async () => {
    const { pool } = await createStallingRig();
    const bridge = pool.getBridge(process.cwd());

    let thrown: unknown;
    try {
      await bridge.send("bash", { command: "touch marker", session_id: "bash-stall" });
    } catch (error) {
      thrown = error;
    }

    expect(thrown).toBeInstanceOf(SubcFrameReadTimeoutError);
    const typed = thrown as SubcFrameReadTimeoutError;
    expect(typed.kind).toBe("outcome_unknown");
    expect(typed.code).toBe("frame_read_timeout");
    expect(typed.message).toStartWith(
      "the command may have run; timed out waiting for frame body bytes from the AFT daemon",
    );
    expect(typed.cause).toBeInstanceOf(SubcError);
    expect((typed.cause as SubcError).message).toBe("timed out waiting for 359 bytes");
    expect((typed.cause as SubcError).closeReason).toBe("connection_lost");

    // Must never be classified as transport_dead, which would trigger host fallback
    expect(classifyBashHostFallbackError(typed)).toBeUndefined();
    expect(isBashTransportDeadError(typed)).toBe(false);
    expect(isSubcFrameReadTimeoutError(typed)).toBe(true);

    // adaptToolError appends the bash outcome-unknown disposition
    const adapted = adaptToolError("bash", typed) as SubcFrameReadTimeoutError;
    expect(adapted).toBe(typed);
    expect(adapted.message).toContain(BASH_FRAME_READ_TIMEOUT_DISPOSITION);
    expect(adapted.message).toContain("UNKNOWN");
    expect(adapted.message).toContain("bash_status");
    expect(adapted.message).toContain("do not re-run side-effecting commands blindly");
    expect(adapted.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
    expect(adapted.message).not.toContain(SUBC_MODULE_RESTART_DISPOSITION);
  });

  test("a frame-read timeout on a read tool yields the typed outcome-unknown reply", async () => {
    const { pool } = await createStallingRig();
    const bridge = pool.getBridge(process.cwd());

    let thrown: unknown;
    try {
      await bridge.toolCall("read-stall", "read", { file: "test.txt" });
    } catch (error) {
      thrown = error;
    }

    expect(thrown).toBeInstanceOf(SubcFrameReadTimeoutError);
    const typed = thrown as SubcFrameReadTimeoutError;
    expect(typed.kind).toBe("outcome_unknown");
    expect(typed.code).toBe("frame_read_timeout");
    expect(typed.message).toStartWith(
      "the command may have run; timed out waiting for frame body bytes from the AFT daemon",
    );
    expect(typed.cause).toBeInstanceOf(SubcError);
    expect((typed.cause as SubcError).message).toBe("timed out waiting for 359 bytes");
    expect(isSubcFrameReadTimeoutError(typed)).toBe(true);

    // adaptToolError appends the generic outcome-unknown disposition
    const adapted = adaptToolError("read", typed) as SubcFrameReadTimeoutError;
    expect(adapted).toBe(typed);
    expect(adapted.message).toContain(SUBC_FRAME_READ_TIMEOUT_DISPOSITION);
    expect(adapted.message).toContain("UNKNOWN");
    expect(adapted.message).toContain("never blind-retry a mutation");
    expect(adapted.message).not.toContain(BASH_TRANSPORT_DISPOSITION);
    expect(adapted.message).not.toContain(BASH_FRAME_READ_TIMEOUT_DISPOSITION);
  });

  test("red control: raw socket timeout without the frame-read classifier", () => {
    const raw = new SocketTimeoutError("timed out waiting for 359 bytes");
    // With the mapping in place, adaptToolError elevates raw SocketTimeoutError to SubcFrameReadTimeoutError
    const mapped = adaptToolError("bash", raw);
    expect(mapped).toBeInstanceOf(SubcFrameReadTimeoutError);
    expect((mapped as SubcFrameReadTimeoutError).message).toContain("the command may have run");
    expect((mapped as SubcFrameReadTimeoutError).message).toContain(
      BASH_FRAME_READ_TIMEOUT_DISPOSITION,
    );

    // If an error is an unmapped generic error with no frame-read pattern, it remains raw
    const generic = new Error("other network failure");
    const unmapped = adaptToolError("bash", generic);
    expect(unmapped).toBe(generic);
    expect((unmapped as Error).message).toBe("other network failure");
  });
});
