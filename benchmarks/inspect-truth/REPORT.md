# aft_inspect truth at scale: campaign report

## Negotiated Python document pulls: implemented and measured

The scoped Python authority path now uses the document diagnostic protocol
established in the source investigation below. A successful response certifies
**that file**, not a whole workspace. An empty startup push, a hover response or
the passage of two seconds cannot substitute for a negotiated document pull.
Non-negotiated servers retain the separately disclosed progress/push fallback.

### Registration, coverage and warm feedback

- Pyright/basedpyright clients advertise dynamic document diagnostics unless
  `disablePullDiagnostics` is true. Registrations are server-instance scoped,
  match the document selector, carry the negotiated identifier and are removed
  on unregistration. Static providers remain independent.
- Scoped inspection opens all selected document bodies before sending analysis
  requests. It also notices registration arriving after didOpen. Background
  pulls pause while the scoped sweep owns that transport, preventing a large
  later didOpen from blocking behind an earlier expensive analysis request.
- Full and valid unchanged responses supply file authority. The unchanged cache
  is the exact previous **pull** report sent with the request, not an intervening
  unsolicited push. Replies for changed documents or replaced server instances
  are rejected. Failed/cancelled/rejected requests and missing unchanged caches
  leave coverage unknown; they do not certify startup E0.
- Normal opens/changes, registration and `workspace/diagnostic/refresh` queue
  background pulls of open files. Accepted pull snapshots enter the same
  version-checked alert path as pushes, so users need not invoke inspect to keep
  warm counts and alert episodes current. No workspace diagnostic request or
  unopened-neighbor analysis was introduced.
- Producer details report `authority=document_pull` and
  `no_progress_grace_ms=0`; the prose now describes a file response rather than
  the fallback timing rule. The fallback still requires the server's
  analysis-progress end event (`$/progress`, `kind: end`) after publication
  for an announced/begun run.

### Real protocol evidence

Both the direct protocol driver and AFT were exercised against Pyright **1.1.414**
and basedpyright **1.40.2**, with isolated installations (basedpyright also ships
`pyright` command names). Both initialize results omit diagnosticProvider and
then dynamically register `textDocument/diagnostic` with the correct identifier.
A direct open and unchanged reopen each returned one assignment error. Pyright
sent no pushes in that interval; basedpyright sent an empty close-time push,
which was not used as diagnostic authority.

AFT's real-server captures each returned **E1, coverage 1/1, document_pull, zero
grace**, both normally and with an explicitly marked startup empty push injected
by the tracing proxy. Selected wire events from the injected runs:

```text
Pyright:
1792591025.2862737 didOpen broken.py
1792591025.2868865 injected publishDiagnostics []
1792591025.7459648 textDocument/diagnostic id=3
1792591025.9207008 response id=3 kind=full items=1
basedpyright:
1792591033.9986312 didOpen broken.py
1792591033.9987884 injected publishDiagnostics []
1792591034.5320425 textDocument/diagnostic id=3
1792591034.5607530 response id=3 kind=full items=1
```

Raw frames, including registration acknowledgements and request identifiers, are
retained under `.cortexkit/internal-docs/investigations/inspect-document-pull/raw/`.
The traced binary hash is
`06779918d6ce61c5820d7ee39336906939e76eb7804616a08e22b5fa383a4aa1`;
the final binary below additionally corrects the authority explanation, without
changing negotiation or certificate behavior.

### Final FastAPI diagnostics: three fresh processes

Same source pin, dependency freeze and seeded **52-file** domain as the
[progress-liveness captures](#progress-liveness-and-warm-feedback-follow-up)
(retained as `inspect-progress-liveness/raw/fastapi-{1,2,3}/results.json` in the
investigation directory): FastAPI `26dbe0b00e1211b99446500fc6e402cfec6a3b99`, Python
**3.14.4**, Pyright **1.1.414**, Pydantic **2.14.1**, Starlette **1.9.0**.

| Fresh process | AFT E/W | Reference E/W | Agreements | AFT-only / reference-only | Authoritative files | Authority |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| 1 | 62 | 62 | 62 | 0 / 0 | 52/52 | document_pull |
| 2 | 62 | 62 | 62 | 0 / 0 | 52/52 | document_pull |
| 3 | 62 | 62 | 62 | 0 / 0 | 52/52 | document_pull |

The collector issues three scoped diagnostic requests for batches of 20, 20 and
12 files. Each reply has full file coverage (20/20, 20/20, 12/12 authoritative
files/examined files), no Python gaps, and zero timing grace. Diagnostic totals
by reply remain **14, 11, 37** (61 errors and one
warning overall). The collector still exits **1** for named project-wide
Bash/TypeScript/YAML producer gaps; these are not Python-sample incompleteness.
The historical reference of 50 is not silently replaced: its complete dependency
freeze was unavailable, as documented in the original follow-up below. These
new same-environment triples establish stable observed counts, not equivalence
to that older environment.

Canonical records are `raw/certified-fastapi-{1,2,3}/results.json`; executable
SHA-256 is `917049cf6a486305421b7e888bfdb8d699ff72e487a566ebf2d9ce7abdffa1d7`.
The executable was built from the reviewed working source before the delivery
commit, not claimed to embed that later SHA. The original twelve-row Rust
judgment/ablation table below remains the same-source D12-to-D0 measurement;
this protocol follow-up does not change liveness scoring or exclusions.

### Regression and mutation evidence

- Without pull negotiation,
  `scoped_python_dynamic_pull_replaces_startup_empty_push_with_real_error`
  fails with **FRESH/complete E0, expected E1**. The disabled-pull compatibility
  test and Rust push-only control both remain green.
- Ignoring server diagnostic refresh makes only
  `python_dynamic_pull_refresh_updates_warm_counts_and_alerts_without_inspect`
  fail: **server refresh did not drive a clean open-file pull**. Initial warm
  status/alert population and scoped-pull controls remain green.
- Allowing unchanged responses to borrow an unsolicited startup push makes only
  `unchanged_pull_cannot_certify_a_startup_push_cache` fail: expected
  **RequestFailed(no_cache_for_unchanged)**. The intervening-push/request-cache
  sibling passes.
- Each deliberate production-code mutation tested that removing a repair makes
  its regression fail. The mutation carried the `NON-VACUITY BREAK` marker, was
  applied after staging the live state, recorded with a non-empty diff, and
  restored with checkout/touch
  to an empty unstaged diff. No break remains. Logs and exact diff pairs are in
  the investigation's `verification/` directory.
- Eight dynamic-pull integration cases cover registration, delayed registration,
  large document transport ordering, opt-out, unregistration, warm feedback,
  dependency refresh and failure honesty. Error, method-not-found, cancellation
  and missing-cache unchanged replies all remain incomplete/unknown.
- Final Linux gates (serialized, eight allocated CPUs): LSP units **310 passed,
  3 existing ignored**; diagnostics-category units **24**; Rust dead-code scanner
  units **70**; own published-view fixture **1**; Python progress/pull
  integrations **21**. `cargo check -p agent-file-tools --tests` finished and
  `cargo fmt --all -- --check` exited 0. Cargo/rustc **1.99.0**, rustfmt
  **1.10.0-stable**. Existing scoped blocking integrations **13**, status-cache
  units **11**, and alert target **18** also passed during this follow-up.
- Benchmark helper regressions: `test_diagnostics.py` **7 passed**,
  `test_score.py` **39 passed**; Python **3.14.4**. Bridge build (`tsc`, TypeScript
  **5.9.3**) passed; `bun run lint` checked **751 files** with Biome **2.4.7**.
- Tool diagnostics were PARTIAL because worker rust-analyzer capacity was
  exhausted and compiler checks are disabled there; the compiler gates above
  are the authoritative checks. No Windows/macOS runtime claim is made.

## Python document-pull protocol: pre-build source findings

Both checked releases implement **per-document pull diagnostics**: Pyright
**1.1.414**, commit `a28cce4077d87b80bead12de809f79845e39c8ba`, and the published
latest basedpyright **1.40.2**, commit
`344759687c2fff3bd0a7d6a83a9d1847f806b795`. No build or test was needed to
establish these protocol facts; this section records the source decision before
the implementation build.

### Registration is dynamic, not an initialize-result diagnosticProvider

- Pyright [`languageServerBase.ts:628-630`](https://github.com/microsoft/pyright/blob/a28cce4077d87b80bead12de809f79845e39c8ba/packages/pyright-internal/src/languageServerBase.ts#L628-L630)
  and basedpyright [lines 715-717](https://github.com/DetachHead/basedpyright/blob/344759687c2fff3bd0a7d6a83a9d1847f806b795/packages/pyright-internal/src/languageServerBase.ts#L715-L717)
  enable pull only if the client declares
  `textDocument.diagnostic.dynamicRegistration` and initialization option
  `disablePullDiagnostics` is not true.
- Their static initialize results do not contain `diagnosticProvider`. Instead,
  Pyright `languageServerBase.ts:693-703` and basedpyright `:801-811` install and
  register `PullDiagnosticsDynamicFeature`.
- Pyright [`pullDiagnosticsDynamicFeature.ts:41-49`](https://github.com/microsoft/pyright/blob/a28cce4077d87b80bead12de809f79845e39c8ba/packages/pyright-internal/src/languageService/pullDiagnosticsDynamicFeature.ts#L41-L49)
  and basedpyright [lines 38-48](https://github.com/DetachHead/basedpyright/blob/344759687c2fff3bd0a7d6a83a9d1847f806b795/packages/pyright-internal/src/languageService/pullDiagnosticsDynamicFeature.ts#L38-L48)
  register `textDocument/diagnostic` through `client/registerCapability`, with
  `interFileDependencies: true`. `workspaceDiagnostics` is conditional on
  workspace diagnostic mode; it is not a prerequisite for a document pull.
- At the source decision, AFT advertised `dynamicRegistration: false`, opting the
  client out of dynamic registration, and discovered only static diagnosticProvider.
  The missing static initialize field therefore did not establish that either
  server lacks pull support.

The document handlers are bound at Pyright `languageServerBase.ts:561` and
basedpyright `:654`. Pyright [lines 1156-1230](https://github.com/microsoft/pyright/blob/a28cce4077d87b80bead12de809f79845e39c8ba/packages/pyright-internal/src/languageServerBase.ts#L1156-L1230)
and basedpyright [lines 1559-1639](https://github.com/DetachHead/basedpyright/blob/344759687c2fff3bd0a7d6a83a9d1847f806b795/packages/pyright-internal/src/languageServerBase.ts#L1559-L1639)
await `analyzeFileAndGetDiagnostics` for the requested URI and repeat if its
diagnostic version changes during analysis. The underlying Program explicitly
analyzes the file and retrieves its full-range diagnostics (Pyright
`analyzer/program.ts:827-835`; basedpyright `:873-881`). This is a file certificate,
not a claim that unrelated files or the whole workspace have been checked.
The scoped client must open the file first: in open-files-only mode the handler
intentionally returns an empty full report for a closed document. The handlers
also have early-return guards for cancellation and files inside ZIP archives
(`canNavigateToFile`), and Pyright
additionally guards disabled language services. Client timeout, cancellation,
missing-cache and rejected-request outcomes must remain failure/unknown rather
than being converted to successful reports containing zero diagnostics.

### Negotiating pull stops ordinary open-file push analysis

Pushes **cannot remain the assumed source for warm counts or alerts** after this
negotiation. Pyright [`createAnalyzerService():313-330`](https://github.com/microsoft/pyright/blob/a28cce4077d87b80bead12de809f79845e39c8ba/packages/pyright-internal/src/languageServerBase.ts#L313-L330)
and basedpyright [lines 353-370](https://github.com/DetachHead/basedpyright/blob/344759687c2fff3bd0a7d6a83a9d1847f806b795/packages/pyright-internal/src/languageServerBase.ts#L353-L370)
request `workspace/diagnostic/refresh` on invalidation in open-files-only pull
mode, and their `shouldRunAnalysis` callback disables ordinary background
analysis when pull is supported unless a workspace diagnostic request is active.

This does not mean the publishDiagnostics protocol handler is removed. Their send routines still
call `connection.sendDiagnostics` when not streaming a workspace result
(Pyright `:1506-1522`; basedpyright `:2075-2098`). Those notifications cannot
replace the missing normal open-file analysis stream.

The selected implementation path is therefore negotiated document pulls for
exactly the scoped files, and pull-driven refresh of open files for warm counts
and alert observations. AFT will honor `disablePullDiagnostics`, issue no
unregistered document request, retain push/status compatibility and the progress
fence for non-negotiated servers, and remove the two-second certificate only
where document pull is actually in effect. Specifically, scoped file authority
will no longer wait two seconds hoping that no further analysis is announced.
It will await the file analysis requested by `textDocument/diagnostic`, rather
than treating a hover/documentSymbol round trip as a proxy for diagnostic completion.

## Progress liveness and warm-feedback follow-up

The END-after-publication barrier is retained for an **announced or begun**
analysis run. It is not a requirement that Pyright start a run on every open.
The earlier implementation incorrectly treated historical progress support as
such a requirement; a later small analysis could leave the whole producer pending.

### Real Pyright 1.1.414: all three idle-open cases can send no progress

The pinned source is still `a28cce4077d87b80bead12de809f79845e39c8ba`.
The real-server driver first opens three actual FastAPI source files to establish
observed BEGIN/END support, waits for completion, then exercises each case in a
small fixture workspace. Every event below is from that same process. URI paths
are reduced to their fixture basename; the full frames and version-0 document
contents are retained in the investigation artifacts.

```text
1792568426.0213230  window/workDoneProgress/create
1792568428.5428643  $/progress begin
1792568428.5430865  $/progress end

1792568439.6000087  didOpen excluded.py version=0
1792568439.6088226  publishDiagnostics excluded.py version=0 diagnostics=1

1792568446.6165123  didOpen notes.txt version=0 languageId=plaintext
1792568446.6254098  publishDiagnostics notes.txt version=0 diagnostics=6

1792568460.6700075  didOpen reopen.py version=0
1792568460.6772976  publishDiagnostics reopen.py version=0 diagnostics=1
1792568464.6821284  didClose reopen.py
1792568467.6866434  didOpen reopen.py version=0 (unchanged bytes)
1792568467.6930757  publishDiagnostics reopen.py version=0 diagnostics=1

1792568474.6985430  didOpen first.py version=0
1792568474.7059670  publishDiagnostics first.py version=0 diagnostics=1
1792568478.7083395  didOpen second.py version=0 (first file remains open)
1792568478.7155580  publishDiagnostics second.py version=0 diagnostics=1
```

**No BEGIN, END, or progress-create request occurs in any of those small-file
stages**, including the unchanged reopen and second-after-idle stages. The driver
captures at least four seconds per stage and waits for protocol quiet; the source
explains why no END is owed, rather than interpreting a short wait as completion.
The `.txt` case was repeated with `languageId=python` with the same six diagnostics
and no progress. The excluded file is explicitly listed in `pyrightconfig.json`.

Both `reopen.py` opens have text SHA-256
`285363e2d3f909e9683d1c83781a42bad62ee60aa5894cd731d4a4309e2766c6`.
The warmup also demonstrates why an announcement matters: CREATE precedes BEGIN
by about 2.52 seconds, longer than the no-run grace.

This is workload-dependent, not a rule that excluded files never cause progress:

- [`languageServerBase.ts:1096-1118`](https://github.com/microsoft/pyright/blob/a28cce4077d87b80bead12de809f79845e39c8ba/packages/pyright-internal/src/languageServerBase.ts#L1096-L1118)
  forwards an explicit open to the containing workspaces and constructs a Python
  document even if the client supplied another language ID.
- [`program.ts:458-505`](https://github.com/microsoft/pyright/blob/a28cce4077d87b80bead12de809f79845e39c8ba/packages/pyright-internal/src/analyzer/program.ts#L458-L505)
  opens untracked/excluded files as `isOpenByClient`; [lines 1380-1388](https://github.com/microsoft/pyright/blob/a28cce4077d87b80bead12de809f79845e39c8ba/packages/pyright-internal/src/analyzer/program.ts#L1380-L1388)
  keep explicitly opened files needed for analysis. Reopening resets the
  diagnostic version to force a report, even if content did not change.
- [`sourceFile.ts:585-614`](https://github.com/microsoft/pyright/blob/a28cce4077d87b80bead12de809f79845e39c8ba/packages/pyright-internal/src/analyzer/sourceFile.ts#L585-L614)
  avoids marking unchanged content dirty; `service.ts:360-372` still schedules
  reanalysis for the open.
- `languageServerBase.ts:1352-1373` sends each batch's computed diagnostics before
  reporting its remaining-file count. At [lines 1396-1413](https://github.com/microsoft/pyright/blob/a28cce4077d87b80bead12de809f79845e39c8ba/packages/pyright-internal/src/languageServerBase.ts#L1396-L1413),
  a zero count calls `end()` without first calling `begin()`.
  [`ProgressReportTracker.end():53-60`](https://github.com/microsoft/pyright/blob/a28cce4077d87b80bead12de809f79845e39c8ba/packages/pyright-internal/src/common/progressReporter.ts#L53-L60)
  is a no-op if progress was never displaying. A batch that finishes all its work
  can therefore publish diagnostics with **no progress messages at all**.

### Liveness rule and negative controls

AFT now distinguishes an open/sync grace from an actual run. A post-input
publication may settle after the two-second **publication** grace when no run
was announced or begun. This works even after an earlier run completed. The
producer details disclose `post_input_publish_no_analysis_run`; unsupported
progress retains its separately named first-publication rule. There is no
production sleep or request-budget increase.

The proof remains per file: its current epoch and publication time come from
that exact `(server, file)` entry. A local input epoch excludes older cached
reports; dependent-file publications from the same short batch can also settle.
Opens are removed from arming state on close. After two seconds, a file with no
publication no longer keeps the producer pending, but that file stays uncovered; it
cannot reuse its cached pre-open report or poison the producer forever.

`window/workDoneProgress/create` is an analysis announcement for this Pyright
implementation (`server.ts:277-322` creates it inside `begin()`). It blocks the
no-run route even before BEGIN arrives. Every announced/begun run still needs an
END **after** the publication it certifies. Tests still reject an END before the
publication, a new BEGIN without its own END, a run without END, and a startup-empty
report treated as final. The early-END
fake now places END before the file's **first** publication, so an earlier empty
publication cannot accidentally satisfy the ordering being tested.

Before this follow-up, all four new fake-server scenarios failed with
`errors: null`, `complete: false`, `analysis_progress_pending` despite a valid
post-open report: excluded file, routed non-source file, unchanged reopen, and
second file after idle. The six original progress tests passed in that red run.
All scenarios now settle, and the explicit silent-open control remains an honest
file gap while `server_is_warming` and producer settlement recover.

### Status bar and alerts: keep history separate from fresh authority

The status bar did churn: the before-fix integrated test obtained E1/W1, then
`(errors, warnings) = (None, None)` during reanalysis. The count cache attempted to
retain prior values, but the still-provisional producer prevented the bar from
displaying them, replacing the numbers with unknown E?/W? until completion.
The alert engines did **not** legitimately close episodes from
that demotion: `live_publish_drop_reason` excludes provisional publications,
`alert_state.rs` mutates only accepted observations, and `alert_render.rs` removes
a rendered identity only on an authoritative snapshot that omits it. Absence from
an observation batch is not an authoritative empty snapshot.

The repair no longer demotes every certified store entry on an open/run. Old
certificates remain warm snapshots; **replacement publications** are provisional.
Fresh inspect separately gates producer state (and the open document's version),
so old snapshots cannot claim fresh scoped authority while a run can affect them.
Warm status can retain an actually certified E/W pair through that interval;
first startup, missing/failed producers and unproven values remain unknown.
The status cache now stores absent values for unavailable diagnostic counts and
includes producer authority in its cache identity, so discovering the producer
set cannot reuse a zero that was previously hidden as unknown. That additional
regression was red with `(Some(0), Some(0))`
where `(None, None)` was required.

Cross-file effects are real, so fresh authority cannot be inferred from just the
file being opened/changed. The same real-server trace also records:

```text
1792568482.7264745  publishDiagnostics consumer.py version=0 diagnostics=0
1792568486.7368786  publishDiagnostics dependency.py version=0 diagnostics=0
1792568490.7392557  didChange dependency.py version=1
1792568490.9966006  publishDiagnostics consumer.py version=0 diagnostics=1
1792568490.9969873  publishDiagnostics dependency.py version=1 diagnostics=0
```

Only the dependency changed from an `int` return to `str`; the still-open consumer
gained an error. This small batch also emitted no progress. Pyright's
`backgroundAnalysisProgram.ts:128-131` marks changed files dirty and
`program.ts:2546-2562` recursively reanalyzes their `importedBy` dependents.

Certification now delivers the deferred, document-version-checked replacement to
alert ingestion exactly once, rather than dropping the publication while warming
and never forwarding it later. The integrated regression drives actual LSP drain
snapshots through `AcceptedObservationBatch`, `AlertDeltaState` and `AlertEngine`:
E1/W1 survives pre-publication, watcher-stale and startup-empty phases; no pending
empty snapshot is accepted; after END, the unchanged error has the same episode
ID and produces no repeated reminder. Failure and unversioned-report exclusions
are unchanged. This is not a new project-wide or unopened-file diagnostic claim.

### Follow-up gates and repeat capture

On `linux,8c`, the affected gates pass: **307 LSP unit tests** (three existing
manual/performance tests ignored), **24 diagnostics-category tests**, **11 status
cache tests**, **9 renderer tests** (one existing performance probe ignored),
**3 alert-state tests**, **13 Python fake-server integrations**, **13 existing
blocking-scope integrations**, and **18 alert integration tests**. Package test
compilation and format checking also pass. The delayed-announcement fixture uses
a 30-second test budget: inspect reserves five seconds and allocates half the
remaining work time to a phase, so an 8-second value (clamped to 10) cannot fit
the injected 2.5-second announcement delay plus the fake's 0.6-second run. No
production budget was changed to make that test pass.

Three new fresh FastAPI processes after these repairs remain **62 AFT / 62
reference / 62 agreements / zero differences**, all 52 sampled files complete,
with 14/11/37 E/W diagnostics per batch. All pages use `work_done_progress` for
this larger workload. The executable SHA-256 is
`10734aa7ea2f8ef51f43379e700afcbf0bba1c70951a5a1faa6776d38f35f3a8`.
The collector still exits 1 for the unchanged missing project Bash/YAML producers,
not for Python sample incompleteness. Source pins and primary tool/dependency
versions are unchanged from the preceding capture.

The full traces, fixture/config bytes, failing-before outputs and final gates
are retained outside `target/` under
`.cortexkit/internal-docs/investigations/inspect-progress-liveness/`; the real-server
driver is retained under `.cortexkit/scripts/inspect-progress-liveness/`.

## Accuracy follow-up: the twelve Rust rows and Python startup authority

This follow-up judges **all twelve** rows in the earlier capture's
`items.dead_code` Rust bucket. All twelve are false positives; none was removed
with an exclusion, public-API blanket root, oracle adjustment or scoring change.
The nine previously unjudged rows are now recorded individually in
`judgments/diagnostics-follow-up.json`. Older sections below retain their
original numbers and acceptance boundaries.

### Individual Rust decisions

Line numbers below identify the original rows. The symbols and production
callers still exist in this checkout; no row was explained away as source drift.

| Original path under `crates/aft/src/` | Symbol / original line | Judgment and production evidence |
|---|---|---|
| `db/file_identity.rs` | `is_live_sqlite_handle` / 627 | **False positive.** `commands/file_read_exact.rs:75,144` calls the guard. The production chain reaches it through the private `read_as_parent` call inside the `flow_read.rs:363` macro body, then `flow_read.rs:496`. Losing that private macro hop disconnected the guard; this was not a missing blanket `Drop` root. |
| `edit.rs` | `link_count` / 452 | **False positive.** The Windows `source_replacement::may_keep_access` branch calls `windows::link_count` at :339; existing-file writes use `may_keep_access` at :97-99. |
| `edit.rs` | `copy_platform_security` / 660 | **False positive.** `copy_access` calls `xattrs::copy_platform_security` at :384. Linux and macOS definitions are alternative live implementations, not unused duplicate declarations. |
| `edit.rs` | `platform_acls_match` / 678 | **False positive.** The shared Linux/macOS `copy_access` branch verifies ACLs through this helper at :396; the macOS definition compares both ACLs at :684. |
| `edit.rs` | `extended_acl_text` / 705 | **False positive.** Production ACL comparison calls it twice at :684; ACL removal also calls it at :747,766. Its additional test callers do not make it test-only. |
| `edit.rs` | `clear_extended_acl` / 743 | **False positive.** The macOS `source_recovery::make_private` branch calls it at :1014 before a recovery copy is written. Other platform definitions of `make_private` must not replace that source body. |
| `edit.rs` | `copy_protected` / 857 | **False positive.** Windows replacement and recovery paths call it at :410 and :1028 to protect their DACLs. A Linux compiler cannot judge those Windows paths dead. |
| `subc/scope_describe.rs` | `describe_answer_ends_scope` / 28 | **False positive.** The running scope worker passes it to `is_ok_and` inside `tokio::select!` at :187, before revoking an ended scope. This is a function-item argument inside opaque macro tokens. |
| `subc/tool_provider.rs` | `broca_text` / 377 | **False positive.** `render_catalog` calls it for `CatalogPreset::Head` at :598-611; the production catalog route reaches that renderer. |
| `subc/tool_provider.rs` | `worker_text` / 394 | **False positive.** `render_catalog` calls it for the accepted Worker preset at :598-611. |
| `subc/tool_provider.rs` | `reader_text` / 412 | **False positive.** `render_catalog` calls it for the accepted Reader preset at :598-611. |
| `subc/tool_provider.rs` | `catalog_for_confined_request` / 525 | **False positive.** `subc/remote_policy.rs:195-199` selects this function in an `if` branch, then invokes the selected `render` value at :200-207. Dropping that value edge also orphaned the three text helpers above. |

The three extra names in an initial extraction were **not** D12 rows:
`temp_cleanup.rs::{ready,failed}` and `callgraph_maintenance.rs::pending` were in
`project_summary.dead_code.test_only_top` / `side_lists.dead_code_test_only`.
They also have production callers (`bash_background/registry.rs:8887,8893` and
`context.rs:8581-8582`). That separate test-only attribution concern is recorded
separately; these three names are not part of the twelve false-positive rows above.

### Four causes, each isolated rather than inferred from the count

The scanner now retains local private declaration identities for macro hops;
joins inline source identities to the existing short-name liveness nodes;
retains calls from **every source cfg variant** when a callgraph node has only
one body; and follows function-valued branch tails and unshadowed macro callback
arguments. The facts version advances from 7 to 8 so cached old facts cannot
preserve the false positives. Function-value edges are liveness facts, not
fabricated direct calls in callgraph navigation. Ordinary unreachable functions,
unreachable macro callers, shadowed callback names and private library APIs keep
their existing tests and verdicts.

A same-source, full Rust-domain store-backed capture covers **961 Rust files**,
including the normal test/fixture classification. Substituting the original
scanner from the imported prerequisite's HEAD reproduces **the exact D12 row
identities above**. Restoring the candidate gives **D0**. The unchanged all-target
Rust compiler reference reports **dead_code 0** (585 compiler artifacts).
The final comparison runs prebuilt driver binaries after moving the temporary
driver source out of the product tree. Earlier ablation side captures include
that one driver (962 files); its presence changes none of the D12 identities.
Individually neutralizing the four repairs, with the other repairs left live,
restores exactly these own rows:

| Neutralized cause | Own Rust rows restored | Minimal regression / red |
|---|---:|---|
| Private macro intermediate declarations | 1: SQLite handle guard | `own_accuracy_private_macro_hop_keeps_handle_guard_live`: `is_live, unused` instead of `unused` |
| Alternate cfg bodies and their lexical paths | 6: the six `edit.rs` helpers | `own_accuracy_cfg_variants_preserve_nested_and_parent_module_calls`: `verify, leaf, links, unused` instead of `unused` |
| Function-valued conditional branch tails | 4: catalog renderer and its three text helpers | `own_accuracy_selected_function_item_keeps_catalog_renderers_live`: `normal, confined, unused` instead of `unused` |
| Macro callback arguments | 1: scope-ended predicate | `own_accuracy_macro_callback_is_live_but_unused_sibling_is_dead`: `ends_scope, unused` instead of `unused` |

Every deliberately broken control used as proof is marked `NON-VACUITY BREAK`
only while applied, runs against a
staged live-state backup, and is restored with an empty unstaged diff. The named
regression is the only failing test in each selected run; ordinary private-call,
single-variant, unused-function or bound-name sibling controls remain green.
The real source shapes are also checked through a published checkout-view
fixture, not only a hand-built callgraph snapshot.

This Rust-domain capture is not a claim of a completed project-wide live-view
benchmark: the standalone own-checkout collector stayed `callgraph_building` and
its log records a 600-second Tier-2 timeout. The direct production scanner and
store projection establish the Rust liveness numbers; the published-view fixture
checks that projection boundary. No scoped-timing or all-language acceptance
claim is added.

### Why FastAPI changed from 38 to 22

The problem is **not** that scoped inspect forgot to wait for a publication from
each file. It already does that. Pyright can publish version-0 **empty startup
reports before background analysis completes**; AFT accepted those as final and
closed the sampled files immediately. A real trace reproduced twenty `didOpen`
messages at 1792547289.425-.475, twenty empty reports at 1792547290.498-.499, then the first
`didClose` at 1792547290.523. That first batch was marked authoritative for
20/20 files with E0. An unmodified same-environment collector observed 34/62;
another traced process observed 0/11/37 E/W diagnostics in its three batches. These are reproductions of the
observation defect, not claims to recreate the old environment's exact 38/22.

AFT now advertises `window.workDoneProgress` for Python and basedpyright (Rust
already did), answers `window/workDoneProgress/create` through the existing
reader response path, and tracks overlapping server-owned progress tokens.
Authority requires both a post-open publication **and an analysis END after that
publication**. An END merely read after our `didOpen` is insufficient. The
diagnostics-store epoch at END preserves the server stream's publication order;
a later publication cannot borrow an earlier END. A run already active while
additional documents open can certify them only after their publications. A new
BEGIN keeps reports provisional until its own END. Budget exhaustion yields
`complete: false` with `analysis_progress_pending`, not a complete-and-clean
partial set. No production sleep or larger inspect budget was added.

This completion interpretation is restricted to Pyright/basedpyright, whose
analysis progress has the necessary meaning. In pinned Pyright **1.1.414**, commit
[`a28cce4077d87b80bead12de809f79845e39c8ba`](https://github.com/microsoft/pyright/commit/a28cce4077d87b80bead12de809f79845e39c8ba),
[`languageServerBase.ts:1352-1373`](https://github.com/microsoft/pyright/blob/a28cce4077d87b80bead12de809f79845e39c8ba/packages/pyright-internal/src/languageServerBase.ts#L1352-L1373)
sends computed diagnostics before updating progress from
`results.requiringAnalysisCount.files`; [lines 1396-1413](https://github.com/microsoft/pyright/blob/a28cce4077d87b80bead12de809f79845e39c8ba/packages/pyright-internal/src/languageServerBase.ts#L1396-L1413)
end progress when that remaining-files count is zero. Settings are unchanged
during these captures; the separate configuration-disable path is not used as
analysis completion evidence here.

For versions/servers that **never** report analysis progress, the documented
two-second negotiation grace retains first-publication authority. Once progress
support is observed, an unfinished run cannot fall back. The actual response
discloses each root's rule under
`by_producer.python.authority[]` (`work_done_progress`,
`analysis_progress_pending` or `first_publish_no_analysis_progress`), as well as
producer-labelled notes. This fallback is explicitly weaker, not a claim to
prove that an unsupported server finished background analysis.

The slow fake server first publishes empty version-0 diagnostics, then real
diagnostics at analysis completion. Before the fix,
`scoped_python_progress_waits_past_startup_empty_publication` returned **FRESH,
complete, E0, authoritative 1/1**, failing expected E1. The never-ending variant
also returned complete and failed
`scoped_python_progress_without_end_is_pending_not_clean`. Regressions additionally
cover a pre-existing run, END-before-publication, a second BEGIN, overlapping
tokens, and a server without progress support.

The additional ordering regressions were independently reddened and restored:

| Neutralized guarantee | Only failing integration test | Captured failure / surviving controls |
|---|---|---|
| END must follow the file's publication | `scoped_python_progress_end_before_post_open_publication_is_pending` | Returned FRESH/complete instead of pending; the other five progress tests passed. |
| Preserve a run already active at open | `scoped_python_progress_existing_run_certifies_later_opened_files` | Returned unknown E/W instead of E1; the other five passed. |
| Every new BEGIN needs its own END | `scoped_python_progress_new_begin_waits_for_its_own_end` | Returned the old error/warning pair instead of `test diagnostic after change`; the other five passed. |
| Do not settle at the startup publication | `scoped_python_progress_waits_past_startup_empty_publication` | Returned E0 instead of E1; the unsupported-server control passed. |
| A run with no END stays pending | `scoped_python_progress_without_end_is_pending_not_clean` | Returned FRESH/complete E0 instead of pending. The same applied control retained the unsupported-server case. |

An initial overly broad trial disabled the entire progress opt-in, also removing
the fallback-rule disclosure and reddening its control. It was discarded as
confounded, restored, and replaced by the startup-fence control above, which
preserves disclosure. That discarded trial is retained in the evidence, not
counted as a successful single-red mutation.

### Three fresh captures and reproducibility

All runs use `runon: linux,8c` (network only for provisioning), serialized Cargo,
FastAPI pin `26dbe0b00e1211b99446500fc6e402cfec6a3b99`, and the unchanged
diagnostics collector's seeded **52-file** sample in batches 20/20/12. The runner
cannot write `/srv/build/review`; the approved copy location is
`target/inspect-accuracy/`. Python **3.14.4**, Pyright **1.1.414**, Pydantic
**2.14.1**, Starlette **1.9.0**, Cargo/rustc **1.99.0**, Bun **1.4.2** and Node
**24.21.0** match the available earlier primary-version provenance. The full new
Python dependency freeze is retained. The earlier full transitive freeze is not
available: the reference is now **62**, not the historical **50**, so no accuracy
gain is claimed across those environments.

| Independent fresh process | AFT E/W | Reference E/W | Agreed | AFT-only / reference-only | Scoped coverage |
|---|---:|---:|---:|---:|---|
| 1 | 62 | 62 | 62 | 0 / 0 | 52/52, all three pages complete |
| 2 | 62 | 62 | 62 | 0 / 0 | 52/52, all three pages complete |
| 3 | 62 | 62 | 62 | 0 / 0 | 52/52, all three pages complete |

Each process reports **14/11/37 E/W diagnostics** in its three batches, using `work_done_progress` authority.
The tested debug executable SHA-256 is
`03fc2e9e1b567b9668de01c570d7af31885a59394daba5c3da6b13aff44ef5ed`.
The project footer still correctly shows unknown E/W for missing Bash/YAML
producers; each collector command exits 1 for that incomplete **project**
coverage, not for its complete Python sample. The seven-repository all-producer
gate remains unpassed.

Other registered producer IDs are not silently upgraded to this contract:

| Producer(s) | Analysis-progress evidence / behavior in this slice |
|---|---|
| `python` (Pyright), custom `basedpyright` | Pyright source and real traces establish the contract; enabled for the Pyright-derived protocol. Basedpyright uses the same mechanism, but has no separate installed-version corpus or custom-ID integration run here. |
| `rust` | Existing rust-analyzer server-status / compiler-check progress contract and fake-server regressions retained. |
| `typescript`, `typescript-native`, `go`, `bash`, `yaml`, `ty`, `oxlint`, `biome`, `dockerfile`, `vue`, `astro`, `svelte`, `prisma`, `terraform` | Whether their installed versions report **analysis** progress is unmeasured here; authority unchanged. Generic work-done support alone is not treated as an analysis guarantee. |
| `clojure-lsp`, `dart`, `elixir-ls`, `fsharp`, `gleam`, `haskell-language-server`, `jdtls`, `julials`, `nixd`, `ocaml-lsp`, `php-intelephense`, `ruby-lsp`, `sourcekit-lsp`, `csharp`, `razor`, `clangd`, `lua-ls`, `zls`, `tinymist`, `kotlin-ls`, `texlab`, other custom IDs | Analysis-progress semantics unmeasured; no behavior change. |

Verification includes the 70 scanner tests, published-view source fixture,
18 client and 27 diagnostics-store tests, six Python progress integrations and
13 existing blocking-scope integrations, plus all-target compiler checking and
format checking. Raw captures, source pins, dependency freeze, initial reds,
mutation outputs and restore evidence live outside regenerable build directories
under `.cortexkit/internal-docs/investigations/inspect-accuracy/`. No ranking
fence, tool arguments, dependency manifest or lockfile changed.

## Current follow-up: own checkout and seven-repository diagnostics

The latest measurement uses a Linux **release** build of
`dab88ab951f1216b494bff68f1952c16252fbea6` (AFT 0.61.0), on **eight allocated
vCPUs**, with serialized Cargo jobs. The benchmark-driver repair
`29960638e28c8a86cfd1a13b866b7dfd9c692438`, the 43 historical row judgments in
`judgments/own-repo-close.json`, and the generic analyzer repairs
`a85ae362057cf0a42f6317fd139192e3ace4f9c8` were already ancestors of this source.
They were not reimplemented or replaced with exclusions. The actual benchmark
package passes `./node_modules/.bin/tsc --noEmit --project benchmarks/tsconfig.json`
(TypeScript 5.9.3); it has no `typecheck` script of its own.

**The seven-repository diagnostics/footer comparison has now executed, but the
all-producer acceptance gate has not passed.** The following table is the current
result; all tables in the older sections below remain history for their named
source revisions, not current acceptance claims.

| Final target | Current measurement | Acceptance |
|---|---|---|
| AFT Rust <= rustc + 10 | Rust D12; all-target rustc D0 | **Not met**; three sampled new rows are demonstrable false positives, nine remain unjudged in this capture |
| Historical 33 Rust / 10 TS row review | All 43 already individually judged; 29 Rust false positives, four allowed-dead Rust helpers; three TS test-only references, seven genuinely unused TS declarations | Historical review complete; existing generic fixtures and explanations retained |
| Current in-domain AFT TS dead-code review | D7; all seven judged, 0 unjudged | Complete; includes newly unused `bridgeFor`, while historical `OpenCodeHostConfig` was removed |
| Current AFT unused exports | Project U145; **100 in-domain**, oracle 152, agreements 100; precision 1.000, recall 0.658 | Numeric, not unknown; **100 unjudged in this new capture** (the historical U104 review is not relabeled as a current review) |
| Seven-repository diagnostics/footer comparison | Seven repos / eight language samples collected; four samples scoreable, four unknown; all seven project footers honestly mask missing producers | **Executed, incomplete**; no unavailable producer counted as zero; Outline numeric project E/W target not established |
| Overall campaign | New source and dependency environment; no new six-repo liveness before/after comparison | **Not passed**; no monotonicity, TypeORM policy, or scoped-timing claim added |

### Current own-repository capture and source drift

The unchanged collector ran with `--repo aft --aft-bin target/release/aft
--aft-commit dab88ab951f1216b494bff68f1952c16252fbea6 --diagnostics-sample 0` and
`--corpus-root target/inspect-follow-up/corpus`. Only the previously documented
same-mode chmod workaround was applied at runtime: it skips the own config's
chmod only if the saved full mode already matches, and still restores bytes and
checks unchanged Git status. Three such no-op calls were recorded. No plugin,
Rust analyzer, scoring, manifest, or lockfile implementation changed here.

The release reports **project D22/U145**, not the historical Rust D4 / plugin U104
measurements. D22 comprises Rust 12 and TS 10; only **seven** TS rows are inside
the two plugin scoring prefixes. U145 similarly includes other packages; the
unchanged scorer counts **100**, not 145, in the plugin domain. Python coverage
is explicitly incomplete (130 unsupported files, 1,975 supported files), rather
than silently implemented. Own collection preserves the before/after Git status
and config bytes; task harness edits were present on both sides, so this is an
unchanged-status claim, not a claim of an empty checkout.

The new Rust sample is not evidence that the old repairs regressed:

- `db/file_identity.rs::is_live_sqlite_handle` is called by the exact-read path
  at `commands/file_read_exact.rs:75,144`, reached by `subc/flow_read.rs:496`.
  The handle-guard work landed later (`967e667c2d`, `56652ab5e6`). It is a live
  production helper, not one of the old allowed-dead sweep helpers.
- `edit.rs::link_count` is a Windows-only helper actually called by
  `windows::link_count(destination)` at `edit.rs:339`. The Linux rustc oracle
  cannot decide Windows liveness.
- `edit.rs::copy_platform_security` has separate macOS and Linux definitions;
  the replacement path calls it at `edit.rs:384`. Both this and `link_count`
  belong to newer replacement/security work (`7be96cfa4e`, `864a5c7050`). The
  sample demonstrates outstanding resolution/platform false positives; it does
  **not** establish an exhaustive cause diagnosis for the other nine rows.

Those three false positives and the seven current TS decisions are recorded
individually in `judgments/diagnostics-follow-up.json`, with no oracle-side
removals. The original 43-row judgments remain attached to their original
capture. Its four generic causes remain cfg(test) declarations, private trait
receiver dispatch, referenced static initializers, and cache-busting template
imports; the minimal store-backed/Oxc fixtures and prior red/mutation records
are documented in [the earlier repair slice](slices/own-repo-close.md#generic-liveness-repairs-and-row-decisions).
No new analyzer behavior was changed, so no new release note is needed.

The current source sample was scored through unchanged `run.py::score_repo`
with an isolated runtime descriptor containing only these ten current judgments.
The unavailable historical raw-origin files were not synthesized or pointed at
a newer capture to make `load_judgments` pass. Thus current Rust D is 3 judged / 9
unjudged, TS D is 7 / 0, and TS U is 0 / 100. Current TS D matches six of 37
reference rows (precision 0.857, recall 0.162); the additional `bridgeFor` finding
is source-confirmed dead even though Fallow does not report it. The historical
three TS oracle removals were not reapplied to this new score.

### Diagnostics comparison command and observed cells

There was no documented seven-repository diagnostics-only command at this base;
the README's offline command covered three liveness repos with diagnostics off.
The new [README command](README.md#seven-repository-diagnostics-and-footer-comparison)
now runs the missing pass without rebuilding a baseline or changing the scorer:

```sh
bun run --cwd packages/aft-bridge build
python3 benchmarks/inspect-truth/diagnostics.py \
  --aft-bin target/release/aft \
  --corpus-root target/inspect-follow-up/corpus \
  --output-dir target/inspect-follow-up/diagnostics-final \
  --sample 60 --same-mode-config-restore
```

All seven source pins are those in `corpus.json`; the own source is the commit
above. The runner reuses the campaign's seeded samples, NDJSON session/client,
reference invocations and parsers, and `(file,line)` matching. It compares
errors/warnings, not hints. For the own plugin roots only, tsc project arguments
are made relative to the working directory and diagnostic paths are normalized
back to repository-relative paths. Cargo reference checks finish before AFT can
start rust-analyzer. No TypeORM measurement-exclusion policy was changed or
needed for this diagnostics-only pass.

`unknown` below is not zero. Agreement/disagreement counts for unknown rows are
also unknown, not zero scores. A zero in a scoreable sample means the scoped
producer actually returned authoritative reports. It does not imply project-wide
producer completeness.

| Repository / language | Sampled files | AFT E/W | Reference E/W | Agreed | AFT-only | Reference-only |
|---|---:|---:|---:|---:|---:|---:|
| Outline / TS | 60 | 1 | tsc: 0 | 0 | 1 | 0 |
| TypeORM / TS | 60 | 0 | tsc: 0 | 0 | 0 | 0 |
| ripgrep / Rust | 60 | unknown | cargo: unavailable | unknown | unknown | unknown |
| axum / Rust | 60 | unknown | cargo: unavailable | unknown | unknown | unknown |
| soft-serve / Go | 60 | unknown | go vet: 0 | unknown | unknown | unknown |
| FastAPI / Python | 52 | 22 | Pyright: 50 | 22 | 0 | 28 |
| AFT / Rust | 60 | unknown | cargo: 0 | unknown | unknown | unknown |
| AFT / TS | 60 (58 examined) | 0 | tsc: 0 | 0 | 0 | 0 |

The inherited sampler filters TypeScript extensions but otherwise selects tracked
paths under the configured scoring prefixes; a Rust/Go sample is not promised to
contain only source files. The selected paths are retained verbatim. Outline,
TypeORM and FastAPI examined all sampled files; AFT TS examined 58 of
60 selected paths. The Rust/Go pages examined zero files and carry explicit
failed-producer/uncovered-file gaps, rather than authoritative empty lists.
The matching rule is the existing line-based rule, not a claim that diagnostic
codes or severity are always identical.

### Named producer gaps and the actual footer

Each row below was rendered by the production OpenCode `coerceAftStatus` parser
and `formatAftStatusSegment` formatter, not reconstructed from diagnostic counts.
All project snapshots have `diagnostics: "producer_missing"`; null E/W and
`E? W?` are correct, including in projects whose sampled TS files were complete.
The initial footer was `AFT E? W? | D? U? C? | T?` in each fresh process.

| Repository | Unavailable/failed project producers | Final real footer |
|---|---|---|
| Outline | `yaml-language-server` | `AFT E? W? \| D74 U245 C173 \| T19` |
| TypeORM | `bash-language-server`, `yaml-language-server` | `AFT E? W? \| D140 U190 C427 \| T10` |
| ripgrep | `rust-analyzer`, `bash-language-server`, `yaml-language-server` | `AFT E? W? \| D4 U0 C23 \| T11` |
| axum | `rust-analyzer` | `AFT E? W? \| D6 U0 C31 \| T7` |
| soft-serve | `gopls`, `yaml-language-server` | `AFT E? W? \| D29 U0 C11 \| T33` |
| FastAPI | `bash-language-server`, `yaml-language-server` | `AFT E? W? \| D0 U0 C37 \| T18` |
| AFT | `rust-analyzer`, `bash-language-server`, `yaml-language-server`, Dockerfile server (`docker-langserver`/`docker-language-server`) | `AFT E? W? \| ~D? U145 C532 \| T0` |

Rust's executable was the rustup proxy, which exited during initialization:
`Unknown binary 'rust-analyzer' in official toolchain
'1.99.0-x86_64-unknown-linux-gnu'`. This is a failed producer, not a working
server with no diagnostics. `gopls` and the named Bash/YAML/Dockerfile binaries
were unavailable. No producer request hit the runner's 900-second timeout.
Ripgrep's separate Cargo oracle could not resolve `pcre2`, and axum's could not
resolve `multer`, in the runner's offline registry. Those reference failures are
also unknown, not empty Rust reference outputs. The own Cargo oracle completed
with `Finished`/exit 0; its zero diagnostic/dead-code counts are real results.

### Judged diagnostic examples and remaining uncertainty

There are no agreements or disagreements to sample from a **complete empty**
TypeORM/AFT-TS comparison, and unknown Rust/Go lists are not evidence of no
findings. The nonempty observed cells were reviewed against the exact source
pins:

- **AFT-only, Outline `app/hooks/useShareDataLoader.ts:30`:** the source calls
  `setLoading(false)` synchronously inside the effect at lines 28-31. The AFT
  oxlint warning correctly identifies that React Compiler/lint pattern; tsc
  does not implement that lint. This is a producer-domain difference, not an
  invented TypeScript type error or a reason to delete an oracle row.
- **Agreed, FastAPI `fastapi/_compat/v2.py:27`:** both reports name the same
  private-import diagnostic for `GetJsonSchemaHandler` from Pydantic's internal
  module. The source has `# type: ignore[attr-defined]`, which does not suppress
  Pyright's `reportPrivateImportUsage`. The diagnostic is supported for the
  installed dependency/type-checker combination, not a claim of a runtime bug.
- **Agreed, FastAPI `fastapi/_compat/v2.py:164`:** both reports flag the dynamic
  `Annotated[annotated_args]` passed to `TypeAdapter`; the source's explicit
  `# ty: ignore[invalid-type-form]` is for a different checker. The paired
  argument-type and invalid-type-form messages are not fabricated agreements.
- **Reference-only, FastAPI `fastapi/telemetry/_asgi.py:176,178`:** the source
  directly indexes `config["tracing"]` and `config["tracer_provider"]`, whose
  declared `TelemetryConfig` is `TypedDict(total=False)` in `_api.py:35`.
  Pyright's optional-key-access diagnostics have a concrete type-level basis;
  no preceding membership guard narrows those keys. These are actual missing
  sampled AFT observations, not oracle rows judged out. The adjacent metrics
  access at line 183 is the same class.

An earlier retained capture observed **38** FastAPI E/W (38 agreements, 12
reference-only), while the final fresh process observed **22** (22 agreements,
28 reference-only), despite the same sampled files, binary and reference total
50. Both captures' pages claim authoritative coverage. This is unresolved
observation instability, not an accuracy improvement; no wait loop was added
until a desired count appeared. The project footer remains correctly unknown
for the named non-Python producer gaps. Neither this result nor fixture-only
masking tests establishes the all-repository diagnostic acceptance gate.

### Verification and retained current evidence

The release build finished (`cargo build --release -p agent-file-tools`,
17m26s; Cargo/rustc 1.99.0). Bun is 1.4.2, Python 3.14.4, Fallow 2.88.3 (verified
signed binary), Knip 5.85.0, Pyright 1.1.414 and typescript-language-server 6.0.2.
The Go bootstrap is 1.24.4; soft-serve's `go vet` automatically uses its required
1.26.5 toolchain. Its unrelated staticcheck/deadcode runs report unsupported
newer Go types; they are not substituted for the completed empty `go vet`
diagnostic report. FastAPI uses its own editable virtualenv, with Pydantic
2.14.1 and Starlette 1.9.0; these freshly resolved dependencies are another
reason not to treat the old and current diagnostic ratios as a controlled
before/after product comparison. No tracked manifest or lockfile changed.

The final gates passed: `python3 benchmarks/inspect-truth/test_diagnostics.py -v`
**7 tests**, `python3 benchmarks/inspect-truth/test_score.py -v` **39 tests**
(Python 3.12.3 on the non-network runner), and `bun run lint` **751 files**
(Biome 2.4.7). The benchmarks typecheck above exits 0. Scoped Pyright inspection
is authoritative for both new Python files with zero errors/warnings; the
unrelated checkout Tier-2 view is not ready, so that inspection's overall header
is partial, not a claimed complete repository-health pass.

The diagnostics runner tests cover unknown-vs-zero, omitted successful
`complete`, reference failure, real footer masking/numbers and the narrowly
scoped chmod skip. Two staged-state `NON-VACUITY BREAK` controls each reddened
exactly one test while the other six passed:

- Discarding page coverage failures: `test_missing_producer_is_unknown_not_zero`,
  `AssertionError: 'scored' != 'unknown'`.
- Treating an omitted successful `complete` flag as false:
  `test_complete_flag_may_be_omitted_on_success`,
  `AssertionError: 'unknown' != 'scored'`.

Each mutation had a non-empty `diagnostics.py` diff and an empty unstaged
`git diff --stat` after restoration from the staged live state. One queued
mutation attempt was refused before execution (`queue_wait_exceeded`); it was
restored immediately and later executed successfully on Linux, not retried on
macOS. No mutation remains in the delivered source. The corpus command exits **1**, correctly reporting seven
incomplete project diagnostic/footer coverages, rather than passing all-repo
acceptance. Earlier runner attempts failed before collection until the bridge
was built in the same remote job; the first collector's omitted-`complete`
classification was corrected and tested before the final capture.

Current raw evidence is retained outside regenerable build directories under
`.cortexkit/internal-docs/investigations/inspect-truth-follow-up/`: both diagnostic
captures, raw replies, reference output/metadata, own findings/oracles, scored
current sample and verification logs. The exported archive SHA-256 is
`86a96f7ed9c244456729f7d161e6a453cb4d9de274b67725ec1c23de5f28a509` (173 extracted
files); the candidate binary's SHA-256 is
`bae96f74de7996673b15c5ef2b0bfd3f3d101d191af57d0619eaa40c5ff4833b`.
The earlier reports and their evidence identities below are retained as history.

## Acceptance-blocker follow-up: own repository and TypeORM

Seven-corpus release measurements using new Agent File Tools (AFT) processes,
storage generations and full finding-list collection from
`972d87b8157db1f5b23141ae47f7a2c85371873a` supersede the remaining-row and TypeORM
completeness outcomes of the upgrade-and-report run below. The complete
[follow-up report](slices/own-repo-close.md) gives every before/after cell,
row decisions, measurement policy, raw evidence hashes, timing samples, and
plain-origin/main gate comparison. Earlier captures and reports remain historical
evidence, not overwritten results.

| Blocker | Before | Follow-up |
|---|---|---|
| Own Rust findings no more than ten above rustc's dead-code lint count | AFT 33 vs rustc 0; 33 unjudged | **4 vs 0**; source review explains 29 removed false positives and four unreachable production helpers retained for fixtures or under a dead-code lint allowance, with one reason per original row |
| Own in-domain TS dead-code review | 10 unjudged | All original 10 judged; three query-suffix-import test exports move to test-only usage, **7 remaining, 0 unjudged** |
| Own unused-export scoring and review | Unknown: real exercise driver syntax error | Queued repair `342737adf` included, **not excluded**; **104/162**, precision 1.000, recall 0.642; all 104 names now source-reviewed, 0 unjudged |
| TypeORM scoring | D/U unknown: four invalid codemod fixtures | Historical four-path exclusion declared in `corpus.json`, applied/restored by the collector; **U3**, precision 1.000/recall 0.750; **D0**, recall 0.000 |
| Score-cell monotonicity | Four TypeORM cells unknown | **No lowered cell**, 20 buckets/80 comparisons including the own checkout; external cells otherwise unchanged except axum D9 -> D5 |
| TypeORM scoped timing, four vCPUs | Two-vCPU enabled 5.707473s, disabled 5.307470s | **Still a real miss**: enabled 6.909799s, disabled 5.405199s; difference 1.504600s; <5s does not pass |
| Four old full-gate failures on plain origin/main | Not compared there | Outline portability and candidate-byte release pass on both; blackholed backend and missing `zip` fail on both |

The product fixes are generic: cfg(test) declaration exclusion, used private
trait receiver roots, referenced static initializer nodes, and fixed-path
cache-busting template imports. No symbol allowlist or tolerant-parser workaround
was added. Each repair has a red-before-fix test and an independently reddened
mutation control. Fact/producer keys invalidate old contributions; the aggregate
encoding version remains 35. The historical judgment files remain unchanged;
`judgments/own-repo-close.json` adds the requested 43 row decisions and three
category-specific test-only oracle decisions applied equally to both sides.

All six external source commits and their reference analyzers' frozen finding
snapshots are retained. The own oracles
were rerun on the repaired tree. Raw captures, timing responses and scored output
are ignored under `local-ignore/truth-close/`, not committed. Collection leaves
all seven source trees' tracked status and the own config unchanged.

**This is not an overall campaign pass.** Timing still misses on four allocated
vCPUs; only 9 of soft-serve's 26 reported dead functions match its oracle
(precision 0.346); and the corpus diagnostics/footer comparison is
still unrun. The full gate also remains red for environment failures reproduced
on exact plain `origin/main` (`1a87fec37`), including a separate missing-build-SHA
assertion. Focused scanner, Oxc, cache, calls, integration and binary suites pass;
the [slice](slices/own-repo-close.md#full-gate-baseline-comparison-and-verification)
records exact counts and the intentionally upgraded contribution/producer
goldens. No unrelated baseline failure or timing optimization is bundled.

The subsequent [Linux TypeORM phase profile](slices/typeorm-profile.md) attributes
the common cost to whole-project contribution rollups performed again for scoped
cache reads (5.60s mean across five categories). The repeatable enabled-config
increment is about 0.50s in selecting scope-relevant language-server runtime
notes, not a diagnostics sweep. The earlier 1.5s median gap is not a stable LSP
surcharge: repeat runs fluctuate and one enabled control falls rather than
rises. Full reference/probe reply bodies match after removing only request ID
and the last-run timestamp. No safe, byte-identical shortcut was proved, so this
follow-up reports the breakdown without changing runtime behavior.

The [104-row unused-export review](slices/own-unused-exports.md) now closes own
export judgment coverage. All 51 OpenCode and 53 Pi names match Fallow and have
individual source reasons in `judgments/own-unused-exports.json`; the rescored
bucket is 104 judged/0 unjudged with unchanged scores. Most are live same-file
helpers whose exported names have no downstream consumer. Separately consumed
test aliases do not consume an unused primary name, and an unconsumed private
barrel does not consume its canonical export. Those distinctions are checked
against source and regression fixtures; no false-positive class was found, no
export was removed, and no product allowlist or scoring-policy change was made.

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
