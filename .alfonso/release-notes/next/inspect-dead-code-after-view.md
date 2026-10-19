### Inspection catches up after checkout indexing

On projects using checkout index views, publishing a view now schedules a Tier-2 refresh, so dead-code analysis no longer stays stale after an early scan ran before its call graph was available. Inspection reports when dead code is waiting for the checkout's view to publish and explains that publication refreshes it automatically, instead of claiming that source or configuration changes were established.

Incomplete analysis now names up to five files that could not be parsed and reports how many additional files were omitted, helping distinguish source syntax errors from parser coverage gaps.
