# upgrade-and-report: inspect-truth cell gate

Baseline: 0.58.2 (`13dd9cb71`). Both tables use this revision of `run.py score`.
The Before table measures that binary; After measures the release binary from
`ffe59a5d7d1f333deaca3f84b798459ea9b088df`. The same scorer and category-specific
judgments prevent a scoring-policy change from masquerading as an accuracy gain.
AFT's own checkout is a new corpus entry with no Before measurement.

Table key: AFT/oracle are finding counts in the configured scoring paths;
`agreed (+file)` is exact agreement plus unused-file evidence. Precision is
agreement/AFT, recall is agreement/oracle. Oracle judgments remove oracle rows;
AFT judgments annotate rows without removing them. `unknown` is an unscoreable
incomplete/unavailable result, not an empty list; its collected/unjudged zeros
cannot prove complete review. `n/a` is an undefined ratio.

Absolute raw paths below identify this retained worktree's captures; the portable
locations are under `local-ignore/upgrade-and-report/final/corpus/_results/`.
The corpus identifier is the exact repository source commit.
`baseline_aft_truncated_scopes` identifies baseline scopes whose list exceeded
100 rows: TypeORM typings has 134 findings but only 100 returned, so that scope
is omitted from both comparisons. An empty list means no such omission.
`judgments_unapplied` counts previously judged rows absent from the particular
output being scored, not invalid or deleted judgments. Unsupported-language
files are named coverage gaps, not purported implementations of those languages.
The cache tests call version 34 “pre-campaign”: it is the frozen aggregate format
before the liveness and full-list repairs in these implementation steps.

## Before

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

- outline raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/pool-653/local-ignore/upgrade-and-report/final/corpus/_results/outline/13dd9cb71` (corpus `d410db1b24594d71b766212282804d79ca18e5c1`).
- outline baseline_aft_truncated_scopes (excluded from comparison): []
- outline judgments_unapplied: {"upgrade-and-report.json": 0, "harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 0, "inspect-tool.json": 0}

- typeorm raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/pool-653/local-ignore/upgrade-and-report/final/corpus/_results/typeorm/13dd9cb71` (corpus `17e858da8d2becab1ca47826f670a4660da70142`).
- typeorm baseline_aft_truncated_scopes (excluded from comparison): [{"category": "unused_exports", "path": "packages/typeorm/src/driver/mongodb/typings.ts", "count": 134, "returned": 100}]
- typeorm judgments_unapplied: {"upgrade-and-report.json": 0, "harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 0, "inspect-tool.json": 0}

- ripgrep raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/pool-653/local-ignore/upgrade-and-report/final/corpus/_results/ripgrep/13dd9cb71` (corpus `3fce3b5bb0236da2df6d99672afb8a719642eca7`).
- ripgrep baseline_aft_truncated_scopes (excluded from comparison): []
- ripgrep judgments_unapplied: {"upgrade-and-report.json": 0, "harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 0, "inspect-tool.json": 0}

- axum raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/pool-653/local-ignore/upgrade-and-report/final/corpus/_results/axum/13dd9cb71` (corpus `f8b02f22cf10bee707bda19b58265b9e33677535`).
- axum baseline_aft_truncated_scopes (excluded from comparison): []
- axum judgments_unapplied: {"upgrade-and-report.json": 0, "harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 4, "inspect-tool.json": 0}

- soft-serve raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/pool-653/local-ignore/upgrade-and-report/final/corpus/_results/soft-serve/13dd9cb71` (corpus `37685d36f5b7bf0e32217ddd7c8e045c57772619`).
- soft-serve baseline_aft_truncated_scopes (excluded from comparison): []
- soft-serve judgments_unapplied: {"upgrade-and-report.json": 0, "harness.json": 0, "callgraph.json": 5, "ts-liveness.json": 0, "dead-code-scanner.json": 0, "inspect-tool.json": 0}

- fastapi raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/pool-653/local-ignore/upgrade-and-report/final/corpus/_results/fastapi/13dd9cb71` (corpus `26dbe0b00e1211b99446500fc6e402cfec6a3b99`).
- fastapi baseline_aft_truncated_scopes (excluded from comparison): []
- fastapi judgments_unapplied: {"upgrade-and-report.json": 0, "harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 0, "inspect-tool.json": 0}

## After

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

- outline raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/pool-653/local-ignore/upgrade-and-report/final/corpus/_results/outline/ffe59a5d7d1f333deaca3f84b798459ea9b088df` (corpus `d410db1b24594d71b766212282804d79ca18e5c1`).
- outline baseline_aft_truncated_scopes (excluded from comparison): []
- outline judgments_unapplied: {"upgrade-and-report.json": 0, "harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 15, "dead-code-scanner.json": 0, "inspect-tool.json": 0}

- typeorm raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/pool-653/local-ignore/upgrade-and-report/final/corpus/_results/typeorm/ffe59a5d7d1f333deaca3f84b798459ea9b088df` (corpus `17e858da8d2becab1ca47826f670a4660da70142`).
- typeorm baseline_aft_truncated_scopes (excluded from comparison): [{"category": "unused_exports", "path": "packages/typeorm/src/driver/mongodb/typings.ts", "count": 134, "returned": 100}]
- typeorm judgments_unapplied: {"upgrade-and-report.json": 0, "harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 196, "dead-code-scanner.json": 0, "inspect-tool.json": 0}
- typeorm/typescript/unused_exports unknown: {"count": 190, "generated_count": 0, "total_count": 984, "test_only_count": 794, "top": [{"file": "packages/codemod/src/transforms/ast-helpers.ts", "symbol": "TYPEORM_FIND_OPTIONS_METHODS"}, {"file": "packages/codemod/src/transforms/ast-helpers.ts", "symbol": "TYPEORM_REPOSITORY_TYPES"}, {"file": "packages/codemod/src/transforms/ast-helpers.ts", "symbol": "TYPEORM_DATASOURCE_TYPES"}], "generated_top": [], "test_only_top": [{"file": "packages/typeorm/test/benchmark/bulk-save-case1/entity/Post.ts", "symbol": "Post", "kind": "class", "line": 6, "provenance": "oxc", "used_by": ["packages/typeorm/test/benchmark/bulk-save-case1/bulk-save-case1.test.ts"]}, {"file": "packages/typeorm/test/benchmark/bulk-save-case2/entity/Document.ts", "symbol": "Document", "kind": "class", "line": 6, "provenance": "oxc", "used_by": ["packages/typeorm/test/benchmark/bulk-save-case2/bulk-save-case2.test.ts", "packages/typeorm/test/benchmark/bulk-save-querybuilder/bulk-save-querybuilder.test.ts"]}, {"file": "packages/typeorm/test/benchmark/bulk-sql-build/entity/Post.ts", "symbol": "Post", "kind": "class", "line": 8, "provenance": "oxc", "used_by": ["packages/typeorm/test/benchmark/bulk-sql-build/bulk-sql-build.test.ts"]}], "excluded_test_count": 93, "excluded_test_files": 57, "complete": false, "gaps": [{"kind": "analysis_incomplete", "reason": "4 files could not be parsed"}], "parse_errors": [{"file": "packages/codemod/test/transforms/v1/fixtures/column-readonly/column-readonly-metadata-args-namespace-type-only.input.ts", "message": "parse: An implementation cannot be declared in ambient contexts."}, {"file": "packages/codemod/test/transforms/v1/fixtures/column-readonly/column-readonly-metadata-args-namespace-type-only.output.ts", "message": "parse: An implementation cannot be declared in ambient contexts."}, {"file": "packages/codemod/test/transforms/v1/fixtures/column-readonly/column-readonly-metadata-args-namespace.input.ts", "message": "parse: An implementation cannot be declared in ambient contexts."}, {"file": "packages/codemod/test/transforms/v1/fixtures/column-readonly/column-readonly-metadata-args-namespace.output.ts", "message": "parse: An implementation cannot be declared in ambient contexts."}]}
- typeorm/typescript/dead_code unknown: {"count": 140, "supported_language_files": 3606, "generated_count": 0, "total_count": 934, "test_only_count": 794, "by_language": {"tsx": 3, "typescript": 137}, "languages_skipped": ["bash", "json", "markdown", "yaml"], "top": [{"file": "packages/typeorm/test/functional/data-source/modules/blog/entity/Blog.ts", "symbol": "Blog"}, {"file": "packages/typeorm/test/functional/data-source/modules/blog/subscriber/TestBlogSubscriber.ts", "symbol": "TestBlogSubscriber"}, {"file": "packages/typeorm/test/functional/data-source/modules/question/entity/Question.ts", "symbol": "Question"}], "generated_top": [], "test_only_top": [{"file": "packages/typeorm/test/benchmark/bulk-save-case1/entity/Post.ts", "symbol": "Post", "kind": "class", "line": 6, "provenance": "oxc", "used_by": ["packages/typeorm/test/benchmark/bulk-save-case1/bulk-save-case1.test.ts"]}, {"file": "packages/typeorm/test/benchmark/bulk-save-case2/entity/Document.ts", "symbol": "Document", "kind": "class", "line": 6, "provenance": "oxc", "used_by": ["packages/typeorm/test/benchmark/bulk-save-case2/bulk-save-case2.test.ts", "packages/typeorm/test/benchmark/bulk-save-querybuilder/bulk-save-querybuilder.test.ts"]}, {"file": "packages/typeorm/test/benchmark/bulk-sql-build/entity/Post.ts", "symbol": "Post", "kind": "class", "line": 8, "provenance": "oxc", "used_by": ["packages/typeorm/test/benchmark/bulk-sql-build/bulk-sql-build.test.ts"]}], "excluded_test_count": 21, "excluded_test_files": 17, "complete": false, "gaps": [{"kind": "analysis_incomplete", "reason": "4 files could not be parsed"}], "parse_errors": [{"file": "packages/codemod/test/transforms/v1/fixtures/column-readonly/column-readonly-metadata-args-namespace-type-only.input.ts", "message": "parse: An implementation cannot be declared in ambient contexts."}, {"file": "packages/codemod/test/transforms/v1/fixtures/column-readonly/column-readonly-metadata-args-namespace-type-only.output.ts", "message": "parse: An implementation cannot be declared in ambient contexts."}, {"file": "packages/codemod/test/transforms/v1/fixtures/column-readonly/column-readonly-metadata-args-namespace.input.ts", "message": "parse: An implementation cannot be declared in ambient contexts."}, {"file": "packages/codemod/test/transforms/v1/fixtures/column-readonly/column-readonly-metadata-args-namespace.output.ts", "message": "parse: An implementation cannot be declared in ambient contexts."}]}

- ripgrep raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/pool-653/local-ignore/upgrade-and-report/final/corpus/_results/ripgrep/ffe59a5d7d1f333deaca3f84b798459ea9b088df` (corpus `3fce3b5bb0236da2df6d99672afb8a719642eca7`).
- ripgrep baseline_aft_truncated_scopes (excluded from comparison): []
- ripgrep judgments_unapplied: {"upgrade-and-report.json": 0, "harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 27, "inspect-tool.json": 0}
- ripgrep/rust/dead_code languages_unsupported: [{"language": "ruby", "files": 1}]

- axum raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/pool-653/local-ignore/upgrade-and-report/final/corpus/_results/axum/ffe59a5d7d1f333deaca3f84b798459ea9b088df` (corpus `f8b02f22cf10bee707bda19b58265b9e33677535`).
- axum baseline_aft_truncated_scopes (excluded from comparison): []
- axum judgments_unapplied: {"upgrade-and-report.json": 0, "harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 1, "inspect-tool.json": 0}

- soft-serve raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/pool-653/local-ignore/upgrade-and-report/final/corpus/_results/soft-serve/ffe59a5d7d1f333deaca3f84b798459ea9b088df` (corpus `37685d36f5b7bf0e32217ddd7c8e045c57772619`).
- soft-serve baseline_aft_truncated_scopes (excluded from comparison): []
- soft-serve judgments_unapplied: {"upgrade-and-report.json": 0, "harness.json": 0, "callgraph.json": 3, "ts-liveness.json": 0, "dead-code-scanner.json": 2, "inspect-tool.json": 0}

- fastapi raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/pool-653/local-ignore/upgrade-and-report/final/corpus/_results/fastapi/ffe59a5d7d1f333deaca3f84b798459ea9b088df` (corpus `26dbe0b00e1211b99446500fc6e402cfec6a3b99`).
- fastapi baseline_aft_truncated_scopes (excluded from comparison): []
- fastapi judgments_unapplied: {"upgrade-and-report.json": 0, "harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 0, "inspect-tool.json": 0}
- fastapi/python/dead_code languages_unsupported: [{"language": "python", "files": 1166}]

- aft raw output: `/Users/ufukaltinok/.local/share/cortexkit/alfonso/worktrees/6d75dd56448a4a9c/pool-653/local-ignore/upgrade-and-report/final/corpus/_results/aft/ffe59a5d7d1f333deaca3f84b798459ea9b088df` (corpus `ffe59a5d7d1f333deaca3f84b798459ea9b088df`).
- aft baseline_aft_truncated_scopes (excluded from comparison): []
- aft judgments_unapplied: {"upgrade-and-report.json": 0, "harness.json": 0, "callgraph.json": 0, "ts-liveness.json": 0, "dead-code-scanner.json": 0, "inspect-tool.json": 0}
- aft/rust/dead_code languages_unsupported: [{"language": "python", "files": 97}]
- aft/typescript/unused_exports unknown: {"count": 155, "generated_count": 0, "total_count": 236, "test_only_count": 81, "top": [{"file": "packages/aft-bridge/src/command-timeouts.ts", "symbol": "PASSIVE_COMMANDS"}, {"file": "packages/aft-bridge/src/downloader.ts", "symbol": "__test__"}, {"file": "packages/aft-bridge/src/durable-log.ts", "symbol": "LOG_DIR_MODE"}], "generated_top": [], "test_only_top": [{"file": "packages/aft-bridge/src/bash-host-fallback.ts", "symbol": "hostFallbackPathWithShims", "kind": "function", "line": 99, "provenance": "oxc", "used_by": ["packages/aft-bridge/src/__tests__/audit-boundaries.test.ts", "packages/aft-bridge/src/__tests__/bash-host-fallback.test.ts"]}, {"file": "packages/aft-bridge/src/binary-identity.ts", "symbol": "__waitForIdentityWritesForTests", "kind": "function", "line": 261, "provenance": "oxc", "used_by": ["packages/aft-bridge/src/__tests__/binary-identity.test.ts", "packages/aft-bridge/src/__tests__/resolver-version-mismatch.test.ts"]}, {"file": "packages/aft-bridge/src/bridge.ts", "symbol": "__setBinaryFingerprintForTests", "kind": "function", "line": 82, "provenance": "oxc", "used_by": ["packages/aft-bridge/src/__tests__/pool-lifecycle.test.ts"]}], "excluded_test_count": 2973, "excluded_test_files": 144, "complete": false, "gaps": [{"kind": "analysis_incomplete", "reason": "1 file could not be parsed"}], "parse_errors": [{"file": "benchmarks/src/exercise.ts", "message": "parse: Unexpected token"}]}
- aft/typescript/dead_code languages_unsupported: [{"language": "python", "files": 97}]
- aft git status --porcelain before:
```text

```
- aft git status --porcelain after:
```text

```

## Cell rule

- aft: new, no baseline
- FAIL: ('typeorm', 'typescript', 'unused_exports'): precision lowered
- FAIL: ('typeorm', 'typescript', 'unused_exports'): recall lowered
- FAIL: ('typeorm', 'typescript', 'dead_code'): precision lowered
- FAIL: ('typeorm', 'typescript', 'dead_code'): recall lowered

## Upgrade verification and acceptance boundary

The aggregate version remains 35: the one campaign bump from 34 landed with the
callgraph slice, with parent approval. The upgrade tests pin frozen 34 and assert
it is older than the current version; this slice does not bump again.

| Cache regression test | Stored pre-campaign aggregate | Restored result |
|---|---|---|
| `inspect::cache::tests::pre_campaign_aggregate_reopen_pages_all_150_dead_code_findings` | 100 of 150 findings, version-34 hash | 150; pages 100/50/0 |
| `inspect::cache::tests::pre_campaign_aggregate_reopen_pages_all_150_unused_exports` | 100 of 150 findings, version-34 hash | 150; pages 100/50/0 |
| `inspect::cache::tests::pre_campaign_aggregate_reopen_rebuilds_go_initializer_liveness` | Incorrect dead `Register` in `var Boot = Register()` fixture | Count 0; clean-build rows restored |

For each mutation control the live file was staged first and the unstaged diff
was empty. Hash folding was changed to frozen 34; `git diff --stat` then reported
`cache.rs | 2 +-; 1 insertion(+), 1 deletion(-)`. Each test was invoked separately
and was its invocation's **only failing test**, with all other tests filtered.
Both pagination tests failed with “the old truncated aggregate must not be
reused”; Go failed with count 1 instead of 0. `git checkout --` from the staged
implementation plus `touch` restored an empty unstaged diff. Logs are retained
under `local-ignore/upgrade-and-report/mutations.{stdout,stderr}.log`.

Authoritative narrow verification, Cargo 1.99.0 on Linux:

- `cargo test -p agent-file-tools --lib -- inspect::cache::tests`: **21 passed**.
- `cargo test -p agent-file-tools --test list_envelope`: **175 passed**.
- `cargo fmt --all` and `cargo fmt --all -- --check`: exit 0, rustfmt 1.10.0-stable.
- The actual exact-13dd9cb71 binary storage experiment confirms old SQLite D/U
  aggregates have 100 stored items and count 150; the reopened candidate and clean
  candidate return identical findings and pagination totals, with unchanged source
  hashes. The additional tag-v0.58.2 experiment is labeled separately.
- The mandatory `bash scripts/rust-test-gate.sh` **failed**: Biome 2.4.7 checked
  721 files; TLS 1 passed; tokenizer 4 passed/1 ignored; main library 6185 passed,
  4 failed, 56 ignored. The failures are
  `outline_portability_goldens_stay_lf_with_autocrlf`,
  `blackholed_backend_never_blocks_status_or_search`,
  `storage_debug_archive_ignores_public_member_modes`, and
  `candidate_bytes_are_released_before_derived_materialization`. The latter two
  lack fixture files; the semantic response assertion is documented in prior
  deliveries. Later full-gate phases were not reached. No unrelated test was
  changed. Full logs are retained outside build trees in
  `local-ignore/upgrade-and-report/rust-test-gate.{stdout,stderr}.log`.
- Scoped inspection was partial while rust-analyzer checked and the checkout
  graph was unavailable; direct compilation and the focused tests are the source
  gate, not a claim of fresh editor diagnostics.

All seven candidates and six exact-baseline captures were collected and scored.
The score command validates existing entries using relocated descriptor copies
and original or exact-code-regenerated origins. It returns **exit 1, four lowered
TypeORM cells**, not a campaign pass. No earlier judgment was deleted; the new
judgments file adds provenance but no unsupported row decisions.

[REPORT.md](../REPORT.md) records all target outcomes, disabled-server and
unsupported-language coverage, raw/binary hashes, own-checkout empty status
before/after, the remote-only matching-mode chmod workaround, and the remaining
acceptance blockers. In particular: own Rust dead-code count 33 versus rustc's
0 dead-code warnings is not <=10,
33 Rust and 10 in-domain TS dead-code rows remain unjudged, default own U is
unknown because the real benchmark runner is syntactically corrupt, and scoped
release timing on two Linux vCPUs is 5.707473s enabled versus 5.307470s disabled.
The diagnostics/footer corpus pass was not run. These are not explained-away
passes. Cache upgrade delivery is coherent, but campaign acceptance needs the
follow-up work listed in the report.
