/**
 * Shared handling of raw bash output pages. `bash_status` returns task output
 * in byte pages of at most 64 KiB, with `output_has_more` / `stderr_has_more`
 * saying whether captured bytes remain past the page.
 */

/**
 * Watch result text when the watch limit ends while an exited task still has
 * captured output left to scan. The wording matches the engine's own watch
 * (`commands/bash_watch.rs`), so every host reports this case the same way.
 */
export const TERMINAL_SCAN_INCOMPLETE_TEXT =
  "watch limit reached while scanning captured output. The task already exited; the pattern scan is incomplete. Use a longer watch or paged raw output to inspect the remaining bytes.";
