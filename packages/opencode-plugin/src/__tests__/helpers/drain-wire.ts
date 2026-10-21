import { readFileSync } from "node:fs";
import { join } from "node:path";

/**
 * A completion in the shape one `bash_drain_completions` reply entry has.
 *
 * The daemon scopes that reply by its `session_id` request parameter and
 * serializes each entry without `session_id` or `type`: Rust `BgCompletion`
 * skips the session, which only `bash_completed` push frames carry. A test
 * double that echoes the push frame back is more capable than the daemon, and
 * that once hid a consumer that required the field and so never woke its
 * session. Every drain double builds its entries with this function.
 */
export function drainEntry(frame: object): Record<string, unknown> {
  const { session_id: _session, type: _type, ...entry } = frame as Record<string, unknown>;
  return entry;
}

/**
 * A `bash_drain_completions` reply captured from the real daemon (a Linux
 * debug build answering for a finished `sleep 1; echo done` background task).
 * The Rust integration test `drain_completion_entries_match_the_plugin_wire_fixture`
 * checks that the daemon still omits `session_id` the way this capture does.
 */
export function capturedDrainReply(): {
  success: boolean;
  bg_completions: Array<Record<string, unknown>>;
  pending_matches: unknown[];
} {
  return JSON.parse(
    readFileSync(join(import.meta.dir, "../fixtures/bash-drain-completions-reply.json"), "utf8"),
  );
}
