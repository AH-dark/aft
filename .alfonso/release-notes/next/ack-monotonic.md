Prevent acknowledged background completions from becoming undelivered when a stale task snapshot is persisted by another registry. Restart recovery also honors durable acknowledgements when task metadata on disk predates the acknowledgement, avoiding duplicate completion wakes.

After restart, the agent can still receive a promoted command's completion, even if the command finished while the daemon was stopped. An older saved foreground state cannot mark that completion as already delivered.
