# Search-quality reference re-recorded after the semantic-lane ranking change

The ranking train that keeps the semantic lane on for every natural-language
`aft_search` query landed on main. It also counts declarations that extend a
split-pattern alternative as definitions, and names module-level declarations
first in "defined in". A ranking landing passes the gate against the old
reference but does not update it, so every later train that touches a
ranking-fenced file and declares `engine_unwired` was byte-compared against
results the engine no longer produces. The first to hit it failed with
`engine_unwired_mismatch:row=real_query.followup-census:7614`.

The reference was recorded from a release build of main commit
`62d19f17282bed88bba4c9f810a38f8860f4d7b6` on the Linux build server, with
`AFT_BINARY_PATH=target/release/aft scripts/telemetry/cost-gate.sh --search-quality --mode record-reference`
(corpus provisioned with `benchmarks/aft-search/provision_corpus.py`). The
engine source matches the landed ranking change; the commits after it on that
main touch symbol edits only.

Compared with the previous reference, all 98 rows are present and none was
added or removed. Ranked paths changed on 24 rows. Across `hit_at_1`,
`hit_at_5` and `mrr_at_10`, 13 per-row metric values improved and none got
worse. This is the movement the ranking train measured when it landed.
