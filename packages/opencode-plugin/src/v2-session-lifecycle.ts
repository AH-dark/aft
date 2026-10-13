import { Effect, Stream } from "effect";
import { warn } from "./logger.js";
import { resolveIsSubagent } from "./shared/subagent-detect.js";
import { callBashBridge } from "./tools/_shared.js";
import type { PluginContext } from "./types.js";

interface SessionWatches {
  stopped: boolean;
  settling?: { promise: Promise<void>; resolve: () => void };
  wakes: Set<string>;
  watches: Map<string, { taskId: string; cancel: () => Promise<unknown> }>;
}

/** Hold a racing wake until the host classifies an interrupted tool's execution. */
export function noteV2ToolAbort(client: unknown, sessionID: string): void {
  const state = stateFor(client, sessionID);
  if (!state || state.settling) return;
  let resolve = () => {};
  const promise = new Promise<void>((done) => {
    resolve = done;
  });
  state.settling = { promise, resolve };
}

export async function awaitV2ExecutionSettlement(
  client: unknown,
  sessionID: string,
): Promise<void> {
  await stateFor(client, sessionID)?.settling?.promise;
}
const locations = new WeakMap<object, Map<string, SessionWatches>>();

function stateFor(client: unknown, sessionID: string): SessionWatches | undefined {
  if (!client || typeof client !== "object") return;
  const sessions = locations.get(client);
  if (!sessions) return;
  let state = sessions.get(sessionID);
  if (!state) {
    state = { stopped: false, wakes: new Set(), watches: new Map() };
    sessions.set(sessionID, state);
  }
  return state;
}

export function v2SubagentStopped(client: unknown, sessionID: string): boolean {
  return stateFor(client, sessionID)?.stopped === true;
}

export function allowV2WatchWake(client: unknown, sessionID: string, taskId: string): void {
  const state = stateFor(client, sessionID);
  if (state && !state.stopped) state.wakes.add(taskId);
}

export function v2WatchWakeAllowed(client: unknown, sessionID: string, taskId: string): boolean {
  const state = stateFor(client, sessionID);
  return state !== undefined && !state.stopped && state.wakes.has(taskId);
}

export function forgetV2Watch(client: unknown, sessionID: string, taskId: string): void {
  const state = stateFor(client, sessionID);
  if (!state) return;
  state.wakes.delete(taskId);
  for (const [watchId, watch] of state.watches) {
    if (watch.taskId === taskId) state.watches.delete(watchId);
  }
}

export async function trackV2Watch(
  ctx: PluginContext,
  runtime: { sessionID: string; directory: string },
  taskId: string,
  watchId: unknown,
): Promise<void> {
  const state = stateFor(ctx.client, runtime.sessionID);
  if (!state || typeof watchId !== "string") return;
  const cancel = () =>
    callBashBridge(
      ctx,
      runtime,
      "bash_unnotify",
      { task_id: taskId, watch_id: watchId },
      { abortSignal: undefined },
    );
  // Registration may finish after the execution's stop event was pulled.
  if (state.stopped) {
    await cancel();
    return;
  }
  state.wakes.add(taskId);
  state.watches.set(watchId, { taskId, cancel });
}

/** Own the lazy host subscription before any tool can register a watch. */
export function registerV2SessionLifecycle(context: unknown) {
  const host = context as {
    event?: {
      subscribe?: () => Stream.Stream<
        { type: string; data: { sessionID?: string; reason?: string } },
        unknown
      >;
    };
  };
  return Effect.gen(function* () {
    if (!host.event?.subscribe || !context || typeof context !== "object") return;
    const sessions = new Map<string, SessionWatches>();
    locations.set(context, sessions);
    yield* Effect.addFinalizer(() =>
      Effect.sync(() => {
        for (const state of sessions.values()) state.settling?.resolve();
        locations.delete(context);
      }),
    );
    // toPull acquires the subscription; the scoped consumer starts pulling it
    // before registration returns, rather than merely constructing a Stream.
    const pull = yield* Stream.toPull(host.event.subscribe());
    yield* Effect.forkScoped(
      Effect.forever(
        pull.pipe(
          Effect.flatMap((events) =>
            Effect.forEach(
              events,
              (event) =>
                Effect.promise(async () => {
                  const sessionID = event.data.sessionID;
                  if (!sessionID) return;
                  const state = stateFor(context, sessionID);
                  if (!state) return;
                  if (event.type === "session.execution.started") {
                    state.stopped = false;
                    return;
                  }
                  if (
                    event.type === "session.execution.failed" ||
                    event.type === "session.execution.succeeded" ||
                    (event.type === "session.execution.interrupted" &&
                      event.data.reason !== "user" &&
                      event.data.reason !== "inactivity")
                  ) {
                    state.settling?.resolve();
                    state.settling = undefined;
                    return;
                  }
                  if (
                    event.type !== "session.execution.interrupted" ||
                    (event.data.reason !== "user" && event.data.reason !== "inactivity")
                  )
                    return;
                  if (!(await resolveIsSubagent(context, sessionID))) {
                    state.settling?.resolve();
                    state.settling = undefined;
                    return;
                  }
                  state.stopped = true;
                  state.wakes.clear();
                  state.settling?.resolve();
                  state.settling = undefined;
                  const watches = [...state.watches.values()];
                  state.watches.clear();
                  await Promise.all(
                    watches.map((watch) =>
                      watch.cancel().catch((error) => {
                        warn(
                          `[bash_watch] failed to cancel stopped session ${sessionID}'s watch on ${watch.taskId}: ${String(error)}`,
                        );
                      }),
                    ),
                  );
                }),
              { discard: true },
            ),
          ),
        ),
      ),
      { startImmediately: true },
    );
  });
}
