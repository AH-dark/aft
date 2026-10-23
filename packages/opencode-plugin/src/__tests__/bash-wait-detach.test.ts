/// <reference path="../bun-test.d.ts" />

import { afterAll, afterEach, beforeAll, describe, expect, mock, spyOn, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  extractUserMessageText,
  interruptBashWaitsForChatMessage,
  shouldDetachBashWaitOnUserMessage,
  signalBashWaitDetachForProject,
  stripUserMessageDetachKeyword,
  waitDetachSummaryLevel,
} from "../bash-wait-detach.js";
import * as logger from "../logger.js";
import { __resetSyncWatchAbortForTests, isSyncWatchAborted } from "../sync-watch-abort.js";

let projectRoot: string;

beforeAll(() => {
  projectRoot = mkdtempSync(join(tmpdir(), "aft-test-repo-"));
});

afterAll(() => {
  rmSync(projectRoot, { recursive: true, force: true });
});

describe("bash wait detach helper", () => {
  test("default config detaches on a plain user message", () => {
    expect(shouldDetachBashWaitOnUserMessage({}, "please continue")).toBe(true);
  });

  test("opt-out keeps a plain message blocking but &detach overrides it", () => {
    const config = { bash: { detach_on_user_message: false } };
    const plain = "please continue";
    const forced = "please &detach continue";

    expect(shouldDetachBashWaitOnUserMessage(config, plain)).toBe(false);
    expect(shouldDetachBashWaitOnUserMessage(config, forced)).toBe(true);
    expect(forced).toBe("please &detach continue");
  });

  test("strips every token while preserving the rest of a message", () => {
    const output = {
      parts: [{ type: "text", text: "before &detach middle &detach after" }],
    };

    expect(stripUserMessageDetachKeyword(output)).toBe("before middle after");
    expect(output.parts[0].text).toBe("before middle after");
  });

  test("recognizes standalone tokens at message boundaries", () => {
    const config = { bash: { detach_on_user_message: false } };
    const first = { parts: [{ type: "text", text: "&detach, continue" }] };
    const last = { parts: [{ type: "text", text: "continue &detach" }] };

    expect(shouldDetachBashWaitOnUserMessage(config, first.parts[0].text)).toBe(true);
    expect(shouldDetachBashWaitOnUserMessage(config, last.parts[0].text)).toBe(true);
    expect(stripUserMessageDetachKeyword(first)).toBe(", continue");
    expect(stripUserMessageDetachKeyword(last)).toBe("continue ");
  });

  test("does not detach or strip when the keyword is part of an identifier", () => {
    const config = { bash: { detach_on_user_message: false } };
    const messages = [
      "Please document &detachment behavior",
      "keep before&detach unchanged",
      "keep &detach_mode unchanged",
      "keep &detaché unchanged",
    ];

    for (const message of messages) {
      const output = { parts: [{ type: "text", text: message }] };
      expect(shouldDetachBashWaitOnUserMessage(config, message)).toBe(false);
      expect(stripUserMessageDetachKeyword(output)).toBe(message);
      expect(output.parts[0].text).toBe(message);
    }
  });

  test("substitutes an honest message when the token is the only user text", () => {
    const output = { parts: [{ type: "text", text: "  &detach  " }] };

    expect(stripUserMessageDetachKeyword(output)).toBe("(requested background detach)");
    expect(output.parts[0].text).toBe("(requested background detach)");
  });

  test("extracts only non-synthetic, non-ignored text parts", () => {
    const message = extractUserMessageText({
      parts: [
        { type: "text", text: "plain" },
        { type: "text", text: " synthetic", synthetic: true },
        { type: "text", text: " ignored", ignored: true },
      ],
    });

    expect(message).toBe("plain");
  });

  test("user-message detach sends bash_wait_detach on the active bridge", async () => {
    const calls: Array<[string, Record<string, unknown>, Record<string, unknown>]> = [];
    const bridge = {
      send: async (
        command: string,
        params: Record<string, unknown>,
        options: Record<string, unknown>,
      ) => {
        calls.push([command, params, options]);
        return { success: true, detached: true };
      },
    };
    const pool = {
      getActiveBridgeForRoot: (root: string) => {
        expect(root).toBe(projectRoot);
        return bridge;
      },
      activeBridges: () => [bridge],
    };

    await signalBashWaitDetachForProject(
      pool as Parameters<typeof signalBashWaitDetachForProject>[0],
      projectRoot,
      "session-1",
    );

    expect(calls).toHaveLength(1);
    expect(calls[0][0]).toBe("bash_wait_detach");
    expect(calls[0][1]).toEqual({ session_id: "session-1" });
    expect(calls[0][2]).toMatchObject({
      keepBridgeOnTimeout: true,
      transportTimeoutMs: 30_000,
    });
  });

  test("user-message detach is skipped without a session or any live bridge", async () => {
    const send = mock(async () => ({ success: true }));
    const pool = {
      getActiveBridgeForRoot: () => null,
      activeBridges: () => [],
    };

    await signalBashWaitDetachForProject(
      pool as Parameters<typeof signalBashWaitDetachForProject>[0],
      projectRoot,
      undefined,
    );
    await signalBashWaitDetachForProject(
      pool as Parameters<typeof signalBashWaitDetachForProject>[0],
      projectRoot,
      "session-2",
    );

    expect(send).not.toHaveBeenCalled();
  });

  test.serial(
    "no bridge yet is expected before the first tool call and is not a warning",
    async () => {
      const warn = spyOn(logger, "warn");
      const debug = spyOn(logger, "debug");
      try {
        await signalBashWaitDetachForProject(
          { getActiveBridgeForRoot: () => null, activeBridges: () => [] } as unknown as Parameters<
            typeof signalBashWaitDetachForProject
          >[0],
          projectRoot,
          "session-first-run",
        );
        expect(warn).not.toHaveBeenCalled();
        expect(debug).toHaveBeenCalledTimes(1);
        expect(String(debug.mock.calls[0]?.[0])).toContain("nothing to detach");
      } finally {
        warn.mockRestore();
        debug.mockRestore();
      }
    },
  );

  test.serial("no active wait result uses debug severity while pending state is info", () => {
    expect(waitDetachSummaryLevel(false)).toBe("debug");
    expect(waitDetachSummaryLevel(true)).toBe("info");
  });

  test("root-key miss fans out to every live bridge instead of dropping", async () => {
    const sends: string[] = [];
    const bridgeFor = (label: string) => ({
      send: mock(async (command: string, params: Record<string, unknown>) => {
        sends.push(`${label}:${command}:${String(params.session_id)}`);
        return { success: true };
      }),
    });
    const bridgeA = bridgeFor("a");
    const bridgeB = bridgeFor("b");
    const pool = {
      // Exact root resolution misses (the silent-drop bug this guards):
      getActiveBridgeForRoot: () => null,
      activeBridges: () => [bridgeA, bridgeB],
    };

    await signalBashWaitDetachForProject(
      pool as unknown as Parameters<typeof signalBashWaitDetachForProject>[0],
      "/repo-that-does-not-match",
      "session-3",
    );

    expect(sends.sort()).toEqual(["a:bash_wait_detach:session-3", "b:bash_wait_detach:session-3"]);
  });
});

// A new message interrupts both blocking waits (a wait:true bash and a sync
// bash_watch) while detach_on_user_message is on, including a machine-generated
// one such as AFT's own background-completion wake: the agent should act on it
// instead of sitting out the rest of the wait. With the setting off, both waits
// are protected and only &detach interrupts them.
describe("chat.message interrupts waits by one decision", () => {
  const sessionID = "session-chat";

  afterEach(() => __resetSyncWatchAbortForTests());

  function recordingPool() {
    const sends: string[] = [];
    const bridge = {
      send: async (command: string, params: Record<string, unknown>) => {
        sends.push(`${command}:${String(params.session_id)}`);
        return { success: true, detached: true };
      },
    };
    const pool = {
      getActiveBridgeForRoot: () => bridge,
      activeBridges: () => [bridge],
    } as unknown as Parameters<typeof interruptBashWaitsForChatMessage>[0];
    return { pool, sends };
  }

  async function deliver(config: Record<string, unknown>, parts: unknown[]) {
    __resetSyncWatchAbortForTests();
    const { pool, sends } = recordingPool();
    const output = { parts };
    interruptBashWaitsForChatMessage(pool, config, projectRoot, sessionID, output);
    // Let the fire-and-forget detach reach the bridge.
    await new Promise((resolve) => setTimeout(resolve, 0));
    return { detached: sends.includes(`bash_wait_detach:${sessionID}`), output };
  }

  test("an all-synthetic wake message detaches and aborts with the default config", async () => {
    // Same part shape the background-completion wake sends through promptAsync.
    const { detached } = await deliver({}, [
      {
        type: "text",
        text: "<system-reminder>bash task finished</system-reminder>",
        synthetic: true,
      },
    ]);

    expect(detached).toBe(true);
    expect(isSyncWatchAborted(sessionID)).toBe(true);
  });

  test("with detach_on_user_message off a wake message interrupts nothing", async () => {
    const config = { bash: { detach_on_user_message: false } };
    const { detached } = await deliver(config, [
      { type: "text", text: "bash task finished", synthetic: true },
    ]);

    expect(detached).toBe(false);
    expect(isSyncWatchAborted(sessionID)).toBe(false);
  });

  test("a typed message detaches and aborts with the default config", async () => {
    const { detached } = await deliver({}, [{ type: "text", text: "please continue" }]);

    expect(detached).toBe(true);
    expect(isSyncWatchAborted(sessionID)).toBe(true);
  });

  test("with detach_on_user_message off a typed message interrupts nothing", async () => {
    const config = { bash: { detach_on_user_message: false } };
    const { detached } = await deliver(config, [{ type: "text", text: "please continue" }]);

    expect(detached).toBe(false);
    expect(isSyncWatchAborted(sessionID)).toBe(false);
  });

  test("with detach_on_user_message off &detach still detaches and aborts", async () => {
    const config = { bash: { detach_on_user_message: false } };
    const { detached, output } = await deliver(config, [{ type: "text", text: "&detach" }]);

    expect(detached).toBe(true);
    expect(isSyncWatchAborted(sessionID)).toBe(true);
    // The bare token is replaced after the decision, not before it.
    expect(output.parts).toEqual([{ type: "text", text: "(requested background detach)" }]);
  });

  test("a synthetic &detach does not count as the operator's token", async () => {
    const config = { bash: { detach_on_user_message: false } };
    const { detached } = await deliver(config, [
      { type: "text", text: "&detach", synthetic: true },
    ]);

    expect(detached).toBe(false);
    expect(isSyncWatchAborted(sessionID)).toBe(false);
  });
});

test("detach preserves indentation and TSV away from the token", () => {
  const text = "&detach please\n    print('x')\na\t\tb\n| a  | b |";
  const output = { parts: [{ type: "text", text }] };
  expect(stripUserMessageDetachKeyword(output)).toBe(" please\n    print('x')\na\t\tb\n| a  | b |");
});
