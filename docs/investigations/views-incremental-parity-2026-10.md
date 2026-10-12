# Incremental callgraph parity repair — October 2026

## Root cause

The discrepancy is **colliding reference ordinals during unchanged-caller
emission**, not a change to node identity or a missed invalidation. A stored
reference is keyed by `view:{caller_path}:{ref_ordinal}`; its ordinary call edge
is keyed by `edge:{ref_id}`. More than one joined row can use those keys. Rust
calls in separate arguments of one macro naturally share an AST ordinal; some
captured references also combine a call and an export-alias reference.

Cold emission uses the canonical joined-row order and `INSERT OR REPLACE`.
Reference metadata comes from the first extracted reference for the ordinal;
resolution fields come from the last joined row. Edges separately keep the last
eligible call row. Incremental emission incorrectly compared each joined row to
the *persisted base* and skipped identical rows before batching writes. An earlier
changed row could consequently replace a skipped later row, changing the winner.
An unresolved final reference can coexist with an earlier call edge under the
existing cold contract; replay must preserve both, rather than deriving edges
only from the final reference.

The repair records colliding ordinals while constructing the existing emission
lookup, then replays every joined row for those ordinals. Unique-ordinal refs keep
the existing identical-row optimization. Node/ref/edge IDs, schema and resolver
policy are unchanged. Materialization version advances from 9 to 10 so older,
potentially incorrect generations on their next materialization, including when
that call supplies an unchanged manifest.

## Classification of every baseline difference

The original spike is pinned at `f9a62e5f6e6337249af61ea6c656b33cfc01c160`.
Its captured 17 revisions / 5,411 source blobs were reused without regeneration.
Baseline all-table comparison reproduced the original counts exactly. Case
numbers identify the ten base-to-tip revision pairs in the overlay investigation;
the tip column identifies each pair. Counts are distinct primary keys, not the
two differing row versions. Ref categories are mutually exclusive: kind changes
first, then status changes, then target-only changes.

| Case / tip | Edge keys | Ref keys | Total keys | Ref target-only | Ref status | Ref kind/resolution |
|---|---:|---:|---:|---:|---:|---:|
| 0 / `1d5e0846fe17` | 0 | 0 | 0 | 0 | 0 | 0 |
| 1 / `b4de1db5e0b7` | 1 | 2 | 3 | 1 | 1 | 0 |
| 2 / `d78fd9dc8e3f` | 0 | 0 | 0 | 0 | 0 | 0 |
| 3 / `437b6a53cfc2` | 0 | 0 | 0 | 0 | 0 | 0 |
| 4 / `c070b741ed39` | 0 | 0 | 0 | 0 | 0 | 0 |
| 5 / `36534036c2d9` | 15 | 70 | 85 | 15 | 55 | 0 |
| 6 / `4f5e17028da2` | 40 | 150 | 190 | 40 | 110 | 0 |
| 7 / `0c3974b20926` | 116 | 813 | 929 | 116 | 693 | 4 |
| 8 / `e756124e9b50` | 23 | 65 | 88 | 23 | 42 | 0 |
| 9 / `49daf5d12140` | 93 | 696 | 789 | 93 | 599 | 4 |
| **Total** | **288** | **1,796** | **2,084** | **288** | **1,500** | **8** |

Every key has both an incremental and a cold row: no missing/extra primary keys.
All edge differences are limited to target node/file/symbol. Ref differences are
limited to kind, status and target node/file/symbol. Independently extracting each
associated tip file confirmed **every one of the 2,084 keys has multiple
references at its ordinal**: 2,076 call-only collisions and eight call/export-alias
collisions. The eight mixed-kind keys are in
`crates/aft/tests/integration/inspect_tier2_reuse_test.rs` (four each in cases 7
and 9). In those eight groups, the export-alias references' `full_ref` text starts
at the `exports_incremental_matches_cold(...)` substring of a Rust helper name
and extends into its argument strings. These structural references share AST
ordinals with the ordinary helper calls. Changing that extraction policy is
outside this repair.

For example, case 1's `warmup.rs:7842` is shared by `warmup_storage_dir()` and
`aft::bash_background::storage_dir(None)` inside `assert_eq!`. The cold edge points
to `warmup_storage_dir`, whereas incremental points to `storage_dir`.
`warmup.rs:8002` is shared by `trigger_callgraph_warm(...)` and `Some(...)`;
incremental keeps `status='resolved'` and the target node/file/symbol for
`trigger_callgraph_warm`, whereas cold keeps the later `Some(...)` reference's
`status='unresolved'` and null target node/file/symbol.

There were **zero differences** in bindings, dependencies, surfaces, dispatch
sites, unknown-live/dead-code tables, nodes, files, metadata or other derived
tables. Selected unchanged callers were reached; their ordered emission was
wrong. Ordinal shifts in target symbols expose the bug, but are not themselves
an invalidation defect. The small permanent fixtures deliberately shift a target
ordinal while keeping the caller's manifest entry unchanged.

## Permanent coverage and sibling seeds

`materialization/tests.rs` adds all-schema/all-table logical parity tests for:

- A naturally extracted Rust macro with two resolved calls sharing an ordinal.
- Two resolved calls sharing the same ref/edge IDs through a duplicate ordinal:
  the later call's target stays unchanged while the earlier target's node ID shifts.
- A final unresolved reference row that leaves the edge emitted by an earlier
  resolved call intact, matching the existing cold writer's separate edge batch.
- Mixed call/export-alias references sharing an ordinal.
- Rebuilding a version-9 generation despite an identical manifest.

The fixtures clone an immutable seed with `generation::clone_derived`, patch the
clone with its owner's manifest and compare against an independent cold build.
The seed is also checked for immutability in the parametrized fixture.

Sibling seeding is absent from `assembly.rs` at baseline
`a4decaeff44a2eee87f9befcbf797c2774f439cd`. Inspection of commit
`2805f6775bee6b0c2e199f8ec409e7bf64dc7a27`, `assembly.rs:649-709`, confirms it
supplies the sibling's path/manifest to the same clone and
`apply_manifest_diff_profiled` path. The common writer and clone fixture therefore
cover the sibling-seed emission fix; no assembly change is needed. This is not a
claim that the absent sibling-discovery integration tests were run at this base.

## Ten-case result

**10/10 incremental outputs equal their independent cold builds**, with zero
differing keys or rows in every derived table. These counts are direct
incremental-vs-cold comparisons, not overlays derived from a cold fallback.

| Case | Before: incremental-vs-cold differing keys | After: incremental-vs-cold differing keys | Rows checked |
|---|---:|---:|---:|
| 0 | 0 | 0 | 1,780,921 |
| 1 | 3 | 0 | 1,804,570 |
| 2 | 0 | 0 | 1,805,602 |
| 3 | 0 | 0 | 1,776,557 |
| 4 | 0 | 0 | 1,775,899 |
| 5 | 85 | 0 | 1,773,478 |
| 6 | 190 | 0 | 1,795,828 |
| 7 | 929 | 0 | 1,781,124 |
| 8 | 88 | 0 | 1,767,020 |
| 9 | 789 | 0 | 1,589,356 |
| **Total** | **2,084** | **0** | **17,650,355** |

The preserved worker evidence outside build directories includes complete
baseline/fixed stdout, per-key extraction diagnostics and
`.overlay-reproduction/key-classification.json` (all 2,084 primary keys, their
changed columns, collision counts and categories). Private source/SQLite inputs
and the temporary harness are not committed.

## Reproduction method

The overlay investigation at the pinned `f9a62e5f6e6337249af61ea6c656b33cfc01c160`
commit contains `crates/aft/src/callgraph_store/overlay_spike.rs`,
`docs/investigations/callgraph-worktree-overlay-spike-2026-10.md` and
`scripts/spikes/collect-callgraph-overlay.py`. These files were exported
temporarily to reuse its original harness and captured dataset; its test-only
wiring was also applied temporarily. None of that prototype was landed. Commands use Linux, four vCPUs,
Rust/Cargo 1.99.0, and `CARGO_PROFILE_STAGE_DEBUG_ASSERTIONS=true`:

```sh
AFT_OVERLAY_SPIKE_INPUT="$PWD/.overlay-spike-input" \
AFT_OVERLAY_SPIKE_OUTPUT="$PWD/.overlay-fixed-all" \
AFT_OVERLAY_SPIKE_ROWS_ONLY=1 AFT_OVERLAY_SPIKE_EXPECT_EQUAL=1 \
CARGO_PROFILE_STAGE_DEBUG_ASSERTIONS=true \
cargo test --profile stage -p agent-file-tools --lib overlay_spike_real_branches -- --ignored --nocapture
```

The temporary rows-only mode returns **after the original independent
incremental-vs-cold comparison of every derived table** and asserts equality. It
skips overlay delta export, navigation and full scanner queries, so the spike's
cold-correctness fallback cannot disguise an incremental mismatch. The first
unmodified case-1 run reproduced all three differences, but its subsequent full
scanner phase exceeded a 30-minute command deadline; the ten-case comparisons
use the rows-only mode and a longer command deadline instead. Permanent dispatch
and dead-code projection proofs run with the materialization unit tests.

## Verification

All Cargo gates ran on Linux with four requested vCPUs and Rust/Cargo 1.99.0:

- `cargo fmt --all -- --check`: exit 0; rustfmt 1.10.0-stable.
- `CARGO_PROFILE_STAGE_DEBUG_ASSERTIONS=true cargo check --profile stage -p agent-file-tools --tests`:
  all package test targets checked, `Finished` line, exit 0.
- `CARGO_PROFILE_STAGE_DEBUG_ASSERTIONS=true cargo test --profile stage -p agent-file-tools --lib views:: -- --nocapture`:
  232 passed, four ignored offline/child-only tests, zero failed.
- After restoring both mutations,
  `CARGO_PROFILE_STAGE_DEBUG_ASSERTIONS=true cargo test --profile stage -p agent-file-tools --lib views::materialization -- --nocapture`:
  55 passed, zero failed.

Before the fix, the resolved-last, unresolved-last and natural-macro parity tests
failed with incorrect ref statuses/targets and edge targets, matching the
baseline differences classified above. Mutation
proofs subsequently restored the unsafe `if same` skip: the macro,
resolved-last, unresolved-last and mixed-reference-kind tests each failed in a
separate exact-filter run (one failure and no other tests run per control).
Restoring materialization version 9 independently failed
`colliding_relink_legacy_generation_is_rebuilt_even_without_manifest_changes`.
Each temporary source mutation was marked `NON-VACUITY BREAK` to identify the
intentional restoration of incorrect behavior used to prove the tests fail. The
mutations ran against staged live source and were restored with an empty
`git diff --stat` before the final green gate.

Editor diagnostics remained partial (`rust-analyzer` reported its cargo check
running); the explicit package check above is the authoritative typecheck. No
TypeScript, manifest, lockfile, search-ranking, resolver-policy or tool-schema
changes were made.
