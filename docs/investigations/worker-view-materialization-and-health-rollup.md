# Worker view materialization and health rollup

## Scope and causes

Investigated repository base `236b14b97a58fd793413b22894905eb4eca24547`.
The deployed daemon's reported build identifier, `be2b7b11`, is not a Git
revision available in this worktree, so its exact source cannot be compared here.
No live SQLite
database was opened for diagnosis, copied with filesystem APIs, or inspected
through a second raw descriptor. Measurements below use disposable fixtures on
the Linux runner, not the deployed daemon. The dispatch corpus-read optimization
is a separate change: it is not deployment-ready without optimized-profile
measurements showing that it does not worsen latency. The debug measurements
below document the investigation, not approval of that optimization.

1. Legacy checkout publication selected only its own current generation as an
   incremental base. A worker's scope is path-derived and distinct from its main
   checkout, even though their blob family is shared. A first publication had no
   base and always built cold. It was not an unsuccessful attempt to clone the
   main checkout: no such candidate was considered.
2. Dispatch type binding eagerly read every manifest payload again even though
   its caller supplied decoded source parses. Those bytes were used only for
   resolver configuration, not source parsing. This doubled source payload reads
   and held another serialized corpus in memory. Payload lookups already use the
   primary key; the fixture did not establish a missing index or quadratic SQL
   lookup. Resolver inputs such as `tsconfig.json` and Cargo workspace metadata
   are read only when binding needs their bytes, still checking the payload's
   digest and schema and returning read failures instead of silently guessing.
3. The health worker's three-second timeout was an idle wait, not a rate limit.
   A bind-completion request queued during a refresh started another census
   immediately. Its one-slot channel coalesced queued signals but did not bound
   refresh frequency. Refresh requests now respect a minimum three-second wait
   after the preceding census; startup remains immediate and shutdown interrupts
   the wait. All diagnostic fields and root coverage are retained.

## Donor admission and correctness

A missing or unusable legacy base considers registered Git worktree siblings.
The topology reader consumes `git worktree list --porcelain -z`: NUL-delimited
records describing checkout paths and their Git metadata. It enforces a 4 MiB
total byte limit, 64 KiB record limit and
4,096-worktree limit, all applied at the iterator. An incomplete topology disables
seeding. Existing sibling pointers are ranked by their immutable manifest's
modification time, which follows materialization rather than preceding it like
the generation-name timestamp. The freshest ready member of the same artifact
family wins; an unreadable optional sibling does not hide an older ready one.
Separate Git directories and path whitespace do not require guessing the parent
of `.git`.
The donor must have the same artifact family and an existing, readable, ready
view. Current-generation and derived-owner query pins protect the donor until
the copy and patch finish. The target still gets its own SQLite backup, not a
cross-checkout artifact reference.

The existing cutoff counts changed manifest entries: below 100 changes it
always permits a patch; otherwise it builds cold above 40% changed entries
(or 10% when resolver configuration/membership can force full resolution).
That decision precedes copying. Schema/build-output
readiness, the owning manifest fingerprint, and materialization version checks
remain in force. Missing/damaged or busy optional donors yield to another ready sibling; own-base
SQLite contention remains retryable. Existing usable checkout-local bases remain
preferred. Publication logs now name `seed_source=own|sibling|cold` and
`cold_build_reason=none|no_base|large_diff|base_unreadable`.
There is no schema, stored format, producer, or materialization-version change.
The separate version-2 first-load orchestration still refuses to copy a derived
graph from another checkout; this change is limited to legacy publication.

`worker_first_publication_clones_main_with_cold_query_parity` compares every
table's typed query results and schema with an independently cold-materialized
worker manifest, including TypeScript path configuration, cross-file calls and
Rust method dispatch. Its donor pointer reuses an older derived owner, and the
worker edits two files, adds and deletes files, and renames a source file. The
fixture also compares callers and impact result bytes, serialized trace bytes,
and dead-code scan bytes. Projection timestamps record when a query runs, not
stored graph content, so both scans omit that invocation-time field.
The donor's query snapshot must remain unchanged.
The 4,891-file unchanged-worker measurement also checks full query parity.

## Paired measurements

Commands use `cargo test -p agent-file-tools --lib -- <name> --ignored --nocapture`
on `linux,4c`, rustc 1.99.0, the debug test profile. Timings are individual samples,
not a production latency forecast. The worker baseline already includes the
independent redundant-read fix, isolating donor selection.

| Fixture / test | Before | After |
| --- | ---: | ---: |
| `bench_4891_file_worker_publication`: worker materialize | 3,058 ms | 20 ms |
| Same fixture: prepare_checkout (manifest, blobs, clone and derived build before pointer CAS) | 3,291 ms | 333 ms |
| Same fixture: clone | none | 122 ms |
| `bench_4891_file_materialization`: cold materialize | 2,585 ms | 2,723 ms |
| Same fixture: verified payload reads | 9,782 | 4,891 |
| Same fixture: payload bytes | 44,941,630 | 22,470,815 |
| `bench_health_rollup_cpu_per_minute`: 50 roots, 50 requests/s | 12,659 CPU ms/min | 157 CPU ms/min |
| Same 60-second fixture: completed refreshes | 2,992 | 20 |

No cold wall-time improvement on this small-payload fixture was established
(an earlier fixed sample was 2,576 ms); the read-volume reduction is deterministic. The exact reported 285–375
second workload was not available in this worktree. A second fixture archives
this worktree's committed repository corpus (2,219 tracked manifest entries,
not the reported 4.9k-file deployed corpus) and measured **762,010 ms** cold
materialization in the debug profile, 810,666,544 payload bytes read, 1,623 reads,
and 586,355 unknown-liveness rows (potential targets protected from dead-code
removal because their receiver could not be resolved). This demonstrates that realistic payloads and
liveness rows are much larger than the simple 4,891-file fixture. It is not a
release-profile or old-daemon/new-daemon comparison. Reinstating eager dispatch
reads on that same corpus measured 668,714 ms, 3,240 payload reads and
1,621,245,608 bytes. The fixed path therefore reads about half the bytes, but
was 14% slower in this single debug-profile pair (the fixed run was first).
No wall-time speedup on the real corpus is claimed; further controlled release
measurements are needed to attribute that variation. Table sizes and liveness
row counts were identical. The results do not establish
that the live 12 GB RSS will fall to a particular value.

The investigation started from an October 11 daemon reporting 150–330% CPU,
12 GB RSS, and worker view publications averaging 109 seconds versus 85 seconds
on October 10. The six view/publication-related commits deployed during that
interval (`b9457bc73`, `fb6e7e96c`, `a2d4e725a`,
`bcaccdd3a`, `c2ae62417`, `d702143ec`) do not change the cold materializer, dispatch
emitter, joiner, or verified blob reader. Their source blob IDs at `b9457bc73^`
and `236b14b97` are byte-identical. Each hash below is Git's source-file blob
identifier, not a runtime database checksum:

- `crates/aft/src/views/materialization.rs`: `c3a6b4d32c6d90629a9b346f47e5e43e80b37fd6`
- `crates/aft/src/views/materialization/dispatch.rs`: `30861655c6421ae44c7967e2c6dea8426b0004e1`
- `crates/aft/src/callgraph_store/join.rs`: `640b2a570091c8137ed8e9fc6185380f6ab0cf5c`
- `crates/aft/src/blob_store/mod.rs`: `0904ed2d7c927e71587818edaa5d58e7401a70b1`

The six publication-related commits listed above alter publication eligibility, readiness/fallback selection,
concurrency, and Markdown eligibility, not the per-manifest cold algorithm.
The reported October 10 versus October 11 average worker-publication durations
(85 versus 109 seconds) do not isolate an algorithm
regression; no direct cold-algorithm regression in those commits was established.

### Large-table ownership

The offline corpus allocates 127,479,808 bytes to `refs`, 42,811,392 to `edges`,
42,782,720 to `view_bindings`, 52,203,520 to `view_unknown_live`, and 58,925,056 to
its SQLite primary-key autoindex (111,128,576 bytes combined). The keeper's
existing SQLite connection supplies `dbstat`; no extra descriptor is opened.
The supplied live `dbstat` measurements allocate 66 MB to `view_unknown_live`
and 74 MB to `sqlite_autoindex_view_unknown_live_1`, or 140 MB combined.
This rowid table has a four-text/integer-column composite primary key, so its
key is stored both in the row and in the autoindex. It emits a protected-target
row for every uncertain receiver site and feeds scoped dead-code liveness.
The two reported `refs` indexes are different keys, not duplicate explicit
indexes: `(caller_node, kind, status)` versus `(kind, caller_file)`.
A `WITHOUT ROWID` unknown-liveness table or a coarser per-file protection key
could reduce storage, but either changes stored layout/semantics and requires
approval and separate parity evidence. No table, key, or index format was changed.

## Bash GC and memory observations

Persisted bash GC is already cursor-bounded (128 directory entries per batch),
coalesced per storage root, and loads recorded processes once per session.
Source inspection found bounded cursor work and coalesced sweep admission, not
a repeated-work spin in persisted GC. No live per-GC-thread trace was captured
to attribute the reported CPU samples. Small historical maps can accumulate recovered
session records, canonical GC root slots, and exhausted deletion-retry counters;
their memory impact was not measured and they do not explain 12 GB by themselves.
No speculative GC or allocator changes were made. Removing dispatch's duplicate
serialized corpus is the demonstrated memory-related reduction.

## Deferred CoW investigation

No CoW implementation is included in this delivery. Publication's durability
checkpoint is PASSIVE and does not establish an empty WAL. A keeper remains open
after the pointer CAS and is handed to a delayed TRUNCATE checkpoint; that job
can still checkpoint into the source main file, or fail/defer because of readers.
See `views/mod.rs::prepare_inner`, `sqlite_full_sync_checkpoint`,
`views/assembly.rs::PreparedAssembly::commit`, and
`views/generation.rs::schedule_derived_checkpoint` / `checkpoint_derived`.
`db/file_identity.rs` tracks this process's connections, not arbitrary writers
in other processes. These facts do not yet prove both requested CoW admission
conditions. The existing backup API still includes committed WAL content.
The CoW path, its WAL-frame mutation proof, and the 1 GB APFS time/shared-block
measurement are deferred rather than implemented with a weaker guard.
