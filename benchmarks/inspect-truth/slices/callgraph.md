# Callgraph slice

## Delivery and corpus gate

The code and behavioral fixtures are delivered. The corpus gate is **not measured, owed to the chair**. Here, “chair” means the coordinating review agent that owns the campaign's final acceptance. That agent took ownership of Linux baseline/candidate collection and scoring because the worker's remote runner uploads tracked source but not ignored baseline/corpus inputs. No accuracy score, no-lowered-cell verdict, or corpus `NewServer` classification is claimed here. Historical slice tables are not substituted for raw captures.

`judgments/callgraph.json` records this unmeasured status. It intentionally has no row judgments or after-output directories: none have been collected by this slice.

| Repository | Language | Before | After | Cell verdict |
|---|---|---|---|---|
| ripgrep | Rust | not re-scored | not measured | pending collection/scoring |
| axum | Rust | not re-scored | not measured | pending collection/scoring |
| soft-serve | Go | not re-scored | not measured | pending collection/scoring |
| aft | Rust | new, no baseline | not measured | pending collection/scoring if the language re-run includes the local corpus entry |

### Pinned inputs retained in this worktree

Paths below are relative to this worktree, not to the operator's checkout. They are ignored artifacts outside regenerable build directories; the worker did not modify the corpora.

| Input | Revision | Retained source path |
|---|---|---|
| baseline AFT | `13dd9cb71a7bbf9076377ac1ba1717dabf15e586` (0.58.2) | `local-ignore/callgraph/baseline-source/` — exact `git archive`, without `.git` |
| ripgrep | `3fce3b5bb0236da2df6d99672afb8a719642eca7` | `local-ignore/callgraph/corpus/ripgrep/` |
| axum | `f8b02f22cf10bee707bda19b58265b9e33677535` | `local-ignore/callgraph/corpus/axum/` |
| soft-serve | `37685d36f5b7bf0e32217ddd7c8e045c57772619` | `local-ignore/callgraph/corpus/soft-serve/` |

These local ignored paths are **not** automatically uploaded to remote Linux jobs. Transfer them manually into the Linux repository copy that will run the evaluation.

### Linux collection/scoring recipe

Run from the repository root containing the delivered candidate changes. Preserve captures/oracles outside `target/`; each collection already gets independent AFT storage. Build the baseline archive and candidate serially, with Rust wrappers disabled:

```sh
cargo --version
CARGO_TARGET_DIR="$PWD/target/callgraph-baseline" CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER= \
  cargo build --release -p agent-file-tools \
  --manifest-path local-ignore/callgraph/baseline-source/Cargo.toml
CARGO_BUILD_RUSTC_WRAPPER= RUSTC_WRAPPER= cargo build --release -p agent-file-tools
./target/callgraph-baseline/release/aft --version
./target/release/aft --version
```

Record the exact candidate revision (the source version of this delivered branch, or its reviewed merge) and use it consistently for collection and scoring:

```sh
CANDIDATE_COMMIT="$(git rev-parse HEAD)"
python3 --version
python3 benchmarks/inspect-truth/run.py collect \
  --repo ripgrep --repo axum --repo soft-serve \
  --corpus-root local-ignore/callgraph/corpus \
  --baseline-bin target/callgraph-baseline/release/aft \
  --aft-bin target/release/aft --aft-commit "$CANDIDATE_COMMIT" \
  --diagnostics-sample 0
python3 benchmarks/inspect-truth/run.py score \
  --repo ripgrep --repo axum --repo soft-serve \
  --corpus-root local-ignore/callgraph/corpus \
  --aft-commit "$CANDIDATE_COMMIT" \
  --output-dir local-ignore/callgraph/score --slice callgraph
```

`collect` gathers baseline and candidate, and runs each oracle once per corpus pin. Rust uses the `corpus.json` Cargo-check commands; Go uses staticcheck U1000, deadcode (production and test runs), and go vet. Stage required oracle executables and dependencies on the benchmark host. `--diagnostics-sample 0` omits diagnostics sampling; diagnostics/status-bar acceptance is not claimed by this slice.

For the local AFT Rust entry, add `--repo aft` to the `run.py collect` and `run.py score` commands if required by the campaign language re-run. That entry has no baseline and also invokes its configured TypeScript oracle/install steps. Record `git status --porcelain` before/after; these must match.

The scorer reads **every** existing judgments file, validating entries against each file's own baseline/after origins. Earlier slices recorded some origins under deleted `target/` trees; those captures must be restored or recollected before a score can validate them. Do not delete old judgments or reconstruct their raw rows from Markdown. After collecting, set this slice's `after_outputs` to the real retained directories and add actual row judgments before the final score.

### Acceptance to settle on the corpus

- No defined precision/recall cell may fall below its baseline. Defined precision may become `n/a` only with AFT count zero and every before-side AFT row judged; other defined-to-`n/a` moves fail.
- soft-serve `dead_code (functions)` precision must reach at least **0.6**, or be explained using the reviewed spec's row judgments. `NewServer` must no longer appear in the test-only list; its production caller is inside a package-level var closure.
- ripgrep and axum dead-code counts must each be **<= 10**, with **`aft_rows_unjudged: 0`**, or the missed target must be explained row by row. An explanation is not a claim that the numeric target passed.
- If AFT's own Rust entry is included, its cells are new: Rust count <= rustc count + 10, all dead-code/unused-export rows judged, Python unsupported gap retained, and clean checkout status unchanged.
- Preserve raw baseline, oracle, candidate and judgment-origin outputs outside regenerable directories. Unknown/pending/analysis-incomplete responses are not empty findings and cannot establish accuracy.

## Implemented behavior

- Go file-level `var`/`const` bindings are `SymbolKind::Variable` on the parser-backed surfaces. Local bindings and blank identifiers are excluded. Initializer/closure calls belong to the binding range, not `<top-level>`.
- Go callback values and package-variable references enter the value-ref lane. Bare names resolve within the package; dotted paths resolve through an import alias (including a raw-text alias fallback) or a same-package name. Dotted unresolved values survive projection. Value-ref kind determines projected provenance on both legacy and view stores, so callbacks do not masquerade as method dispatch.
- `resolve_ref` is the single Go resolver. It uses go.mod module prefixes to find package files for calls, values and imports. Import resolutions are stored; snapshot initialization edges consume those stored targets, not current checkout bytes. A reached file initializes its package and imported packages, including blank imports; a package imported only by dead code remains unreachable. A separate `<go-package-init>` intermediate avoids accidentally rooting unattributed calls.
- go.mod events drop the shared module map and re-resolve stored Go refs. The aggregate manifest list and rollup predicate already included go.mod at this slice's base; the new warmed-cache fixture proves the real aggregate hash rejects a go.mod-only edit and its findings equal a clean build.
- Rust inline-module use aliases now reach the original scoped item; qualified re-export aliases land on the definition. Macro-template calls have `name!` owner nodes and edges, never unconditional roots. View blobs retain those calls and explicit initializer/attribute roots.
- Actual view materialization previously marked every exported variable as an entry point. For Go variables only, it now uses the serialized explicit roots instead. The Cobra imported/unimported pair checks real view/legacy equality; a separate TypeScript exported-variable fixture retains the old root flag and its language-fence mutant is red.

All new fixtures are crate-internal tests in fenced files. The Go provider test exercises package variables/constants through the same symbol-list path used by outline and symbol lookup. Existing Go parser/outline contracts passed in the repository library gate; no existing expected-value contract needed reversing.

### Upgrade and one-time rebuild cost

`BUILD_OUTPUT_VERSION` is now `v14-go-package-initializers-rust-aliases`. `CALLGRAPH_PRODUCER_VERSION` is `callgraph-v3-package-initializers-macro-owners`; the active view `PRODUCER` is `ruled-callgraph-v4-package-initializers-macro-owners`. Old parse-blob keys are misses, proved by a real blob-store lookup fixture. The producer/schema pin test is intentionally advanced with the producer version.

This upgrade rebuilds legacy callgraph stores and invalidates existing view callgraph blob keys. Every view re-extracts/materializes its callgraph once; thereafter identical file content/language/producer keys reuse the rebuilt parse blobs. This is a **one-time extraction cost per file content**, not a recurring rebuild on every inspect. It belongs in release notes. Updating `TIER2_CONTRIBUTION_CACHE_VERSION` is deferred to the subsequent campaign upgrade before release. This slice does not claim migration of old stored Tier-2 aggregates.

To reach the active view surface, narrowly scoped changes also cover join serialization/forwarding, both producer constants, go.mod input predicates, and the Go-variable-only node root flag in views/materialization.rs. Other languages and symbol kinds retain the existing root flag. The inspect aggregate/rollup hooks needed no implementation edits because go.mod was already present in their manifest lists at the base revision.

## Verification

- Initial red: `parser::tests::go_package_variables_and_constants_are_symbols` failed on missing `Boot` before extraction was implemented; restored fixture passed.
- Final focused resolver module: Cargo 1.99.0, `cargo test -p agent-file-tools --lib -- rust_resolution_tests`: **21 passed**. Includes imported/unimported Cobra vars, Boot/Register, callbacks, warmed module map, warmed aggregate, aliases, macro invocation/no-invocation, old-version lookup misses, and legacy/view parity.
- `cargo test -p agent-file-tools --test integration -- callgraph_test`: **58 passed**.
- `cargo test -p agent-file-tools --bin aft`: **122 passed**.
- Mandatory `bash scripts/rust-test-gate.sh` ran after each implementation refinement (three runs). Initial library phase: **5705 passed, 3 failed, 50 ignored**; intermediate: **5706 passed, 2 failed, 50 ignored**. Final library phase: **5706 passed, 3 failed, 50 ignored**; TLS prerequisite **1 passed**, tokenizer **3 passed**, Biome **711 files checked**. No later full-gate phases were reached after the library failure. These gates are **failed**, not passed.
- The preceding [ts-liveness delivery](ts-liveness.md) records exact-base reproductions for three matching failure names: `commands::semantic_search::tests::blackholed_backend_never_blocks_status_or_search` (Network is unreachable/os error 101 text), `storage_permissions_tests::storage_debug_archive_ignores_public_member_modes` (ENOENT), and `subc::test_support::deferred_navigation_producer_notifies_completion` (SendError; passed on the final run). No out-of-scope fixes were included. The final run additionally hit `sandbox_spawn::policy_tests::native_sandbox_predicate_controls_spawn_and_rewrite`: a shared `/motor-home/tmp` directory disappeared during sandbox read-root traversal. Its focused rerun passed **1 test**; this transient environmental race is not claimed to be an exact-base reproduction.
- Rustfmt 1.10.0-stable: local `cargo fmt --all` and `cargo fmt --all -- --check`, plus `git diff --check`, passed. Comment reviews flagged no unclear new comments. AFT diagnostics were partial; direct Rust compilation/tests were authoritative.
- The initial code revision `528861ee3e434a9d6caab2d92ec438189d430e7e` built in release on Linux (13m35s). Subsequent refinements store Go import resolutions before projecting initializer edges and correct the Go-only materialization root flag; the chair must rebuild the delivered final source rather than reuse that earlier release binary.
- Several default-2-vCPU jobs ended SIGTERM before tests. MOTOR identified OOM: test rustc needs about 7.2 GB but the default allocation provides 7 GiB. Subsequent AFT Cargo jobs used `linux,4c` (14 GiB) and completed. No local build or test fallback was used.
- Retained gate outputs: `local-ignore/callgraph/evidence/rust-gate-initial.stdout` and `rust-gate-final.stdout`. Corpus captures/scores remain unrun by agreement; no synthetic load, package-manifest edits, or search-ranking/routing changes were made.

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
