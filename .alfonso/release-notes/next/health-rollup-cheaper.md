# Cheaper steady-state health rollups

Health no longer recounts all retention-eligible shell-task rows in `aft.db` every three seconds. It samples that count at most once per 30 seconds, invalidates the sample when the database handle changes, and retries unsuccessful reads on the next rollup. The eligibility predicate and all existing health fields are unchanged; sweep counters and diagnostics still refresh every rollup.

`bash_task_retention.eligible_but_unpruned_rows_measured_age_ms` reports the age of the count, including the time spent serving a cached rollup. An unavailable count has a null age. This bounds the delay before task deletions or newly eligible rows appear in health without presenting a reused count as current.
