# Disk-limit sizing and SQLite overflow reads

## Source of the reads

The `aft-view-disk-limits` worker calls `views::eviction::enforce`, which
measures the family before and after a bounded family sweep. Measurement's
`measure_family_store` calls `FamilyStoreReader::usage`. Its
`COUNT(*), COALESCE(SUM(length(payload)), 0)` aggregate does **not** read BLOB
overflow pages in the regression fixture. File-byte accounting separately walks
filesystem metadata; it does not inspect SQLite payloads.

The overflow reads occur in the sweep:

```text
enforce
  gc::family::sweep_family_bounded
    delete_unmarked
      FamilyStore::rows_oldest
```

The old listing query was:

```sql
SELECT full_key, length(payload), ref_epoch FROM blob_payloads
ORDER BY ref_epoch ASC, full_key ASC LIMIT ?1
```

`blob_payloads` is a `WITHOUT ROWID` table with `payload` before `ref_epoch`.
Reading the trailing epoch for sorting visits overflow pages even when the BLOB
length itself needs only record metadata. The listing happens even when the
store is below its payload budget: `delete_unmarked` obtains the rows before
its deletion loop checks that budget.

The replacement scans `full_key` and `length(payload)` in the table, but gets
each epoch from the existing covering `blob_membership(full_key, ref_epoch)`
index. This preserves the same order, limit, byte counts, and row fields without
a schema change or backfill. It also leaves the aggregate and eviction policy
unchanged. Reversing this strategy (scanning the index and probing the payload
table for each length) is not safe: primary-key comparisons on the large
`WITHOUT ROWID` records themselves triggered overflow reads in an experimental
query. The sequential length scan is intentional.

Actual deletion and vacuuming can still read payload pages; this change removes
them from sizing, not from operations that must modify stored payloads.

## Physical read regression

`blob_store::v2::sizing_tests::sizing_large_blobs_reads_no_overflow_pages`
creates 96 distinct rows of about 64 KiB each, with tied epochs and reverse
keys. It compares totals to materialized payload lengths, and compares both
bounded and unbounded listings to the old SQL's results. It also checks a zero
limit.

The test obtains overflow-page numbers from SQLite's `dbstat`, checkpoints the
fixture, disables mmap, drops SQLite's page cache, and intercepts the main
database file's real VFS `xRead`. Reads overlapping those page numbers are
counted. The aggregate and the new listings together perform zero overflow
reads; materializing a payload afterwards registers overflow reads as a
positive control. On the old query the test fails with `row sizing read
overflow pages`, actual **90**, expected **0**. The aggregate assertion passes,
isolating the problem to row sizing.

## Offline pass timings

Reproduce with:

```sh
cargo test -p agent-file-tools --lib -- disk_limit_pass_large_store_probe --ignored --nocapture
```

Measured on the Linux runner with **4 vCPUs**, using Cargo/rustc 1.99.0 and the
repository's bundled SQLite, in the unoptimized test profile. Each run creates
its own temporary store: **16,384 callgraph rows**, approximately 32 KiB each,
**536,723,495 payload bytes** (about 512 MiB), and initially **555,729,264 family
file bytes**. No live store is accessed. There are no registered view trees in
this fixture: it isolates the large shared family store in a real, under-budget
`enforce` pass, including both measurements and the family sweep. No blobs are
deleted. This is not a simulation of a hundreds-of-GB checkout family.

Each round drops SQLite's page cache, but OS caches remain warm after fixture
creation and `dbstat` inspection. Instrumentation and fixture setup are outside
the timer. These are instrumented offline timings, not cold-disk production
latencies; page-read counts are the more portable result.

| Query | Pass milliseconds (3 rounds) | Main-file xRead calls/pass | Overflow-page reads/pass |
|---|---|---:|---:|
| Old | 121.852, 116.582, 116.089 | 32,883 | 16,378 |
| Fixed | 98.148, 71.273, 78.196 | 16,657 | 0 |
| Fixed, confirmation run with full-pass zero-read assertion | 92.764, 107.346, 82.229 | 16,657 | 0 |

Median pass time fell from **116.582 ms to 78.196 ms** in the first fixed run
(about **33%**), and to **92.764 ms** in the confirmation run (about **20%**).
Payload
totals were identical before and after every pass and in both implementations.
Family file-byte measurements matched between implementations in corresponding
rounds (555,729,264 / 555,774,584 / 555,819,904 bytes); the registry/WAL grows
between rounds as enforcement records usage and advances the sweep epoch.

## Cadence

`views::eviction::ENFORCEMENT_INTERVAL` is **10 minutes per family** and
`ENFORCEMENT_DEADLINE` is **60 seconds per background pass**. Runtime drain
retries after **30 seconds** when the view is not loaded. The process-wide
scheduler serializes passes across families. A normal pass measures twice and
also sizes/list-sorts candidates in the intervening sweep; it is not just the
two aggregate queries.
