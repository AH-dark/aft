# Scoped inspect: exact rollup reuse and lazy runtime notes

## Change and boundaries

Baseline: `3044304035317f5fbede501e87c2ef8fd26504ec` (`origin/main` when
work began). A preceding TypeORM elapsed-time profile identified whole-project
scope reconstruction and runtime-note producer discovery as the dominant costs.
The reproduction below uses only this baseline and the pinned TypeORM source.

For a freshness-verified legacy scoped cache read, the `dead_code` analysis
category reuses the existing
callgraph projection and its existing uncapped rollup. Reuse requires:

- the same generation, durable graph revision, project root, and snapshot;
- the same actual contribution JSON hashes, not just source hashes;
- the same configuration/manifest/resolver contribution-set key;
- the same public API files, project roles, and uncapped presentation state.

A miss rebuilds the global rollup and retains it in the existing projection
slot. A missing graph still reports unavailable. Stale and last-complete reads
retain cold reconstruction. No persistent format, contribution-cache version,
or new cache is introduced. The snapshot-pointer check also prevents a result
computed from one projection being installed on a concurrently replaced one.

Other categories still reconstruct their scoped summaries: those summaries are
in the response even when their drill-down sections were not requested. Their
redundant project inventory walks are removed because their language-coverage
fields are removed by the existing scoped filter. Contributions are **not**
filtered before global import, reachability, cycle, or duplicate analysis:
outside callers and cross-boundary groups must retain their meaning.

Runtime-note selection now resolves scoped producer keys lazily. If no client
has a raw runtime note, selecting any subset cannot return a note. The check and
selection use one LSP-manager guard. If notes exist, the original producer-key
selection, sorting, deduplication, structured notes, and text rendering remain.
Diagnostics authority and producer-failure reporting are not skipped.

This is a warm-path improvement, **not a claim that every category or cold
request is now strictly proportional to scope size**. Freshness verification,
contribution decoding/hashing, and the other four categories still have global
work. New global liveness analysis remains necessary after invalidation.

## Response parity

`scoped_parity.py` compares complete response bodies after removing **only**
the request `id` and `scanner_state.tier2_last_run`. Counts, findings, object and list ordering,
envelopes, gaps, completion, notes, and rendered text remain in the comparison.
All eight scope/section cases below produced identical before/after response
bodies, as did every timing pair for the `packages`/`dead_code` request:

| Case | Scope | Sections | topK |
|---|---|---|---:|
| 0 | `packages` | dead_code | 100 |
| 1 | `packages/typeorm/src/index.ts` | dead_code | 100 |
| 2 | `.` | dead_code | 100 |
| 3 | `packages/typeorm/src` | dead_code, unused_exports | 7 |
| 4 | `packages/legacy-naming-strategies/src` | all | 100 |
| 5 | `packages/typeorm/src/index.ts` | omitted | 100 |
| 6 | `packages/typeorm/src/index.ts` | diagnostics | 100 |
| 7 | unscoped project | dead_code | 100 |

Diagnostic parity uses disabled producers, preserving their honest coverage
response rather than depending on live language-server timing. Timing requests
leave servers configured normally and omit diagnostics. Runtime-note tests also
exercise a live mock transport with and without a raw note; the existing scoped
Rust/earlier-TypeScript-note integration test passed. This is not a live
TypeScript diagnostics certification.

Corpus: TypeORM `17e858da8d2becab1ca47826f670a4660da70142`, extracted from
`git archive`; dependencies were not installed. Semantic/trigram indexes were
disabled and the callgraph index remained enabled. The same four intentionally
invalid `column-readonly` codemod fixture paths named in the original profile
were excluded on both sides. Other invalid fixtures and incomplete analysis
were not suppressed.

## Interleaved release measurements

One `linux,4c` job measured both binaries in warm processes, after one scoped
warm-up each. Order alternated B/A, A/B, B/A, A/B, B/A. Toolchain:
`rustc 1.99.0 (b940084d7 2026-09-28)`, Python 3.14.4; release builds used four
Cargo jobs and identical temporary stderr-only phase probes.

| Iteration | Before (s) | After (s) |
|---|---:|---:|
| 0 | 7.111277 | 3.014646 |
| 1 | 6.808327 | 3.004656 |
| 2 | 6.908027 | 2.913030 |
| 3 | 7.622407 | 2.912136 |
| 4 | 7.525187 | 3.213763 |
| **Median** | **7.111277** | **3.004656** |

The median reduction is **57.75%**, or **2.37x** faster. The table below uses
means so its non-nested rows are additive. Decode timings are included inside
each scope phase; their individual measurements are retained in the JSON.

| Phase | Before mean (s) | After mean (s) |
|---|---:|---:|
| Dead-code scope reconstruction/filter | 3.987722 | 0.489831 |
| Unused-export scope reconstruction/filter | 0.867377 | 0.844570 |
| Duplicate scope reconstruction/filter | 0.210881 | 0.186725 |
| Cycle scope reconstruction/filter | 0.722447 | 0.704195 |
| Complexity scope reconstruction/filter | 0.101367 | 0.065037 |
| Fetch/decode five stored aggregates | 0.308879 | 0.300675 |
| Five freshness checks | 0.248567 | 0.244192 |
| Runtime-note selection | 0.580308 | 0.000001 |
| Render response | 0.000715 | 0.000724 |
| Inventory and remaining request/client work (residual) | 0.166781 | 0.175696 |
| **Total wall mean** | **7.195045** | **3.011646** |

The remaining global rollups are visible, not removed from the measurements.
Timing variation affected both binaries; this comparison is within one job,
not a claim that the older profile's absolute numbers reproduce unchanged.

Detailed per-run phases, parity hashes, toolchain, runner job identity, and
binary hashes are committed in
[`scoped-performance.json`](scoped-performance.json).
Baseline binary SHA-256:
`251d37b2c5a714205c5ef8fc179b51ffb662f30de5ca236978fb5296d1b6ff35`;
candidate:
`d0c2b9d2e9518f14648941b5f835528e78546b6c4bcc1c7d3fd6c9af0e2f6c3c`.
Raw replies are written by the harness under its explicit output directory;
they are not checked into this repository.

## Reproduction

Prepare inputs locally inside this checkout from the two pinned Git commits:

```sh
mkdir -p local-ignore/scoped-inspect
git clone --no-checkout https://github.com/typeorm/typeorm.git local-ignore/scoped-inspect/typeorm-source
git -C local-ignore/scoped-inspect/typeorm-source archive --format=tar.gz \
  --output=../../../benchmarks/inspect-truth/typeorm-corpus.tar.gz 17e858da8d2becab1ca47826f670a4660da70142
python3 - <<'PY'
import json, subprocess
from pathlib import Path
paths = [
    'crates/aft/src/commands/inspect.rs',
    'crates/aft/src/inspect/manager.rs',
    'crates/aft/src/inspect/scanners/dead_code.rs',
    'crates/aft/src/lsp/manager.rs',
]
base = '3044304035317f5fbede501e87c2ef8fd26504ec'
sources = {p: subprocess.check_output(['git', 'show', f'{base}:{p}'], text=True) for p in paths}
Path('benchmarks/inspect-truth/scoped-baseline-sources.json').write_text(json.dumps(sources))
PY
```

Run on `linux,4c`, allowing enough time for two sequential release builds.
The source map and archive are temporary untracked inputs (so a remote runner
can synchronize them); remove them afterwards rather than committing them:

```sh
python3 benchmarks/inspect-truth/scoped_parity.py \
  --baseline-sources benchmarks/inspect-truth/scoped-baseline-sources.json \
  --corpus-archive benchmarks/inspect-truth/typeorm-corpus.tar.gz \
  --output local-ignore/scoped-inspect/reproduction --runs 5
```

If a runner deadline expires after both builds, the harness also saves a pair
of probe-instrumented release binaries under `target/scoped-parity-pair`; `--binaries-dir` can reuse those binaries in
a new output directory without another compile. This is a build cache, not the
location of retained measurement evidence. The successful final measurement
reused both binaries from the completed release builds of the final sources,
so neither process competed with this worker's compiler during timing.

## Verification notes

- Rust format check passed (rustfmt 1.10.0-stable).
- Inspect library tests: 348 passed, 4 ignored; runtime-note tests: 2 passed.
- Binary tests: 132 passed.
- Inspect integration tests: 122 passed, 4 ignored, one fixture precondition
  failed before calling inspect (`before.errors.is_some()`). The isolated retry
  passed without code changes. Real rust-analyzer tests can silently skip in
  this suite; no real-server certification is claimed.
- Windows `cargo check --tests`, with warnings denied, finished successfully;
  this is compile-only, not a Windows runtime test.
- Harness unit tests: 3 passed. Mutation controls independently disabled rollup
  reuse, contribution-byte identity, graph revision identity, the no-note
  shortcut, summary comparison, and object-field-order comparison. Each produced exactly its named failing
  test, and each staged implementation was restored with an empty unstaged diff.
- Development unit runs exposed fixture mistakes in the added invalidation
  assertions (an absolute path passed to a relative-path deletion API, and a
  no-op refresh that correctly did not advance the graph revision). Those were
  corrected without changing existing response expectations. A two-build benchmark exceeded the runner's
  30-minute deadline after six passing parity cases; the completed final run is
  the only timing set used above.
- An `aft_inspect` diagnostic check over the four changed Rust files returned
  partial coverage while rust-analyzer's Cargo check was pending; the explicit
  Rust test/compile gates are the authoritative verification.
- Release builds emitted the baseline unused `VecDeque` import warning in
  `cache_freshness.rs`; it is outside this change and was not modified.
