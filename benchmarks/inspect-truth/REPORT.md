# aft_inspect truth at scale: campaign report

## Final upgrade-and-report rerun

This accuracy campaign compares AFT (Agent File Tools) with language-specific
reference analyzers, called **oracles**. Each implementation step has a **slice**
report. `D<n>` means the status bar reports n dead-code findings; `D?` means that
count is unknown, and `U<n>` is the unused-export count. A **baseline** is the
before-change binary/measurement, not a separate source revision of the corpus.

**The cache upgrade is verified; the campaign acceptance gate is not passed.**
All seven corpus entries were freshly inspected with a Linux release binary from
`ffe59a5d7d1f333deaca3f84b798459ea9b088df` (AFT 0.59.0). Six before entries use
the exact reviewed `13dd9cb71a7bbf9076377ac1ba1717dabf15e586` source (AFT 0.58.2);
AFT's own checkout is a **new corpus entry with no before-change measurement**.
The complete, same-harness before/after
tables are in [slices/upgrade-and-report.md](slices/upgrade-and-report.md).

| Final target | Measured result | Acceptance |
|---|---|---|
| Outline dead_code precision >=0.7 | 66/73 = 0.904; 15 oracle rows judged out | Pass |
| TypeORM unused_exports / dead_code | Both unknown: four codemod parse errors | Blocked, not zero findings |
| ripgrep dead_code <=10, unjudged 0 | 4; 4 judged, 0 unjudged | Pass |
| axum dead_code <=10, unjudged 0 | 9 scored; 9 judged, 0 unjudged; project D10 | Pass |
| soft-serve function precision >=0.6 | 9/26 = 0.346; 26 judged, 0 unjudged; recall 0.45 | Numeric target missed; existing row explanations retained |
| soft-serve NewServer not test-only | Absent from dead_code and dead_code_test_only | Pass |
| FastAPI explicit unsupported Python coverage | complete:false, Python files:1166, supported files:4, count:0 | Pass: mixed-language D0, not Python-only D? |
| AFT Rust <=rustc+10 | by_language.rust:33; rustc:0 | Fail: 33 > 10, 33 Rust rows unjudged |
| AFT dead_code / unused_exports unjudged 0 | Rust D:33 unjudged; in-domain TS D:10 unjudged; U unknown | Not met; no blanket judgments added |
| AFT Python gap and checkout hygiene | Python files:97; before and after status empty | Pass |
| TypeORM scoped release timing | Enabled median 5.707473s; disabled 5.307470s; difference 0.400003s | Difference passes; <5s target fails |
| All-repo diagnostics/footer pass | Diagnostics sampling disabled | Not run; fixture evidence is not a corpus pass |
| Cell monotonicity | Six before and seven after entries; four TypeORM cells become unknown | Fail, four lowered cells |

The four failed cells are TypeORM TypeScript `unused_exports` precision/recall and
`dead_code` precision/recall. Defined-to-unknown is not the permitted zero-finding
precision exception. Unknown buckets reporting zero collected or unjudged rows
do **not** establish that their findings were reviewed. Other comparable cells
did not lower. Soft-serve's fully judged AFT rows remain in its score; there are
zero Go oracle-side removals, and no claim that its oracle-only rows were wrong.

### Provenance and retained evidence

The six OSS source pins are unchanged from `corpus.json`. Missing source trees
were fetched/installed in this worktree's remote `target/upgrade-and-report/corpus`.
Ripgrep, axum and soft-serve reuse copied oracle snapshots frozen for each exact
corpus source commit;
Outline, TypeORM, FastAPI and AFT run their oracles with Fallow 2.88.3, Knip 5.85.0,
Vulture 2.14 and rustc/Cargo 1.99.0. Bun is 1.4.2, remote Python 3.14.4, scoring
Python 3.9.6. No tracked package manifest or lockfile changed.

The collection command is `python3 benchmarks/inspect-truth/run.py collect
--repo outline --repo typeorm --repo ripgrep --repo axum --repo soft-serve
--repo fastapi --repo aft --corpus-root target/upgrade-and-report/corpus
--baseline-bin <exact-13dd9cb71-release-aft> --aft-bin <candidate-release-aft>
--aft-commit ffe59a5d7d1f333deaca3f84b798459ea9b088df --diagnostics-sample 0`.
Release builds use `CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER=`. Cargo jobs were
serialized. Initial baseline dependency fetches required an isolated writable
Cargo home because the remote default registry is read-only.

Raw records and score JSON/Markdown are retained outside regenerable build trees
under `local-ignore/upgrade-and-report/final/`. The exported raw archive's SHA-256
is `54546c6604b8f8ea681d1a3b06f2a822da2b7baedfc09fae5771daacf1d9578a`;
the supplemental origin/upgrade/timing archive's is
`703a6b4f8614c40434d05f5f60abb6f8523c3d8a24f04736473a373ba6358119`.
Both gzip/tar/base64 transfers were hashed before extraction. Baseline binary
SHA-256: `f58e8ed8d85634e7eb015f452a0b154bdcb66947596a4a96ef5d72a0b0cec216`;
candidate: `e10f4b0626ac8de305c4021c8bfacd6bf8b3a446e4125848d3ac71815d11828d`.

Earlier row entries remain attached to their own origins. Runtime descriptor
copies relocate paths only; the scoring implementation and judgments are not
weakened. Preserved callgraph/scanner raw finding lists and oracle snapshots
validate the earlier row judgments for ripgrep, axum and soft-serve.
Outline and TypeORM's missing `9f382ddf1` origins were regenerated from an archive
of exact local Git object `9f382ddf14a4a9c8f1a8a094caf01c9ad78884f6` (archive
SHA-256 `a8dab40db85ca31390c7617cdccc3bc0c120ffcfe35bce85dc436576a35178e2`).
That source object exists locally although GitHub archive requests return 404.
The historical TypeORM regeneration repeats the TS slice's four documented
codemod `.aftignore` exclusions and reuses the new same-pin oracle snapshot;
the **final candidate default captures have no such exclusions**. Origins were
never synthesized from Markdown or replaced with current-binary captures.

The collection harness in `run.py` checks only the baseline binary's **version
string**, not the source commit used to build it.
The `v0.58.2` tag resolves to `543fe03665c2bb3e2206abe7591daf91aeb74761`, not
`13dd9cb71`; using that same-version binary as the corpus baseline would silently
pass the harness check. The exact reviewed commit was therefore built separately.
A tag-storage upgrade experiment is retained as an additional, explicitly
non-corpus experiment, not relabeled as the reviewed baseline.

### Upgrade and pagination

The **single campaign aggregate bump, 34 -> 35, already landed with the callgraph
slice**, whose Go initializer and caller-path repairs change stored findings
without changing source files. A new aggregate hash forces those findings to be
recomputed. This upgrade slice retains 35 rather than bumping a second time. The new cache
tests pin frozen pre-campaign 34 and require it to be older than the current
version, so later independent bumps do not invalidate the upgrade proof.

Three crate-internal cache tests close the context, reopen the same SQLite
generation and rebuild from unchanged contributions/sources. Independently for
dead_code and unused_exports, a version-34 aggregate holds 100 of 150 findings;
reopen rejects it, restores the complete clean-build list, and real inspect
requests return pages 100/50/0 at offsets 0/100/150, invariant total 150, disjoint
and exhaustive row identities, stable repeats and correct final envelopes. The
Go fixture contains `var Boot = Register()` and rejects an old aggregate falsely
reporting `Register` dead. Neutralizing hash-version folding reddens each named
test alone; restored cache tests pass. Exact test names and mutation results are
in [the upgrade verification table](slices/upgrade-and-report.md#upgrade-verification-and-acceptance-boundary).

A real process experiment additionally writes storage with the exact baseline
binary, shuts it down, starts the candidate on that storage **without deletion**,
and compares with a separate clean candidate build. SQLite reads confirm both
old aggregates contain count 150 but only 100 stored items. Reopened findings,
summary counts and envelopes match clean output byte-for-byte; timing metadata
is intentionally not compared. Source SHA-256 values before/after are identical.
The same Go initializer fixture is included. This is not just a cache-key unit
comparison (`final/exact-binary-upgrade.json`).

Outline's final release `unused_exports` project total is 245. Its comparison
counts 240 rows under the configured `app/`, `server/`, `shared/` and `plugins/`
scoring prefixes, not every file in the project. Unscoped topK100 requests return 100/100/45/0 at offsets 0/100/200/245;
next_offset is 100/200/null/null, all totals are 245, the union has 245 distinct
rows, and repeat order is stable (`final/outline-paging.json`).

### Unsupported-language and diagnostics coverage

| Repo | Supported-language files scanned | language_unsupported gaps |
|---|---:|---|
| outline | 2544 | none |
| typeorm | 3606 | none (separate analysis_incomplete gap) |
| ripgrep | 110 | ruby:1 |
| axum | 306 | none |
| soft-serve | 240 | none |
| fastapi | **4** | **python:1166** |
| aft | 1879 | **python:97** |

FastAPI's four supported files explain its numeric count 0 despite incomplete
Python coverage. Unsupported files are counted across the scanned project, not
only the oracle's `fastapi/` prefix. Python is not silently scored as implemented.

The harness disabled-server list is `typescript`, `typescript-native`, `python`,
`rust`, `go`, `bash`, `yaml`, `ty`, `oxlint`, `biome`, `vue`, `svelte`, `astro`,
`prisma`, `dockerfile`, `terraform`. Collection uses this list; the separate
diagnostics comparison was **not** run, and Outline numeric E/W and the
TypeORM/soft-serve corpus producer masks are not asserted as passed.

Scoped timing used Linux `7.0.0-34-generic`, x86_64, glibc 2.43, with **2 allocated
vCPUs** on ck-motor. Five disabled-server calls took
5.507066/5.507049/5.306499/5.307470/5.306089 seconds; five enabled-config calls took
5.707473/5.605999/5.706456/5.806789/5.806691. Each response carries scoped D135,
no pending/unavailable status, exactly diagnostics `{status:"not_requested"}`,
and no diagnostics details. The enabled median exceeds five seconds; this is a
failed timing gate, not a claim that configured producers were all authoritative.

### Own-checkout hygiene and real remaining blockers

Before collection, `git status --porcelain` was the empty string `""`. After
collection it was the same empty string `""`. The restored tracked config's
SHA-256 is `87e6777009df5cc9b74d6adffaa000a266fda621f8e5addfbd5d85d743a60b25`
on both sides; full file mode is 33204 on both sides (`final/own-hygiene.json`).
The remote bind mount refuses chmod. An authorized runtime-only workaround skips
chmod **only** for `.cortexkit/aft.jsonc` when its current full mode already equals
the saved mode; all other paths and mode mismatches still call the original
chmod. Three no-op restorations were skipped. Nothing was committed to the harness.

Axum D10 is **not a regression from the callgraph slice's D9**: the ten final
raw identities equal the old ten raw identities. The scored nine omit
`examples/unix-domain-socket/src/main.rs::server`, outside axum's include paths.
Both final scoped bucket and historical scoped bucket have nine rows.

`benchmarks/src/exercise.ts` is **real syntax corruption, not an intentionally
broken fixture**: lines 161-163 duplicate the preceding catch tail, refer to `e`
outside its catch and close extra braces. `benchmarks/src/runner.ts:12,135` imports
and calls `runExercise`; `benchmarks/package.json` runs that driver for `bench`.
[The TS-liveness report](slices/ts-liveness.md#measurement-only-exclusions-and-incompleteness)
called it a syntax-invalid fixture, but that description is
not supported by its actual role. It was not excluded from this own-repo run.
The default unused-export cell must remain unknown until the runner is repaired
in a separately scoped change. TypeORM's four deliberately invalid ambient
codemod input/output fixtures are different: they explain the project-wide parse
gap, but do not justify treating an incomplete default aggregate as a zero score.

Next acceptance work is concrete: repair the benchmark runner in its own scope;
establish a sound fixture/reference-domain policy for the TypeORM parse gap;
review/fix the 33 own-Rust and 10 in-domain own-TS dead-code rows; address the
scoped timing miss; and run the seven-repository diagnostics/footer comparison.
This upgrade change is limited to cache tests and reporting. No scanner,
parser or benchmark-driver repair is bundled with those changes.

## Historical baseline report

This report measures how often AFT's inspect findings match the authoritative tool for each
language. It covers six pinned open-source repositories in four languages. The harness is
`benchmarks/inspect-truth/run.py`. The generated tables and every sampled row are in
`results/results.md` and `results/results.json`. This file adds the judgement that a script
cannot make: for each sampled disagreement, whether AFT or the oracle is right, and why.

AFT binary: a release build of this checkout (`agent-file-tools` 0.58.2, base commit
`13dd9cb71`), built with `CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER= cargo build --release -p agent-file-tools`.

## What already existed, and what was reused

- `benchmarks/inspect-field-audit/` is a 12-repo inspect field audit. It records tool
  presence and hand-verified samples, but runs no oracle and computes no precision or
  recall. Reused: its NDJSON client (`NdjsonClient`) and Tier 2 readiness check
  (`pending_tier2`), imported directly. Two corpus entries were also reused
  (`soft-serve`, `fastapi`).
- No earlier fallow or knip comparison script, oracle normalizer or oxc-resolver spike was
  found in `docs/`, `benchmarks/`, `scripts/`, `spikes/` or `.cortexkit/alfonso/`. The oxc
  resolver now ships as production code (`crates/aft/src/inspect/oxc_engine/`).
  `~/Work/OSS/fallow` is a clone of fallow's source, not a harness.

## Corpus

Clones live in `~/Work/OSS/inspect-corpus/<name>`, outside this repo. `run.py fetch`
checks out the pinned commit.

| name | language | role | commit | why |
|---|---|---|---|---|
| outline | TypeScript | app, about 2,200 .ts/.tsx files | `d410db1b24594d71b766212282804d79ca18e5c1` | Large single-package React + Koa app with path aliases and plugins. App-style dead code with no published API. |
| typeorm | TypeScript | library, about 3,600 .ts files | `17e858da8d2becab1ca47826f670a4660da70142` | Widely used ORM. pnpm monorepo whose public API ships from a build directory (`publishConfig.directory`) and a `"./*"` subpath export. |
| ripgrep | Rust | workspace | `3fce3b5bb0236da2df6d99672afb8a719642eca7` | Ten library crates plus the `rg` binary crate. Covers bin-crate code and `lib.rs` re-exports. |
| axum | Rust | workspace, macro and trait heavy | `f8b02f22cf10bee707bda19b58265b9e33677535` | Built from blanket trait impls, `macro_rules!`-generated impls and types (`define_rejection!`, `all_the_tuples!`), plus a proc-macro crate. |
| soft-serve | Go | module | `37685d36f5b7bf0e32217ddd7c8e045c57772619` | Actively maintained Git server: cobra CLI, interfaces, internal packages. |
| fastapi | Python | package | `26dbe0b00e1211b99446500fc6e402cfec6a3b99` | Widely used typed Python package. |

Comparisons are limited to each repo's `include_paths` (in `corpus.json`). Test files are
dropped from both sides (see Method).

## Oracles

| category | language | oracle | status |
|---|---|---|---|
| unused exports, dead code | TS | `fallow dead-code --format json --include-entry-exports` (2.88.3), minus files `fallow list --entry-points` names | ran |
| second opinion | TS | `knip --reporter json` (installed in `~/Work/OSS/inspect-corpus/.tools/node`) | ran; used to label each disagreement |
| dead files | TS | fallow `unused_files` | ran; AFT has no dead-file category |
| dead code | Rust | rustc `dead_code` lint from `cargo +stable check --message-format=json` | ran; 0 warnings in both repos |
| dead code | Rust | `cargo +nightly udeps` | **not run**: not installed, and no new toolchain was installed for this run |
| dead code | Go | `staticcheck -checks U1000` (installed in `.tools/go-bin`) | ran; 0 findings |
| dead code | Go | `golang.org/x/tools/cmd/deadcode` (added; installed in `.tools/go-bin`) | ran; see below for why it was added |
| dead code | Python | `vulture --min-confidence 60` (installed in `.tools/py`) | ran |
| diagnostics | all | `tsc --noEmit`, `cargo check`, `go vet`, `pyright` (with fastapi's own `.venv`) | ran |
| todos | all | a comment-marker regex | ran; a cross-check, not authoritative |
| duplicates | all | none | **not scored**: no authoritative oracle was named |

Why deadcode was added for Go: U1000 treats every exported identifier as used, but AFT's
dead-code scanner only reports exported symbols. U1000 therefore cannot confirm or refute
any AFT finding; its 0 on soft-serve says nothing about AFT. `deadcode` walks the whole
program from `main` and does report exported functions. It is the Go team's own tool, and
like the other oracles it went into the scratch prefix.

## Method

1. **Full AFT lists.** Inspect's per-category item lists are capped at 100 rows. `topK` cannot
   raise the cap and there is no offset. A scoped inspect, though, rolls up the whole project,
   filters to the scope, and only then caps, while `summary.<category>.count` keeps the full
   in-scope count. The harness compares that count to the rows returned and splits the scope
   until each piece fits (a path list in halves, a directory into its children). Every
   category's collected total equals AFT's project-wide count, with one exception:
   `typeorm/packages/typeorm/src/driver/mongodb/typings.ts` holds 134 unused exports in one
   file, and no request can return more than 100 of them. That leaves 34 rows unrecoverable,
   recorded in `aft_truncated_scopes`.
2. **Language servers off during collection.** Every inspect call computes every category,
   whatever `sections` asks for, and a scoped call first has the language servers analyze up
   to 1,000 scoped files. With servers on, collecting typeorm's lists ran past 30 minutes;
   the first attempt hit the shell timeout. Collection therefore uses a user-tier config with
   `lsp.disabled` (the project tier drops that key). The diagnostics pass runs in a separate
   session with servers on. A full re-run reproduced every count exactly.
3. **Normalization.** Both sides become `(category, path, symbol, line)`. Rows pair on
   `(path, symbol)`; when a file declares a name twice, the nearest line wins. Diagnostics
   and todos pair on `(path, line)`.
4. **Test files** (AFT's own `is_test_tree_file` segments plus `test/`, `testing/`, `testdata/`)
   are dropped from both sides. AFT withholds dead symbols in them by design. The per-repo
   counts dropped are in `test_file_findings_excluded`.
5. **TS dead code vs unused exports.** fallow's unit is the unused *export*. An export used
   only inside its own file is an unneeded `export` keyword, not dead code. For the
   `dead_code` comparison those rows (185 in outline, 69 in typeorm) are removed from the
   oracle. When fallow reports a whole file unused, AFT findings in that file count as
   "agreed (file-level)".
6. **Cause hints.** Every disagreement gets a grep-based hint (namespace member reference,
   dynamic import, interface method, library public API, and so on). The hints count causes
   over whole buckets. The verdicts below were checked by reading the code.

## Results

Precision = (agreed + file-level agreed) / AFT count. Recall = agreed / oracle count.

| repo | lang | category | AFT | oracle | agreed | AFT-only | oracle-only | precision | recall |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|
| outline | TS | unused_exports | 223 | 289 | 221 | 2 | 68 | 0.99 | 0.77 |
| outline | TS | dead_code | 386 | 104 | 81 | 305 | 23 | 0.21 | 0.78 |
| outline | TS | dead_files | n/a | 307 | n/a | n/a | n/a | n/a | n/a |
| outline | TS | todos | 19 | 20 | 19 | 0 | 1 | 1.00 | 0.95 |
| outline | TS | diagnostics (60 files) | 1 | 0 | 0 | 1 | 0 | 0.00 | n/a |
| typeorm | TS | unused_exports | 121 | 78 | 17 (+4 file) | 100 | 61 | 0.17 | 0.22 |
| typeorm | TS | dead_code | 75 | 9 | 0 (+4 file) | 71 | 9 | 0.05 | 0.00 |
| typeorm | TS | dead_files | n/a | 19 | n/a | n/a | n/a | n/a | n/a |
| typeorm | TS | todos | 10 | 10 | 10 | 0 | 0 | 1.00 | 1.00 |
| typeorm | TS | diagnostics (60 files) | 0 | 0 | 0 | 0 | 0 | n/a | n/a |
| ripgrep | Rust | dead_code | 52 | 0 | 0 | 52 | 0 | 0.00 | n/a |
| ripgrep | Rust | todos | 11 | 11 | 11 | 0 | 0 | 1.00 | 1.00 |
| ripgrep | Rust | diagnostics (60 files) | 1 | 0 | 0 | 1 | 0 | 0.00 | n/a |
| axum | Rust | dead_code | 55 | 0 | 0 | 55 | 0 | 0.00 | n/a |
| axum | Rust | todos | 5 | 5 | 5 | 0 | 0 | 1.00 | 1.00 |
| axum | Rust | diagnostics (60 files) | 0 | 0 | 0 | 0 | 0 | n/a | n/a |
| soft-serve | Go | dead_code vs U1000 | 101 | 0 | 0 | 101 | 0 | 0.00 | n/a |
| soft-serve | Go | dead_code (functions) vs deadcode | 98 | 20 | 5 | 93 | 15 | 0.05 | 0.25 |
| soft-serve | Go | todos | 33 | 33 | 33 | 0 | 0 | 1.00 | 1.00 |
| soft-serve | Go | diagnostics | 0 | 0 | 0 | 0 | 0 | n/a | n/a |
| fastapi | Python | dead_code | 0 | 123 | 0 | 0 | 123 | n/a | 0.00 |
| fastapi | Python | todos | 17 | 17 | 17 | 0 | 0 | 1.00 | 1.00 |
| fastapi | Python | diagnostics (52 files) | 57 | 57 | 57 | 0 | 0 | 1.00 | 1.00 |

Duplicates are not scored. AFT reported 11 duplicate groups on soft-serve; the other repos'
counts are in `results.json` (`aft_duplicate_groups`).

### Reading the numbers after judging the samples

Raw precision and recall treat the oracle as truth. It is not always right, so here is how
the scores change once the judged samples are taken into account.

- **TS unused exports are good on an app** (outline 0.99 / 0.77). Most of outline's 68
  oracle-only rows are not real AFT misses. 30 are `export type` declarations. The sampled
  ones are used only in their own file, which AFT keeps on purpose
  (`oxc_engine/graph.rs:1037`); fallow and knip report them. Several more are fallow false positives (see "Where the oracle was wrong").
- **TS dead code over-reports badly**: 305 of 386 outline findings are wrong, 233 of them
  from a single cause. AFT's own `unused_exports` gets the same symbols right, so the dead-code
  reachability walk is the defect, not export analysis.
- **TS on a library is poor** (typeorm 0.17 / 0.05 precision). Every AFT-only row in the
  sample is public API.
- **Rust precision is effectively 0.** rustc reports no dead code in either workspace, so any
  `pub(crate)` or bin-crate item AFT flags is in use, and any plain `pub` item in a library
  crate is public API. All 107 Rust findings are false positives, with causes below.
- **Go precision is 0.05.** 93 of 98 function findings are reachable from `main`.
  AFT also misses 15 functions deadcode finds unreachable.
- **Python**: AFT has no Python dead-code lane, so recall is 0 against vulture's 123 rows.
  All 10 sampled vulture rows are vulture false positives, though: pydantic model fields
  read through serialization, and a protocol parameter (`*exc_info` in `__aexit__`). The
  practical miss rate is far below 123.
- **TODOs agree everywhere** (the one outline difference is a marker the regex matches
  inside a string).
- **Diagnostics agree where both sides ran.** fastapi matched pyright 57/57 once pyright
  used the project's venv. The disagreements are producer differences, not errors (below).

## Top causes of AFT errors

Ranked by the number of findings each explains in this corpus, AFT-only and oracle-only
combined, after reading the code.

### 1. Library public API is not treated as live (about 250 findings: TS 171, Rust about 80)

- **typeorm**: all 100 AFT-only unused exports and 54 of 71 AFT-only dead-code rows are in
  `packages/typeorm/src/driver/mongodb/typings.ts`. `package.json` publishes every file
  through `"exports": {"./*": ...}` from `publishConfig.directory: build/package`. AFT
  ignores `*` subpath patterns and does not map `./index.js` (no `dist/` or `build/` prefix)
  back to `src/index.ts`. Example: `typings.ts:5969 MongoClientBulkWriteExecutionError`.
  Another 13 dead-code rows are public error and decorator classes reached only through
  `src/index.ts` → `export * from "./error"` → `export * from "./ConnectionNotFoundError"`.
  Examples: `error/ConnectionNotFoundError.ts:6`, `decorator/listeners/AfterInsert.ts:8`.
  knip agrees with AFT on none of these rows.
- **Rust libraries**: `pub fn` methods on exported types, and `pub` items reached through
  `pub mod` or a glob `pub use …::*`, are flagged. Examples:
  `ripgrep crates/printer/src/json.rs:564 has_written`,
  `crates/globset/src/glob.rs:641 empty_alternates`,
  `axum/src/extract/ws.rs:913 into_text`, `axum-extra/src/either.rs:129 Either4`. The roughly
  22 axum rejection types defined inside `define_rejection! { … }` and re-exported by
  `pub use axum_core::extract::rejection::*` are the same failure, reached through a macro
  invocation (`axum/src/extract/rejection.rs:34 MissingJsonContentType`).

### 2. TS dead code ignores namespace member references (233 findings)

`import * as T from "./schema"` followed by a type-position use such as
`APIContext<T.CommentsUnresolveReq>` does not keep `CommentsUnresolveReq` live in the
dead-code walk. Outline routes every API schema type through this pattern: 233 rows, for
example `server/routes/api/comments/schema.ts:130 CommentsInfoReq` and
`server/routes/api/users/schema.ts:109 UsersNotificationsUnsubscribeReq`. AFT's unused-exports scanner handles the same imports (it marks namespace-imported modules
uncertain), so the two categories contradict each other on these symbols. knip marks
them used.

### 3. Go: uses through function values, closures and interfaces are not credited (108 findings: 93 false positives, 15 misses)

- Calls inside closures assigned to fields of package-level variables are not attributed to
  any live symbol. `cmd/soft/hook/hook.go:30` defines `Command = &cobra.Command{
  PersistentPreRunE: func(...) { cmd.InitBackendContext(c, args) … } }`. Everything
  reachable only from such closures is reported dead, which in a cobra CLI is most of the
  program: `cmd/cmd.go:19 InitBackendContext`, then `pkg/backend/backend.go:25 New`,
  `pkg/store/context.go:8 FromContext`, and on down. The cause hints count 57 such rows as
  "called elsewhere". The calls exist, but in code AFT considers dead.
- Function values are not counted as uses (17 rows): `PersistentPreRunE: cmd.InitBackendContext`,
  `ssh.go:73 CommandMiddleware,`, `http.HandlerFunc(GoGetHandler)`.
- Methods that satisfy an interface and are called only through it (9 rows):
  `pkg/store/database/webhooks.go:55 DeleteWebhookByID` implements `store.WebhookStore`.
- The reverse error comes from the name-based method-dispatch heuristic: 15 functions
  deadcode proves unreachable are kept live because the same method name is called on some
  other type. Examples: `pkg/ui/pages/repo/stashitem.go:45 Less` (sort.Interface on a type
  never sorted) and `cmd/soft/serve/server.go:259 Close`.

### 4. TS dead code does not follow dynamic `import()` (70 findings)

`app/hooks/useSettingsConfig.ts:33 lazy(() => import("~/scenes/Settings/ApiKeys"))` loads a
default export the dead-code walk never reaches, so
`app/scenes/Settings/ApiKeys.tsx:159 default` is reported dead. All 70 rows are default
exports of modules that some file loads with `import("…")`, according to the grep hint. The
sampled ones are lazily loaded settings scenes and plugin pages. Unused exports does not
flag them.

### 5. Rust: calls through `use … as` aliases and inside `macro_rules!` bodies (about 21 findings)

- ripgrep's `crates/core/flags/mod.rs:18` re-exports `bash::generate as generate_complete_bash`
  and ten more aliases, and `main.rs` calls `flags::generate_complete_bash()`. None of the
  aliased functions is credited (`complete/bash.rs:59 generate`, `doc/help.rs:119 generate_long`,
  `doc/version.rs:10 generate_digits`), and neither is anything reachable only from them
  (`flags/parse.rs:147 lookup`): about 13 rows.
- `crates/core/messages.rs:100 messages`, `:112 ignore_messages` and `:133 set_errored` are
  called only from inside the crate's `message!`/`err_message!` `macro_rules!` bodies.
- A handful of `Type::assoc()` and `module::func()` calls in ripgrep are not credited
  (`printer/src/util.rs:191 Sunk::from_sink_match`, `searcher/src/lines.rs:115
  lines::without_terminator`). Their callers are live trait-impl or crate-private methods, so
  these look like path-call resolution misses that remain after AFT's earlier fix for
  path-expression calls. Confidence
  is lower here: the callers are not themselves visible in AFT's output.

Also seen, at lower counts:

- `export type` used in its own file is kept on purpose (about 30 outline rows).
- Unused default exports of live modules are not reported by dead code
  (`shared/editor/lib/markInputRule.ts:21`).
- A vendored `.ts` file of `export declare …` ambient declarations is treated as real
  exports: only `.d.ts` ambient exports are skipped (`graph.rs:1128`). This adds to cause 1
  on typeorm.

## Where the oracle was wrong

- **fallow, namespace member use**: `app/components/primitives/components/Menu.tsx:109
  MenuSubTrigger` is used as `Components.MenuSubTrigger` (`Menu/index.tsx:142`). fallow reports
  it unused; knip and AFT get it right.
- **fallow, member access on a dynamic import**: `server/converters/CsvConverter.ts:9` is
  loaded by `(await import("./CsvConverter")).CsvConverter`. Same for the `server/services/*`
  defaults, which `server/main.ts:117` loads through a lazy map.
- **fallow, unused files**: 306 of outline's 311 unused files are Sequelize migrations, which
  sequelize-cli loads by directory. typeorm's command classes are imported by `src/cli.ts`,
  but fallow does not map the `bin` entry `./cli.js` to source, so it reports
  `commands/CacheClearCommand.ts:12` and its siblings unused. AFT is right on all of these.
- **fallow's default mode cannot be used for apps**: its plugins turned 2,228 of outline's
  2,231 files into entry points, which reports 0 unused exports. This is why the harness
  uses `--include-entry-exports` and removes only the files fallow itself lists as entries.
- **vulture** on fastapi: all 10 sampled rows are false positives (pydantic fields, protocol
  parameters).
- **rustc** cannot see public API. Its 0 is right for these workspaces, but a library item
  rustc stays silent on is not thereby used; each AFT row was judged by reading the code.

## Diagnostics notes

- The outline AFT-only row is an oxlint lint warning (`useShareDataLoader.ts:30`, setState in
  an effect). tsc has no such check. This is a producer difference, not an error.
- The ripgrep AFT-only row is correct: `crates/globset/benches/bench.rs:5 #![feature(test)]`
  fails on stable. rust-analyzer checks bench targets; the oracle skips them because they
  break `cargo check --all-targets` on stable.
- **typeorm: AFT produces no TypeScript diagnostics at all.** The project-root server reports
  `TypeScript SDK unavailable … Could not find a valid TypeScript installation`, because pnpm
  installs `typescript` under `packages/typeorm/node_modules`, not at the root. The gap is
  named, but `E`/`W` read 0 for every TS file in the repo.
- **Go: AFT has no Go diagnostics producer here** (`gopls is unavailable`). Nothing is
  measured; `go vet` is clean.

## Tool problems to pass to the AFT owner

1. Every scoped inspect call recomputes the full uncapped rollup and runs the scoped
   diagnostics sweep, even when `sections` excludes diagnostics. That is 6 s per call with
   servers off and 40-60 s with them on (typeorm). Input: `{"command":"inspect","sections":["dead_code"],"scope":["packages"]}` on
   typeorm. Output: a `diagnostics` summary with `coverage.examined: 1000`. Expected: no
   diagnostics work when diagnostics is not requested. The full typeorm collection took 84
   inspect calls.
2. Lists are hard-capped at 100 with no offset, and the cap applies per scope. A single file
   with more than 100 findings can never be listed in full (typeorm `typings.ts`: 134, 100
   returned).
3. On a cold repo, inspect returns `dead_code: {"unavailable": true, … "inspect_phase_timeout
   … builder_state=building"}`. The `pending_categories` list stays empty, so a client that
   polls `scanner_state` believes the result is final.
4. Test-only rows carry a `used_by` list that can include non-test files.
   `cmd/soft/serve/server.go NewServer` has `used_by: [certreloader_test.go, server.go]` yet is
   test-only. The real non-test caller sits in a closure assigned to a package-level variable (cause 3 above), so the label is
   misleading.

## How to re-run

```
CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER= cargo build --release -p agent-file-tools
python3 benchmarks/inspect-truth/run.py fetch          # fetch/install the pinned corpus
python3 benchmarks/inspect-truth/run.py collect --repo axum --aft-bin target/release/aft --baseline-bin <exact-13dd9cb71-binary>
python3 benchmarks/inspect-truth/run.py score --aft-commit <candidate-source-commit> # re-score saved raw output
```

Oracle binaries are expected in `~/Work/OSS/inspect-corpus/.tools`:

- `go-bin/staticcheck` and `go-bin/deadcode`: `GOBIN=… go install …@latest`
- `py/bin/vulture`: `uv venv py && uv pip install vulture`
- `node/node_modules/.bin/knip`: `npm install --prefix node knip typescript`

Also needed: `/opt/homebrew/bin/fallow`, and `pyright`, `cargo` and `go` on `PATH`. The TS
repos and fastapi need their dependencies installed (`fetch` runs the TS installs; fastapi
uses `uv venv .venv && uv pip install -e .`). Raw tool output, AFT storage and full
disagreement lists go to `~/Work/OSS/inspect-corpus/_results/<repo>/`.

## Where the oracle was wrong

These are the **existing oracle-side judgments**, not newly inferred removals.
`judgments/ts-liveness.json` contains 112 oracle entries covering 89 distinct
(repo,path,symbol) rows; separate entries preserve category boundaries. Outline
has 15 distinct `oracle_wrong` rows, each judged for both dead_code and
unused_exports. TypeORM has 74 distinct public-API rows, 8 also judged for
dead_code; 17 MongoDB typings rows are outside the baseline's truncated-scope
comparison, so the scored unused-export removal count is 57, not 74.

| Repo / path | Judged symbols | Reason |
|---|---|---|
| outline `app/components/primitives/components/Menu.tsx` | `MenuContent`, `MenuExternalLink`, `MenuInternalLink`, `MenuShortcut`, `MenuSubTrigger`, `SelectedIconWrapper` | Production `Components.X` namespace members; concrete use sites in `app/components/primitives/Menu/index.tsx` and `app/components/Menu/transformer.tsx` are recorded per row |
| outline `server/converters/CsvConverter.ts` | `CsvConverter` | `(await import("./CsvConverter")).CsvConverter`, selected in `DocumentConverter.ts:265-267` and used for production conversion |
| outline `server/converters/DocxConverter.ts` | `DocxConverter` | Awaited literal import selected at `DocumentConverter.ts:257-259` |
| outline `server/converters/MimeArchiveConverter.ts` | `MimeArchiveConverter` | Awaited literal import selected at `DocumentConverter.ts:261-263` |
| outline `server/converters/TextPackConverter.ts` | `TextPackConverter` | Awaited literal import selected at `DocumentConverter.ts:269-271` |
| outline `server/services/{admin,collaboration,cron,websockets,worker}.ts` | Each file's `default` | Loaders registered in `server/services/index.ts:7-14`; `server/main.ts:110-118` selects, awaits and invokes them |
| typeorm `packages/typeorm/src/commands/*.ts` | `CacheClearCommand`, `MigrationRevertCommand`, `MigrationRunCommand`, `MigrationShowCommand`, `QueryCommand`, `SchemaDropCommand`, `SchemaLogCommand`, `SchemaSyncCommand`, `EntityCreateCommand`, `InitCommand`, `SubscriberCreateCommand`, `VersionCommand` (each in its same-named file) | `public_api`; package `./*` export covers the published source root |
| typeorm `packages/typeorm/src/driver/mongodb/bson.typings.ts` | `BSON`, `BSONError`, `BSONOffsetError`, `BSONRegExpExtended`, `BSONRegExpExtendedLegacy`, `BSONRuntimeError`, `BSONSymbolExtended`, `BSONTypeTag`, `BSONValue`, `BSONVersionError`, `BinaryExtended`, `BinaryExtendedLegacy`, `BinarySequence`, `ByteUtils`, `CalculateObjectSizeOptions`, `CodeExtended`, `DBRefLike`, `Decimal128Extended`, `DeserializeOptions`, `DoubleExtended`, `EJSON`, `EJSONOptions`, `EJSONOptionsBase`, `EJSONParseOptions`, `EJSONSerializeOptions`, `Int32Extended`, `LongExtended`, `LongWithoutOverrides`, `LongWithoutOverridesClass`, `MaxKeyExtended`, `MinKeyExtended`, `NumberUtils`, `ObjectIdExtended`, `ObjectIdLike`, `OnDemand`, `SerializeOptions`, `TimestampExtended`, `TimestampOverrides`, `UUIDExtended`, `bsonType`, `calculateObjectSize`, `deserializeStream`, `onDemand`, `serializeWithBufferAndIndex`, `setInternalBufferSize` | `public_api`; declaration syntax in a `.ts` file is not the exemption |
| typeorm `packages/typeorm/src/driver/mongodb/typings.ts` | `BSON`, `BSONRegExp`, `BSONSymbol`, `BSONType`, `Binary`, `Code`, `DBRef`, `Decimal128`, `Double`, `Int32`, `Long`, `MaxKey`, `MinKey`, `Timestamp`, `UUID`, `deserialize`, `serialize` | `public_api`; baseline scope truncated at 100/134, excluded from both comparisons |

There are no committed Rust/Go/Python oracle-side row removals. Rust's zero-row
oracle and the residual Go disagreements are not transformed into higher
precision by deleting AFT findings. The own-repository unreviewed rows remain
explicitly unjudged.

## Reading the folded tables

`AFT` and `oracle` count findings inside the corpus entry's scoring prefixes,
after test-path/truncated-scope exclusions. `agreed (+file)` gives exact matched
rows followed by additional matches supported by oracle-unused-file evidence.
Precision is matched findings divided by AFT findings; recall is matched findings
divided by oracle findings. `n/a` denotes an undefined ratio, not a zero.
`oracle_rows_judged_out` counts category-specific oracle rows removed by recorded
judgments. AFT judgments never remove AFT rows: `aft_rows_judged` and
`aft_rows_unjudged` record review coverage. `unknown` means analysis was pending,
unavailable or incomplete and the category cannot be scored as an empty set.
An unsupported-language gap alone does not prevent scoring supported languages.

The historical callgraph progress columns identify the initial extractor change
(`a37cdbe83`), alias/macro follow-up (`213581cc8`) and custom-crate-root repair
(`9e8c83db7`). Its verdict column applies the targets at that measured revision,
not at the final candidate. Inspect-tool's accuracy cells were blocked because
its historical raw judgment-origin records were missing; its paging table is
an independent unused-export list check with topK100.

## Folded slice tables

These tables are copied verbatim from each slice report. Historical tables retain
their original measurement revision and judgments; they are not silently
rescored as the final candidate. The final upgrade table is the new seven-repo
rerun. Inspect-tool records blocked accuracy cells rather than invented scores.

### harness

Source: [slices/harness.md](slices/harness.md).

#### Before

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| outline | typescript | unused_exports | scored | 223 | 289 | 221 (+0) | 0.99 | 0.77 | 0 | 0 | 223 |
| outline | typescript | dead_code | scored | 386 | 104 | 81 (+0) | 0.21 | 0.78 | 0 | 0 | 386 |
| outline | typescript | todos | scored | 19 | 20 | 19 (+0) | 1.00 | 0.95 | 0 | 0 | 19 |
| typeorm | typescript | unused_exports | scored | 21 | 61 | 17 (+4) | 1.00 | 0.28 | 0 | 0 | 21 |
| typeorm | typescript | dead_code | unknown | n/a | 9 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| typeorm | typescript | todos | scored | 10 | 10 | 10 (+0) | 1.00 | 1.00 | 0 | 0 | 10 |
| ripgrep | rust | dead_code | scored | 52 | 0 | 0 (+0) | 0.00 | n/a | 0 | 0 | 52 |
| ripgrep | rust | todos | scored | 11 | 11 | 11 (+0) | 1.00 | 1.00 | 0 | 0 | 11 |
| axum | rust | dead_code | scored | 55 | 0 | 0 (+0) | 0.00 | n/a | 0 | 0 | 55 |
| axum | rust | todos | scored | 5 | 5 | 5 (+0) | 1.00 | 1.00 | 0 | 0 | 5 |
| soft-serve | go | dead_code | scored | 101 | 0 | 0 (+0) | 0.00 | n/a | 0 | 0 | 101 |
| soft-serve | go | dead_code (functions) | scored | 98 | 20 | 5 (+0) | 0.05 | 0.25 | 0 | 0 | 98 |
| soft-serve | go | todos | scored | 33 | 33 | 33 (+0) | 1.00 | 1.00 | 0 | 0 | 33 |
| fastapi | python | dead_code | scored | 0 | 123 | 0 (+0) | n/a | 0.00 | 0 | 0 | 0 |
| fastapi | python | todos | scored | 17 | 17 | 17 (+0) | 1.00 | 1.00 | 0 | 0 | 17 |

#### After

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| outline | typescript | unused_exports | scored | 223 | 289 | 221 (+0) | 0.99 | 0.77 | 0 | 0 | 223 |
| outline | typescript | dead_code | scored | 386 | 104 | 81 (+0) | 0.21 | 0.78 | 0 | 0 | 386 |
| outline | typescript | todos | scored | 19 | 20 | 19 (+0) | 1.00 | 0.95 | 0 | 0 | 19 |
| typeorm | typescript | unused_exports | scored | 21 | 61 | 17 (+4) | 1.00 | 0.28 | 0 | 0 | 21 |
| typeorm | typescript | dead_code | scored | 75 | 9 | 0 (+4) | 0.05 | 0.00 | 0 | 0 | 75 |
| typeorm | typescript | todos | scored | 10 | 10 | 10 (+0) | 1.00 | 1.00 | 0 | 0 | 10 |
| ripgrep | rust | dead_code | scored | 52 | 0 | 0 (+0) | 0.00 | n/a | 0 | 0 | 52 |
| ripgrep | rust | todos | scored | 11 | 11 | 11 (+0) | 1.00 | 1.00 | 0 | 0 | 11 |
| axum | rust | dead_code | scored | 55 | 0 | 0 (+0) | 0.00 | n/a | 0 | 0 | 55 |
| axum | rust | todos | scored | 5 | 5 | 5 (+0) | 1.00 | 1.00 | 0 | 0 | 5 |
| soft-serve | go | dead_code | scored | 101 | 0 | 0 (+0) | 0.00 | n/a | 0 | 0 | 101 |
| soft-serve | go | dead_code (functions) | scored | 98 | 20 | 5 (+0) | 0.05 | 0.25 | 0 | 0 | 98 |
| soft-serve | go | todos | scored | 33 | 33 | 33 (+0) | 1.00 | 1.00 | 0 | 0 | 33 |
| fastapi | python | dead_code | scored | 0 | 123 | 0 (+0) | n/a | 0.00 | 0 | 0 | 0 |
| fastapi | python | todos | scored | 17 | 17 | 17 (+0) | 1.00 | 1.00 | 0 | 0 | 17 |
| aft | rust | dead_code | unknown | n/a | 0 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| aft | rust | todos | scored | 0 | 6 | 0 (+0) | n/a | 0.00 | 0 | 0 | 0 |
| aft | typescript | unused_exports | scored | 110 | 159 | 110 (+0) | 1.00 | 0.69 | 0 | 0 | 110 |
| aft | typescript | dead_code | unknown | n/a | 36 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| aft | typescript | todos | scored | 0 | 0 | 0 (+0) | n/a | n/a | 0 | 0 | 0 |

### dead-code-scanner

Source: [slices/dead-code-scanner.md](slices/dead-code-scanner.md).

#### Before

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| outline | typescript | unused_exports | scored | 223 | 289 | 221 (+0) | 0.99 | 0.77 | 0 | 0 | 223 |
| outline | typescript | dead_code | scored | 386 | 104 | 81 (+0) | 0.21 | 0.78 | 0 | 0 | 386 |
| outline | typescript | todos | scored | 19 | 20 | 19 (+0) | 1.00 | 0.95 | 0 | 0 | 19 |
| typeorm | typescript | unused_exports | scored | 21 | 61 | 17 (+4) | 1.00 | 0.28 | 0 | 0 | 21 |
| typeorm | typescript | dead_code | unknown | n/a | 9 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| typeorm | typescript | todos | scored | 10 | 10 | 10 (+0) | 1.00 | 1.00 | 0 | 0 | 10 |
| ripgrep | rust | dead_code | scored | 52 | 0 | 0 (+0) | 0.00 | n/a | 0 | 31 | 21 |
| ripgrep | rust | todos | scored | 11 | 11 | 11 (+0) | 1.00 | 1.00 | 0 | 0 | 11 |
| axum | rust | dead_code | scored | 55 | 0 | 0 (+0) | 0.00 | n/a | 0 | 6 | 49 |
| axum | rust | todos | scored | 5 | 5 | 5 (+0) | 1.00 | 1.00 | 0 | 0 | 5 |
| soft-serve | go | dead_code | scored | 101 | 0 | 0 (+0) | 0.00 | n/a | 0 | 2 | 99 |
| soft-serve | go | dead_code (functions) | scored | 98 | 20 | 5 (+0) | 0.05 | 0.25 | 0 | 2 | 96 |
| soft-serve | go | todos | scored | 33 | 33 | 33 (+0) | 1.00 | 1.00 | 0 | 0 | 33 |

#### After

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| outline | typescript | unused_exports | scored | 223 | 289 | 221 (+0) | 0.99 | 0.77 | 0 | 0 | 223 |
| outline | typescript | dead_code | scored | 386 | 104 | 81 (+0) | 0.21 | 0.78 | 0 | 0 | 386 |
| outline | typescript | todos | scored | 19 | 20 | 19 (+0) | 1.00 | 0.95 | 0 | 0 | 19 |
| typeorm | typescript | unused_exports | scored | 21 | 61 | 17 (+4) | 1.00 | 0.28 | 0 | 0 | 21 |
| typeorm | typescript | dead_code | scored | 75 | 9 | 0 (+4) | 0.05 | 0.00 | 0 | 0 | 75 |
| typeorm | typescript | todos | scored | 10 | 10 | 10 (+0) | 1.00 | 1.00 | 0 | 0 | 10 |
| ripgrep | rust | dead_code | scored | 21 | 0 | 0 (+0) | 0.00 | n/a | 0 | 21 | 0 |
| ripgrep | rust | todos | scored | 11 | 11 | 11 (+0) | 1.00 | 1.00 | 0 | 0 | 11 |
| axum | rust | dead_code | scored | 10 | 0 | 0 (+0) | 0.00 | n/a | 0 | 10 | 0 |
| axum | rust | todos | scored | 5 | 5 | 5 (+0) | 1.00 | 1.00 | 0 | 0 | 5 |
| soft-serve | go | dead_code | scored | 121 | 0 | 0 (+0) | 0.00 | n/a | 0 | 0 | 121 |
| soft-serve | go | dead_code (functions) | scored | 118 | 20 | 9 (+0) | 0.08 | 0.45 | 0 | 0 | 118 |
| soft-serve | go | todos | scored | 33 | 33 | 33 (+0) | 1.00 | 1.00 | 0 | 0 | 33 |
| aft | rust | dead_code | unknown | n/a | 0 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| aft | rust | todos | scored | 0 | 6 | 0 (+0) | n/a | 0.00 | 0 | 0 | 0 |
| aft | typescript | unused_exports | scored | 110 | 161 | 110 (+0) | 1.00 | 0.68 | 0 | 0 | 110 |
| aft | typescript | dead_code | unknown | n/a | 37 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| aft | typescript | todos | scored | 0 | 0 | 0 (+0) | n/a | n/a | 0 | 0 | 0 |

### ts-liveness

Source: [slices/ts-liveness.md](slices/ts-liveness.md).

#### Before

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| outline | typescript | unused_exports | scored | 223 | 274 | 221 (+0) | 0.99 | 0.81 | 15 | 8 | 215 |
| outline | typescript | dead_code | scored | 386 | 89 | 66 (+0) | 0.17 | 0.74 | 15 | 22 | 364 |
| outline | typescript | todos | scored | 19 | 20 | 19 (+0) | 1.00 | 0.95 | 0 | 0 | 19 |
| typeorm | typescript | unused_exports | scored | 21 | 4 | 3 (+4) | 0.33 | 0.75 | 57 | 21 | 0 |
| typeorm | typescript | dead_code | scored | 75 | 1 | 0 (+4) | 0.05 | 0.00 | 8 | 75 | 0 |
| typeorm | typescript | todos | scored | 10 | 10 | 10 (+0) | 1.00 | 1.00 | 0 | 0 | 10 |

#### After

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| outline | typescript | unused_exports | scored | 240 | 274 | 238 (+0) | 0.99 | 0.87 | 15 | 8 | 232 |
| outline | typescript | dead_code | scored | 73 | 89 | 66 (+0) | 0.90 | 0.74 | 15 | 7 | 66 |
| outline | typescript | todos | scored | 19 | 20 | 19 (+0) | 1.00 | 0.95 | 0 | 0 | 19 |
| typeorm | typescript | unused_exports | scored | 3 | 4 | 3 (+0) | 1.00 | 0.75 | 57 | 3 | 0 |
| typeorm | typescript | dead_code | scored | 0 | 1 | 0 (+0) | n/a | 0.00 | 8 | 0 | 0 |
| typeorm | typescript | todos | scored | 10 | 10 | 10 (+0) | 1.00 | 1.00 | 0 | 0 | 10 |
| aft | rust | dead_code | unknown | n/a | 0 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| aft | rust | todos | scored | 0 | 7 | 0 (+0) | n/a | 0.00 | 0 | 0 | 0 |
| aft | typescript | unused_exports | scored | 113 | 166 | 113 (+0) | 1.00 | 0.68 | 0 | 0 | 113 |
| aft | typescript | dead_code | unknown | n/a | 38 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| aft | typescript | todos | scored | 0 | 0 | 0 (+0) | n/a | n/a | 0 | 0 | 0 |

### callgraph

Source: [slices/callgraph.md](slices/callgraph.md).

#### Before

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep | rust | dead_code | scored | 52 | 0 | 0 (+0) | 0.00 | n/a | 0 | 31 | 21 |
| ripgrep | rust | todos | scored | 11 | 11 | 11 (+0) | 1.00 | 1.00 | 0 | 0 | 11 |
| axum | rust | dead_code | scored | 55 | 0 | 0 (+0) | 0.00 | n/a | 0 | 6 | 49 |
| axum | rust | todos | scored | 5 | 5 | 5 (+0) | 1.00 | 1.00 | 0 | 0 | 5 |
| soft-serve | go | dead_code | scored | 101 | 0 | 0 (+0) | 0.00 | n/a | 0 | 29 | 72 |
| soft-serve | go | dead_code (functions) | scored | 98 | 20 | 5 (+0) | 0.05 | 0.25 | 0 | 26 | 72 |
| soft-serve | go | todos | scored | 33 | 33 | 33 (+0) | 1.00 | 1.00 | 0 | 0 | 33 |

#### After

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep | rust | dead_code | scored | 4 | 0 | 0 (+0) | 0.00 | n/a | 0 | 4 | 0 |
| ripgrep | rust | todos | scored | 11 | 11 | 11 (+0) | 1.00 | 1.00 | 0 | 0 | 11 |
| axum | rust | dead_code | scored | 9 | 0 | 0 (+0) | 0.00 | n/a | 0 | 9 | 0 |
| axum | rust | todos | scored | 5 | 5 | 5 (+0) | 1.00 | 1.00 | 0 | 0 | 5 |
| soft-serve | go | dead_code | scored | 29 | 0 | 0 (+0) | 0.00 | n/a | 0 | 29 | 0 |
| soft-serve | go | dead_code (functions) | scored | 26 | 20 | 9 (+0) | 0.35 | 0.45 | 0 | 26 | 0 |
| soft-serve | go | todos | scored | 33 | 33 | 33 (+0) | 1.00 | 1.00 | 0 | 0 | 33 |

#### Latest corpus measurement and rebuild on main

| Bucket | Baseline 13dd9cb71 | a37cdbe83 | 213581cc8 | Measured 9e8c83db7 | Verdict at measured tip |
|---|---:|---:|---:|---:|---|
| ripgrep dead_code | 52 | 21 | 16 | **4**; 4 judged, 0 unjudged | <=10 **PASS** |
| axum dead_code | 55 | 10 | 9 | **9**; 9 judged, 0 unjudged | <=10 **PASS** |
| soft-serve dead_code | 101 | 32 | 29 | **29**; 29 judged, 0 unjudged | Improved; no separate count threshold |
| soft-serve function rows | 98 | 29 | 26 | **26**; 26 judged, 0 unjudged | Fully explained |
| soft-serve function precision / recall | 0.05 / 0.25 | 0.31 / 0.45 | 0.35 / 0.45 | **0.35 / 0.45** | Precision <0.6 still missed and explained |
| Todos: ripgrep / axum / soft-serve | 11 / 5 / 33 | 11 / 5 / 33 | 11 / 5 / 33 | **11 / 5 / 33** | Unchanged |
| Cell rule | baseline | PASS | PASS | **PASS, no lowered precision/recall cell** | PASS |

### inspect-tool

Source: [slices/inspect-tool.md](slices/inspect-tool.md).

#### Delivery and corpus gate

| Corpus | Language | Before/after cells blocked |
|---|---|---|
| outline | TypeScript | unused_exports, dead_code, todos, diagnostics: precision and recall |
| typeorm | TypeScript | unused_exports, dead_code, todos, diagnostics: precision and recall |
| ripgrep | Rust | dead_code, todos, diagnostics: precision and recall |
| axum | Rust | dead_code, todos, diagnostics: precision and recall |
| soft-serve | Go | dead_code, dead_code (functions), todos, diagnostics: precision and recall |
| fastapi | Python | dead_code, todos, diagnostics: precision and recall |
| aft | Rust | dead_code, todos, diagnostics: precision and recall; new, no baseline |
| aft | TypeScript | unused_exports, dead_code, todos, diagnostics: precision and recall; new, no baseline |

#### Outline corpus paging: passed

| offset | shown | total | next_offset | cap reasons |
|---:|---:|---:|---:|---|
| 0 | 100 | 228 | 100 | cap |
| 100 | 100 | 228 | 200 | cap |
| 200 | 28 | 228 | null | none |
| 238 | 0 | 228 | null | none |

### upgrade-and-report

Source: [slices/upgrade-and-report.md](slices/upgrade-and-report.md).

#### Before

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| outline | typescript | unused_exports | scored | 223 | 274 | 221 (+0) | 0.99 | 0.81 | 15 | 8 | 215 |
| outline | typescript | dead_code | scored | 386 | 89 | 66 (+0) | 0.17 | 0.74 | 15 | 22 | 364 |
| outline | typescript | todos | scored | 19 | 20 | 19 (+0) | 1.00 | 0.95 | 0 | 0 | 19 |
| typeorm | typescript | unused_exports | scored | 21 | 4 | 3 (+4) | 0.33 | 0.75 | 57 | 21 | 0 |
| typeorm | typescript | dead_code | scored | 75 | 1 | 0 (+4) | 0.05 | 0.00 | 8 | 75 | 0 |
| typeorm | typescript | todos | scored | 10 | 10 | 10 (+0) | 1.00 | 1.00 | 0 | 0 | 10 |
| ripgrep | rust | dead_code | scored | 52 | 0 | 0 (+0) | 0.00 | n/a | 0 | 31 | 21 |
| ripgrep | rust | todos | scored | 11 | 11 | 11 (+0) | 1.00 | 1.00 | 0 | 0 | 11 |
| axum | rust | dead_code | scored | 55 | 0 | 0 (+0) | 0.00 | n/a | 0 | 6 | 49 |
| axum | rust | todos | scored | 5 | 5 | 5 (+0) | 1.00 | 1.00 | 0 | 0 | 5 |
| soft-serve | go | dead_code | scored | 101 | 0 | 0 (+0) | 0.00 | n/a | 0 | 29 | 72 |
| soft-serve | go | dead_code (functions) | scored | 98 | 20 | 5 (+0) | 0.05 | 0.25 | 0 | 26 | 72 |
| soft-serve | go | todos | scored | 33 | 33 | 33 (+0) | 1.00 | 1.00 | 0 | 0 | 33 |
| fastapi | python | dead_code | scored | 0 | 125 | 0 (+0) | n/a | 0.00 | 0 | 0 | 0 |
| fastapi | python | todos | scored | 17 | 17 | 17 (+0) | 1.00 | 1.00 | 0 | 0 | 17 |

#### After

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| outline | typescript | unused_exports | scored | 240 | 274 | 238 (+0) | 0.99 | 0.87 | 15 | 8 | 232 |
| outline | typescript | dead_code | scored | 73 | 89 | 66 (+0) | 0.90 | 0.74 | 15 | 7 | 66 |
| outline | typescript | todos | scored | 19 | 20 | 19 (+0) | 1.00 | 0.95 | 0 | 0 | 19 |
| typeorm | typescript | unused_exports | unknown | n/a | 4 | n/a (+n/a) | n/a | n/a | 57 | 0 | 0 |
| typeorm | typescript | dead_code | unknown | n/a | 1 | n/a (+n/a) | n/a | n/a | 8 | 0 | 0 |
| typeorm | typescript | todos | scored | 10 | 10 | 10 (+0) | 1.00 | 1.00 | 0 | 0 | 10 |
| ripgrep | rust | dead_code | scored | 4 | 0 | 0 (+0) | 0.00 | n/a | 0 | 4 | 0 |
| ripgrep | rust | todos | scored | 11 | 11 | 11 (+0) | 1.00 | 1.00 | 0 | 0 | 11 |
| axum | rust | dead_code | scored | 9 | 0 | 0 (+0) | 0.00 | n/a | 0 | 9 | 0 |
| axum | rust | todos | scored | 5 | 5 | 5 (+0) | 1.00 | 1.00 | 0 | 0 | 5 |
| soft-serve | go | dead_code | scored | 29 | 0 | 0 (+0) | 0.00 | n/a | 0 | 29 | 0 |
| soft-serve | go | dead_code (functions) | scored | 26 | 20 | 9 (+0) | 0.35 | 0.45 | 0 | 26 | 0 |
| soft-serve | go | todos | scored | 33 | 33 | 33 (+0) | 1.00 | 1.00 | 0 | 0 | 33 |
| fastapi | python | dead_code | scored | 0 | 125 | 0 (+0) | n/a | 0.00 | 0 | 0 | 0 |
| fastapi | python | todos | scored | 17 | 17 | 17 (+0) | 1.00 | 1.00 | 0 | 0 | 17 |
| aft | rust | dead_code | scored | 33 | 0 | 0 (+0) | 0.00 | n/a | 0 | 0 | 33 |
| aft | rust | todos | scored | 0 | 7 | 0 (+0) | n/a | 0.00 | 0 | 0 | 0 |
| aft | typescript | unused_exports | unknown | n/a | 162 | n/a (+n/a) | n/a | n/a | 0 | 0 | 0 |
| aft | typescript | dead_code | scored | 10 | 38 | 10 (+0) | 1.00 | 0.26 | 0 | 0 | 10 |
| aft | typescript | todos | scored | 0 | 0 | 0 (+0) | n/a | n/a | 0 | 0 | 0 |

#### Upgrade verification and acceptance boundary

| Cache regression test | Stored pre-campaign aggregate | Restored result |
|---|---|---|
| `inspect::cache::tests::pre_campaign_aggregate_reopen_pages_all_150_dead_code_findings` | 100 of 150 findings, version-34 hash | 150; pages 100/50/0 |
| `inspect::cache::tests::pre_campaign_aggregate_reopen_pages_all_150_unused_exports` | 100 of 150 findings, version-34 hash | 150; pages 100/50/0 |
| `inspect::cache::tests::pre_campaign_aggregate_reopen_rebuilds_go_initializer_liveness` | Incorrect dead `Register` in `var Boot = Register()` fixture | Count 0; clean-build rows restored |
