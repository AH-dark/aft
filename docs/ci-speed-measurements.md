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
original names and fail-closed dependency checks.

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
  Eleven dominant producers enable `cache-workspace-crates: true`.
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
