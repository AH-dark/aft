# Views baseline measurement contract (schema 1)

This is an acquisition harness, not an implementation of immutable indexes shared
by checkout roots. It records baselines for the design's G1–G6 performance and
correctness gates (binding, maintenance, graph copying, upgrade, queries and disk),
without declaring those gates passed. The views flag still selects the existing indexing
paths. A failed or incomplete measurement remains in `result.json`; unavailable
metrics are not converted to zeros. Source hashes and byte counts are work
counts, not the number of unique files.

## Invocation and isolation

Build the production subject and the transport-only broker:

```sh
cargo build -p agent-file-tools --release --bin aft --example views_perf_daemon
python3 scripts/test_views_performance_gate.py
bash scripts/views-performance-workload.sh --output /path/to/new/evidence
```

`views-performance-workload.sh` selects N=1,5,15 simultaneously retained checkout
sessions, the design's single/few/many-root comparison points. `--roots 1 5`, `--views off on`,
`--warmup-minutes`, `--activity-minutes`, `--quiet-minutes`, `--cycle-seconds`
`--ready-timeout`, `--state-timeout` and `--request-timeout` can be passed directly
to the Python runner for a smaller acquisition. Initial indexed readiness defaults
to 600 seconds, edit freshness to 120 seconds, and each public response to at most
120 seconds (also capped by the remaining state budget). A timed-out connection
is poisoned so a late reply cannot answer a later request. Failed query durations
remain in raw output. Default warmup/activity/quiet durations are 5/30/20 minutes. Activity is a **minimum**:
every started state must be witnessed before it can be replaced. The quiet
window is at least two seconds, including for a bounded zero-minute request.
A twenty-minute quiet window covers two normal disk-enforcement intervals.

`--semantic --model all-MiniLM-L6-v2` selects the real local fastembed backend.
The model is AFT's current default, pinned by name, not fake vectors or a network
API. Network/download time is included in initial acquisition unless the model
cache has been explicitly prewarmed; record that distinction when comparing.
Without `--semantic`, search is still exercised, but semantic readiness is
**unavailable**, never ready or a full-plane pass.

`--source` must be a local Git checkout containing the full immutable commit
from `views-performance-corpus.json`. AFT is pinned at the implementation base;
opencode's historical short pin was resolved to its full SHA; Kubernetes uses
the immutable v1.35.0 commit. External corpora must be provisioned separately;
the runner does not silently fetch a moving branch. Inventory records tracked
file/byte counts and per-file ignore/config fingerprints before overrides.
The corpus is augmented with a committed three-function Rust witness and two
local workload commits. Those commit/tree IDs and the effective configuration
fingerprint are recorded. The original source checkout is never edited.

Only newly created clones/worktrees below private scratch space are changed.
`--subjects NEW_DIRECTORY` retains those stores for subsequent upgrade/import
cohorts; otherwise they live in job temporary storage. Evidence, including raw
public responses and daemon logs, goes to a separate new output directory and
must not be placed in `target/`, `dist/` or another regenerable cache. The
connection file contains an ephemeral authentication key and is **not** copied
to evidence. Model caches and index stores are not copied implicitly.

The daemon receives an environment allowlist, an isolated HOME/XDG configuration,
isolated storage and identical project/global config tiers in both arms. Only
`views.enabled` differs. Existing project AFT config files are explicitly
replaced in the disposable corpus; their original fingerprints are retained.
The binary version/digest, broker digest, source revision/diff, manifest digest,
CPU affinity, OS/filesystem and command line are recorded. Stores are fresh;
the OS page cache is uncontrolled, not called cold. Arms alternate off/on then
on/off, sequentially on one allocation.

## Real subject and entrypoints

`views_perf_daemon` implements only an isolated transport broker. It launches
one actual `aft --subc` process, authenticates with the production transport,
and sends public RouteBind, tool requests, HealthCheck and Goodbye frames.
Every retained channel is a separately bound checkout session in that same
process. No in-process indexing API, unit benchmark or separate standalone
indexer is used as a ready-time proxy. The producing health-counter PID must
match the spawned child and its `/proc` start identity. All routes share the
same storage registry; worktrees share one Git common directory. The storage
snapshot lists the actual family `members.sqlite` files when present.

Each state queries grep, glob, aft_search (public provider name `search`),
callgraph/call_tree, zoom with callgraph annotations and scoped inspect/todos.
The witness requires the exact current source token, exactly one glob member,
search rows containing current source, and precisely the current resolved graph
edge. Zoom context alone is not a graph witness. The scoped TODO finding must
reflect the changed token. Optional `--graph-inspect` separately times one
whole-root graph-backed inspect on the primary root after all roots have bound;
it records real waits, errors and coverage. A fast not-computed response is not
called full graph-inspect readiness. The default bounded profile performs scoped
TODO inspect during every state but does not claim whole-root graph-inspect coverage.

First useful lexical answer and **indexed** lexical witness are separate clocks:
a correct bounded filesystem scan does not establish indexed readiness. The
callgraph clock requires a nonempty expected resolved edge, not symbol-not-found
or honest building gaps. Bind ACK and per-plane witness times are recorded per
root, along with their absolute monotonic origins. Each plane's clock uses the
completion timestamp of its qualifying public responses, not the end of a batch
that also waits for unrelated inspect/graph work. The lexical witness requires
all three grep/glob/search responses, so its timestamp is their maximum. Clocks
remain polling upper bounds, not proof of when the daemon internally became ready. Root 0's cold/base construction is charged
once and retained while marginal roots bind, with both base and total cost shown.
These witnesses do not establish complete repository-wide coverage or independent
cold parity. Semantic readiness additionally requires a real natural-language
search with ready coverage and a witness-file row when enabled.

Every edit/revert, atomic-save, public write, branch switch/return and transient
worktree create/remove state has an independently enumerated expected ID.
The actual source-file digest is compared to the separately constructed intended
fixture digest before each state is witnessed. Both hashes are recorded. The
next state is not allowed to erase an unobserved intermediate state. Failed
witnesses, retries, query errors and pending duration stay in the raw output.
The activity profile is witness-gated, not a fixed-rate concurrent G2 replay:
long answers cause cadence overruns rather than dropped states. No certification
of five-second query or twenty-second per-root cadence is implied.

## Atomic work counters

`metrics.views_work` is additive, schema-versioned and cheap: relaxed atomic
increments/loads only, no locks, allocations or filesystem operations in the
hooks. It is a process-lifetime snapshot, not a transaction or per-root billing.
Deltas from overlapping root intervals must **not** be summed. Restarts and
counter regressions invalidate a comparison.

| Field | Counted production work sites |
|---|---|
| `source_hashes`, `source_hash_bytes` | `cache_freshness::hash_bytes` (search, parser, legacy semantic/graph verification), v1 semantic/callgraph source-key construction, v2 `ContentHash::of`; repeated hashes count repeatedly. Payload and embedding-text checksums are excluded. |
| `content_reads` | Attempts to read/open admitted content via `project_content::read/open`, and actual bounded-I/O regular-file open attempts. `read_bounded` reaches the bounded hook, not a second hook. Failed open attempts count; files excluded before open do not. |
| `content_read_bytes` | Successfully buffered bytes returned by `project_content::read` and bounded regular reads. Streaming `open` reads do not supply byte totals. |
| `full_walks` | Requests for `search_index::project_walk_builder` (including subtree/bounded enumeration) and v1 publication's full Git-tree inventory. Counts initiated enumeration, even if it aborts, not proof a traversal completed. v2 configured membership uses the project builder. |
| `applicability_walks` | Actual uncached applicability discovery, including scoped discovery. Cache hits do not count. |
| `logical_aggregates` | v1/v2 blob usage `COUNT/SUM(length(payload))` queries, including failed queries, once per actual query; cheap caller invocations that avoid SQL do not count. |
| `own_generation_folds` | v2 checkout owner's `build_own_generation` after ownership validation, including attempts that subsequently fail. v1 publication is a different path, not mislabeled as a v2 fold. |

These sites cover both current views and legacy indexing work, but are **not**
a whole-program source-I/O interposer. Raw direct reads/hashes elsewhere,
streaming byte totals, subprocess-local atomics, arbitrary ignore walkers,
metadata/stat visits, model calls/parses and private graph-copy bytes are not
covered by this schema. Consequently zero at these sites cannot certify G1/G2a
zero-work invariants. Do not describe unmeasured source reads or hashes as
installation-presence checks: checking for local tools is a distinct kind of work.
Later
zero-work gates must extend/close this inventory before relying on zero totals.

## Resource accounting and outstanding gates

Linux `/proc/PID/stat` user+system CPU includes live descendants and waited-child
accounting; `/proc/PID/io` supplies kernel `write_bytes`. Owned LSP/model children
are included in the tree, but roles are not classified. Short-lived unreaped
children and races during tree sampling can be missed. Counters are explicitly
unknown when unavailable; unsupported physical-byte accounting cannot pass a
byte gate. Process intervals are isolated to one subject and never summed over
concurrent publications. Lifetime totals retain startup work before the first
probe. Current samples are at witness/phase boundaries, not uniform one-minute
buckets, so the harness does not manufacture a G2 p95-core statistic.

Storage apparent size and allocated blocks are separate. Allocated blocks are
not unique/shared extents on APFS. WAL/checkpoint tails are included in the
post-activity quiet phase. Waiting at least two seconds does not establish that a
donor graph has finished checkpointing, closed every writer and become immutable.

All G1–G6 verdicts remain open: statistically repeated physical roots, attestation
and strict-fallback cohorts, fixed-rate simultaneous churn, novel model fills,
old-cache eligible-import counts and inclusive graph-upgrade cost, ruled cold
parity/ranked quality, model artifact hashes, donor fallback/latency controls,
GC/admission controls, retained store formats, per-platform CoW/extent accounting
and RSS/one-minute distributions require separate evidence. A safe refusal or
an unavailable plane is a baseline observation, never successful equal-coverage
performance. The design's historical graph-copy comparison of 6.72 MB versus
1.05 GB lacks its original raw run, revision and unit convention; it is not used
as a measured baseline here.
