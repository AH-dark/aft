## Worker GitHub failed-run logs

- Fixed sandboxed `gh run view --log-failed` silently dropping available log
  content when step timestamps fall in the same second.
- Worker reads currently receive whole-job display text, not numbered step-log
  files. Like GitHub CLI's whole-job fallback, logs now retain every line of
  failed jobs and use `UNKNOWN STEP` rather than inferring step names from
  timestamps. Successful jobs remain excluded; raw-byte pagination is unchanged.
- Plexus display text has ANSI codes removed and can differ from native GitHub
  CLI's per-step archive output. A genuinely empty job log remains empty.
