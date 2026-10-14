# TypeORM enabled/disabled timing profile

## What was measured

The earlier five-call run on four Linux vCPUs reported a 1.504600-second median
gap (6.909799s enabled, 5.405199s disabled). That run retained responses but not
phase timings, so its exact historical gap cannot be retroactively assigned to
individual phases. This follow-up profiles the same pinned TypeORM source and
request: `scope:["packages"], sections:["dead_code"], topK:100`.

`perf` 7.0.14 refused performance events because `perf_event_paranoid=4` and the
job lacks the required capability. No permission setting was changed. Instead,
temporary elapsed-time probes measured the request's initial/final file checks,
each cached category read, cached-aggregate fetch, freshness check, scope rollup,
rendering, and language-server runtime-note selection. The probes only wrote
stderr; both modified Rust files were restored from the clean index afterwards.
Their exact patches, binaries' hashes and raw output are retained in ignored
`local-ignore/truth-followups/`, outside build directories. No profiler or
performance change is committed to the product.

All runs used `runon:linux,4c`, the release build, one scoped warm-up and five
measured calls per mode. Disabled switches off the language servers named in
`COLLECTION_USER_CONFIG` in `run.py`; enabled leaves language servers configured
normally by supplying an empty user config. Both exclude exactly the four invalid
codemod input/output fixtures declared in [corpus.json](../corpus.json) and listed
in the [exclusion policy](own-repo-close.md#typeorm-exclusion-policy).
Every sampled response has D135 and diagnostics exactly
`{status:"not_requested"}`. Two deliberately invalid complexity fixtures still
make the overall response partial; this is not a corpus diagnostics pass.

The original executable's SHA-256 is
`0780e19b598552b85a8a11e61b4e8475ceaa483a183fb58b84dbf73c4470c1ba`.
A reference call from it and all ten detailed-probe calls have identical complete
response bodies after removing only the request `id` and
`scanner_state.tier2_last_run` timestamp. No finding, summary, gap, text, envelope
or wait-stamp field was removed. The canonical-body SHA-256 is
`ee72f2bb4fe37608b39726e0d58d4a8676e182380639c3f28ce2375ffe842a20`.
Raw wire replies, including those two variable fields, are retained separately.

## Where the time goes

These are means over five detailed-probe samples, in seconds. Means are used for
an additive breakdown; the separately reported medians must not be added.

| Request work | Disabled mean | Enabled mean | Enabled - disabled |
|---|---:|---:|---:|
| Whole cached-category scope rollups/filtering | 5.598963 | 5.572883 | -0.026080 |
| Fetch/decode five stored aggregates | 0.276735 | 0.263628 | -0.013107 |
| Five aggregate freshness checks | 0.223131 | 0.216955 | -0.006176 |
| Select language-server runtime notes for scope | 0.117308 | 0.613953 | +0.496645 |
| Initial plus final file-stat inventories | 0.079939 | 0.074417 | -0.005522 |
| Render response | 0.000825 | 0.001581 | +0.000756 |
| Remaining request/client overhead | 0.072278 | 0.123591 | +0.051313 |
| Total observed wall time | **6.369179** | **6.867008** | **+0.497829** |

The common cost is scope reconstruction, not file-stat checking or waiting for
language servers. `tier2_read_cached_from_cache` verifies a stored aggregate, then
`filter_outcome_for_scope_with_contributions` calls
`scoped_tier2_payload_from_contributions`. That function loads the **whole
project's** contributions, rolls them up again, then filters the result to the
scope (`inspect/manager.rs:2883-2905,5589-5605,6365-6381`). The legacy cache path
still supplies scoped summaries for all five Tier-2 categories even though only
dead-code details were requested (`commands/inspect.rs:692-709,849-870`). Mean
filter/rollup costs by category are:

| Category | Disabled mean (s) | Enabled mean (s) |
|---|---:|---:|
| dead_code | 3.896338 | 3.895219 |
| unused_exports | 0.759670 | 0.717678 |
| duplicates | 0.202061 | 0.192422 |
| cycles | 0.643756 | 0.675522 |
| complexity | 0.097138 | 0.092042 |

The reproducible configuration-dependent extra half-second is **runtime-note
metadata selection**, not a requested diagnostics sweep. Scoped requests without
diagnostics skip applicability startup at `commands/inspect.rs:543-548`, but the
payload still selects runtime notes at `:990-1000`. With no expected/failed
producers supplied, `scope_producer_keys` derives keys again (`:1093-1101`). It
creates a new tsconfig membership cache, enumerates scoped diagnostic candidates,
and asks which configured servers/workspace roots match each file
(`inspect/diagnostics_category.rs:756-764`,
`inspect/scoped_diagnostics_sweep.rs:50-67`). Enabled configuration has more
matching-server metadata work. Repeated warnings about missing tsconfig extends
in the logs are from that discovery; they do not establish producer startup or
an authoritative diagnostic report.

## Why the apparent rising trend is not a measured fixed LSP surcharge

The first coarse profile reproduced a burst in the enabled run:
6.0074 / 6.6084 / 8.8931 / 8.4224 / 7.1101s, versus a disabled median 6.4093s.
From the first enabled sample to its 8.8931s peak, about 1.650s of the increase
was in dead-code cached scope reconstruction and 0.699s in unused-export
reconstruction, with the rest spread over other work. Process CPU increased from
651 to 957 ticks for those calls, rather than exposing a language-server wait.
The job reported no CPU-quota throttling. Resident memory rose during warm-up
and initial reuse, then plateaued; there is no measured steadily growing producer
queue or per-request memory leak.

The trend does not persist across controls:

| Probe run | Disabled median | Enabled median |
|---|---:|---:|
| Coarse, original timing configuration | 6.409304s | 7.110100s |
| Coarse, collector project config retained | 7.519126s | 7.315240s |
| Detailed, original timing configuration | 6.307160s | 6.993959s |

The earlier collector restored the TypeORM project config before its separate
timing sessions, leaving that file absent. The control retains the collector's
semantic/trigram-disabled project config during timing; it does **not** remove
the large scope-reconstruction cost, and its enabled samples fall from 9.7225s
to 6.8124s instead of rising. Detailed enabled samples are
6.4079 / 7.0107 / 6.9940 / 5.9075 / 8.0150s. At the last spike, runtime-note
selection itself rose to 0.924848s and the dead-code filter took 4.461125s.
Thus the old 1.5s median difference mixes configuration-dependent metadata cost
with substantial variable CPU-bound scope reconstruction. It is not evidence of
a 1.5s diagnostics wait or an intrinsically monotonic enabled-LSP slowdown.
The original five historical samples have no resource/phase trace; a unique
external cause for their particular monotonic sequence is not proved.

## Decision and retained evidence

**No product timing fix is made.** Eliminating runtime-note discovery could hide
already-known, scope-relevant producer failures. Replacing contribution-based
scope reconstruction with the project aggregate or a new memo requires proving
scope-specific accounting and cache invalidation remain correct. Neither shortcut
has been shown to preserve all inspect reply fields across scope, source-change,
cache-generation and producer-note states. The TypeORM comparison above proves
the probes preserved replies, not that either proposed optimization would do so.
The profile names the
expensive work and the configuration-dependent increment; the timing target
remains missed rather than being weakened.

Retained evidence:

- `local-ignore/truth-followups/profile.json`, four coarse cases, SHA-256
  `071f2bd89e5f8f0d0ec7a6263117e9e4e4dab34c29c9ffdbe194a812ca82df0f`;
- `local-ignore/truth-followups/detail.json`, reference plus detailed cases,
  SHA-256 `b7e48dfbf2ad79c30e56e73b96ad62d0bc142b0c932139d9dd2a613602f7ac76`;
- `breakdown.json`, extracted samples and both levels of phase timings;
- `profile-probes.patch` and `detail-probes.patch`, exact temporary probes;
- `*-detail.stderr.log`, `*-detail.log`, and the four coarse stderr logs.

A profiling build was interrupted before its tool result was delivered. The next
job confirmed the binary contained the probes and checked its SHA-256 before
running it. The subsequent detailed-probe build returned `Finished`, exit 0.
One profiling launch was refused during bounded Git-hook refresh; no command ran
on that attempt, and the next attempt completed. `crates/aft/src/commands/inspect.rs`
and `crates/aft/src/inspect/manager.rs` were restored to commit `662eb1d48`, with
an empty source diff afterwards. The TypeORM checkout's `.cortexkit/aft.jsonc`
and `.aftignore` were restored to their prior bytes/existence. The only committed changes in this
follow-up are this report and its campaign-report link.
