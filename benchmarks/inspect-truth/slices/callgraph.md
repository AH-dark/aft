# callgraph: inspect-truth cell gate

Baseline: 0.58.2 (`13dd9cb71`). Both tables use this revision of `run.py score`.

**Current delivery boundary:** the tables remain measured at pre-rebuild tip `9e8c83db7`. The later regression that falsely kept a plain `Less` function live through a same-named Go interface method is corrected at `e9fd4a6f27d168bf1cd7ba5b3ce984957ddae833` on the rebuilt branch; its Go corpus effect is not yet measured and must be re-collected before these numbers are attributed to the new tip.

## Before

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep | rust | dead_code | scored | 52 | 0 | 0 (+0) | 0.00 | n/a | 0 | 31 | 21 |
| ripgrep | rust | todos | scored | 11 | 11 | 11 (+0) | 1.00 | 1.00 | 0 | 0 | 11 |
| axum | rust | dead_code | scored | 55 | 0 | 0 (+0) | 0.00 | n/a | 0 | 6 | 49 |
| axum | rust | todos | scored | 5 | 5 | 5 (+0) | 1.00 | 1.00 | 0 | 0 | 5 |
| soft-serve | go | dead_code | scored | 101 | 0 | 0 (+0) | 0.00 | n/a | 0 | 29 | 72 |
| soft-serve | go | dead_code (functions) | scored | 98 | 20 | 5 (+0) | 0.05 | 0.25 | 0 | 26 | 72 |
| soft-serve | go | todos | scored | 33 | 33 | 33 (+0) | 1.00 | 1.00 | 0 | 0 | 33 |

- ripgrep raw output: `local-ignore/callgraph/corpus/_results/ripgrep/13dd9cb71` (corpus `3fce3b5bb0236da2df6d99672afb8a719642eca7`).
- ripgrep baseline_aft_truncated_scopes (excluded from comparison): []
- ripgrep judgments_unapplied: {"harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 0, "inspect-tool.json": 0}

- axum raw output: `local-ignore/callgraph/corpus/_results/axum/13dd9cb71` (corpus `f8b02f22cf10bee707bda19b58265b9e33677535`).
- axum baseline_aft_truncated_scopes (excluded from comparison): []
- axum judgments_unapplied: {"harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 4, "inspect-tool.json": 0}

- soft-serve raw output: `local-ignore/callgraph/corpus/_results/soft-serve/13dd9cb71` (corpus `37685d36f5b7bf0e32217ddd7c8e045c57772619`).
- soft-serve baseline_aft_truncated_scopes (excluded from comparison): []
- soft-serve judgments_unapplied: {"harness.json": 0, "callgraph.json": 5, "ts-liveness.json": 0, "dead-code-scanner.json": 0, "inspect-tool.json": 0}

## After

| repo | language | category | status | AFT | oracle | agreed (+file) | precision | recall | oracle_rows_judged_out | aft_rows_judged | aft_rows_unjudged |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| ripgrep | rust | dead_code | scored | 4 | 0 | 0 (+0) | 0.00 | n/a | 0 | 4 | 0 |
| ripgrep | rust | todos | scored | 11 | 11 | 11 (+0) | 1.00 | 1.00 | 0 | 0 | 11 |
| axum | rust | dead_code | scored | 9 | 0 | 0 (+0) | 0.00 | n/a | 0 | 9 | 0 |
| axum | rust | todos | scored | 5 | 5 | 5 (+0) | 1.00 | 1.00 | 0 | 0 | 5 |
| soft-serve | go | dead_code | scored | 29 | 0 | 0 (+0) | 0.00 | n/a | 0 | 29 | 0 |
| soft-serve | go | dead_code (functions) | scored | 26 | 20 | 9 (+0) | 0.35 | 0.45 | 0 | 26 | 0 |
| soft-serve | go | todos | scored | 33 | 33 | 33 (+0) | 1.00 | 1.00 | 0 | 0 | 33 |

- ripgrep raw output: `local-ignore/callgraph/corpus/_results/ripgrep/9e8c83db700b8dc26ba624e7d75d74518c4fe1ca` (corpus `3fce3b5bb0236da2df6d99672afb8a719642eca7`).
- ripgrep baseline_aft_truncated_scopes (excluded from comparison): []
- ripgrep judgments_unapplied: {"harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 27, "inspect-tool.json": 0}
- ripgrep/rust/dead_code languages_unsupported: [{"language": "ruby", "files": 1}]

- axum raw output: `local-ignore/callgraph/corpus/_results/axum/9e8c83db700b8dc26ba624e7d75d74518c4fe1ca` (corpus `f8b02f22cf10bee707bda19b58265b9e33677535`).
- axum baseline_aft_truncated_scopes (excluded from comparison): []
- axum judgments_unapplied: {"harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 1, "inspect-tool.json": 0}

- soft-serve raw output: `local-ignore/callgraph/corpus/_results/soft-serve/9e8c83db700b8dc26ba624e7d75d74518c4fe1ca` (corpus `37685d36f5b7bf0e32217ddd7c8e045c57772619`).
- soft-serve baseline_aft_truncated_scopes (excluded from comparison): []
- soft-serve judgments_unapplied: {"harness.json": 0, "callgraph.json": 3, "ts-liveness.json": 0, "dead-code-scanner.json": 2, "inspect-tool.json": 0}

## Cell rule

- PASS: no cell lowered.

## Latest corpus measurement and rebuild on main

The automatic tables above are **measured at `9e8c83db700b8dc26ba624e7d75d74518c4fe1ca`**, the pre-main-rebuild tip with the custom-crate-root repair. They were collected from a fresh Linux release build and re-scored locally from the retained captures. The branch rebuilt on current main has new commit identities and has **not** been re-collected; the measured success below is explicitly attached to the original tip, not silently attributed to the new branch.

The commit columns identify the original extractor/initializer implementation (`a37cdbe83`), the nested-alias/macro/init/callback follow-up (`213581cc8`), and the manifest-declared crate-root repair (`9e8c83db7`). An `unapplied` judgment is an older annotated row absent from the output currently being scored; each JSON-file count measures such rows, not invalid judgments. Those entries remain valid against their own original capture directories.

| Bucket | Baseline 13dd9cb71 | a37cdbe83 | 213581cc8 | Measured 9e8c83db7 | Verdict at measured tip |
|---|---:|---:|---:|---:|---|
| ripgrep dead_code | 52 | 21 | 16 | **4**; 4 judged, 0 unjudged | <=10 **PASS** |
| axum dead_code | 55 | 10 | 9 | **9**; 9 judged, 0 unjudged | <=10 **PASS** |
| soft-serve dead_code | 101 | 32 | 29 | **29**; 29 judged, 0 unjudged | Improved; no separate count threshold |
| soft-serve function rows | 98 | 29 | 26 | **26**; 26 judged, 0 unjudged | Fully explained |
| soft-serve function precision / recall | 0.05 / 0.25 | 0.31 / 0.45 | 0.35 / 0.45 | **0.35 / 0.45** | Precision <0.6 still missed and explained |
| Todos: ripgrep / axum / soft-serve | 11 / 5 / 33 | 11 / 5 / 33 | 11 / 5 / 33 | **11 / 5 / 33** | Unchanged |
| Cell rule | baseline | PASS | PASS | **PASS, no lowered precision/recall cell** | PASS |

Ripgrep's **21 -> 16 -> 4** sequence is now measured, not predicted. The custom-root repair removed the ten generators reached through nested Rust aliases and their two ordinary callees (`generate_digits` and `lookup`) that were still reported in the 213581cc8 run. The four remaining findings are `flags/lowargs.rs::{new,disabled}` and `printer/src/util.rs::{from_sink_match,from_sink_context}`: exactly the deferred trait/caller-contract cases. No trait/interface liveness was changed to meet the count target. Existing judgments remain attached to their original after-output directories; rows absent from newer captures are unapplied, not deleted.

Raw captures for this measurement are under `local-ignore/callgraph/corpus/_results/<repo>/9e8c83db700b8dc26ba624e7d75d74518c4fe1ca/`. Score JSON/Markdown are under `local-ignore/callgraph/score-s4b/`. The local re-score validated three baseline and three candidate repositories and every judgment file referring to these repositories against its original capture directory. Every candidate dead-code row, including the Go function-only bucket, has a recorded judgment; no such after row is unjudged. No new collection was run during the main rebuild.

### New branch and conflict resolution

New branch rebuilt from the original callgraph task branch onto main: **`alfonso/task/bg_dispatch_e643c2cb4cfca3665f1f47a0-callgraph-main-rebuilt`**.

The original task branch was preserved. Its nine commits were replayed onto `origin/main` at `f2b0fdccda4e152eaa4ae1dcd7813b3eb80afc8d`. The only cherry-pick conflict was in `callgraph_store/dead_code_projection.rs`: main requires calls to be grouped in stored caller-file byte order so incremental snapshot replacement agrees with a full rebuild; this slice appends Go package-initialization edges to those calls. Sorting before appending the new edges would break the grouping. The merged sequence is:

1. produce ordinary resolved/value/macro calls;
2. append package-initialization edges;
3. apply main's final stable caller-path sort using the stored byte-key order;
4. return the complete projection.

The merge preserves the same raw path-byte ordering for full and incrementally replaced snapshots, including names such as `a.b.rs` versus `a/child.rs`, whose component-wise Path ordering differs. Main's existing dotted/nested path test remains intact rather than being replaced by the older task file. The Go imported/unimported legacy/view fixture now also contains an ordinary root-file call and asserts caller-path grouping on both surfaces, so synthetic edges cannot undo main's sorting fix.

While verification ran, main advanced with an unrelated sandbox change that tolerates a listed read-permission target disappearing before it is opened. The rebuilt branch was then rebased onto **`236b14b97a58fd793413b22894905eb4eca24547`**. A direct Git diff confirmed that all files mutated by the twelve controls were byte-for-byte unchanged by that sandbox-only advance; final fixtures and the repository gate were run on the resulting branch. No sandbox or publication-manifest changes were authored by this slice.

### Rebuilt verification and twelve break controls

All Cargo jobs were serialized on the remote Linux server with four virtual CPUs (`linux,4c`), Cargo 1.99.0. Final rebuilt checks:

- resolver and legacy/view parity module: **27 passed**;
- audit projection module, including main's dotted/nested splice and sorting tests: **10 passed**;
- immutable manifest integration module: **7 passed**;
- navigation integration: **58 passed**;
- binary/watcher contracts: **132 passed**;
- offline Python collection/scoring fixtures: **30 passed**, Python 3.14.4;
- mandatory `bash scripts/rust-test-gate.sh`: **5953 library tests passed, 2 failed, 50 ignored**; TLS prerequisite **1 passed**, tokenizer **3 passed**, Biome **714 files checked**. The two failures remain `blackholed_backend_never_blocks_status_or_search` (its assertion rejects the network-unreachable backend response) and `storage_debug_archive_ignores_public_member_modes` (fixture archive path not found, OS error 2). Later full-gate phases were not reached. The gate is failed, not reported green.

The final gate log is retained outside build directories at `local-ignore/callgraph/evidence/rust-gate-main-rebuilt.stdout`. Rustfmt 1.10.0-stable format/check and diff checks pass. The repository's inspection tool could not confirm a fresh Rust diagnostic snapshot because it could not fingerprint the changed input set; its editor diagnostics were provisional. Direct Rust compilation and test commands checked the actual rebuilt source instead.

All **twelve documented successful break controls** were independently re-applied on the fresh-main rebuilt branch. Each named test alone failed; all other tests were filtered. Live files were staged first, nonempty mutation diffs captured, then restored with `git checkout -- <path>` plus `touch`; every post-restore unstaged diff was empty.

| # | Neutralized behavior | Exact red fixture (rust_resolution_tests) |
|---:|---|---|
| 1 | Go package-var extraction | `callgraph_go_cobra_imported_initializer_live_unimported_dead` |
| 2 | Rust use-as edge | `callgraph_rust_inline_use_alias_keeps_bin_function_live` |
| 3 | Macro-body edges | `callgraph_rust_macro_body_live_only_with_invocation` |
| 4 | go.mod aggregate hash input | `callgraph_go_mod_edit_re_resolves_and_invalidates_warmed_aggregate` |
| 5 | Go value-reference extraction | `callgraph_go_main_boot_register_and_function_values` |
| 6 | Go module-map invalidation | `callgraph_go_mod_refresh_drops_warmed_module_map` |
| 7 | Go package-initializer graph edges | `callgraph_go_cobra_imported_initializer_live_unimported_dead` |
| 8 | Stored Go import resolution | `callgraph_go_cobra_imported_initializer_live_unimported_dead` |
| 9 | View macro serialization | `callgraph_view_macro_and_go_initializer_equal_cold_legacy` |
| 10 | Serialized package-main roots | `callgraph_view_macro_and_go_initializer_equal_cold_legacy` |
| 11 | Go-only materialization root flag | `callgraph_go_cobra_imported_initializer_live_unimported_dead` |
| 12 | Preserve non-Go root behavior | `callgraph_non_go_exported_variable_retains_view_root_flag` |

An additional merge-specific control removes main's final projection sort. Its first probe was undefended because an empty main function produced no ordinary root-file call; the fixture was strengthened with `main -> control()` so ordinary and synthetic rows genuinely interleave before sorting. The same control then failed only the Cobra parity fixture on “package initialization edges must preserve main's final caller-path grouping.” This is recorded separately from the twelve requested controls. Sorting was restored, and the final resolver/parity module passed.

No push was performed. The measured pre-rebuild corpus target is satisfied for ripgrep/axum; the rebuilt branch preserves both changes under fixture and mutation verification but still requires its own collection before claiming those counts for the new commit identities.


## Preserved historical verification record

The following sections document earlier revisions and why each subsequent fix was needed. Their pending-collection statements and target misses are historical; the measured 9e8c83db7 result and main-rebuild boundary above supersede them. Original judgments and capture origins are unchanged.

## Historical measured revision 213581cc8 and residual diagnosis

The automatic tables above are **measured at `213581cc831fcfdd33360168d1e31d47ca034b55`**, using the retained Linux release captures and the same baseline/judgments. They do not measure the newer custom-crate-root repair at engine commit `ba586aa46f9698684b079ef30a03c3cc8f4a8b96`. Its fixtures and mutation control pass, but its corpus effect awaits the next Linux collection. Existing judgment entries and their original capture directories remain unchanged.

| Bucket | Baseline 13dd9cb71 | Previous a37cdbe83 | Measured 213581cc8 | Target verdict at 213581cc8 |
|---|---:|---:|---:|---|
| ripgrep dead_code | 52 | 21 | 16; 16 judged, 0 unjudged | <=10 **still missed** |
| axum dead_code | 55 | 10 | 9; 9 judged, 0 unjudged | <=10 **PASS** |
| soft-serve dead_code | 101 | 32 | 29; 29 judged, 0 unjudged | Improved; no separate count threshold |
| soft-serve dead_code (functions) | 98 | 29 | 26; 26 judged, 0 unjudged | Precision 0.35 <0.6 **still missed** |
| soft-serve function precision / recall | 0.05 / 0.25 | 0.31 / 0.45 | 0.35 / 0.45 | Precision improves; recall unchanged |
| Todos: ripgrep / axum / soft-serve | 11 / 5 / 33 | 11 / 5 / 33 | 11 / 5 / 33 | Unchanged |
| Cell rule | baseline | PASS | **PASS, 0 lowered cells** | No metric cell lowered |

A direct set comparison of the raw normalized ripgrep findings confirms **21 previous rows, 16 current rows, and no new rows**. The five removals are `messages::{messages,ignore_messages,set_errored}` and `lines::{without_terminator,preceding}`. The cross-file macro repair therefore had the expected corpus effect. My earlier expectation that nested-use flattening would also clear the twelve generator/transitive rows was **wrong for ripgrep's actual layout**: all twelve remained. The previous alias fixture used `src/main.rs`; the corpus declares `[[bin]] path = "crates/core/main.rs"` (`Cargo.toml:30-33`). That missing fixture dimension left an ordinary Rust alias resolver gap; it does not depend on interface or trait-contract dispatch.

### Every remaining ripgrep row, classified at 213581cc8

Classes: **(a)** expected to clear through nested use-tree alias resolution (including its transitive callees) and still present, **(b)** interface/trait-contract reachability outside the authorized alias/macro repair scope, **(c)** a finding absent from the previous capture. All 16 below retain their existing false-positive judgments. There are **12 (a), 4 (b), 0 (c)**. The two other previously deferred searcher helper rows disappeared in this measurement and are not silently counted among the remaining sixteen.

All source paths below are relative to pinned ripgrep `3fce3b5bb0236da2df6d99672afb8a719642eca7` at `local-ignore/callgraph/corpus/ripgrep/`.

| # | Remaining row | Class | Exact construct and evidence |
|---:|---|---|---|
| 1 | `crates/core/flags/complete/bash.rs::generate` | (a) | `crate::flags::{complete::{bash::generate as generate_complete_bash}}` in `flags/mod.rs:18-20`; `flags::generate_complete_bash()` at `core/main.rs:366`, under the custom bin root. |
| 2 | `crates/core/flags/complete/fish.rs::generate` | (a) | Same nested complete group, `fish::generate as generate_complete_fish` at `flags/mod.rs:21`; call at `core/main.rs:368`. |
| 3 | `crates/core/flags/complete/powershell.rs::generate` | (a) | Same group, `powershell::generate as generate_complete_powershell` at `flags/mod.rs:22`; call at `core/main.rs:370`. |
| 4 | `crates/core/flags/complete/zsh.rs::generate` | (a) | Same group, `zsh::generate as generate_complete_zsh` at `flags/mod.rs:23`; call at `core/main.rs:367`. |
| 5 | `crates/core/flags/doc/help.rs::generate_short` | (a) | `doc::{help::{generate_short as generate_help_short}}` at `flags/mod.rs:25-28`; call at `core/main.rs:394`. |
| 6 | `crates/core/flags/doc/help.rs::generate_long` | (a) | Same doubly nested help group, `generate_long as generate_help_long` at `flags/mod.rs:27`; call at `core/main.rs:395`. |
| 7 | `crates/core/flags/doc/man.rs::generate` | (a) | `doc::{man::generate as generate_man_page}` at `flags/mod.rs:30`; call at `core/main.rs:365`. |
| 8 | `crates/core/flags/doc/version.rs::generate_digits` | (a), transitive | Called by `generate_short` at `version.rs:23`; that generator is stranded behind the same custom-root nested alias. This is not a new row or a trait contract. |
| 9 | `crates/core/flags/doc/version.rs::generate_short` | (a) | `doc::{version::{generate_short as generate_version_short}}` at `flags/mod.rs:31-34`; call at `core/main.rs:396`. |
| 10 | `crates/core/flags/doc/version.rs::generate_long` | (a) | Same version group, `generate_long as generate_version_long` at `flags/mod.rs:32`; call at `core/main.rs:397`. |
| 11 | `crates/core/flags/doc/version.rs::generate_pcre2` | (a) | Same group, `generate_pcre2 as generate_version_pcre2` at `flags/mod.rs:33`; calls at `core/main.rs:401` and `version.rs:44`. |
| 12 | `crates/core/flags/lowargs.rs::new` | (b) | `ContextSeparator::new` called through local type alias `Separator` in live `Flag::update` implementations at `flags/defs.rs:1257,1892,2000`; deferred trait/caller reachability, unchanged. |
| 13 | `crates/core/flags/lowargs.rs::disabled` | (b) | `ContextSeparator::disabled` called through `Separator` in `Flag::update` at `flags/defs.rs:1256`; deferred trait/caller reachability, unchanged. |
| 14 | `crates/core/flags/parse.rs::lookup` | (a), transitive | Direct ordinary calls from the stranded help/man generators at `doc/help.rs:170,179` and `doc/man.rs:73,85`; not an interface/trait row. |
| 15 | `crates/printer/src/util.rs::from_sink_match` | (b) | Called by the generic standard printer sink at `standard.rs:905`; the `Sink for StandardSink` contract/caller chain remains the interface-liveness follow-up. |
| 16 | `crates/printer/src/util.rs::from_sink_context` | (b) | Called by the same generic sink at `standard.rs:920`; same deferred contract/caller chain. |

### Residual diagnosis, exact reduced fixture and repair

The defect is in **Rust module candidate construction after leaf flattening**. Qualified calls find the `flags/mod.rs` facade through declared modules, but its flattened `crate::flags::...` re-export refs still constructed candidates from guessed `src/` paths. A reduced pre-fix trace reported `status = resolved` yet `target_file = crates/core/flags/mod.rs` and `target_symbol = generate_complete_bash`: a facade alias with no defining leaf node. Merely asserting “resolved” would have passed for the wrong reason.

`callgraph_rust_nested_aliases_custom_bin_root_match_legacy_and_view` now uses the exact manifest target `crates/core/main.rs`, the `flags/mod.rs`, `complete/mod.rs` and `doc/mod.rs` hierarchy, and the nested groups/aliases from the corpus. It includes **all ten direct aliases plus `generate_digits` and `lookup`**. For every direct alias it asserts the real stored target file/symbol and direct caller, then repeats the target-edge and liveness checks against an actual view; both transitive helpers become live and a spare remains dead. The fixture was red before repair, including the resolved-but-facade target assertion.

The resolver now derives the caller's logical module path and source prefix from the memoized manifest-declared crate target root, including custom bin/lib layouts. It falls back to conventional `src/` only when no target is known. The same resolver helper feeds stored refs, dependencies and view indexes. It does not root generators, guess interface calls, or alter trait liveness. Replacing the manifest-root lookup with the old fallback makes only the named fixture fail, with the wrong facade target reproduced; restoration leaves an empty unstaged diff and the full resolver fixture module passes.

**Yes, <=10 is reachable without interface liveness for this observed set.** Twelve of the sixteen rows are the demonstrated custom-root alias/transitive defect, leaving four deferred contract rows if those twelve disappear. This is a structural feasibility conclusion, **not a measured new count of four**. The current <=10 claim remains unproven until the new engine tip is collected. No out-of-scope trait/interface repair was made.

### Version and verification handoff

Engine repair commit: `ba586aa46f9698684b079ef30a03c3cc8f4a8b96`. Build-output is `v16-manifest-root-reexport-resolution`; blob producers are `callgraph-v5-manifest-root-reexport-resolution` and active-view `ruled-callgraph-v6-manifest-root-reexport-resolution`. These invalidate stale resolver interpretation and derived graphs. The rebuild is still once per file content/language/current producer, followed by reuse; the older version descriptions retained below are historical.

Cargo 1.99.0, serialized `linux,4c` gates: resolver module **27 passed**, manifest module **7 passed**, callgraph integration **58 passed**, binary **122 passed**. Mandatory full gate: TLS **1 passed**, tokenizer **3 passed**, Biome **711 files**, library **5711 passed, 4 failed, 50 ignored**. Failures were the semantic-backend status/wording fixture, storage archive ENOENT, deferred producer SendError, and `warm_verify_uses_bounded_workers_and_one_stat_per_indexed_file` (“must use multiple workers”); the latter passed its focused one-test rerun. No unrelated fix was included, and later full-gate phases were not reached. Log: `local-ignore/callgraph/evidence/rust-gate-custom-root-repair.stdout`.

Rustfmt 1.10.0-stable format/check and diff checks passed. AFT's completed Rust diagnostic snapshot covered `callgraph_store/mod.rs`, `blob_store/mod.rs` and `views/callgraph.rs` with zero errors/warnings. New source comments were reviewed without flags. Local scoring of the retained 213581cc8 captures validated three before/after repositories with **no decrease in any scored precision/recall cell** relative to baseline. That score does not measure the current custom-root repair, whose corpus collection awaits the next review run. All old judgments remain attached to their original captures; no collection was rerun here.


## Preserved historical implementation and verification record

The sections below describe earlier measured/implemented revisions. The latest 213581cc8 table and custom-root diagnosis above supersede their pending-rerun statements. They remain here to preserve original judgments, fixture provenance and earlier controls.

## Historical measured revision a37cdbe83 and its row review

Baseline `13dd9cb71` and engine candidate `a37cdbe83ec281fd7a13fe76b83d5b1eae96deae` were collected on Linux and retained in this worktree alongside their oracles. The tables above are a **local re-score of those captures**, not a new collection. Subsequent row-review commits changed only the collection harness, tests, provenance descriptors, judgments and this report. A later scoped engine repair is documented below; its results must not be inferred from these older tables.

| Acceptance | Before | After | Verdict |
|---|---:|---:|---|
| No lowered precision/recall cell | baseline | 0 lowered cells | **PASS** |
| ripgrep dead-code count <= 10 | 52 | 21; all 21 judged, 0 unjudged | **Numeric target missed; all remaining rows explained below** |
| axum dead-code count <= 10 | 55 | 10; all 10 judged, 0 unjudged | **PASS** |
| soft-serve ordinary dead-code count | 101 | 32; all 32 judged, 0 unjudged | Improvement; no separate count threshold |
| soft-serve function precision >= 0.6 | 0.05 (5/98) | 0.31 (9/29); all 29 judged, 0 unjudged | **Numeric target missed; every row judged and mechanisms explained** |
| soft-serve function recall | 0.25 | 0.45 | Improved |
| soft-serve NewServer not test-only | previously stranded initializer caller | absent from ordinary and test-only findings | **PASS** |

The soft-serve capture has exactly 32 ordinary rows and three test-only rows. A direct check of the normalized ordinary rows and the complete three-row `test_only_top` confirms that `NewServer` occurs in neither. Judgment annotations do **not** remove AFT rows or raise the scorer's precision: the missed 0.6 target remains missed, even after all rows are explained. Ripgrep likewise remains above ten. Explanation is the reviewed spec's fallback, not a claim that either numeric target passed.

`judgments/callgraph.json` now has the actual candidate output directories and **32 source-reviewed Go entries: 19 aft_correct and 13 aft_false_positive**. They cover all 29 function/method rows plus three non-function rows. The scorer's `judgment_category()` maps `dead_code (functions)` to the canonical `dead_code` category, so one entry per `(path, symbol)` covers both Go buckets; no duplicate function-bucket entries are needed. Ripgrep's 21 existing judgments remain unchanged in `judgments/dead-code-scanner.json`.

### Retained inputs and reproducible scoring

| Repository | Corpus pin |
|---|---|
| ripgrep | `3fce3b5bb0236da2df6d99672afb8a719642eca7` |
| axum | `f8b02f22cf10bee707bda19b58265b9e33677535` |
| soft-serve | `37685d36f5b7bf0e32217ddd7c8e045c57772619` |

Sources are under `local-ignore/callgraph/corpus/<repo>/`. Raw baseline, candidate and oracle records are retained under `local-ignore/callgraph/corpus/_results/<repo>/<revision>/`; local score JSON/Markdown are under `local-ignore/callgraph/score/`. These are outside regenerable build directories. The prior scanner origins, deleted with their old `target/` worktree artifacts, were recollected at the **same** revisions (`7306b6953ace4ad20e8b3108486e6b0d831d8af5` for Rust, `715f01b05062908be3f9cd147f0e4399c40274ec` for Go). `dead-code-scanner.json` now points directly to these three retained origins without changing any existing judgment or revision, rather than relying on a temporary `target/inspect-truth-corpus` symlink.

```sh
python3 benchmarks/inspect-truth/run.py score \
  --repo ripgrep --repo axum --repo soft-serve \
  --corpus-root local-ignore/callgraph/corpus \
  --aft-commit a37cdbe83ec281fd7a13fe76b83d5b1eae96deae \
  --output-dir local-ignore/callgraph/score --slice callgraph
```

This command validates every applicable judgments file against its own baseline/after origin, scored three before and three after repositories, and reported **0 lowered cells**. It regenerates the automatic tables above; the explanatory sections below are authored additions. No collection was rerun during this follow-up. Diagnostics sampling was disabled in collection; no error/warning diagnostic or release-timing claim follows from these results. Local AFT and the other corpus languages are not included in this three-repository run. Ripgrep carries a scoreable `language_unsupported` gap for Ruby (one file).

## Soft-serve row decisions

All paths in this section refer to the pinned soft-serve source root above. The JSON contains an individual one-line source note for every finding, including the three non-functions: `pkg/db/models/settings.go::Settings`, `pkg/git/service.go::ServiceHandler`, and `pkg/ui/pages/repo/repo.go::CopyURLMsg`. All three are declared but neither constructed nor referenced elsewhere.

The **16 correct function/method findings** are `IsBranch`, `HashPassword`, `ListPublicKeys`, `SetPassword`, `Transaction`, the three standalone service wrappers (`UploadPack`, `UploadArchive`, `ReceivePack`), `jobs.Register`, `ReadPointer`, `GeneratePointer`, `Common.Repo`, and `Swap`/`Less` on each unused slice type `FileItems` and `StashItems`. Those slice types have no construction/sort uses; satisfying `sort.Interface` does not make an unused receiver live. The pointer scanner/tests use `ReadPointerFromBuffer`, not the unused reader wrapper. `HashPassword` does have unit-test callers (`pkg/backend/auth_test.go:6,16`) and a production caller inside unused `SetPassword` (`pkg/backend/user.go:384`); it is production-transitively dead, not a claim that nobody calls it. The reviewed rule says a production caller prevents test-only classification.

The **13 false-positive function/method findings** are grouped below. These classifications rely on actual registration or live receiver contracts, not merely capitalized/exported names.

| Mechanism | Rows | Source evidence | Fixable within this slice's original engine fence? |
|---|---|---|---|
| External LFS interface dispatch | `pkg/git/lfs.go::{LockBackend, FromID, FromPath, AsArguments, AsLockSpec}` (5) | Processor construction/execution at `:85-95`; live lock backend returned at `:234-241`; compile-time `transfer.LockBackend` assertion at `:231` and `transfer.Lock` assertion at `:417`; method declarations at `:234,275,305,420,430` | **Not as a simple missing call/value edge.** Recognizing these external contracts belongs to Go interface liveness in `inspect/scanners/dead_code.rs` (or shared typed dispatch), outside the original file fence. |
| External Bubble Tea delegate dispatch | `Spacing` in `pkg/ui/pages/repo/{filesitem,logitem,refsitem,stashitem}.go` and `pkg/ui/pages/selection/item.go` (5) | Actual delegate instances passed to `selector.New` at `files.go:89`, `log.go:69`, `refs.go:45`, `stash.go:49`, `selection.go:70` | **Outside the original fence's interface-liveness owner.** These live `list.ItemDelegate` implementations need their imported interface contract recognized; blanket preservation of every method would incorrectly keep the unused sorting receivers above. |
| Standard HTTP unwrapper contract | `pkg/web/logging.go::Unwrap` (1) | `:43` supplies the `http.ResponseController` unwrapper contract; the middleware constructs the receiver at `:74` and passes it to the handler at `:79` | **Interface-liveness follow-up outside the original fence.** This is an implicit standard HTTP adapter contract, not a direct repository call to `Unwrap`. |
| HTTP callback reachability | `pkg/web/goget.go::GoGetHandler` (1) | `pkg/web/git.go:116` registers `http.HandlerFunc(GoGetHandler)` under wrappers; `pkg/web/server.go:22` calls its `GitController` | **Yes, a candidate for fenced Go value-reference/caller resolution work.** The simple HandlerFunc fixture passes, but this real nested registration is still stranded. The exact missing graph edge was not runtime-traced in this report-only pass, so the registration is evidence of a gap, not a diagnosis of a particular line to patch. |
| Go package init-function reachability | `pkg/webhook/event.go::Events` (1) | The imported SSH command package calls it from `pkg/ssh/cmd/webhooks.go:37-42` to initialize CLI event names | **Yes.** `go_initializer_edges` currently selects variables and `<go-package-init>` intermediates, not `init` functions (`dead_code_projection.rs:942`). A paired imported/unimported init-function fixture is the appropriate follow-up; rooting all packages unconditionally would be wrong. |

The two latter rows are residual callgraph-layer gaps related to this slice's callback/initialization work and should be investigated inside the fence. The eleven interface-contract rows need the scanner/dispatch owner rather than more name-based call guesses. **No engine change was authorized or made in this row-review pass.**

## Ripgrep: all 21 remaining rows

All paths below refer to pinned ripgrep. Every row is already `aft_false_positive` in `judgments/dead-code-scanner.json`; none is newly reclassified or deleted. Declaration line numbers can differ from AFT's comment-inclusive symbol-range line.

| # | Finding | Row-by-row production-use explanation | Mechanism / slice relationship |
|---:|---|---|---|
| 1 | `crates/core/flags/complete/bash.rs::generate` | Re-exported as `generate_complete_bash` at `flags/mod.rs:20`; invoked by `core/main.rs:366`. | Nested use-tree alias; fenced alias gap that this slice should close. |
| 2 | `crates/core/flags/complete/fish.rs::generate` | Re-exported as `generate_complete_fish` at `flags/mod.rs:21`; invoked by `core/main.rs:368`. | Same nested alias gap. |
| 3 | `crates/core/flags/complete/powershell.rs::generate` | Re-exported as `generate_complete_powershell` at `flags/mod.rs:22`; invoked by `core/main.rs:370`. | Same nested alias gap. |
| 4 | `crates/core/flags/complete/zsh.rs::generate` | Re-exported as `generate_complete_zsh` at `flags/mod.rs:23`; invoked by `core/main.rs:367`. | Same nested alias gap. |
| 5 | `crates/core/flags/doc/help.rs::generate_short` | Re-exported as `generate_help_short` at `flags/mod.rs:28`; invoked by `core/main.rs:394`. | Same nested alias gap. |
| 6 | `crates/core/flags/doc/help.rs::generate_long` | Re-exported as `generate_help_long` at `flags/mod.rs:27`; invoked by `core/main.rs:395`. | Same nested alias gap. |
| 7 | `crates/core/flags/doc/man.rs::generate` | Re-exported as `generate_man_page` at `flags/mod.rs:30`; invoked by `core/main.rs:365`. | Same nested alias gap. |
| 8 | `crates/core/flags/doc/version.rs::generate_digits` | Called by `generate_short` at `version.rs:23`; that caller is reached through `generate_version_short`. | Transitive consequence of the nested alias gap. |
| 9 | `crates/core/flags/doc/version.rs::generate_short` | Re-exported as `generate_version_short` at `flags/mod.rs:34`; invoked by `core/main.rs:396`. | Nested alias gap. |
| 10 | `crates/core/flags/doc/version.rs::generate_long` | Re-exported as `generate_version_long` at `flags/mod.rs:32`; invoked by `core/main.rs:397`. | Nested alias gap. |
| 11 | `crates/core/flags/doc/version.rs::generate_pcre2` | Re-exported as `generate_version_pcre2` at `flags/mod.rs:33`; invoked at `core/main.rs:401` and `version.rs:44`. | Nested alias gap. |
| 12 | `crates/core/flags/lowargs.rs::new` | `ContextSeparator::new` is invoked through local alias `Separator` in `Flag::update` implementations at `flags/defs.rs:1257,1892,2000`. | Trait/caller and local-type alias reachability; candidate callgraph follow-up, not a missing generator root. |
| 13 | `crates/core/flags/lowargs.rs::disabled` | `ContextSeparator::disabled` is invoked through `Separator` in flag update code at `flags/defs.rs:1256`. | Same trait/caller and local-type alias issue. |
| 14 | `crates/core/flags/parse.rs::lookup` | Called by help rendering at `doc/help.rs:170,179` and man rendering at `doc/man.rs:73,85`. | Transitive consequence of stranded nested generator aliases. |
| 15 | `crates/core/messages.rs::messages` | Invoked in the exported `message!` and `ignore_message!` templates at `messages.rs:73,94`; `core/main.rs:11-12` imports macros from this module. | Cross-file macro invocation/owner linkage; fenced macro-liveness gap that the same-file fixture does not eliminate. |
| 16 | `crates/core/messages.rs::ignore_messages` | Invoked by the exported `ignore_message!` template at `messages.rs:94`, available through the same macro-use module. | Same cross-file macro linkage gap. |
| 17 | `crates/core/messages.rs::set_errored` | Invoked by exported `err_message!` at `messages.rs:84`, which also invokes `message!` at `:85`. | Same cross-file/nested macro linkage gap. |
| 18 | `crates/printer/src/util.rs::from_sink_match` | `Sunk::from_sink_match` is called by the standard printer's sink implementation at `standard.rs:905`. | Generic `Sink for StandardSink` caller reachability; not unused library API. |
| 19 | `crates/printer/src/util.rs::from_sink_context` | `Sunk::from_sink_context` is called by the standard printer's sink implementation at `standard.rs:920`. | Same generic sink caller reachability gap. |
| 20 | `crates/searcher/src/lines.rs::without_terminator` | Called in production search processing at `searcher/core.rs:120,351`. | Generic search execution/helper caller reachability gap. |
| 21 | `crates/searcher/src/lines.rs::preceding` | Called in production context extraction at `searcher/core.rs:198,253`. | Same generic search execution/helper caller reachability gap. |

### Remaining mechanisms and scope

- **Nested Rust re-export aliases: 12 rows**, counting ten directly aliased generators and the two stranded helpers (`generate_digits`, `lookup`). `flags/mod.rs:18-39` is an actual nested use tree (`complete::{...}`, `doc::{help::{...}, version::{...}}`), not the flat re-export covered by the new fixture. Preserved raw use text is available to `callgraph_store/mod.rs`, so recursively flattening/retaining that tree and resolving each alias is **inside the fence**. This is part of the intended Rust use-as behavior and is the clearest residual gap this slice should close; the report-only follow-up does not claim it fixed.
- **Cross-file exported macro reachability: 3 rows.** The helpers are visibly called in templates and the root imports the module with `#[macro_use]`. The delivered owner/body-edge fixture proves the same-file case, not cross-file exported-macro scoping or nested macro invocations. Extraction/resolution of macro owners and their invocation associations is **fenced work** in parser/calls/callgraph/callgraph_store. The exact absent incoming/outgoing edge was not inspected from the runtime graph here; do not reinterpret these rows as proof that no body edges were extracted.
- **Trait/generic caller reachability: 6 rows.** Two local `Separator` references are candidates for fenced Rust type-alias/qualified-call resolution; four printer/searcher helpers sit under generic sink/search caller chains. Some fixes may fit the resolver, but full receiver/trait liveness may require `callgraph_store/dispatch.rs` or `inspect/scanners/dead_code.rs`, which are **outside the original file fence**. Source uses prove false positives; they do not, without a graph trace, identify a single safe fenced patch.

Thus at least the **12 alias-related and 3 macro-related rows are clear follow-up obligations within this slice's mechanisms**. The six generic/trait rows warrant a separate traced diagnosis rather than optimistic blanket rooting. No new engine changes were made in this report-only follow-up; ripgrep's 21 rows still exceed the <=10 target, with every row explained rather than silently waived.

## Implemented behavior

- Go file-level `var`/`const` bindings are `SymbolKind::Variable` on the parser-backed surfaces. Local bindings and blank identifiers are excluded. Initializer/closure calls belong to the binding range, not `<top-level>`.
- Go callback values and package-variable references enter the value-ref lane. Bare names resolve within the package; dotted paths resolve through an import alias (including a raw-text alias fallback) or a same-package name. Dotted unresolved values survive projection. Value-ref kind determines projected provenance on both legacy and view stores, so callbacks do not masquerade as method dispatch.
- `resolve_ref` is the single Go resolver. It uses go.mod module prefixes to find package files for calls, values and imports. Import resolutions are stored; snapshot initialization edges consume those stored targets, not current checkout bytes. A reached file activates variables in its package and imported packages, including blank imports; the paired unimported-package fixture stays unreachable. Package init functions still need the explicit reachability follow-up identified above. A separate `<go-package-init>` intermediate avoids accidentally rooting unattributed calls.
- go.mod events drop the shared module map and re-resolve stored Go refs. The aggregate manifest list and rollup predicate already included go.mod at this slice's base; the new warmed-cache fixture proves the real aggregate hash rejects a go.mod-only edit and its findings equal a clean build.
- Rust inline-module use aliases now reach the original scoped item; flat qualified re-export aliases land on the definition; nested use-tree aliases remain a corpus gap described above. Macro-template calls have `name!` owner nodes and edges, never unconditional roots. View blobs retain those calls and explicit initializer/attribute roots.
- Actual view materialization previously marked every exported variable as an entry point. For Go variables only, it now uses the serialized explicit roots instead. The Cobra imported/unimported pair checks real view/legacy equality; a separate TypeScript exported-variable fixture retains the old root flag and its language-fence mutant is red.

All new fixtures are crate-internal tests in fenced files. The Go provider test exercises package variables/constants through the same symbol-list path used by outline and symbol lookup. Existing Go parser/outline contracts passed in the repository library gate; no existing expected-value contract needed reversing.

### Upgrade and one-time rebuild cost

`BUILD_OUTPUT_VERSION` is now `v14-go-package-initializers-rust-aliases`. `CALLGRAPH_PRODUCER_VERSION` is `callgraph-v3-package-initializers-macro-owners`; the active view `PRODUCER` is `ruled-callgraph-v4-package-initializers-macro-owners`. Old parse-blob keys are misses, proved by a real blob-store lookup fixture. The producer/schema pin test is intentionally advanced with the producer version.

This upgrade rebuilds legacy callgraph stores and invalidates existing view callgraph blob keys. Every view re-extracts/materializes its callgraph once; thereafter identical file content/language/producer keys reuse the rebuilt parse blobs. This is a **one-time extraction cost per file content**, not a recurring rebuild on every inspect. It belongs in release notes. Updating `TIER2_CONTRIBUTION_CACHE_VERSION` is deferred to the subsequent campaign upgrade before release. This slice does not claim migration of old stored Tier-2 aggregates.

To reach the active view surface, narrowly scoped changes also cover join serialization/forwarding, both producer constants, go.mod input predicates, and the Go-variable-only node root flag in views/materialization.rs. Other languages and symbol kinds retain the existing root flag. The inspect aggregate/rollup hooks needed no implementation edits because go.mod was already present in their manifest lists at the base revision.

## Verification

### Exact-total collection regression

The collection harness now reads `total.value` for `{"kind":"exact","value":N}` envelopes while retaining numeric-total compatibility. The new `CollectionFixtures.test_offset_paging_reads_exact_total_object_not_summary_count` fixture collects two pages using the envelope's total even when summary count is one; reverting the fix fails that fixture with `collected 2 of {'kind': 'exact', 'value': 2} rows`. The companion `test_offset_paging_rejects_missing_rows_with_exact_total_object` fixture rejects a one-row terminal page advertising two rows. Disabling the completeness guard makes only that second fixture fail (`RuntimeError not raised`), while the valid-paging fixture remains green. Both mutations restored an empty unstaged diff. Python 3.14.4 on Linux ran all **30 offline harness fixtures**, passing. Local Python 3.9.6 scoring then validated three before/after repositories and all applicable judgments, with no lowered cells.

- Initial red: `parser::tests::go_package_variables_and_constants_are_symbols` failed on missing `Boot` before extraction was implemented; restored fixture passed.
- Final focused resolver module: Cargo 1.99.0, `cargo test -p agent-file-tools --lib -- rust_resolution_tests`: **21 passed**. Includes imported/unimported Cobra vars, Boot/Register, callbacks, warmed module map, warmed aggregate, aliases, macro invocation/no-invocation, old-version lookup misses, and legacy/view parity.
- `cargo test -p agent-file-tools --test integration -- callgraph_test`: **58 passed**.
- `cargo test -p agent-file-tools --bin aft`: **122 passed**.
- Mandatory `bash scripts/rust-test-gate.sh` ran after each implementation refinement (three runs). Initial library phase: **5705 passed, 3 failed, 50 ignored**; intermediate: **5706 passed, 2 failed, 50 ignored**. Final library phase: **5706 passed, 3 failed, 50 ignored**; TLS prerequisite **1 passed**, tokenizer **3 passed**, Biome **711 files checked**. No later full-gate phases were reached after the library failure. These gates are **failed**, not passed.
- The preceding [ts-liveness delivery](ts-liveness.md) records exact-base reproductions for three matching failure names: `commands::semantic_search::tests::blackholed_backend_never_blocks_status_or_search` (Network is unreachable/os error 101 text), `storage_permissions_tests::storage_debug_archive_ignores_public_member_modes` (ENOENT), and `subc::test_support::deferred_navigation_producer_notifies_completion` (SendError; passed on the final run). No out-of-scope fixes were included. The final run additionally hit `sandbox_spawn::policy_tests::native_sandbox_predicate_controls_spawn_and_rewrite`: a shared `/motor-home/tmp` directory disappeared during sandbox read-root traversal. Its focused rerun passed **1 test**; this transient environmental race is not claimed to be an exact-base reproduction.
- Rustfmt 1.10.0-stable: local `cargo fmt --all` and `cargo fmt --all -- --check`, plus `git diff --check`, passed. Comment reviews flagged no unclear new comments. AFT diagnostics were partial; direct Rust compilation/tests were authoritative.
- The initial code revision `528861ee3e434a9d6caab2d92ec438189d430e7e` built in release on Linux (13m35s). Subsequent refinements store Go import resolutions before projecting initializer edges and correct the Go-only materialization root flag; the chair must rebuild the delivered final source rather than reuse that earlier release binary.
- Several default-2-vCPU jobs ended SIGTERM before tests. MOTOR identified OOM: test rustc needs about 7.2 GB but the default allocation provides 7 GiB. Subsequent AFT Cargo jobs used `linux,4c` (14 GiB) and completed. No local build or test fallback was used.
- Retained gate outputs: `local-ignore/callgraph/evidence/rust-gate-initial.stdout` and `rust-gate-final.stdout`. The reviewer subsequently supplied the corpus captures, which were scored locally above; no synthetic load, package-manifest edits, or search-ranking/routing changes were made.

## Mutation controls

Every control staged the live implementation first, captured a nonempty unstaged `git diff --stat`, ran its named test, then restored with `git checkout -- <path>` and `touch <path>` and captured an empty diff. Successful controls each ran one test: exactly that test failed; all other tests were filtered, not claimed green under the mutant.

| Removed behavior | Named red fixture |
|---|---|
| Go var extraction | `callgraph_go_cobra_imported_initializer_live_unimported_dead` — imported InitBackendContext became an ordinary dead finding |
| Rust use-as resolution | `callgraph_rust_inline_use_alias_keeps_bin_function_live` — no main -> a::f caller edge |
| Macro body edges | `callgraph_rust_macro_body_live_only_with_invocation` — missing m! -> helper |
| go.mod aggregate hash input | `callgraph_go_mod_edit_re_resolves_and_invalidates_warmed_aggregate` — old aggregate reused |
| Go value-ref extraction | `callgraph_go_main_boot_register_and_function_values` — missing Local callback edge |
| Go module-map invalidation | `callgraph_go_mod_refresh_drops_warmed_module_map` — exact ref status stayed unresolved |
| Go initializer liveness edges | `callgraph_go_cobra_imported_initializer_live_unimported_dead` — Command and InitBackendContext became dead |
| Stored Go import resolution | `callgraph_go_cobra_imported_initializer_live_unimported_dead` — import could not activate initializer |
| View macro serialization | `callgraph_view_macro_and_go_initializer_equal_cold_legacy` — view missing m! -> helper |
| Serialized main-package roots | `callgraph_view_macro_and_go_initializer_equal_cold_legacy` — Boot missing from explicit liveness roots |
| Go-only materialization root flag | `callgraph_go_cobra_imported_initializer_live_unimported_dead` — unimported view Command rooted incorrectly |
| Non-Go root preservation | `callgraph_non_go_exported_variable_retains_view_root_flag` — TypeScript Public lost its existing root |

An initial module-map mutation was **undefended**: direct-callers assertions also accept name-match edges. The fixture was strengthened to check the real `refs.status = resolved`, and the same mutation then failed it. A preliminary view-macro mutation did not reach its intended assertion because the stronger Go parity check first exposed lost value-ref provenance in view projection; after that projection fix, the macro control failed specifically on missing m! -> helper. Two initial view-root controls were also undefended because actual materialization copied `exported` into the root flag and bypassed the serialized evidence. The real-view unimported-package fixture exposed that bug; after the Go-only fix, omitting serialized main-package roots fails the Boot assertion. A binder-only flag control remains redundant on the materialization path. These preliminary runs are not counted as successful proofs. The declaration carries their outcomes and the successful replacements.

## Scoped mechanism repairs completed after the measured revision

The four requested mechanisms are implemented and verified without blanket roots at engine commit `8035691a2b288875959cfd7c102d31bab678df8f`. The corpus tables remain historical until the new engine tip is rerun. The eleven Go interface-contract rows and six Rust trait/generic rows remain the **interface-liveness follow-up**, owned by the scanner/typed-dispatch layers (`inspect/scanners/dead_code.rs` and `callgraph_store/dispatch.rs`), unchanged by this work.

| Mechanism | Diagnosis and repair | Before/after fixture |
|---|---|---|
| Nested Rust use-tree aliases | Walk the use AST recursively once at extraction, emitting canonical leaf bindings with distinct source spans. Preserve visibility, aliases, self and wildcard leaves. Both bare imports and qualified facade calls resolve. View indexes choose the most-specific Rust re-export module, not a lexically earlier parent from its dependency set. | `callgraph_rust_nested_use_tree_aliases_match_legacy_and_view` was red on the missing main-to-generator edge; now generators plus transitive lookup/digits are live on both stores while spare remains dead. |
| Cross-file/nested macros | Persist macro-use module declarations and resolve owners through lexical module ancestors, including nested macro-use imports. Macros remain non-root intermediates; uninvoked exported macros keep their helpers dead. | `callgraph_rust_cross_file_exported_nested_macros_match_legacy_and_view` was red on stranded helpers; now main -> outer!, outer! -> inner! and both helper edges survive on both stores. The no-invocation half remains dead. |
| Go init functions | Package-initialization edges include init owners only when their package becomes live, like variable initializers. Multiple init declarations in one file share an owner but merge all outgoing bodies instead of overwriting the first. | `callgraph_go_imported_init_functions_match_legacy_and_view` was red on Events/OtherEvents. Both are live only in the imported half; legacy/view findings agree. |
| Fluent HTTP callback registration | Traced before fixing: ordinary wrapper calls were extracted, but the outer fluent selector skipped the computed receiver containing `http.HandlerFunc(GoGetHandler)`. Walk computed receivers while continuing to treat pure dotted names as one reference. | `callgraph_go_fluent_http_callback_trace_and_view_parity` was red at the extraction trace. It now checks a resolved value-ref row, the stored GitController -> GoGetHandler edge on both surfaces, and live handler/dead unregistered control. |

### View persistence collision and upgrade safety

The nested macro fixture exposed a view-only collision: two template calls occupy one opaque AST ordinal. Structural import/re-export facts can also share an ordinal. The old bound key and emission lookup used only that ordinal, replacing one fact with another. AST ordinals remain unchanged in parse blobs. Singleton references retain their old bound IDs; colliding facts receive deterministic disambiguated keys derived from source byte span, caller, kind and preserved path text. Hashing uses canonical source facts, never rowids, time, machine paths or randomized map iteration; deterministic probing prevents numeric key collisions. The unique-call primary in a collision group retains its existing dispatch-site key.

`callgraph_view_collision_keys_are_stable_and_previous_rows_are_refused` checks repeated extraction, distinct collision keys, unchanged singleton keys, and both outer-macro body edges in an actual view. It then stores an old-scheme row in a ready database stamped with the previous fingerprint and proves the current reader refuses it before querying rows. Restoring the previous `v14-go-package-initializers-rust-aliases` build-output version makes that test fail because its fingerprint then admits the old row-ID scheme as current instead of requiring a rebuild.

The current versions are `v15-nested-use-macro-imports-go-init-callbacks`, `callgraph-v4-nested-use-macro-imports-go-init-callbacks`, and active-view `ruled-callgraph-v5-nested-use-macro-imports-go-init-callbacks`. They invalidate prior parse keys and derived database fingerprints, so old IDs/surface snapshots cannot seed a current read. Rebuilding remains a one-time cost per file content/language/current producer, followed by reuse. The previous-producer lookup fixture now targets the immediately preceding versions. Earlier upgrade-version descriptions in this report describe the measured `a37cdbe83` revision.

### Gates and mutation controls for the scoped repairs

- Cargo 1.99.0, all Cargo jobs on `linux,4c`: resolver module **26 passed**, actual crate-internal manifest integration module **7 passed**, callgraph navigation integration **58 passed**, binary fixtures **122 passed**.
- Mandatory full gate ran: TLS prerequisite **1 passed**, tokenizer **3 passed**, Biome **711 files checked**, main library **5712 passed, 2 failed, 50 ignored**. The two failures were `blackholed_backend_never_blocks_status_or_search` (the assertion rejects Linux's Network is unreachable/os error 101 wording) and `storage_debug_archive_ignores_public_member_modes` (archive fixture ENOENT); neither involves the changed callgraph code. Later full-gate phases were not reached. Retained log: `local-ignore/callgraph/evidence/rust-gate-mechanism-repairs.stdout`.
- An initial `--test integration -- join_manifest_test` filter ran zero tests and is not counted as a pass; that file is included as `callgraph_store::join::manifest_integration_tests`, whose seven tests were then run successfully.
- Rustfmt 1.10.0-stable format/check and diff checks passed. Fresh scoped diagnostics were authoritative for 15 files with zero errors/warnings; unrelated Tier-2 inspect categories were unavailable. Source comments were reviewed without flags.
- Eight independent controls were staged safely, showed nonempty mutation diffs and restored empty diffs. Each selected one named test, and only that test failed; others were filtered. Controls removed recursive leaves, Rust-specific view target selection, macro-use linking, init roots, multiple-init body merging, fluent receiver traversal, collision disambiguation and the upgraded admission floor. Detailed evidence is in the delivery declaration. The restored 26-test resolver module passed afterward.

No benchmark re-collection or interface/trait liveness change was made. The existing 32 Go judgments and 21 ripgrep explanations describe the actual dead-code findings captured at the older engine revision `a37cdbe83`; they are retained even if a newer engine removes those findings. Engine revision `8035691a2b288875959cfd7c102d31bab678df8f` is ready for the next Linux corpus collection. No improved counts or precision are claimed until that run completes.

## Same-name Go dispatch regression: traced and corrected

Engine correction: **`e9fd4a6f27d168bf1cd7ba5b3ce984957ddae833`** on the rebuilt branch. The prior corpus tables are retained as historical measurements; this change moves Go projection/root semantics and awaits a new corpus run.

### Actual edge/root cause

The unchanged main integration fixture `inspect_dead_code_keeps_go_sort_interface_methods_live` was reproduced red: only the unused method `Unused` was reported, while the unrelated plain `Less` function was incorrectly live. A reduced fixture traced all stored references and the fixture's outgoing graph edges **before repair**:

- `sort.Sort` was unresolved external dispatch, not a call to plain `Less`;
- no stored ref targeted `Less`, and there were no explicit projection roots;
- the slice fabricated `Less -> <go-package-init>` from the unused plain function, because it generated initialization edges from every declaration;
- that outgoing edge introduced a graph-source key named `Less`. The scanner's existing live sort-method name could then adopt that key as a dispatch root, despite having no real reference to the plain function.

The source fix is therefore **not** a change to interface-contract liveness or to the resolver's target binding. Initialization edges now originate only from the package bridge, `main`, or an owner with an already confirmed call/value edge to an indexed definition. Unresolved local identifiers and builtin/external-only calls do not qualify. Package bridges still initialize package variables, init functions and imported package bridges, so empty imported files and initializer-only packages retain their required execution behavior. The imported/unimported initialization, Cobra, Boot/Register and callback fixtures all remain green.

### Actual-view parity and narrow flag correction

The new parity fixture also exposed an independent view-root issue: materialization copied `exported` into every Go method's root flag. It rooted both `Unused` and a bare `Less` name regardless of the legacy graph. The existing Go-variable exception now also applies to **Go methods only**: use the serialized explicit roots rather than export capitalization. Every other language and symbol kind keeps its previous flag.

`callgraph_go_sort_dispatch_does_not_rescue_same_named_plain_function` has six variants: same-file `package main` and a separately imported `items` package, each with a constant-false plain Less body, unresolved parameter comparisons, or a builtin-only call. Both the fresh legacy-store build and a database materialized from immutable view blobs must produce snapshots that:

- contain no fabricated initializer edge owned by the plain `Less`;
- keep the plain `Less` function and `Unused` method dead;
- retain live `Len`, `Less` and `Swap` methods through the used sort interface;
- return identical findings, including the exported methods on the imported package's used receiver type.

The non-Go preservation fixture retains its TypeScript exported-variable assertion and now also checks a Rust exported method's existing root flag. A method-only language-fence mutant leaves the TypeScript variable intact but makes the Rust method assertion fail, so method preservation is independently defended.

### Existing-view invalidation

The legacy/derived database build fingerprint is `v17-go-initializer-owner-method-root-identity`; immutable blob lookup keys use producer `callgraph-v6-go-initializer-owner-method-root-identity`; active view manifest headers and attachment keys use `ruled-callgraph-v7-go-initializer-owner-method-root-identity`. The previous-view rejection fixture now stamps the immediately preceding v16 fingerprint; the current reader refuses it, and reverting the floor to v16 makes the fixture red. Previous producer keys are lookup misses. Old views therefore rebuild the changed Go owner/root interpretation rather than reusing it. The rebuild remains one time per file content/language/current producer, followed by reuse.

### Gates and controls

Cargo 1.99.0, all builds/tests serialized on remote Linux with four virtual CPUs:

- full integration **inspect** family: **311 passed, 6 ignored, 0 failed**, including the unchanged main sort-interface fixture;
- full integration **callgraph** family: **112 passed, 2 ignored, 1 failed**. The sole failure is `subc_format_test::callgraph_format_matches_typescript_golden_fixtures`: seven TypeScript formatting cases expect `<PROJECT_ROOT>` but receive a home-abbreviated `~/tmp/...` path. Restoring all five changed source files to exact prior HEAD `481b7ede9` (apart from a nonexecuting control comment) reproduces the same failure. No formatter/test expectation was changed;
- corpus-facing resolver/legacy-view fixtures: **28 passed**;
- binary/watcher fixtures: **132 passed**;
- mandatory full Rust gate: **5954 library tests passed, 2 failed, 50 ignored**, TLS **1 passed**, tokenizer **3 passed**, Biome **714 files checked**. The two failures remain the semantic-backend response assertion and missing storage archive fixture; later phases were not reached;
- rustfmt 1.10.0-stable format/check and diff checks passed. Editor diagnostics were provisional; direct compilation checked the actual source.

Four new independent controls were red and restored from the staged live state with empty post-restore diffs: restoring synthetic edges from unused declarations such as `Less` to the package-init bridge fails the **original unchanged integration test**, restoring exported Go-method roots fails the new parity fixture, leaking the method flag rule into non-Go code fails only the Rust-method preservation control, and reverting the admission floor fails the previous-view rejection fixture. Each invocation selected one named test; other tests were filtered. The restored 28-test corpus-facing module passed afterward.

Retained logs outside build directories: `local-ignore/callgraph/evidence/inspect-integration-go-sort-fix.stdout` and `rust-gate-go-sort-fix.stdout`. All entries in `judgments/callgraph.json` and the earlier judgment files remain attached to their original capture directories. No resolver or interface-contract policy was changed, no new corpus collection was run, and the coordinating review run must settle the new Go counts before release.
