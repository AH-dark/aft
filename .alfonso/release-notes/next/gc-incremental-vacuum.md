Storage GC no longer runs full SQLite VACUUM on blob caches. New/rebuilt legacy
blob stores use incremental auto-vacuum; existing NONE-mode stores remain usable
without an upgrade-time rewrite. Already-incremental per-checkout stores now drain
SQLite's multi-step vacuum correctly and retry old free lists even when no new
rows are deleted.

Reclaim starts only when free pages occupy at least 20% of the database and
256 MiB. Each store/pass reclaims at most 4096 free pages, in 16-page steps with a
one-second deadline and cancellation checks between pages. Sweep reports include
pages examined, free and reclaimed, bytes reclaimed, steps, and deferred work.
Normal WAL checkpoints apply file truncation; GC does not force an unbounded
TRUNCATE checkpoint. Blob deletion and pin/reference protections are unchanged.
