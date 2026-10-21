# Semantic base-reference prototype (slice 4a)

## Harness correction before the baseline

The original natural-language witness query, `function probe returning the number
twenty nine`, did not return the probe in its top 20 on the pinned AFT corpus,
even with trigram and semantic search both ready. Waiting longer cannot repair
that query's ranking. The witness now asks `function probe_entry in
views_perf_probe.rs`, which targets the same unchanged probe fixture. A captured
real reply with trigram enabled returns its line-3 state marker in the snippet.
Neither the corpus pin nor the probe's bytes changed.

The witness still requires a successful, complete public reply and the expected
file and current-state marker anywhere in its returned snippet. It additionally
requires both `semantic_status: ready` and `lanes.semantic.status: ready`; a
missing or building semantic lane fails closed. No clock, generic status label,
or inferred embedding throughput establishes readiness. Captured real ready and
building replies are retained as `slice-4a-witness-*.json` receipts and exercised
by the harness self-tests. The existing synthetic current-state test retains
its assertions, with its missing ready-lane field supplied explicitly.

Use the corrected witness and the frozen harness configuration (including the
trigram lane) for subsequent gates. A semantic-only acquisition cannot use
whole-response completeness as a substitute for semantic coverage: its bounded
exact fallback can make the response incomplete even when semantic is ready.

## Prototype result

The G1 fresh-checkout bind-to-semantic-ready baseline, prototype parity,
measurements and go/adjust verdict will be recorded after the corrected harness
acquisition and prototype verification. The measurement contract is described in
[`scripts/views-performance-contract.md`](../../scripts/views-performance-contract.md).
