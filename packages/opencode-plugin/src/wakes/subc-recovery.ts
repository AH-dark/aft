import type { SessionDomain } from "@opencode/plugin/effect/session";
import { DateTime, Effect } from "effect";
import { warn } from "../logger.js";

const RECENT_SESSION_LIMIT = 20;
const RECENT_SESSION_WINDOW_MS = 24 * 60 * 60 * 1000;

/**
 * A Location owns no subc subscriptions after release. Recover a small recent
 * host-owned set, using each session's own route, without delaying startup.
 * Older sessions recover when they next call a tool or send a prompt.
 */
export function recoverV2SubcSessions(
  context: unknown,
  directory: string,
  restoreSession: (sessionID: string) => Promise<void>,
) {
  const host = context as { session?: Pick<SessionDomain, "list"> };
  return Effect.gen(function* () {
    if (typeof host.session?.list !== "function") return;
    // Yield to foreground Location setup, then recover sequentially with a
    // scheduling gap between sessions rather than opening a burst of routes.
    yield* Effect.sleep("10 millis");
    const sessions = yield* host.session.list({
      directory: directory as NonNullable<Parameters<SessionDomain["list"]>[0]>["directory"],
      order: "desc",
      limit: RECENT_SESSION_LIMIT,
    });
    const cutoff = Date.now() - RECENT_SESSION_WINDOW_MS;
    for (const session of sessions.data.slice(0, RECENT_SESSION_LIMIT)) {
      // Verify the returned directory too, so this Location cannot wake a
      // session from another workspace if the host's list filter is ignored.
      if (
        session.location.directory !== directory ||
        DateTime.toEpochMillis(session.time.updated) < cutoff
      )
        continue;
      yield* Effect.promise(() => restoreSession(session.id)).pipe(
        Effect.catch((error) =>
          Effect.sync(() => warn(`[bash_completion] session recovery failed: ${String(error)}`)),
        ),
      );
      yield* Effect.sleep("10 millis");
    }
  }).pipe(
    Effect.timeout("30 seconds"),
    Effect.catch((error) =>
      Effect.sync(() =>
        warn(`[bash_completion] recent session discovery failed: ${String(error)}`),
      ),
    ),
  );
}
