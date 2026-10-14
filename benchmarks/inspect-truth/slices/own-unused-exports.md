# Own-repository unused-export review

## Scope and result

Every one of the **104** newly scoreable findings in the repaired-source capture
was reviewed: 51 names under `packages/opencode-plugin/src/` and 53 under
`packages/pi-plugin/src/`. The capture's source is
`972d87b8157db1f5b23141ae47f7a2c85371873a`; the queued benchmark-driver syntax
repair is present, and that driver is not excluded. No plugin implementation has
changed since that capture.

`judgments/own-unused-exports.json` records one nonempty, single-line source reason
per exact `(path,symbol)` finding. All 104 findings are **aft_correct for
unused_exports**. All 104 also match Fallow 2.88.3 rows, but agreement was not
used as a substitute for reading declarations, same-file callers, cross-module
references and package export maps. No false-positive class was found in this
set, so there is no generic product repair or symbol suppression to add.

The score remains AFT 104, oracle 162, agreement 104, precision **1.000**, recall
**0.642**. Review coverage changes from 0 judged/104 unjudged to **104 judged/0
unjudged**. No AFT finding or oracle row was removed. The 58 oracle-only rows are
not reviewed in this slice. The other corpus accuracy cells remain unchanged:
this follow-up changes judgments and a test fixture, not runtime analysis.

## What the findings mean

An unused export is an exported **name without a downstream consumer**, not
necessarily a dead function or value. Most reviewed names are live implementation
helpers called inside their own file. The package export maps do not expose
those source modules as public subpaths: OpenCode publishes its root, server and
TUI entries; Pi publishes its root entry. Their actual entry-point imports do
not publish the reviewed helper names.

| Group | Concrete source evidence | Judgment |
|---|---|---|
| Config constants, schemas and resolvers | OpenCode `config.ts` uses its timeout/wait/watch defaults in the same file; Pi's separate constants likewise feed its own clamps/resolvers. Inferred `GithubConfig`, `OpenCodeHostConfig` and `PiToolPresentation` aliases have no named consumer. | The export names are unconsumed; local implementation liveness does not require an export keyword. |
| Live-reload/error/status/workflow infrastructure | The key lists and helper functions feed same-file wrappers; callers import those wrappers. Same-named OpenCode/Pi/Rust declarations are distinct implementations, not imports of each other. | Correct unused exported names. |
| Pi section builders and render helpers | AST, conflicts, imports, inspect, navigation, reading, safety and semantic renderers call their builders locally. Renderer tests import wrappers, not the reviewed builder names. Bash companion factories are registered inside their own module. | Correct unused exported names, not dead tool implementations. |
| Unreferenced setters/constants/collapse helpers | `setActiveSessionId`, `PI_REGISTRABLE_TOOLS`, and both `collapseInspectPhases` helpers have no importing consumer; the collapse helpers also lack callers. | Correct findings. |

Two apparently contrary reference patterns were checked explicitly rather than
being declared false positives:

### A consumed alias does not consume the primary export name

Both `lsp-github-install.ts` files declare the primary exported name
`precheckArchiveContents`, call it locally during extraction, and separately
export `_precheckArchiveSizeForTesting`. Tests import the **alias**, not the
primary name (OpenCode `:1560`, Pi `:1507`; both security tests import the alias
at `src/__tests__/e2e/lsp-github-install-security.test.ts:8`). The helper and alias
must remain available, but the primary export keyword remains unnecessary.
This is not missed test usage of the reported name.

The new integration fixture
`consumed_local_alias_does_not_consume_an_unimported_primary_export_name` checks
that exact distinction: the primary name has only same-file references while
the separately exported alias has an importing consumer. The analyzer's internal
`Used` verdict includes same-file binding liveness; unused-export classification
also uses `only_same_file_references`. The fixture does not assert that the
function body is dead or that the consumed alias is unused. During fixture
construction, assertions incorrectly conflating internal `Used` with consumed
export status and assuming a particular test-only reference list were corrected;
no existing test or product behavior was changed to make it pass.

### A private forwarding barrel is not an end consumer

`RpcSession`, `IndexProgress` and `HostToolOverlapNotice` in
`opencode-plugin/src/tui/aft-rpc.ts` are live values inside `AftRpc` and its derived
types (`:36-52`). `src/rpc/contract.ts:1-9` forwards their names, but downstream
users select `AftRpc` and the derived types, not those three constant names.
The barrel is not a published package subpath. Its unused forwarding exports
therefore do not consume the canonical names; removing an unused forwarding
chain is different from deleting the live schema values.

This interpretation is already pinned by
`inspect_unused_exports_oxc_reexport_alias_reports_canonical_once_with_context`:
an unconsumed private forwarder is not reported twice, and the canonical source
export is reported with the forwarding context. The existing private-barrel Oxc
fixtures retain the same contract. Treating a re-export alone as liveness would
undo those claims and mask genuinely unused export chains.

## Verification and retained evidence

The scorer validates every new judgment against the original recorded AFT row;
an additional `(path,symbol)` identity check verifies every reviewed name against
Fallow's findings generated after the benchmark driver's syntax repair. Fallow
itself was not modified. The rescored unused-export bucket reports 104 judged, zero
unjudged, and unchanged 1.000/0.642 precision/recall. The original capture and
previous judgment descriptors are preserved.

Relevant Rust gates run on Linux with four vCPUs; Cargo is 1.99.0. The Oxc engine
suite, including the new alias fixture, passed 52 tests with one manual benchmark
ignored. The unused-export scanner suite passed 11 tests, and the canonical
forwarder regression passed its one focused test. The Python scoring/harness
suite passed 39 tests (Python 3.14.4); rustfmt 1.10.0-stable returned exit 0 for
`cargo fmt --all -- --check`. Repository lint verifies the report additions. No fresh
product-binary corpus run is needed for a review-only change; no runtime code,
package manifest, lockfile or scoring policy changed.

Biome 2.4.7 checked 721 files with `bun run lint`. Scoped diagnostics for the
modified Rust fixture were authoritative for 1/1 file, with zero errors or
warnings; the checkout's unrelated Tier-2 graph was still unavailable.

Retained outside build directories:

- `local-ignore/truth-followups/export-review/score.json`, SHA-256
  `a7b0db697e4d75c7bd48ca94024026eb7222173a24afa9c32ef37489737d0620`;
- the copied original raw record/oracle and path-relocated judgment descriptors
  under that directory;
- `judgments/own-unused-exports.json`, SHA-256
  `a1d2d2b2fc03b02b997d3f5f7915ca3b45f0bb64898ab4c2d2ff6eb632f72df9`.

Own-repository review now covers all 33 original Rust and 10 original in-domain
TypeScript dead-code findings, plus these 104 unused-export findings. Broader
campaign checks remain open: scoped TypeORM inspect still exceeds five seconds,
soft-serve function precision is 0.346 against a 0.6 target, and the seven-corpus
diagnostics/footer comparison remains unrun. See [the campaign report](../REPORT.md).
