import { expect, test } from "bun:test";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { relative, resolve } from "node:path";
import type { BridgePool } from "@cortexkit/aft-bridge";
import { Error as ToolError } from "@opencode/plugin/promise/tool";
import { Cause, Effect } from "effect";
import { projectV2Tool } from "../tools/definitions/v2.js";
import { hoistedV2ToolConsumers } from "../tools/hoisted/v2.js";
import { readingTools } from "../tools/reading.js";
import type { PluginContext } from "../types.js";
import { engineOutsideAsk } from "./helpers/engine-outside-ask.js";
import { permissionHost } from "./v2-permission-host.js";

const execution = {
  sessionID: "ses_concurrency",
  messageID: "msg_concurrency",
  id: "call_concurrency",
  agent: "build",
  progress: () => Effect.void,
};

function heldHost(
  evaluate?: NonNullable<Parameters<typeof permissionHost>[0]>["evaluate"],
  releaseReads = false,
) {
  const fixture = permissionHost({
    agentRules: [{ action: "*", resource: "*", effect: "allow" }],
    evaluate,
  });
  type Input = Parameters<typeof fixture.host.permission.assert>[0];
  const assert = fixture.host.permission.assert;
  const started: Input[] = [];
  const waiting = new Map<number, { allow: () => void; reject: (error: Error) => void }>();
  const observers: Array<{ count: number; resolve: () => void }> = [];
  const idleObservers: Array<() => void> = [];
  let active = 0;
  let peak = 0;
  let completed = 0;
  let interrupted = 0;
  let autoRelease = false;
  fixture.host.permission.assert = (input) =>
    Effect.suspend(() => {
      const index = started.length;
      started.push(input);
      active++;
      peak = Math.max(peak, active);
      const held = Effect.promise(
        () =>
          new Promise<void>((allow, reject) => {
            waiting.set(index, { allow, reject });
            if (autoRelease || (releaseReads && input.action === "read")) allow();
            for (const observer of observers)
              if (started.length >= observer.count) observer.resolve();
          }),
      );
      return held.pipe(
        Effect.flatMap(() => assert(input)),
        Effect.tap(() =>
          Effect.sync(() => {
            completed++;
          }),
        ),
        Effect.onInterrupt(() =>
          Effect.sync(() => {
            interrupted++;
          }),
        ),
        Effect.ensuring(
          Effect.sync(() => {
            active--;
            waiting.delete(index);
            if (active === 0) for (const resolve of idleObservers) resolve();
          }),
        ),
      );
    });
  return {
    ...fixture,
    started,
    get active() {
      return active;
    },
    get peak() {
      return peak;
    },
    get completed() {
      return completed;
    },
    get interrupted() {
      return interrupted;
    },
    waitForStarts(count: number) {
      if (started.length >= count) return Promise.resolve();
      return new Promise<void>((resolve) => observers.push({ count, resolve }));
    },
    waitForIdle() {
      return active === 0
        ? Promise.resolve()
        : new Promise<void>((resolve) => idleObservers.push(resolve));
    },
    releaseAll() {
      autoRelease = true;
      for (const control of [...waiting.values()].reverse()) control.allow();
    },
    reject(index: number, error: Error) {
      const control = waiting.get(index);
      if (!control) throw new Error(`assertion ${index} is not waiting`);
      control.reject(error);
    },
  };
}

async function fixture(
  name: "aft_outline" | "aft_zoom",
  host: ReturnType<typeof heldHost>,
  count = 17,
  external = false,
) {
  const root = await mkdtemp(resolve(process.cwd(), ".aft-read-concurrency-"));
  const directory = external ? resolve(root, "project") : process.cwd();
  if (external) await mkdir(directory);
  const files = Array.from({ length: count }, (_, index) => resolve(root, `file-${index}.ts`));
  const dispatched: Record<string, unknown>[] = [];
  const bridge = {
    toolCall: async (_session: unknown, _name: string, args: Record<string, unknown>) => {
      if (external) {
        const ask = engineOutsideAsk(files[0]!, args);
        if (ask) return ask;
      }
      dispatched.push(args);
      const excluded = args._exclude_read_paths as string[] | undefined;
      return {
        success: true,
        text: "allowed content",
        content_files: files.filter((file) => !excluded?.includes(file)),
      };
    },
  };
  const tools = readingTools({
    pool: { getBridge: () => bridge } as unknown as BridgePool,
    client: {},
    config: {},
    storageDir: "/isolated/storage",
  } as PluginContext);
  const projected = projectV2Tool(
    name,
    tools[name]!,
    { directory },
    hoistedV2ToolConsumers(host.host),
  );
  const args =
    name === "aft_outline"
      ? { target: root }
      : { targets: files.map((path) => ({ path, symbol: "allowed" })) };
  return { root, files, dispatched, effect: projected.execute(args, execution) };
}

async function firstWave(host: ReturnType<typeof heldHost>, count = 8) {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([
      host.waitForStarts(count),
      new Promise<void>((resolve) => {
        timer = setTimeout(resolve, 2000);
      }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

for (const name of ["aft_outline", "aft_zoom"] as const) {
  test(`${name} read permissions hold at eight concurrent assertions and complete every file`, async () => {
    const host = heldHost();
    const f = await fixture(name, host);
    const pending = Effect.runPromise(f.effect);
    try {
      await firstWave(host);
      host.releaseAll();
      expect(await pending).toMatchObject({ content: "allowed content" });
      expect(host.completed).toBe(17);
      expect(host.started).toHaveLength(17);
      expect(host.active).toBe(0);
      expect(host.peak).toBeLessThanOrEqual(8);
      expect(host.peak).toBe(8);
      expect(host.started.map((input) => input.resources[0]).toSorted()).toEqual(
        f.files.map((file) => relative(process.cwd(), file)).toSorted(),
      );
      for (const input of host.started) {
        expect(input).toMatchObject({
          sessionID: execution.sessionID,
          agent: execution.agent,
          action: "read",
          save: ["*"],
          source: { type: "tool", messageID: execution.messageID, id: execution.id },
        });
        expect(input).not.toHaveProperty("metadata");
      }
    } finally {
      host.releaseAll();
      await pending.catch(() => {});
      await rm(f.root, { recursive: true, force: true });
    }
  }, 10000);

  test(`${name} keeps denial and allowed-file output order when assertions finish out of order`, async () => {
    const host = heldHost((input, effect) =>
      input.resources.some((path) => /\/file-[02]\.ts$/.test(path)) ? "deny" : effect,
    );
    const f = await fixture(name, host);
    const pending = Effect.runPromise(f.effect);
    try {
      await firstWave(host);
      host.releaseAll();
      const result = await pending;
      expect(host.started).toHaveLength(17);
      if (name === "aft_outline") {
        expect(f.dispatched[1]?._exclude_read_paths).toEqual([f.files[0], f.files[2]]);
        expect(result.content).toBe("allowed content\n2 files left out by read rules.");
      } else {
        const denial =
          "permission_denied: read is denied by this session's permission rules (not a user choice)";
        expect(result.content).toBe(
          `allowed content\n\n${f.files[0]}: ${denial}\n${f.files[2]}: ${denial}`,
        );
        expect(f.dispatched[0]?.batch).toEqual(
          f.files
            .filter((_, index) => index !== 0 && index !== 2)
            .map((path) => ({ target: path, symbols: "allowed" })),
        );
      }
    } finally {
      host.releaseAll();
      await pending.catch(() => {});
      await rm(f.root, { recursive: true, force: true });
    }
  }, 10000);

  for (const rejection of ["step", "feedback"] as const) {
    test(`${name} stops launching permission checks after ${rejection} rejection and cancels siblings`, async () => {
      const host = heldHost();
      const f = await fixture(name, host);
      const rejected =
        rejection === "step"
          ? new Error("stop this step")
          : new ToolError({
              message: "Use a narrower set of files",
              error: { _tag: "Permission.CorrectedError" },
            });
      const pending = Effect.runPromiseExit(f.effect);
      try {
        await firstWave(host);
        expect(host.started).toHaveLength(8);
        host.reject(0, rejected);
        const result = await pending;
        expect(result._tag).toBe("Failure");
        if (result._tag === "Failure") {
          expect(Cause.squash(result.cause)).toBe(rejected);
          expect(Cause.findDefect(result.cause)._tag).toBe(
            rejection === "step" ? "Success" : "Failure",
          );
        }
        expect(host.started).toHaveLength(8);
        expect(host.active).toBe(0);
        expect(host.interrupted).toBe(7);
        expect(f.dispatched).toHaveLength(name === "aft_outline" ? 1 : 0);
      } finally {
        host.releaseAll();
        await pending;
        await rm(f.root, { recursive: true, force: true });
      }
    }, 10000);
  }

  test(`${name} caller cancellation interrupts the whole permission wave`, async () => {
    const host = heldHost();
    const f = await fixture(name, host);
    const controller = new AbortController();
    const pending = Effect.runPromiseExit(f.effect, { signal: controller.signal });
    try {
      await firstWave(host);
      expect(host.started).toHaveLength(8);
      controller.abort();
      expect((await pending)._tag).toBe("Failure");
      await host.waitForIdle();
      expect(host.started).toHaveLength(8);
      expect(host.interrupted).toBe(8);
      expect(f.dispatched).toHaveLength(name === "aft_outline" ? 1 : 0);
    } finally {
      controller.abort();
      host.releaseAll();
      await pending;
      await rm(f.root, { recursive: true, force: true });
    }
  }, 10000);
}

test("zoom rejection during external authorization never launches subsequent read prompts", async () => {
  const host = heldHost(undefined, true);
  const f = await fixture("aft_zoom", host, 17, true);
  const rejected = new Error("stop outside access");
  const pending = Effect.runPromiseExit(f.effect);
  try {
    await firstWave(host, 18);
    expect(host.started).toHaveLength(18);
    expect(host.started.slice(0, 17).every((input) => input.action === "read")).toBe(true);
    expect(host.started[17]?.action).toBe("external_directory");
    host.reject(17, rejected);
    const result = await pending;
    expect(result._tag).toBe("Failure");
    if (result._tag === "Failure") expect(Cause.squash(result.cause)).toBe(rejected);
    expect(host.started).toHaveLength(18);
    expect(host.active).toBe(0);
    expect(host.interrupted).toBe(0);
    expect(f.dispatched).toEqual([]);
  } finally {
    host.releaseAll();
    await pending;
    await rm(f.root, { recursive: true, force: true });
  }
}, 10000);
