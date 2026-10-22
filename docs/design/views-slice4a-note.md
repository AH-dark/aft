# Semantic base-reference prototype (slice 4a)

## Harness correction before the baseline

The original natural-language witness query, `function probe returning the number
twenty nine`, did not return the probe in its top 20 on the pinned AFT corpus,
even with trigram and semantic search both ready. Waiting longer cannot repair
that query's ranking. The witness now asks `function probe_entry in
views_perf_probe.rs`, which targets the same unchanged probe fixture. A captured
real reply with trigram enabled returns its line-3 state marker in the snippet.
Neither the corpus pin nor the probe's bytes changed.

The witness still requires a successful, complete public reply and the expected
file and current-state marker anywhere in its returned snippet. It additionally
requires both `semantic_status: ready` and `lanes.semantic.status: ready`; a
missing or building semantic lane fails closed. No clock, generic status label,
or inferred embedding throughput establishes readiness. Captured real ready and
building replies are retained as `slice-4a-witness-*.json` receipts and exercised
by the harness self-tests. The existing synthetic current-state test retains
its assertions, with its missing ready-lane field supplied explicitly.

Use the corrected witness and the frozen harness configuration (including the
trigram lane) for subsequent gates. A semantic-only acquisition cannot use
whole-response completeness as a substitute for semantic coverage: its bounded
exact fallback can make the response incomplete even when semantic is ready.

## Verdict: adjust

The prototype passes the frozen semantic parity checks, but not the thresholds
of this [bind-performance measurement contract](../../scripts/views-performance-contract.md)
subset. Median bind-to-semantic-ready time improves
**21.55%**, short of 30%; hashes plus folds drop **17.87%**, short of 50%; sampled
peak process-tree resident set size (RSS) rises **7.62%**, exceeding the 5%
allowance. CPU and kernel write bytes improve. This is an **adjust**, not a
production enablement. The broader G1 bind gate also covers lexical/callgraph
readiness and other checkout cohorts; this semantic subset does not certify
those routes or other platforms. The harness-only prototype can merge on that
verdict.

## Repeated G1 semantic subset

The receipts use the frozen measurement-harness field names shown below. Each
median covers **15 distinct
fresh physical worker roots**, arranged as five positions repeated three times.
One separately charged, fresh-store base stays bound while workers bind
sequentially in the same daemon. Both arms ran on `linux,16c` on
`cortexkit-builder`, with the frozen configuration: trigram, semantic and
callgraph enabled, the same disabled LSPs, and real `all-MiniLM-L6-v2` inference.
No fake vectors were used for acquisition.

| Slice-1 key | Strict baseline median | Prototype median | Change |
|---|---:|---:|---:|
| `semantic.bind_to_ready_s` | 1.795147483 s | 1.408234327 s | -21.55% |
| `metrics.views_work.source_hashes` | 24,278 | 19,941 | -17.86% |
| `metrics.views_work.own_generation_folds` | 1 | 0 | -100% |
| hashes + folds | 24,279 | 19,941 | -17.87% |
| `cpu_seconds` | 3.62 | 2.59 | -28.45% |
| `rss_bytes` (sampled peak process-tree RSS) | 4,654,977,024 | 5,009,653,760 | +7.62% |
| `physical_bytes_written` | 7,421,952 | 4,554,752 | -38.63% |
| `model_calls` (indexing calls) | 0 | 0 | unchanged |

Each worker also had one separately recorded query-embedding observation in
both arms. The indexing-call count is not a claim of zero model work for the
readiness query. Counters are process-lifetime instrumented-site deltas, not a
whole-program source-I/O interposer or per-plane attribution. Other indexing
lanes remain enabled and contribute to the intervals. RSS is sampled every
100 ms and sums the process tree, not proportional/private memory; transient
child/process peaks are retained. No outliers were removed to improve the
verdict, and the measured RSS regression still fails the gate.

The base-preparation phases are excluded from every worker delta above:

| Separate phase | Strict baseline | Prototype |
|---|---:|---:|
| public semantic-ready observation | 1,029.435122 s | 969.324199 s |
| process CPU | 6,053.27 s | 5,948.56 s |
| indexing model calls | 1,082 | 1,082 |

Both base phases stayed within the separately authorized 1,800-second budget.
The prototype additionally waited for the settled base fill report before
starting workers; this establishes that base construction finished and never
substitutes for the public readiness witness. Worker deadlines were 120 seconds
and individual public requests were bounded at 30 seconds. The model files were
prewarmed, the vector stores and worker roots were fresh, and OS page-cache
warmness was uncontrolled.

All **15/15** prototype workers logged an installed in-process reference, and
`metrics.views_work.base_ref_prototype` was true. Merely selecting the variable
cannot pass an acquisition when the public metric is missing or false.

## Pins, identity and strict fallback

`AFT_VIEWS_BASE_REF_PROTOTYPE=1` is honored only when the effective storage root
contains `views-harness.marker`. The harness writes that marker into its own
isolated storage and forwards exactly this variable. An invalid selection is
ignored with one warning per process. There is no configuration key; ordinary
views-on defaults never construct a reference.

The in-process base has compatible storage/family/semantic-producer identity
and a fully ready immutable manifest. An adopter walks current membership and
hashes stable source bytes, but does not derive inherited semantic attachments
or fold an own generation. The root `.git` administrative pointer is omitted
from the detached base and equality walk: linked worktrees necessarily point at
different private directories, while those administrative bytes do not enter
semantic keys. Every other admitted source/ignore entry is checked. This is not
Git tree attestation or cached-stat reuse.

An acquisition pin is an `Arc`, with no residency marker. Even an empty ordinary
semantic fill creates a durable `LivePin`, so inherited-ready references skip
that fill entirely. Existing strict base construction and normal view/session
registration still have their ordinary v2 files and rows; the prototype adds no
durable base-reference record, base member, pin or read marker for the adopter.
It writes no v3 data or v3 floor. A restart must re-establish state through the
strict loader rather than follow a durable reference.

Changed/new/deleted files, unsupported file kinds, incompatible identities,
unstable reads, pending bases, active writes and failed installation take the
strict path. A pre-bind content edit therefore remains correct, but this
prototype repeats the full strict walk and generation build rather than doing
work proportional only to the changed files. Non-Unix targets
lack the required change-sensitive file identity and stay strict.

## Parity and non-vacuity evidence

Before recording the verdict, the real pinned-model test performed **18 exact
top-k comparisons** on the **same physical worker**, clean and modified before
bind: three frozen queries at k=1,3,20 in both states. Relative paths, names,
start lines and score bits matched the prototype-off strict path, with complete
semantic answers. The clean case demonstrably used a reference; the modified
case demonstrably fell back to strict loading. Deterministic unit-model parity
also covers edits, membership changes and additional k values.

The kill test serves a semantic query through a live reference, signals its
bounded test-only rendezvous, then the parent sends and verifies **SIGKILL**.
Before any recovery/GC pass, it compares full normalized registry rows and
pin/read-marker inventories with a prototype-off child. Normalization removes
run-specific IDs, timestamps, generation labels and subject-root prefixes, not
semantic row contents; publication timestamps can be NULL for an adopter that
never folds. Members match, no extra registry/protection records appear, and the
adopter's entire view directory contains no files. A synthetic extra pin must
fail the same inventory comparison.

There are **19 named red mutation controls**, including the targeted witness,
public lane/active-metric guards, variable/marker/default selection, once-only
warning, effective identity, readiness, stable reads, content/membership,
private Git administration, empty-fill pins, leaked registry rows, leaked pins,
v3-directory creation, and active pre-bind writes whose begin event predates the
new driver's listener. Every mutant was marked `NON-VACUITY BREAK` and restored;
non-empty/empty diff pairs and named failures are retained with the raw output.
The remote runner transports HEAD rather than the local staged index, so the
remote proofs used an equivalent private index/object directory and job backups
without writing the repository's read-only `.git`.

Verification also passed the semantic runtime regression suite (55 tests, plus
the later active-write regression in the 14-test focused suite), all 162 binary
tests, 16 registry integration tests, the 22-test harness suite, Rust formatting,
and the warnings-denied Windows test-target cross-compile. The real-model test
is ignored in ordinary unit runs and was explicitly executed. Windows evidence
is compile-only; native Windows/macOS behavior and performance are not claimed.

## What slice 4 should change

1. Replace the process-only acquisition with the designed v3 base scope,
   full effective identity, depth-one durable references, unique exact-generation
   read markers, referrer index, recovery and reference-aware GC/eviction. An
   `Arc` is not protection after process death, main deletion or foreign GC.
2. Preserve the strict membership/content identity until the later verified-bind
   stage (4v) proves Git blob identity and pairs trusted byte/stat observations
   with the conversion policy actually in force at the observation. Equality of
   absent tree IDs, a worktree's `.git` pointer or Git's stat cache is not proof.
3. Add composed delta entries, explicit tombstones and readiness overrides, and
   retain the base owner's single-flight fills/readiness successors. This
   prototype only adopts a fully ready base and otherwise falls back wholesale.
4. Implement the specified publication/install fences and query-owned reader
   lifetimes. Do not turn an empty delta, a losing CAS or an equivalent winner
   into a falsely verified ready installation.
5. Keep the corrected public witness. To reach the hash target, reduce repeated
   inherited-byte verification with the later proven watcher/stat protocol and
   eliminate duplicate work in the remaining indexing lanes as they migrate.
   Skipping folds alone is insufficient: 19,941 instrumented hashes per bind
   remain in this full-configuration workload. Investigate RSS with additional
   private-memory and proportional set size (PSS) accounting before attributing
   its regression to a particular allocation, but do not waive the recorded gate
   failure.

## Receipts and provenance

- Witness correction: commit `0a8aacb50c8198c1626cefa1ba6a7f6e93d128e1` and
  `scripts/views-performance-baselines/slice-4a-witness-{ready,not-ready}.json`.
- Real unchanged-main baseline: commit
  `6ea92a991b02ae38ca1d06ea7acbf08a09f27ecc`, implementation
  `5a736ae671ea92ae17209ae3be75068faa6fc8ed`, and
  `scripts/views-performance-baselines/slice-4a-linux-x86_64-baseline.json`.
- Measured prototype: implementation
  `1a5084698de12bf74350464a4213b646a57866fb` and
  `scripts/views-performance-baselines/slice-4a-linux-x86_64-prototype.json`.
- Both augmented corpus trees are
  `dbed38822622564133983f8f80e55a974ba0f0a7`, derived from the unchanged corpus pin
  `05d40ac417db6137a4204521bd3c59853325e968`; configuration digests match exactly.
- Both use ONNX Runtime (ORT) 1.24.4 and MiniLM revision
  `d13954661f83248295ba75c1ed411eef3b7b936e`. Binary, broker and harness hashes,
  CPU affinity, raw worker intervals and separate base phases are in the receipts.
- The earlier `slice-4a-linux-x86_64-blocked.json` is historical evidence from
  before witness diagnosis/correction, not the final baseline or verdict.
- Raw public replies, progress, full named mutation output and source snapshots
  are retained outside regenerable build directories under
  `.cortexkit/internal-docs/investigations/views-slice4a-acquire/` for delivery.

The measurement contract is
[`scripts/views-performance-contract.md`](../../scripts/views-performance-contract.md).
No full G1, all-plane quality, cross-platform performance, durable-reference or
all-day CPU-savings claim is made by this semantic-runtime prototype.
