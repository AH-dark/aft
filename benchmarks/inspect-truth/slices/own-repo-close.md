# Own-repository liveness and TypeORM capture follow-up

## Comparison and provenance

This follow-up compares the upgrade-and-report candidate captures from
`ffe59a5d7d1f333deaca3f84b798459ea9b088df` with fresh Linux release captures from
`972d87b8157db1f5b23141ae47f7a2c85371873a`, including the test-file tally
refinement `009941f58ded3d1c2dcc1e1fa41ed0f3b9e92f76`. The latter includes the separately
queued exercise-syntax repair `342737adfcd8b179bbcd3eaebe7594508807fc8b`, applied as
`97db9691607eccf0553b1afe249594d495e2274c`; `benchmarks/src/exercise.ts` is **not
excluded**. All six external source pins remain exactly those in `corpus.json`.
The own repository's analyzed source is fixed to the candidate commit; no plugin
implementation changed.

For each of the seven repositories, `collect_aft_record` started a new AFT
process/storage generation and collected summaries and full paginated finding
lists from the candidate release binary, rather than constructing an after list
from prior findings. The six external normalized oracle snapshots
are the frozen same-pin snapshots from upgrade-and-report. AFT's Rust and plugin
oracles were rerun on the repaired source with Cargo/rustc 1.99.0 and Fallow 2.88.3
(`npm install --prefix target/truth-close/tools fallow@2.88.3`, verified signed GNU
x64 binary). Bun is 1.4.2; remote Python is 3.14.4; scoring Python is 3.9.6. No
tracked manifest or lockfile changed. External dependencies were installed with
the corpus's pinned commands; oracle tools and results live only in ignored paths.

The scorer is the existing `run.py` implementation. Runtime judgment descriptor
copies relocate origin directories only. Earlier entries and the original
TypeORM >100-row typings omission are preserved. `own-repo-close.json` adds a
reason for each of the original 33 Rust and 10 in-domain TypeScript dead-code rows.
It also identifies three test-only TS oracle rows, removed only from dead_code
on **both** comparison sides. That explains the own TS before cell changing from
38 oracle rows (10 agreements) to 35 oracle rows (7 agreements); it is not a product gain
claimed by changing only the after denominator. Unused-export oracles are not
trimmed by those judgments.

## Before and after, every score cell

`unknown` is an incomplete category, not an empty list. `n/a` is an undefined
ratio. The two precision and recall cells are shown per bucket; the other two
scorer cells (`recall_in_aft_domain`, `recall_exported`) are n/a on both sides in
all 20 buckets. A direct `cell_changes` comparison, **including AFT despite its
historical no-baseline designation**, checked all 80 cells: zero lowered cells.
Counts here are in-domain findings after the unchanged test/truncation filters,
not project-wide totals. AFT-side judgments annotate finding rows without
removing those rows from the counts.

| Repo | Language/category | AFT before -> after | Oracle | Precision before -> after | Recall before -> after | After judged / unjudged |
|---|---|---:|---:|---|---|---:|
| outline | TS unused_exports | 240 -> 240 | 274 | 0.992 -> 0.992 | 0.869 -> 0.869 | 8 / 232 |
| outline | TS dead_code | 73 -> 73 | 89 | 0.904 -> 0.904 | 0.742 -> 0.742 | 7 / 66 |
| outline | TS todos | 19 -> 19 | 20 | 1.000 -> 1.000 | 0.950 -> 0.950 | 0 / 19 |
| typeorm | TS unused_exports | unknown -> 3 | 4 | unknown -> 1.000 | unknown -> 0.750 | 3 / 0 |
| typeorm | TS dead_code | unknown -> 0 | 1 | unknown -> n/a | unknown -> 0.000 | 0 / 0 |
| typeorm | TS todos | 10 -> 10 | 10 | 1.000 -> 1.000 | 1.000 -> 1.000 | 0 / 10 |
| ripgrep | Rust dead_code | 4 -> 4 | 0 | 0.000 -> 0.000 | n/a -> n/a | 4 / 0 |
| ripgrep | Rust todos | 11 -> 11 | 11 | 1.000 -> 1.000 | 1.000 -> 1.000 | 0 / 11 |
| axum | Rust dead_code | 9 -> 5 | 0 | 0.000 -> 0.000 | n/a -> n/a | 5 / 0 |
| axum | Rust todos | 5 -> 5 | 5 | 1.000 -> 1.000 | 1.000 -> 1.000 | 0 / 5 |
| soft-serve | Go dead_code | 29 -> 29 | 0 | 0.000 -> 0.000 | n/a -> n/a | 29 / 0 |
| soft-serve | Go dead_code (functions) | 26 -> 26 | 20 | 0.346 -> 0.346 | 0.450 -> 0.450 | 26 / 0 |
| soft-serve | Go todos | 33 -> 33 | 33 | 1.000 -> 1.000 | 1.000 -> 1.000 | 0 / 33 |
| fastapi | Python dead_code | 0 -> 0 | 125 | n/a -> n/a | 0.000 -> 0.000 | 0 / 0 |
| fastapi | Python todos | 17 -> 17 | 17 | 1.000 -> 1.000 | 1.000 -> 1.000 | 0 / 17 |
| aft | Rust dead_code | 33 -> 4 | 0 | 0.000 -> 0.000 | n/a -> n/a | 4 / 0 |
| aft | Rust todos | 0 -> 0 | 7 | n/a -> n/a | 0.000 -> 0.000 | 0 / 0 |
| aft | TS unused_exports | unknown -> 104 | 162 | unknown -> 1.000 | unknown -> 0.642 | 0 / 104 |
| aft | TS dead_code | 10 -> 7 | 35 | 0.700 -> 1.000 | 0.200 -> 0.200 | 7 / 0 |
| aft | TS todos | 0 -> 0 | 0 | n/a -> n/a | n/a -> n/a | 0 / 0 |

TypeORM now reproduces the historical TS-liveness candidate's 3 unused exports
and zero in-domain dead-code rows, without weakening parse completeness. Its
project-wide D140/U190 include out-of-domain test and documentation exports;
those totals are not substituted for the scoring prefixes. Axum's project D6
includes the out-of-domain unix-domain-socket example, leaving five scored rows.
The removed four raw axum findings are cfg(test) declarations/trait dependencies;
no Rust oracle row was removed to improve precision.

## Generic liveness repairs and row decisions

- **cfg(test) declarations:** call origins were already classified by cfg ranges,
  but declarations inside an inline test module or a cfg-gated function were
  still product candidates. Declaration exclusion now uses those same ranges,
  including preceding attributes because parser symbol ranges begin there. Existing
  test-file processing still checks reachability before tallying withheld dead
  findings; a used export in a test file must not increase that tally.
- **Private trait dispatch:** the public-module-path rule severed a used private
  receiver's Default/Drop body. Receiver references outside impl headers now
  keep its trait method bodies and constructor/callback chains reachable. An
  impl header alone does not establish use; the private uncalled-trait and
  planted-dead controls still pass. This restores BackendSlot's reranker tree,
  the failure guard's Drop logging, and the single-flight constructors.
- **Static initialization:** private statics were absent from parser symbols, so
  their initializer calls had a `<top-level>` owner with no reachable incoming
  edge. Static/const initializers now have separate owner nodes, activated by
  value references (including receiver and index uses). This restores the
  SpawnEnvValue constructor and lazy served-tool catalog. The fixture includes
  an unread LazyLock whose callback must remain dead; not every static is a root.
- **TS cache-busting imports:** a template whose interpolations occur only after
  a fixed query/fragment delimiter now resolves the fixed source pathname.
  Interpolations that can change the pathname remain computed imports. The three
  updater test exports move to test-only usage, not to production liveness.

There are no product per-symbol allowlists. Dead-code facts advance 6 -> 7, Oxc
facts 5 -> 6, and callgraph blob/build/view producer keys advance to invalidate
old extraction. The aggregate version remains 35; changed producer identities
and fact formats invalidate the old inputs rather than inventing another
aggregate encoding change. The contribution golden tests explicitly pin the new
format and receiver/initializer fields; the producer-pair test pins the new key.
The tests' existing dead-code behavioral assertions remain in place.

29 original Rust rows are false positives; the remaining four are the explicitly
allowed logging wrapper and superseded orphan-sweep chain retained for fixtures.
They remain AFT findings and are individually explained; rustc's zero is not
proof of liveness when `allow(dead_code)` suppresses a lint. All original 43
requested rows have judgments, and all 11 remaining in-domain dead-code rows
are judged. Own U is now scoreable at 104/162; these newly available 104 export
rows have not received a separate manual review. Agreement with Fallow is not
relabeled as manual judgment.

## TypeORM exclusion policy

`corpus.json` declares exactly the historical four deliberately invalid ambient
codemod input/output fixtures. `collect_aft_record` applies the policy before
starting AFT by temporarily appending root-anchored rules to `.aftignore`; it
records the paths in each capture and restores the previous bytes, including on
collection errors. A symlinked ignore file is refused. This is analysis-domain
policy, not a scorer filter or tolerant-parser change. The original default
unknown captures remain preserved; no source fixture was edited. The real AFT
benchmark driver was repaired, not placed in this exclusion list.

## Four-vCPU TypeORM timing

Linux `7.0.0-34-generic`, x86_64, glibc 2.43, **four allocated vCPUs**; release
binary and the same installed TypeORM pin. Requests are
`{scope:["packages"], sections:["dead_code"], topK:100}`. Each configuration gets
one warm-up followed by five measured calls. Disabled uses the harness's server
list; enabled uses an empty user config. The four fixture exclusions apply to
both. Every timed response has scoped D135, no pending category, diagnostics
exactly `{status:"not_requested"}`, and no diagnostics details.

| Mode | Five seconds samples | Median |
|---|---|---:|
| Disabled | 5.505742 / 5.405043 / 5.405096 / 5.405199 / 5.706288 | 5.405199s |
| Enabled | 6.408237 / 6.308367 / 6.909799 / 7.431910 / 8.419120 | 6.909799s |

Enabled minus disabled is **1.504600s**, not the earlier 0.4-second gap, and the
enabled median still misses **<5s**. The initial four-vCPU run of b040edec4 also
missed (10.114079s enabled, 9.915733s disabled, gap 0.198346s); its captures remain
retained separately. The enabled samples in the final run rise noticeably, so
these results do not establish stable producer overhead. Both four-vCPU attempts
miss: the old miss cannot simply be dismissed as a two-vCPU allocation artifact.
The preceding REPORT.md timing
section recorded five calls per mode on two vCPUs; it named two as the measured
allocation, not as a two-vCPU target. This rerun uses four vCPUs. These
measurements are not proof that configured producers returned authoritative
reports. An exploratory request with sections omitted returned diagnostics and
was rejected before sampling; it is not counted in these medians. No unrelated
performance or default-diagnostics change is bundled here.

## Full-gate baseline comparison and verification

Plain `origin/main` is `1a87fec37ba54a0edcd6ac65bffdbd38851cff87`. After committing
the product changes, its exact source/index tree was temporarily installed in
this worktree (both diffs against origin/main empty), tested on Linux with four
vCPUs, and the task HEAD tree restored. This avoids the misleading portability
failure of an earlier source archive nested beneath a different Git index.

| Test | Plain origin/main | Candidate full gate | Interpretation |
|---|---|---|---|
| outline_portability_goldens_stay_lf_with_autocrlf | 1 passed | passed | Old ownership failure not reproduced; the baseline here has the checkout's actual Git index |
| blackholed_backend_never_blocks_status_or_search | failed | failed | Offline runner reports unavailable instead of the expected timeout |
| storage_debug_archive_ignores_public_member_modes | failed | failed | `zip` executable absent (NotFound at spawn) |
| candidate_bytes_are_released_before_derived_materialization | 1 passed | passed | Prior failure not reproduced |
| provenance_reports_build_facts_with_and_without_a_nonce | failed | failed | Remote build has no Git build SHA; additional baseline environment failure |

The full gate checked 721 files with Biome 2.4.7; TLS prerequisite 1 passed;
tokenizer 4 passed/1 ignored; main library 6188 passed/4 failed/56 ignored.
One of those four was the intentionally changed producer-pair golden, corrected
and rerun: 1 passed. The blackholed-backend, missing-zip, and missing-build-SHA
failures reproduce on plain origin/main. Full-gate
later phases were not reached; it is **not** reported as passing. No unrelated
failure was fixed. Narrow authoritative verification (Cargo 1.99.0):

- dead-code scanner lib suite: 59 passed;
- Oxc engine integration: 51 passed, 1 manual benchmark ignored;
- dead-code integration: 31 passed;
- calls lib suite: 16 passed;
- inspect cache lib suite: 21 passed;
- binary `aft`: 132 passed;
- Python harness: 39 tests passed (Python 3.14.4);
- `cargo fmt --all -- --check`: exit 0, rustfmt 1.10.0-stable;
- final release build: Finished; only the pre-existing cache_freshness VecDeque
  unused-import warning;
- `aft_inspect` diagnostics for dead_code.rs, Oxc facts.rs and calls.rs were
  partial while rust-analyzer's Cargo check was running; compiled tests and the
  release build are authoritative, not a claim of complete editor diagnostics.

Each regression test was observed failing with its corresponding repair absent.
Separate staged-state mutations
neutralized cfg-declaration exclusion, private trait roots, static-owner
reassignment, template import handling, the fixture exclusion policy, and the
used-test-file tally check. Each
named regression alone failed; sibling controls passed or were filtered out.
Every mutation has a non-empty applied diff and an empty diff after restoration.
The six controls and captured output are retained outside build directories:

| Neutralized behavior | Exact regression test | Evidence under `local-ignore/truth-close/logs/` |
|---|---|---|
| cfg declaration exclusion | `inspect::scanners::dead_code::tests::own_repo_cfg_test_declarations_are_not_product_dead_code` | `mutation-cfg.stdout` |
| private trait receiver root | `inspect::scanners::dead_code::tests::own_repo_private_default_impl_keeps_constructor_callback_chain_live` | `mutation-trait.stdout` |
| static initializer owner reassignment | `inspect::scanners::dead_code::tests::own_repo_static_receiver_and_index_uses_keep_initializers_live` | `mutation-static.stdout` |
| fixed-path template import | `inspect_oxc_engine_test::own_repo_query_suffix_template_imports_are_test_only_references` | `mutation-template.stdout` |
| fixture exclusion policy | `ScoreFixtures.test_typeorm_fixture_exclusions_are_applied_and_restored` | `mutation-exclusions.stderr` |
| used test-file tally | `inspect::scanners::dead_code::tests::used_test_file_exports_are_not_tallied_as_excluded_dead_findings` | `mutation-test-tally.stdout` |

The task checkout was restored after every deliberate test-breaking edit; none
of those edits remains in its source files.

## Retained evidence and acceptance boundary

Raw AFT findings, rerun rustc/Fallow reference findings for the own repository,
all ten final timing responses, path-relocated original judgment descriptors,
both scored tables, and all 80 cell comparisons support the counts and score
claims above. They are retained outside
build directories in `local-ignore/truth-close/`. Key SHA-256 values:

- `final2-metadata.json`: `7e9aecd011e5ae8089a92e9312e486e326e561201c2459bd844a74ebc0288cc8`
- `scored2/results.json`: `79d24004c240b79d6dc93bf3d3838bd6ea57e0640d16a002e0e700813335ed4f`
- `scored2/cell-diff.json`: `de898fa180036c56b37974cf5d55f9df6a24ad265ac9d97ee4a84eb81cfd0ff0`
- Linux `aft` release executable built from `972d87b8157db1f5b23141ae47f7a2c85371873a`
  (`/run/motor/target/release/aft` during collection):
  `0780e19b598552b85a8a11e61b4e8475ceaa483a183fb58b84dbf73c4470c1ba`

All seven source trees have unchanged empty tracked status before/after
collection. The own config was restored byte-for-byte. As in the previous
remote measurement, the runtime-only chmod workaround skips only a restoration
whose own-config full mode already matches; no chmod behavior was committed.
FastAPI retains the explicit 1166-file Python gap; AFT retains the 97-file gap.
Diagnostics sampling was zero, so the corpus diagnostics/footer pass remains
unrun. The broader campaign is **not passed**: timing still misses, soft-serve's
function precision remains 0.346, and the newly scoreable own unused-export rows
are not manually reviewed. The own Rust +10 bound, requested dead-code judgment
coverage, repaired-driver scoring, TypeORM completeness policy, and score-cell
monotonicity blockers are closed.
