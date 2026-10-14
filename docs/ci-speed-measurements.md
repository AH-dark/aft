# AFT CI wall-time measurements

## Method and scope

Collected on 2026-10-14 for `cortexkit/aft`, starting from main commit
`1a87fec37ba54a0edcd6ac65bffdbd38851cff87`. The baseline is the latest 15
completed `tests.yml` runs on `main` or `train/*` (656 job records). Failures
and cancellations remain in the sample rather than being cherry-picked out.
There are two successful baseline runs; timing improvements are not evidence
that the existing test failures have been fixed.

Reproduce the collection with authenticated GitHub CLI:

```sh
python3 scripts/ci-job-timings.py --recent 15 --output /tmp/aft-ci-baseline.json
python3 scripts/ci-job-timings.py --run RUN_ID --attempt 1 --output /tmp/aft-ci-cold.json
python3 scripts/ci-job-timings.py --run RUN_ID --attempt 2 --output /tmp/aft-ci-warm.json
```

The collector uses `gh run view --json jobs` for job/step timings and the
Actions Jobs API's `created_at` for actual submission-to-runner queue time.
Dependent jobs are submitted after their prerequisites finish: subtracting
workflow creation from their start would wrongly count dependency wait as
runner queue time. Wall time is the attempt's `run_started_at` to its last
job completion; reruns retain the original workflow creation date, so that
date must not be used for warm wall time. Request-to-workflow-start queue
time for reruns is not exposed by this measurement.

The table groups **step durations**, not individual compiler invocations.
Shell gates that both compile and test are explicitly classified as
`compile+test`; they must not be interpreted as pure test execution. Setup
includes other unclassified work (inventory proofs, signing, lint, dependency
installs, artifact transfer). Cleanup and gaps between steps are retained
in the JSON evidence but omitted from the compact table. Independent column
medians do not necessarily add up to the median execution time. Skipped
steps contribute no duration; a zero test median can mean the preceding
Rust gate failed before Bun tests ran, not that those tests were removed.

The raw baseline JSON and representative build logs are retained outside
build/cache directories at `/tmp/aft-ci-speed-bg_2b4afa03/` in the measurement
worker session. Run links below are the durable public source of truth.

## Baseline runs

| Run | Branch | Conclusion | Wall minutes |
| --- | --- | --- | ---: |
| [38536886943](https://github.com/cortexkit/aft/actions/runs/38536886943) | train/grep-deadline-test | failure | 38.25 |
| [38534618492](https://github.com/cortexkit/aft/actions/runs/38534618492) | train/retention-drain | failure | 38.15 |
| [38532646078](https://github.com/cortexkit/aft/actions/runs/38532646078) | train/perf-a14 | failure | 37.18 |
| [38530791502](https://github.com/cortexkit/aft/actions/runs/38530791502) | train/runon-windows-desc | failure | 38.25 |
| [38529211282](https://github.com/cortexkit/aft/actions/runs/38529211282) | train/relative-paths | failure | 34.23 |
| [38527434230](https://github.com/cortexkit/aft/actions/runs/38527434230) | train/perf-a04 | failure | 35.30 |
| [38525636966](https://github.com/cortexkit/aft/actions/runs/38525636966) | train/hook-refresh | failure | 34.33 |
| [38523935853](https://github.com/cortexkit/aft/actions/runs/38523935853) | train/identifier-prose-ranking | failure | 33.82 |
| [38522238644](https://github.com/cortexkit/aft/actions/runs/38522238644) | train/identifier-prose-rows | failure | 38.43 |
| [38522040626](https://github.com/cortexkit/aft/actions/runs/38522040626) | main | failure | 45.65 |
| [38519315798](https://github.com/cortexkit/aft/actions/runs/38519315798) | train/scope-end-kill | success | 28.28 |
| [38518196081](https://github.com/cortexkit/aft/actions/runs/38518196081) | train/perf-a04 | failure | 28.45 |
| [38515885699](https://github.com/cortexkit/aft/actions/runs/38515885699) | train/hook-refresh | failure | 34.43 |
| [38515649944](https://github.com/cortexkit/aft/actions/runs/38515649944) | main | cancelled | 46.63 |
| [38512878392](https://github.com/cortexkit/aft/actions/runs/38512878392) | train/callgraph-slice | success | 31.10 |

Median wall time is **35.30 minutes**, range **28.28–46.63 minutes**.
The two main runs use the fat-LTO `release` profile; the train runs use
`stage`. Comparing them without labeling the profile would conflate cache
and optimization/linking cost.

## Baseline job medians

Queue is in seconds; all other columns are minutes. Summary jobs keep their
original names and dependency checks that require every constituent lane to
succeed; a failed or cancelled dependency fails the summary too. The
Compile+test column is for mixed compile-and-execute shell gates, separate
from steps that only build or only run tests.

| Job | Queue s | Execution min | Setup/other min | Compile min | Test min | Compile+test min |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| E2E / E2E (macOS native) | 10.00 | 35.12 | 0.70 | 31.78 | 1.87 | 0.00 |
| Unit / Release storm (Linux) | 4.00 | 22.63 | 0.27 | 0.00 | 0.00 | 22.33 |
| E2E / E2E (Windows native) | 4.00 | 22.47 | 1.30 | 17.52 | 3.03 | 0.00 |
| Unit / Cargo test archive (macOS) | 10.00 | 20.32 | 0.93 | 19.20 | 0.00 | 0.00 |
| Unit / Cargo libtest archive (macOS) | 9.00 | 18.58 | 0.70 | 17.67 | 0.10 | 0.00 |
| Unit / Cargo bin-test archive (macOS) | 9.00 | 18.52 | 0.73 | 17.50 | 0.00 | 0.00 |
| Search quality binary (Linux x64) | 3.00 | 15.82 | 0.32 | 15.52 | 0.00 | 0.00 |
| Unit / Unit tests (Linux) | 4.00 | 15.50 | 0.80 | 1.68 | 0.00 | 13.07 |
| search-quality-gate | 3.00 | 11.82 | 0.30 | 0.00 | 0.10 | 11.37 |
| Unit / Cargo libtest shard 1/3 (Windows) | 3.00 | 11.30 | 0.85 | 0.00 | 10.32 | 0.00 |
| Unit / Cargo test archive (Windows) | 4.00 | 10.80 | 3.92 | 6.72 | 0.00 | 0.00 |
| Unit / Cargo integration shard 3/3 (Windows) | 3.00 | 10.78 | 0.42 | 0.00 | 10.35 | 0.00 |
| E2E / OpenCode 2 (Linux Docker) | 3.00 | 10.75 | 1.17 | 0.00 | 9.58 | 0.00 |
| E2E / E2E binary (Linux x64) | 4.00 | 10.17 | 0.42 | 9.48 | 0.00 | 0.00 |
| Unit / Cargo conformance + Bun tests (macOS) | 8.00 | 9.77 | 1.12 | 3.37 | 0.00 | 4.57 |
| Unit / Cargo watcher + conformance (Windows) | 4.00 | 9.23 | 0.65 | 0.00 | 0.00 | 8.48 |
| Unit / Cargo watcher tests (macOS) | 10.00 | 8.07 | 0.60 | 0.00 | 0.00 | 7.33 |
| Unit / Cargo libtest shard 2/3 (Windows) | 3.00 | 7.02 | 0.83 | 0.00 | 6.13 | 0.00 |
| Unit / Cargo integration shard 3/3 (macOS) | 7.50 | 6.29 | 0.60 | 0.00 | 5.60 | 0.00 |
| Unit / Cargo libtest shard 3/3 (Windows) | 3.00 | 6.05 | 0.82 | 0.00 | 5.20 | 0.00 |
| Unit / Cargo integration shard 1/3 (macOS) | 8.00 | 5.91 | 0.76 | 0.00 | 5.07 | 0.00 |
| Unit / Cargo integration shard 2/3 (macOS) | 9.00 | 5.53 | 0.76 | 0.00 | 4.70 | 0.00 |
| Unit / Bash permission e2e (Windows) | 3.00 | 5.23 | 1.62 | 3.10 | 0.43 | 0.00 |
| Unit / Cargo integration shard 2/3 (Windows) | 3.00 | 5.18 | 0.37 | 0.00 | 4.77 | 0.00 |
| Unit / Cargo integration shard 3/3 (Linux) | 3.00 | 5.10 | 0.60 | 0.00 | 4.40 | 0.00 |
| Unit / Cargo test archive (Linux) | 4.00 | 5.02 | 0.95 | 3.98 | 0.00 | 0.00 |
| Unit / Cargo integration shard 1/3 (Windows) | 3.00 | 4.52 | 0.37 | 0.00 | 4.13 | 0.00 |
| Unit / Plugin e2e (opencode-ndjson, Linux) | 4.00 | 4.40 | 0.52 | 1.45 | 2.27 | 0.00 |
| Unit / Cargo integration shard 1/3 (Linux) | 3.00 | 4.05 | 0.40 | 0.00 | 3.62 | 0.00 |
| Unit / Cargo check deny-warnings (Windows) | 4.00 | 4.00 | 1.42 | 2.40 | 0.00 | 0.00 |
| Unit / Cargo libtest shard 2/3 (macOS) | 9.00 | 3.92 | 0.88 | 0.00 | 3.08 | 0.00 |
| Unit / Cargo integration shard 2/3 (Linux) | 3.00 | 3.88 | 0.53 | 0.00 | 3.07 | 0.00 |
| Unit / Cargo libtest shard 1/3 (macOS) | 9.00 | 3.77 | 0.79 | 0.00 | 2.88 | 0.00 |
| Unit / Cargo libtest shard 3/3 (macOS) | 9.00 | 3.64 | 0.82 | 0.00 | 2.73 | 0.00 |
| Unit / Sandbox real-kernel Landlock probes | 4.00 | 3.17 | 0.25 | 1.48 | 1.35 | 0.00 |
| Unit / Plugin e2e (bridge, Linux) | 4.00 | 2.95 | 0.53 | 1.55 | 0.85 | 0.00 |
| E2E / E2E (Linux Docker) | 3.00 | 2.72 | 1.52 | 0.00 | 1.13 | 0.00 |
| Unit / Plugin e2e (opencode-subc, Linux) | 4.00 | 2.60 | 0.55 | 1.43 | 0.55 | 0.00 |
| Unit / Plugin e2e (pi, Linux) | 4.00 | 2.47 | 0.57 | 1.42 | 0.42 | 0.00 |
| Unit / MSRV check (rustc 1.92) | 4.00 | 1.68 | 0.35 | 1.25 | 0.00 | 0.00 |
| Unit / Prove Rust test inventory (macOS) | 8.00 | 1.37 | 1.32 | 0.00 | 0.00 | 0.00 |
| E2E / E2E (Pi RPC) | 3.00 | 1.30 | 0.30 | 0.03 | 0.92 | 0.00 |
| Unit / Cargo libtest + watcher (Windows) | 3.00 | 0.05 | 0.02 | 0.00 | 0.00 | 0.00 |
| Unit / Unit tests (macOS) | 3.00 | 0.05 | 0.00 | 0.00 | 0.00 | 0.00 |

## Critical path and changes under measurement

1. **macOS native E2E is the primary bottleneck**, not just the unit archives.
   Its compile step alone has a 31.78-minute median; the E2E harness has a
   1.87-minute median. The original workflow comment called this a debug
   build, but the command uses `stage` on trains and `release` on main.
2. The macOS integration archive (20.32-minute job median) must finish before
   the slowest integration shard (6.29-minute median) and the summary. The
   inventory proof and conformance lane add further dependencies. Medians
   of individual jobs are not a measured end-to-end DAG duration.
3. The **release-storm cost is compilation**, not 22 minutes of stress tests.
   As supporting evidence from one run (not the 22.63-minute job median),
   [38536886943](https://github.com/cortexkit/aft/actions/runs/38536886943)'s
   Linux storm log reports `Finished release ... in 23m 03s`,
   then 18 tests in 61.527 seconds. Its final sccache stats show zero requests,
   consistent with a daemon having exited before the final stats query.
4. Linux unit tests spend 13.07 minutes in a combined Rust gate. This gate
   includes additional feature-enabled compilations; step timings alone do
   not support attributing all of it to execution. It is not the longest
   pipeline branch in this sample.
5. The search-quality path also matters: a 15.82-minute release producer
   precedes an 11.82-minute evaluation job. Fixing only macOS would leave
   that path near or above 25 minutes on a cache miss.
6. Windows archive consumers and native E2E are next, around 22 minutes
   including their producer dependency or build. Bun/installer setup is not
   the dominant cost: nextest installation median 1 second/p95 4 seconds,
   Bun setup 2/3 seconds, stable rustup setup 6/11 seconds, dependency install
   2/43 seconds. Extra installer caches are deferred rather than adding
   restore overhead to save a one-second direct install.

### Cache alternatives

- **A, existing sccache GHA backend + dependency-only rust-cache:**
  `d0aa8fa0ab6bdf117f18785737c75e15ffda234a`, branch
  `ci-speed/sccache-bg2b4afa`, run
  [38543321218](https://github.com/cortexkit/aft/actions/runs/38543321218).
  `SCCACHE_IDLE_TIMEOUT=0` keeps the daemon and its counters alive between
  steps. It is not proof that a long compile itself would otherwise be
  cancelled: sccache's shutdown path allows active requests to finish. The
  representative baseline macOS native log also reports an unexpected
  server shutdown and subsequent local compilation; the cause is not proven.
- **B, sccache GHA + rust-cache workspace libraries:**
  `36c7d430e8d38aa6891bb5cd46f16d00f2ac3a22`, branch
  `ci-speed/workspace-bg2b4afa`, run
  [38544407835](https://github.com/cortexkit/aft/actions/runs/38544407835).
  The search-quality producer, all three E2E producers, Linux unit/integration/
  storm producers, three macOS archive producers and the Windows archive
  producer enable `cache-workspace-crates: true` (eleven jobs).
  Integration/libtest/bin archive producers use distinct keys so the first
  producer to finish cannot freeze a cache containing only its own inventory.
  macOS continues excluding `~/.cargo/bin`: restoring cached cargo/rustup
  shims there can overwrite the fresh toolchain setup with obsolete shims.

Both variants retain every original job name, matrix, dependency, timeout,
build/test command and failure policy. The macOS and Windows summary jobs
remain fail-closed. All 26 names in `.github/required-checks.txt` are unchanged.
No runner is self-hosted. Existing archive sharing and parallel scheduling
are retained: combining the three macOS archive builds into one runner would
serialize large test-harness compiles rather than remove the critical path.

### Interpretation constraints

- Attempt 1 of each A/B run above is cold for **new branch-scoped cache
  entries**, not an
  assertion that every dependency/compiler cache is empty: default-branch
  caches may be restored. A rerun is warm only for entries actually saved by
  the first attempt. Logs must confirm those hits; the attempt number alone
  does not prove cache warmth.
- A and B were launched concurrently; organization runner concurrency can
  inflate queue time. Compare execution time separately from queue time.
- The `ci-speed/*` branches use `stage` just like trains. They do **not** prove
  main's fat-LTO `release` path is below 25 minutes.
- rust-cache retains workspace library artifacts, not every binary/link
  product. Its stable exact keys can skip subsequent saves after a source
  change; Cargo still validates source freshness and sccache is content keyed,
  but warm same-commit improvements should not be extrapolated to every new
  commit. The default-branch cache is the only common inherited cache for
  sibling train branches, not caches produced by other trains.

## Real-run comparison

Cold and warm attempts are collected separately after they complete. Do not
claim the under-25-minute objective from an incomplete run or from a faster
failed test gate that stopped before running its full inventory.

### C: reduced CI test/debug information, without broad workspace caches

C restores A's dependency-only cache configuration and sets
`CARGO_PROFILE_DEV_DEBUG=line-tables-only` and
`CARGO_PROFILE_TEST_DEBUG=line-tables-only` in the unit reusable workflow.
The dev setting keeps the explicit debug builds and test builds compatible;
otherwise the same library can be compiled with two debug-info settings.
These overrides change debug information only: they do not remove debug
assertions, optimization checks, test invocations, or the existing native
backtrace/dSYM/`atos` probes. `stage` and `release` profiles are not overridden.

The broad workspace-cache guard tests added solely for variant B are removed
with that experiment's cache settings, not weakened into passing assertions.
All pre-existing tests remain, and the replacement guard checks that both CI
debug profiles retain line tables and that release/stage debug settings are
not overridden. A Linux real backtrace probe with both settings ran and
passed: `logging::tests::panic_hook_preserves_default_backtrace_behavior_and_persists`
(1 test, cargo 1.99.0). Native macOS probes still require hosted-run evidence.

### Cache pressure observed during A/B

An Actions cache-usage snapshot reported **11,427,931,461 bytes / 753 entries**
while the variants coexisted. A paginated inventory taken afterward showed
about **7,489 MiB** associated with B's branch, including three separate
**721 MiB** macOS archive caches. The inventory is not an atomic snapshot:
concurrent saves and eviction can change entries while it is paged. It
therefore cannot be added to the earlier usage snapshot as though collected
at the same instant. Saved cache sizes, per-attempt cache-hit behavior and
write-error counters matter more than assuming an attempt is warm.

A cold macOS native log reported 617 compile requests, 87 hits, 409 misses
and 75 cache-write errors. Unlike the baseline's final two-request daemon,
its counters cover the build; these errors' cause is not established by the
stats alone. The default cache budget is approximately 10 GB, so retaining
large per-job copies across sibling measurement/train branches risks evicting
useful default-branch and compiler entries. C deliberately does not keep B's
broad workspace-cache settings.

### B's additional failures and artifact provenance

B's first-attempt macOS libtest archive log says **`No cache found.`**, then
`Compiling agent-file-tools` and `Finished test ... in 23m 33s`. The serial
panic/backtrace probe passed. Its shard consumers downloaded this freshly
built archive. Rust source, Cargo manifests/lockfile and `.cargo` inputs are
identical between A and B. Thus a restored stale **workspace** library or
build-script output cannot explain these failures in B's first attempt:
the workspace cache was a miss before the archive build.

- `exec_remote_bash_post_spawn_persistence_failure_never_claims_no_run`
  failed at `crates/aft/src/bash_background/remote_tests.rs:1875` with
  `left: []`, `right: [120]`. The test waits for `counter.exists()` at
  lines 1868–1874, then immediately reads bytes. A shell's file creation can
  precede its `printf x` write, matching the observed empty-file race. This
  is runtime synchronization, not a generated/cached workspace input.
- `root_without_a_project_watcher_watches_its_config_files_itself` failed
  after 11.046 seconds with `no reload was requested by the watch` at
  `crates/aft/src/config_live_tests.rs:602`. It writes a temporary runtime
  config and waits up to 10 seconds for a watch event; the exact reason the
  event was absent on that runner is not established. It does not directly
  consume a cached build artifact.

These are suspected runtime flakes, not silently fixed, retried away or
removed. Failed shards stopped early (136/2064 and 939/2111 libtests ran),
so their reduced durations are **not** speedup evidence for full inventories.
`scope_revocation_live_new_epoch_holds_group` also failed in the Linux and
macOS gates of control run 38543321218 and recent baseline runs; its separate
main fix is outside this change. These existing failures keep the full
workflow red even where the cache/compile changes are independently verified.

### D: binary-owned per-commit provenance

`crates/aft/build.rs` deliberately watches Git refs and emits the exact
checkout SHA. Previously `launch_nonce.rs` consumed that value with
`option_env!` inside the large library, making even a workflow-only commit a
new library compiler-cache input. D moves the macro use to `main.rs`, which
hands the validated revision to a set-once library declaration before CLI
handling or HELLO. The build script and package layout are unchanged.

An embedder/test process that never runs binary startup reports no SHA, as a
build without the emitted env would; an already reported unknown identity
cannot later be upgraded. The shipped binary still reports the actual SHA.
The existing unit test's assertion of a compiled library SHA is intentionally
changed to the new unknown-without-startup contract, and is defended by new
set-once/unknown tests plus the real-binary HELLO test's stronger exact-SHA
assertion. All nonce source/value checks and child-environment scrubbing are
retained.

Before/after binaries built at the **same HEAD**
`907a7092cfb4b68a6e9b4f2d108f0f48879c4cd3` both emitted:

```text
aft 0.59.0
{"build_git_sha":"907a7092cfb4b68a6e9b4f2d108f0f48879c4cd3","wire_crate_version":"0.31.0","launch_nonce_source":"fd"}
```

The same real-binary test asserted the fixed pipe nonce rather than the
conflicting environment nonce, and clean foreground/background/PTY children,
in both builds. Neutralizing binary revision initialization made that exact
test fail with `HELLO build Git SHA`; restoring it passed. A source fence
finds any library `env!`/`option_env!` read of an emitted build-script value,
and is now run early in the existing Linux unit job. Reintroducing the Git
macro in the library made only that fence test fail.

This is **sccache reuse**, not a claim of Cargo `Fresh`: Cargo still invokes
rustc after the package's Git-sensitive build script runs. A small Cargo 1.99
probe demonstrated that merely moving macro consumption does not suppress
that Cargo invocation. Library dep-info and cache-decision logs from two
commits with identical Rust source must establish the actual cache hit; the
search-quality producer reports per-library before/after sccache integer
counters in its ordinary job log for this measurement. Raw compiler debug
logs/environments are not published, because they may contain cache-service
credentials. None of the ranking/routing source paths registered in
`benchmarks/aft-search/search_quality_lib.py:RANKING_FENCE_PREFIXES` is edited;
this experiment changes compilation/provenance, not query ranking policy.

## Completed A/B/C comparison

All attempts used GitHub-hosted runners and the train-like `stage` E2E
profile. Each has 44 job records; none is a successful full-suite result.
A/B/C preserve all existing required check names and summary jobs. B had an
operator-triggered third attempt; attempt 2 is the primary warm comparison,
and attempt 3 overlapped A/C execution while sharing the repository-wide
cache storage/eviction budget. Jobs marked † below failed or
stopped early, so their elapsed time is not a complete-inventory speedup.

| Variant | Run/attempt | Wall min | Median queue s | Max queue s | Rust-cache uploaded bytes | Restores / misses | sccache hit rate | sccache write errors |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| A cold | 38543321218/1 | 34.43 | 4 | 13 | 1,639,289,990 | 25 / 3 | 13.75% | 229 |
| A warm | 38543321218/2 | 31.17 | 4 | 12 | 3,111,392,554 | 16 / 11 | 14.67% | 2,179 |
| B cold | 38544407835/1 | 32.77 | 3.5 | 12 | 6,845,851,555 | 19 / 9 | 15.14% | 2,441 |
| B warm | 38544407835/2 | 35.27 | 5 | 14 | 5,509,322,083 | 16 / 12 | 11.42% | 2,488 |
| B warm, coexistence | 38544407835/3 | 42.42 | — | — | 8,959,210,463 | 6 / 22 | 11.98% | 3,453 |
| C cold | 38551284210/1 | 32.40 | 4 | 11 | 5,684,163,409 | 7 / 21 | 7.68% | 4,989 |
| C warm | 38551284210/2 | 39.30 | 4 | 13 | 4,702,775,859 | 10 / 18 | 4.26% | 2,335 |

Rust-cache bytes count completed `Sent N of N (100%)` uploads during each
job's rust-cache post step. They exclude sccache object bytes (not exposed by
its stats) and other tool/artifact caches. Hit rates aggregate reported
sccache hits/misses across Rust, C/C++ and assembler, not just AFT's library.
Use the last JSON stats record per job, not the duplicate human-readable
summary. Historical attempt ZIPs sometimes contain only whole-job logs;
step-file-only parsing would incorrectly report zero bytes/counters for B2.
The retained raw ZIPs and JSON summaries allow auditing these totals.

Warm labels did **not** mean the caches survived: B2 rebuilt/reuploaded
5.51 GB, B3 8.96 GB, and B3 restored only six rust-cache entries while missing
22. C's warm hit rate fell to 4.26%. Queue medians stayed at 3.5–5 seconds;
these regressions cannot be explained by runner queue time alone. The cause
of individual cache-write errors is not proved by aggregate counters, but
cache churn and failure to persist/reuse artifacts are directly observed.
**Reject B's broad workspace caching as a default**; C and D retain the
original dependency-only layout instead.

### Job execution minutes (queue excluded)

| Job | A cold | A warm | B cold | B warm | C cold | C warm |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| E2E / E2E (Linux Docker) | 2.28 | 2.67 | 2.30 | 2.62 | 4.07 | 2.77 |
| E2E / E2E (Pi RPC) | 0.98 | 1.38 | 1.47 | 1.45 | 1.58 | 1.27 |
| E2E / E2E (Windows native) | 22.93 | 0.67† | 22.15 | 6.55 | 24.23 | 10.83 |
| E2E / E2E (macOS native) | 22.73 | 15.92 | 23.35 | 27.35 | 26.63 | 19.42 |
| E2E / E2E binary (Linux x64) | 7.37 | 8.42 | 8.62 | 10.70 | 8.20 | 10.97 |
| E2E / OpenCode 2 (Linux Docker) | 11.18 | 11.18 | 10.88 | 8.83 | 10.05 | 10.82 |
| Search quality binary (Linux x64) | 13.52 | 19.23 | 16.25 | 18.52 | 14.28 | 21.17 |
| Unit / Bash permission e2e (Windows) | 5.07 | 5.22 | 6.42 | 4.82 | 8.42 | 7.90 |
| Unit / Cargo bin-test archive (macOS) | 23.15 | 21.85 | 13.93 | 20.62 | 19.80 | 24.98 |
| Unit / Cargo check deny-warnings (Windows) | 3.82 | 3.88 | 3.60 | 3.82 | 5.53 | 5.45 |
| Unit / Cargo conformance + Bun tests (macOS) | 9.92† | 5.80† | 7.25 | 10.70† | 9.90† | 13.88 |
| Unit / Cargo integration shard 1/3 (Linux) | 3.85 | 3.35 | 4.13 | 3.88 | 4.28 | 4.58 |
| Unit / Cargo integration shard 1/3 (Windows) | 6.98 | 4.25 | 4.82 | 7.48 | 8.53 | 4.88 |
| Unit / Cargo integration shard 1/3 (macOS) | 5.45 | 6.07 | 7.68 | 6.63 | 6.08 | 7.00 |
| Unit / Cargo integration shard 2/3 (Linux) | 3.65 | 4.43 | 4.03 | 4.08 | 4.33 | 3.73 |
| Unit / Cargo integration shard 2/3 (Windows) | 4.48 | 6.00 | 5.53 | 5.62 | 4.95 | 5.12 |
| Unit / Cargo integration shard 2/3 (macOS) | 6.12 | 5.93 | 5.93 | 5.50 | 6.15 | 5.55 |
| Unit / Cargo integration shard 3/3 (Linux) | 5.28 | 4.22 | 4.78 | 4.80 | 4.07 | 5.20 |
| Unit / Cargo integration shard 3/3 (Windows) | 10.45 | 10.23 | 10.57 | 10.47 | 11.67† | 12.83 |
| Unit / Cargo integration shard 3/3 (macOS) | 7.03 | 6.98 | 7.72 | 6.48 | 7.00 | 2.40† |
| Unit / Cargo libtest + watcher (Windows) | 0.03 | 0.05 | 0.05 | 0.05 | 0.03 | 0.07 |
| Unit / Cargo libtest archive (macOS) | 24.03 | 15.13 | 25.07 | 24.08 | 9.97 | 10.57 |
| Unit / Cargo libtest shard 1/3 (Windows) | 11.05 | 10.73 | 11.38 | 12.10 | 11.02 | 11.53 |
| Unit / Cargo libtest shard 1/3 (macOS) | 3.67 | 4.22 | 1.72† | 4.62 | 3.92 | 4.20 |
| Unit / Cargo libtest shard 2/3 (Windows) | 8.05 | 12.05 | 6.20 | 10.57 | 6.05 | 6.57 |
| Unit / Cargo libtest shard 2/3 (macOS) | 4.52 | 4.10 | 4.32 | 3.38 | 3.88 | 4.15 |
| Unit / Cargo libtest shard 3/3 (Windows) | 5.92 | 6.58 | 6.25 | 5.68 | 6.32 | 6.72 |
| Unit / Cargo libtest shard 3/3 (macOS) | 3.33 | 4.08 | 2.92† | 4.55 | 3.47 | 4.07 |
| Unit / Cargo test archive (Linux) | 5.15 | 7.28 | 6.77 | 7.48 | 5.40 | 6.88 |
| Unit / Cargo test archive (Windows) | 7.68 | 10.22 | 13.97 | 11.02 | 11.22 | 10.32 |
| Unit / Cargo test archive (macOS) | 22.62 | 17.83 | 20.80 | 21.92 | 22.02 | 8.53 |
| Unit / Cargo watcher + conformance (Windows) | 8.93 | 9.28 | 8.52 | 9.52 | 11.77 | 9.95 |
| Unit / Cargo watcher tests (macOS) | 6.73 | 7.22 | 7.00 | 7.67 | 9.38 | 5.08 |
| Unit / MSRV check (rustc 1.92) | 1.67 | 3.00 | 1.62 | 3.75 | 2.85 | 3.98 |
| Unit / Plugin e2e (bridge, Linux) | 2.60 | 4.23 | 2.82 | 4.03 | 4.05 | 3.73 |
| Unit / Plugin e2e (opencode-ndjson, Linux) | 3.82 | 4.32 | 5.13 | 5.70 | 5.57 | 5.88 |
| Unit / Plugin e2e (opencode-subc, Linux) | 2.47 | 3.32 | 2.92 | 4.22 | 4.02 | 3.50 |
| Unit / Plugin e2e (pi, Linux) | 2.27 | 3.92 | 2.45 | 3.93 | 3.47 | 3.87 |
| Unit / Prove Rust test inventory (macOS) | 1.60 | 1.50 | 1.10 | 1.03 | 1.50 | 1.28 |
| Unit / Release storm (Linux) | 17.47 | 27.00 | 22.83 | 21.65 | 25.75 | 22.47 |
| Unit / Sandbox real-kernel Landlock probes | 3.17 | 5.10 | 2.78 | 4.63 | 4.85 | 4.10 |
| Unit / Unit tests (Linux) | 16.48† | 16.93† | 16.18 | 14.40† | 16.33† | 15.87† |
| Unit / Unit tests (macOS) | 0.05† | 0.05† | 0.12† | 0.05† | 0.07† | 0.03† |
| search-quality-gate | 9.22 | 11.77 | 6.80 | 11.80 | 9.68 | 9.07 |

C materially cuts the macOS libtest archive from A's 24.03/15.13 minutes to
9.97/10.57 minutes and keeps the native symbolication/backtrace tests. It does
**not** achieve the whole-workflow objective: the bin archive and conformance
chain still reached roughly 39 minutes in C warm, and release compilation
plus search-quality evaluation remained around 24–31 minutes. D must prove
cross-commit library cache reuse before claiming those paths are solved.

### Search build policy candidates (report only)

The current `ci_quality_mode.py` permits `verify` only for a manifest-only
diff; every other diff evaluates. This change does not alter that selector,
its registered ranking/routing source restrictions or any quality threshold.

Using each historical gate's **logged merge-base**, not just the tip commit's
parent, one sampled train has neither a registered ranking-source difference
nor a conservative product build-input difference (the registered
`RANKING_FENCE_PREFIXES` list above, plus all product Rust/tokenizer sources,
Cargo manifests/lockfile, build script and `.cargo` configuration):

- `train/identifier-prose-rows`, run 38522238644,
  `1a87fec37ba54a0edcd6ac65bffdbd38851cff87...bf2cd8f0ffea30f63ef3566510355f7a3cbb6e82`.
  Changes are `.gitignore`, benchmark row authoring/harness/tests, query vectors,
  manifest/baseline/descriptors and versioned evidence JSON under `docs/`.
  No `crates/aft/src`, tokenizer, Cargo manifest/lockfile, build script or
  `.cargo` input changes. It still needs quality evaluation for the changed
  benchmark inputs, but not a newly optimized search implementation.

Other sampled trains without ranking-prefix changes still modify product
Rust input (for example parser/outline, retention or subc code), so the
conservative audit does **not** declare them safe to skip a fresh build.
The dependency-cache/stats control A, workspace-cache experiment B and
CI test-debuginfo experiment C (defined above) likewise leave the production
release recipe/source untouched. Reusing an older executable would still
require preserving accurate build-Git provenance; excluding that requirement
is a separate policy decision, not an optimization made here. The exact
13 train diff inventories are retained in `search-build-eligibility.json`.

### macOS archive deadline headroom

No timeout is increased. These are whole-job execution times, so successful
headroom includes setup/upload overhead; a cancellation at the limit can
report a slightly longer job duration because cleanup follows the killed step.

| Archive | Existing limit min | C cold min | C cold headroom min | D1 cold min | D1 cold headroom |
| --- | ---: | ---: | ---: | ---: | ---: |
| Integration (`Cargo test archive (macOS)`) | 30 | 22.02 | 7.98 | 30.72, cancelled | 0; limit reached |
| Library (`Cargo libtest archive (macOS)`) | 30 | 9.97 | 20.03 | 21.55 | 8.45 min |
| Binary (`Cargo bin-test archive (macOS)`) | 25 | 19.80 | 5.20 | 20.82 | 4.18 min |

C warm's bin archive finished at 24.98 minutes, only about 1 second inside
its 25-minute limit. A separate train run 38560400715 also cancelled its bin
archive at that limit. The cold integration path therefore **does not fit
structurally yet**, and bin headroom is not robust.

Line-tables already reduce the oversized libtest harness while retaining the
native probes. D can remove repeated production-library compilation on warm
same-source commits only if its library result was actually stored and the
next runner hits it. That can improve integration/bin archive budgets, but
is not a guarantee for a genuinely empty cache or changed Rust source. If D2
still misses or cold deadlines remain unsafe, the next structural experiment
should partition archive compilation targets (and prove exact inventory)
rather than raise these limits or remove tests. Combining all archives on
one runner would serialize compilation; a prebuild dependency also needs
end-to-end measurement because it can merely move the cold compile before
all shards. No unmeasured split is claimed as an implemented fix here.
