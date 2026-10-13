# AFT performance audit (2026-10)

**Scope:** the whole repository at commit `21126fc`. That covers the Rust engine `crates/aft` (~670k lines, including inline tests) and `crates/aft-tokenizer`, plus the TypeScript packages `aft-bridge`, `opencode-plugin`, `pi-plugin` and `aft-cli` (~80k lines). Test-only code was excluded.
**Method:** static code review only, with no code changes. The code was split into 14 subsystem areas and each was audited on its own. Every finding names the file and line, how often the code runs, what goes wrong, the estimated impact, a suggested fix, and a confidence level. The four CRITICAL findings and several HIGH ones were re-checked against the source while this report was being consolidated. **No profiling or benchmarks were run.** Impact figures are estimates from reading the code and should be confirmed with measurements before investing in a fix.

**Totals: 187 findings — 4 CRITICAL, 41 HIGH, 70 MEDIUM, 72 LOW.**

| # | Area | CRIT | HIGH | MED | LOW |
|---|------|-----:|-----:|----:|----:|
| 1 | Trigram search, grep/glob | 0 | 4 | 8 | 5 |
| 2 | Semantic index and search | 0 | 3 | 5 | 5 |
| 3 | Callgraph | 0 | 5 | 7 | 5 |
| 4 | Parsing, read/outline/zoom | 0 | 2 | 6 | 6 |
| 5 | Edit / hashline / backup | 0 | 5 | 5 | 4 |
| 6 | Core runtime, executor, tool-call pipeline | 0 | 1 | 4 | 7 |
| 7 | Subc daemon edge, gh shim | 2 | 1 | 3 | 6 |
| 8 | Inspect (Tier-1/Tier-2) | 1 | 7 | 4 | 4 |
| 9 | Views, blob store, GC, storage (views off by default) | 0 | 4 | 6 | 4 |
| 10 | Bash, background tasks, compression | 0 | 4 | 5 | 7 |
| 11 | LSP | 1 | 1 | 4 | 6 |
| 12 | Persistence, watchers, locks, logging | 0 | 3 | 5 | 5 |
| 13 | TS bridge | 0 | 0 | 4 | 3 |
| 14 | TS plugins and CLI | 0 | 1 | 4 | 5 |

---

## 1. Executive summary

Overall the code is carefully engineered. Tree-sitter queries are compiled once. Snapshots are shared through `Arc`. SQLite runs in WAL mode with batched transactions and cached statements. The NDJSON framing is linear and bounded. Most request paths have work budgets. The serious problems fall into a small number of recurring patterns.

1. **Periodic background work that runs whether or not anything changed.** These are the worst offenders, because they burn CPU, disk and locks on an *idle* daemon.
   - Standing roots reconcile every 250 ms. Each pass spawns 2 git processes per root, opens and migrates aft.db, and runs one `BEGIN IMMEDIATE` per root. The aft.db open happens even when no standing roots are configured.
   - Every unbound standing root runs a full strict verify, or a full rebuild, back to back.
   - The subc health rollup runs every 3 s. It walks every resident index while holding those indexes' read locks and the LSP manager and callgraph connection mutexes, and it measures allocator statistics twice per refresh.
2. **Heavy work while holding a hot lock.**
   - rust-analyzer fingerprinting (subprocess spawns plus a workspace-wide stat/hash) runs inside `Mutex<LspManager>` on every `cargo check` begin and end.
   - Oxc analysis holds a global mutex for the whole analysis.
   - Inspect's `contribution_set_hash` walks the tree while holding the SQLite connection mutex.
   - Symbol-cache persistence serializes the whole cache while holding its read lock.
   - The blob durability mutex is held across fsyncs.
3. **Project-wide work for a one-file change.**
   - Inspect duplicates reloads every fragment hash as JSON, which can reach GBs on large repos.
   - Tier-2 rollups reload all contributions 2–3 times.
   - Callgraph re-resolves every caller after a line shift.
   - Views republishes the whole semantic index.
   - Semantic refresh helpers walk the whole shared base.
   - Strict-hash passes over the whole project run on the dispatch thread.
4. **Scheduling and latency on the bash hot path.**
   - Bash calls are classified as exclusive writers, so they wait for every reader and maintenance commit on the root.
   - Reads are preferred over writers for up to 6 s.
   - In subc mode, a piped child's exit is noticed only on the 500 ms watchdog tick.
   - Finalization for every root runs inline on that single watchdog thread.
5. **Repeated or redundant work per call.**
   - Outline re-parses files that are already in the symbol cache and reads each file 3–4 times.
   - An edit runs about 6 full-file reads and about 5 unbounded Myers diffs.
   - Server-root resolution runs 3–6 times per LSP operation.
   - Indexed grep/glob spawns one thread per directory.
   - Mutations ship full before and after text over NDJSON, and the host then re-diffs it.
6. **Serial loops where the work could run in parallel.** Examples: outline over many files, `move_symbol`'s parse of the whole project, symbol prewarm, re-indexing after a branch switch, formatter spawns for multi-file edits, and HTTP embedding requests.
7. **Unbounded growth in the long-lived daemon.** Examples: `AlertDeltaState` sessions, the in-memory backup store (which keeps two copies and never evicts), `OxcFactsCache`, the TS subc session cache and `configureWarningClients`, unbounded watcher channels, and per-session subc maps.

## 2. Prioritized fix list (recommended order)

The order weighs user-visible impact × how often the code runs ÷ cost of the fix. The links point to the detailed findings in §3.

| Pri | Finding | Area | Why first |
|----|---------|------|-----------|
| P0 | Standing-root reconcile every 250 ms (git spawns, aft.db open and migrate, an IMMEDIATE txn per root) | Subc §3.7 | Permanent load on every daemon; an easy gate on config change |
| P0 | Unbound standing roots re-run strict verify or a full rebuild back to back | Subc §3.7 | Continuous full-tree walk and hash; needs a dirty bit or minimum interval |
| P0 | rust-analyzer check fingerprinting under the LSP manager lock | LSP §3.11 | Every Rust save stalls all LSP calls; drops diagnostics |
| P0 | Inspect duplicates: JSON fragment store reloaded on every change | Inspect §3.8 | GB-scale memory and seconds of parsing per edit on large repos |
| P1 | Bash and bash plumbing on the `Mutating` writer barrier | Core §3.6 | Bash is the most-used tool; seconds of start latency under load |
| P1 | Subc piped-exit detection waits for the 500 ms watchdog tick; finalization runs on the single watchdog thread | Bash §3.10 | Adds ~250 ms on average to every foreground bash call |
| P1 | Health rollup every 3 s walks all indexes under their locks, and measures allocator stats twice | Subc §3.7 | Idle CPU plus periodic lock stalls on tool calls |
| P1 | Unbudgeted strict hashing and replays on the dispatch thread (semantic corpus completion, watcher semantic phase) | Persistence §3.12 | Multi-second tool-call stalls after file bursts or checkouts |
| P1 | inotify Remove/rename scans every watched dir | Persistence §3.12 | Overflow-and-rescan cascade during checkouts |
| P1 | Tier-2: contributions reloaded 2–3 times, uncached entry points, 10–15 tree walks per pass, Oxc global mutex | Inspect §3.8 | Background CPU and slow blocking inspect |
| P1 | LSP applicability walk: uncached root resolution plus rebuilding `ServerDef`s per file | LSP §3.11 | Unscoped inspect hits its deadline in monorepos |
| P2 | Outline re-parses on cache hits; serial reads, 3–4 per file | Parse §3.4 | Common agent tool |
| P2 | Edit path: 6 reads and about 5 unbounded diffs per edit; batch is O(edits × size); undo loads the whole history; backup store keeps two copies and never evicts | Edit §3.5 | Every mutation; memory leak over a long session |
| P2 | Mutations ship full before and after text; an unused hashline preview round trip | TS plugins §3.14 | Every mutation; cuts NDJSON volume severalfold |
| P2 | Search: a thread per directory per request; glob sorts every match; full posting reads; fallback grep walks the whole tree first | Search §3.1 | Every grep/glob |
| P2 | Semantic: scalar brute-force scan; unsorted ORT batches (wasted padding); duplicate hashing on warm load | Semantic §3.2 | Query latency and build time |
| P2 | Callgraph: Rust qualified-call scan per call; caller fan-out on a line shift; dispatch re-parse; unbudgeted `impact` | Callgraph §3.3 | Build and refresh time on large repos |
| P3 | Views publication pipeline (only if views are enabled) | Views §3.9 | Off by default |
| P3 | MEDIUM and LOW items | all | Opportunistic cleanup |

**Quick wins (small, local fixes with clear payoff):**
- Gate standing reconcile on a config-generation change.
- Use `AllocatorSource::Cached` in the health rollup.
- Call `registry.poll_task` in `observe_deferred_bash_wait`.
- Cache `Runtime::hash()` per LSP client.
- Use `builtin_servers_shared()` plus a per-directory memo in the applicability walk.
- Use top-k selection instead of a full sort in glob.
- Use binary-search removal of delta postings.
- Length-sort ORT batches.
- Skip the unused hashline preview round trip.
- Put a fast path before the bounded diff in `edit`.
- Reap `AlertDeltaState` sessions.
- Use `prepare_cached` in the inspect SQLite loops.

## 3. Detailed findings by area

Each area's findings are listed in order of severity, followed by the auditor's area summary and a list of what was checked and found OK.

### 3.1 Trigram search & grep/glob

Files reviewed (all under `crates/aft/src`): `search_index.rs` (deep: query paths, snapshot/delta update, build, spill/merge, load/verify, path helpers, sorting), `grep_executor.rs` (deep), `commands/grep.rs`, `commands/glob.rs` (deep), `bounded_io.rs` (deep), `watcher_filter.rs` (runtime loop and filter; test half skimmed), `cache_freshness.rs`, `gitignore_state.rs`, `pattern_compile.rs`, `fuzzy_match.rs`, `query_shape.rs`, `search_b2/*` (router, variants, lane_plan, readiness, embed_counter), `walk_boundary.rs`, `readonly_artifacts.rs` (skim), `effective_path.rs` (skim: startup-only), `search_hot_path_measurements.rs` (test-only, `#[cfg(test)]` in lib.rs:199). I also followed call sites into `commands/semantic_search/lexical_lane.rs`, `commands/configure.rs` (warm load) and `runtime_drain.rs` (watcher drain, ignore refresh).

#### Findings

##### [HIGH] Every indexed grep/glob spawns one OS thread per distinct directory it touches (directory probe cache resets every request)
- Location: bounded_io.rs:316-388 (`directory`), :268-310 (`run_with_limiter`), :476-505 (`read`), :559-568 (`metadata`); callers: search_index.rs:3700 (`search_candidate_file` -> `read_indexed_file_bytes`), :3347 (`collect_grep_matches_by_file`), :5832 (`path_missing_on_disk`), :6046 (`path_modified_time`)
- Hot path / frequency: every indexed `grep` (rayon workers read each candidate file), every `aft_search` exact/regex verification, every glob page presence check (`take_present_on_disk`), and every mtime sort that falls back to stat.
- Problem: `read()`/`metadata()` call `parent()` -> `directory()`. The first time a request touches a directory, `directory()` calls `run()`, which does `std::thread::Builder::new().spawn(...)` (plus an mpsc channel and `recv_timeout`) to run `read_dir` + `statfs`. The result cache is keyed by the per-request `Budget` id (`DIRECTORIES` thread-local is cleared when `dirs.0 != budget.id`; `budget.directories` is created fresh in `Budget::after`). So every request pays a thread spawn, an opendir/readdir and a statfs for each distinct parent directory of every file it reads. The inline short-circuit (`HELPER_DEADLINE`) only applies inside a bounded walk, so the indexed paths (no walk) always spawn. The in-repo test `ten_thousand_file_grep_uses_directory_probes_not_per_file_helpers` asserts "one spawn per directory" as the target, which confirms this behavior.
- Impact: a common-term or fully-degraded grep on a 100k-file / ~10k-directory repo spawns up to ~10k threads (64 at a time through `Limiter`, with mutex/condvar handoff) and does ~30k extra syscalls. At ~20-50 µs per spawn+probe that adds 0.2-0.5 s of wall time spread over the rayon workers, every call. The same cost repeats on the next call.
- Suggested fix: cache `Access` across requests, keyed by directory plus device id (or by mount: `statfs` result per `st_dev`), and invalidate it on watcher events or after a TTL. Once the project root has been probed as `direct` on a local filesystem, treat descendants on the same `st_dev` as direct without probing (a single `lstat`, which the read path already does). Where a probe is still needed, run it on a persistent helper pool instead of `thread::spawn`.
- Confidence: High (mechanism); Medium (exact cost depends on platform).

##### [HIGH] Indexed glob materializes, allocates and fully sorts every match to return 100
- Location: search_index.rs:3427-3488 (`glob_profiled`); commands/glob.rs:340-372 (`glob_root`), :214 (`sort_paths_by_cached_mtime_desc`); search_index.rs:6000-6064 (`sort_paths_by_mtime_desc_with_key_normalization`)
- Hot path / frequency: every `glob` call, a very common agent tool.
- Problem: `glob_profiled` scans all `files` (expected), and for each one calls `is_within_search_root` up to twice and `PathFilters::matches`. Each of those normalizes both the root and the path into new `PathBuf`s, and `matches` also builds a relative `PathBuf` and a `to_glob_path` `String`: about 6-8 heap allocations per indexed file. It then clones every matching path. `glob_root` builds an `indexed_mtimes: HashMap<PathBuf, SystemTime>` with another clone of every match. `sort_paths_by_cached_mtime_desc` then canonicalizes the root (a syscall), builds two more `HashMap<PathBuf, _>`s (`mtimes`, `display_paths`) with two more clones per path plus a `normalize_path` and a display `String` per path, and sorts with a comparator that does four `PathBuf`-hashing map lookups per comparison. Only `DEFAULT_MAX_RESULTS` (100) paths are used afterwards (`take_present_on_disk`); the rest only feed `total`. `glob_root` also runs `has_file_in_scope` first (another O(n) normalize-per-file pass) even though `glob_profiled` returns `scope_has_files`.
- Impact: `**/*.ts` or `**/*` on a 100k-file repo means ~1M allocations plus an n log n sort with ~7M hash lookups, roughly 0.3-1 s per glob. The cost scales with the match count, not the page size.
- Suggested fix: have `glob_profiled` carry `(path_ref, modified)` from `FileEntry` and select the top `limit + MISSING_ON_DISK_REFILL_BUDGET` with `select_nth_unstable_by` or a bounded `BinaryHeap`, and return the total count separately. Compute the display tie-break key only for the selected items. Normalize the search roots once outside the loop and compare with `starts_with` against pre-normalized indexed paths (indexed paths are already canonical). Drop the separate `has_file_in_scope` pass.
- Confidence: High.

##### [HIGH] Lexical lane reads every query trigram's full posting list from disk, and the candidate pool is unbounded
- Location: search_index.rs:927-966 (`lexical_selected_pool`), :5503-5514 (`materialize_query_postings`), :3575-3634 (`postings_for_trigram`); also :765-838 (`lexical_rank_with_stats`) and :841-924 (`lexical_rank_at_depth`); caller commands/semantic_search/lexical_lane.rs:263 (`score_complete_selected_pool`)
- Hot path / frequency: every `aft_search` query with a lexical lane (the default hybrid search).
- Problem: `materialize_query_postings` preads and decodes the complete posting list of every query trigram, including very common ones (`the`, `ion`, `ent`, `re_`). Those lists hold nearly every file id: 100k files x 6 bytes is about 600 KB per trigram. Each posting then pays two `superseded` HashSet probes (`postings_for_trigram` checks `self.delta.superseded.contains` and then `is_active_file`, which checks it again) plus a `files` lookup, and a `Vec<u32>` is allocated. Only the three rarest lists are needed for discovery. The common lists serve only as membership tests for the scored candidates. `lexical_selected_pool` also scores, clones the `PathBuf` of, and fully sorts the whole union of the three rarest lists; the caller then truncates to `LEXICAL_MAX_DEPTH` (lexical_lane.rs:143). The rank variants apply `candidate_filter` (usually a glob match) to the whole union before `take(candidate_cap)`.
- Impact: for an NL query with ~30 trigrams on 100k files: up to ~18 MB of pread, ~3M hash probes and ~3M u32 pushes per query, roughly 20-80 ms of CPU before any scoring. When the rare trigrams are themselves common, the full sort of 10k+ candidates adds more.
- Suggested fix: sort trigrams by `posting_count` (lookup table only, no I/O) first. Read only the rarest k lists for discovery. For the remaining trigrams, test membership of the (capped) candidate set with a streaming merge over the posting bytes that stops early, or skip trigrams whose df/N exceeds a threshold (their IDF is near zero anyway). Use top-k selection instead of a full sort. Remove the duplicate `superseded.contains` check in `postings_for_trigram`.
- Confidence: Medium-High.

##### [HIGH] Fallback grep walks the whole tree before searching a single file, so early termination cannot cut the walk
- Location: grep_executor.rs:1154-1204 (`for_each_bounded_fallback_walk_file_with_limits`), :1398-1521 (`fallback_grep_with_limits_counted`)
- Hot path / frequency: every grep while the index is building, evicted, disabled or unavailable; every grep on an external, ignored or symlinked target; parent-session child roots without an index. This is the cold-start experience on large repos.
- Problem: the walk runs inside `bounded_io::run` and pushes every eligible path into a `Vec` (`&mut |path| files.push(path.clone())`). Only after the walk returns does it call `on_file` for each path, which batches 256 at a time into the rayon search. So the comment's "flush_batch" streaming never overlaps with walking. `stop_scan` (set once `max_results * 2` matches exist) cannot stop the walk, which always runs to `MAX_FALLBACK_WALK_FILES` (50k) or the 10 s budget. The walk and the search also share the same wall-clock budget sequentially (`scan_deadline` is set before the walk starts), so a slow walk leaves the search little time.
- Impact: a grep for a common identifier in a 50k+ file tree finds its first 100 matches in the first few hundred files but still pays the full walk (seconds on cold caches) before reading any file, and may hit the 10 s budget with `walk_truncated`. Latency is O(tree), not O(results).
- Suggested fix: stream paths from the helper walk thread through a bounded channel (`crossbeam_channel::bounded(1024)`) to the search side. Search batches while the walk continues, and have the walker poll `stop_scan` and exit early. Alternatively run the walk on the calling thread (it already runs under `HELPER_DEADLINE` semantics) and keep the batch flush inline as originally intended.
- Confidence: High.

##### [MEDIUM] Per-file path normalization allocations in scope/filter checks, plus an extra full scan after every indexed grep
- Location: search_index.rs:7560-7563 (`is_within_search_root`), :7338-7367 (`PathFilters::matches`), :743-762 (`has_file_in_scope`, `has_file_in_scope_with_filters`), :3108-3118 (candidate filter in `search_grep_profiled_with_filters_and_query_and_limits`); grep_executor.rs:518 and :539-541
- Hot path / frequency: every indexed grep (per candidate file), every glob (per indexed file), every `collect_grep_matches_by_file`.
- Problem: `is_within_search_root` calls `inspect::job::normalize_path` on both arguments every call, rebuilding two `PathBuf`s component by component; the root is identical across the whole loop. `PathFilters::matches` normalizes both again, then `strip_prefix` plus `to_path_buf` plus `to_string_lossy().replace` (another `String`). After the query finishes, `execute_root_profiled` also calls `has_file_in_scope_with_filters` (grep_executor.rs:539) on every grep just to fill a diagnostic. With include filters that match nothing in scope (zero-result greps, which are common), that is a full O(N) pass over the index with these allocations.
- Impact: ~6-8 allocations per file x 100k files is about 20-100 ms extra on fully-degraded or broad greps, and the same again for the trailing scope probe on empty results.
- Suggested fix: normalize the search root once per query and pass it in. Indexed paths are stored canonical, so compare with `Path::starts_with` directly (or byte prefix on `as_os_str`). In `PathFilters::matches`, skip `normalize_path` when the path is already under the root and match the globset against `path.strip_prefix(root)` directly (globset accepts `Path`; the `\\` replace is only needed on Windows). Compute `indexed_scope_has_files` lazily, only when the result is empty (commands/grep.rs:151 only consults it in that case).
- Confidence: High.

##### [MEDIUM] Watcher updates deep-clone the whole index tables whenever a query snapshot is alive
- Location: search_index.rs:1811-1847 (`index_prepared_new_file`), :1864-1925 (`remove_resolved_file`), :2761-2785 (`allocate_file_id_with_metadata`); caller runtime_drain.rs:2813-2845 (per-path write lock + `apply_search_update`)
- Hot path / frequency: every watcher-driven file update (every agent edit) that coincides with an in-flight grep, glob or `aft_search`. Indexed grep scans hold their snapshot for up to `INDEXED_GREP_SCAN_BUDGET` (10 s).
- Problem: `SearchIndexSnapshot` shares `files`, `path_to_id`, `delta`, `file_trigram_count` and `unindexed_files` through `Arc`. The first `Arc::make_mut` after a snapshot is taken clones each structure in full: `Vec<FileEntry>` (100k `PathBuf`s), `HashMap<PathBuf,u32>` (100k more), and `DeltaState` (all delta postings, up to `DELTA_COMPACT_HARD_BYTES` = 128 MB, plus `superseded_paths`). This happens on every update after each new query, because every grep or glob takes a fresh snapshot.
- Impact: tens of ms of allocation/copy per edit under concurrent search, plus a transient doubling of resident index memory (up to hundreds of MB with a large delta). This can run while holding the index write lock, which blocks `try_read_with_budget` readers.
- Suggested fix: use persistent or chunked structures, e.g. `im::HashMap`/`im::Vector`, or split `files` into fixed-size `Arc<[FileEntry]>` chunks and `delta.postings` into per-trigram `Arc<Vec<Posting>>` so that copy-on-write clones only the touched chunk or list. At minimum, batch a whole watcher slice under one write lock so the clone happens once per batch rather than once per path.
- Confidence: Medium (the clone is certain; how often it triggers depends on query/edit overlap).

##### [MEDIUM] Warm index load walks the project tree two or three times, stats every file twice, and hashes the whole corpus on the first configure of each process
- Location: search_index.rs:2281 (`read_from_disk_with_policy` -> `ignore_rules_fingerprint`), :7181-7277 (`ignore_rules_fingerprint` + `collect_ignore_rule_files` full walk + `git rev-parse` subprocess in `git_info_exclude_path`), :2306-2316 (`cached_path_under_root_with_memo` does `symlink_metadata` per record), :2716 (fingerprint walk again in `rebuild_or_refresh_with_strategy`), :7470-7480 (`verify_file_mtimes` full `walk_project_files`); cache_freshness.rs:131-152 (`warm_verify_plan` memo is process-static, so a new process gets `Strict`), :483-506 (strict hashes every file whose stat matches); commands/configure.rs:4897-4930
- Hot path / frequency: every daemon start or bind (each new session spawns `aft`), every configure reload, every HEAD change.
- Problem: the same-HEAD warm path does (1) a full ignore-file discovery walk (with `.gitignore` parsing) inside `read_from_disk`, (2) an `lstat` per cached file while decoding records, and (3) a second full `walk_project_files` plus an `fs::metadata` per file in `verify_file_mtimes`. The HEAD-changed path adds (4) another fingerprint walk in `rebuild_or_refresh_with_strategy`. Because the verify memo lives only in process memory, the first configure in every new process is `Strict`: it reads and blake3-hashes every indexed file even when size and mtime match. The cold build also walks twice (fingerprint walk at :4313 plus `walk_project_files`) and spawns `git rev-parse HEAD` twice (:4410 and :4441).
- Impact: on a 100k-file / ~1 GB repo, a "warm" start costs 2-3 tree walks (~0.5-2 s each on cold dentry cache), ~200k stats, and reading ~1 GB to hash it. That is close to the cost of a cold trigram build minus trigram extraction, and it repeats per process.
- Suggested fix: compute the ignore fingerprint from the nested-ignore list the main walk discovers (one walk: collect `.gitignore`/`.aftignore` paths during `walk_project_files`), and cache the fingerprint per root across the load and refresh calls. Skip the per-record `symlink_metadata` at load, since verify stats every file anyway. Replace "hash every file on a new process" with a ctime/inode check: persist the time of the last completed verify in the cache header and hash only files whose `ctime`/`ino` changed since then. User-space writers cannot preserve ctime, so this keeps the correctness guarantee.
- Confidence: High (mechanics); Medium (magnitude).

##### [MEDIUM] Ignore-rule refresh rewrites the full cache.bin it just wrote, and every cache write re-reads the whole file for its CRC
- Location: runtime_drain.rs:2243-2258 (`build_with_limit_to_cache_dir` then `index.write_to_disk`); search_index.rs:4471-4555 (`write_cache_file_from_sources` + `crc32_file_range` at :5245)
- Hot path / frequency: every ignore-rule change rebuild (an edit to `.gitignore`/`.aftignore`/`info/exclude`), plus every compaction or persist.
- Problem: `build_with_limit_to_cache_dir` -> `build_streaming_index` already writes `cache.bin`. The refresh thread then calls `write_to_disk` on the fresh index, which has no delta (`has_pending_disk_changes()` is false). That repeats `CacheWritePlan::from_index` (clones every `FileEntry`/path), re-reads every base posting through `BaseRecordSource` (a pread per trigram), rewrites the whole file, and renames it. Separately, every write computes the CRC by reading back the whole postings section from disk (`crc32_file_range`) instead of hashing while writing.
- Impact: on a large repo (cache.bin of hundreds of MB), each ignore refresh does about 2x the necessary write I/O plus one full extra read. Compactions pay the extra read as well.
- Suggested fix: guard the call with `index.has_pending_disk_changes()`, or drop it because the cold build already persisted. Wrap the `BufWriter` in a CRC-computing writer (`crc32fast::Hasher` updated in `write_all`) for the postings section so no read-back is needed.
- Confidence: High.

##### [MEDIUM] Stale and new files are re-indexed serially after branch switches, warm verify and worktree reconcile
- Location: search_index.rs:7525-7541 (`verify_file_mtimes`: `update_file_with_canonicalization_memo` in a loop), :2630-2651 (`reconcile_borrowed_snapshot_with_disk`)
- Hot path / frequency: daemon start after a branch switch, `git checkout`/`stash pop`, opening a linked worktree on a different branch.
- Problem: verification of size, mtime and hash runs in a bounded parallel pool, but each stale or added file is then read, hashed and trigram-extracted (`prepare_search_path`) one at a time on a single thread, followed by `canonicalize_existing_or_deleted_path_with_memo` (an extra `lstat`). The cold build and `ingest_paths_parallel` already have a parallel prepare stage, and `prepare_search_update` exists for exactly this split.
- Impact: switching branches with 5k changed files means about 5k sequential read+extract operations (~2-10 s) where a parallel prepare would take ~1/N of that.
- Suggested fix: map `stale_paths` plus `new_paths` through `prepare_search_update` in the bounded pool (chunked, as `ingest_paths_parallel` does), then apply the prepared updates serially.
- Confidence: High.

##### [MEDIUM] Removing a delta file scans each of its trigrams' delta posting lists linearly
- Location: search_index.rs:1899-1913 (`remove_resolved_file`: `postings.retain(|p| p.file_id != file_id)`)
- Hot path / frequency: every watcher update of a file already in the delta. Agents re-edit the same file repeatedly, so this is the common case.
- Problem: for each of the file's ~1-5k trigrams, `retain` walks the whole delta posting list of that trigram. With up to `DELTA_COMPACT_SOFT_FILES` (1,000) to `HARD` (5,000) delta files, common trigrams have lists of thousands, giving O(trigrams x delta_files) ≈ 5-25M comparisons per edit, all while holding the write lock. The lists are already sorted by `file_id` (`insert_delta_posting` keeps them sorted).
- Impact: roughly 5-30 ms of write-lock hold per edit late in a delta cycle, blocking readers.
- Suggested fix: `let i = postings.partition_point(|p| p.file_id < file_id); if postings.get(i).is_some_and(|p| p.file_id == file_id) { postings.remove(i); }`. That is O(log n) search plus a memmove.
- Confidence: High.

##### [MEDIUM] Compaction merge pays a HashMap lookup, a HashSet lookup and a heap operation per posting
- Location: search_index.rs:4236-4262 (`BaseRecordSource::load_next_group`), :4561-4622 (`merge_sources_to_writer`), :4081-4128 (`CacheWritePlan::from_index`)
- Hot path / frequency: each delta compaction or persist (`write_to_disk`), and the legacy-repair path at load.
- Problem: for every base posting (often 10^7-10^8 on large repos) the merge does `delta.superseded.contains(&file_id)` (HashSet) and `id_map.get(&file_id)` (`HashMap<u32,u32>`), pushes into a temporary `Vec<SpillRecord>`, then goes through a `BinaryHeap` pop and push with only two sources. It also issues two `write_all` calls per posting. File ids are dense, so both maps can be plain vectors. `from_index` also clones every `FileEntry`/`PathBuf` twice (`files` + `path_to_id`).
- Impact: about 2-5 s of pure CPU per compaction at 10^8 postings, on top of I/O.
- Suggested fix: make `id_map` a `Vec<u32>` (with `u32::MAX` meaning dropped) and `superseded` a bitset. Use a two-way merge when there are two sources. Write each posting group as one 6*n byte buffer.
- Confidence: Medium-High.

##### [MEDIUM] Per-file trigram extraction inserts into a BTreeMap for every byte
- Location: search_index.rs:5463-5478 (`trigram_filter_map`), used by `prepare_search_path` (:3981-3995) and `index_file_with_metadata`
- Hot path / frequency: every file in a cold build, every watcher update, every re-index.
- Problem: `scan_trigrams` visits every byte position and does `BTreeMap::entry(trigram).or_default()`, about log2(distinct) ≈ 12-16 pointer-chasing comparisons per byte. A 1 MB file means 1M BTreeMap entry operations.
- Impact: dominant CPU cost of indexing, roughly 3-5x slower than necessary.
- Suggested fix: accumulate into a `Vec<(u32 trigram, u8 next_mask, u8 loc_mask)>`, `sort_unstable_by_key`, and fold adjacent equal trigrams (or use a 16M-entry-free `HashMap` with `FxHasher` and sort the keys at the end). The output order (ascending trigram) is preserved.
- Confidence: Medium-High.

##### [LOW] Fallback grep builds line tables and lowercase copies for files that never match
- Location: grep_executor.rs:1560 (`let line_starts = line_starts(&content);` runs eagerly), :1660-1667 and search_index.rs:3758-3764 / :3847-3853 (`content.to_ascii_lowercase()` for case-insensitive literals)
- Hot path / frequency: per file in the fallback walk (up to 50k files) and per candidate in indexed case-insensitive literal greps.
- Problem: the fallback path scans every file byte by byte to build `line_starts` before searching, and most files have no match (the indexed path builds it lazily with `get_or_insert_with`). A case-insensitive literal copies the whole file to lowercase before running `memmem`.
- Impact: roughly doubles per-file CPU in the fallback walk; an extra allocation and copy of every candidate's bytes for `-i` literals.
- Suggested fix: build `line_starts` lazily as the indexed path does (or use `memchr::memrchr`/`memchr_iter` from the match offset). For `-i` literals, compile to a `regex::bytes` with `(?i-u)` (literal prefilters are fast) or lowercase only a sliding window.
- Confidence: High.

##### [LOW] Cold build parallelism is capped by 32-file chunks with a barrier per chunk
- Location: search_index.rs:4317-4318 and :4340-4351 (`build_streaming_index`); same in `ingest_paths_parallel` (:1741-1765)
- Hot path / frequency: cold index builds (one-shot per root, but long on big repos).
- Problem: `chunk_size = (pool_size*4).clamp(1, 32)`. Each chunk is a separate `par_iter().collect()` followed by serial insertion, so one large file (up to 1 MB of BTreeMap work, see above) stalls the other workers. With 100k files that is about 3k fork/join barriers.
- Impact: poor core utilization; build wall time likely 1.5-2x of what is achievable.
- Suggested fix: use a bounded channel from a `par_bridge`/pool producer to the single consumer, with memory bounded by channel capacity rather than lock-step chunks, or use larger chunks such as 256-1024 files.
- Confidence: Medium.

##### [LOW] Watcher thread stats the project root on every raw event
- Location: watcher_filter.rs:1525 and :1591-1593 (`project_root_was_deleted` -> `exists()` at the top of every loop iteration)
- Hot path / frequency: once per raw notify event. During a build or `npm install` flood that can be hundreds of thousands of events.
- Problem: the root-deleted check is meant to run every `ROOT_DELETED_CHECK_INTERVAL`, but it runs on every received event, adding one `stat` syscall per event on the single watcher thread. The high-churn infra filter exists precisely to avoid per-event syscalls.
- Impact: 100k+ extra stats per flood on the thread that must keep up with FSEvents/inotify.
- Suggested fix: check at most once per `ROOT_DELETED_CHECK_INTERVAL`, tracking the time of the last check.
- Confidence: High.

##### [LOW] Fallback walker repeats per-directory syscalls the walker already performs
- Location: grep_executor.rs:768-797 (`fallback_target_walk_builder` filter: `DeviceBoundary::should_descend` (stat) + `bounded_io::probe_directory` (opendir+readdir+statfs)) on top of `same_file_system(true)`
- Hot path / frequency: per directory in each fallback walk (grep/glob while the index is unavailable, scope probes, empty-result diagnostics).
- Problem: `same_file_system(true)` already compares device ids. The extra `stat` and the readdir probe roughly double the directory syscalls of the walk.
- Impact: 10-30% slower fallback walks.
- Suggested fix: rely on `same_file_system` for the boundary, and only probe directories when the root itself was not `direct` (non-local filesystem).
- Confidence: Medium (the double device check may be intentional for counting `skipped_foreign_mounts`; the counter could instead be kept by checking only directories where the walker's own check would fail).

##### [LOW] Redundant canonicalizations and lock acquisitions per grep/glob call
- Location: commands/grep.rs:126 + grep_executor.rs:173 and :364 (`project_root(ctx)` canonicalizes three times per grep); grep_executor.rs:596-602 (`is_same_directory` canonicalizes both sides); commands/glob.rs:343-347 (blocking `search_index().read()` without the `try_read_with_budget` grep uses)
- Hot path / frequency: per call.
- Problem: three to five `realpath` calls per request, each going through the `bounded_io` probe machinery. Glob's unbounded read lock can wait behind a long write such as a compaction or the CoW clone above.
- Impact: small CPU cost; latency spikes for glob under write contention.
- Suggested fix: canonicalize once per request and pass it down. Use `try_read_with_budget` in `glob_root` and fall back the same way grep does.
- Confidence: High.

#### Area summary
The trigram index design (disk-backed postings, `pread`, immutable snapshots, a delta with compaction) is sound, and query paths are budgeted. The biggest costs come from per-request overheads that scale with repository size rather than result size. The biggest is `bounded_io`'s per-request, per-directory thread-spawn probes. Glob materializes and fully sorts every match to return 100. The lexical lane reads full posting lists for common trigrams. Fallback grep finishes its whole walk before searching anything. Path normalization allocates per file inside O(N) loops. On the write and maintenance side: copy-on-write clones of whole tables when edits overlap queries, linear delta removal under the write lock, BTreeMap trigram extraction, serial re-indexing after branch switches, redundant full cache rewrites, and warm loads that walk the tree two or three times and hash the whole corpus on every new process. None of these are correctness issues. Most fixes are local: top-k selection, root normalization hoisting, a binary-search removal, a guard on `has_pending_disk_changes`, and a cross-request directory/mount cache.

#### Things checked and found OK
- Snapshot pattern: grep/glob/aft_search take the index RwLock only long enough to clone `Arc`s, then query without the lock (`try_read_with_budget` for grep).
- Posting lookups binary-search a resident `lookup` table and `pread` one contiguous list; `candidates` intersects AND trigrams rarest-first and stops early when the set is empty.
- `intersect_sorted_ids`/`union_sorted_ids` are linear merges; `insert_delta_posting` appends in the common case.
- Indexed grep verification is parallel (rayon), stops at `2*max_results`, checks a deadline per file and per match, reports only the first match per line, and builds line tables lazily; line text is capped at 500 chars, so minified lines do not explode.
- Regexes are compiled once per request (`pattern_compile::compile`); `Regex` clones are cheap; `decompose_grep_pattern` runs once per request and is reused across roots.
- Spill-based cold build bounds memory (128/256 MB SPIMI limits) and uses buffered sequential writes; the load path reads the lookup section in one read and verifies the CRC in memory.
- `ParentCanonicalizationMemo` deduplicates parent `realpath` calls during load and verify.
- `cache_freshness::verify_files_bounded` uses a private bounded pool; StatFirst only hashes size-equal, mtime-changed files.
- Watcher: high-churn infra paths are dropped before `realpath`; `WatcherBatchMetadata::resolve` runs once per flush, not per path; `filter_canonical_paths` avoids a second canonicalize.
- `fuzzy_match` passes are only on the edit-failure path; pass 4 normalizes each line once; reflow pass windows are bounded by needle length.
- `query_shape::minimum_content_token_window` / `contains_all_content_tokens` are per-line with early exit and no per-window joins.
- `search_b2` (router/variants/lane_plan/readiness) does small per-query string work only; `embed_counter` releases per-request observations.
- `effective_path.rs` is startup-only with a cached result; `search_hot_path_measurements.rs` is `#[cfg(test)]`.
- `take_present_on_disk` stats only the page plus a bounded refill (256), not the full result set.

---

### 3.2 Semantic index & semantic search

Files reviewed (all under `crates/aft/src`):
- `semantic_index.rs`, deep-read: build, refresh (stale and invalidated), reuse map, search, persistence (snapshot, segment log, compaction), load, memory estimate, HTTP embedding path, query cache, local query worker. Skimmed: ORT discovery and probing, SSRF, error classification.
- `local_embed.rs` (deep), `synapse_embed.rs` (client/runtime/batch paging), `semantic_admission.rs`, `ort_pin.rs`, `ort_lifecycle.rs` (skim).
- `commands/semantic_search/`: `mod.rs` (query embed, split semantic lane, `file_cosines`, lexical exactness, snippet rendering, recall audit), `external_pattern.rs`, `memo.rs`, `nearest_names.rs`, `confidence.rs`, `rerank/mod.rs`, `rerank/onnx.rs`; other files skimmed for I/O, regex and lock patterns.
- Callers in `commands/configure.rs` (refresh worker, warm load) and `runtime_drain.rs` (refresh apply), read to work out how often each path runs.

Test-only code (`mod tests`, `rerank/tests.rs`, `rerank/fixture.rs`) was ignored.

#### Findings

##### [HIGH] Per-query brute-force cosine scan is scalar, cache-unfriendly and hashes a path per entry
- Location: semantic_index.rs:6666 (`search_filtered`), semantic_index.rs:9794 (`dot_product`), semantic_index.rs:3848 (`EmbeddingEntry`)
- Hot path / frequency: every `aft_search` with a semantic lane (`mod.rs:1331`, `mod.rs:3112`, `mod.rs:5582`, `external_pattern.rs:143`, `views/semantic.rs:1193`). Scans every chunk in the index on each call.
- Problem:
  1. `dot_product` is `a.iter().zip(b).map(|(a,b)| a*b).sum::<f32>()`. Without fast-math, LLVM will not reorder an f32 reduction, so this compiles to one serial chain of dependent adds, roughly 4 cycles per element. At dimension 384 and N chunks that is about 1.5k cycles per entry.
  2. Each entry's vector is its own heap `Vec<f32>` inside an `EmbeddingEntry` that also holds the whole `SemanticChunk` (strings). The scan chases a pointer per entry and has no contiguous matrix to stream through.
  3. Every entry does a SipHash lookup of `(entry.chunk.file, shared)` in `included_paths` (a HashMap rebuilt per query). Once any file is edited on a shared base, it also does a second SipHash in `self.tombstones.contains(&entry.chunk.file)` (the set is no longer empty, so hashbrown's empty-set shortcut stops applying). Chunks of one file are contiguous, so this is about 10–20 hashes per file where one would do.
  4. Every query collects a fresh `Vec<(&EmbeddingEntry, bool)>` of all N entries.
- Impact: with the default `max_files = 20_000` (about 150k–300k chunks), the dot products alone take about 50–150 ms per query, and path hashing adds about 5–15 ms. This latency is on the interactive search path, and the search holds the semantic index read lock for the whole time.
- Suggested fix: (a) Use a vectorizable dot product: 8 or 16 independent accumulators over `chunks_exact(8)`, or the `wide`/`simsimd` crates. Also pre-normalize vectors at insert (store the norm, or store unit vectors) so scoring is a pure dot product. Expect 4–8x. (b) Store vectors in one contiguous `Vec<f32>` per run (`SemanticVectors`) indexed by row, with chunk metadata in a parallel array. (c) Decide the include/tombstone check once per file run: remember the last `&Path` pointer or keep a precomputed run-boundary table, and stop hashing per entry. (d) Iterate directly instead of collecting the `entries` Vec; record (run, row) indices for the top-k. (e) Optional: once N > ~50k, an int8/f16 quantized copy or an HNSW index would bound latency further.
- Confidence: High (code verified). The latency numbers are estimates from N and dim.

##### [HIGH] Local (fastembed/ORT) build pads every batch to its longest row; batches are never length-sorted
- Location: local_embed.rs:286 (`LocalEmbedder::embed`), local_embed.rs:333 (`run_inference`); batching in semantic_index.rs:5544 (`build_from_chunks`) and semantic_index.rs:5418 (`entries_for_chunks_with_reuse`)
- Hot path / frequency: every cold build and every refresh embed with the default local backend. This is the dominant cost of indexing.
- Problem: chunks go into 64-row batches in file order. `run_inference` pads every row to the sub-batch `max_len` (`ids = vec![0; batch * max_len]`). Embed texts range from about 30 to 450 tokens (up to 1600 chars of name, file, signature and body), so a typical batch with a single long row is padded far past its median. Transformer cost is about `batch × L × d²` for the FFN and `batch × L² × d` for attention, both paid on the padded length. The attention-unit splitter (`MAX_BATCH_ATTENTION_UNITS`) only bounds memory and does not reduce padding: it splits in arrival order, so sub-batches stay mixed.
- Impact: usually 2–4x more ONNX compute than needed on code corpora with mixed symbol sizes, which can mean many minutes of CPU on a 200k-chunk cold build. A secondary point: if the cached `tokenizer.json` carries a padding config, `enc.get_ids().len()` is the padded length and the splitter's cost estimate is inflated too (I could not confirm this offline; the code never calls `with_padding(None)`).
- Suggested fix: inside `LocalEmbedder::embed`, after `encode_batch`, sort row indices by token length, greedily partition the sorted list by the attention budget, run inference, and scatter results back to the original order. This is a local change and the vectors are bit-identical. Better still, sort the whole miss list by `embed_text.len()` before batching in `build_from_chunks` / `entries_for_chunks_with_reuse`, and restore order when assembling entries. This needs the file-completion progress accounting adjusted. Also call `tokenizer.with_padding(None)` explicitly.
- Confidence: High for the mechanism. Medium for the size of the speedup.

##### [HIGH] Warm load hashes every indexed file sequentially just to decide whether to wait, then the refresh hashes them all again
- Location: commands/configure.rs:5393-5395 (warm semantic load), which calls `SemanticIndex::is_file_stale` (semantic_index.rs:6779, `cache_freshness::verify_file_strict`)
- Hot path / frequency: every daemon start or bind of a root that has a cached semantic index, unless `warm_verify_plan` returns `Skip`.
- Problem: `catch_up_requested = cached.indexed_file_count() != current_files.len() || current_files.iter().any(|p| cached.is_file_stale(p))`. `is_file_stale` always uses strict verification, which reads and blake3-hashes the whole file even when size and mtime match (`verify_file_inner(.., true)`), and it does so on one thread. When nothing changed (the common restart case), `any` never short-circuits, so every file under the cap (up to 20k) is read once. Then `refresh_stale_files_with_strategy_and_blob_reuse(.., verify_strategy, ..)` re-verifies the whole set, in parallel with Strict hashing or stat-only with StatFirst. The precheck ignores `verify_plan == StatFirst`, the case where the plan already decided hashing was unnecessary.
- Impact: one or two full reads of the source tree per warm start (hundreds of MB on large repos). The first read is single-threaded, which delays the semantic lane becoming ready by seconds on big repos.
- Suggested fix: drop the `any(is_file_stale)` precheck and run the refresh directly. It already returns counts and handles the no-op fast path. If the quiet-window sleep is wanted, decide it from a stat-only pass (`verify_file`, not strict) or from the refresh summary. At minimum, honor `verify_plan` and use `verify_files_bounded` in parallel.
- Confidence: High (code verified). The call is in configure.rs, but the expensive API it calls belongs to this area.

##### [MEDIUM] Each root (and each root's refresh worker) loads its own embedding model; for fastembed that is a separate ONNX session and thread pool
- Location: commands/semantic_search/mod.rs:6767 (`embed_query_for_dimension`, which lazily creates `EmbeddingModel::from_config_for_query` per `AppContext`), commands/configure.rs:1416 (`ensure_ready_semantic_refresh_worker`, another `from_config`), semantic_index.rs:2323 (`from_config_with_timeout_ms`, which creates a new `LocalEmbedder` and query worker thread each time)
- Hot path / frequency: once per root, but resident for the daemon's lifetime. The subc daemon runs one `AppContext` per root actor (`subc/mod.rs:6306`, `subc/standing.rs:449`).
- Problem: `semantic_embedding_model` is a per-`AppContext` `Mutex<Option<EmbeddingModel>>`, and the refresh worker builds another one. With the local backend, each copy is a full `ort::Session` (MiniLM weights about 90 MB plus an arena that grows to the largest inference), an intra-op pool of up to 8 threads, and an `aft-local-query-embed` thread. With Synapse, each copy is a tokio runtime plus a `models.list` discovery round trip. HTTP backends are cheap, since `platform_tls::blocking_client()` is shared.
- Impact: about 2 × roots × (90 MB+ weights, more in arenas, plus up to 9 threads). With several open worktrees or standing roots that is hundreds of MB to over 1 GB of RSS and heavy CPU oversubscription when builds overlap.
- Suggested fix: keep a process-wide model registry keyed by fingerprint-relevant config (backend, model, base_url, caps), holding `Arc<Mutex<..>>` to one session per backend (the local query worker already serializes inference). Hand a shared handle to the query path and the refresh workers instead of calling `from_config` per root.
- Confidence: Medium (each instantiation verified; the exact root count per deployment is unknown).

##### [MEDIUM] `file_cosines` / `Corpus::cosines` do up to 8 full index passes per split query
- Location: commands/semantic_search/mod.rs:1367 (`file_cosines`), commands/semantic_search/external_pattern.rs:164 (`cosines`)
- Hot path / frequency: every split request (`query` and `pattern` together) with selective definitions. `MAX_PLACED_DEFINITIONS = 8`.
- Problem: for each path, `index.search_filtered(vector, 1, |f| f == path)` walks all N entries again. Each pass rebuilds the N-sized `entries` Vec, a new `included_paths` HashMap, and SipHashes every entry's path. On a shared base it also calls `project_root.join(..)` once per distinct file to evaluate the predicate. Only the target file's dot products are skipped.
- Impact: 8 × N hashes plus 8 × F path joins/allocations per split query. At N = 200k and F = 20k that is roughly 10–30 ms of pure overhead.
- Suggested fix: add `SemanticIndex::best_scores_for_files(&[&Path]) -> HashMap<PathBuf, f32>`, which builds a `HashSet` of the targets (relativized for the base) and does one pass, scoring only matching entries. Better, reuse the per-file run boundaries from the first finding so it skips non-target runs entirely.
- Confidence: High.

##### [MEDIUM] Watcher-driven refresh and persist paths do O(total index) work with an allocation per base entry
- Location: semantic_index.rs:5275 (`take_chunk_reuse_map`), semantic_index.rs:7142 (`delta_for_paths`, via `live_entries` at 4872), semantic_index.rs:5107 (`indexed_file_count` → `indexed_paths()` BTreeSet), semantic_index.rs:4986 (`entry_count`), commands/configure.rs:907 (`SemanticWorkerMemory::update` → `estimated_memory`)
- Hot path / frequency: every watcher refresh batch (after the quiet window) in the refresh worker, plus the following persist (`write_to_disk` → `build_segment_frame`).
- Problem: after `fork_for_refresh` the worker index is a shared base plus a small overlay, but several routines still walk the whole base:
  - `take_chunk_reuse_map` iterates every base entry, does `tombstones.contains` (a hash), then `self.project_root.join(&entry.chunk.file)` (a heap allocation) and a `requested.contains` (another hash), all just to find the few edited files.
  - `delta_for_paths` iterates `live_entries()`, which allocates `project_root.join(..)` for every base entry, then does a BTreeSet lookup.
  - `refresh_invalidated_files_inner` calls `indexed_file_count()` twice (lines 6380 and 6416). Each call builds a `BTreeSet<PathBuf>` of every indexed file with a join per file, just to count.
  - After each refresh, `SemanticWorkerMemory::update` calls `estimated_memory()`, which walks entries and also calls `entry_count()` (an O(N) tombstone filter) and `indexed_file_count()` (another BTreeSet build), though only `estimated_bytes` is used.
  - `self.deferred_files.retain(|p| p.exists())` stats every deferred file per batch.
- Impact: per single-file edit, about 3–5 × N path allocations and hashes plus 3 × F BTreeSet inserts. At N = 200k and F = 20k that is roughly 30–100 ms of CPU and allocator churn per watcher batch, against embedding work of a few chunks. It is background work, but it competes with the interactive path and inflates refresh latency.
- Suggested fix: keep `entry_files` in the base (already present) plus a per-file row-range index (relative path → `Range<usize>` per run). Relativize the requested paths once and look them up there, never joining base paths. Make `indexed_file_count` count arithmetically: base `file_mtimes.len()` minus tombstoned members plus local-only keys. Cache `entry_count` (base count minus tombstoned rows, kept up to date in `hide_base_files`). Add an `estimated_bytes()` method that skips the counts.
- Confidence: High.

##### [MEDIUM] HTTP (OpenAI-compatible/Ollama) builds send one embedding request at a time
- Location: semantic_index.rs:2637 (`embed_build_http_adaptive`), with the callers in `build_from_chunks` / `entries_for_chunks_with_reuse` processing batches strictly sequentially
- Hot path / frequency: cold builds and large refreshes on remote backends.
- Problem: the adaptive AIMD loop keeps exactly one request in flight. With remote APIs, round-trip latency (200–800 ms) dominates, not server compute. A 200k-chunk build at 64 rows per request is about 3k sequential round trips.
- Impact: builds take many minutes longer than needed against hosted endpoints. Throughput is capped at batch_size / RTT.
- Suggested fix: allow a small bounded concurrency (2–4 in-flight batches, configurable, with AIMD also on concurrency). Use the shared blocking client from a small scoped thread pool, or switch this path to the async client. Keep the per-row overflow bisection per batch, and reassemble results in order.
- Confidence: Medium (it is a design choice; it trades off rate limits and local Ollama single-slot servers).

##### [MEDIUM] Full `embed_text` stays resident for every entry of a non-view index
- Location: semantic_index.rs:3826 (`SemanticChunk::embed_text`), used only by `build_chunk_reuse_map` / `take_chunk_reuse_map` / `reusable_vector_for_chunk` (hash and compare) and by serialization
- Hot path / frequency: resident memory for the daemon's lifetime, per root (and duplicated across the live index and worker overlays).
- Problem: each entry keeps its embed text (up to 1600 chars by default and more for remote backends with `max_input_tokens`) plus `snippet`. The view path already strips it (`SemanticVectors::strip_embed_text`, `decode_view_payload` discards it), which shows it is not needed for search. Refresh reuse only needs `blake3(embed_text)`, and even then it re-hashes the text on every refresh.
- Impact: roughly 300–1500 bytes per entry, which is 20–100% on top of the 1.5 KB vector. At 200k entries that is about 60–300 MB per root.
- Suggested fix: keep `embed_text_hash: blake3::Hash` (32 B) in memory and compare by hash. That accepts the theoretical collision; or confirm against the on-disk text only on a hash hit if exact equality must be kept. Persist the hash (format bump), or derive it at load and drop the string. Writing a snapshot then needs the text from disk or a hash-keyed format.
- Confidence: Medium (needs a persistence format change).

##### [LOW] Build and refresh clone each chunk's strings several times and keep all chunks alive alongside entries
- Location: semantic_index.rs:5544-5551 and 5554 (`build_from_chunks`), semantic_index.rs:5420-5428 (`entries_for_chunks_with_reuse`), semantic_index.rs:3908 (`execute_build_embedding_batch`: `requested_texts = texts.clone()`), semantic_index.rs:2557 (`embed_texts(texts.clone(), ..)`)
- Hot path / frequency: per embedded row during builds and refreshes.
- Problem: each row's `embed_text` is cloned into `batch_texts`, cloned again in `execute_build_embedding_batch`, cloned again for HTTP, and then the whole `SemanticChunk` is cloned (`chunks[batch_start + i].clone()`, `misses[..].clone()`) only to overwrite `embed_text`. The `chunks` Vec is held until the build finishes, so peak memory is about chunks plus entries (2x text).
- Impact: CPU is minor next to inference. Peak RSS during a cold build is higher by the size of all chunk text (tens to hundreds of MB at 200k chunks).
- Suggested fix: consume `chunks` with `into_iter()` and per-batch `drain`. Move `embed_text` out with `mem::take` and get it back from the row metadata. Pass `&[String]` instead of cloning into `requested_texts`.
- Confidence: High.

##### [LOW] `run_inference` copies the full `last_hidden_state` tensor and clones the attention mask
- Location: local_embed.rs:381 (`attention_mask.clone()`), local_embed.rs:405 (`d.to_vec()`)
- Hot path / frequency: per ONNX sub-batch during builds.
- Problem: `try_extract_tensor::<f32>()` returns a borrowed slice, but the code `to_vec()`s it: batch × seq × 384 × 4 bytes, up to about 50 MB per inference at 64 × 512. The mean pool then reads `mask_at(&attention_mask, ..)` through ndarray indexing in the inner loop.
- Impact: an extra large memcpy and allocation per inference (milliseconds against inference time measured in seconds). It also raises peak RSS slightly.
- Suggested fix: mean-pool directly from the borrowed slice (convert only on the f16 path). Keep the raw `Vec<i64>` mask for pooling and hand ORT a moved array.
- Confidence: High.

##### [LOW] Per-query bookkeeping: `index.len() > 0` is O(N) and the semantic config is cloned
- Location: commands/semantic_search/mod.rs:6716 (`embed_query` → `index.len()` → `entry_count()`), commands/semantic_search/mod.rs:6775 (`ctx.config().semantic.clone()`)
- Hot path / frequency: every semantic query.
- Problem: `entry_count()` walks every base entry with a tombstone hash when tombstones are present, just to test non-emptiness.
- Impact: about 1–5 ms at 200k entries after edits.
- Suggested fix: add an `is_empty()` that checks `!entries.is_empty() || base.entry_count > tombstoned_rows`, or `live_entries().next().is_some()`, or a cached count.
- Confidence: High.

##### [LOW] Lexical-lane exactness reads up to 50 whole files serially per query, with three full-text copies each
- Location: commands/semantic_search/mod.rs:3325 (`lexical_candidate_exactness`), called from mod.rs:4312; related re-reads in mod.rs:4009 (`matching_line_from_source`) and rerank/mod.rs:568 (`candidate_text`, which reads the file and then calls `matching_line_from_source`, which reads it again)
- Hot path / frequency: per NL query with a lexical lane (`LEXICAL_ENUMERATION_LIMIT = 50`). Rerank adds up to 20 more files on page one.
- Problem: sequential `fs::read`, then `from_utf8_lossy`, then `normalize_exact_phrase` (split, collect `Vec<&str>`, join, lowercase: three copies). The same files are read again for snippet rendering and rerank text. `matching_line_from_source` also allocates a compacted `String` per line and does an O(token_len × line_len) trigram `windows().any()` scan per token per line.
- Impact: a few ms to tens of ms on large files or slow disks, serial on the request thread.
- Suggested fix: run the 50 checks with rayon, normalize in a single pass into one buffer, and cache file bytes per request (a small `HashMap<PathBuf, Arc<str>>`) to share between exactness, snippet and rerank.
- Confidence: Medium.

##### [LOW] Load decodes the entire artifact before checking the fingerprint, and replays segments with O(N) work each
- Location: semantic_index.rs:7915-7927 (`read_from_disk`: fingerprint compared after `load_artifact_path`), semantic_index.rs:7749 (`apply_segment_log`: `remove_indexed_files` retain over all entries plus `any_missing_sizes` recomputed over all `file_mtimes`, per segment)
- Hot path / frequency: daemon start, and after a backend or model change.
- Problem: on a model change the whole snapshot (hundreds of MB of vectors) is decoded and then thrown away, even though the fingerprint sits in the header. Segment replay costs O(segments × (N + F)), with segments up to 64.
- Impact: up to seconds of wasted startup I/O and CPU in those cases.
- Suggested fix: peek the header fingerprint (as `scan_artifact_for_append` already does) before decoding the body. During replay, collect all tombstones and apply one retain at the end, and compute `any_missing_sizes` once.
- Confidence: High.

#### Area summary
The semantic stack is generally careful. It shares one HTTP client, has an LRU-ish query-embedding cache, persists through an append-only checksummed segment log with background compaction, reuses vectors by embed-text hash and content-addressed blobs, caps ORT threads, and bounds inference memory. The main performance gaps are in compute rather than I/O:
- the per-query brute-force scan is scalar, pointer-chasing and hashes a path per entry;
- the local embedder wastes most of its FLOPs on padding because batches are not length-sorted;
- the warm-start freshness precheck re-reads the whole tree on one thread before the refresh reads it again.

Below those, the sharing/overlay design is undermined by helper functions (`indexed_file_count`, `entry_count`, `live_entries`, `take_chunk_reuse_map`) that silently walk the entire shared base with an allocation per entry on every watcher refresh. Model instances are duplicated per root and per worker, which is the biggest avoidable RSS cost for multi-root daemons using the local backend.

#### Things checked and found OK
- The HTTP client is a process-wide `OnceLock` (`platform_tls::blocking_client`) reused by every OpenAI/Ollama model. Request bodies are typed serde, with no `Value` round trips on the hot path.
- The query-embedding cache is bounded (1000, FIFO) and keyed by the exact text sent. The local query worker rejects concurrent queries instead of queueing them.
- Top-k selection uses `select_nth_unstable_by` and then sorts only k elements (no full sort).
- Persistence: snapshots go through a `BufWriter`, with an fsync only on full snapshots. Deltas are append-only checksummed frames. `scan_artifact_for_append` seeks over frames and hashes only the last one. Compaction runs on a background thread under a lock and triggers at 64 segments or 25% bytes.
- Load streams vectors through `BufReader` / `CountingReader` (one vector at a time, no whole-file buffer).
- Refresh reuses unchanged chunks by `blake3(embed_text)`, so only edited symbols are re-embedded. Chunk collection is parallel (rayon) for more than 2 files.
- `fork_for_refresh` converts the live index into a shared `Arc` base, so the worker does not deep-copy the vectors.
- The rerank ONNX model loads once per model (process-wide `BACKENDS` map), uses static shapes and length buckets, and has a bounded memo (4096).
- The exact-lane memo is bounded (128, cleared when full). `nearest_names` is bounded by file and occurrence limits.
- `semantic_admission`: a cheap registry lookup, evaluated once per worker batch. `ort_pin` and `ort_lifecycle`: one-shot or startup work, with a cheap gate mutex per inference.
- Synapse batch polling sleeps 100 ms only when there is no progress, and pages follow server-recommended row and token budgets.

---

### 3.3 Callgraph

Files reviewed (all under `crates/aft/src`):
- `callgraph_store/mod.rs`: all non-test code (lines 1–~20550). I deep-read the refresh worker, `refresh_files_profiled_with_workspace_crate_prefix_cache`, the cold build (`cold_build_chunked_from_staged_inventory_with_resolution_memo`), the resolver indexes (`ProjectIndex`, `LazyDbFileIndexes`, `DiskProjectIndex`, `CrateScanMemo`), Rust/JS resolution, the method-dispatch pass, dependency selection (`ref_ids_depending_on`, `reexport_consumer_refs`, `importers_of_created_files`, `dependency_change`), the schema and indexes, open/reconcile, and the query helpers. I skimmed the receiver-type inference helpers, migration/GC/sweep code and the generation publish code.
- `callgraph_store/dead_code_projection.rs` (non-test), `join.rs` (non-test: `join_manifest_with_surfaces`, `JoinResult`), `dispatch.rs`, `facts.rs`, `disk_facts.rs`, `extraction_index.rs`, `extraction_work.rs`, `resolution_config.rs` (all skimmed).
- `callgraph.rs` (extraction and attribution paths, memo types), `calls.rs` (call extraction walkers), `callgraph_maintenance.rs`.
- `commands/callgraph_store_adapter.rs` (callers/impact/call_tree/trace_to/trace_to_symbol/trace_data), `commands/callgraph_borrowed.rs`, plus the `commands/callers.rs` and `commands/impact.rs` default depths.
- Not reviewed: test modules (`*_tests.rs`, `perf_tests.rs`, `#[cfg(test)] mod`); test-only performance does not matter here.

#### Findings

##### [HIGH] Rust qualified-call resolution scans every Rust file for each `X::y()` call
- Location: `callgraph_store/mod.rs:4409` (`DiskProjectIndex::inline_scoped_target`, cold build); `:4019` (`ProjectIndex::inline_scoped_target_by_scan`, incremental refresh); `:3961` (`module_parent_by_scan`); `:14552` (`LazyDbFileIndexes::stored_paths`); callers are `rust_target_for_qualified` (`:13497`) and `rust_caller_module_segments` (`:13964`).
- Hot path / frequency:
  - Every Rust `call`/`value_ref` whose `full_ref` contains `::` goes through `rust_target_for_qualified`. For each module-path candidate (usually 2: child and sibling reading) it calls `inline_scoped_target`.
  - This repo alone has about 40k `Type::fn(` call sites and 896 `.rs` files.
  - Runs once per ref in a cold build, and for every ref that a refresh re-resolves (own file plus dependents) on every save.
- Problem:
  - Cold build: `check(path.clone())` runs for every Rust file in the store on every lookup that misses. Every external `Vec::new`, `String::from` or `Foo::bar` with no local match is a miss. Each check allocates the path clone, `rust_module_segments_for_rel` (a `Vec<String>`) and `rust_src_prefix` (a `String`), and computes the segments before the cheap prefix comparison.
  - Incremental refresh (`lazy.is_some()` disables `CrateScanMemo`), per lookup:
    - `scan_paths(rust_files)` clones the whole stored Rust path list (`stored_paths` returns `paths.clone()`).
    - It then `sort()`s the list and recomputes prefix and segments per file.
    - `module_parent_by_scan` does the same over every module-declaring file. It runs once per ancestor module level, for every qualified ref, through `rust_registered_module_segments`.
- Impact:
  - O(qualified_refs × rust_files) with 2–4 heap allocations per pair. On this repo's own corpus that is about 40k × 2 × 896 ≈ 70M iterations in a cold build, plausibly several seconds of single-threaded CPU inside the resolution transaction. It is far worse on large Rust monorepos.
  - Per save, a ~3k-call file such as `callgraph_store/mod.rs` costs roughly 6k × (clone, sort and scan of 896 paths) on the refresh worker. That is hundreds of ms before dependents are counted.
- Suggested fix:
  - Build a per-pass map once: `(src_prefix, module_segments) -> file`. Only path data is needed, so it also works for the lazy index from `SELECT path FROM files WHERE lang='rust'`.
  - Then `inline_scoped_target` probes only the ≤ `module_segments.len()` prefixes of `module_segments` (O(depth) hash lookups). Keep the existing "caller file first, then path order" tie-break by sorting those few hits.
  - Memoize `module_parent` per target for the batch: a `HashMap` built once from `module_declaring_files`, as `DiskProjectIndex::module_parents` already does.
  - At minimum, compare `rust_src_prefix` before computing segments, and return `Rc<Vec<String>>` instead of cloning from `stored_paths`.
- Confidence: High (code paths verified; the cost estimate is extrapolated).

##### [HIGH] A line shift in a widely-called file re-parses and re-resolves every caller of that file
- Location:
  - `callgraph_store/mod.rs:12287` (`node_id`: the hash includes start/end line and column)
  - `:13139` (`surface_fingerprint` includes node ranges)
  - `:18293` (`ref_ids_depending_on`, branch "Still bound to a node that exists unchanged")
  - `:6257` (dependent parse in `refresh_files_profiled_*`)
- Hot path / frequency: every save that inserts or removes lines above any symbol in a file that other files call. The refresh worker runs this per watcher batch.
- Problem:
  - Node ids are position hashes, so adding one line at the top of `utils.ts` gives every node below it a new id.
  - `dependency_change` correctly finds no name changes (`names` is empty). But `ref_ids_depending_on` keeps every `call`/`value_ref` whose `target_file` is the changed file and whose stored `target_node` is not in the new id set. That is all refs into the shifted nodes.
  - Their caller files become `touched_callers`. Each is re-read and re-parsed in full (`pool.parse_files(&dependent_paths)`), then re-resolved through `upsert_resolved_ref_delta`.
  - `dispatch_callers_to_recompute` then also recomputes dispatch edges for every caller holding a name_match/type_match edge into the file. For Java/C++/Rust receivers that means more re-reads and re-parses (see the next finding).
  - The only thing that actually changed for those callers is the target node id string.
- Impact:
  - A whitespace or comment edit near the top of a hub file with N calling files costs about N full parses plus resolution per save. N = 500–2000 is realistic for shared utility modules, so seconds of CPU per keystroke-save.
  - The worker holds `writer_serial` and the writer `conn` mutex for most of this time, which delays every queued refresh for that root.
- Suggested fix: for position-only changes (`dependency_change` → `Some` with empty `names`), skip the dependent re-parse:
  - build an old→new id map for the changed file (matched by `(scoped_name, kind, range_ordinal)` in order);
  - apply it with set-based `UPDATE refs SET target_node=? WHERE target_node=?` and `UPDATE edges ...`.
  - Alternatively, make node ids position-independent (file + scoped_name + same-name ordinal) and keep the range only as a column.
- Confidence: High for the mechanism (traced through `dependency_change` and `ref_ids_depending_on`); Medium on typical N.

##### [HIGH] The method-dispatch pass re-reads and re-parses caller files for receiver inference even when no candidate method exists
- Location: `callgraph_store/mod.rs:15570` (`insert_method_dispatch_edges_cached`; the inference call is at `:15597`); `parse_dispatch_source` (`:16227`); `infer_java_like_receiver_type` / `infer_cpp_receiver_type` / `infer_rust_direct_self_field_receiver_type`.
- Hot path / frequency:
  - Cold build: `insert_method_dispatch_edges_chunked` (`:15657`) runs over every caller file, single-threaded, inside the final publish transaction.
  - Refresh: `refresh_method_dispatch_edges` runs for every dispatch caller on every refresh.
- Problem:
  - For each unresolved receiver call, candidates are loaded and then `infer_receiver_type_state` runs regardless. For Java/Kotlin/C++ that happens for any bare-identifier receiver (`list.add(x)`, `foo.bar()`); for Rust, for `self.field.m()`.
  - Inference does `std::fs::read_to_string` plus a fresh tree-sitter `Parser::new()` and full parse of the caller file. That is cached only within a 256-file chunk (100 in refresh).
  - With an empty candidate list, `select_type_match_candidate`, `select_rust_direct_self_field_candidate` and `select_name_match_candidate` all return `None`, so the parse is wasted. This is the common case for stdlib/library receivers (`String.length()`, `std::vector::push_back`).
- Impact:
  - In Java, Kotlin and C++ corpora the cold build parses nearly every file a second time, serially, after the parallel extraction. That roughly doubles total parse CPU and adds wall time proportional to corpus size.
  - Refresh pays one extra read and parse per dispatch caller.
- Suggested fix:
  - Add `if candidates.iter().all(|c| c.node_id == reference.caller_node) { continue; }` before `infer_receiver_type_state`.
  - Also consider reusing the trees from extraction, or doing receiver inference during extraction (it is per-file, so it can run on the parallel pool) and storing the inferred receiver type in `refs`.
- Confidence: High.

##### [HIGH] `impact`/`callers` fetch the whole transitive reverse closure with no work budget, plus N+1 boundary count queries
- Location: `commands/callgraph_store_adapter.rs:2630` (`prefetch_callers`), `:2662` (`collect_callers_recursive`, with the per-node `direct_caller_counts_of` at `:2723`), `impact_result` (`:853`), `callers_result` (`:528`). Default depths: `commands/impact.rs` = 5, `commands/callers.rs` = 1, both capped at 100.
- Hot path / frequency: every `impact` tool call (and every `callers` call with depth > 1).
- Problem:
  - `prefetch_callers` runs a BFS over all callers up to `max_depth` layers and keeps every `StoreCallSite` (full caller and target node rows, including signatures). Unlike `call_tree` (`CALL_TREE_NODE_BUDGET`) and `trace_to` (`TRACE_TO_EXPANSION_BUDGET`), it has no node or edge budget.
  - `collect_callers_recursive` then walks the closure depth-first, re-expanding a node whenever a shallower path reaches it (up to `max_depth` re-expansions per node).
  - For every node it visits at depth `max_depth-1`, it issues a separate `direct_caller_counts_of` query: dynamically built SQL with `conn.prepare` (not cached), a 4-way join and GROUP BY. That is N+1 over the last layer.
  - The hub summary (`HUB_SUMMARY_LIMIT`) is applied only after all of this work.
- Impact:
  - For a hub symbol (a logger or error helper), depth-5 `impact` can pull most of the repo's call edges into memory and issue hundreds to thousands of SQL round-trips, each taking the reader's single `conn` mutex. Expect seconds of latency and large transient memory on 100k-file repos.
  - Depth 100 is allowed and is bounded only by graph size.
- Suggested fix:
  - Add a site/node budget to `prefetch_callers`, report `truncated`/`depth_limited` when it is hit, and stop early once `HUB_SUMMARY_THRESHOLD` × some factor is exceeded.
  - Compute the boundary counts once: after the last prefetch layer, call `direct_caller_counts_of` on the whole final frontier and pass the map into `collect_callers_recursive` instead of querying per node.
- Confidence: High.

##### [HIGH] `importers_of_created_files` scans every import/reexport/module ref once per created file
- Location: `callgraph_store/mod.rs:18925` (`importers_of_created_files`), `CreatedFilePatterns::query` (`:19052`).
- Hot path / frequency: every refresh that creates files. A normal edit creates 1 file; `git checkout`, codegen, scaffolding or unpacking can create hundreds in one watcher batch.
- Problem:
  - For each created file, the code builds a new SQL string with N `module_path GLOB ?` ORs (prepared with `conn.prepare`, not cached).
  - `WHERE kind IN ('import','reexport','module') AND module_path IS NOT NULL AND (... GLOB ... OR caller_file GLOB '<crate>/*')` cannot use an index beyond `kind`, so each query walks every import row in the store.
  - Matching rows then go through `module_dependencies_with_memo` / `rust_declared_module_target` (filesystem probes; memoized per call).
  - For a Rust file the `caller_file GLOB '<crate_dir>/*'` arm selects every import/module row of the whole crate.
- Impact: O(created_files × import_refs). With about 300k import rows, one created file costs one full scan (~100 ms); a 500-file branch switch costs 500 full scans plus re-resolution, i.e. tens of seconds on the refresh worker.
- Suggested fix:
  - Do one pass over the import rows per batch: build all created files' patterns up front, scan `refs` once, and test each row against a pattern set in Rust (or one SQL query with all patterns, chunked).
  - Then run the resolution check only for rows that match some pattern.
  - Use `prepare_cached` for any fixed-shape SQL.
- Confidence: Medium-High (verified code; magnitude depends on corpus).

##### [MEDIUM] `backend_file_state` has no index usable for status lookups; every refresh and every writer open does full-table scans
- Location:
  - schema `callgraph_store/mod.rs:10062` (PK `(backend, workspace_root, file_path, content_hash)`) and `:10684` (`idx_backend_file_state_file(file_path, backend)`)
  - `stale_backend_file_paths` (`:17812`), `backend_file_paths_with_status` (`:17832`), `stored_workspace_roots` (`:10825`)
  - `stale_backend_file_count` (`dead_code_projection.rs:605`), `staged_files_with_status` (`:10647`)
- Hot path / frequency:
  - Each refresh batch runs `stale_backend_file_paths` and `backend_file_paths_with_status(undecodable)`.
  - The worker opens a fresh writer store per batch (`process_callgraph_refresh_batch` → `CallGraphStore::open_ready`). That open runs `reconcile_workspace_roots` → `SELECT DISTINCT workspace_root FROM backend_file_state` (full scan plus temp B-tree).
  - It also runs `initialize_schema` (12 `CREATE TABLE IF NOT EXISTS`, 17 `CREATE INDEX IF NOT EXISTS`, and `insert_meta`'s three autocommit `INSERT OR REPLACE` writes, each its own WAL commit), creates a read marker and acquires the writer lease.
  - Each dead-code projection runs `stale_backend_file_count`.
- Problem:
  - The cold build writes one `backend_file_state` row per file. The `status = 'stale'` / `'undecodable'` predicates are only filterable after a range scan over the whole `(backend, workspace_root)` prefix, i.e. every row. `DISTINCT workspace_root` scans everything too.
  - Rebuilding the connection per batch also discards the 96-entry prepared-statement cache, so every batch re-prepares every statement.
- Impact: about 3–4 full scans of an O(files) table plus schema DDL and 3 meta writes per watcher batch. That is roughly 10–50 ms per save at 100k files, before any real work, plus extra WAL churn.
- Suggested fix:
  - Add a partial index `CREATE INDEX ... ON backend_file_state(backend, workspace_root, status) WHERE status <> 'fresh'`, or a plain `(status)` index.
  - Answer the roots check with `SELECT workspace_root FROM backend_file_state GROUP BY workspace_root LIMIT 2` over an index on `workspace_root`, or store the root once in `meta`.
  - Make `insert_meta` write only when values differ.
  - Keep the writer `CallGraphStore` open across batches for a root, revalidating it with `is_current()` and lease checks, instead of reopening per batch.
- Confidence: High (query shapes and schema verified); Medium on the ms figures.

##### [MEDIUM] `ref_ids_depending_on` reads every ref of every importer, then filters in Rust
- Location: `callgraph_store/mod.rs:18293`.
- Hot path / frequency: every surface-changing or deleted file in a refresh, which includes the line shifts above. It also runs for barrels under resolution changes.
- Problem:
  - `SELECT DISTINCT ... FROM refs WHERE caller_file IN (SELECT file_path FROM file_dependencies WHERE dep_file=?1) OR target_file=?1 ORDER BY ref_id` returns all refs (imports, module refs, calls) of every importing file, then sorts them (DISTINCT plus ORDER BY on a non-index key builds a temp B-tree).
  - With `change` set, only `call`/`value_ref` rows survive, and most are dropped by name.
  - `import_names_bound_in` runs one more query per distinct caller file.
- Impact: for a module imported by 2000 files with about 300 refs each, about 600k rows are read and sorted per save of that module. That is hundreds of ms, under the writer lock.
- Suggested fix:
  - When `change` is `Some`, push `kind IN ('call','value_ref')` into SQL, and prefilter by name: `callee_last_segment(short_name)` stored as a column, or `short_name IN (...)` plus `target_file=?1`.
  - Drop `DISTINCT`/`ORDER BY`; dedupe in a set if needed.
  - Split the OR into a UNION ALL of two indexed queries.
- Confidence: Medium-High.

##### [MEDIUM] `dispatch_callers_to_recompute` does a full scan of unresolved calls with GLOB whenever a non-JS file's method set changes
- Location: `callgraph_store/mod.rs:15846` (the `path_form` branch at `:15884`).
- Hot path / frequency: any refresh where a Rust, Java, Python or similar file adds, removes or renames a function or method (`changed_dispatch_candidate_names` is not empty). That is common while coding.
- Problem:
  - `SELECT caller_file, short_name FROM refs WHERE kind='call' AND status='unresolved' AND (short_name GLOB '*::*' OR ...)` walks every unresolved call in the repo (index on `kind` only).
  - It then calls `callee_last_segment` per row.
  - The by-name loop also adds every caller with an unresolved call of a common name (`run`, `apply`, `handle`). Each such caller gets `refresh_method_dispatch_edges`, which means receiver inference re-reads and re-parses (see above).
- Impact: one O(unresolved_calls) scan (often hundreds of thousands of rows) per such save. A common method name can also fan out into hundreds of caller-file re-parses.
- Suggested fix:
  - Store the callee's last segment as an indexed column (`refs.callee_name`) at extraction, so both lookups become `WHERE callee_name IN (...) AND status='unresolved'` on an index.
  - Also skip names that `method_name_match_denylisted` would reject and that have no type-match path.
- Confidence: Medium-High.

##### [MEDIUM] Method-dispatch candidate selection is O(refs × same-name candidates) with per-pair allocations
- Location: `callgraph_store/mod.rs:17549` (`type_candidate_matches`: two `format!` calls per candidate per ref), `:17590` (`select_scored_name_match_candidate`: `split_camel_case(candidate.scoped_name)` per candidate per ref), `:17757` (`compute_path_proximity`).
- Hot path / frequency: cold-build dispatch pass and refresh dispatch recomputes, once per unresolved receiver call.
- Problem:
  - The candidate list per `(method_name, lang)` is cached, but each reference re-filters it with fresh String allocations: `receiver_type.replace('.', "::")`, `format!("{normalized_type}::{method_name}")`, `format!("::{suffix}")`, and camel-case word vectors per candidate.
  - Java and Kotlin getter names (`getName`, `getId`) and Python method names can have hundreds of same-name candidates.
- Impact: 50k refs × 500 candidates gives about 25M iterations with roughly 3 allocations each, i.e. seconds in a serial pass.
- Suggested fix:
  - Precompute per candidate, inside `DispatchCandidateCache`, the normalized `scoped_name` and its camel-case words.
  - Index candidates by enclosing type (`HashMap<type_suffix, Vec<idx>>`) so a type match is a hash probe.
  - Compute `normalized_type` and `suffix` once per reference.
- Confidence: Medium.

##### [MEDIUM] Borrowed-callgraph disclosure recomputes an O(files) diff, and may spawn git, on every callgraph tool call in read-only worktrees
- Location: `commands/callgraph_borrowed.rs:224` (`disclose_borrowed_answer`), `:280` (`borrowed_callgraph`), `:381` (`files_differing_from_borrowed_graph`), `:501` (`collect_differing`), `:564`/`:593` (`locate_edits` / `git_paths`).
- Hot path / frequency: every `callers`, `call_tree`, `trace_to`, `trace_to_symbol` and `trace_data` response when the session is borrowing another checkout's graph (linked worktree, read-only artifacts).
- Problem: each call does all of the following.
  - Clones the search index's full `live_file_identities()` map.
  - Reads every `files` row from SQLite (`file_identities` → hex decode per file), collects and sorts all graph paths, and compares them.
  - Hashes up to 256 files from disk.
  - When HEADs match and anything differs, spawns `git diff --name-only` and `git ls-files --others` (with a 5 ms poll loop and up to 2 s deadline each).
  - `checkout_head` reads git ref files.
- Impact: O(files) CPU and allocation (about 100k map entries) plus 2 subprocess spawns per tool call. That is tens to hundreds of ms of avoidable latency per call in worktree sessions.
- Suggested fix: cache `BorrowedCallgraph` keyed by (store generation plus `projection_write_revision`, search-index overlay generation, owner HEAD, checkout HEAD), and invalidate on watcher events. Cache the `git_paths` results the same way, keyed by `.git/index` mtime.
- Confidence: Medium-High.

##### [MEDIUM] Cold-build resolution clones each ref's dependency set twice and never uses the copies
- Location: `callgraph_store/mod.rs:10592` (`load_staged_ref_window`: `raw.dependencies = dependencies.get(..).cloned()` per ref); `resolve_ref` (`:13189`: `raw.dependencies.clone()` again); `insert_resolved_ref_prepared` (`:15151`, which reads them only in a `debug_assert`).
- Hot path / frequency: every ref in a cold build (millions on large repos). This is the serial resolution loop under the write transaction.
- Problem:
  - Each staged ref receives a full `BTreeSet<String>` copy of its caller file's `file_dependencies`, and `resolve_ref` copies it again into `ResolvedRef.dependencies`.
  - The cold-build insert never writes these; `file_dependencies` were already written during extraction.
  - The whole resolution phase is also single-threaded, while extraction uses the `BuildPool`.
- Impact: O(refs × deps_per_file) String clones, e.g. 1M refs × 20 deps ≈ 40M allocations. Several seconds of cold-build CPU are wasted.
- Suggested fix: share the set as `Rc<BTreeSet<String>>` / `Arc` per caller file, or skip populating `dependencies` in cold-build windows and have `resolve_ref` record only the newly added target. Separately, consider resolving each window's callers on the pool (resolution is read-only against an immutable snapshot) and inserting serially.
- Confidence: High (code); Medium on magnitude.

##### [MEDIUM] Dead-code projection canonicalizes every store path on a full projection, and splicing clones the whole previous snapshot
- Location: `callgraph_store/dead_code_projection.rs:984` (`SnapshotPathResolver::resolve` → `canonicalize_normalized` per distinct path; the resolver is created fresh per projection), `:352` (`splice_files` clones all retained items), `:189` (`project_snapshot`).
- Hot path / frequency:
  - A full projection runs on cold start, on journal gap and on `splice_costlier`.
  - A spliced projection runs after each refresh revision when the inspect/dead-code consumer asks for one.
- Problem:
  - A full projection calls `realpath` on every file path (about 100k syscall chains).
  - Every splice copies every element of `outbound_calls`, `exported_symbols` and `files` (PathBuf and String clones) even when one file changed.
- Impact: hundreds of ms to seconds per full projection at 100k files. A splice is O(total calls) in allocations rather than O(changed).
- Suggested fix:
  - Keep the path resolver cache across projections (keyed by root).
  - Use a persistent structure for the snapshot, such as `Arc<[T]>` per file in a `BTreeMap<PathBuf, Arc<Vec<T>>>`, so a splice replaces only changed files' entries.
- Confidence: Medium. The consumer is in the inspect area, so the frequency there is not verified.

##### [LOW] Qualified `nodes_matching_symbol` forces a full `nodes` scan
- Location: `callgraph_store/mod.rs:8069`.
- Hot path / frequency: `trace_to_symbol_candidates` and `nodes_matching` with `::` queries (Rust `Type::method`), once per tool call.
- Problem: `WHERE n.scoped_name = ?1 OR (f.lang='rust' AND instr(n.scoped_name,' for ')>0 AND substr(...)=?1)`. The second OR arm cannot use an index, so SQLite scans every node joined to `files`.
- Impact: O(nodes), about 1M rows, roughly 100–300 ms per call on large repos.
- Suggested fix: run two queries: the indexed `scoped_name = ?1`, then the trait-impl form via `name = <last segment>` (indexed) filtered in Rust. Or store a `impl_target` column.
- Confidence: High.

##### [LOW] `ref_sites_named` sorts all matches before applying LIMIT
- Location: `callgraph_store/mod.rs:8618`; used by `macro_via_entries` / `unanalyzed_macro_note` on every Rust `callers` call (`commands/callgraph_store_adapter.rs:685`, `:751`).
- Problem: there is only an index on `refs(short_name)`. The `kind` filter and `ORDER BY caller_file, line, ref_id` force a full sort of every ref with that short name (e.g. `new` or `run` can be tens of thousands) to return at most a few dozen rows.
- Suggested fix: add an index `refs(short_name, kind, caller_file, line, ref_id)`; this also covers `unresolved_method_sites_named`.
- Confidence: Medium.

##### [LOW] Store-level traversal APIs are N+1 and unbudgeted, but only used by benches and probes
- Location: `callgraph_store/mod.rs:6811` (`callers_of` → recursive `collect_callers_recursive` with one SQL per node), `:7003` (`trace_to`: one SQL per expansion, no memo), `:7164` (`trace_to_symbol`: one query per node, no budget).
- Hot path / frequency: none of the tool commands. The commands use the batched adapter versions. Callers are `views/lazy_read_benchmark.rs`, `views/dispatch_parity_probe.rs` and tests.
- Problem: if anything routes to these, performance regresses sharply.
- Suggested fix: delegate them to the adapter implementations or mark them test/bench-only.
- Confidence: High.

##### [LOW] `DiskProjectIndex::contains_file` is an unmemoized SQL lookup
- Location: `callgraph_store/mod.rs:4368`.
- Hot path / frequency: cold-build resolution. It is called several times per Rust ref (`rust_file_from_module_declarations`, `rust_file_for_src_prefix` with `.rs` and `/mod.rs` candidates) and from `note_module_ref`.
- Problem: one point query per probe, with no memo, unlike `LazyDbFileIndexes::contains_file`, which caches.
- Suggested fix: add a `RefCell<HashMap<String,bool>>` to `DiskIndexMemos`.
- Confidence: Medium.

##### [LOW] Minor query overheads
- Batched helpers (`direct_callers_for_tuples`, `direct_caller_counts_for_tuples`, `nodes_for_symbol_tuples`, `outgoing_calls_for_symbol_tuples`, `load_name_match_refs`, `stale_backend_file_paths`) use `conn.prepare(&format!(...))` per call.
  - SQL compile is about 50–200 µs each. Padding chunks to fixed sizes and using `prepare_cached` would make the plans reusable.
- `callers_result` runs `SELECT COUNT(*) FROM files` on every call (`scanned_files`). This could come from a cached `meta` value.
- One global `aft-callgraph-refresh` thread serves every root (`RefreshWorker::spawn`). In daemon (subc) mode a slow batch in one project delays all others. Consider a worker per root, or a small pool keyed by root.

#### Area summary
The callgraph's SQLite layer is mostly sound:
- prepared statements are cached;
- writes are batched in transactions;
- secondary indexes are dropped and recreated around the bulk load;
- the adapter's caller fetches are batched per frontier;
- `call_tree` and `trace_to` have work budgets.

The real costs are in resolution and incremental-update scope:
1. Rust qualified-call resolution scans every Rust file for each call, in both cold build and refresh.
2. Position-based node ids turn a line shift in a hub file into a re-parse of all its callers.
3. The dispatch pass re-parses caller files for receiver inference even when no candidate exists, roughly doubling parse work for Java, Kotlin and C++.
4. `impact` (default depth 5) fetches an unbounded transitive closure and then runs N+1 count queries.
5. File creation triggers one full import-ref scan per created file.

Secondary issues: a missing status index on `backend_file_state` combined with reopening the writer store on every watcher batch, broad dependency-selection queries, per-call borrowed-graph diffs in worktrees, and wasted per-ref clones in cold-build resolution.

#### Things checked and found OK
- Cold-build bulk load: prepared `ColdBuildInsertStatements`, per-batch transactions, secondary indexes dropped then recreated, `delete_staged_file_rows` avoids scans for fresh staging, and resolution uses upsert `DO UPDATE` rather than REPLACE.
- `prepare_cached` throughout the refresh and query paths, with the statement cache raised to 96.
- WAL with `synchronous=NORMAL`, autocheckpoint, and an idle TRUNCATE checkpoint that is rate-limited (60 s).
- The adapter's `prefetch_callers`, `trace_to` and `trace_to_symbol` batch caller/callee fetches per frontier. `call_tree` memoizes adjacency and target resolution and is budgeted. `impact` reads only the needed source lines (`read_source_lines_at`).
- Refresh releases the `conn` mutex while parsing, and parsing is parallel on `BuildPool`. Readers use separate `ReadonlyCallGraphStore` connections, so tool queries do not block on the writer.
- `attribute_sites_to_symbols` is a linear sweep; `collect_calls_full` is iterative with range pruning; `LineIndex` is used for byte/line mapping.
- `RetainedCallerData` budget avoids re-parsing callers in resolution; the `DiskIndexMemos` file-index LRU is bounded (4096); `ModuleResolutionMemo` is bounded.
- `reconcile_stored_files_with_disk` is bounded by `RECONCILE_MAX_EXAMINED_FILES`, and it hashes only when sizes match.
- `stored_extract_matches` / `stored_node_ids_match_extract` avoid rewriting rows for unchanged extracts.
- `views`/`join.rs` restores cached surfaces and replays recorded surface queries, so unchanged callers are not re-decoded. That path uses `CrateScanMemo`, which is cheaper per file than the lazy path, though the inline lookup there is still a linear scan.

---

### 3.4 Parsing & reading tools

Files reviewed (all under `crates/aft/src`): parser.rs (deep-read: query/parser caching, `FileParser`, `SymbolCache`, freshness, provider; skimmed per-language extractors), commands/outline.rs (deep), commands/zoom.rs (deep on symbol/targets/enrichment paths), commands/read.rs (text/streaming paths; hashline internals belong to another area), commands/symbol_render.rs, commands/ast_scope.rs, commands/ast_replace.rs, symbol_diff.rs, symbol_cache_disk.rs, symbols.rs, extract.rs (skim), imports/ (mod.rs skim + per-language files, quotes.rs), ast_grep_lang.rs, ast_grep_hints.rs, indent.rs, language.rs, url_fetch.rs, github_read/ (cache.rs, fetch/render/bot_compress skim). For the symbol-cache prewarm and persistence path I also read `commands/configure.rs:2327-2440`, because that code drives `parser.rs` and `symbol_cache_disk.rs`.

Overall, the parser core is in good shape. Queries are compiled once (`LazyLock` per language). Parsers are kept per thread (`REUSABLE_PARSERS`). Symbol tables are shared through `Arc`. Export lookup uses a binary search, and the TS/JS name dedupe uses an index. The remaining problems are mostly in the outline command and in serial whole-repo loops.

#### Findings

##### [HIGH] Directory/multi-file outline fully re-parses every file on every call, even when the symbol cache already has it, serially, and reads each file 3-4 times
- Location: commands/outline.rs:2085 (`outline_many_files`), :2501 (`outline_skip_reason`), :718 (`outline_structure_entries`); edit.rs:116 (`validate_syntax_with_parser`)
- Hot path / frequency: every `outline` call with `directory` (up to `OUTLINE_FILE_WALK_CAP` = 200 files) or a `files` array. This is a common first call when an agent explores a repo.
- Problem: For each file, in this order:
  1. `outline_skip_reason` does `path.is_file()` and a `metadata()`, then `validate_syntax_with_parser`, which runs `parser.parse(path)`. That is a full `read_to_string` plus a tree-sitter parse. `batch_parser` is a new `FileParser` each request, so its tree cache is always empty. The parse runs even when `extract_symbols` is a hit in the shared symbol cache.
  2. `extract_symbols`: a stat, a freshness stat, and on a miss another read and a blake3 hash. On a hit it deep-clones the `Vec<Symbol>` (`Arc::unwrap_or_clone`).
  3. `outline_structure_entries`: another `read_to_string` plus `parse_cloned`, which re-stats for freshness.
  4. Rust only: a 4th `read_to_string` plus `parser.parse` (2 more stats) for `add_trait_member_previews`.

  All of this runs serially in a `for` loop. The symbol cache cannot help, because the syntax-validity check needs a full parse every time.
- Impact: 200 files × (a parse of about 1-10 ms + 3-4 reads + about 8 stats) ≈ 0.5-2 s per directory outline, warm or cold. With a warm cache this could be close to stats only.
- Suggested fix:
  - (a) Keep `has_error: bool` in `CachedSymbols` (computed once at extraction) and answer the parse-error skip from the cache.
  - (b) Read the source once per file and pass `&source` to the parse/extract/test-region/trait-preview steps, using `parse_with_source`.
  - (c) Only Rust/TS/Py/Go need a tree, for test-region collapsing. Skip the tree when `collect_test_regions` cannot apply.
  - (d) Run the per-file work with rayon. Each worker gets its own `FileParser` on the shared symbol cache, and results are collected in order.
- Confidence: High

##### [HIGH] Single-file outline reads and parses the file for nothing (Py/Go/TS/JS), and always re-parses Rust even on a symbol-cache hit
- Location: commands/outline.rs:238-249 (`handle_outline_inner` single-file branch), :718-753 (`outline_structure_entries` with `collapse_tests=false`)
- Hot path / frequency: every single-file `outline` call, which is one of the most frequent read tools.
- Problem: The single-file branch calls `outline_structure_entries(..., collapse_tests=false)` with a new `FileParser::new()`. For Rust/Python/Go/TS/TSX/JS this always runs `read_to_string(path)` and then `parser.parse_cloned(path)`, which reads the file a second time and does a full tree-sitter parse.
  - With `collapse_tests=false`, `regions` stays empty. For every language except Rust, the result is `build_outline_tree(&product_symbols)`, so the source and tree are thrown away unused.
  - For Rust the tree is only used for `rust_module_outline`, and it is rebuilt from scratch on every call because nothing caches trees across requests.

  The symbol table is also deep-copied three times: `list_symbols` (`Arc::unwrap_or_clone`), then `product_symbols.push(symbol.clone())`, then `symbol_to_entry`.
- Impact: Each outline of a cached file still pays 2 full reads and 1 full parse: about 1-5 ms for typical files and tens of ms for 5-10k-line files. It should be a cache lookup costing tens of µs.
- Suggested fix:
  - When `!collapse_tests && lang != Rust`, return `build_outline_tree(symbols)` right away.
  - For Rust, keep module ranges (or a flag saying the file has no `mod` items) in the cached symbol entry, so `rust_module_outline` can run without a tree. Failing that, read the file once and use `parse_with_source`.
  - Use `list_symbols_shared` and filter by reference instead of cloning.
- Confidence: High

##### [MEDIUM] Directory outline outlines all discovered files, then the 30 KB render discards most of them
- Location: commands/outline.rs:2846 (`format_multi_file_tree`) together with :103 (`outline_many_files` called on all `files`)
- Hot path / frequency: directory outline calls.
- Problem: `outline_many_files` builds full outlines (parse plus extraction) for every discovered file, up to 200. `format_multi_file_tree` then sorts by path and `break`s at the first file that pushes output past `MAX_OUTPUT_BYTES` (30 KB). For large directories, much of the parse/extract work is never shown.
- Impact: Up to about 200 parses per call where perhaps 30-80 files' output is rendered. This multiplies the HIGH finding above.
- Suggested fix: Sort the file list by display path first. Then outline files in order (or in parallel chunks) and stop once the rendered byte budget is reached. Count the remaining files for the footer without outlining them.
- Confidence: High

##### [MEDIUM] `outline files:true`: serial whole-file line counting over up to 10,000 files, then serial cold parses for rendered rows
- Location: commands/outline.rs:1476 (`append_outline_directory_tree`), :1750 (`outline_file_entry`), :1845 (`inspect_outline_file_content`), :1774 (`populate_rendered_file_symbols`)
- Hot path / frequency: every files-mode outline (directory/directories table).
- Problem:
  - Every discovered file, up to `OUTLINE_FILE_COLLECTION_CAP` = 10,000, is opened and fully read (up to 1 MiB) in a single-threaded loop to count newlines. That includes files that end up in rollup rows and are never shown individually. `relative_path_from_root` may also `canonicalize` twice per file when `strip_prefix` fails.
  - `populate_rendered_file_symbols` then extracts symbols serially for each shown row on a cache miss, calling `list_symbols` and deep-cloning just to take `.len()`.
  - Its `cached_symbol_count` fallback calls `SymbolCache::get`, which deep-clones the whole `Vec<Symbol>` for `.len()`. It does so while holding the cache read lock, and may blake3-hash the file under that lock (`cached_file_is_fresh`).
- Impact: Roughly 10k open+read syscall sequences run serially, which takes seconds on a large monorepo or a cold page cache. Then up to about 500 serial cold parses follow.
- Suggested fix:
  - Do the per-file `inspect_outline_file_content` with `par_iter`, since files are independent.
  - Do the same for symbol counts in `populate_rendered_file_symbols`.
  - Add a `symbol_count_if_fresh` that returns `symbols.len()` without cloning, and do the hash outside the lock.
- Confidence: High

##### [MEDIUM] `plan_outline_file_rows` re-renders the whole table for every candidate expansion
- Location: commands/outline.rs:1663-1747 (`plan_outline_file_rows`), :1971 (`format_files_table`)
- Hot path / frequency: every files-mode outline. The loop runs once per non-data directory node, and directory count can reach thousands for a 10k-file tree.
- Problem: For each candidate directory the code does `rows.clone()`, a `splice`, and a full `format_files_table(&candidate_rows, ...)`, only to compare `.len()` against the budget.
  - `format_files_table` does 3 passes over all rows. It calls `format!` 2-3 times per row and `directory_rollup_summary` twice per rollup row, and allocates the full output string.
  - There is also a `rows.iter().position(...)` linear search per candidate.

  The cost is O(D × R) row formats, where R is the current row count (about 500 at a 30 KB budget, more before rejection).
- Impact: Thousands of directories × about 500 rows × several allocations is about 10^6 small allocations, roughly 100-300 ms on large trees.
- Suggested fix: Track the byte length incrementally. Each row's rendered width depends only on `path_width`/`language_width`/`middle_width`, so keep running maxima plus a per-row "content length" sum. Recompute the total as `rows × padded_width` when a width maximum changes, and never build the string until the final render. At minimum, compute the length without allocating, using `write!` into a counting sink.
- Confidence: Medium (the cost depends on directory count, but the loop is clearly quadratic-ish)

##### [MEDIUM] ast_search / ast_replace scope collection is a serial, unbounded whole-tree walk, and ast_replace parses every file with no literal prefilter
- Location: commands/ast_scope.rs:198-286 (`walk_roots` / `walk_root`); commands/ast_replace.rs:245-283 (phase 1)
- Hot path / frequency: every `ast_search` / `ast_replace` call.
- Problem:
  - `walk_root` uses `WalkBuilder::build()`, the single-threaded walker, and collects every matching file into a `Vec` with no cap. It never uses the trigram index (which already knows the file list) or `build_parallel()`.
  - `scope_warnings` may run a second full unfiltered walk per labelled root when include globs filter everything out.
  - `ast_replace` then reads and fully parses every file in scope with ast-grep (`lang.ast_grep(&original)`), even though most files cannot match. A pattern like `console.log($A)` requires the literal tokens `console` and `log` to appear in the file.
  - Phase 2's `validate_syntax_str` re-parses each changed file serially on the request thread, using a new `tree_sitter::Parser::new()` and `set_language` each time instead of the thread-local parser cache.
- Impact: In a repo of about 100k files, the serial walk takes about 0.5-2 s before any matching. Parsing is parallel, but all-file parsing is the dominant CPU cost per call.
- Suggested fix:
  - Use `WalkBuilder::build_parallel()`, or reuse the search index's file list, for the scope.
  - Pull literal identifier tokens from the pattern (non-`$` leaf text) and skip files with `memchr::memmem` before calling `ast_grep`.
  - Move the `validate_syntax_str` check into the parallel phase 1, and route it through `parse_source_with_cached_parser`.
- Confidence: Medium-High

##### [MEDIUM] Symbol-cache persistence holds the shared `SymbolCache` read lock across full JSON serialization, file I/O and a lock wait of up to 2 s
- Location: commands/configure.rs:2397-2425 (`prewarm_symbol_cache_from_search_files`) → symbol_cache_disk.rs:157 (`write_to_disk`) / :337 (`write_cache_file`)
- Hot path / frequency: once per prewarm (startup, reconfigure, root switch). This is exactly the window in which agents start issuing tool calls.
- Problem: `if let Ok(cache) = symbol_cache.read() { ... write_to_disk(&cache, ...) }` keeps the `RwLock` read guard while `write_to_disk` does the following:
  - `SymbolCacheLock::acquire`, a file lock with a timeout of up to 2 s.
  - `serde_json::to_vec` for every entry's symbols, for all files in the project.
  - Writing everything through `BufWriter`, then a rename.

  Meanwhile any cache-miss `extract_symbols` blocks on `symbol_cache.write()` in `extract_symbols_shared_inner`. With std's writer-preferring futex RwLock, every later `symbol_cache.read()` (every outline/zoom/symbol lookup) queues behind that waiting writer as well.
- Impact: For a 50k-file project, serialization plus the write likely takes about 1-3 s, plus up to 2 s of lock wait. Symbol-based tool calls stall for that whole time at startup.
- Suggested fix: Under the read lock, snapshot `disk_entries()` as `Vec<(PathBuf, mtime, size, hash, Arc<Vec<Symbol>>)>`, which is cheap thanks to the `Arc` bumps. Release the lock, then serialize and write. Also consider a binary encoding (bincode/postcard) instead of a JSON blob per entry; that speeds up both write and the startup decode in `read_cache_file`.
- Confidence: High (the lock scope is clear), Medium (the size of the impact)

##### [MEDIUM] Symbol prewarm is single-threaded and deep-clones every newly extracted symbol table only to drop it
- Location: commands/configure.rs:2327-2391 (`prewarm_symbol_cache_file`, the loop in `prewarm_symbol_cache_from_search_files`); parser.rs:1767-1778 (`extract_symbols_with_cache_status`)
- Hot path / frequency: background, once per prewarm over all indexed source files (a cold start parses everything).
- Problem:
  - The loop walks `symbol_files` one file at a time on one thread, with one `FileParser`. It does a cache read lock, stat, read, hash, parse, extract and a write lock per file. There is no rayon, even though parsers are already thread-local and the cache is `Arc<RwLock>`.
  - `extract_symbols_with_cache_status` returns `Arc::unwrap_or_clone(symbols)`. On a fresh extraction the cache holds the other `Arc` reference, so this always deep-copies the whole `Vec<Symbol>` (names, signatures, scope chains). The prewarm ignores that copy (`result.map(|(_, changed)| changed)`).
- Impact: A cold prewarm of 50k files takes about 1-3 ms each serially, so 1-3 minutes, during which tool calls miss the cache and parse on demand. The useless clone adds about 10-30% allocation overhead per file.
- Suggested fix:
  - Add an `extract_symbols_shared_with_cache_status` that returns the `Arc`, or just the `changed` flag, for prewarm.
  - Parallelize prewarm with `par_chunks` and a per-worker `FileParser::with_symbol_cache_generation`. The disk-load verification loop in `DecodedSymbolCache::read` (parser.rs:1181, a serial stat per entry) can also be parallelized.
- Confidence: High

##### [LOW] Python and C++ symbol extraction run the full language query twice per file
- Location: parser.rs:3015-3060 (`extract_py_symbols`, the first pass collects only `dec.*` captures), parser.rs:4152-4183 (`extract_cpp_symbols`, the first pass collects only type names)
- Hot path / frequency: every Python/C++/CUDA/Metal symbol extraction (prewarm, cache misses, callgraph builds).
- Problem: The first `QueryCursor::matches` pass runs the whole multi-pattern query over the whole tree only to gather decorated rows or type names. The second pass repeats the same matching.
- Impact: About 2x the query-matching cost for these languages, perhaps 20-40% of total extraction time.
- Suggested fix: Use one pass and buffer the needed node data, or use a tiny dedicated query or a direct tree walk for the pre-pass (for Python, `decorated_definition` children).
- Confidence: Medium

##### [LOW] Every symbol lookup stats the file twice; `list_symbols` deep-clones on cache hits
- Location: parser.rs:1786-1847 (`extract_symbols_shared_inner`: `metadata()` for `current_mtime`, then `cached_file_is_fresh` calls `metadata()` again); parser.rs:8851 (`list_symbols` → `extract_symbols` → `Arc::unwrap_or_clone`)
- Hot path / frequency: every outline/zoom/resolve_symbol call; zoom does several (resolve → `build_container_outline` → enrichment).
- Problem:
  - There are 2 stat syscalls per lookup where 1 is enough: pass the metadata into `cached_file_is_fresh`.
  - Callers in this area that only read the symbols still use the owned `list_symbols`: single-file outline, zoom heading fallback, files-mode counts, and `symbol_for_rank0_render`. Each of these copies the entire table.
  - `resolve_symbol_inner` also does `std::fs::canonicalize` per call.
- Impact: Small per call (µs to sub-ms for large tables), but it is paid on every lookup.
- Suggested fix: Thread the `Metadata` through, and use `list_symbols_shared` in these call sites.
- Confidence: High

##### [LOW] Per-line `format!` and temporary allocations in text renderers
- Location: commands/read.rs:1738-1766 (`render_text_read`), :2020-2040 (streaming range); commands/outline.rs:2638-2716 (`render_entries` / `render_top_level_entries`: `format!` wrapping `format_entry_*`, which itself allocates); commands/outline.rs:2931 (`symbol_to_entry` converts each symbol's kind via `serde_json::to_value`); commands/symbol_render.rs:29 (`symbol_kind_string`, same)
- Hot path / frequency: every read (up to 2000 lines / 50 KB) and every outline.
- Problem: Each rendered line allocates 1-2 temporary `String`s before `push_str`. `symbol_to_entry` builds a `serde_json::Value` per symbol just to get a static kind string. `render_text_read` also collects every line of the whole file into a `Vec<&str>`, only for `total_lines`.
- Impact: Small, about 2-4k allocations per call (tens to hundreds of µs).
- Suggested fix: Use `write!(output, "{:>w$}: {}\n", ...)` directly, and add a `SymbolKind::as_str() -> &'static str`.
- Confidence: High (that the code allocates), Low (that the impact matters)

##### [LOW] Zoom batch re-splits the file into lines once per target
- Location: commands/zoom.rs:320 (`zoom_one_target_response`)
- Hot path / frequency: `zoom targets=[...]` with several symbols in the same file.
- Problem: The source is memoized in `memo.files`, but `source.lines().collect()` runs again for every target. `resolve_zoom_file` uses an unbounded `std::fs::read_to_string`, with no size cap, unlike `bounded_io`.
- Suggested fix: Memoize the line-start offsets with the source, and bound the read size.
- Confidence: High (minor)

##### [LOW] `symbol_diff_range` spawns one `git show` process per blob, serially (currently not called anywhere)
- Location: symbol_diff.rs:168-256, :476 (`read_git_blob`)
- Problem: Each modified file costs 2 `git show` process spawns, about 5-10 ms each. A 500-file range would take about 5-10 s. Running `rg` found no callers outside the module, so this does not affect anything today.
- Suggested fix: If the function gets wired up, use one `git cat-file --batch` process (or gix) and parse files in parallel.
- Confidence: High (that the code does this), with no current impact

##### [LOW] Misc one-shot costs
- `url_fetch.rs:505` `build_client` creates a new reqwest `Client` (connection pool, TLS config) for every URL fetch, so there is no keep-alive reuse across fetches. Also, the `<a>` handler (url_fetch.rs:818) calls `walk_children` to test for `§`, and `fallback` then walks the children again: two renders per anchor.
- `imports/quotes.rs:182` compiles a `Regex` per call to `prettier_bool`. `config_setting` re-reads formatter configs up the directory tree on every add-import, with no caching.
- `github_read/cache.rs:74` opens a new SQLite connection (`crate::db::open`) for each cache lookup/upsert.
- `edit.rs:131` `validate_syntax_str` creates a new `tree_sitter::Parser` per call instead of using `parse_source_with_cached_parser`.

#### Area summary
The tree-sitter layer is already well optimized:
- Queries are compiled once.
- Parsers are reused per thread.
- Symbol tables are shared through `Arc`.
- Freshness uses size/mtime first, then a content hash.

Nearly all the remaining cost is in the commands that use it:
- **The outline command.** Directory and multi-file outlines always re-parse every file for a syntax check, read each file 3-4 times, and work serially on files the 30 KB budget then drops. Single-file outlines read and parse the file even when the cached symbols are all that is needed.
- **Serial whole-repo loops.** Files-mode line counting, ast scope walks and symbol prewarm all run one file at a time.
- **Persistence locking.** Symbol-cache persistence serializes the entire cache while holding the shared lock, which stalls symbol lookups at startup.

Fixing the two outline HIGHs means keeping a `has_error` bit (and Rust module ranges) in the cached symbol entry, plus read-once/parse-once plumbing. That would turn warm outlines into stat-only operations.

#### Things checked and found OK
- Query compilation: `LazyLock` per language (parser.rs:860-975), never compiled per call.
- Parser reuse: the `REUSABLE_PARSERS` thread-local, the Rust macro-body parser thread-local, and the imports module's retained parsers.
- `extract_symbols` cold path reads once and hashes once (`parse_with_source`). The symbol cache read lock is not held while hashing.
- TS/JS export detection: `collect_exports` does a single walk, then a binary search in `is_exported`. `VariableNameIndex` avoids O(n²) name checks. `dedup_symbols` uses a borrowed HashSet.
- `build_outline_tree` uses an indexed scope lookup (`OutlineScopeIndex`), so it is not quadratic.
- Zoom: per-file enrichment is memoized across batch symbols; the parse reuses the already-read source; call-attribution scans are bounded.
- ast_scope compiles globs once per glob (not per file) and skips canonicalize for single-root walks. ast_replace phase 1 is parallel and the pattern is precompiled.
- read.rs: explicit ranges stream with bounded buffers; image decode is bounded and has a timeout.
- github_read: regexes are in `LazyLock`; in-flight fetches are coalesced per slot.
- indent.rs, language.rs, symbols.rs, ast_grep_lang.rs, ast_grep_hints.rs: there is no hot-path cost here.
- The disk cache skips payloads that are too large and writes atomically; there is no per-entry fsync.

---

### 3.5 Editing & mutation path

Files reviewed (non-test code, under `crates/aft/src`): `edit.rs`, `format.rs`, `fuzzy_match.rs` (called by edit/batch), `patch/{apply,matcher,parser}.rs`, `backup.rs` (store, disk layout, hydrate, snapshot, restore, DB mirror), `backup/purge.rs` (skim), `checkpoint.rs`, `write_ledger.rs`, `durability.rs::atomic_write` (callee), `hashline/{apply,transaction,snapshot,integration/binding}` (skim plus transaction deep-read), `commands/{write,edit_match,batch,apply_patch,hashline,edit_symbol,move_symbol,move_file,delete_file,delete_tree,lsp_rename,ast_replace,conflicts,undo,edit_history,checkpoint}.rs`. I skimmed `inline_symbol`, `extract_function`, `organize_imports`, `add_import`, `remove_import` and `import_specifier_edit`. I also checked `main.rs::dispatch`, `run_tool_call.rs` (backup-lock touch points) and the OpenCode plugin's `hoisted.ts` call pattern (preview, then apply).

#### Findings

##### [HIGH] `undo` loads and locks the session's whole backup history, content included
- Location: `backup.rs:1210` (`restore_last_operation_preserving`), `backup.rs:3568` (`refresh_disk_index_for_session`), `backup.rs:3518` (`load_from_disk_if_needed_locked`), `backup.rs:3497` (`acquire_stack_disk_locks`)
- Hot path / frequency: every operation-level undo (`aft_safety undo`). How long it takes grows with everything the session has ever touched, not with the size of the undo.
- Problem: `restore_operation_candidate_keys` calls `refresh_disk_index_for_session`, which runs `read_dir` on the session dir and reads and `serde_json`-parses every `meta.json`. On the normal path that happens twice. The function then takes one `fs_lock` (mkdirs plus open plus flock) for every key in the session. After that, `for key in &current_keys { self.load_from_disk_if_needed_locked(..) }` loads every key's full stack, including every `.bak` content blob (`std::fs::read`), and builds a `String::from_utf8_lossy(..).into_owned()` copy of each one (`entry_from_meta`, `backup.rs:5217`). All of this exists only to find the newest `op_id` and its keys, which needs metadata alone. The loaded stacks then stay in `self.entries` (see the memory finding below).
- Impact: take a session that ran one `delete_file` on a 2,000-file tree (allowed by `RECURSIVE_DELETE_BACKUP_MAX_FILES`), or a glob edit over hundreds of files. Every later undo, even of a one-line edit, does about 4k meta reads and parses, 2k lock files, up to 100 MB of blob reads and 2x that in String copies. That means seconds of latency and a large RSS spike. The cost is O(sum of the session's history).
- Suggested fix: keep an op index: `op_id -> keys`, or "latest op per session". The SQLite `backups` mirror already stores `op_id` and `order`. A small per-session `ops.json` would also work. Lock and load only that op's keys. Where a scan can't be avoided, use `read_stack_heads_from_disk_unlocked` (metadata only), not full stack loads.
- Confidence: High

##### [HIGH] The in-memory backup store keeps every stacked file twice and never evicts
- Location: `backup.rs:152` (`BackupEntry { content: String, content_bytes: Arc<[u8]> }`), `backup.rs:4572` / `4666` / `5188` (constructors), `backup.rs:745` (`entries: HashMap<session, HashMap<PathBuf, Vec<BackupEntry>>>`)
- Hot path / frequency: every snapshot (every mutating tool call), for the daemon's whole lifetime
- Problem: each entry holds the file bytes (`content_bytes`) and also a full `String::from_utf8_lossy(..).into_owned()` copy in `content`. In non-test code `content` is never read; the doc comment says "kept for API/tests". Stacks are hydrated into `self.entries` and stay there until session GC, purge or restore. Nothing bounds this by bytes. The store is disk-backed, so the in-memory copies exist only as a cache. `DEFAULT_MAX_UNDO_DEPTH = 20` and `DEFAULT_MAX_BACKUP_FILE_SIZE = 64 MiB`.
- Impact: RSS is roughly 2 x (sum over edited files of stack depth x file size). For example, an agent iterating 20 times on a 5 MB generated file or fixture keeps about 200 MB resident. A long subc daemon serving many sessions accumulates this for each one.
- Suggested fix: delete `BackupEntry::content` and add a `#[cfg(test)]` accessor that decodes `content_bytes` on demand. Keep only metadata, or heads, in `entries`, and load blobs lazily on restore. The `hydrated_entries` Arc cache could become a byte-bounded LRU.
- Confidence: High

##### [HIGH] `move_symbol` reads and tree-sitter-parses every TS/JS file in the project, serially
- Location: `commands/move_symbol.rs:387` (`collect_ts_js_files`), loop at `:422`, `rewrite_consumer_imports` at `:1126` / `:1158`
- Hot path / frequency: every `move_symbol` call (a refactor tool, user-visible latency)
- Problem: the walk collects every TS/JS file, including hidden dirs, and calls `std::fs::canonicalize` on each one. The loop then reads each file with `CapturedRegularFile::read_text`, which copies the bytes into an `Arc` and then into a `String`. Any file containing the substring `"import"` or `"export"` (nearly every module) gets a full tree-sitter parse plus `imports::parse_imports` before the code checks whether any import points at the source file. All of it runs on one thread.
- Impact: O(total TS/JS bytes) of parsing. A 20k-file monorepo means tens of seconds per move, while the Mutating barrier blocks the root.
- Suggested fix: add a cheap prefilter before parsing. Require the consumer text to contain the source file's stem, e.g. `memchr::memmem` for `"utils"` from `./utils.ts`, or for `index` plus the parent dir name. Better, query the trigram index for that stem to get candidates. Parallelize read and parse with rayon, as `ast_replace` already does. Drop the per-file `canonicalize` when the walk root is already canonical.
- Confidence: High

##### [HIGH] Unbounded Myers diffs, run up to about 6 times per edit and duplicated
- Location: `edit.rs:190` (`build_unified_diff`), `edit.rs:225` (`attach_preview_diff`), `edit.rs:238` (`diff_counts`), `edit.rs:388` (`compute_reformatted_excerpt`); callers `commands/write.rs:88/224/227`, `edit_match.rs:189/362/390/1326/1438/1441`, `batch.rs:145/199/230`, `edit_symbol.rs:296/440/443`, `apply_patch.rs` (`diff_counts` per hunk, `metadata_files` per file, which re-diffs at each repeated hunk)
- Hot path / frequency: every write or edit. The OpenCode plugin always sends a `preview: true` call and then the real call (`packages/opencode-plugin/src/tools/hoisted.ts:606/798/936`).
- Problem: `similar::TextDiff::from_lines` uses similar 2.7's default config: Myers with no deadline, O((N+M)·D). The preview call alone runs three full diffs. `attach_mutation_diff` builds the unified diff, then `preview_diff` builds the same unified diff a second time with identical inputs, and `compute_diff_for_response` adds a count diff. The apply call runs another unified diff and a count diff, plus a third diff for the reformatted excerpt when a formatter changed the file. The unified diff is also uncapped. It is attached as `metadata.diff` on every response, and twice in preview responses (`metadata.diff` plus `preview_diff`), so a whole-file `write` of a 2 MB file returns about 2 MB of diff, or about 4 MB in preview.
- Impact: when an edit rewrites most of a large file (`write` of regenerated or reformatted content), D ≈ N. For 20k lines that is about 1.6e9 steps per diff, repeated about 5 times: multi-second stalls on the Mutating lane, plus multi-MB JSON over NDJSON.
- Suggested fix: compute one `TextDiff` per (before, after) pair and derive counts and the unified text from it. Drop the duplicate `build_unified_diff` in `attach_preview_diff`, since `metadata.diff == preview_diff`. Set `TextDiff::configure().timeout(Duration::from_millis(..))`, or use `Algorithm::Patience`, and cap the unified-diff size (a truncation marker like `compute_diff_info`'s 512 KB). In `apply_patch::metadata_files`, compute counts once per final entry instead of on every merge.
- Confidence: Medium (the cost depends on edit shape; it is cheap for small localized edits)

##### [HIGH] `batch` applies edits with one full-file copy per edit: O(edits × file size)
- Location: `commands/batch.rs:121-131` (Phase 4 loop calling `edit::replace_byte_range`), fed by `resolve_edit` (`:250`), which expands `replace_all` into one `ResolvedEdit` per match
- Hot path / frequency: every multi-edit `edit` call, and especially `replace_all` inside a batch
- Problem: `content = replace_byte_range(&content, ..)` allocates and copies the whole file for each resolved edit. With `replace_all`, the edit count equals the occurrence count. `edit_match` already fixed this exact pattern with `apply_sorted_non_overlapping_fuzzy_matches`, and has a test (`replace_all_output_construction_allocates_once`), but `batch` still does it the old way. The overlap check already proves the ranges are disjoint and sorted.
- Impact: a 1 MB file with 2,000 `replace_all` hits means about 2 GB of memcpy and allocation, which is seconds.
- Suggested fix: sort ascending and build the output in one pass (`push_str(&source[cursor..start]); push_str(repl); cursor = end`), or reuse `edit_match::apply_sorted_non_overlapping_fuzzy_matches`.
- Confidence: High

##### [MEDIUM] Each snapshot rehydrates the whole on-disk stack and re-copies every entry's content
- Location: `backup.rs:3563` (`ensure_stack_hydrated_locked` → `load_from_disk_if_needed_locked`, which always reads), `backup.rs:3841` (`entry_from_v2_meta`), `backup.rs:5217` (`content` String rebuild); called from `snapshot_with_op`, `snapshot_with_op_from_capture`, `snapshot_op_tombstone`, `snapshot_built_entry` and `record_post_mutation_state`
- Hot path / frequency: every mutation, once per touched file
- Problem: despite the name, `load_from_disk_if_needed_locked` reads unconditionally. Each snapshot reads and parses `meta.json` and `post-state.json`, takes two `stat`s per entry (the `DiskFileVersion` check), clones `Arc` bytes out of the `hydrated_entries` cache, and rebuilds every entry's `content` String with `from_utf8_lossy(..).into_owned()`. It then replaces the in-memory stack. `memory_stack_matches_disk` is the cheap "is my copy current?" check and already exists, but only `record_post_mutation_state` uses it.
- Impact: per edit, about (depth ≤ 20) × file-size bytes of UTF-8 validation plus copy, e.g. about 20 MB of churn per edit of a 1 MB file. Plus a JSON parse and about 40 stats.
- Suggested fix: compare stack ids against `meta.json` (as `memory_stack_matches_disk` does) and skip rehydration on a match. Once `content` is gone (see the memory finding), rehydration costs only metadata.
- Confidence: High

##### [MEDIUM] About 6 full-file reads and 2-4 parses per single-file edit
- Location: `edit.rs:673` (`write_format_validate_impl`: pre-write `read_to_string` plus parse, post-format `read_to_string` plus parse, a third read when `validate:"full"`), `edit.rs:316` (`auto_backup` → `backup_entry_from_path` re-reads), `backup.rs:2407` (`record_post_mutation_state` → `PathFingerprint::of_path` re-reads plus blake3 of the whole file, re-reads `meta.json`); callers `write.rs`, `edit_match.rs:1170-1441`, `batch.rs`, `edit_symbol.rs` (also `resolve_symbol` plus `FileParser::parse_source` in `callable_declaration_edit_range`, plus 3 more post-write reads)
- Hot path / frequency: every write or edit
- Problem: the handler has already read `source` and computed `new_source`, yet the pipeline reads the file again for the backup, again for pre-write syntax validity (with a re-parse), again after formatting, again for `lsp_post_write`, again for `final_content` (`write.rs` and `batch.rs` read it twice: once for the diff and once for `attach_mutation_diff`), and once more in `record_post_mutation_states` to hash it. Syntax is parsed before and after even when no formatter ran, in which case post-write content == `content`.
- Impact: about 6 reads, about 2-4 tree-sitter parses and 1 blake3 over the file per edit. Negligible for small files. For multi-MB files it is tens to hundreds of ms per call, doubled by the preview round trip.
- Suggested fix: pass the already-read `source` (or a `CapturedRegularFile`) into `auto_backup_from_capture` and `write_format_validate`. Skip the post-format read when the formatter did not run (`formatted == false`). Return `final_content` from the pipeline instead of letting each caller re-read it. Compute the post-state fingerprint from the bytes just written, avoiding a re-read.
- Confidence: High

##### [MEDIUM] Multi-file mutations spawn the formatter once per file, sequentially; glob edits also double-snapshot
- Location: `format.rs:1593` (`auto_format`, one subprocess per path); per-file loops in `edit_match.rs:699` (glob), `apply_patch.rs:336` (`write_patched_file` per hunk), `move_symbol` per file; `edit_match.rs:612` (glob checkpoint) followed by per-file `auto_backup_from_capture`
- Hot path / frequency: glob `edit`, `apply_patch` with N files, `move_symbol`, when `format_on_edit` is enabled (default `false`, `config.rs:919`)
- Problem: formatters like prettier, biome and ruff accept many paths in one invocation, but each file pays a separate process spawn: node startup of about 0.2-1 s for prettier, plus 2 reader threads. The glob path also writes a durable checkpoint, one fsynced blob per file, and then separately takes a durable backup of each file (content fsync, meta fsync, dir fsync). It deletes the checkpoint on success, so every file is snapshotted to disk twice. It also re-parses each file with a fresh `FileParser` (`validate_syntax`) after formatting.
- Impact: a glob edit over 200 TS files with prettier takes about 200 × 0.3-1 s, i.e. 1-3 minutes, plus about 1,000 fsyncs.
- Suggested fix: batch formatter invocations per (tool, root), as `validate_full_batch` already does for checkers. For glob edits, rely on the per-file backups, or roll back from the in-memory `PendingEdit.original_source`, instead of also creating a durable checkpoint.
- Confidence: High (the cost applies only when `format_on_edit` is on; the checkpoint duplication applies always)

##### [MEDIUM] Recursive delete and multi-file ops: about 4 fsyncs and dozens of syscalls per backed-up entry, with no batching
- Location: `backup.rs:3950` (`write_snapshot_to_disk_locked_with_db_plan`: content `write_temp_fsync_rename`, meta fsync, `fsync_dir`, `create_private_durable_dir`, which fsyncs the parent for each new stack dir), `backup.rs:3455` (`acquire_stack_disk_lock`: several mkdir/is_dir calls, gc-gate flock, lock-file flock per snapshot), `backup.rs:2768` (`touch_session` rewrites `session.json` per snapshot), `mirror_stack_to_db` (one SQLite tx per snapshot), then `record_post_mutation_states` (`main.rs:1273`), which per entry re-reads `meta.json`, rewrites `post-state.json` and runs another SQLite tx; `commands/delete_file.rs:894-940`
- Hot path / frequency: `delete_file` recursive (up to 2,000 entries), `apply_patch`, `move_symbol`, glob edit
- Problem: each file is its own durable stack, with its own dir, metadata and directory fsync, and no batching across an operation.
- Impact: a 2,000-file tree delete means about 8k fsyncs plus about 4k SQLite transactions plus about 60k metadata syscalls. On macOS (`F_FULLFSYNC`) that is tens of seconds, all under the Mutating barrier and the backup mutex.
- Suggested fix: add an operation-scoped batch API: write all content blobs, fsync them, then do one directory-fsync pass, write the metas, and commit all DB rows in one transaction. Rewrite `session.json` at most once per request. In `record_post_mutation_states`, group the pending entries by key and skip the meta re-read when the stack was written in this same request.
- Confidence: Medium (the per-entry cost is certain; the wall time depends on the filesystem)

##### [MEDIUM] Ambiguous-match error path is O(occurrences × file size) and returns unbounded data
- Location: `commands/edit_match.rs:1229-1240` and `commands/batch.rs:400-411` (`source[..pos].matches('\n').count()` plus `build_context`, which runs `source.lines().collect()` per occurrence); `edit_match.rs:1446`, `batch.rs:240`
- Hot path / frequency: every ambiguous `edit` (a common agent error path)
- Problem: for each occurrence the code rescans the prefix to count lines and re-collects every line of the file. The `occurrences` JSON array is uncapped. The human-readable text is capped at 8 by `AMBIGUOUS_OCCURRENCE_LIST_LIMIT`, but the data payload is not.
- Impact: a short needle (`;`, `}`, `return`) in a 20k-line file gives about 20k occurrences × 20k-line collect, which is seconds, plus a multi-MB error payload.
- Suggested fix: compute `compute_line_offsets` and `lines` once and binary-search each position. Cap the `occurrences` array at the same limit, plus a total count.
- Confidence: High

##### [LOW] Checkpoint store: every create/list/restore re-reads all meta and stats all blobs; checkpoints keep all bytes in RAM
- Location: `checkpoint.rs:939` (`hydrate_session_locked`), `checkpoint.rs:1434` (`read_checkpoint_from_disk`: re-reads `meta.json`, compares bytes, one `stat` per blob, clones the `HashMap`)
- Hot path / frequency: each checkpoint op, including the glob/move/rename rollback checkpoints
- Problem / Impact: O(checkpoints × files) syscalls per call (≤ 20 checkpoints). Bytes are `Arc`-shared, so memory is up to 20 × the tracked-file bytes per session. That is modest but unbounded by bytes.
- Suggested fix: skip rehydration when the session dir's mtime or an in-process generation counter is unchanged.
- Confidence: Medium

##### [LOW] write_ledger: backup and checkpoint credits keyed by file path, and DB rows written without cached statements
- Location: `backup.rs` (`write_ledger::credit(Domain::Backups, key.display().to_string(), ..)`), `checkpoint.rs` (keyed by checkpoint dir, a unique name per glob edit), `write_ledger.rs:259` (`register` takes the global registry mutex and allocates a String per call), `write_ledger.rs:504` (`tx.execute` per row)
- Problem / Impact: the `root_id` cardinality is the number of distinct edited files (and checkpoint dirs) per minute. Each fold writes one row each and keeps it for 7 days. `register` allocates and locks on every snapshot, against the module's own "retain a Counter" guidance. These are small costs, but they grow with heavy multi-file ops such as the 2k-entry delete.
- Suggested fix: credit the project root (the store's storage root) rather than the file path, and use `prepare_cached` in the fold.
- Confidence: Medium

##### [LOW] apply_patch / patch engine minor overheads
- Location: `patch/apply.rs:139` (owned `Vec<String>` per line; `line_refs()` rebuilt per chunk; reverse `splice` per replacement is O(chunks × lines)), `commands/apply_patch.rs:336` (re-reads the original already read in `apply_update`), `apply_patch.rs:834` (`metadata_files` clones before/after per hunk, re-diffs at each merge, and emits each file's patch twice: per-file `patch` plus the joined `diff`)
- Impact: small at normal patch sizes. Larger patches over big files pay extra copies and diffs.
- Suggested fix: borrow lines as `&str`, build the result in one forward pass, and pass `original` into `write_patched_file`.
- Confidence: High

##### [LOW] `CapturedRegularFile::read_text` keeps two copies of each file
- Location: `backup.rs:314`
- Problem: the bytes go into an `Arc<[u8]>`, then `.to_owned()` copies them into a `String`. Glob edits and `move_symbol` keep both copies for every candidate file at once (`PendingEdit`, `captures`, `consumer_rewrites`).
- Suggested fix: validate the UTF-8 once and share it, e.g. store `Arc<str>` when the bytes are valid UTF-8.
- Confidence: High

#### Area summary
The mutation path is correct and defensive, but it treats durability and observability as costs paid per file, per request, on every call. The main issues: undo cost grows with the whole session's history; the backup store keeps two in-memory copies of every stacked file version and rehydrates and re-copies the full stack on each snapshot; `move_symbol` parses the entire TS/JS project with no prefilter; and every edit runs about 5 unbounded Myers diffs (two of them identical) plus about 6 full-file re-reads. Batch `replace_all` is still quadratic even though `edit_match` already fixed the same pattern. Multi-file operations multiply all of this, through formatter spawns, about 4 fsyncs per entry and duplicate checkpoint-plus-backup snapshots. The most valuable fixes are an op index for undo, dropping `BackupEntry::content` and lazy stack loading, a consumer prefilter for `move_symbol`, and a single bounded diff per edit.

#### Things checked and found OK
- `fuzzy_match::find_all_fuzzy`: exact pass is `match_indices`; line passes pre-normalize once; the reflow pass is bounded by its non-whitespace window. Nearest-miss diagnostics are capped (`NEAREST_MISS_*` limits, 2 MB file cap).
- `patch/matcher.rs`: normalized tiers use a KMP-style scan; reflow is window-bounded; nearest-miss uses bounded sampling.
- `edit_match` single-file `replace_all` builds its output in one pass.
- `lsp_rename::apply_text_edits` resolves positions in one pass and builds in one pass.
- `ast_replace` computes in parallel with rayon.
- `format.rs::wait_with_deadline` is pipe-close driven (no fixed 50 ms sleep); tool resolution and availability are TTL-cached; `validate_full_batch` batches checker runs per tool and root.
- Hashline transaction: snapshot store is bounded (30 paths, 4 versions, 64 MB per session); edits splice only per operation; no formatter in that path. It holds the backup mutex across durable writes, but the executor's Mutating barrier already serializes these.
- Backup DB mirror uses append deltas in one transaction; SQLite runs WAL with `synchronous=NORMAL`.
- `delete_tree` walk is budgeted (2,000 entries, 100 MB) and walks once.
- `write_ledger::credit` on the hot path is atomic adds once registered; the folded registry entries are pruned.
- Backup-mutex touches per tool call in `run_tool_call.rs` are O(1) (`latest_skipped_order`, `skipped_reason_after`).

---

### 3.6 Core runtime & tool-call pipeline

Files reviewed (all paths under `crates/aft/src`):
- Deep-read: `run_tool_call.rs`, `commands/tool_call.rs`, `response_finalize.rs`, `executor/mod.rs` (scheduler, admission, lanes, `run_lane_job`), `main.rs` (standalone loop, `dispatch`, `dispatch_outcome`), `repeat_breaker.rs`, `alert_state.rs`, `fleet_status.rs` (client publish path), `config_live.rs` (reload path), `commands/configure.rs` (the `handle_configure_inner` prefix up to `artifact_owner_claim`, fast/equivalent paths, `detect_worktree_bridge`, `workspace_manifest_fingerprint`), `context.rs` (config snapshot/pin, status-bar counts, `StatusBarEmission`, path restriction, borrowed index open, health summary).
- Skimmed: `subc_translate.rs`, `subc_format.rs`, `alert_render.rs`, `alert_records.rs`, `list_envelope.rs`, `list_surfaces.rs` + `list_surfaces/*`, `ndjson_text.rs`, `commands/status.rs`, `commands/state.rs`, `config.rs`, `config_resolve.rs`, `feature_config.rs`, `protocol.rs`, `executor/single_flight.rs`, `executor/view_publication.rs`.
- Read for context (outside the area): `subc/manifest.rs` (`command_lane`), `subc/mod.rs` (where the lane is chosen), `subc/bash.rs` (bash lanes), `subc/wire.rs` (response body), `runtime_drain.rs` (watcher slice budget), `hashline/integration/binding.rs`, `reader_floor.rs`, `bounded_io.rs`.
- Test-only code (`executor/tests.rs`, `*_tests.rs`, `#[cfg(test)]`) was ignored.

#### Findings

##### [HIGH] Every bash call (and bash_kill/bash_write/notify/ack/db_set_state) is a per-root writer barrier, so it waits for every in-flight read and maintenance commit on the root
- Location: `subc/manifest.rs:155-193` (`command_lane_explicit`: `"bash" | "powershell" | "bash_kill" | "bash_write" | "bash_notify" | "bash_ack_completions" | "db_set_state" | ... => Lane::Mutating`); `subc/bash.rs:719-721` (spawn submitted on `Lane::Mutating`), `subc/bash.rs:1941-1943` (promotion is another `Lane::Mutating` job); admission at `executor/mod.rs:3676` (`try_admit_actor`): `Lane::Mutating => bind_may_share || (!has_epoch_reader && ...)` where `has_epoch_reader = read_inflight > 0 || lsp_inflight || maintenance_commit_inflight`; run in `run_lane_job` under `epoch.write()` / `try_write_for` (`executor/mod.rs:4120-4133`).
- Hot path / frequency: Bash is the agent's most frequent tool. This runs on every bash or powershell call in subc mode, again when a call hands off to the background (promotion), and on every plumbing call that touches bash state (`bash_ack_completions`, `bash_notify`, `bash_kill`, `bash_write`).
- Problem: A Mutating job is admitted only when no PureRead, SerialLspStatus, or MaintenanceCommit job holds the actor's epoch read gate, and it then takes the write gate. That means:
  1. A bash spawn waits for the longest in-flight read on the root. That can be a grep fallback walk while the trigram index builds, a read of a slow file, or an `lsp_*` or `status` call.
  2. It also waits for an in-flight MaintenanceCommit unit. The code itself notes that configure-tail units "can run for seconds on a large root" (comment in `try_admit_actor`).
  3. While the bash job is queued, `next_interactive_lane` still prefers PureReads (`executor/mod.rs:2729-2759`) until the writer is 6 s old (`INTERACTIVE_WRITER_PROMOTION_AGE`). Under a steady read stream from several sessions, bash can wait up to 6 s plus the time for the reads to drain.
  4. While bash holds the write gate, every new read on the root is blocked.

  The existence of the `bash_start_deadline` error path in `subc/bash.rs` suggests this already happens. Spawning a process and registering it in `BgTaskRegistry` (which has its own locks) does not need to exclude readers. Bash cannot be ordered against concurrent reads in any meaningful way anyway, because its file effects happen asynchronously after the spawn returns. `bash_ack_completions` and `bash_notify` only change registry state.
- Impact: Bash start latency is roughly the age of the oldest of: the longest in-flight read, the longest in-flight maintenance unit, or the 6 s writer-promotion window. This can easily mean seconds on large roots right after a bind or during index builds. Reads on the root also stall for the whole spawn.
- Suggested fix: Move the bash spawn and promotion, and the registry-only bash plumbing (`bash_ack_completions`, `bash_notify`, `bash_unnotify`, `bash_kill`, `bash_write`, `bash_abort_inflight`), to a non-barrier lane. That could be `PureRead`, or a new "SideEffect" lane that takes the epoch read gate, is serialized per actor like `SerialLspStatus`, and is not blocked by readers. Keep `Mutating` for AFT's own file mutations (`write`, `edit`, `apply_patch`, ...) and configure. If ordering after a preceding AFT edit matters, it already holds: the edit is a writer and runs before any later job is admitted.
- Confidence: Medium on magnitude (it depends on read and maintenance durations), High on the mechanism.

##### [MEDIUM] Head-of-line blocking in interactive admission: a HeavyInit job waiting for one of 3 global heavy permits blocks every non-read job queued behind it on that root
- Location: `executor/mod.rs:2729-2759` (`ClassQueues::next_interactive_lane` returns the first non-PureRead lane in arrival order), `executor/mod.rs:3756-3765` (`Lane::HeavyInit` is runnable only if `heavy.try_acquire()` succeeds, otherwise `try_admit_actor` returns `None`); permit count `heavy_permits = min(pool-1, 3)` (`executor/mod.rs:171-185`). The lane mapping in `subc/manifest.rs:150-153` routes **every** `search` (aft_search) and **every** `callgraph` op to `HeavyInit`, including warm, cheap queries.
- Hot path / frequency: Every `aft_search` and `aft_callgraph` call, on every root, shares 3 process-wide permits.
- Problem:
  1. A semantic search holds its permit for the whole job, including the query-embedding round trip to a remote or Ollama backend, which is bounded only by `semantic.query_timeout_ms`. `run_lane_job` runs HeavyInit without any gate, so the permit is the only limiter.
  2. With 3 slow searches in flight anywhere in the daemon, every root's callgraph queries (warm SQLite lookups, milliseconds each) and searches queue.
  3. Because admission picks only the first non-read lane, an `edit`, `lsp_*`, or `status` queued after that search on the same root cannot be admitted either. It waits for another root's permit holder to finish.
- Impact: Cross-root latency coupling. A slow embedding backend in one project stalls callgraph, edit, and LSP calls in unrelated sessions. On 2-4 core machines `heavy_permits` is 1-2, which makes this worse.
- Suggested fix:
  1. Classify warm callgraph ops and lexical-only searches as `PureRead`. Use `HeavyInit` only when a cold build or load is actually needed. The handler can return `Building` and let the background build hold a permit (`cold_build_limiter.rs` already limits cold builds).
  2. Release the permit around the network wait for query embedding.
  3. In `next_interactive_lane`, when the chosen lane is not runnable, fall through to the next runnable lane, or at least to queued Mutating and SerialLspStatus work.
- Confidence: Medium.

##### [MEDIUM] Standalone mode runs up to ~250 ms of watcher drain, plus the other runtime drains, on the critical path before *every* request
- Location: `main.rs` standalone loop: `configure_maintenance.drain_prefix(...)` then `drain_non_configure_runtime_events(&registry)` runs before `dispatch_outcome` for each request (`main.rs` around lines 560-575). `drain_non_configure_runtime_events` (`main.rs:797-818`) calls `drain_watcher_events` (`runtime_drain.rs:2563`, budget `WATCHER_DRAIN_SLICE_BUDGET = 250ms`, `WATCHER_PATH_DRAIN_BATCH_CAP` paths), plus the search, semantic, inspect, LSP, and callgraph drains, `maybe_spawn_fold`, and `maybe_spawn_retention`.
- Hot path / frequency: Every NDJSON request in standalone mode. The cost is non-trivial whenever watcher events are pending: after the agent's own edits, `git checkout`, formatters, or builds that touch tracked files.
- Problem: The drain runs before every command, not only index-backed ones. A `read`, `edit`, `bash`, `outline`, or `ping` that does not consult the trigram, semantic, or callgraph indexes still pays for a watcher slice: symbol-cache invalidation, search-index updates, callgraph refresh enqueueing, and semantic queueing. The response is also written only after that slice finishes. The post-response path already drains when no input is queued, so the pre-request drain only matters for index-backed queries.
- Impact: Up to ~250 ms (plus unit overrun) of added latency per request while a change burst is being applied. During a large checkout this repeats for many consecutive requests.
- Suggested fix: Before dispatch, drain only when `watcher_query_pending(ctx, &req.command)` would be true (the index-backed commands: grep, glob, search, inspect, callgraph, ...). Leave other commands to the post-response and idle drains, which already exist. Alternatively, use a much smaller pre-request budget, such as 10-20 ms.
- Confidence: Medium-High.

##### [MEDIUM] `AlertDeltaState` grows without bound: sessions with new diagnostics are never reaped because nothing marks them rendered
- Location: `alert_state.rs:409-434` (`reap_idle_sessions_at` skips any session with `live` identities that are not in `rendered`), `alert_state.rs:441-530` (`accept_observation`: only the baseline pass inserts into `rendered`; identities that enter later are put in `live` only). The only production writer is `context.rs:4647` (`accept_alert_observation_batch`, called from `commands/inspect.rs:1365`). `AlertEngine::finalize` (`alert_render.rs:263`), which marks alerts rendered, is reached only through `response_finalize::finalize_response_for_dispatch_root`, and that function has no non-test caller. Nothing calls `remove_session` on this state either.
- Hot path / frequency: Every blocking `aft_inspect` diagnostics observation, in every session, for the life of the process.
- Problem:
  1. After a partition's baseline, any newly entered diagnostic stays "undelivered" forever. The owning session is then exempt from idle reaping, so `partitions` and `session_last_touched` keep that session's diagnostics forever.
  2. Every later `accept_batch` first runs `reap_idle_sessions_at`, which scans **all** partitions' `live` maps. That is O(total retained identities) per inspect call, and the total keeps growing.
- Impact: A memory leak that grows with sessions times diagnostics in a long-running subc daemon. The per-inspect reap cost also grows linearly with it.
- Suggested fix: Either wire the rendering path so identities get marked rendered, or, while alert delivery is disabled, stop exempting undelivered sessions from TTL reaping (or skip `accept_alert_observation_batch` entirely). Also clear the session's state from the route or session teardown path.
- Confidence: Medium-High (verified that no production caller marks rendered after baseline).

##### [MEDIUM] Interactive writers (edits, writes, apply_patch) wait up to 6 s behind a stream of pure reads on the same root
- Location: `executor/mod.rs:2729-2759` (`next_interactive_lane`: reads first unless a writer is older than `INTERACTIVE_WRITER_PROMOTION_AGE = 6s`); `executor/mod.rs:3767-3769` (a writer needs `!has_epoch_reader`).
- Hot path / frequency: Any root where several sessions or subagents share a root actor in subc mode, or where one session pipelines reads.
- Problem: With `read_cap` up to 4 and reads always preferred, a writer is admitted only when the PureRead queue happens to empty *and* every in-flight reader has finished. If any of the 4 reader slots is always occupied, that never happens until the 6 s promotion. Promotion then still waits for in-flight reads to drain.
- Impact: Edit latency of up to about 6 s plus the longest read in multi-session or subagent workloads. Bind configures have their own 500 ms promotion, but edits do not.
- Suggested fix: Use a much shorter promotion age for interactive tool mutations (for example 250-500 ms, as binds do), or alternate fairly between lanes. For example, stop admitting new reads as soon as a writer is queued, unless the writer is blocked on stuck readers (the existing `READER_STUCK_CENSUS_AGE` logic already handles that case).
- Confidence: Medium.

##### [LOW] subc picks the executor lane from the agent's tool name; unlisted spellings default to the writer barrier
- Location: `subc/mod.rs:8851` (`let lane = command_lane(&bare_name);` with the *agent* name), `subc/manifest.rs:199-201` (`command_lane_explicit(...).unwrap_or(Lane::Mutating)`). `run_tool_call.rs:372` and `subc_format.rs:288` both accept the `aft_inspect` spelling, but `command_lane("aft_inspect")` is `Mutating`. That spelling also misses `uses_deferred_response_seam` (`subc/mod.rs:8866`, which checks only `"inspect"`). `health.digest` and `memory.census` are also unlisted and fall to `Mutating`.
- Hot path / frequency: Only when a host sends one of those spellings. Normally plugins hoist the name to `inspect`.
- Problem: An unlisted read-only tool silently becomes an exclusive writer, which drains all readers and blocks the root for the length of an inspect.
- Suggested fix: Map the name the same way `prepare_tool_call` does before choosing the lane. Make the default `PureRead` for read-only names, or fail closed with a log line instead of silently using `Mutating`.
- Confidence: Medium.

##### [LOW] Agent-visible payloads are serialized twice: the full structured data and the rendered text that copies it
- Location: `commands/tool_call.rs:136-143` (`response_with_text` inserts `text` into `data`, which still holds `content` / `matches` / `output`); `subc_format.rs:1481-1509` (`format_read` copies `data.content` into the text); `subc/wire.rs:533-600` (`structuredContent` carries the full flat data plus `text`, and `content[0].text` carries the text again).
- Hot path / frequency: Every standalone `tool_call` and every trusted subc tool response.
- Problem: For `read`, `bash`, and `grep` the dominant payload (file content, command output, match lines) is sent two or three times. That costs one more serialization on the Rust side, more stdout or TCP bytes, and a bigger `JSON.parse` on the TS side.
- Impact: About 2-3x the wire bytes for large reads and outputs, at up to 2 MB per reply (`REPLY_CEILING_BYTES`).
- Suggested fix: For first-party plugins that consume `text`, drop the duplicated large fields (`content`, `output`, `matches[*].line_text`) from `data` after formatting, or replace them with a marker. Alternatively, let the plugin opt out of structured duplicates per call.
- Confidence: Medium (the plugin may still read some structured fields; check before removing them).

##### [LOW] Repeat breaker deep-clones and serializes the full arguments of every write, edit, and apply_patch, then throws the result away
- Location: `repeat_breaker.rs:33-41` (`RepeatCall::new` always computes `semantic_key`), `repeat_breaker.rs:263-272` (the default arm clones the whole `input` and runs `canonicalize` + `serde_json::to_string`); `repeat_breaker.rs:155-171` (`observe_at` returns `None` for mutating non-shell calls without using the key). `run_tool_call.rs:524-525` also hashes the full rendered text (`output_hash`) before that early return.
- Hot path / frequency: Every mutating tool call. `RepeatObservation::for_agent_call` runs before dispatch.
- Problem: An O(content size) clone, sort, and serialize of the file content or patch text, plus an O(output) hash, whose results are never used.
- Impact: Roughly 1-3 extra copies of each write payload (hundreds of KB to MB for large writes). Small but pure waste.
- Suggested fix: Compute `mutating` first and skip `semantic_key`/`output_hash` for mutating non-shell tools. In `select_fields`, avoid `input.clone()` in the default arm by serializing directly from a borrowed, sorted view.
- Confidence: High.

##### [LOW] Borrowed search-index open waits with a 1 ms sleep loop on the request worker
- Location: `context.rs:5838-5849` (`open_borrowed_search_index`: `loop { slot.lock() ...; std::thread::sleep(Duration::from_millis(1)); }` until `BORROWED_INDEX_LOAD_BUDGET`).
- Hot path / frequency: The first query after a borrowed artifact generation changes on worktree or borrow-only roots, and on cross-root searches (`commands/semantic_search/mod.rs:317`).
- Problem: A busy-poll that wakes and locks about 1000 times a second while holding an executor worker (and a heavy permit for `search`).
- Suggested fix: Put a `Condvar` (or a crossbeam oneshot) in `BorrowedSearchFlight`, have the loader thread signal it, and wait with a timeout.
- Confidence: High (mechanism), Low (impact).

##### [LOW] Path-restriction root memo holds a per-root mutex across a filesystem `exists`/`canonicalize`
- Location: `context.rs:11860-11900` (`resolved_path_restriction_root`: `self.path_restriction_root_memo.lock()` stays held across `cached.resolved_root.exists()` / `bounded_io::metadata`, and across `canonicalize` on a miss).
- Hot path / frequency: Every path-validating request (`validate_path`, `validate_read_path`, `validate_write_location`) when `restrict_to_project_root` is on.
- Problem: Concurrent readers on the same root serialize on one stat. On a slow or remote volume (the reason `bounded_io` exists) every validation queues behind the probe of whichever request got the lock first.
- Suggested fix: Clone the memo under the lock, release the lock, stat, and re-lock only to update. Or trust the memo and revalidate only on watcher or configure events.
- Confidence: Medium.

##### [LOW] Per-call finalize overhead: repeated locks, redundant copies, and a quadratic gap merge
- Location:
  - `run_tool_call.rs:581-604`: two separate `app_ctx.backup().lock()` acquisitions per call. The backup store mutex is shared with snapshot I/O, so a slow snapshot elsewhere stalls every tool call's finalize.
  - `run_tool_call.rs:230-272` and `run_tool_call.rs:347-362`: `ensure_hashline_registration` and `capture` take the binding registry lock 2-3 times per call, each time allocating a `SessionKey` (a PathBuf and a String).
  - `response_finalize.rs:1159-1179` (`attach_checkout_query_gaps`): `gaps.contains(&gap)` inside a loop over `unreflected` is O(n²) over JSON values, and it runs twice per call (in `dispatch`/`run_tool_call` and again in `finalize_prefix`).
  - `response_finalize.rs:258-290` (`attach_alert_block`): copies the whole text (`to_string`) and builds a new string with `format!` (dormant path).
  - `commands/tool_call.rs:64-69`: deep-clones `arguments` out of `req.params` instead of taking it.
  - `subc_format.rs:1643-1648` (`format_grep` fallback): `.cloned()` copies the whole `matches` array.
- Hot path / frequency: Every tool call. The gap merge matters only with views enabled and many unreflected paths.
- Problem / impact: Each item is small. Together they add fixed overhead, plus one O(n²) term after large checkouts with views on.
- Suggested fix: Take the `latest_skipped_order` / `skipped_reason_after` values in one lock acquisition. Cache the `SessionKey`, or make registration a no-op when nothing changed (via a read-only peek). Use a `HashSet<&str>` of existing gap paths. Move `arguments` out with `take()`. Iterate `matches` by reference.
- Confidence: High (mechanisms), Low (impact).

##### [LOW] `db_set_state` writes SQLite and also an fsynced legacy file, under the writer barrier
- Location: `commands/state.rs:62-123` (SQLite `set_harness_state`, then `atomic_write` of the legacy file with `sync_all` on the file and on its directory, `commands/state.rs:333-363`); its lane is `Mutating` (`subc/manifest.rs`).
- Hot path / frequency: Plugin notification-state updates (`packages/*/src/notifications.ts`). Infrequent.
- Problem: Two durable writes and two fsyncs per call, and it holds the root's write gate, so all reads stall meanwhile.
- Suggested fix: Drop the legacy mirror once readers have migrated, or write it without fsync. Run state get/set on a non-barrier lane, since it touches only the DB and the harness file, not the workspace.
- Confidence: Medium.

#### Area summary
The single-request pipeline is well optimized. Config is an `Arc` snapshot pinned per request with no re-resolution. Status-bar counts are cached by generation and read with `try_lock`. Fleet publishing is non-blocking and rate-limited. Configure has a fast path and memoizes its worktree probe, cache key, and reader floor. Health and status probes use `try_*` locks throughout. The real latency risks are in **executor scheduling**, not per-call CPU:
- bash and bash-plumbing commands are classified as exclusive writers, so they wait on readers and maintenance;
- reads are preferred over writers for up to 6 s;
- warm callgraph and search queries contend for three process-wide heavy permits, and head-of-line blocking spreads that wait to other lanes on the same root.

In standalone mode, a watcher drain slice of up to 250 ms sits in front of every request. One real leak exists: `AlertDeltaState` retains sessions forever because the rendering half of the alert pipeline is not wired. The remaining findings are small constant-factor waste on the per-call path: duplicated payloads, repeat-breaker serialization of write content, a sleep-poll loop, and repeated locks.

#### Things checked and found OK
- `AppContext::config()` / `pin_config()`: an `Arc` clone plus a thread-local pin, with no clone of `Config` per request. `update_config`/`publish_config` clone `Config` only on publish.
- Live config reload (`config_live::drain_config_reload`): signal- and debounce-gated. Files are re-read only when a watch fires, and the reload compares texts before resolving.
- Repeat breaker state is bounded (256 calls and 64 keys per session; idle sessions pruned). The work per call is small apart from the mutating-call waste noted above.
- `try_status_bar_count_values`: `try_lock`s the LSP manager and returns the cached counts when the diagnostics, tier-2, and tsconfig generations are unchanged. `StatusBarEmission` holds at most 256 sessions.
- `FleetStatusClient::publish`: `try_lock`, a cadence gate, and `try_send`. No I/O on the request path.
- Status snapshot pushes are debounced (`status_debounce_loop`). `dir_size` sizes only the per-project key directory. Compression stats use `try_lock` with a cached fallback.
- Scheduler: events are processed in batches capped at 64. Maintenance is bounded (512) and idempotent maintenance jobs coalesce. Binds get reserved workers and skip DRR. Epoch waits use timed lock attempts, not busy loops.
- Configure: the equivalent-bind fast path avoids canonicalize and the git probe. `reader_floor::prepare` is memoized per storage root. `detect_worktree_bridge` is cached by `.git` shape. `workspace_manifest_fingerprint` reads a bounded set of files. Config equality uses `to_value` on two `Config`s, which is fine at bind frequency.
- `subc_translate`: owned translation moves `write` content without copying. `edit` copies old and new strings once (small).
- `ndjson_text` and list envelopes: `from_value(v.clone())` is used only on small envelope objects.
- The `fleet_status`, `alert_records`, `list_surfaces/*`, and `feature_config` code paths have no hot-path I/O or quadratic loops.

---

### 3.7 Subc daemon edge and gh shim

Files reviewed (non-test code; test modules skipped):
- `crates/aft/src/subc/mod.rs` (frame loop, reader/writer tasks, maintenance arm, route state, deferred responses, health probe dispatch, gh relay/backups spawn), `subc/wire.rs`, `subc/health.rs` (DispatchPathMetrics, HealthRollupCache/Worker, rollup builder, health report, budget), `subc/tool_provider.rs` (catalog, admission, LedgerEdge defer/authorize/proxy/custody), `subc/bash.rs` (held/deferred bash waits), `subc/push.rs`, `subc/standing.rs`, `subc/drain.rs`, `subc/run_scope.rs`, `subc/consent.rs`, `subc/readiness.rs`, `subc/stall_watchdog.rs`, `subc/persistence.rs`, `subc/access_grants.rs`, `subc/remote_policy.rs`, `subc/manifest.rs`, `subc/blob_store.rs` (skim)
- `standing_roots.rs`, `scoped_key.rs`, `subc_config.rs`, `memory.rs`, `lifecycle_census.rs`
- `gh_shim.rs` (non-test part, lines 1-7289), `gh_shim_relay/mod.rs`, `gh_shim_relay_client.rs`, `gh_shim_ticket.rs`
- Followed call-outs where the cost is incurred: `context.rs::memory_estimates`, the `estimated_memory` impls (semantic_index, search_index, parser, inspect, bash registry, lsp), `db/mod.rs::open_with_mode`, `db/standing_roots.rs::ensure_standing_root`, `search_index.rs::verify_file_mtimes` / `repo_root_commit_with_retry`, `executor/mod.rs` coalescing, `callgraph_store::stale_path_census`, `lsp/child_registry.rs::health_for_children`.

#### Findings

##### [CRITICAL] Standing-root reconciliation runs every 250 ms: 2 git subprocesses per root, a fresh aft.db open, and a write transaction per root
- Location: `subc/mod.rs:5877-5881` (maintenance arm requests `standing_worker.request()` every `STANDING_MAINTENANCE_INTERVAL` = `DRAIN_TICK_PERIOD` = 250 ms); `subc/standing.rs:233` (`StandingActor::tick`); `standing_roots.rs:295` (`StandingRoots::reconcile`), `standing_roots.rs:641` (`resolve_entries`); `scoped_key.rs:236` (`resolve_standing_root`), `scoped_key.rs:310` (`find_git_toplevel`), `scoped_key.rs:203` (`classify_resolved_standing_root`, which calls `artifact_cache_key` / `canonical_git_root_commit_identity`, both going to `search_index.rs:6961` `repo_root_commit_with_retry`); `db/standing_roots.rs:95` (`ensure_standing_root`).
- Hot path / frequency: every maintenance tick (about 4 Hz) for the whole life of every subc daemon process. The worker is `sync_channel(1)`, so when a pass takes longer than 250 ms the next one is already queued and the thread runs back to back.
- Problem: every `tick()`:
  1. `observe_config_snapshot` clones the whole `Config` and enumerates `executor.actor_entries()`. `tick` then clones it again (`self.observed_config.lock().clone()`).
  2. `reconcile` → `resolve_entries` calls `resolve_standing_root` for **every** configured `index.roots` entry. Each call does `std::fs::canonicalize`, spawns `git -C <path> rev-parse --show-toplevel` and canonicalizes its output. It then spawns `git` again for the root-commit probe (`git_root_commit_once`, via `artifact_cache_key` or `canonical_git_root_commit_identity`), with up to 3 retries and 50/100 ms sleeps on transient failures. The comment at `standing_roots.rs:326` acknowledges "Resolving again would spawn git", but resolution itself happens on every tick.
  3. `crate::db::open(&db_path)` runs **unconditionally, even with zero standing roots**. `open_with_mode` does `peek_schema_version` (a second, read-only SQLite open), `private_storage::open_root`, the connection open, PRAGMAs (journal_mode/synchronous) and `run_migrations`, then drops it all.
  4. For each entry, `ensure_standing_root` opens `BEGIN IMMEDIATE` (takes the aft.db write lock), reads the row, reconciles the freshness rows and commits. This contends with the call ledger, bash task persistence, write-ledger folds and other processes sharing aft.db.
  5. `resume_entries_without_bound_session` enumerates `actor_entries()` again and calls `memoized_artifact_cache_key` for each.
- Impact: with N standing roots, about 8N git process spawns per second, plus 4 SQLite open/migrate cycles and 4N IMMEDIATE transactions per second, all permanently. The 2026-09 investigation found 18 standing-root rows on a real machine, which works out to about 144 git spawns per second. That is a CPU core spent on fork/exec, and aft.db write-lock churn that shows up as `database is locked` waits for tool-path writers. On large repos the root-commit probe (`rev-list --max-parents=0`) can take tens to hundreds of ms per call, so the worker never idles. Even with no standing roots, an idle daemon opens aft.db about 345k times a day.
- Suggested fix: reconcile only when the observed config snapshot changes. Compare `index.roots` and `storage_dir`, or bump a config generation, and skip resolution and DB work when nothing changed. Cache `ResolvedStandingRoot` per literal path and re-validate it cheaply: stat or mtime of the target and of `.git/HEAD`, or a slow cadence such as 30-60 s. Keep one long-lived connection (or `shared_app.db()`) instead of `db::open` per tick. Only call `ensure_standing_root` for added or replaced entries. Return early before `db::open` when the entries are empty and were empty last time.
- Confidence: High (the path was traced end to end; `tick` has no gate on config change).

##### [CRITICAL] Unbound standing roots re-run a full strict index verification (or a full rebuild) back to back, about every 250 ms
- Location: `subc/standing.rs:259-263` (`tick` → `submit_entry_pass` for every active entry), `subc/standing.rs:337-421` (`submit_entry_pass` job), `subc/standing.rs:501-545` (`strict_verify_current_state`), `subc/standing.rs:547-605` (`build_missing_search_after_strict_check`); `standing_roots.rs:209` (`ArtifactLifecycle::admit` gates only on admission open / bind pending / session bound); `executor/mod.rs:2774` (`has_maintenance_coalesce_key` coalesces only against **queued** jobs).
- Hot path / frequency: every standing tick (250 ms), for each configured standing root that no session currently has bound. Coalescing drops a duplicate only while one is still queued, so as soon as a pass starts running the next tick queues another. In effect the passes run continuously on the root's MaintenanceCommit lane.
- Problem: the pass does not consult `needs_strict_verify` (the durable flag exists in `db/standing_roots.rs`, and `route_explicit_path` reads it). The comment says "A strict plan is selected unconditionally at a standing pass boundary". For `IndexKind::Search` each pass does:
  - `SearchIndex::read_from_disk(&cache_dir, ...)`, which deserializes the whole trigram index;
  - `verify_against_disk_with_strategy(..., VerifyStrategy::Strict)`, which does a full `walk_project_files_from_inner` of the tree and verifies **every** file with the strict strategy (content-hash path in `cache_freshness::verify_files_bounded`);
  - when verification finds any drift, `SearchIndex::build_with_limit_to_cache_dir`, a full cold rebuild, followed by `write_to_disk`;
  - `record_strict_verification`, which opens yet another aft.db connection (`open_database` → `db::open`) and writes.
  Each pass also acquires a cold-build limiter permit, which competes with real session cold builds.
- Impact: for a 100k-file standing root this means continuous full-tree walks plus a blake3 pass over the whole tree, plus a whole-index deserialize, repeated for as long as the daemon lives (minutes of CPU per hour, heavy page-cache churn and disk reads). On an actively edited tree the strict check fails on every pass, so the root is fully rebuilt and rewritten in a loop. The cold-build limiter permits also delay session builds.
- Suggested fix: gate the pass on `needs_strict_verify` (or a lifecycle "dirty" bit set by `revoke_and_mint` / `resume_after_session` / reconcile changes) and skip when the last strict verification succeeded and nothing invalidated it. Otherwise use a long minimum interval per entry (minutes), StatFirst for routine passes, and incremental update instead of full rebuild. At minimum, keep a per-entry "last pass finished at" and do not resubmit within a configured period.
- Confidence: Medium-High. No gate was found between `admit_build` and the strict verify, and coalescing semantics were verified. Not measured live.

##### [HIGH] Health rollup walks every root's in-memory indexes, plus several DB queries and git spawns, every 3 s whether or not anyone asks for health
- Location: `subc/health.rs:1836-1870` (`HealthRollupWorker::start_with_interval`: refresh, then wait `HEALTH_ROLLUP_TTL` = 3 s, unconditionally), `subc/health.rs:2078-2453` (`build_health_diagnostic_rollup`); `context.rs:12321` (`memory_estimates`); `semantic_index.rs:4998/4113`, `parser.rs:1507` (+`symbols_estimated_bytes` at 1142), `search_index.rs:491`, `inspect/manager.rs:2045` + `inspect/cache.rs:461` (`estimated_json_bytes` recursion, `memory.rs:810`), `bash_background/registry.rs:8462`, `lsp/manager.rs:1172` / `lsp/diagnostics.rs:163`; `callgraph_store/mod.rs:7379,17854` (`try_stale_path_census`); `subc/health.rs:5381-5407` (`standing_health_entries`); `lifecycle_census.rs:96` (`collect`).
- Hot path / frequency: a dedicated thread per subc connection runs the rollup every 3 s for the whole process lifetime, including when idle. It is also requested again after each route bind.
- Problem: per actor root on every refresh:
  - `ctx.memory_root_snapshot_and_rollup()` → `memory_estimates()` does O(size) walks: every semantic entry (twice: `vector_bytes` and `text_bytes` folds, plus every path in four maps), every cached symbol file and every symbol and scope string (`symbols_estimated_bytes`), every search-index file / path_to_id / delta posting list, every inspect aggregate's JSON payload recursively, every LSP document and diagnostic, and a clone of every bash task Arc. These run under `try_read` on `semantic_index`, `search_index` and `symbol_cache`, and under `lsp_manager.try_lock()`. The try only avoids waiting; once acquired, the lock is **held for the whole walk**. Writers (index installs, symbol-cache inserts during parsing) and LSP tool calls that need `lsp_manager` block behind the walk.
  - `try_stale_path_census` holds the callgraph store's connection mutex while it runs two `SELECT ... FROM backend_file_state` queries and does a `normalize_project_file_path` + `fs::metadata` per stale row. Tool-path callgraph queries block on that mutex meanwhile.
  - `refresh_build_suspensions` reads the build-breaker store. `crate::persisted_format::refusals_under` and `storage_retention::schedule` are cheap. `embedding_backend_build_health` and `query_embed_health_snapshot` are global lookups.
  - Process-wide: `standing_health_entries` opens aft.db read-only and calls `resolve_standing_root` (2 git spawns) per standing root. `lifecycle_census::collect` reads `/proc/self/task/*/comm` for every thread (hundreds of file reads), counts `/proc/self/fd`, and probes cwd/RSS of every LSP child (macOS also allocates 512 KiB + 128 KiB buffers). `render_memory_census` builds the full uncapped census JSON.
- Impact: proportional to total resident index size. With a few large roots (around 1M symbols, 100k+ semantic chunks) a single rollup is plausibly tens of ms of pointer-chasing every 3 s, about 1-3% of a core even when idle. More importantly, it periodically holds index read locks and the `lsp_manager` / callgraph connection mutexes for that long, so tool calls see latency spikes. Standing-root git spawns add 2N processes every 3 s.
- Suggested fix: maintain memory attribution incrementally (byte counters updated on insert/remove in each structure) so `estimated_memory` is O(1), or at least cache the per-root estimate and recompute it only when that structure's generation changes. Never walk while holding a lock that writers need: snapshot the counters, or clone `Arc`s and walk outside the lock. Make the rollup demand-driven: refresh only when a health probe or census request arrived within the last TTL, or on a much longer idle interval such as 30-60 s. Cache the standing-root resolution (see the first finding). Read `/proc/self/status` `Threads:` for the count and classify thread names only on explicit census requests.
- Confidence: High for the frequency and the walks. Medium for the magnitude (not profiled).

##### [MEDIUM] Health rollup measures allocator statistics twice per refresh, defeating the 60 s allocator sampling cache
- Location: `subc/health.rs:2380-2383` (`memory_rollup_metrics` → `MemoryRollupSnapshot::new`, plus `MemorySnapshot::new_uncapped`); `memory.rs:574-583` (`MemoryRollupSnapshot::new` uses `AllocatorSource::Measure`), `memory.rs:604-609` (`new_uncapped` uses `AllocatorSource::Measure`), `memory.rs:949` (`measure_allocator_observation`), comment at `memory.rs:~985` ("Sampling at this cadence would repeatedly take allocator-zone locks, so cache a process-wide observation for 60s").
- Hot path / frequency: every 3 s rollup, 2 calls each, so 40 allocator walks per minute instead of the intended 1.
- Problem: glibc `mallinfo2()` locks and walks every arena, and macOS `malloc_zone_statistics` walks the default zone under its lock. Both stall every allocating thread for the duration. The code documents that the walk should be cached for 60 s, but both rollup paths request a fresh `Measure`. The rollup also reads RSS and phys-footprint twice and calls `shared_semantic_bases_memory()` twice.
- Impact: on a 30+ thread daemon with large heaps each walk can take hundreds of microseconds to milliseconds and briefly serialize allocation process-wide, 40 times a minute.
- Suggested fix: use `AllocatorSource::Cached` in the periodic rollup and let the existing 60 s sampler refresh it. Compute `ProcessMemorySnapshot` once and share it between `memory` and `memory_census`. Reserve `Measure` for explicit on-demand census requests.
- Confidence: High.

##### [MEDIUM] Keyed tool calls (with `call_key`) take two extra Mutating-lane hops and several deep copies; reads are serialized behind mutating work, and the reply waits for a ledger write
- Location: `subc/tool_provider.rs:1128-1512` (`LedgerEdge::defer`), `subc/tool_provider.rs:1874-1975` (`LedgerEdge::proxy`), `subc/mod.rs:4680-4684` (all writer traffic is routed through the proxy).
- Hot path / frequency: every tool call that carries a `call_key` (subc-protocol v1 consumers), including read-only tools. Every outgoing frame of every route also passes through the proxy task.
- Problem:
  - Admission submits a `Lane::Mutating` job (`ledger::sweep` at most every 250 ms per root, `reduce_expired` and `admit` on aft.db) **before** the call is dispatched, "even a read" (comment at line 1239). A `grep`/`read` with a key therefore queues behind any running edit, write or apply on that root.
  - On completion the proxy intercepts the terminal frame and submits a second `Lane::Mutating` job (`ledger-settle`: `get` + `settle` with the full response body). Only after that job finishes does it forward the reply to the real writer. Response latency to the agent therefore includes queueing on the root's Mutating lane.
  - Copies: `defer` deep-clones the whole request `Value` (`body.clone()` at 1172) for `from_value`, clones `call.arguments` into `plan_args` even when `held` is false (1192), and serializes `arguments` again for `composition_digest` (`json!` takes it by value via `to_value`). `handle_tool_call` then re-runs `from_value` and `admit` (JSON-schema validation) on the same body. In `proxy`, `outgoing.body.clone()` copies the whole response body into `recorded`, which is then cloned again into `record_for_job` (1912-1916) and never used otherwise. That is two full copies of a response that can be hundreds of KB.
  - Every frame, including lossy Push and Pong, pays an extra mpsc hop through the proxy task and a `calls` mutex lock for terminals.
- Impact: added p50 latency of two executor round trips plus two SQLite writes per keyed call. Under concurrent mutating work, read calls inherit the mutating queue wait (possibly seconds behind a long apply). For large `write`/`edit` bodies, there are 3-4 redundant deep copies of multi-MB payloads on the frame-loop thread.
- Suggested fix: run admission and settle for read-only (PureRead) tools on a dedicated ledger lane or thread that owns its own aft.db connection, rather than the root's Mutating lane. Forward the reply to the writer immediately and settle asynchronously (or settle before enqueuing only for held bash). Move `recorded` into the job instead of cloning twice. Deserialize `ToolCallRequest` once and carry it through `DecodedFrame` to `handle_tool_call`. Only clone `plan_args` when `held`. Compute the digest from a borrowed serializer (`serde_json::to_vec(&(&call.name, &call.schema_pin, &call.arguments))` into the hasher).
- Confidence: Medium-High for the structure. The latency magnitude depends on lane load.

##### [MEDIUM] Health probe reply: deep clone of cached metrics plus repeated full re-serialization inside the budget loop, on the frame-loop thread
- Location: `subc/health.rs:2492-2625` (`build_health_report_with_indexing`), `subc/health.rs:1462-1570` (`encoded_health_metrics_len`, `refresh_metrics_bytes`, `budget_health_metrics`); called from `subc/mod.rs:6739` (`send_cached_health_response`) on the single-threaded frame loop.
- Hot path / frequency: every daemon health probe (periodic, from the daemon's metrics cache).
- Problem: `rollup.metrics.as_object().cloned()` deep-clones the cached rollup map, which includes per-root detail, storage retention and lifecycle data. `budget_health_metrics` then loops. Each iteration calls `refresh_metrics_bytes`, which can serialize the whole map up to 5 times, and each serialization first **clones the whole map** (`Value::Object(metrics.clone())`). Each over-budget iteration pops just one root, or one retention report, and repeats. Worst case is about (8 roots + 1 ledger compaction + N retention reports) × 5 × (clone + `to_vec`) per probe.
- Impact: with a pre-trim map of tens of KB, this can reach on the order of 100 clone+serialize passes (a few ms) of synchronous work on the frame loop per probe, which delays every route's frames. Each probe also redoes the same trimming of identical cached data.
- Suggested fix: serialize by reference (`serde_json::to_writer(CountingWriter, &metrics)` with no clone and no allocation). Do the budget trimming once in the background rollup and cache the trimmed result. Size `metrics_bytes` arithmetically: the digit-count change is at most a few bytes, so one pass suffices.
- Confidence: High for the code behaviour. Medium for the magnitude.

##### [LOW] Maintenance tick spawns two fresh OS threads every 250 ms
- Location: `subc/mod.rs:5809` + `subc/mod.rs:2481-2498` (`spawn_orphaned_lsp_child_reap`), `subc/mod.rs:5822` + `subc/mod.rs:2122-2141` (`RootPresenceProbe::request`), `subc/mod.rs:5795-5800` (collects every root's bash registry each tick for `maybe_spawn_retention` even when retention is not due).
- Hot path / frequency: every maintenance tick (4 Hz), forever, when any root is live.
- Problem: each tick creates `aft-lsp-child-reap` (which probes the cwd of every tracked LSP child) and `aft-root-presence` (one `exists()` per root), each a new `std::thread`. `RootPresenceProbe::latest()` also clones two `HashSet<ProjectRootId>` per tick, and `retention_registries` allocates and clones a Vec of registries even though `maybe_spawn_retention` returns immediately when not due.
- Impact: about 700k thread creations per day, plus constant small allocations. That is not large CPU, but it is wasted wakeups and churn in a daemon that should be quiet when idle. Root deletion and orphaned-LSP detection do not need 250 ms resolution.
- Suggested fix: use one long-lived housekeeping thread fed by a channel, or run these probes on a 5-10 s cadence. Check `retention_next_due` before collecting registries.
- Confidence: High.

##### [LOW] Per-session and per-request maps that are only pruned on root deletion or on a Response/Error terminal grow over a long daemon life
- Location: `subc/health.rs:607-695` (`unanswered_tools`: inserted for every channel≠0 Request, removed only in `tool_replied` for `Response | Error`); `subc/mod.rs:3754` (`remember_session_identity`), `subc/mod.rs:2645-2647` (only `purge_deleted_root_residents` prunes `session_identity`, `push_buffer` and `bg_wake_epoch`); `subc/push.rs:721-751` (`buffer_detached_reliable_push_frame`, up to `PUSH_BUFFER_MAX_PER_KEY` = 256 frames per (root, harness, session)).
- Hot path / frequency: each bg_events subscription (its stream ends with `StreamEnd`, which never removes the `unanswered_tools` entry), each request on a route torn down without a terminal, and each new agent session id.
- Problem: `unanswered_tools` leaks one entry per bg_events subscription, and the stall watchdog iterates the whole map every second (`warn_unanswered_tools`), emitting a spurious "unanswered past client deadline" warning for each. `session_identity` and `bg_wake_epoch` keep one entry per (root, session) ever seen. `push_buffer` keeps up to 256 reliable frames (BashCompleted with command plus 300-byte preview) for every detached session that never reattaches. None of these is pruned by idle-root eviction.
- Impact: slow unbounded growth (KBs to MBs over days of churn), plus O(map) work per watchdog tick and misleading warnings. Not a hot-path cost.
- Suggested fix: remove `unanswered_tools` entries on `StreamEnd` and on route teardown. Give retained session identities and push buffers a TTL (for example the bash completion retention window) and prune them on the maintenance tick, or when an idle root is evicted.
- Confidence: Medium.

##### [LOW] Deferred foreground bash waits poll through `spawn_blocking` every 100 ms each
- Location: `subc/bash.rs:1537-1548` (`run_deferred_bash_wait` select with `sleep(PENDING_POLL_INTERVAL)` = 100 ms), `subc/bash.rs:157-187` (`observe_deferred_bash_wait` → `tokio::task::spawn_blocking` → `registry.observed_status` + `wait_mode_detach_pending`, and for workers `renew_hard_kill`).
- Hot path / frequency: for every held foreground bash call, for its whole duration (`block_to_completion` can run up to 30 min). That is 10 blocking-pool hops per second per call.
- Problem: a `terminal_transition` Notify already exists, but the loop also polls at 10 Hz to catch detach requests and deadlines. Each poll takes the task state mutex, which the bash watchdog holds while persisting.
- Impact: a 30-minute build makes about 18k blocking-pool dispatches and registry lock acquisitions. Modest per call, but it scales with concurrent held calls.
- Suggested fix: compute the next deadline (promotion, remote hand-back, worker cap) and sleep until it. Signal wait-mode detach through a Notify, as terminal transitions are, and fall back to a 1 s safety poll.
- Confidence: Medium.

##### [LOW] gh shim spawns `git remote get-url origin` several times per invocation; the relay opens and closes a route per call
- Location: `gh_shim.rs:2191-2199` (`RungRecordProvenance::for_target`), `gh_shim.rs:3006-3044` (`resolved_agent_binding` / `target_or_checkout_binding`, reached from `determine_rung_for_target` 2561/2578, `governing_binding` 748, `unbound_write` 1126/1848), `gh_shim.rs:5719-5734` (`TargetRepository::resolve` → `repository_key_from_origin` → `origin_remote` spawns git, plus `project_root_for` canonicalizes and walks ancestors); `gh_shim_relay/mod.rs:650-746` (`route_request`: open route, request, then close when the last holder releases).
- Hot path / frequency: every `gh` invocation by an agent; every relayed bot write.
- Problem: when the command names no repository, each `repository_key(cwd)` call spawns git and walks ancestors. The R3 path reaches it 3-5 times per invocation with identical results. `resolve_manifest` is also called 2-3 times; signature verification is memoized thread-locally, but each call still re-reads and compares the manifest, last-valid and high-water files. On the daemon side, each relay call does open_route + request + close (3 round trips) per target, and up to two targets per write.
- Impact: roughly 10-40 ms of extra wall time per `gh` call (git fork/exec on macOS is about 5-10 ms each). That is small next to GitHub network latency but pure overhead.
- Suggested fix: memoize the origin remote and project root per process (a `OnceCell` keyed by cwd), or parse `.git/config` directly. Resolve the manifest once in `run` and pass it down. In the relay, keep routes open for a short idle period instead of closing after every call.
- Confidence: High (code paths). The impact is an estimate.

##### [LOW] Frame I/O details: per-frame body copy, scratch buffer freed after every large frame, BiLock split, and per-channel re-serialization of reliable Push
- Location: `subc/mod.rs:6158-6196` (`write_frame_contiguous`), `subc/mod.rs:3878` (`tokio::io::split(stream)` on a `TcpStream`), `subc/push.rs:276-305,346-360` (`try_send_push_frame` → `serde_json::to_vec` then `body.to_vec()` per channel), `subc/mod.rs:6198-6259` (reader reads directly from the unbuffered socket through `subc_transport::read_frame`).
- Hot path / frequency: every frame written or read.
- Problem: the writer memcpys every body into a scratch buffer just to prepend a header of about 20 bytes, and it drops the buffer whenever capacity exceeds 64 KiB, so every large response reallocates. `tokio::io::split` puts a BiLock between the reader and writer tasks, where `TcpStream::into_split` is lock-free. Reliable Push frames are serialized once per matching channel and then copied again by `body.to_vec()`. Lossy Push already shares one `Arc` body. Reads go through `read_frame` on the raw socket, likely 2+ syscalls per frame (implementation not vendored, not verified).
- Impact: small. The in-code comment notes egress write time measured at about 0. Listed for completeness.
- Suggested fix: use `write_vectored`/`write_all_buf` with a header + body chain, or keep a modest capped scratch buffer (for example shrink to 256 KiB rather than to zero). Use `into_split()`. Serialize reliable Push once and share an `Arc` body, as the lossy path does. Wrap the read half in a `BufReader`.
- Confidence: Medium.

##### [LOW] Deferred responses: every wake re-polls every pending entry
- Location: `subc/mod.rs:606-700` (`PendingSubcResponses::poll_if_woken` / `poll_ready`, `refresh_poll_interval`).
- Hot path / frequency: each frame-loop turn after any producer bumps the shared `DeferredResponseWake` generation, and every 100 ms while any entry has a poll interval (inspect / LSP navigation / run-scope waits).
- Problem: a single global generation wakes all entries. `poll_ready` calls `executor.actor_context()` (a scheduler lookup) and the entry's poll closure for each pending entry, then rebuilds the vector and recomputes the minimum interval. `register` and `cancel_request` do linear `retain` scans.
- Impact: O(pending) per wake. Negligible for a handful of entries, but it grows with concurrent inspect/LSP waits across sessions.
- Suggested fix: use per-entry wake flags (or a generation per root) and poll only woken or due entries, with a min-heap keyed by the next poll time.
- Confidence: Medium.

#### Area summary
The subc frame loop itself is carefully engineered: a dedicated reader task, a bounded writer with try-reserve, lock-free metrics, and blocking work moved to the executor or `spawn_blocking`. The serious costs sit in the **periodic background machinery around it**. The standing-roots feature reconciles on the 250 ms maintenance cadence. Each tick it re-resolves every standing root with git subprocesses, reopens and migrates aft.db, and opens an IMMEDIATE transaction per root; the DB open happens even when no standing roots are configured. It also resubmits a full strict search-index verification, or a full rebuild, for every unbound standing root with no "needs verify" gate, so those passes run back to back. Both are near-continuous CPU, disk and SQLite-lock load on any daemon with `index.roots` configured. Next is the 3 s health rollup, which runs whether or not anyone reads health. It computes memory attribution by walking every resident index (semantic chunks, symbols, trigram tables, inspect JSON) while holding those structures' read locks and the `lsp_manager` / callgraph-connection mutexes, and it measures allocator statistics twice per refresh despite a documented 60 s sampling cache. On the request path, keyed (call_key) tool calls go through two Mutating-lane ledger jobs that serialize reads behind writes and delay replies, with several redundant deep copies. The remaining items are low-severity churn (thread spawns per tick, 100 ms bash-wait polling, slowly growing per-session maps, repeated git spawns in the gh shim).

#### Things checked and found OK
- The reader task owns the socket and forwards whole frames over a channel, so `read_frame` is never cancelled mid-frame. The writer is single-consumer with a bounded queue and reliable or try-send variants. Header/body length is validated before write.
- Lossy Push fan-out serializes once and shares an `Arc<Vec<u8>>` body across channels. Lossy overflow coalesces to the latest frame per key, and `coalesce_push_batch` is O(n) with a HashMap.
- Reliable Push drain is budgeted (`RELIABLE_PUSH_DRAIN_BUDGET` = 32) with a yield. Per-key push and retry buffers are capped at 256. `CompletedTaskIds` is bounded at 4096.
- `DecodedFrame` memoizes the parsed body (`OnceLock`), so the request JSON is parsed once on the frame loop.
- Catalog schemas and JSON-schema validators are built once (`SERVED_TOOLS` `LazyLock`, validators in `OnceLock`).
- The drain tick is a pre-turn deadline check, so a biased select cannot starve it. The stall watchdog ticks at 1 s on its own thread and uses try_lock.
- `HealthRollupCache::snapshot` uses `try_read`, so the probe never waits on a refresh in progress. The refresh swaps an `Arc` under a brief write lock.
- `gh_shim_ticket` linear scans are intentional (constant-time comparison) and ticket counts are small. `gh_shim` manifest signature verification is memoized per thread. The bypass audit file is read only for `self-report`.
- The gh relay runs on its own task, off the frame loop, and holds no lock across network calls. The relay consumer connection is cached and reconnected only on failure.
- `consent.rs` waits are long-polls (`consent.await` with server-side timeout) plus a 100 ms local stop-signal check, which is cheap.
- `RouteBind` canonicalization on the frame loop is a single `ProjectRootId::from_path` per bind, which is acceptable.
- `subc_config::catalog_disabled_tools` reads the user config once at HELLO, not per call.
- `lifecycle_census` and `memory` census management requests are on-demand and therefore acceptable. The cost concern is only the 3 s rollup.

---

### 3.8 Inspect (diagnostics, dead code, duplicates, cycles)

Files reviewed (all paths under `crates/aft/src`):
- Read closely: `inspect/manager.rs` (Tier-2 reuse pipeline, read paths, scope filtering, rollups, completion routing), `inspect/cache.rs` (SQLite store, contribution-set hash, resolver/manifest fingerprinting), `inspect/scanners/duplicates.rs`, `inspect/scanners/cycles.rs`, `inspect/oxc_engine/mod.rs`, `inspect/oxc_engine/resolver.rs`, `inspect/entry_points.rs` (walks), `inspect/scanners/dead_code.rs` (incremental rollup, materialize, fragments), `commands/inspect.rs` (`handle_inspect_payload`, stat verification, Tier-2 thread fan-out and waits).
- Skimmed: `inspect/oxc_engine/graph.rs`, `inspect/tier2_scheduler.rs`, `inspect/phase_log.rs`, `inspect/job.rs`, `inspect/dispatch.rs`, `inspect/freshness.rs`, `inspect/generated.rs`, `inspect/scanners/{todos,metrics,complexity,source_text,duplicates_classifier,unused_exports}.rs`, `inspect/diagnostics_category.rs`, `inspect/scoped_diagnostics_sweep.rs`.
- Skipped as test-only: `view_projection_tests.rs` and the `#[cfg(test)]` modules.

#### Findings

##### [CRITICAL] Duplicates: every file's fragment hashes are stored as JSON Values, and the whole set is loaded and deserialized several times on every incremental change
- Location: `inspect/scanners/duplicates.rs:333` (`collect_fragments`), `:512` (`file_scan_to_contribution`), `:546` (`aggregate_duplicate_contributions_with_limit`). Also `inspect/manager.rs:5476` (`duplicates_contributions_need_fact_refresh`), `:3778` (`load_contributions` before the rollup), and `:4059` (`route_tier2_reuse_completion`).
- Hot path / frequency: every Tier-2 duplicates pass that has any change: a watcher-driven background refresh, or every blocking `aft_inspect` after an edit.
- Problem:
  - `collect_fragments` emits one `DuplicateFragment` for every named node whose subtree cost falls in `20..=7000`. That is roughly every statement or expression above about 20 nodes, so hundreds to thousands per file.
  - Each fragment is a JSON object holding a 64-character hex hash string plus three numbers. It is stored per file as a serde `Value` blob in `tier2_contributions`.
  - After even a single-file edit, `tier2_run_with_reuse_job` does the following:
    1. `apply_contribution_updates_for_config` writes the change, which produces a new set hash, so the aggregate lookup misses.
    2. `duplicates_contributions_need_fact_refresh` calls `load_contributions` and `serde_json::from_slice`s every file's blob, only to check for `line_count`.
    3. `load_contributions` runs again for the rollup.
    4. `aggregate_duplicate_contributions_with_limit` walks every fragment twice: once in the `fragment_counts` HashMap and once in `by_hash`.
  - The full contribution vector is then returned in `InspectScanSuccess.contributions`, which the reuse path never uses. `route_tier2_reuse_completion` does `result.outcome.clone()`, which deep-clones that full vector again. At peak there are two complete in-memory copies of every fragment.
- Impact: in-memory `Value` cost is about 300–400 B per fragment (Map, 4 entries, 64-byte String). Example: 10k files at about 400 fragments each is 4M fragments, which is about 1.5 GB per copy and about 3 GB at peak. That means seconds of JSON parsing and possible OOM on a large monorepo, on every edit-triggered refresh. The O(total fragments) cost is paid even though only a single file changed.
- Suggested fix:
  - Store fragments compactly: a binary blob of `[u8;32]` hash plus u32 lines and cost, or a `fragment_hashes(file, hash, start, end, cost)` table with an index on `hash`.
  - Keep a persistent hash→count table. Then an incremental update only touches the changed files' hashes, and the rollup only needs fragments whose count is 2 or more (`SELECT ... WHERE hash IN (SELECT hash ... GROUP BY hash HAVING count(*)>=2)`).
  - Drop the version-probe full load. Store a facts-format version per row or in `tier2_meta`.
  - Do not return contributions from the reuse path, and do not clone `result.outcome` in `route_tier2_reuse_completion`. Take the aggregate by value instead.
- Confidence: High (code path verified; byte sizes are estimates)

##### [HIGH] Dead-code rollup: per-file linear scans over the whole reachable set and the aggregate item arrays (O(files × (reachable + items)))
- Location: `inspect/scanners/dead_code.rs:1160` (`reachable_symbols_for_file`), `:833-841` (the `fragments_match` check in `aggregate_dead_code_contributions_incremental`), `:1169-1232` (`fragments_from_aggregate`, closure `items_for_file` and `by_language`).
- Hot path / frequency:
  - `fragments_match` runs on every dead-code rollup that has a previous state. That is every incremental pass.
  - `fragments_from_aggregate` runs over `rendered`. That is all files on a cold or full rollup, and for every scoped read (via `aggregate_dead_code_contributions_with_snapshot` with `previous=None`), and the affected files on an incremental rollup.
- Problem:
  - `reachable_symbols_for_file` iterates the whole `BTreeSet<ExportNode>` and filters by `node.0 == file`. It is called twice per file (the all and production sets).
  - `fragments_from_aggregate` scans `aggregate["items"]` twice, plus `generated_items`, `test_only_items` and `uncertain_items`, once per file. It also `.cloned()`s matching items.
  - `ExportNode` is `(String, String)` ordered by file first, so a range query would be O(log R + k).
- Impact: with F = 10k files, R = 100–300k reachable export nodes and I = 5k items, this is about 10k × (2·200k + 5·5k), roughly 4e9 string comparisons, in the rollup. That is tens of seconds of CPU on a cold or full rollup and on every scoped dead-code read. The incremental `fragments_match` path alone is about 2·F·R.
- Suggested fix:
  - Use `reachable.range((file.to_string(), String::new())..)` with `take_while(|n| n.0 == file)`, or group reachable nodes by file once (`BTreeMap<&str, BTreeSet<&str>>`).
  - Bucket the aggregate items by `file` once (`HashMap<&str, Vec<&Value>>`) before the per-file map.
- Confidence: High

##### [HIGH] Every incremental Tier-2 change reloads and deserializes all contributions 2–3 times, deep-cloning each one, and then discards them
- Location: `inspect/manager.rs:3555-3570` and `:3612-3627` (the hash-hit return paths call `load_contributions`), `:3576-3590` (the three `*_need_fact_refresh` probes), `:5433-5508` (each probe calls `load_contributions(cache, job)` and then `serde_json::from_value::<T>(contribution.contribution.clone())`), `:3778` and `:3793` (the rollup loads, plus a reload after `fill_missing_dead_code_facts`). Rollup parsers use `from_value(x.clone())` at `manager.rs:5667`, `scanners/cycles.rs:167`, `scanners/dead_code.rs:1364` and `scanners/complexity.rs:366`.
- Hot path / frequency: every background or blocking Tier-2 pass, for each of the five categories, whenever any file changed.
- Problem:
  - The incremental scan is per-file, but every rollup starts with `load_tier2_contributions`. That is a full table read plus `serde_json::from_slice` of every blob.
  - On the hash-miss path it is done once for the "needs fact refresh" probe and once more for the rollup. Dead code adds a third load after a fill.
  - Each probe and rollup deep-clones the `Value` before `from_value`.
  - On the hash-hit paths the contributions are loaded only to be returned in `InspectScanSuccess`, which reuse callers ignore (only `aggregate` is used in `route_tier2_reuse_completion`). They are then cloned again via `result.outcome.clone()`.
- Impact: for a one-file edit, cost is O(project contribution bytes) × 3–5 for each category. On large repos that is hundreds of MB of JSON parsing per pass. The cost lands on the blocking `aft_inspect` latency and on background CPU.
- Suggested fix:
  - Load contributions once per pass and pass the vector through.
  - Detect stale fact versions with a per-row `facts_version` column, or one meta row, instead of a full load.
  - Use `T::deserialize(&value)` (`&Value` implements `Deserializer`) instead of `from_value(value.clone())`.
  - Stop returning or loading contributions on reuse hits (`contributions: Vec::new()`).
  - Change `route_tier2_reuse_completion` to consume `result` rather than clone `outcome`.
  - Longer term: keep per-category parsed contributions in memory, keyed by contribution-set hash, and apply deltas.
- Confidence: High

##### [HIGH] Scoped `aft_inspect` re-runs a full, uncapped project rollup from every contribution, for each Tier-2 category, on every call
- Location: `inspect/manager.rs:6365` (`filter_outcome_for_scope_with_contributions`), `:5589` (`scoped_tier2_payload_from_contributions`), `:5608` (`scoped_tier2_rollup_job`). Reached from `commands/inspect.rs:863` (`tier2_read_cached_readonly`) for scoped requests and from `wait_for_tier2_reuse`.
- Hot path / frequency: every scoped inspect request (a common agent call), once for each of up to five Tier-2 categories.
- Problem:
  - For a non-project-wide scope, each category walks the whole project (`scope_files` via `walk_project_files`) and loads all contributions.
  - It then runs the full rollup with `drill_down_limit = None`, and only afterwards filters to the scope.
  - For dead code it also calls `build_tier2_callgraph_snapshot` and the full `aggregate_dead_code_contributions_with_snapshot` (`previous = None`). That triggers the O(F·R) fragment loops above, plus `resolve_entry_points` and `resolve_project_roles` walks.
  - For cycles and unused exports, the rollup re-runs project-wide module resolution (see the oxc finding below).
  - The read path has already paid `get_aggregated_for_config`, whose contribution-set hash does tree walks, and `tier2_cached_aggregate_is_fresh`, which does a full walk and stats every cached file.
- Impact: a scoped request on a one-file directory costs as much as a full cold rollup of every category. That is many seconds on large repos for what should be a cheap filter.
- Suggested fix: persist per-file finding fragments, which dead code already has as `DeadCodeFileFragment`, and an indexed item table. Answer scoped reads by selecting only the in-scope files' findings from the cached project aggregate or fragments. Cache the uncapped project payload in memory, keyed by contribution-set hash, so repeated scoped calls reuse it.
- Confidence: High

##### [HIGH] `contribution_set_hash` walks the whole project tree (1–2 times) and reads files while holding the cache's SQLite connection mutex, and is computed several times per pass and per read
- Location: `inspect/cache.rs:2061` (`contribution_set_hash_with_version`), `:2390` (`update_manifest_fingerprint_hash`, which calls `entry_points::collect_entry_point_manifests`, a full `ignore` walk), `:2120`/`:2182` (`update_resolver_config_fingerprint_hash`, which calls `walk_resolver_config_files`, a second full walk, for DeadCode, UnusedExports and Cycles). Callers: `get_aggregated_with_config` (`cache.rs:739`, which holds `self.conn` across the hash), `apply_contribution_updates_with_config` (`:1022`, inside the write transaction), `store_tier2_result_with_config` (`:900`), and `contribution_set_hash_with_config` (`:1323`). Manager calls are at `manager.rs:2896`, `:3284`, `:3355`, `:3513`, `:3540` and in `fill_missing_dead_code_facts`.
- Hot path / frequency:
  - On every Tier-2 pass (quick-reuse check, the update transaction, the aggregate lookup) for each category.
  - On every `tier2_read_cached*` call for each category.
  - For dead code, 2–3 times per pass.
- Problem:
  - Each hash does a full `SELECT file_path, file_hash` scan, a single-threaded gitignore-aware walk of the whole repo to find `Cargo.toml`/`package.json`/`go.mod`, and `fs::read` plus blake3 of each manifest.
  - Three categories do another full walk for `tsconfig*.json`, plus resolution of `extends` (canonicalize, `node_modules` probing).
  - It runs while `self.conn` (a `Mutex<TrackedConnection>`) is locked. Inside `apply_contribution_updates` it runs inside an open write transaction. All other users of the cache serialize behind that.
- Impact: on a 50–100k-file repo each walk takes about 0.3–2 s. A no-op refresh of five categories spends roughly 5–8 full walks just on fingerprinting, and other categories' DB access is blocked during them.
- Suggested fix:
  - Compute the manifest and resolver-config fingerprint once per pass, or per snapshot, outside the DB lock, and pass it in. It could also be derived from the `scope_files` walk the pass already did (filter by file name).
  - Invalidate it on watcher events for manifest or tsconfig names instead of re-walking.
  - Split the hash so the DB part (file_path, file_hash) is computed under the lock and the filesystem part is computed outside.
- Confidence: High

##### [HIGH] Many more whole-repo tree walks per Tier-2 pass and per inspect: `resolve_entry_points` is uncached and called repeatedly
- Location: `inspect/entry_points.rs:100` (`resolve_entry_points`, which does `collect_manifests`, then `collect_conventional_executable_root_entry_points`, then possibly `collect_fallback_entry_points`; each calls `entry_point_walk_files`, a full sorted walk, and each package with `exports["./*"]` gets an extra subtree walk at `:445`), `:961` (`resolve_project_roles`, another walk). Call sites per pass:
  - `manager.rs:2484` (`oxc_result_for_scan`: every oxc category, every scan, including `fill_missing_dead_code_facts`)
  - `scanners/dead_code.rs:5737` (`collect_public_api_files`)
  - `:1383` and `:1666` (materialize, `oxc_verdicts_by_file`)
  - `manager.rs:6358` and `:5892` (unused-exports rollup, called twice)
  - `scanners/unused_exports.rs:94` and `:256`
  - `manager.rs:4314` (`scope_files`): one more full walk per category per pass, plus per read
- Hot path / frequency: every Tier-2 pass (dead_code, unused_exports and cycles especially) and every scoped read.
- Problem: the same project-wide file list is enumerated about 10–15 times per dead-code pass: scope walk, 2–3 entry-point walks in oxc analysis, 2–3 in `collect_public_api_files`, 1 in roles, 2–3 in materialize, plus the fingerprint walks above. None of it is memoized, although the inputs (manifest contents) rarely change.
- Impact: O(walks × files) syscalls and gitignore matching. That is seconds per pass on large monorepos and dominant for small edits.
- Suggested fix: build one `ProjectInventory` per pass (or per snapshot generation): the file list, manifest paths and hashes, and tsconfig paths. Derive `EntryPointSet`, `ProjectRoles` and the fingerprints from it. Memoize `resolve_entry_points` keyed by the manifest fingerprint.
- Confidence: High

##### [HIGH] Oxc analysis holds a global mutex for the entire analysis (I/O, parse, resolve, verdicts), and parsing is serial
- Location: `inspect/manager.rs:2468-2508` (`oxc_result_for_scan`: `self.oxc_facts_cache.lock()` is held across `analyze_files_with_cache`), `inspect/oxc_engine/mod.rs:218-241` (serial `for (idx, path) in files.iter().enumerate()` with `fs::metadata`, `read_to_string` and `parse_file_facts`).
- Hot path / frequency: every dead_code, unused_exports and cycles scan. An explicit inspect starts all Tier-2 categories concurrently on separate threads (`commands/inspect.rs:777`).
- Problem:
  - The three oxc categories serialize on `oxc_facts_cache` for their full duration, including module resolution and `compute_verdicts`, which do not need the cache.
  - On a cold or forced rescan every TS/JS file is read and parsed on one thread while the inspect rayon pool sits idle.
  - `rebind_facts` deep-clones the cached `FileFacts` on every hit.
- Impact: cold analysis of N TS files takes about N × (1–3 ms) on one core (10k files is 10–30 s). The concurrent category passes then wait in line, so a blocking inspect sees roughly the sum, not the max.
- Suggested fix:
  - Take the lock only to look up and insert entries.
  - Parse misses in parallel (`par_iter` over the files not in the cache, then insert under the lock).
  - Store `Arc<FileFacts>` to avoid deep clones.
  - Run resolution and verdicts outside the lock.
- Confidence: High

##### [HIGH] Cycles and unused-exports rollups redo project-wide module resolution from scratch on every change, deep-cloning all facts, with a fresh resolver
- Location: `inspect/scanners/cycles.rs:444-487` (`CycleGraph::from_oxc_facts`: clones every field of every cached fact, then `analyze_file_facts`), `inspect/manager.rs:5850-5900` (`roll_up_unused_exports_oxc_contributions`: same pattern), `inspect/oxc_engine/resolver.rs:130-153` (`ModuleResolver::new`: `fs::canonicalize` per file, new `oxc_resolver::Resolver` with a cold fs cache each call), `:155-254` (`resolve_modules`: serial, `file_facts.clone()` again into `ResolvedModule`, a fresh `ResolverPassCache`).
- Hot path / frequency: every cycles and unused-exports rollup, meaning every pass with any change, plus every scoped read (see the scoped-read finding).
- Problem:
  - The incremental per-file scan resolves only the changed files, but the rollup throws that away. It rebuilds `FileFacts` for all files (two deep copies), canonicalizes every path (a syscall per file), and re-resolves every import specifier with a brand-new resolver. That repeats filesystem probes for `package.json`, tsconfig and extension candidates.
  - It then recomputes all verdicts.
  - The two categories do this independently over the same facts.
- Impact: O(files + imports) syscalls and resolution work per rollup per category. That is seconds on large TS monorepos for every single-file edit.
- Suggested fix:
  - Persist the resolved edges per file, which cycles already stores as `edges`, and re-resolve only the changed files plus files whose specifiers could now point at added or removed files. Invalidate everything only on a resolver-config fingerprint change.
  - Share one resolution result between cycles and unused_exports in a pass.
  - Keep a long-lived `oxc_resolver::Resolver` per root and clear its cache on config change.
  - Avoid the double `FileFacts` clone (borrow or use `Arc`).
- Confidence: Medium-High

##### [MEDIUM] Blocking inspect walks and stats the entire project twice, serially, in addition to the per-category walks
- Location: `commands/inspect.rs:277` (`capture_inspect_root_stats_until`) and `:309` (`verify_final_root_stats`). Per-category `scope_files` walks happen in each Tier-2 thread.
- Hot path / frequency: every blocking `aft_inspect`.
- Problem: a full `walk_project_files` plus `fs::metadata` per file at the start and again at the end, on the request thread, collected into a `Vec` and sorted. The same list is walked again independently by each of five Tier-2 workers and by the fingerprint and entry-point walks above.
- Impact: 2 × N stats, plus walk overhead, are added straight to user-visible latency (about 1–2 s for 100k files).
- Suggested fix: walk once per request and share the inventory with the Tier-2 jobs (`job.scope_files`). Use parallel stat (an `ignore` parallel walker or rayon), or rely on watcher-driven change counters / a journal generation to detect mid-request changes instead of a second full stat sweep.
- Confidence: High

##### [MEDIUM] Cycle chain reconstruction runs one BFS per SCC member, with String clones and BTree containers
- Location: `inspect/scanners/cycles.rs:591-655` (`report_for_component`, `shortest_path_in_component`). Also `strongly_connected_components` and `finish_order` (`:538-589`) clone a String per visit.
- Hot path / frequency: every cycles rollup.
- Problem: for an SCC of k files, `report_for_component` runs k BFS traversals, each O(V_c + E_c) with `BTreeSet<String>`/`BTreeMap<String,String>` and a `String` clone per node. Barrel-heavy TS projects commonly have one SCC with hundreds to thousands of files.
- Impact: k = 2,000 and E_c = 20k gives about 4e7 BTree operations and allocations, roughly seconds per rollup.
- Suggested fix:
  - Intern file names to `u32` ids and use `Vec` adjacency.
  - Build the chain with one DFS or Euler-style walk (or one BFS tree from the first node plus back-edges) instead of k independent BFS runs.
  - Cap the chain length reported for very large SCCs.
- Confidence: Medium (depends on SCC size in practice)

##### [MEDIUM] `OxcFactsCache` grows without bound between idle evictions
- Location: `inspect/oxc_engine/mod.rs:36-150` (`entries_by_hash` keyed by `v{ver}:{type}:{content_hash}` and never pruned), `inspect/manager.rs:2024` (cleared only in `evict_idle_caches`).
- Hot path / frequency: every edit to a TS/JS file adds a new entry. The old content's entry stays forever while the daemon is active.
- Problem: in a long session with continuous edits, every historical version's full `FileFacts` (exports, imports, binding sets) is retained. `entries_by_path` is also never pruned for deleted files. `estimated_memory` reports an `oxc_fact_bytes` gap, so this is invisible.
- Impact: slow memory growth proportional to edit count × facts size. Medium in long-running sessions.
- Suggested fix: when `entries_by_path` is updated for a path, drop the old `cache_key` if no other path references it (refcount). Prune `entries_by_path` to the current file set after each full analysis.
- Confidence: High

##### [MEDIUM] Export-graph resolution is not memoized; `has_local_export` is a linear scan
- Location: `inspect/oxc_engine/graph.rs:925-966` (`resolve_export_name`), `:968-1059` (`visible_export_resolutions`), `:1061` (`has_local_export`).
- Hot path / frequency: called per import or name in `record_reference_origins`, `apply_imports`, `apply_entry_reachability` and the seeding passes. That is several passes over all imports per analysis, and analysis runs per rollup (see the cycles/unused-exports finding).
- Problem:
  - Every `import * as X` and `export *` usage recomputes the full star-re-export closure of the target, with a fresh `visited` set.
  - Each named lookup walks the forwarding chain again, calling `has_local_export`, which is O(exports of file) per hop.
  - Barrel files mean the same closures are recomputed for every importer.
- Impact: O(importers × barrel closure size), which can reach millions of operations on barrel-heavy projects. Medium.
- Suggested fix: memoize `visible_export_resolutions(target, include_default)` and `resolve_export_name(target, name)` per analysis (compute once; cycles are handled by visited markers). Index local exports by name in a `HashSet`/`HashMap` per file.
- Confidence: Medium

##### [LOW] `get_aggregated_with_config` clones the full cached aggregate before checking whether it is still valid; aggregates are uncapped
- Location: `inspect/cache.rs:774-786`. Rollups store `drill_down_limit=None` aggregates (`manager.rs:5552`).
- Hot path / frequency: every Tier-2 read and pass.
- Problem: `.get(key).cloned()` deep-copies the `MemoryAggregate` payload (all findings for dead code and duplicates), and the copy is thrown away if the hash mismatches. On a hit it is cloned again by callers (`aggregate.clone()` in `store_tier2_aggregate`, `DeadCodeRollupState`).
- Suggested fix: store `Arc<Value>` in the memory map, compare the hash under the read lock, and clone the Arc.
- Confidence: High

##### [LOW] SQLite loops re-prepare the same statement for every row
- Location: `inspect/cache.rs:862-899` and `:964-1019` (`tx.execute(...)` inside `for contribution in contributions` / `updates.upserts` / `deletes` / `metadata_updates`). Also `conn.prepare` in `load_tier2_contributions`, `contribution_freshness` and `contribution_set_hash_with_version`.
- Hot path / frequency: a cold build upserts one row per project file, and incremental passes upsert the changed files.
- Problem: rusqlite's `Connection::execute` prepares the SQL on every call. For 10k-row cold stores that is 10k prepares. The transactions themselves are correct.
- Suggested fix: use `tx.prepare_cached(...)` once before each loop. Use `prepare_cached` for the repeated read queries too.
- Confidence: High (impact modest)

##### [LOW] Dead-code item ranking recomputes the sort key, with two String allocations and a role lookup, on every comparison
- Location: `inspect/scanners/dead_code.rs:1235-1251` (`rank_dead_code_items`).
- Hot path / frequency: every dead-code rollup, four times (headline, generated, test-only, uncertain).
- Problem: `sort_by` builds `(role_for(file), file.to_string(), line, symbol.to_string())` for both sides of each comparison, which is O(n log n) allocations.
- Suggested fix: use `sort_by_cached_key`, or borrow `&str` in the key.
- Confidence: High

##### [LOW] Minor overheads on the explicit-inspect path
- `acquire_interactive_tier2_permit_slot` (`manager.rs:938`) holds the `interactive_tier2_permits` mutex while blocking on cold-build admission. This is intentional for one root, but it serializes unrelated waiters on that manager.
- `tier2_read_cached_readonly` (`manager.rs:2845`) opens a new read-only SQLite connection and read marker for each category on each call.
- `run_tier2_pass_with_deadline` (`manager.rs:483`) spawns a timer thread per pass, and `commands/inspect.rs:777` spawns one OS thread per Tier-2 category per request.
- `Tier1FileMemo` (`cache.rs:379-440`) takes a single global mutex twice per file from rayon workers and clones the memo value (todo item vectors) on each hit.

None of these is significant alone.
- Confidence: Medium

#### Area summary
The Tier-2 incremental machinery is per-file at the scan stage, but every stage after that is project-wide on each pass:
- every contribution is reloaded and deserialized (2–3 times, with deep clones),
- project-wide module resolution is redone for cycles and unused exports,
- dead-code fragment bookkeeping does O(files × reachable) scans,
- the repo is walked 10–15 times per pass (scope, entry points, roles, manifest and tsconfig fingerprints), partly under the SQLite connection lock.

Duplicates is the worst case for memory. It persists every subtree hash as JSON, so a one-file edit loads the whole fragment corpus, at GB scale on large repos, and the completion path clones it again. Scoped inspect requests pay for a full uncapped project rollup per category. The highest-value fixes are:
1. A compact, indexed duplicate-fragment store with a hash→count table.
2. Loading contributions once per pass and never returning or cloning them on reuse.
3. One cached project inventory and entry-point/fingerprint set per pass, computed outside DB locks.
4. Range queries or bucketing in the dead-code fragment code.
5. A narrower oxc cache lock plus parallel parsing.
6. Scoped reads served from persisted per-file fragments.

#### Things checked and found OK
- `tier2_scheduler.rs`: a pure state machine (debounce, ceiling, min interval, cold pacing), with no polling or locks of its own.
- `phase_log.rs`: small `parking_lot` mutex with a bounded retained-log ring. Not hot.
- The duplicates nesting collapse (`surfaced_duplicate_hashes`) uses a sorted interval stack, O(n log n) per file. `merged_interval_line_count` is fine.
- Duplicates `collect_fragments` reuses a thread-local parser per language and a hash scratch buffer, and has a recursion-depth guard.
- SCC detection (Kosaraju, iterative) is linear apart from String cloning. Only the chain reconstruction is costly.
- SQLite writes are batched in transactions, WAL with `synchronous=NORMAL`, and primary keys cover all lookups.
- Tier-1 todos and metrics scan in parallel with per-file freshness memo (stat-only hit path), with bounded LRU and prune-to-scope.
- `commands/inspect.rs` Tier-2 waits (`receive_tier2_completion_until`) use `recv_timeout` slices with deadline and cancellation, not busy-waiting. The root-quiescence wait is a 50 ms sleep loop bounded by the phase deadline, which is acceptable.
- `scoped_diagnostics_sweep` caps opened files (`SCOPED_SWEEP_FILE_CAP = 1000`) and pulls in windows.
- The per-pass freshness verification is stat-only for unchanged files (hash only on mtime or size mismatch).

---

### 3.9 Views, blob store, GC, storage retention, migration

Files reviewed (non-test code, all under `crates/aft/src`): `views/` (mod.rs, assembly.rs, assembly_seed.rs, generation.rs, materialization.rs (skim), read.rs, snapshot.rs, live_delta.rs, first_load.rs, semantic.rs, semantic_runtime.rs, trigram.rs, intent.rs, eviction.rs, registry.rs, manifest_v2.rs, segment_store.rs, callgraph.rs, parent/worker.rs; the rest skimmed by grep), `blob_store/mod.rs`, `blob_store/v2.rs`, `pins/mod.rs`, `pins/live.rs` (skim), `gc/mod.rs`, `gc/family.rs`, `refresh/mod.rs`, `path_status/mod.rs`, `alias/mod.rs`, `migration/mod.rs`, `migration/per_checkout.rs` (skim), `migration/prune_legacy.rs` (skim), `migrate_storage.rs` (skim, one-shot CLI), `storage_retention.rs`, `legacy_partitions.rs`, `production_storage.rs`, `private_storage.rs`, `persisted_format.rs`, `durability.rs`, `disk_write_hunt.rs` (bench only). I also followed the call sites in `context.rs` (`prepare_view_paths`, `record_checkout_watcher_change`), `executor/view_publication.rs`, `runtime_drain.rs` and `commands/configure.rs`.

Context: everything under `views/` and `blob_store/` runs only when `views.enabled = true`, which is off by default (ARCHITECTURE.md, config.rs:583). The severities below assume the feature is on. Each one would be a step lower for the default install, where none of this code runs.

#### Findings

##### [HIGH] Every view publication serializes, re-parses, re-reads and re-hashes the whole semantic index, whatever changed
- Location: context.rs:7314-7337 (`prepare_view_paths`), migration/mod.rs:281-354 (`store_live_semantic_blobs`)
- Hot path / frequency: every view publication when semantic search is on and the root is not read-only. That includes the watcher-driven one (runtime_drain.rs:3138, `publish_view_if_quiet`, one per quiet watcher batch), the semantic-ready republish and the lazy callgraph build. `changed_paths` is ignored here.
- Problem: `self.semantic_index.read().clone()` deep-clones `SemanticIndex`. The field is `RwLock<Option<SemanticIndex>>` (context.rs:3390), and the struct holds `entries: Vec<EmbeddingEntry>` with every vector and several HashMaps. The code then calls `index.to_bytes()` (a full serialization of every vector), `parse_legacy_snapshot(&raw)` (a full parse back), and for every indexed file `fs::read(source)` plus `blake3::hash` plus `regular_mode` (a stat). It then encodes a payload, writes an `AssemblyPin` keys file listing every key, and calls `BlobStore::put` once per file. Each put is its own `BEGIN IMMEDIATE` transaction under the process-wide blob durability mutex (blob_store/mod.rs:563-604), and almost all of them come back `Reused`. One edited file therefore costs a full pass over the repository's semantic data.
- Impact: O(index size) memory, about 3x the index at peak (clone, serialized buffer, parsed copy), plus N file reads, N BLAKE3 hashes and N SQLite transactions per publication. With 100k chunks × 384 floats the index is about 150 MB, so each watcher batch costs hundreds of MB of allocation and seconds of I/O. This is a background worker, but it holds a `cold_build_limiter` permit for the whole time.
- Suggested fix: restrict the work to `changed_paths` plus the paths still semantic-pending in the previous manifest, and reuse the previous manifest's `semantic` keys for everything else. Iterate the index's entries in place under the read lock instead of cloning and round-tripping through `to_bytes`/`parse_legacy_snapshot`. Use the index's own `file_hashes` instead of re-reading files. Batch the puts in one transaction.
- Confidence: High

##### [HIGH] `LiveDelta::apply` does a linear `journal.retain`, so bulk delta rebuilds are O(N²); write-intent handling rebuilds the delta for every path
- Location: views/snapshot.rs:267-293 (`LiveDelta::apply`); callers views/first_load.rs:183-209 (`SiblingLoader::reconciled`), first_load.rs:1041-1058 (`CheckoutDriver::record_change`), first_load.rs:2096-2125 (directory `WriteIntent`)
- Hot path / frequency:
  - `reconciled` runs on every checkout-semantic refresh that needs a reload (semantic_runtime.rs:206-214), which follows every watcher or AFT write wake-up.
  - `record_change` runs for every watcher event (context.rs:7909) and twice for every AFT write: write, edit, apply_patch and undo call `intent::record_paths`, which notifies in both the Before and After phases.
- Problem:
  - `apply` runs `self.journal.retain(|r| r.rel_path != rel_path)` before each push, which is O(journal length) per call.
  - `reconciled` builds a fresh `LiveDelta` and applies every walked checkout entry. When the base is the `"empty"` generation (first load with no sibling) or a sibling on another branch, every differing path is journaled, so the total is N²/2 RelPath comparisons. The journal of this temporary delta is then thrown away, because `snapshot()` does not carry it.
  - `record_change` rebuilds a new `LiveDelta` from every live entry and re-inserts every pending intent, on each event: O(K²) for K live entries and O(I log I) for I intents.
  - A `WriteIntent::Directory` calls `record_change` once per member of `snapshot.membership()`. Each call copies the growing intent set, so the total is O(N²) intent inserts plus O(N·K²) journal work.
- Impact: first load of a 100k-file root with no sibling costs about 5·10⁹ path comparisons, roughly 10-30 s of CPU. A directory-level intent on a large root can effectively hang the actor. Steady-state edits cost O(K²) per event, where K is the number of files that changed since the last generation (thousands after a branch switch).
- Suggested fix: make `journal` a `BTreeMap<RelPath, JournalRecord>` (or an index map), or skip journaling entirely in a bulk-construction path (`LiveDelta::from_entries(base, iter)`) that builds `entries` directly. In `record_change`, mutate the installed delta's `intent` map instead of rebuilding the whole delta. For directory intents, insert one subtree-intent marker instead of N per-path calls.
- Confidence: High

##### [HIGH] Path-status annotations are written as one autocommit per path with `synchronous=FULL`, so every pending path costs an fsync on every publication
- Location: path_status/mod.rs:139-157 (`open_at`: WAL is set, `synchronous` is not), 167-180/309-330 (`mark_pending` → `upsert` via `Connection::execute`); caller views/assembly.rs:562-572
- Hot path / frequency: every view publication. `semantic_pending_paths` contains every semantic-eligible candidate without a semantic key, including unchanged ones reused from the previous manifest (assembly.rs:445-470). That is the whole repository whenever `semantic_keys` is empty: on read-only/borrow-only roots (`allow_semantic_blob_put` false, context.rs:7314) and in every state where the semantic handoff produces no keys.
- Problem: each `upsert` is its own implicit transaction. In WAL mode with SQLite's default `synchronous=FULL`, that means one WAL fsync per row. `Connection::execute` also re-prepares the statement each time. `clear_many` (assembly.rs:583) correctly uses a single transaction, but `mark_pending` does not.
- Impact: N fsyncs per publication, for example 20k semantic files × about 1-5 ms each, which is 20-100 s per publication on a borrow-only worktree. A watcher-driven publication loop on such a root keeps the disk busy.
- Suggested fix: add a `mark_pending_many` that wraps all upserts in one transaction with `prepare_cached`, and set `PRAGMA synchronous=NORMAL` on this rebuildable annotation store. Better still, skip the upsert when the row already has the same state and reason.
- Confidence: High

##### [HIGH] Blob GC runs a full `VACUUM` on v1 blob stores after any deletion, aborts it at 60 s and retries every pass
- Location: gc/mod.rs:178-209 (`sweep_planes_bounded` reclaim loop), 429-451 (`reclaim_plane`); blob_store/mod.rs:106-121 (schema, no `auto_vacuum`)
- Hot path / frequency: the storage-retention pass (every 10 min, storage_retention.rs:36, `collect_one_family`), configure maintenance (commands/configure.rs:6847) and migration.
- Problem: v1 blob databases never set `auto_vacuum`, so `PRAGMA auto_vacuum` is 0. `force = report.deleted_blobs > 0 || pending.exists()`, and with `force` the 128 MiB free-space threshold is skipped. Any deletion of even one blob therefore triggers a full `VACUUM`, which rewrites the entire multi-GB file and takes an exclusive lock that stalls `BlobStore::put` for the whole time (busy timeout). The progress handler aborts it at the 60 s deadline, which throws away all the work done. The `retention/vacuum/<id>.json` marker then forces another full `VACUUM` attempt on the next pass, so a large store never finishes and pays the full I/O cost every 10 minutes.
- Impact: repeated multi-GB rewrite I/O and writer stalls in the background on any family store larger than about 60 s of VACUUM throughput.
- Suggested fix: create v1 stores with `auto_vacuum=INCREMENTAL` (as v2 does, blob_store/v2.rs:968) and migrate existing ones once. Use `PRAGMA incremental_vacuum(N)` in bounded steps. Keep the 128 MiB threshold even when `force` is set, and never start a full `VACUUM` that cannot finish within the deadline.
- Confidence: High

##### [MEDIUM] Each publication runs `git ls-tree -r HEAD` and `git check-attr` over every path twice
- Location: context.rs:7302 (`prepare_view_paths` → `head_tree_entries`), views/assembly.rs:270 (`prepare_checkout_deferring` → `head_tree_entries` again); alias/mod.rs:778-805, 895-958
- Hot path / frequency: every view publication (watcher batches, semantic-ready, lazy builds, configure).
- Problem: both call sites spawn `git ls-tree -r -z HEAD` and then `git check-attr --cached -z --stdin filter`, which pipes every tracked path. Neither result is cached, even though HEAD has not moved between the two calls a few milliseconds apart. `views/parent/worker.rs:520-531` already has a `head_probe` cache (HEAD/ref/packed-refs stat) that would avoid this.
- Impact: 4 subprocesses per publication, with O(N) output and parse and an O(N) `BTreeMap` of attributes. That is about 100-500 ms per publication on a 100k-file repository, and it is pure duplication.
- Suggested fix: compute `head` once in `prepare_view_paths` and pass it into `AssemblyRequest`, and cache it per root keyed on the `head_probe` stamps, the way the parent worker does.
- Confidence: High

##### [MEDIUM] Full-rebuild assemblies re-read and re-hash every tracked file, retain all of it in memory, extract serially, and commit one transaction per file
- Location: views/assembly.rs:302-417 (candidate loop), 497-545 (put loop), 1412-1443 (`missing_callgraph_payload`); alias/mod.rs:244-301 (`seed_proven_alias`); blob_store/mod.rs:563-604 (`put`)
- Hot path / frequency: whenever `changed_paths` is empty. That covers initial and configure publications (configure.rs:7778), every lazy on-demand callgraph build (context.rs:8124 schedules with `BTreeSet::new()`), and any job that merges with one of those (view_publication.rs:349-354 clears the set). It also covers every unchanged member, because each one is stat'd on every publication (assembly.rs:313-321).
- Problem:
  - With `rebuild_all`, every regular file is `fs::read` and BLAKE3-keyed, even when the previous manifest already holds the same key. There is no size or mtime short-circuit.
  - For a cold build, every changed file's `source` and extracted `payload` are kept in `candidates` until the put loop. Peak memory is about the total source bytes plus the total payload bytes.
  - `CallgraphBlob::extract` (a tree-sitter parse) runs serially on one thread.
  - Each new payload then gets one `BEGIN IMMEDIATE` blob put, while holding the global `BLOB_DURABILITY_BARRIER`, plus one `BEGIN IMMEDIATE` alias transaction, which computes git SHA-1 and BLAKE3 over the source again. That is 2N write transactions.
- Impact: on a 100k-file, 1 GB repository: about 1 GB of reads and hashing per lazy callgraph build even when nothing changed, about 1-2 GB peak RSS on a cold publication, a single-core parse of the whole tree, and 200k small write transactions.
- Suggested fix:
  - For lazy builds, pass only the paths that lack a callgraph key, or reuse previous entries whose key is present.
  - Keep a per-root stat cache (size, mtime, inode), as `CheckoutDriver::reconcile` does, so unchanged files are not read.
  - Stream in bounded chunks: extract in parallel with rayon, then put and alias-seed each chunk in one transaction, and drop the sources.
  - Reuse the BLAKE3 digest from the key for the alias.
- Confidence: High

##### [MEDIUM] The process-wide blob durability mutex is held across derived-DB checkpoint and fsync, trigram fsync, alias checkpoint and the closure probe
- Location: views/mod.rs:918-968 (`prepare_inner`); blob_store/mod.rs:91-95, 566, 637
- Hot path / frequency: every publication. `BlobStore::put` and `quarantine` take the same static mutex, so every root in the process is affected (relevant to the subc daemon serving many roots).
- Problem: the barrier is needed only so that "checkpoint blob DB → mark durable → probe closure" is atomic against concurrent puts. It is also held through `sync_database_with_passive_checkpoint(derived)` (a FULL checkpoint plus fsync of a possibly hundreds-of-MB WAL after a cold build), `sync_file_and_parent(trigram)`, the alias DB checkpoint, and `probe_publication_closure`, which looks up every manifest key in batches of 500. All blob puts in the process, including another root's assembly or the semantic handoff, block for that whole time.
- Impact: blob writers in other roots stall for seconds per publication after large builds.
- Suggested fix: run the derived, trigram and alias durability steps before taking the barrier. They do not touch the blob databases. Hold the barrier only for the blob-DB checkpoint, the durable mark and the closure probe. Consider making the barrier per blob-DB path instead of global.
- Confidence: Medium-High

##### [MEDIUM] Each incremental publication copies the whole derived DB and serializes or clones the full manifest several times
- Location: views/assembly.rs:661-718 (`clone_derived`), views/generation.rs:495-525/586-615 (SQLite backup `step(-1)`); views/materialization.rs:172-177, 291, 304, 1104 (`fingerprint` = `to_json_bytes` + blake3); views/mod.rs:1225-1271 (`write_manifest_once`); assembly.rs:295-305 (`previous_entries` clone), 380-395; context.rs:7362-7366 (`manifest.clone()`)
- Hot path / frequency: every publication whose manifest changed, which includes every watcher batch that edits a tracked file.
- Problem: an immutable generation is produced by a full-page SQLite backup of the base derived database, followed by a FULL checkpoint and fsync, even when only one file changed. Around that, the code builds:
  - the manifest JSON for the base fingerprint,
  - the manifest JSON for the new fingerprint, and again when writing `manifest-<gen>.json`,
  - a full `BTreeMap<Vec<u8>, ManifestEntry>` clone of the previous manifest,
  - a per-candidate `entry.clone()`,
  - another `manifest.clone()` in `prepare_view_paths`,
  - a full `load_manifest` (a JSON parse) of the previous generation.
- Impact: O(derived DB bytes) copy and fsync plus about 4-6 O(N) manifest copies or serializations per single-file edit. For 100k entries the manifest is about 20 MB of JSON, and the derived DB is often 100 MB or more.
- Suggested fix: clone derived with reflink or `copy_file_range` where available, or keep one mutable derived DB per view and use the generation pointer only for readers. Store the manifest fingerprint at publish time instead of recomputing it. Pass `&Manifest` or `Arc<Manifest>` instead of cloning. Iterate `previous.entries()` instead of copying it into a new map.
- Confidence: Medium (part of this is deliberate design for immutability; the cost still scales with repository size per edit)

##### [MEDIUM] Views-on semantic search rebuilds full membership, re-reads ignore files, and may walk the whole tree on each query while holding the view mutex
- Location: views/semantic.rs:1205-1290 (`SemanticPlane::overlay`), 1107-1141 (`ignore_rules_changed`), 1143-1153 (`walked_members`); views/snapshot.rs:688-711 (`membership`)
- Hot path / frequency: every `aft_search` semantic lane query on a views-on root.
- Problem:
  - `lock(&view)` is taken first.
  - Then `snapshot.membership()` allocates a new `BTreeMap` with every regular path, cloning each `RelPath`.
  - `ignore_rules_changed` `fs::read`s and BLAKE3-hashes every `.gitignore`, `.aftignore` and `.ignore` member.
  - When the watcher is not healthy, `walked_members` does a full `walk_project_files` of the checkout.
  - The cache key requires a full pass with a blake3 shape hash and a `HashSet<PathBuf>` of tombstones.
  - `SemanticIndex::for_view` may run on a cache miss.
  
  All of this happens with the per-view mutex held, which also serializes fills and other queries for that view.
- Impact: O(N) allocation per query (about 10-30 ms at 100k files). After any watcher overflow, every query does a full repository walk under the lock.
- Suggested fix: cache the membership and ignore-file digests per `(generation, delta_version)`. Cache the walk result per watcher epoch instead of per query. Compute the overlay key from `fill_version` plus the delta version. Drop the view lock before walking.
- Confidence: Medium-High

##### [MEDIUM] Blob GC marking parses every manifest of every view under the storage-wide barrier to examine at most 4096 rows, and scans the whole payload table for an unused number
- Location: gc/mod.rs:122-170 (`sweep_planes_bounded`), 343-347 (`SUM(length(payload))`), views/generation.rs:64-91 (`blob_references_by_generation`); commands/configure.rs:6831-6858
- Hot path / frequency: each storage-retention pass (10 min) and each configure maintenance unit with views on.
- Problem: under `storage_retention::barrier(storage)`, which blocks pin admission and `record_bind` (configure), the sweep reads `views/*`. For each view it calls `blob_references_by_generation`, which fully deserializes every `manifest-*.json` (up to 4096 per view, 8192 views) only to collect keys. It then sweeps a cursor batch of 4096 rows. Each sweep also runs `SELECT SUM(length(payload))` over the whole table for `retained_bytes`, while `_byte_budget` is unused. The configure path computes the own view's references and then the sweep computes them again.
- Impact: O(total manifest bytes across all checkouts) of JSON parsing per pass, while holding the bind and pin barrier. This can exceed the 5 s control deadline, which is checked only between views.
- Suggested fix: persist a compact `keys-<gen>.bin` (sorted 32-byte keys) next to each manifest at publish time and read that instead. Cache it per `(view, generation)`, since generations are immutable. Drop the `SUM` scan or track it incrementally. Check the deadline inside the per-manifest loop.
- Confidence: High

##### [LOW] `FamilyStore::rows_oldest` sorts the whole payload table without an index
- Location: blob_store/v2.rs:590-620; schema at v2.rs:55-64 (index is `(full_key, ref_epoch)`)
- Hot path / frequency: eviction `account_keys` and family GC `delete_unmarked` (background, every 10 min, deadline-bounded).
- Problem: `ORDER BY ref_epoch ASC, full_key ASC LIMIT ?` cannot use `blob_membership`, so it does a full table scan plus a sort, reading the payload B-tree leaf pages, which is where payloads are stored inline (WITHOUT ROWID).
- Suggested fix: add `CREATE INDEX ... ON blob_payloads(ref_epoch, full_key)`.
- Confidence: Medium

##### [LOW] The retention pass fsyncs its cursor file and directory after every examined entry
- Location: storage_retention.rs:1663-1675 (`atomic_json(&batch.cursor_path, &after)` per item, FileAndDirectory durability), 870-958 (`traversal_batch` reads the full domain directory each pass)
- Hot path / frequency: every 10 min per storage root.
- Problem: up to 64 entries × 2 fsyncs per pass for a resumable cursor that does not need to be crash-exact. Each pass also enumerates the whole domain directory to pick 64 names.
- Suggested fix: write the cursor once at the end of the batch, or every K entries, with `AtomicWriteDurability::File`.
- Confidence: High

##### [LOW] Latent: the trigram and callgraph plane materializers do one write transaction per manifest entry and a cold callgraph build per fold
- Location: views/trigram.rs:568-640 (`materialize`, `put_or_touch` per entry), views/first_load.rs:1709-1780 (callgraph plane `put_or_touch` per entry plus `callgraph::materialize`, which is a cold build), blob_store/v2.rs:459-511 (`put_or_touch` does an UPDATE plus commit even when the key is reused); views/live_delta.rs:43-73 (`strict_walk` reads every file and BLAKE3-hashes it twice: `DiskState::of_bytes` and `Attachment.content`), trigram.rs:455-472 (holds the `delta` mutex across that walk)
- Hot path / frequency: currently only in tests and opt-in compositions. The production `CheckoutSemantic` registers only the semantic plane (semantic_runtime.rs:117-129), and `TrigramDriver` is never constructed outside tests. These would become HIGH if those planes were wired in.
- Problem: N `BEGIN IMMEDIATE` write transactions per own-generation build. Each reused key still writes a WAL frame, so about 4 KB × N of WAL I/O. Each payload is re-encoded or re-serialized. There is a full cold derived callgraph rebuild on every fold, and a full-content walk while holding the delta lock.
- Suggested fix: use one `touch(&keys)` batch (it already exists) plus one transaction for the inserts. Cache encoded payloads in attachments. Enable incremental callgraph seeding. Hash once in `strict_walk`.
- Confidence: Medium

##### [LOW] `private_storage::open_dir` does O(depth²) `openat` calls on every store open
- Location: private_storage.rs:109-198 (`tighten_open_dir` → `tighten_within` per ancestor, each re-walking from root)
- Hot path / frequency: `BlobStore::open` ×2, `AliasStore::open` and `PathStatusStore::open_at` on every publication.
- Problem: about depth²/2 `openat` calls plus metadata reads per open. This is small, but it repeats for permissions that were already tightened.
- Suggested fix: remember tightened directories in a process-wide `HashSet` and skip them.
- Confidence: Medium

#### Area summary
The worst costs sit in the views publication pipeline. A single-file edit in a views-enabled root still pays for the whole repository in several places:
- the semantic handoff (`store_live_semantic_blobs`) clones, serializes, re-reads and re-hashes the entire semantic index;
- `git ls-tree` and `git check-attr` run twice;
- the full derived DB is copied and fsynced;
- the manifest is serialized and cloned several times;
- on read-only or semantic-pending roots, every pending path gets its own fsync'd path-status write.

The v2 live-delta code has a true quadratic (`journal.retain` inside `apply`), which bulk rebuilds (`reconciled`, `record_change`, directory intents) hit directly. Background GC can thrash a large v1 blob store with full `VACUUM` runs that are aborted and retried. Because views are off by default, none of this affects the default configuration. With views on, the first three HIGH findings scale badly on repositories of 50k files or more.

#### Things checked and found OK
- `BlobStore::contains` uses a covering membership index (`prepare_cached`) and does not read payloads. The closure probe batches membership checks 500 keys per statement.
- `CheckoutDriver::reconcile` (production v2 path) reuses stat results, so unchanged files are not read again.
- The semantic plane's `materialize` skips Ready entries and probes `contains` before reusing stored keys.
- `generation_has_callgraph` and `main_checkout_callgraph` cache their per-generation verdicts. The parent worker caches the HEAD fingerprint with a cheap probe.
- Eviction measurement and walks, and storage-retention tree walks, are bounded by entry caps and deadlines. Retention runs at most every 10 min on its own thread.
- Pin key files are written with a single `write_all`. `AssemblyPin::renew_if_due` is time-gated.
- `durability::record` is a no-op outside tests. `production_storage::refuse_write` returns immediately in release builds.
- `subc/blob_store.rs` caches quota usage and avoids running `usage()` on every put.
- `migrate_storage.rs`, `migration/prune_legacy.rs` and `legacy_partitions.rs` are one-shot CLI or migration paths, so their per-file copies and transactions are acceptable. `disk_write_hunt.rs` is benchmark-only.
- `refresh/mod.rs` deduplicates by full key before running expensive work.

---

### 3.10 Bash execution & background tasks

Files reviewed: `crates/aft/src/bash_background/` (registry.rs non-test half deep-read on spawn / poll / finalize / render / completion / watch / GC / status paths; watchdog.rs, buffer.rs, persistence.rs (layout resolve, task read/write, artifacts, exit marker), remote.rs (sink, run loop), pty_process.rs, pty_runtime.rs, exit_observer.rs, gc_cursor.rs, watches.rs, output.rs constants, process.rs (group sampling), mod.rs spawn), `commands/bash.rs`, `bash_status.rs`, `bash_watch.rs`, `bash_orchestrate.rs` (foreground wait/poll), `subc/bash.rs` (deferred wait loop; outside the area but it drives the hot path), `sandbox_spawn.rs` (resolve/build profile), `agent_child_env.rs` (inject/maintain), `bash_db_hints/mod.rs`, `bash_rewrite/` (rules, observation, catalog), `bash_permissions/scan.rs`, `exec_remote/stream.rs`, `pty_render.rs`, `process_io.rs`, `windows_shell.rs` (skim), `compress/` (mod.rs dispatch, generic, toml_filter, line_cut, matchers in all compressors), `crates/aft-tokenizer/src/{lib,claude}.rs`. Test modules skipped.

#### Findings

##### [HIGH] In subc mode, the exit of a piped foreground command is only noticed on the 500 ms watchdog tick, so every foreground bash call waits up to 500 ms extra
- Location: `subc/bash.rs:1530-1548` (`run_deferred_bash_wait` select loop), `subc/bash.rs:157-185` (`observe_deferred_bash_wait`), `bash_background/registry.rs:4875` (`observed_status`), `bash_background/watchdog.rs:9` (`WATCHDOG_INTERVAL = 500ms`), `registry.rs:6521` (`poll_task`) / `6583` (`reap_child`)
- Hot path / frequency: every foreground `bash` call routed through the subc daemon, which is the normal production path.
- Problem: piped tasks have no waiter thread. The child writes straight into files (`Stdio::from(file)`, `registry.rs:10368`), and only PTY readers and remote sinks send `wake_tx`. A piped child's exit is found only when something calls `poll_task` → `reap_child` → `try_wait`. The subc wait loop wakes on `terminal_transition_notified()` or a 100 ms sleep, then calls `observed_status`, which only snapshots the task and never calls `poll_task`/`reap_child`. So the terminal transition (and its `notify_waiters`, `registry.rs:1705`) only happens on the watchdog's periodic tick. The standalone `main.rs` loop does reap, because its pending poll calls `status()` → `poll_task` every 100 ms. Only subc pays the cost.
- Impact: 0–500 ms (about 250 ms on average) added to every foreground bash reply, including trivial commands like `git status`. On top of that comes the watchdog head-of-line delay in the next finding.
- Suggested fix: on Linux, give each piped child a pidfd (already used for adopted tasks in `exit_observer.rs`), or a small waiter thread that does `child.wait()` and then `wake_tx.try_send(())`. A cheaper alternative: have `observe_deferred_bash_wait` call `registry.poll_task(&task)` (a `try_wait` is cheap) before taking the snapshot.
- Confidence: Medium-High. I traced every wake source, but did not measure it.

##### [HIGH] One process-wide watchdog thread does all terminal finalization inline, for every project root
- Location: `bash_background/watchdog.rs:126-251` (`scheduler`), `253` (`run_pass`); `registry.rs:7600-7731` (`finalize_from_marker`, including the 200 ms `reader_done` sleep-poll at about line 7717), `1661-1797` (`post_terminal_transition` → `finish_terminal_transition` → `ensure_terminal_output_cache` + `enqueue_completion_from_parts`)
- Hot path / frequency: every task completion that the watchdog detects. Per the previous finding, that is every subc foreground bash call.
- Problem: one `aft-bash-watchdog` thread serves all registries (all roots). When it sees an exit it runs the whole finalization synchronously:
  - metadata JSON rewrite
  - a reading of up to 10 MB of output (several times, see below)
  - ANSI strip and compression (holding the per-root compressor mutex)
  - Claude tokenization of up to 256 KB
  - aft.db upsert and compression-event insert under the global aft.db mutex
  - for a PTY task, up to 200 ms of `sleep(10ms)` polling for the reader

  Waiters are notified only after all of this (`notify_waiters` comes after `finish_terminal_transition`). While it runs, no other task in any project gets its exit observed, timeout-killed, or watch-scanned.
- Impact: completion latency for unrelated commands grows by the cost of another task's finalization: tens of ms for large outputs, up to 200 ms for a PTY. Parallel subagents in different roots serialize on this thread.
- Suggested fix:
  - Split detection from rendering. Publish the terminal status and `notify_waiters()` immediately after `finalize_from_marker` updates the state.
  - Hand rendering, tokenization and DB work to a small worker pool. The per-task `terminal_output_cache` already gives readers a place to wait or compute.
  - Replace the PTY reader sleep-poll with a condvar or channel wait off the watchdog thread.
- Confidence: High.

##### [HIGH] `bash_status` on a PTY task reads the whole PTY log and replays it through vt100 on every call; `output_offset` reads are unbounded
- Location: `commands/bash_status.rs:228-256` (`maybe_render_pty_screen`), `commands/bash_status.rs:139-170` (`read_artifact_range` → base64), `bash_background/registry.rs:1141-1178` (`read_artifact_range`, `read_artifact` → `read_all`), `pty_render.rs:10-34`
- Hot path / frequency: every `bash_status` poll of a PTY task, including the plugin's streaming polls. PTY sessions are typically long-lived: dev servers, watchers, REPLs.
- Problem:
  - `maybe_render_pty_screen` calls `read_artifact(.., TaskArtifact::Pty)`, which reads the entire PTY file into memory. While running, that file is uncapped; the 100 MB cap applies only at terminal.
  - It then runs `vt100::Parser::process` over all bytes to render a 24×80 screen. The raw bytes are read even when `output_mode` does not ask for raw.
  - `read_artifact_range(offset)` reads from `offset` to EOF with no cap and base64-encodes it into the JSON reply.
- Impact: O(total PTY history) CPU and memory per poll. A dev server that has written 200 MB means about 200 MB read plus a full terminal emulation each poll (seconds), with a matching heap spike. `output_offset=0` on a large log produces a reply in the hundreds of MB.
- Suggested fix:
  - Keep an incremental `vt100::Parser` per PTY task, fed by the reader thread or by a cursor of bytes already applied, and render only from it.
  - Alternatively, render from a bounded tail (for example the last 256 KB, after a clear-screen sequence).
  - Cap `read_artifact_range` to a chunk size (for example 1 MB) and let `output_next_offset` drive paging.
  - Read the raw bytes only when `output_mode` is `raw` or `both`.
- Confidence: High.

##### [HIGH] Remote-run output sink does fsync, a metadata JSON rewrite and an aft.db upsert for every output record
- Location: `bash_background/remote.rs:576-605` (`TaskSink::output`), `488-510` (`TaskSink::commit` → `persist_task_locked`), `registry.rs:1812-1825`, `1842-1915` (`dual_write_task`); similarly `unknown_output` (`remote.rs:609-640`)
- Hot path / frequency: every stdout/stderr record streamed from the remote runner for a `runon` task, possibly thousands per build.
- Problem: for each record the sink does:
  - `seek` + `write_all` + `set_len` + `sync_all` (fsync) on the output file
  - `commit`, which takes the task state lock and runs `persist_task_locked`:
    - `resolve_task_layout`: about 6 directory/file opens, plus reading the metadata JSON and parsing it twice (once to `Value`, once typed, `persistence.rs:1316-1330`)
    - an atomic temp-file write and rename of the full metadata
    - a deferred `upsert_bash_task` transaction under the process-wide aft.db mutex

  All of this exists to advance `stdout_len` / `last_seq`.
- Impact: roughly 1–10 ms per record (two fsync-class operations plus a WAL commit). A chatty remote `cargo build` can therefore spend seconds to minutes on bookkeeping. It also contends on the global aft.db mutex with every other tool in the daemon.
- Suggested fix:
  - Coalesce: buffer records and commit the cursor at most every 100–250 ms or N KB, plus on terminal or `started`. Crash safety is kept because bytes past the committed cursor are already truncated on resume (`TaskSink::new`).
  - Skip the aft.db mirror for cursor-only updates.
  - Keep the resolved layout or open metadata handle instead of re-resolving each time.
- Confidence: Medium-High. The record granularity depends on the runner, which I did not check.

##### [MEDIUM] The output of every completed task is re-read from disk 3–5 times (up to 10 MB each)
- Location:
  - `registry.rs:1076-1086` (`finish_db_hint`)
  - `registry.rs:3015-3026` and `3270-3281` (a `HintJob` is created for every non-PowerShell task)
  - `registry.rs:1535` and `7780-7783` (`finish_db_hint` called from `render_terminal_output` and from `enqueue_completion_from_parts`)
  - `registry.rs:5519-5525` (on every PTY terminal snapshot)
  - `registry.rs:1567-1571` (compression read)
  - `registry.rs:8243-8270` + `buffer.rs:210` (token-count read)
- Hot path / frequency: every bash completion. For PTY tasks the hint read repeats on every `bash_status`.
- Problem:
  - `finish_db_hint` runs `read_combined_head_tail(10 MB, 4 MB, 6 MB)` before calling `job.finish()`. `finish()` returns immediately when the result is already set (a `OnceLock`) or when `exit_code == Some(0) && !pipeline`, which is the common case. Because every task gets a `HintJob` (`cli: None` for non-DB commands), every completion pays this read at least twice: once from the render, once from `finish_terminal_transition` → `enqueue_completion_from_parts(Some(&buffer))`.
  - The render reads the same head and tail again for compression.
  - `completion_token_counts` reads the stream tails a third time.
- Impact: for a command with 10 MB of output, about 30–40 MB of extra reads and allocations per completion, all on the watchdog or reply path. Small outputs pay only extra opens and `resolve_task_layout` calls (several opens plus two JSON parses each).
- Suggested fix:
  - Check `job.result.get().is_some()` and `(exit_code == Some(0) && !pipeline)` before reading.
  - Skip creating `HintJob` when `detect()` finds nothing and the command is not a pipeline.
  - Pass the raw text already read by `render_terminal_output` into `job.finish`, and the stripped text into token counting, instead of re-reading.
- Confidence: High.

##### [MEDIUM] Terminal output cache fill has no single-flight guard, and the compressor mutex is held during compression
- Location: `registry.rs:1504-1529` (`ensure_terminal_output_cache`), `registry.rs:1489-1500` (`compress_output` holds `inner.compressor.lock()` across the compressor call)
- Hot path / frequency: every completion. The watchdog's `finish_terminal_transition` races with the reply poll (`status` / `observed_status` → `snapshot_with_terminal_cache`), because the status is published as terminal before rendering starts (`finalize_from_marker`, then `post_terminal_transition`).
- Problem:
  - `ensure_terminal_output_cache` checks for the cache, drops the lock, and renders. Two concurrent callers both render (read, strip, compress up to 10 MB) and one result is thrown away.
  - `compress_output` keeps the `Mutex<Option<Box<dyn Fn>>>` guard while the closure runs. All compressions in a root are therefore serialized, and the reply poll's duplicate render waits behind the watchdog's render.
- Impact: up to 2× compression CPU per completion, and serialized compression when several parallel agent calls in one root finish together. This adds latency to foreground replies.
- Suggested fix:
  - Store the compressor as `RwLock<Option<Arc<dyn Fn ...>>>` (or `ArcSwap`) and clone the `Arc` out before calling it.
  - Make the cache fill single-flight: a per-task `OnceLock<TerminalOutputCache>`, or an "in progress" state plus the existing `kill_settled`-style condvar.
- Confidence: High.

##### [MEDIUM] Compression dispatch makes many full passes and copies over up to 10 MB, and the eslint/ruff sniffers bypass the shared JSON probe
- Location: `registry.rs:1567-1596` (`render_terminal_output`), `compress/mod.rs:389-487`, `514-609`, `631-661`, `compress/line_cut.rs:170-172`, `compress/eslint.rs:34-58`, `compress/ruff.rs:30-70`, `compress/{bun,cargo,mypy,pytest,tsc,prettier,go,biome,playwright,vitest}.rs` `matches_output`
- Hot path / frequency: every compressed completion. The worst case is commands that no command-name compressor claims (custom scripts, `make`, `node x.js`).
- Problem: for one render:
  1. The renderer runs `strip_ansi` on each stream for `endings` (full copy). `ensure_final_lines(text, "")` builds a `Vec<&str>` of every line just to take the last 5.
  2. `compress_with_registry_exit_code` runs `output.lines().count()`, then `strip_ansi` again (another full copy).
  3. The Tier 1b output-sniffer loop calls `matches_output_probe` on about 15 compressors. Each does its own `output.lines().any(...)` over the whole text, so about 15 full line scans when none match. `EslintCompressor` and `RuffCompressor` override only `matches_output`, so each runs a full `serde_json::from_str::<Value>` on any output that starts with `[`, on top of the `OutputProbe` parse.
  4. With a nonzero exit, `failure_preserving_result` lowercases the whole raw output (`missing_raw_failure_signal_lines`, `text_has_failure_signal`, `result_looks_successful`). The `GenericCompressor` fallback then runs `strip_ansi` again on already-stripped text.
- Impact: on 10 MB of output, about 20+ linear passes and 4–5 full 10 MB allocations, plus up to 3 full JSON parses of a 10 MB array. That is tens to hundreds of ms per completion, on the path described in the watchdog finding.
- Suggested fix:
  - Strip ANSI once and pass `&str` throughout.
  - Compute `endings` from the tail only (for example the last 4 KB) with `rsplit`.
  - Have sniffers inspect a bounded head and tail window (signatures are summary lines), or do one combined per-line classification pass.
  - Move eslint and ruff onto `OutputProbe::json()`.
  - Lowercase per line (lazily) rather than the whole buffer, and use `compress_stripped_output` in the failure fallback.
- Confidence: High on the passes; Medium on the absolute cost.

##### [MEDIUM] Claude tokenizer runs fancy-regex over up to 256 KB on every completion, before waiters are notified
- Location: `registry.rs:8243-8279` (`completion_token_counts`, `TOKENIZE_CAP_BYTES_PER_STREAM = 128 KB`, line 98), `aft-tokenizer/src/claude.rs:23-60`, `223-231`
- Hot path / frequency: every non-PTY completion, inside `enqueue_completion_from_parts` and before `notify_waiters`.
- Problem:
  - `PAT_STR` contains a lookahead (`\s+(?!\S)`), so the whole pre-tokenizer runs on fancy-regex's backtracking VM (roughly 5–20 MB/s).
  - `count_tokens` builds the full `Vec<u32>` just to take `.len()`.
  - Ranks are looked up in default SipHash `HashMap`s, `get_rank_for_slice` runs `from_utf8` on every pair, and `byte_pair_merge` allocates two `Vec`s plus a heap per piece.

  All of this produces only analytics numbers (`original_tokens` / `compressed_tokens`).
- Impact: about 10–50 ms per completion with full-size output, on the latency-critical path. Throughput matters when many parallel agents complete commands.
- Suggested fix:
  - Compute the token counts after the completion is published and waiters are notified (async or worker), or sample (for example tokenize 32 KB and extrapolate by bytes).
  - Add a count-only path that keeps no `Vec`.
  - Use FxHash or ahash for the encoders.
  - Optionally use the `regex` crate for everything except the lookahead branch, handling trailing whitespace manually as tiktoken's newer core does.
- Confidence: Medium. This is a cost estimate, not a measurement.

##### [MEDIUM] Watch scanning: per-pass database sweep, per-tick artifact re-resolution, and a 64 KB-per-tick read cap that falls behind fast output
- Location: `registry.rs:2360-2440` (`evaluate_erased_watch_targets`, called at the start of every `run_pass` and from `has_erased_watch_reference` on every `bash_status`), `registry.rs:4745-4873` (`scan_watch_file`, `scan_task_watch_output`), `persistence.rs:2238-2259`, `942-952`
- Hot path / frequency: every watchdog pass (2 Hz plus every wake from any PTY or remote task) whenever any watch is registered in a root, and every `bash_status`.
- Problem:
  - `evaluate_erased_watch_targets` takes the global aft.db mutex and runs a `get_bash_task` query per watched task on every pass. That is O(watched tasks) DB queries at 2+ Hz, purely to detect rows erased by GC.
  - `scan_task_watch_output` calls `open_task_artifact` for stdout and stderr on every pass. Each call goes through `resolve_task_layout` (opening the session, task, control and io directories, probing the flat JSON, and reading plus double-parsing the metadata).
  - Non-drain passes read at most one 64 KB chunk per stream per pass, so throughput is capped at about 128 KB/s per stream at the 500 ms tick. Piped tasks never send wakes, so a task emitting more than about 128 KB/s falls further and further behind: pattern-match notifications lag without bound until the task exits.
- Impact: constant background DB and syscall load while watches are active, and missed or very late `bash_watch` matches on chatty builds or logs.
- Suggested fix:
  - Run the erased-row sweep on the 60 s cleanup ticker, or trigger it only when the GC actually deletes rows (it already calls it there).
  - Cache the `ValidatedArtifact` handles per watched task (the task already holds pinned `io_handles` while running).
  - Loop chunks until caught up, with a per-pass byte budget of a few MB, instead of one 64 KB chunk.
- Confidence: High.

##### [LOW] Each artifact open re-resolves the task layout and parses the metadata JSON twice, sometimes under the task state lock
- Location: `persistence.rs:942-952` (`resolve_task_layout` → `read_task_at`), `1316-1330` (`read_task_file` parses to `serde_json::Value`, then to the typed struct), `2238-2259`; callers include `registry.rs:9931-9934` (`snapshot_locked` opens `SandboxUnavailable` under the state lock on every snapshot when `sandbox_native`), `persistence.rs:2015` (`read_exit_marker`), `registry.rs:1799-1840` (every `persist_task_locked` / `update_task_metadata_locked`)
- Hot path / frequency: every 100 ms reply poll (snapshot), every watch tick, every metadata write.
- Problem: about 6–8 syscalls plus two JSON parses per open, used purely as an identity check. In `snapshot_locked` this runs with the task's state `Mutex` held, which blocks the watchdog and other readers of that task.
- Impact: small per call, but multiplied across polls, passes and writes. It is also lock hold time on a contended mutex.
- Suggested fix:
  - Cache the `ResolvedTask` (pinned dirs) on `BgTask` and revalidate only on error.
  - Parse the metadata once, reading `schema_version` from the typed struct with `#[serde(default)]`.
  - Compute `sandbox_unavailable` once at terminal and store it in the metadata.
- Confidence: High.

##### [LOW] Every foreground bash call writes metadata about 4–5 times
- Location: `registry.rs:2918-2975` (Starting write + `dual_write_task`, then the Running `persist_task`), `7670` (terminal `update_task_metadata_locked`), `1744-1766` (`sample_task_process_group` always calls `persist_task_locked`, even when the sample is `None` or empty), `7933-8057` (compression event insert)
- Hot path / frequency: every bash call.
- Problem: each of these is a full JSON atomic replace (temp file, write, rename, after a layout resolve) plus a separate SQLite upsert transaction under the process-wide aft.db mutex.
- Impact: about 5 file replaces and 5 WAL commits per command. That is modest, but it adds latency and contention on a shared mutex used by every tool.
- Suggested fix:
  - Skip the descendant persist when `live_descendants` did not change (None or empty both before and after).
  - Combine the Starting and Running writes when the spawn succeeds immediately.
  - Put the terminal row and the compression event in one transaction.
- Confidence: Medium.

##### [LOW] Per-spawn child-environment maintenance (gh shim by default; git hooks when co-author is on)
- Location: `agent_child_env.rs:782-889` (`inject` → `maintain`, `625-655`), `1127-1153` (`ensure_managed_git_hooks`), `1170-1260` (`refresh_legacy_git_hooks`), `1661-1675` (`write_hook_if_changed`)
- Hot path / frequency: every bash spawn (`bash_background::spawn` → `agent_child_env::inject`).
- Problem:
  - With the default `github.shim = true`, each spawn does `current_exe`, two `canonicalize` calls, a `stat`, a lock on the probe cache, `private_storage::open_dir` and a `readlink`.
  - With `git.co_author` enabled, each spawn also:
    - takes an exclusive cross-process `flock`
    - reads the hooks directory
    - reads every managed hook file in full to compare it
    - scans the whole hook cache root, opening and reading up to 256 legacy directories × N hooks (64 KB cap each) with a 2 s budget

    The code comment acknowledges that this "runs before every child launch".
- Impact: with co-author on, possibly milliseconds per spawn, and concurrent spawns across all AFT processes serialize on the flock. With defaults, the cost is minor.
- Suggested fix: memoize the verification result per (hooks dir, mtime, managed-set key) for the process and re-verify at most every N seconds or on configure. Run the legacy refresh once per process or configure, not per spawn.
- Confidence: Medium.

##### [LOW] Native sandbox profile is rebuilt from scratch on every spawn, including enumerating `$HOME`
- Location: `sandbox_spawn.rs:1276-1410` (`resolve_sandbox_spawn`), `1449-1520` (`build_native_profile`), `2265-2435` (`build_linux_profile` / `compute_linux_profile`, with `force_children: true` for HOME and `/var`), `2517-2600` (`split_read_grant` listing)
- Hot path / frequency: every bash spawn when `sandbox.enabled` (default off) or the worker jail is active.
- Problem: about 25 static canonicalizations, a git policy resolve (reading `.git`), and a secure directory listing of every `$HOME` child, recursing where denies are nested (and listing the project root when repo hooks are denied). It is recomputed each time although it depends only on config, root and HOME.
- Impact: hundreds to thousands of syscalls per command (about 1–10 ms) for sandboxed users.
- Suggested fix: cache the computed read/write grant set keyed by (config generation, project roots, HOME). Add only the per-task paths (task io, temp dir, payload grants) per spawn. Invalidate on configure or when the HOME listing's mtime changes.
- Confidence: Medium.

##### [LOW] Linear scans over all retained tasks for artifact-ownership checks
- Location: `registry.rs:1094-1114` (`is_session_owned_artifact_path`), `1116-1139` (`read_artifact_path`), `8550-8576` (`background_slot_holders` locks every task's state)
- Hot path / frequency: twice per render (`render_terminal_output`, lines 1537-1543), once per path argument in the permission scan, and once per background spawn.
- Problem: the `tasks` map retains finished tasks for 24 h (`FINISHED_RETENTION`). Each check runs `canonicalize` and then iterates every task under the global `tasks` mutex, building 4 `PathBuf` joins per task.
- Impact: O(N) with N in the thousands for heavy sessions, about 10^4 allocations per check, under the registry's main lock.
- Suggested fix: keep a `HashMap<PathBuf, (session, task_id, artifact)>` index of canonical artifact paths, maintained on insert and remove. Keep a separate running-and-slot-holding set for admission.
- Confidence: High (the cost grows with retention).

##### [LOW] Terminal disk cap loads up to 100 MB per stream into memory
- Location: `bash_background/buffer.rs:242-255` (`enforce_terminal_cap`), `persistence.rs:2261-2280` (`replace_artifact_with_tail`)
- Hot path / frequency: once per completion of a task whose output exceeded 100 MB.
- Problem: `read_range(len - 100 MB, 100 MB)` reads the retained tail into a `Vec` and then writes it to a temp file. That is a 100 MB heap spike per stream, 200 MB for stdout plus stderr, on the watchdog thread.
- Impact: transient memory spikes and slow finalization for very chatty commands.
- Suggested fix: stream-copy in 1 MB chunks into the temp file (or use `copy_file_range`/`fallocate(FALLOC_FL_COLLAPSE_RANGE)` on Linux).
- Confidence: High.

##### [LOW] Idle polling and wake fan-out
- Location: `bash_background/remote.rs:1198-1203` (a 50 ms `sleep` → `continue` in each remote task's run loop), `registry.rs:1705` (`notify_waiters` wakes every deferred bash waiter of the root on any task's completion), `commands/bash_watch.rs:233-241` (100 ms polling for the first 5 s per watch)
- Hot path / frequency: per active remote task, per waiting foreground call, per watch.
- Problem: about 20 wakeups/s per remote task while it is idle. Every completion wakes all N waiters in the root, and each runs a `spawn_blocking` `observed_status`.
- Impact: minor CPU only; this is a thundering herd at about N=10s.
- Suggested fix: use per-task `Notify` or a per-task terminal watch channel. Wait on cancel intent with a `Notify` instead of a 50 ms timer.
- Confidence: Medium.

#### Area summary
The bash subsystem's per-call correctness machinery is careful: DB writes are deferred off the state lock, GC is bounded and coalesced, and TOML filters and regexes are compiled once. The real performance problems are in the completion pipeline and its scheduling:
- In subc mode, piped exits are discovered only on a 500 ms tick.
- All finalization for every root runs inline on one watchdog thread, and waiters are notified only after rendering, compression, tokenization and DB work.
- Output is re-read 3–5 times per completion (the DB-hint read runs unconditionally for every task).
- The cache fill can render twice, while a mutex held across the compressor call serializes compression.
- The compression dispatch makes about 20 linear passes over up to 10 MB.

PTY `bash_status` is O(total history) per poll, and remote runs fsync and rewrite metadata plus the DB row for every output record. Fixing the subc reap latency and moving finalization off the watchdog thread would give the largest user-visible win: lower latency on every foreground bash call.

#### Things checked and found OK
- TOML filter registry: built once lazily, cached in `Arc<RwLock<FilterRegistry>>`, rebuilt only on configure (`context.rs:4990-5050`). No per-call TOML or regex compile.
- Watch regexes are compiled once at registration. The tokenizer regex and encoders are built once via `OnceLock`. The bash_db_hints regex is built once via `OnceLock`.
- `bash_rewrite` compiles the user's grep pattern once per rewrite, as validation only (negligible). `bash_permissions` creates a tree-sitter `Parser` per scan (cheap).
- Persisted GC: bounded to 128 directory entries per run via `gc_cursor`, coalesced per storage root (30 s window), and off-thread. Its aft.db liveness query is batched per session.
- Hard-kill renewal persistence is throttled by `RENEWAL_PERSIST_STEP_MS`.
- Watch cursor DB writes are skipped when the offsets are unchanged.
- The cleanup queue is processed in budgets of 128, and the side maps (`completion_pass_cause`, `persisted_watch_cursors`, `session_task_ids`) are pruned with their tasks.
- Adopted-task exit detection uses a pidfd/kqueue with marker backoff; there is no PID polling every tick.
- Process-group sampling has a fast path (`kill(-pgid, 0)`) before walking `/proc`. Timeout kills go to a 4-thread pool, so they do not block the watchdog.
- `strip_ansi`, `dedup_consecutive` and `middle_truncate` are single linear passes. `OutputProbe` shares one JSON parse among the probe-aware matchers.
- The PTY reader uses an 8 KB buffer and writes straight to the file (`flush` on `File` is a no-op). There is no per-chunk fsync for local tasks.
- `snapshot_locked` returns the cached terminal preview and reads no tail when `preview_bytes == 0` (the foreground poll path).
- Foreground output preview is capped at 16 KB per cached task.
- The Windows shell candidate list is cached in a `OnceLock`.

---

### 3.11 LSP integration

Files reviewed (non-test code, under `crates/aft/src`): `lsp/transport.rs`, `lsp/jsonrpc.rs`, `lsp/writer.rs`, `lsp/client.rs`, `lsp/manager.rs` (deep-read: open/change/pull/wait/event/watcher/applicability paths), `lsp/document.rs`, `lsp/diagnostics.rs`, `lsp/pending_changes.rs`, `lsp/registry.rs` (servers_for_file, root resolution, binary resolution), `lsp/roots.rs`, `lsp/typescript_project.rs` (memo paths), `lsp/tsconfig_membership.rs` (skim), `lsp/completed_rust_check.rs` (capture/fingerprint/begin/complete), `lsp/child_registry.rs` (skim), `lsp/position.rs`, `lsp/pull_params.rs`, `lsp/environmental.rs`, `commands/lsp_*.rs` (hover/definition/references/rename/prepare_rename/diagnostics/inspect/navigation), `lsp_hints.rs`, `developer_tools.rs`. Callers checked where they decide frequency: `context.rs` (post-edit diagnostics, watcher forward, applicability refresh), `commands/inspect.rs` (applicability walk), `runtime_drain.rs` (event drain).

#### Findings

##### [CRITICAL] rust-analyzer check fingerprinting (subprocess spawns + whole-workspace walk/hash) runs under the LSP manager lock on every `cargo check` begin/end
- Location: `lsp/client.rs:1344-1347` (`record_rust_progress` -> `cache.begin(began)`), reached from `lsp/manager.rs:4992` (`handle_rust_progress`) <- `handle_event` <- `drain_events_bounded` (`manager.rs:3083`); and `manager.rs:3103-3110` (`drain_events_bounded` -> `cache.complete()`). Work is in `lsp/completed_rust_check.rs:1441` (`begin`), `:1477` (`complete_until`), `:698` (`Runtime::capture`), `:656` (`Runtime::hash`), `:555` (`version`), `:329` (`fingerprint`).
- Hot path / frequency: every Rust workspace (enabled whenever the root has `Cargo.toml`, `manager.rs:6142`). Every save/edit of a `.rs` file makes rust-analyzer run flycheck, emitting a `$/progress begin` and `end`. Both are handled inside `drain_events*`, which every caller runs with the manager lock held (`ctx.lsp().drain_events()` in each lsp_* command, the maintenance drain, post-edit waits, `handle_waited_event`).
- Problem: `begin()` calls `runtime.capture()` synchronously. That does three things. (1) `hash()` runs `version()` twice, spawning `rustc -vV` (usually through the rustup proxy) and `rust-analyzer --version`, and polls `try_wait` with 5 ms sleeps. Nothing is cached, so each capture costs two process spawns. (2) `fingerprint()` does `read_dir` over every workspace package directory and `symlink_metadata` + `metadata` on every `.rs`/manifest input, and walks the ancestors for cargo config. (3) It `read`s, BLAKE3-hashes and parses `source_inputs` for every file without a reusable stamp. On the first check there is no previous fingerprint, so that is every file (budget `INITIAL_CAPTURE_BUDGET` = 15 s); later checks use `BUDGET` = 2 s. `complete()` then runs `capture` twice more plus `build_inputs` (another 2 s budget each). All of this runs while `parking_lot::Mutex<LspManager>` is held, so hover, definition, didChange delivery, post-edit diagnostics and the status-bar drain all block behind it.
- Impact: each Rust save holds the manager lock for at least about 6 subprocess spawns (around 50-300 ms with rustup) plus a stat of every workspace source file (around 10-50 ms at 10k files). On a cold start or after `cargo update` it can hold the lock for up to 15 s (first capture) plus 2x2 s. Edits that request diagnostics use `try_lock_for(deadline)` and then fall back to "pending", so diagnostics silently go missing. Interactive LSP commands hang.
- Suggested fix: take capture/complete off the lock. In the drain, record only `began`/`finished` (cheap). Run `capture`/`complete_until` on a worker thread, as the record write and `validated()` already do (`aft-rust-check-save`, `validation`), and store the result back with a short re-lock. Cache `Runtime::hash()` per client lifetime, keyed by the binary path/mtime and the relevant env digest, instead of spawning `rustc -vV` and `rust-analyzer --version` on every capture.
- Confidence: High (call chain verified; begin/complete run inside `handle_event`/`drain_events_bounded`, which are `&mut self` methods on the locked manager).

##### [HIGH] Applicability walk resolves server roots per file with no caching and rebuilds every built-in `ServerDef` for every file
- Location: `lsp/manager.rs:6937` (`walk_applicable_area`): per-file loop at `:6986-7016`; `lsp/registry.rs:1071` (`servers_with_root_marker` -> `resolved_servers` -> `builtin_servers()`), `registry.rs:890` (`servers_for_file`), `registry.rs:385` (`workspace_root_for_file_with_project_root`), `registry.rs:925` (`biome_config_excludes`), `lsp/roots.rs:13` / `:91` (`find_workspace_root_within`, `find_rust_workspace_root`).
- Hot path / frequency: on every unscoped `aft_inspect` request (`commands/inspect.rs:471`, with the request deadline) and on every `set_config`/configure (`context.rs:6439`, a background thread whose result gates diagnostics "pending" state). It runs for every file in the project tree.
- Problem, per walked file:
  - `servers_with_root_marker(file)` calls `resolved_servers(config)` -> `builtin_servers()`, which allocates all ~30 built-in `ServerDef`s with their `Vec<String>` fields from scratch. The comment in `servers_for_file` says this was exactly the cost removed elsewhere. The only goal is to check whether the file name is a marker; most files are not.
  - `servers_for_file(file)` clones each matching `ServerDef`. A `.ts`/`.js` file matches TypeScript, Oxlint, Biome and others.
  - For each def, `server_key_for_definition` -> `workspace_root_for_file_with_project_root` canonicalizes the file and the project root, runs `is_dir`, takes a device-id stat per ancestor, and stats every marker per ancestor (`dir.join(marker).exists()`). None of this is memoized, although the answer depends only on the parent directory. For Biome, `biome_config_excludes` repeats the whole root walk once more plus a canonicalize. For Rust, `find_rust_workspace_root` reads and TOML-parses `Cargo.toml` of every ancestor up to the workspace (`cargo_workspace_contains_crate`, `roots.rs:161`), and compiles a glob per `members` pattern (`roots.rs:220`), for every `.rs` file.
  - The result is deduplicated only after the work (`seen.insert(key)`).
- Impact: about 100-150 syscalls plus ~300 small allocations per JS/TS file at depth around 8. At 50k files that is about 5-7M syscalls and 15M allocations, several seconds, so an unscoped inspect in a large monorepo hits `RequestDeadline` and reports an applicability gap. In a Rust workspace with N `.rs` files there are N or more TOML parses of the root `Cargo.toml`.
- Suggested fix: memoize per walk with `HashMap<(ServerKind, parent_dir), Option<ServerKey>>`, since every file in one directory resolves to the same root. Check the file name against the precomputed marker-name set (`builtin_config_file_names()` already exists) before building any defs, and use `builtin_servers_shared()` instead of `builtin_servers()`. Cache parsed `[workspace]` tables by path+mtime inside `cargo_workspace_contains_crate`.
- Confidence: High.

##### [MEDIUM] Server-root resolution repeated 3-6 times per LSP operation and per edit; Rust parses Cargo.toml each time; part of it under the manager lock
- Location: `manager.rs:63` (`server_key_for_definition`), called from `ensure_server_for_file_detailed` (`:1794`, under lock), `running_server_keys_for_file` (`:1752`, under lock on every write via `lsp_notify_file_changed`), `server_key_for_file`/`client_for_file_mut` (`:5513`, under lock in `send_file_request_unlocked`), `ensure_server_for_file_detailed_unlocked` (`:6507`) and `expected_server_keys_for_file` (`:6585`). `registry.rs:1129` (`resolved_servers`).
- Hot path / frequency: every AFT write (the `notify_file_changed_if_running` path) and every lsp_* command. An edit with `diagnostics: true` (`context.rs:11440-11620`) resolves roots in `expected_server_keys_for_file`, `start_servers_for_file_unlocked`, `notify_file_changed_versioned`, then again in the pull's `start_servers_for_file_unlocked` and in `begin_file_pulls` -> `ensure_file_open` -> `ensure_server_for_file`: about 5 times. Hover/definition/references resolve 3 times (`ensure_file_open_unlocked`, `ensure_file_open`, `client_for_file_mut`) and canonicalize the path 3-4 times.
- Problem: each resolution walks ancestors stat'ing markers (see the HIGH finding). For `.rs` files it also reads and TOML-parses one or more `Cargo.toml` files. When the user config has any `lsp_servers` entry, `servers_for_file` takes the `resolved_servers(config)` branch, which rebuilds every built-in def and merges the user defs on every call.
- Impact: roughly 0.3-2 ms per resolution, so a few ms per edit or command, much of it while holding the manager lock, which serializes concurrent sessions and background drains. It is worse with deep trees and large workspace manifests.
- Suggested fix: add a small LRU in `LspManager` (or a process-wide one, like the TS selection memo) keyed by `(def kind, parent dir, project_root)` -> root, invalidated on watcher events for marker files (`is_config_file_path_with_custom` already identifies them). Cache `resolved_servers(config)` per `Config` generation. Within one request, resolve the key list once and pass it down (`ensure_file_open` already returns `server_keys`; `send_file_request_unlocked` could accept it).
- Confidence: Medium-High (call counts verified; absolute cost estimated).

##### [MEDIUM] Watcher-event forwarding stats and canonicalizes every path under the manager lock, even when no language server is running
- Location: `manager.rs:6782` (`WatcherForwardBacklog::apply`: `path.exists()` per path), `manager.rs:2377` (`forward_watcher_file_events`: `resolve_for_lsp_uri` = `fs::canonicalize` per path, plus a fallback ancestor walk for deleted paths), `manager.rs:2529` (`announce_external_rust_save`: `is_stale_on_disk` per path reads and hashes up to 64 files). Callers: `context.rs:11304-11340` (holds `lsp_manager` via `try_lock` or the helper's `lock()`).
- Hot path / frequency: every file-watcher drain batch (each save by any tool, builds and codegen writing into the tree, branch switches), up to `WATCHER_FORWARD_BACKLOG_CAP` = 2048 paths per apply.
- Problem: `apply` builds the `(path, typ)` list with one `stat` per path, and `forward_watcher_file_events` then canonicalizes every path (`realpath` = an lstat per component) before it looks at `self.clients`. With zero clients, all this work is thrown away. Everything runs with the LSP manager locked. For rust-analyzer, `announce_external_rust_save` can read and BLAKE3-hash up to 64 `.rs` files (the mtime of AFT-written files is unchanged, so `is_stale_against` hashes) while still under the lock.
- Impact: up to about 2048 x (1 + depth) syscalls (~20-40k, around 20-60 ms) per batch under the manager lock during churn, which blocks interactive LSP commands and post-edit diagnostics. Pure waste when no server is running.
- Suggested fix: return early when `lsp.clients.is_empty()` (check under the lock, then drop it). Compute `exists()` and `resolve_for_lsp_uri` before taking the manager lock: `apply` could take a pre-resolved list, and the helper thread can resolve paths before calling `manager.lock()`. Filter by `key.root` prefix on the raw path first, and canonicalize only paths that can match a root.
- Confidence: High.

##### [MEDIUM] Every pull failure sleep-polls up to 100 ms under the manager lock
- Location: `manager.rs:4128` (`cache_post_initialize_exit` -> `client.wait_for_exit(Duration::from_millis(100))`, `client.rs:1866`, which polls with 5 ms `thread::sleep`). Called from `finish_document_pull` (`:3962`) on any `Err` and from `finish_workspace_pull` (`:4078`) on non-timeout errors. Both run under the manager lock (`pull_file_diagnostics_unlocked_inner` re-locks to finish; `pull_workspace_diagnostics_many_unlocked` likewise).
- Hot path / frequency: any per-file pull that times out (busy server, cold tsserver), any server that advertises pull support but returns an error, and any transient JSON-RPC error. These are common during warm-up, and every post-edit diagnostics request and `lsp_diagnostics` file-mode call pulls.
- Problem: the probe is meant to detect "server died after initialize", but when the server is alive (the normal case for a timeout) it waits out the full 100 ms while holding the lock, once per failed server per pull.
- Impact: +100 ms of lock hold and latency per failed pull per server. With 3 servers timing out it is 300 ms beyond the caller's deadline, during which every other manager user waits.
- Suggested fix: call `child.try_wait()` once (non-blocking). Wait only when the reader thread has already reported EOF or the error is a transport/closed error, never for `LspError::Timeout`/`ServerError`. If a grace wait is still wanted, do it after releasing the lock.
- Confidence: High.

##### [MEDIUM] Full-document sync on every change and save, written under the manager lock with a 250 ms stdin deadline
- Location: `manager.rs:2107` (`notify_file_changed_for_server_keys`: `send_full_did_change` + `send_did_save` with `IncludeText`), `manager.rs:1888` (`ensure_file_open` drift resync), `client.rs:2059` (`full_did_change_message`), `writer.rs:79` (`LspWriter::send`, `STDIN_WRITE_TIMEOUT` = 250 ms), `document.rs:39` (store documents `TextDocumentSyncKind::Full`).
- Hot path / frequency: every AFT write to a file that is open in one or more servers, and every drift resync.
- Problem: each edit serializes and pipes the entire file to every server. When a server asked for `includeText` on save, a second full copy follows in `didSave`. The editors already know the edited ranges, but the incremental sync kind is never negotiated or used. The writes run while holding the manager lock (`notify_file_changed*`, `ensure_file_open`, `PendingLspChanges::apply`). For multi-MB files a busy server that is not draining its stdin pipe (64 KiB buffer) can exceed the 250 ms ack deadline. `mark_unresponsive_if_silent` then makes every later send fail fast with "server not responding" until the server emits any message, which turns a slow write into lost diagnostics.
- Impact: for a 1 MB file and 3 servers, about 3-6 MB of JSON per edit and up to 250 ms x servers of lock hold. Small files are unaffected.
- Suggested fix: use incremental `didChange` (`TextDocumentContentChangeEvent` with ranges) when the server advertises `TextDocumentSyncKind::Incremental` and the edit path can supply ranges, or at least when the change is a single contiguous splice found by a cheap common-prefix/suffix diff of old vs new. Send `didSave` without text unless strictly required. Scale the stdin-ack deadline with payload size instead of a fixed 250 ms.
- Confidence: Medium (correctness of the fallback depends on server behavior; the cost itself is certain).

##### [LOW] `didOpen` copies the whole document three times per server
- Location: `manager.rs:1934-1944` (`content.clone()` into `TextDocumentItem` per server in `ensure_file_open`), `manager.rs:2170-2178` (`content.to_string()`), then `client.rs:1735` (`send_notification`: `serde_json::to_value(params)` -> `serde_json::to_string`).
- Hot path / frequency: first open of each file per server (every lsp_* command on a new file, each pull of a new file, each edit to a not-yet-open file).
- Problem: `didChange`/`didSave` already use borrowed serialization (`BorrowedNotification`), but `didOpen` still goes `String clone -> Value (copy) -> String (copy)`.
- Impact: about 3x file size in transient allocations per server per open. Minor for normal files and noticeable for multi-MB generated files.
- Suggested fix: add a borrowed `did_open_message(uri, language_id, version, &str)` like `full_did_change_message`, serialize once and reuse the string for all servers.
- Confidence: High.

##### [LOW] Workspace/document pull reports clone every diagnostic item out of an owned report
- Location: `manager.rs:4100-4104` (`finish_workspace_pull`: `full.full_document_diagnostic_report.items.clone()` and `.result_id.clone()` on the owned `full`), `manager.rs:4182-4186` (`ingest_document_report`: same `items.clone()`).
- Hot path / frequency: every `textDocument/diagnostic` full reply and every `workspace/diagnostic` reply (`lsp_diagnostics` directory mode, post-edit pulls, inspect).
- Problem: the report is consumed by value, but its `Vec<Diagnostic>` (with message strings, related information and data) is deep-cloned before conversion.
- Impact: doubles allocation for workspace pulls with thousands of diagnostics. Not latency-critical.
- Suggested fix: move the fields out (`full.full_document_diagnostic_report.items` / `.result_id`) instead of cloning.
- Confidence: High.

##### [LOW] Redundant file reads and hashes on every notify and LSP query
- Location: `document.rs:103` (`bump_version_with` -> `DiskSnapshot::content_hash` reads the file from disk again and hashes it), `manager.rs:2185-2187`/`2196` (notify path already holds `content`), `document.rs:153` (`is_stale_against` hashes whenever mtime and size are unchanged, i.e. on every query of an unchanged open file up to 4 MB).
- Hot path / frequency: every AFT write to an open document, and every lsp_* command and pull on an open document (`ensure_file_open`).
- Problem: after an edit the content is already in memory, but the store re-reads the file to hash it. Every hover/definition/pull of an unchanged open file reads and BLAKE3-hashes the whole file under the manager lock just to prove it has not changed.
- Impact: one extra full read and hash per edit and per query. BLAKE3 is fast, so this is sub-ms for typical files and a few ms for MB-sized files, under the lock.
- Suggested fix: offer `bump_version_with_content(path, snapshot, &str)` that hashes the in-memory bytes. In `is_stale_against`, trust `(mtime, size)` when the recorded mtime is older than about 2 s (the coarse-mtime race the hash guards against only applies to very recent writes).
- Confidence: High.

##### [LOW] Post-edit waiters wake on every LSP event and wake themselves once more after handling one
- Location: `manager.rs:4864` (`wake_post_edit_waiters` after every `handle_event`), `manager.rs:3444` (`poll_post_edit_diagnostics_wait` handles the event, which wakes its own `wake_rx`), `manager.rs:699` (`PostEditDiagnosticsWait::next_event`; unlike `LspEventWait` it never calls `clear_pending_wake`). Loop in `context.rs:11607-11613`.
- Hot path / frequency: while a post-edit diagnostics wait is active (each edit with diagnostics, up to 10 s). rust-analyzer and tsserver emit many `$/progress` and `window/logMessage` events during analysis.
- Problem: each event takes the manager lock, handles the event and wakes the waiter, which immediately returns `None` from the pending wake, re-locks and re-polls again. That is two lock cycles per event, and every concurrent waiter does the same for events about other files.
- Impact: extra lock churn proportional to event rate (hundreds per second during indexing). Not a hang.
- Suggested fix: drain `wake_rx` (`try_recv`) at the end of `poll_post_edit_diagnostics_wait` while still locked, as `LspEventWait::clear_pending_wake` does. Optionally wake waiters only when the event changed the diagnostics store or a client exited.
- Confidence: Medium.

##### [LOW] `PendingLspChanges::queue_watched` is quadratic; overflow resync stats every open document per server under the lock
- Location: `pending_changes.rs:76` (`self.watched.iter_mut().find(...)` per event, cap 1024), `manager.rs:2208` (`resync_drifted_open_documents`: `is_stale_on_disk` per open document per server store; it re-stats the same path once per server and may hash it).
- Hot path / frequency: only while the manager lock is contended and changes queue up, or after more than 1024 queued documents.
- Problem / Impact: up to about 1M path comparisons in the worst case. On overflow the resync does O(open_docs x servers) stats and hashes plus a read and a `didChange` per drifted document, all under the lock. Bounded and rare.
- Suggested fix: index `watched` with a `HashMap<PathBuf, usize>`. In the resync, take one `DiskSnapshot` per unique path and reuse it across stores, as `ensure_file_open` already does.
- Confidence: High.

##### [LOW] Debug logging of LSP events renders whole payloads before truncating to 512 bytes
- Location: `runtime_drain.rs:3674` (`lsp_params_for_log`: `Value::to_string()` of the full params, then `aft_redactor` regex passes, then truncate to 512 bytes). Called per drained notification and request inside `log::debug!`.
- Hot path / frequency: only when debug logging is enabled for the module, and then for every LSP event, including multi-MB `publishDiagnostics` and workspace pull payloads.
- Problem / Impact: O(payload) serialization plus several regex scans to keep 512 bytes. Off by default, so LOW.
- Suggested fix: serialize into a size-capped writer (stop after about 4 KiB) before redacting.
- Confidence: High.

#### Area summary
The JSON-RPC layer is in good shape: framing is buffered, there is a dedicated writer thread with bounded acks, `params` are moved rather than cloned, `didChange`/`didSave` use borrowed serialization, and responses arrive via per-request channels. Interactive requests, pulls and server spawns already release the manager lock while waiting on servers. The main remaining problem is work that still runs under `Mutex<LspManager>`. The worst case is the rust-analyzer completed-check fingerprint: process spawns plus a whole-workspace stat/hash pass, run synchronously from event handling on every `cargo check` begin/end. Next are filesystem-heavy root resolution and watcher path canonicalization, and a 100 ms sleep-poll on every pull failure. The second theme is uncached server-root resolution. The applicability walk repeats it, and rebuilds every built-in server definition, for every file in the project. That is O(files x defs x depth) syscalls plus O(.rs files) Cargo.toml parses on each unscoped inspect and each configure. Per-request paths repeat it 3-6 times per edit or command. Document sync is always full-text, which is fine for typical files but scales poorly and interacts badly with the fixed 250 ms stdin-ack deadline for very large files.

#### Things checked and found OK
- `transport::read_message`: buffered reader, `read_exact` into a single `Vec`, `String::from_utf8` without copying, 10 MiB cap; one `String` per header line is negligible.
- `ServerMessage::from_json`: parses once and moves `params` out instead of cloning.
- Writer: one `BufWriter` per server, a single flush per frame, header and body written once, a bounded(1) queue with deadline (no unbounded buffering).
- Request/response: `PendingMap` lock held only for insert/remove; `PendingLspRequest::wait` runs without the manager lock; timeouts send `$/cancelRequest`.
- `send_file_request_unlocked`, `pull_file_diagnostics_unlocked`, `pull_workspace_diagnostics_many_unlocked`: requests sent together, waits outside the lock, absolute deadlines.
- Server spawn/initialize (`ensure_server_for_file_detailed_unlocked`): runs without the lock, using per-key reservations.
- `wait_for_rust_check` / `wait_for_push` in `commands/lsp_diagnostics.rs`: event-driven (select on channel and wake), no fixed polling interval, lock released between checks.
- `DiagnosticsStore`: per-file secondary index (`by_file`), LRU via `BTreeMap` order with O(log n) touch, capacity-bounded (default 5000). Status-bar counts are gated by `generation`. The linear scans (`has_authoritative_report_for_server`, `result_ids_for_server`, `for_directory`) are O(entries ≤ cap) and run only per inspect or directory query. `debug_assert_index_consistent` is O(n) per mutation, but only in debug builds.
- `handle_publish_diagnostics`: deserializes from the borrowed `Value` (no clone); `slog_info!` is gated on level before formatting.
- `RegisteredFileWatcher` globs are compiled once at registration; `registered_file_watchers()` snapshots once per batch; forwarding is capped at 512 events per server.
- `typescript_project` selection and shared-root memos are bounded (cleared at capacity). `biome_includes` and `TsconfigMembershipCache` are memoized.
- Pending-change and watcher-forward backlogs: one helper thread per context, deduplicated, capped (no thread-per-change growth).
- Stderr drain: bounded tail ring (`STDERR_TAIL_LINES`), one reused `String` buffer.
- `lsp_hints.rs`: linear filtering over a handful of symbol matches. `developer_tools.rs`: detector and git availability cached in `OnceLock`.
- `child_registry.rs`: `/proc` reads are per tracked child during maintenance only. Shutdown/exit sleep-polling runs at shutdown only, off the hot path.
- `position.rs`: no file reads for position conversion.

---

### 3.12 Persistence, file watching, locks, logging, background drains

Files reviewed: runtime_drain.rs (deep: all of the watcher, search, callgraph, semantic and semantic-refresh drains), watcher/mod.rs, watcher_backend/{mod,inotify,fsevents,windows}.rs (deep), db/mod.rs (pragmas, maintenance_write), db/{compression_events,call_ledger,write_ledger,backups,bash_tasks,bash_watches,access_*,github_read_cache,standing_roots,lifecycle,file_identity}.rs (skim), database_open.rs, fs_lock.rs (deep: acquire, heartbeat, drop), root_cache.rs, artifact_owner.rs, reader_floor.rs, logging.rs (deep: init, TeeWriter, file writer, perf), log_ctx.rs, log_redact.rs, cold_build_limiter.rs (+progress.rs skim), build_breaker.rs, access_policy.rs, path_identity.rs, os_metadata.rs, harness.rs, launch_nonce.rs, cli/* (one-shot CLI, skim), setup_plan.rs, jsonc*.rs, tool_gate.rs, lock_diagnostics.rs. I followed call sites out of the area where a drain calls them (cache_freshness, semantic_index::borrowed_changed_paths, inspect/generated.rs, context.rs helpers, main.rs loop).

Context: in standalone mode every `drain_*` in runtime_drain.rs runs on the **single request/dispatch thread** (main.rs `drain_non_configure_runtime_events`, which runs after each response when no request is queued, and on idle ticks). Work done there delays the next tool call.

#### Findings

##### [HIGH] Semantic corpus-refresh completion strict-hashes the whole project on the dispatch thread
- Location: crates/aft/src/runtime_drain.rs:1801-1835 (`drain_semantic_refresh_events`, `SemanticRefreshEvent::CorpusCompleted` arm) → commands/configure.rs:4176 `borrowed_semantic_refresh_paths` → semantic_index.rs:4919 `borrowed_changed_paths` → `cache_freshness::verify_file_strict`. The same call is at runtime_drain.rs:1167 (`drain_semantic_index_events`, `Ready` arm, read-only/borrowed roots only).
- Hot path / frequency: once per corpus refresh. Corpus refreshes are triggered by every ignore-rule change (5 s quiet window), every watcher overflow rescan (`refresh_project_after_watcher_rescan` → `refresh_project_corpus`), and every lost-load replay. On borrowed roots it also runs once per semantic index load.
- Problem: the `CorpusCompleted` arm runs for **all** roots, not only borrowed ones. It does `walk_semantic_project_files_bounded` (a full tree walk, up to `semantic.max_files`), then `borrowed_changed_paths` calls `verify_file_strict` on every indexed path. Strict verification always reads and blake3-hashes content. All of this runs serially inside `with_current_semantic_refresh_rx`, on the dispatch thread. It then runs `semantic_paths_with_changed_content` over the result, which re-verifies the changed subset. cache_freshness.rs already has `verify_files_strict_bounded`, a bounded rayon pool written for exactly this case ("load-time strict verification can hash every indexed file … beside the bridge's single dispatch thread"), but this path does not use it.
- Impact: a 20-50k-file repo with a few hundred MB of source spends seconds hashing on the request thread after every corpus refresh. Every tool call queued behind it stalls for that long. A rescan storm (a branch switch that overflows inotify) causes a rescan, then a corpus refresh, then this stall.
- Suggested fix: move the snapshot comparison into the corpus worker, which already walked the tree, before it sends `CorpusCompleted`, and ship the precomputed `snapshot_changes`. At minimum, run it only for `shared_artifacts_read_only()` roots, as the `Ready` arm does, and use `verify_files_strict_bounded`. Owner roots built the corpus from the current disk, so the extra walk only finds edits made during the build, and the pending-path replay already covers those.
- Confidence: High (code path verified; strict hashing confirmed in cache_freshness.rs:285-297).

##### [HIGH] inotify backend scans every watched directory on every Remove/rename event
- Location: crates/aft/src/watcher_backend/inotify.rs:172-195 (event loop) → :442 `forget_removed_watch_paths`
- Hot path / frequency: every `EventKind::Remove(_)` and `Modify(Name(From|Both))` event, including **file** removals (one event per path). A `git checkout`/`rebase`/`rm -rf build-output` produces thousands.
- Problem: `watched.iter().filter(|d| paths.iter().any(|p| d.starts_with(p)))` walks the full `BTreeSet<PathBuf>` of watched directories (one per non-ignored directory, often 5k-30k) for each event. `Path::starts_with` compares component by component. The set is ordered, so this could be a range query, and for a file removal no watched directory can match at all. This runs on the single `aft-inotify-backend` thread that also forwards events. While it is busy, the kernel inotify queue (default `max_queued_events` = 16384) fills and overflows, which forces a full rescan.
- Impact: O(removed_paths × watched_dirs). For example, 10k deleted files × 20k watched dirs = 2×10^8 component-wise prefix compares, which is several seconds of backend stall. That turns a checkout into an `IN_Q_OVERFLOW` rescan (full corpus refresh, plus the HIGH finding above).
- Suggested fix: skip events whose kind is `Remove(RemoveKind::File)`. Otherwise, for each removed path `p` use `watched.range(p.clone()..).take_while(|d| d.starts_with(p))`, which costs O(log W + k).
- Confidence: High.

##### [HIGH] Watcher apply slice: semantic-phase content hashing is unbudgeted and runs on the dispatch thread
- Location: crates/aft/src/runtime_drain.rs:2848-2930 (`apply_watcher_slice`, `WatcherDrainApplyPhase::SemanticIndex`) → `semantic_paths_with_recorded_freshness` + `semantic_index::files_with_changed_content` (semantic_index.rs:4281, `indexed_file_content_unchanged` → `verify_file`/`verify_file_strict`)
- Hot path / frequency: every watcher batch, up to `WATCHER_PATH_DRAIN_BATCH_CAP` = 2048 paths per slice.
- Problem: the per-path loop for this phase only pushes into `invalidated_paths`, so it finishes all 2048 paths well inside the 250 ms budget. The batch step that follows then stats every path and, for files whose stat changed (all of them in a real checkout or codegen), reads and hashes the whole file. That step is outside the budget check and on the request thread. It also calls `watcher_path_is_ignored_by_current_matcher` per path (a `config()` Arc clone, a `gitignore()` lookup and an `is_dir()` stat).
- Impact: a branch switch touching 2,000 source files gives about 2,000 file reads and hashes (tens to hundreds of MB) in one drain call. That is hundreds of ms to seconds before the next request is read, despite the 250 ms "slice budget".
- Suggested fix: split `invalidated_paths` into budgeted chunks (re-check `started.elapsed()` between files), or run the freshness comparison on a worker. The semantic refresh worker already re-reads these files, so the main thread could mask on stat change alone and let the worker hash and skip unchanged content. Use the bounded verify pool if it stays inline.
- Confidence: High.

##### [MEDIUM] Search/semantic replay after a build is unbounded inline work on the dispatch thread
- Location: crates/aft/src/runtime_drain.rs:692 `replay_search_index_pending_updates` (called from `drain_search_index_events`, :795-801); runtime_drain.rs:1196-1210 (`Ready` arm: `semantic_paths_with_changed_content` over all paths pending during the build).
- Hot path / frequency: once per finished search/semantic build. Pending paths accumulate for the whole build duration, which can be minutes on a cold start.
- Problem: every path that changed during the build is replayed inline: `path.exists()`, an ignore check (stat), and `index.update_file` (read the file and re-trigram it), or a strict/stat hash for semantic. There is no cap and no slice budget, unlike the watcher drain. A checkout or `npm install` into a non-ignored directory during the cold build leaves tens of thousands of pending paths. (`pending_search_index_paths` is a set, so it is deduped, but unbounded.)
- Impact: one drain call that does O(pending files) file reads; multi-second stalls are plausible on big repos.
- Suggested fix: replay before publishing, on the build thread (have the worker take the pending set just before sending), or past a threshold mark the index for a background refresh instead of replaying inline.
- Confidence: Medium-High.

##### [MEDIUM] Watcher drain slice blocks the next request for up to the 250 ms budget, and single units can overshoot it
- Location: crates/aft/src/runtime_drain.rs:31-32 (`WATCHER_DRAIN_SLICE_BUDGET = 250ms`), :2582 `apply_watcher_path_phase`, :2813-2846 (SearchIndex phase); main.rs:693-700 (drain runs after each response when `collect_queued_lines` returned 0).
- Hot path / frequency: every request during and after a file-change burst.
- Problem: queued stdin lines are checked only before the drain starts. A request that arrives just after a slice begins waits up to the full 250 ms, plus the in-flight unit (one `prepare_search_update` file read, a callgraph `is_generated_file` probe, or the unbudgeted semantic batch above). In the SearchIndex phase each path also takes the `search_index` read lock, reads the file, then takes the **write** lock again per path. That is 2048 write-lock round-trips per slice, and in subc mode they contend with concurrent query readers.
- Impact: p99 tool-call latency is inflated by about 250 ms during edit/build bursts. Lock churn affects concurrent readers.
- Suggested fix: cut the slice to about 25-50 ms, or pass a "requests queued" probe (`line_rx` non-empty) into `apply_watcher_path_phase` and yield when it trips. In the SearchIndex phase, prepare updates for a chunk without the lock and apply them under one write lock.
- Confidence: Medium.

##### [MEDIUM] Each filesystem lock acquire spawns an OS thread and re-reads /proc; used on every edit's backup
- Location: crates/aft/src/fs_lock.rs:495-552 (`create_new_lock`), :1542 `process_identity`, :231 `Drop for LockGuard` (join); caller backup.rs:3487 (per-path backup lock, plus `acquire_stack_disk_locks` for N paths), also checkpoint.rs, root_cache.rs, symbol_cache_disk.rs.
- Hot path / frequency: per edit/write tool call (backup snapshot), N times for multi-file edits.
- Problem: every acquisition does: `process_identity(own pid)` (reads `/proc/self/stat` and `/proc/sys/kernel/random/boot_id`, constant for the process), creates a temp file, writes JSON, `hard_link`s, unlinks, and **spawns a dedicated heartbeat thread**. Release unparks and joins that thread, then reads the lock back and unlinks it. That is about 10 syscalls plus a thread create/join per lock. artifact_owner.rs already shows the cheaper design: one shared heartbeat thread for all leases (`register_heartbeat`).
- Impact: roughly 0.2-1 ms per lock. A 50-file batch edit pays that 50× with 50 concurrent threads. Contended acquires poll at `POLL_INTERVAL_MS` = 100 ms, which adds latency when two sessions edit the same file.
- Suggested fix: cache own `ProcessIdentity`/hostname in a `OnceLock`. Move heartbeats to a single shared ticker thread (as artifact_owner does) that holds a registry of live guards. Short-lived locks (backups) could skip the heartbeat entirely if hold time is bounded well under `STALE_HEARTBEAT_MS`.
- Confidence: Medium (cost per op is estimated; frequency verified).

##### [MEDIUM] `pending_paths` continuation is unbounded and only deduped per 2048-path slice
- Location: crates/aft/src/runtime_drain.rs:3252-3260 (extend), :3462-3474 (per-slice `HashSet` dedupe); context.rs:1547 (`pending_paths: VecDeque<PathBuf>`).
- Hot path / frequency: every dispatch batch (the filter thread flushes every 250 ms, up to 1024 paths).
- Problem: the filter dedupes only within one 250 ms window. When the drain falls behind (slices are at most 2048 paths per idle/request turn and spread across 7 phases), a hot file set rewritten repeatedly (a watcher-mode compiler writing into a non-ignored `dist/`, a log file in the repo) is re-appended every window. The deque grows without bound, and each duplicate is re-run through all 7 phases in later slices. Ingest also does an `Arc` clone and `RwLock` read per path (`queued_path_is_ignored_by_matcher(&ctx.shared_gitignore(), path)`).
- Impact: memory grows linearly with storm duration, and redundant phase work delays convergence.
- Suggested fix: make `pending_paths` an `IndexSet`, or keep a `HashSet` beside the deque, so a path is queued once until it is taken. Above a cap (for example 50k), collapse to `rescan_required`. Hoist `ctx.shared_gitignore()` and the read guard out of the per-path filter.
- Confidence: Medium-High.

##### [MEDIUM] Semantic refresh completion re-hashes every embedded file on the dispatch thread
- Location: crates/aft/src/runtime_drain.rs:1752-1790 (`SemanticRefreshEvent::Completed`: `verify_file_strict` for each `updated_metadata` entry).
- Hot path / frequency: after every semantic refresh batch (each watcher batch that touched semantic sources).
- Problem: the worker has just read and hashed these files. The drain strict-hashes them all again serially on the request thread to detect a race with a newer edit. Any newer edit already produces a watcher event, which re-invalidates the file.
- Impact: doubles the hashing cost of a refresh. With a checkout-sized batch (thousands of files), hundreds of ms on the dispatch thread.
- Suggested fix: compare stat (size+mtime+inode) only, and rely on the watcher for content races, or do the re-verification on the worker immediately before sending.
- Confidence: Medium.

##### [LOW] Generated-file probe can read an entire minified file per watcher path
- Location: crates/aft/src/runtime_drain.rs:2497 `watcher_path_is_generated_for_callgraph` → inspect/generated.rs:96-110 `first_lines_have_generated_marker` (`BufReader::lines().take(5)`).
- Hot path / frequency: Callgraph apply phase, once per changed callgraph-indexed path, on the dispatch thread. Again on store install (runtime_drain.rs `drain_callgraph_store_events`) and in `refresh_callgraph_store_for_watcher`.
- Problem: `lines()` reads up to the next newline, so a single-line minified bundle (`*.min.js`, built `dist/*.js` that is not gitignored) is read completely into a String just to check for a marker. The probe is also repeated for the same path in later calls, and the callgraph worker reads the file again anyway.
- Suggested fix: read a fixed prefix (for example `take(4096)` bytes) instead of lines. Optionally decide generated-ness inside the callgraph worker, from the source it already reads (`is_generated_file_from_source`).
- Confidence: High (code), Low-Medium (impact depends on repo).

##### [LOW] Per-path singleton calls in watcher apply phases defeat batch-friendly APIs
- Location: crates/aft/src/runtime_drain.rs:2720-2770 (PendingTier2 and PendingIndexes phases): `ctx.add_pending_tier2_paths([path])`, `ctx.add_pending_search_index_paths([path])`, `ctx.add_pending_semantic_index_paths([path])`.
- Hot path / frequency: per watcher path (up to 2048 per slice).
- Problem: each call allocates a Vec and takes its mutex. `add_pending_search_index_paths` also calls `cache_freshness::invalidate_verify_memo`, which takes a global mutex and scans all memo entries for the root, so the cost is O(paths × memo entries). `add_pending_tier2_paths` calls `config()` and `note_tier2_input_paths_changed`, which, while Tier-2 is in flight, runs `canonicalize_normalized(project_root)` (a realpath syscall) for every single path.
- Suggested fix: collect the slice's paths in the closure and call each `add_*` once per phase chunk. Invalidate the verify memo once per slice (`apply_watcher_slice` already does this at its top, so the per-path call is redundant).
- Confidence: Medium.

##### [LOW] Logging: stderr is written synchronously on the caller's thread for every line
- Location: crates/aft/src/logging.rs:1192-1205 (`TeeWriter::write`), :754-800 (`init`: env_logger `Target::Pipe`).
- Hot path / frequency: every enabled log line (info is the default) from any thread, including the dispatch thread (`invalidated N files`, watcher warnings, slow-call warnings).
- Problem: the durable-file copy is correctly offloaded to a bounded channel, but the stderr copy is an unbuffered `write_all` under env_logger's global pipe mutex, on the logging thread. In standalone mode stderr is a pipe drained by the Node plugin. If the plugin's event loop is busy, the 64 KB pipe buffer fills and the request thread blocks inside a log call, and every other logging thread queues on the same mutex. Each line is also formatted three times: the inner `format!`, the outer `format!` with the session prefix in `slog_*`, and env_logger's own format. A slow tool call also builds its full ~25-field message even when its level (Debug for Plumbing) is disabled (logging.rs:2117-2150).
- Suggested fix: send stderr lines through the same writer thread (or a second bounded channel) with drop-on-full. Gate the slow-call `format!` on `log_enabled!(level)`.
- Confidence: Medium (blocking needs a slow reader; mechanism verified).

##### [LOW] Cold-build limiter waiters busy-poll with a 100 ms sleep
- Location: crates/aft/src/cold_build_limiter.rs:347 (`acquire_blocking_while_with_limiter` loop).
- Hot path / frequency: every queued cold build or maintenance build when slots are full (multi-root startup, standing roots).
- Problem: waiters `thread::sleep(100ms)` and retry instead of waiting on a Condvar. A freed slot stays idle for up to 100 ms, and with many waiters they wake on a thundering-herd schedule.
- Suggested fix: use a `Condvar` (or `parking_lot::Condvar`), notified on permit drop, with a timeout so the `admitted()`/`cancelled()` predicates are still re-checked.
- Confidence: High (code), Low (impact).

##### [LOW] Backend→filter channels are unbounded; storms buffer in memory
- Location: watcher_backend/inotify.rs:73, fsevents.rs:87 and the `tx: mpsc::Sender` passed in (std unbounded `mpsc::channel`); Windows has no backend exclusion (windows.rs comment at :411), so all `target/` events go through it.
- Problem: if the filter thread (which canonicalizes each path, out of area) falls behind, every raw `notify::Event` (Vec<PathBuf> + allocation) queues without bound. On Windows a `cargo build` storm becomes hundreds of thousands of queued events.
- Suggested fix: use a bounded channel. On `Full`, set a "rescan needed" flag and drop events, which matches the semantics the drain already supports.
- Confidence: Medium.

#### Area summary
Persistence and logging are generally in good shape. SQLite uses WAL + `synchronous=NORMAL`, maintenance writes are batched and run off-thread with busy-budget retries, compression aggregates are cached by watermark, and the durable log goes to a batched background writer. The real costs sit in the **background drains that run on the single dispatch thread**. Several of them do O(files) content hashing or re-indexing without any budget: semantic corpus completion walks and strict-hashes the whole project for every root, the watcher semantic phase hashes a whole 2048-path batch after its budget check, and the post-build search/semantic replays and refresh-completion re-verification do the same kind of work. Each turns a file-change burst into multi-second tool-call stalls. On Linux, the inotify backend's O(removes × watched dirs) bookkeeping during checkouts is likely to overflow the kernel queue and force a full rescan, which then triggers the corpus-completion hashing above. Fixing the inotify range query and moving the hashing off the dispatch thread (or into the bounded rayon verify pool) gives the largest wins.

#### Things checked and found OK
- SQLite pragmas (db/mod.rs:761): WAL, `synchronous=NORMAL`, `foreign_keys`, busy_timeout 5 s; migrations in IMMEDIATE transactions.
- `maintenance_write` releases the shared connection mutex between busy retries; write-ledger fold and compression retention are throttled to once per `maintenance_interval` and run on spawned threads.
- Compression aggregates: watermark-cached, advanced in place on local inserts; indexes exist for the session/project lookups. Retention deletes are batched (500) inside one transaction with `prepare_cached`.
- Most DB helpers use `conn.execute` (not statement-cached), but they are per-tool-call single statements, so the overhead is negligible.
- Durable log file writer: bounded channel (4096, drop-on-full), batches of 256 lines, one flush per batch, rotation is size-tracked in memory (no stat per write). The redaction prefilters with `contains` before running regexes, and the regexes are `LazyLock` statics.
- `slog_*` macros check `log_enabled!` before formatting.
- artifact_owner heartbeats: one shared thread, no fsync, in-place rewrite. root_cache read markers piggyback on read paths and do not spawn threads.
- FSEvents backend excludes ignored subtrees at the API and only restarts the stream when exclusions actually change. Its callback work per event is O(1).
- Windows backend rearms before translating, so the copy per completion is bounded by the buffer size.
- Watcher filter coalesces per 250 ms window before dispatch, and ignore-rule refresh is debounced (5 s quiet window).
- `drain_callgraph_store_events` defers cold builds off-thread (the comment documents the earlier inline-build hang); `refresh_project_corpus` spawns search rebuilds.
- Log sweep and write-ledger fold are gated to at most one spawn per interval.
- reader_floor, launch_nonce, setup_plan, jsonc*, tool_gate, harness, access_policy, path_identity, os_metadata, build_breaker, lock_diagnostics: startup, one-shot or tiny per-call work; nothing material.
- cli/: one-shot commands; the polling loops there (warmup `POLL_INTERVAL`, profile sleeps) are fine for a CLI.
- `watcher/mod.rs` `read_stable_file` / `WatcherCollector` currently have no production callers.

---

### 3.13 TypeScript bridge (packages/aft-bridge/src)

Files reviewed: all non-test files in `packages/aft-bridge/src`. Deep-read: `bridge.ts`, `subc-transport.ts`, `pool.ts`, `revivable-transport.ts`, `transport-factory.ts`, `project-identity.ts`, `binary-identity.ts`, `durable-log.ts`, `active-logger.ts`, `config-watch.ts` (watch core), `resolver.ts` (call sites), `bash-host-fallback.ts`, `location-lifecycle.ts` (wrappers). Skimmed: `lifecycle-registry.ts`, `downloader.ts`, `npm-resolver.ts`, `onnx-runtime.ts`, `onnx-probe.ts`, `migration*.ts`, `version-probe.ts`, `config-tiers.ts`, `feature-config.ts`, `path-aliases.ts`, `bash-hints.ts`, `callgraph-format.ts`, `zoom-format.ts`, `tool-format.ts`, `coerce.ts`, `edit-summary.ts`, `semantic-status.ts`, and the small helpers. To check how often things run, I also looked at the call sites in `opencode-plugin/src/tools/_shared.ts` and `opencode-plugin/src/index.ts`.

Overall: this area has already had a lot of performance work. NDJSON framing is linear, stderr is bounded, binary hashing is off-thread, and resolution is cheap because of the identity sidecar. I found no CRITICAL or HIGH problems. What is left is redundant sync I/O on every call, per-line log appends, and retention/fan-out problems in the subc session cache that grow over the life of a long-running host.

#### Findings

##### [MEDIUM] Project root is canonicalized 3–4 times per tool call with JS `realpathSync` (plus a `statSync` per subc call)
- Location:
  - `project-identity.ts:26-30` (`canonicalizeProjectRoot` → `realpathSync(trimmed)`)
  - `revivable-transport.ts:52-53` (`RevivableTransportPool.getBridge` canonicalizes)
  - `revivable-transport.ts:293-296` (`toolCallOnProject`/`send` → `pool.getBridge(projectRoot)` with a key that is **already canonical**)
  - `pool.ts:233` / `pool.ts:527` (`BridgePool.getBridge` → `normalizeKey` → canonicalize again)
  - `subc-transport.ts:1545-1546` (`SubcTransportPool.canonicalRoot`, on every `getBridge`)
  - `transport-factory.ts:120-126` (when reaping is enabled, `satisfyLifecycleDemand` calls `activePool.getBridge` a further time)
  - `subc-transport.ts:2222` → `rootCanAttach` `statSync(root)` at `:1555-1558` (every subc `routeRequest`)
  - Caller: `opencode-plugin/src/tools/_shared.ts:153-165` (`projectRootFor` canonicalizes too) and `bridgeFor` (`existsSync(projectRoot)`)
- Hot path / frequency: every tool call and every `send()`, in both transports.
- Problem: `node:fs.realpathSync` (not `.native`) is Node's JS implementation. It `lstat`s every path component and keeps no cache across calls. A typical call runs it in the plugin (`projectRootFor`), in the revivable wrapper, and again in the concrete pool, plus once more with lifecycle reaping. Subc adds a `statSync`, and the plugin adds an `existsSync`. All of this is synchronous on the single JS thread the host shares with every plugin, and most of it re-canonicalizes a string that is already canonical (the wrapper passes `this.projectRoot`, which is already the key).
- Impact: about 3–4 × depth(root) `lstat` calls plus 1–2 `stat` calls per call, so roughly 15–30 syscalls for a root at depth 5. That is tens of µs locally. On NFS, SMB, FUSE or iCloud-backed directories each blocking stat can cost ms, which adds to every call's latency and stalls the host's event loop.
- Suggested fix: canonicalize once at the outermost boundary and pass a branded `CanonicalRootPath` downward. Give `BridgePool.getBridge` / `SubcTransportPool.getBridge` a fast path for already-canonical keys (for example, check `this.bridges.get(projectRoot)` / `this.transports.get(projectRoot)` before canonicalizing). Alternatively, memoize `canonicalizeProjectRoot` in a small LRU keyed by the raw string, and use `realpathSync.native` (one syscall). For `rootCanAttach`, cache a positive result for a short TTL (for example 1–2 s) or rely on the lifecycle sweep and the daemon's `config_divergence` reply instead of a stat per request.
- Confidence: High (call chain verified). The magnitude depends on the filesystem.

##### [MEDIUM] Durable plugin log does one `appendFile` (open+write+close) per record, serialized, and drops lines under bursts
- Location: `durable-log.ts:64-108` (`RotatingLogSink.append`), `:159` (`appendFile` per record). Fed by `bridge.ts:1650-1657` (`flushStderrBuffer` → `logVia` for every Rust stderr line) and all plugin `log/warn/error` calls (`opencode-plugin/src/logger.ts`, `pi-plugin/src/logger.ts`).
- Hot path / frequency: every forwarded `aft` stderr line, which is heavy during index builds, LSP chatter, or with `RUST_LOG=debug`, plus every plugin log line.
- Problem: each record becomes its own link in a promise chain. Each link runs `fs.promises.appendFile`, which opens the file, writes, and closes it. Writes are fully serialized, so throughput is bounded by about 3 threadpool syscalls per line. When the queue exceeds 4096 records or 1 MiB, new lines are **dropped** (an overflow note is written instead), so a burst of Rust diagnostics, the most useful lines when debugging, gets lost.
- Impact: about 3 syscalls per log line on the libuv pool, plus a promise and closure per line. Under bursts (thousands of lines per second), lines are dropped and threadpool contention slows other async fs work in the host.
- Suggested fix: batch the pending records. Have `append` push to an array, and have one drain task join everything queued and write it with a single `appendFile`, or better, a persistent `FileHandle`/`WriteStream` opened in append mode and reopened on rotation. Then the queue limits only matter if the disk truly stalls.
- Confidence: High.

##### [MEDIUM] Subc session cache: resumed sessions are never reaped, keep a tool route plus a bg_events subscription forever, and each request does an O(sessions) scan
- Location:
  - `subc-transport.ts:1877-1897` (`reapIdleSessions` skips any record with `!record.observedFromStart`)
  - `subc-transport.ts:1063-1161` (`ensureBgSubscription`: one held-open route and subscription per session)
  - `subc-transport.ts:2555-2558` → `deleteSessionIfEmpty` (`:2086-2104`) → `stopUnusedHostScopeRetry` (`:2781-2784`) → `unscopedHostRecord` (`:2757-2773`, a linear scan of `this.sessions`)
  - `hostScopeCache` (`:2718-2722`) and `hostScopeOutcomes` (`:2854-2855`), which only shrink on `closeSession`/`invalidateHostScope`/`shutdown`
  - Caller: `opencode-plugin/src/index.ts:862`, where `pool.closeSession(input.directory, sessionID)` passes the plugin-init directory, not the session's directory
- Hot path / frequency: the scan runs in the `finally` of every subc `routeRequest`. Retention grows by one entry per distinct session that ever calls a tool during the host's lifetime.
- Problem: only sessions whose `session.created` event the host saw (`observeSessionStart`) can ever be idle-reaped. Every resumed session (`opencode -s`, sessions restored after a host restart, Pi, which never calls `observeSessionStart`/`closeSession`) keeps its `SessionRecord`, its tool route, its dedicated bg_events route and subscription (with its reconnect loop), and its host-scope cache entries until `session.deleted`. In OpenCode, `closeSession` canonicalizes `input.directory` (the plugin-init cwd), so for a session rooted in another project (OpenCode Desktop / `opencode serve` serving several projects), the identity key does not match and the close is a no-op. Meanwhile each request's `finally` scans all sessions whenever host scopes are enabled (always the case for opencode/pi).
- Impact: daemon-side routes and subscriptions plus plugin memory grow linearly with resumed sessions over a long-running server, and each of them reconnects on daemon restart (see the next finding). The per-call scan is O(S). That is small while S is in the tens, but S has no upper bound.
- Suggested fix: allow idle reaping of unobserved sessions that have no known background work. Releasing a carrier is already safe, because the next request reopens it and subscription startup forces a drain. At minimum, reap the bg_events subscription and tool route while keeping a lightweight record. In the OpenCode `session.deleted` hook, close using the session's stored directory (`getSessionDirectoryCached(sessionID)`). Index sessions by `session` (`Map<session, Set<IdentityKey>>`) so `unscopedHostRecord`, `promoteHostScope` and `invalidateHostScope` touch only that session's records. Delete `hostScopeCache`/`hostScopeOutcomes` entries when the last record for a session is detached.
- Confidence: Medium-High. The retention is verified in the code. How large S gets depends on host usage.

##### [MEDIUM] bg_events reconnect fan-out: every session's loop retries `connect` on its own while the daemon is down, then all resubscribe and force a drain together
- Location: `subc-transport.ts:803-815` (`BgSubscription.run` → `acquireClient` → `ensureClient`), `:440-442` (`reconnectBackoffMs`, capped at 2 s, per subscription), `:2598-2636` (`ensureClient`: single-flight only while a connect is in flight, with no shared failure backoff), `:915` (`onNudge(false)` after every successful subscribe).
- Hot path / frequency: whenever the daemon restarts or the socket drops, multiplied by the number of retained sessions (see the previous finding).
- Problem: each session's subscription loop has its own exponential backoff, capped at 2 s. `ensureClient` shares only a connect that is *currently* in flight. Once a connect fails, `this.connecting` is cleared, so loops waking at different moments each start a fresh `SubcClient.connect`, which re-reads the connection file and attempts TCP plus the handshake. With S sessions, that is up to S connect attempts every 2 s for as long as the daemon is down. When it comes back, S subscriptions reopen routes at once, and each `onNudge(false)` makes the host issue a `bash_drain_completions` request, so 2S round trips land on a daemon that is still warming up. Tool calls (`routeRequest`) likewise attempt a connect on every call, with no shared not-before time.
- Impact: with S=100, about 50 connection attempts per second during an outage, then a burst of about 200 route.open and drain requests at recovery. Logging stays rate-limited, so the cost is CPU, sockets and daemon load rather than log spam.
- Suggested fix: add a pool-level connect backoff (a shared `connectNotBefore`/delay like `RevivableTransportPool.ensureActivePool`'s `revivalRetryNotBefore`) that every `ensureClient` caller honors. Add jitter to `reconnectBackoffMs`. Coalesce post-resubscribe drains, for example skip the forced drain for sessions with no known background work (`backgroundTasks.size === 0 && !unknownBackgroundWork`), or stagger them.
- Confidence: Medium.

##### [LOW] `BinaryBridge.configureWarningClients` gains an entry for every session and is almost never cleared
- Location: `bridge.ts:951-953` (set on every `send` that carries `configureWarningClient` and a session id, which the OpenCode and Pi `_shared.ts` call helpers pass on every call), with deletes only at `bridge.ts:1260-1262` / `:1289-1291` (only when configure warnings are actually delivered).
- Hot path / frequency: every tool call sets it again. Entries are only freed on bridge `shutdown()`.
- Problem: in the common case (configure has no warnings, so no `configure_warnings` frame arrives), each session's entry stays for the bridge's lifetime. The value is a shared host client object, so the per-entry cost is small, but the map grows by one per distinct session on long-lived bridges (30-minute idle timeout, often never idle).
- Impact: small memory growth in proportion to session count, plus a Map write per call.
- Suggested fix: record the client only while `!this.configured` or while a deferred warning frame is still pending, and clear the map when the bridge has configured and the deferred frame has arrived or timed out. Alternatively, cap it with an LRU.
- Confidence: High.

##### [LOW] Per-call timer churn on the warm path
- Location: `bridge.ts:1796` → `scheduleRestartCountReset` (`:1961-1969`), which does a `clearTimeout` plus a new 5-minute `setTimeout` for **every** matched response. `subc-transport.ts:2238-2243` and `:2337-2341` each create a deadline `setTimeout` per call to race `ensureClient()` and `openRoute()`, even when the client and route are already cached and the promise resolves immediately.
- Hot path / frequency: every tool call.
- Problem: three timer create/cancel pairs per call, plus extra promise allocations, for state that has not changed (the client and route are cached, and the reset only needs re-arming after a crash).
- Impact: microseconds per call. Not a bottleneck, but pure overhead on the hottest path.
- Suggested fix: arm the restart-count reset only when `_restartCount > 0`. In `routeRequest`, return early without a race when `this.client` is set (sync check) and `record.routeEntry?.handle` is live for that client.
- Confidence: High (the impact is low).

##### [LOW] `BridgePool.evictLRU` cannot evict when every bridge is busy, so the pool can grow past `maxPoolSize`
- Location: `pool.ts:249-253`, `:310-326`.
- Hot path / frequency: when a new project root arrives and all 8 existing bridges have pending requests or outstanding background bash tasks.
- Problem: `evictLRU` skips busy bridges, and `getBridge` then adds a new bridge anyway, so the cap is soft. Long-running background bash tasks keep bridges permanently "busy". Each bridge is a full `aft` process with indexes, LSP and ONNX, so going over the cap has a real RAM cost.
- Impact: unbounded number of processes only in unusual multi-project setups with many concurrent background tasks.
- Suggested fix: log when the cap is exceeded and evict on the next cleanup tick once a bridge becomes idle. Alternatively, count only bridges with no background tasks toward the cap.
- Confidence: Medium.

#### Area summary
The standalone NDJSON path is in good shape. Partial stdout lines are stored as chunks and joined once (scanning is linear, with a 64 MB guard), stderr partials are capped at 64 KB, every pending request's timer is cleared on response, timeout, crash or shutdown, abort listeners are removed through `onSettled`, idle child handles are unref'd, crash restarts back off and are capped, and binary identity and fingerprinting never hash or exec on the host thread. Binary resolution (`findBinary*`) runs only at startup or download and is a stat plus a sidecar read. The remaining costs are: (1) redundant synchronous path canonicalization and stats on every call, across the plugin → revivable wrapper → concrete pool layers; (2) a log sink that opens and closes the file for every line and drops lines under bursts; and (3) the subc session cache, where resumed sessions keep routes and subscriptions for the life of the host, per-request work scales with the session count, and reconnects fan out per session.

#### Things checked and found OK
- `bridge.ts` `onStdoutData`/`flushStdoutBuffer`: chunk-list buffering, newline search runs only over new data, single `join` per line, malformed-frame log is truncated to 512 chars; `processStdoutLine` handles push frames before looking up the id.
- `onStderrData`: bounded 64 KB fragments; the stderr tail is a 20-line ring.
- Pending map: removed on response, timeout, write failure, crash, kill (`rejectAllPending`), shutdown and binary swap; `handleTimeout` rejects siblings immediately; `outstandingBackgroundTaskIds` is cleared on crash or timeout.
- Timers: the restart timer, `restartResetTimer`, the pool cleanup interval, the subc session sweep, the dispatch-probe interval (one per client, ref-counted), host-scope retries, the lifecycle sweep and the config-watch revalidate are all unref'd and/or cleared.
- `syncEventLoopHold` calls ref/unref only on a state change.
- Binary fingerprint check: only during an idle maintenance tick (once a minute per bridge). It is a stat plus a small sidecar read; the sidecar is read twice in `peekBinaryContentHash` (`binary-identity.ts:330-333`), which is trivial. The async 85 MB hash is cached by stamp.
- `RevivableTransportPool` revival has exponential backoff, and pending session-start hints are bounded at 256; subc `observedStarts` is bounded at 256.
- `OutputTail` (host bash fallback) is a bounded chunk list with no per-chunk concatenation; the migration subprocess buffers are capped at 4000 chars.
- `config-watch.ts`: debounced (at most 1 s), reads two small files only when an event fires, and revalidates every 2 s with a few stats. There is one watcher per plugin instance.
- Formatters (`callgraph-format`, `zoom-format`, `tool-format`, `path-aliases`, `coerce`) do linear work per response, with no quadratic string building found.
- `cachedFileSha256Sync` (a sync hash) is called only from the plugins' LSP install paths, which are outside this area and not per call.
- The 64 MB stdout guard kills the bridge on a single oversized line. That is a functional limit, not a performance issue.

---

### 3.14 TypeScript harness plugins and CLI

Files reviewed:
- `packages/opencode-plugin/src`: deep-read `index.ts` (all hooks), `tools/_shared.ts`, `tools/hoisted.ts` (read/write/edit/hashline/apply_patch/delete/move), `tools/permissions.ts`, `tools/bash_watch.ts`, `tools/search.ts`, `bg-notifications.ts` (ingest/append/drain/stateFor), `bash-wait-detach.ts`, `tool-perf.ts`, `logger.ts`, `config.ts` (load + `createUnchangedConfigLoader`), `config-live-reload.ts`, `shared/session-directory.ts`, `shared/subagent-detect.ts`, `shared/rpc-client.ts`, `shared/rpc-server.ts`, `shared/rpc-notifications.ts`, `rpc/register.ts`, `entry/server-runtime.mjs`, `v2-tool-hooks.ts`, `tools/definitions/v2.ts`, `permissions/v2.ts`, `hooks/auto-update-checker/index.ts`, `notifications.ts`, `lsp-project-relevance.ts`, `tui/v2.tsx` (status refresh). Skimmed the rest (tools/*, tui/*, lsp-*, workflow-hints, normalize-schemas, shutdown-hooks).
- `packages/pi-plugin/src`: deep-read `index.ts` hooks (`session_start`, `before_agent_start`, `tool_result`, `turn_end`, `input`), `tools/_shared.ts`, `tools/hoisted.ts` (write/edit/hashline), `tools/diff-format.ts`, `tools/render-helpers.ts`, `bg-notifications.ts` (`appendToolResultBgCompletions`), `session-kind.ts`, `dialogs/status-dialog.ts`, and the `tools/bash.ts` watch loop. Skimmed the rest.
- `packages/aft-cli/src`: `index.ts` (startup), `commands/doctor.ts` (run/fix flow), `lib/diagnostics.ts`, `lib/fs-util.ts`, `lib/binary-probe.ts`, `lib/harness-select.ts`, `adapters/{index,opencode,pi,omp}.ts`, `lib/legacy-storage.ts`. Skimmed the other commands and setup modules.
- Test files (`__tests__`, `*.test.ts`) were ignored.

#### Findings

##### [HIGH] Every mutation round trip ships the full before and after file text, the preview round trip included, then the host re-diffs it
- Location: `crates/aft/src/subc_translate.rs:1505` (`insert_common_mutation_flags` always sets `include_diff_content: true`, so `edit.rs:284 compute_diff_info` returns full `before`/`after` up to 512 KB each). Consumers: `packages/opencode-plugin/src/tools/hoisted.ts:606-655` (write) and `:936-983` (edit); `packages/pi-plugin/src/tools/hoisted.ts:1002-1010` (`buildMutationResult` → `formatDiffForPi`).
- Hot path / frequency: every `write`/`edit`/`apply_patch` tool call. OpenCode write and edit make two `tool_call` round trips (`preview: true`, then the real call), and both carry the flag.
- Problem: For a file just under the 512 KB cap, each round trip returns about 1 MB of JSON-escaped text: the whole file twice. The bridge `JSON.parse`s it on the host event loop. On OpenCode the preview response's `before`/`after` is discarded (only `preview.preview_diff` is read). The final response's `before`/`after` is used only to rebuild a unified diff in TypeScript (`buildUnifiedDiff`: an LCS table over the changed middle, capped at 5000 lines, about 50 MB and 250 ms at the cap). That diff already exists in `preview.preview_diff`, which the code falls back to anyway in the `truncated` case. On Pi, `formatDiffForPi` runs `diff.diffLines` (Myers, O((N+M)·D)) with no line cap or `maxEditLength`. A whole-file rewrite of a few thousand lines can block Pi's UI thread for hundreds of ms to seconds. The resulting unbounded diff string is also stored in `details.diff`, which persists in the session.
- Impact: About 4x file size of avoidable JSON per OpenCode edit (2x on Pi), plus an O(changed²) or O(N·D) diff on the host thread. Edits to large generated or markdown files cause visible stalls.
- Suggested fix: Have Rust return the rendered unified diff (it already produces `preview_diff`, hunk-scoped) and send `before`/`after` only when a host explicitly asks. Never send them for `preview: true`. In OpenCode, use `preview.preview_diff`, or a diff from the final response, for `metadata.diff`/`filediff`. In Pi, pass `{ maxEditLength }` or `timeout` to `diffLines`, or consume a Rust-rendered diff and cap the stored `details.diff`.
- Confidence: High (the flag is unconditional in `insert_common_mutation_flags`, and both host consumers were read).

##### [MEDIUM] Hashline edit makes an unused `preview` round trip (both plugins)
- Location: `packages/opencode-plugin/src/tools/hoisted.ts:776-801` (`createHashlineEditTool`); `packages/pi-plugin/src/tools/hoisted.ts:768-795`.
- Hot path / frequency: every `edit` call when `edit_mode: "hashline"`.
- Problem: The sequence is `hashline_preflight` → permission asks → `edit {preview:true}` → `edit`. The preview result is used only for `if (preview.success === false) throw …`. The permission ask is raised before the preview and gets no diff from it. The real `edit` is a two-phase verify-then-apply transaction (ARCHITECTURE.md) that returns the same failure envelope. The preview is a full extra executor round trip: it re-reads, re-verifies and composes every file in the patch, and its response carries `before`/`after` content (see the HIGH finding).
- Impact: Three bridge round trips per hashline edit instead of two, about 33% extra latency and Rust work per edit, and more for multi-file patches.
- Suggested fix: Drop the preview call in both plugins. If it guards a specific race, document it and request a counts-only preview.
- Confidence: Medium (verify that nothing on the Rust side depends on the preview, such as a cache warm-up).

##### [MEDIUM] Synchronous `bash_watch` waits poll `bash_status` (100 ms → 1 s), with an extra RTT and up to 64 KB per regex check
- Location: `packages/opencode-plugin/src/tools/bash_watch.ts:386-560` (`waitForBashStatus`), `:637` (`matchRegexWithBridge`); `packages/pi-plugin/src/tools/bash.ts:1410-1470`, `:1669`; poll schedule `packages/aft-bridge/src/bash-hints.ts:105` (`watchPollDelayMs`).
- Hot path / frequency: each sync `bash_watch` / `bash_status` with wait, for the whole wait (up to the deadline, or unbounded for workers).
- Problem: The wait is client-side polling: 50 `bash_status` round trips in the first 5 s, then 4/s, 2/s, 1/s. Each goes through `callBridge` (session lookups, `projectRootFor` realpath, `existsSync`, the executor queue) and returns base64 output chunks. For `kind: "regex"`, every poll that brings new output adds a second round trip that sends the retained scan window (up to 64 KB, `REGEX_WAIT_SCAN_WINDOW_BYTES`) back to Rust for `bash_regex_match`, which presumably recompiles the pattern each time. Meanwhile the engine already pushes bg events.
- Impact: A 10-minute watch costs about 700+ round trips, and twice as many on a chatty process with a regex. That is steady load on the actor's PureRead lane and on the host event loop for every concurrent watch.
- Suggested fix: Add a server-side blocking wait (`bash_status` with `wait_ms` and a pattern, answered on exit, match or timeout, cancellable through `bash_abort_inflight`), or drive the loop from the existing push channel. At minimum, match regexes server-side inside `bash_status` using the cursor, so the text is not shipped back.
- Confidence: Medium (frequency is certain; per-poll Rust cost not measured).

##### [MEDIUM] `tool.execute.after` / `tool_result` adds a `bash_drain_completions` round trip after every tool result while any background task is outstanding
- Location: `packages/opencode-plugin/src/bg-notifications.ts:524-560` (`appendInTurnBgCompletions`), called from `index.ts:1002-1020` for every tool, AFT or not; `packages/pi-plugin/src/bg-notifications.ts:439-467` (`appendToolResultBgCompletions`), called from `index.ts:971` for every tool result.
- Hot path / frequency: every tool call (including host-native tools) in any session that has a background bash task running, such as a dev server or watcher left running for the whole session.
- Problem: `if (state.outstandingTaskIds.size > 0 || !state.forcedDrainCompleted) await drainCompletions(...)` runs a full bridge round trip before the tool result is returned to the model. Completions already arrive by two other routes: piggybacked on every response (`ingestBgCompletions(response.bg_completions)` in `callBridge`/`callToolCall`) and pushed (`handlePushedBgCompletion`). A long-lived task therefore taxes every subsequent tool call with one serial RTT. `stateFor()` also runs `cleanupIdleSessionStates` (a scan of all session states) on every call (`bg-notifications.ts:1792`); that is cheap but O(sessions) per tool call.
- Impact: One extra serial bridge RTT added to each tool result's latency for the life of a long-running background task.
- Suggested fix: Drain only when there is a signal that something completed: a push or nudge since the last drain (`untrackedCompletionSinceDrain`), a piggybacked `bg_completions` entry, or a rate-limited fallback (for example at most once per N seconds). Keep the forced first drain per session.
- Confidence: Medium (a push transport is present; verify that push is reliable in standalone mode before relying on it alone).

##### [MEDIUM] `aft doctor` spawns `pi --version` repeatedly and re-walks the whole storage tree with synchronous recursive `dirSize`
- Location: `packages/aft-cli/src/adapters/pi.ts:168-197` (`isInstalled` with `execSync("pi --version")`, and `getHostVersion` with another `spawnSync`, neither cached); callers `adapters/index.ts:23`, `lib/diagnostics.ts:233-234`, `commands/doctor.ts:955,1742,1827`, `commands/setup.ts:109`. `packages/aft-cli/src/lib/fs-util.ts:5` (`dirSize`), used by `adapters/*.ts describeStorageSubtrees`, `lib/legacy-storage.ts`, `lib/lsp-cache.ts`, `lib/binary-cache.ts`, `commands/doctor.ts:808,1764`.
- Hot path / frequency: CLI startup of `doctor` / `doctor --fix` / `setup` / `--version`. Not a per-tool-call path, but it is the user-facing latency of the CLI.
- Problem: (a) Each `isInstalled()` and `getHostVersion()` on the Pi adapter boots the Pi Node CLI (a timeout of up to 5 s each). `doctor --fix` reaches it about 5-7 times: adapter detection, diagnose ×2 runs, plugin-entry fix, update loop. (b) `dirSize` is recursive with `existsSync` + `statSync` for every entry. It uses `statSync` and so follows symlinks: there is no cycle guard, and symlinked `node_modules` in the LSP cache can be double-counted. It runs over `index/`, `semantic/`, `backups/`, `url_cache/`, `onnxruntime/` and `logs/` under the shared CortexKit storage root (the same root for every adapter). `doctor --fix` calls `collect()` twice, before and after, so the full tree is walked twice.
- Impact: Several seconds of startup for `doctor` on machines with Pi installed and/or large backup or index stores (100k+ files means 200k+ sync syscalls per walk).
- Suggested fix: Memoize `isInstalled` and `getHostVersion` per adapter instance; one `spawnSync("pi", ["--version"])` answers both. Make `dirSize` use `lstatSync` (skip symlinks), drop the redundant `existsSync`, and use `readdirSync(..., { withFileTypes: true })`. Cache storage sizes per run, or skip the "after" re-walk when no clear or migration step ran.
- Confidence: High.

##### [LOW] Repeated `realpathSync` and `existsSync` canonicalization per tool call
- Location: `packages/opencode-plugin/src/tools/_shared.ts:153-166` (`projectRootFor` → `canonicalizeProjectRoot` → `fs.realpathSync`, the JS implementation, which lstats each path component), `:240` (`bridgeFor` `existsSync`); `tools/permissions.ts:328-353,417-425` (`normalizePath`, called for the target, the root and the worktree on every check); `packages/pi-plugin/src/tools/_shared.ts:173`.
- Hot path / frequency: every tool call. A `read` performs `resolveProjectRoot` (1 realpath), `assertExternalDirectoryPermission` (about 4 realpaths) and `callToolCall` → `bridgeFor` (1 realpath + 1 `existsSync`). Write and edit do this twice because they call `callToolCall` twice.
- Problem: The same project root is canonicalized 3-4 times per call with synchronous syscalls on the host event loop. Each JS `realpathSync` lstats every component of a deep path.
- Impact: Small, about tens of syscalls and well under 1 ms per call, but pure overhead on every call and it grows with path depth or network filesystems.
- Suggested fix: Memoize `canonicalizeProjectRoot(raw)` per raw string, with a short TTL or invalidation on bridge errors. Compute the root once per tool call and pass it through to `bridgeFor` and the permission helpers. Use `realpathSync.native` consistently.
- Confidence: High (cost is small).

##### [LOW] V2 permission checks do two serial host lookups per `ask`, uncached within a tool call
- Location: `packages/opencode-plugin/src/permissions/v2.ts:264-275` (`configuredRules`: `session.get` then `agent.get`), `:652` (`requestPermission`).
- Hot path / frequency: every permission ask on OpenCode 2: a `read` asks once; `write`/`edit` can ask `external_directory` plus `edit`.
- Problem: For each ask the plugin reads the session record and then the agent's rules, sequentially. When `context.agent` is set, the two lookups are independent and could run in parallel, and within one tool call the ruleset does not change.
- Impact: 2-4 extra in-process host calls per tool call.
- Suggested fix: Resolve the ruleset once per tool call (cache it on the runtime context) and fetch session and agent in parallel when the agent id is known. V2 also decides `allow` locally (`decidePermission`), so OpenCode 2 write/edit could skip the `preview: true` round trip whenever the decision is `allow`.
- Confidence: Medium.

##### [LOW] OpenCode 2 TUI status refresh is undebounced, and every status push triggers a full status RPC
- Location: `packages/opencode-plugin/src/tui/v2.tsx:195-245` (`subscribeV2StatusRefresh` → `refresh()` on every `statusInvalidated`/`indexProgress`); `rpc/register.ts:160-175` (each pushed status snapshot is turned into an invalidation instead of being forwarded).
- Hot path / frequency: each status push or index-progress frame while an OpenCode 2 TUI is open, which is frequent during index builds.
- Problem: The pushed snapshot is discarded and refetched through HTTP RPC. On the client, `AftRpcClient.call` → `cachedEndpoint()` → `readAllPortFiles()` performs `readdirSync`, a `readFileSync` per port file and `kill(pid, 0)` on each refresh. The V1 sidebar debounces this (`createDebouncedStatusRefresh`); V2 does not.
- Impact: Redundant RPC plus synchronous fs work per progress frame; the TUI process only.
- Suggested fix: Debounce as V1 does (200 ms), or carry the snapshot in the event payload.
- Confidence: Medium.

##### [LOW] `chat.message` sends `bash_wait_detach` to every active bridge, serially, on every user message by default
- Location: `packages/opencode-plugin/src/bash-wait-detach.ts:117-164` (`signalBashWaitDetachForProject`); `index.ts:907-928`; `detach_on_user_message` defaults to `true` (`config.ts:1094,1116`).
- Hot path / frequency: every user message.
- Problem: With no wait in flight (the common case), it still sends one RPC to each active bridge in sequence until one reports `detached`. It is fire-and-forget, so it does not delay the message, but it is N bridge calls per message in multi-project processes.
- Suggested fix: Keep a local "waits in flight" set (the plugin already tracks sync watches via `markTaskWaiting`/`signalSyncWatchAbort` and starts every wait:true bash itself) and skip the RPC when it is empty.
- Confidence: Medium.

##### [LOW] LSP relevance scan walks the project twice at startup
- Location: `packages/opencode-plugin/src/lsp-github-install.ts:1531` and `lsp-auto-install.ts:673` (and the Pi copies), both calling `relevantExtensionsInProject` (`lsp-project-relevance.ts:80`, a synchronous BFS of up to 200 directories at depth 4, with `queue.shift()`).
- Hot path / frequency: once per plugin start or session-work start.
- Problem: Two independent synchronous walks of the same tree with different extension maps.
- Suggested fix: Walk once, collecting all extensions, and filter per table.
- Confidence: High (impact small and bounded).

#### Area summary
The plugin adapters are generally careful. Session-directory and subagent lookups are cached and run in parallel, the hot `chat.message` config load is stat-gated (`createUnchangedConfigLoader`), logging is buffered and async, notification queues are capped, timers are unref'd and cleared, and the CLI lazy-imports each command. The largest cost is on the mutation path. Rust always returns full `before`/`after` file contents, including for previews, so every write or edit moves several times the file size across NDJSON. The host then recomputes a diff Rust already produced, unbounded on Pi (Myers via `diff` with no `maxEditLength`). Hashline edits add a preview round trip whose result is never used. Background-task handling adds per-tool-call drains while a task is outstanding, and sync `bash_watch` is implemented as 100 ms-1 s client polling, with a second round trip per regex check. On the CLI side, `doctor` is slowed by uncached `pi --version` spawns and repeated synchronous recursive size walks of the storage root.

#### Things checked and found OK
- `chat.message` config reload: stat-identity cache, so no re-parse per message unless a file changed (a rejected config is re-parsed every message, which is acceptable).
- Session-directory and subagent caches: bounded LRU (`CACHE_MAX_ENTRIES`), in-flight dedupe, timeouts cleared.
- Logger: buffered, 500 ms unref'd flush timer, async `RotatingLogSink`; perf instrumentation is one `AsyncLocalStorage.run` plus one log line per call.
- Auto-update checker: the event hook is a no-op; a single unref'd init timer, cleared on abort.
- RPC server: body capped (`MAX_BODY_BYTES`), heartbeat `setInterval` unref'd and cleared on stop, port-file sweep only at startup.
- RPC notifications queue: capped at 100 with session-fair eviction.
- `rpc-client` health checks: serial, but only on discovery; the fast path reuses the cached endpoint.
- `observeOpenCodeBgNotificationEvent` on every event: O(1) type checks, and loops only on `message.updated` over a small admissions map.
- V1 TUI sidebar: push-driven, with debounced refresh.
- Pi status dialog: interval cleared on close.
- `bash_watch` scan buffers are trimmed (substring: pattern length; regex: 64 KB window via binary search).
- `failureEnvelopeMessage` (V2) only parses output that starts with `{` and contains `"success"`.
- OpenCode `buildUnifiedDiff` has prefix/suffix trimming, a Uint16 table and a 5000-line cap (see HIGH for why it is still avoidable).
- `server-runtime.mjs`: boot-time only; finalizers release the bridge, the live-reload watch and the RPC server.
- `search.ts` path probing: a couple of `existsSync`/`lstatSync` calls per grep or glob argument, which is negligible.
- `bash_status` and `apply_patch` permission-pattern dedupe use `indexOf` (O(n²)), but n is the number of files in one patch.

---

