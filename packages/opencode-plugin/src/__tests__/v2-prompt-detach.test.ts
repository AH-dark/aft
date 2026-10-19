/// <reference path="../bun-test.d.ts" />

import { afterEach, describe, expect, spyOn, test } from "bun:test";
import { Effect } from "effect";

import type { AftConfig } from "../config.js";
import * as logger from "../logger.js";
import { __resetSyncWatchAbortForTests, isSyncWatchAborted } from "../sync-watch-abort.js";
import {
  createV2PromptDetachHook,
  registerV2PromptDetachHook,
  type V2PromptDetachRuntime,
  type V2PromptHookEvent,
} from "../v2-prompt-detach.js";

const PROJECT_ROOT = "/work/project";

/** A bridge pool whose one bridge records every bash_wait_detach it receives. */
function recordingRuntime(config: AftConfig = {}) {
  const detached: string[] = [];
  const bridge = {
    send: async (command: string, params: Record<string, unknown>) => {
      if (command === "bash_wait_detach") detached.push(String(params.session_id));
      return { success: true, detached: true };
    },
  };
  const runtime = {
    pool: {
      getActiveBridgeForRoot: () => bridge,
      activeBridges: () => [bridge],
    } as unknown as V2PromptDetachRuntime["pool"],
    projectRoot: PROJECT_ROOT,
    config,
    getConfig() {
      return runtime.config;
    },
  };
  return { runtime, detached };
}

function promptEvent(
  text: string,
  extra: Partial<V2PromptHookEvent> & { prompt?: Partial<V2PromptHookEvent["prompt"]> } = {},
): V2PromptHookEvent {
  return {
    sessionID: "session-v2",
    delivery: "steer",
    ...extra,
    prompt: { text, ...extra.prompt },
  };
}

/** The host awaits this hook before durable inbox admission, for queue and steer alike. */
async function runHook(runtime: V2PromptDetachRuntime, event: V2PromptHookEvent): Promise<void> {
  await Effect.runPromise(createV2PromptDetachHook(runtime)(event));
  await new Promise((resolve) => setTimeout(resolve, 0));
}

const AFT_WAKE_METADATA = { source: "aft", kind: "bash_completion", task_ids: ["bash-1"] };
const OPT_OUT: AftConfig = { bash: { detach_on_user_message: false } };

afterEach(() => {
  __resetSyncWatchAbortForTests();
});

describe("OpenCode 2 prompt hook detaches waits", () => {
  test("a user message detaches a wait:true bash and aborts a sync bash_watch", async () => {
    const { runtime, detached } = recordingRuntime();
    const event = promptEvent("what is taking so long?");

    await runHook(runtime, event);

    expect(detached).toEqual(["session-v2"]);
    expect(isSyncWatchAborted("session-v2")).toBe(true);
    expect(event.prompt.text).toBe("what is taking so long?");
  });

  test("queue delivery leaves a wait:true bash and a sync bash_watch running", async () => {
    const { runtime, detached } = recordingRuntime();
    const event = promptEvent("tell me when it finishes", { delivery: "queue" });

    await runHook(runtime, event);

    expect(detached).toEqual([]);
    expect(isSyncWatchAborted("session-v2")).toBe(false);
    expect(event.prompt.text).toBe("tell me when it finishes");
  });

  test("steer delivery detaches a wait:true bash and aborts a sync bash_watch", async () => {
    const { runtime, detached } = recordingRuntime();

    await runHook(runtime, promptEvent("change direction now", { delivery: "steer" }));

    expect(detached).toEqual(["session-v2"]);
    expect(isSyncWatchAborted("session-v2")).toBe(true);
  });

  test.each([
    {},
    OPT_OUT,
  ])("&detach in a queued message forces a detach with config %j", async (config) => {
    const { runtime, detached } = recordingRuntime(config);
    const event = promptEvent("please &detach now", { delivery: "queue" });

    await runHook(runtime, event);

    expect(detached).toEqual(["session-v2"]);
    expect(isSyncWatchAborted("session-v2")).toBe(true);
    expect(event.prompt.text).toBe("please now");
  });

  test("with the setting false a plain message leaves the waits alone", async () => {
    const { runtime, detached } = recordingRuntime(OPT_OUT);

    await runHook(runtime, promptEvent("keep going"));

    expect(detached).toEqual([]);
    expect(isSyncWatchAborted("session-v2")).toBe(false);
  });

  test("&detach forces a detach with the setting false and is stripped from the text", async () => {
    const { runtime, detached } = recordingRuntime(OPT_OUT);
    const event = promptEvent("please &detach now");

    await runHook(runtime, event);

    expect(detached).toEqual(["session-v2"]);
    expect(isSyncWatchAborted("session-v2")).toBe(true);
    expect(event.prompt.text).toBe("please now");
  });

  test("a token-only message becomes the fixed notice", async () => {
    const { runtime } = recordingRuntime(OPT_OUT);
    const event = promptEvent("  &detach  ");

    await runHook(runtime, event);

    expect(event.prompt.text).toBe("(requested background detach)");
  });

  test("the live config is read on every prompt", async () => {
    const { runtime, detached } = recordingRuntime(OPT_OUT);

    await runHook(runtime, promptEvent("first"));
    expect(detached).toEqual([]);

    runtime.config = { bash: { detach_on_user_message: true } };
    await runHook(runtime, promptEvent("second"));
    expect(detached).toEqual(["session-v2"]);
  });
});

describe("OpenCode 2 prompt hook treats AFT's own wakes as synthetic", () => {
  test("an AFT-marked steer prompt detaches while the setting is on", async () => {
    const { runtime, detached } = recordingRuntime();
    const event = promptEvent("[BACKGROUND BASH COMPLETED] bash-1", {
      delivery: "steer",
      metadata: AFT_WAKE_METADATA,
    });

    await runHook(runtime, event);

    expect(detached).toEqual(["session-v2"]);
    expect(isSyncWatchAborted("session-v2")).toBe(true);
  });

  test("&detach inside an AFT wake neither forces a detach nor is stripped", async () => {
    const { runtime, detached } = recordingRuntime(OPT_OUT);
    const text = "[BACKGROUND BASH COMPLETED] output mentioned &detach here";
    const event = promptEvent(text, { metadata: AFT_WAKE_METADATA });

    await runHook(runtime, event);

    expect(detached).toEqual([]);
    expect(isSyncWatchAborted("session-v2")).toBe(false);
    expect(event.prompt.text).toBe(text);
  });

  test("&detach inside an AFT wake is kept even when the wake detaches", async () => {
    const { runtime, detached } = recordingRuntime();
    const text = "[BACKGROUND BASH COMPLETED] output mentioned &detach here";
    const event = promptEvent(text, { metadata: AFT_WAKE_METADATA });

    await runHook(runtime, event);

    expect(detached).toEqual(["session-v2"]);
    expect(event.prompt.text).toBe(text);
  });

  test("a prompt from another source is not treated as AFT's", async () => {
    const { runtime, detached } = recordingRuntime(OPT_OUT);
    const event = promptEvent("other plugin says &detach", { metadata: { source: "other" } });

    await runHook(runtime, event);

    expect(detached).toEqual(["session-v2"]);
    expect(event.prompt.text).toBe("other plugin says ");
  });
});

describe("OpenCode 2 prompt hook keeps mention offsets on their text", () => {
  test("mentions before and after the stripped token still select their text", async () => {
    const { runtime } = recordingRuntime();
    const original = "see @src/a.ts  &detach  and   @build with @skill-x";
    const at = (needle: string) => {
      const start = original.indexOf(needle);
      return { start, end: start + needle.length, text: needle };
    };
    const event = promptEvent(original, {
      prompt: {
        files: [{ mention: at("@src/a.ts") }],
        agents: [{ mention: at("@build") }],
        skills: [{ mention: at("@skill-x") }, {}],
      },
    });

    await runHook(runtime, event);

    const text = event.prompt.text;
    expect(text).toBe("see @src/a.ts and   @build with @skill-x");
    const mentions = [
      ...(event.prompt.files ?? []),
      ...(event.prompt.agents ?? []),
      ...(event.prompt.skills ?? []),
    ]
      .map((item) => item.mention)
      .filter((mention) => mention !== undefined);
    expect(mentions).toHaveLength(3);
    for (const mention of mentions) {
      expect(text.slice(mention.start, mention.end)).toBe(mention.text);
    }
  });

  test("mentions are untouched when nothing is stripped", async () => {
    const { runtime } = recordingRuntime();
    const mention = { start: 4, end: 10, text: "@build" };
    const event = promptEvent("ask @build   now", { prompt: { agents: [{ mention }] } });

    await runHook(runtime, event);

    expect(event.prompt.text).toBe("ask @build   now");
    expect(event.prompt.agents?.[0]?.mention).toBe(mention);
  });
});

describe("OpenCode 2 prompt hook failures", () => {
  test("an error passes the prompt through unchanged and is logged once per reason", async () => {
    const warn = spyOn(logger, "warn");
    try {
      const runtime: V2PromptDetachRuntime = {
        pool: { getActiveBridgeForRoot: () => null, activeBridges: () => [] } as never,
        projectRoot: PROJECT_ROOT,
        getConfig: () => {
          throw new Error("config unavailable");
        },
      };
      const hook = createV2PromptDetachHook(runtime);
      const mention = { start: 15, end: 21, text: "@build" };
      const event = promptEvent("please &detach @build", { prompt: { agents: [{ mention }] } });

      const first = await Effect.runPromiseExit(hook(event));
      const second = await Effect.runPromiseExit(hook(promptEvent("again")));

      expect(first._tag).toBe("Success");
      expect(second._tag).toBe("Success");
      expect(event.prompt.text).toBe("please &detach @build");
      expect(event.prompt.agents?.[0]?.mention).toBe(mention);
      const failures = warn.mock.calls
        .map((call) => String(call[0]))
        .filter((line) => line.includes("prompt hook failed"));
      expect(failures).toHaveLength(1);
      expect(failures[0]).toContain("config unavailable");
      expect(failures[0]).toContain("passes through unchanged");
    } finally {
      warn.mockRestore();
    }
  });

  test("a host without session.hook registers nothing and does not fail", async () => {
    const warn = spyOn(logger, "warn");
    try {
      const { runtime } = recordingRuntime();
      const registered = await Effect.runPromise(
        registerV2PromptDetachHook({}, runtime) as Effect.Effect<boolean>,
      );
      expect(registered).toBe(false);
    } finally {
      warn.mockRestore();
    }
  });
});

test("localized detach cleanup preserves code and mention offsets", async () => {
  const { runtime } = recordingRuntime();
  const text = "please  &detach  now\n    print('x')\na\t\tb\n@file";
  const start = text.indexOf("@file");
  const event = promptEvent(text);
  event.prompt.files = [{ mention: { start, end: start + 5, text: "@file" } }];
  await runHook(runtime, event);
  expect(event.prompt.text).toBe("please now\n    print('x')\na\t\tb\n@file");
  const mention = event.prompt.files[0]?.mention;
  expect(mention?.start).toBe(event.prompt.text.indexOf("@file"));
  expect(event.prompt.text.slice(mention?.start, mention?.end)).toBe("@file");
});
