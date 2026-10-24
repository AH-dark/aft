# Bounded family-sweep health telemetry

Health reports now retain telemetry for only the 64 most recently updated v2
family directories. Repeated sweeps update the same family's counters rather
than adding history rows. Counters restart if an evicted family is seen again;
`v2_family_sweep_details_evicted_total` discloses those cap evictions. A retired
family is removed from telemetry when its next sweep finds its directory missing,
counted separately by `v2_family_sweep_details_retired_total`.

When health metrics reach their 12 KiB budget, historical sweep rows now yield
before current per-root and standing-health diagnostics. The existing
`v2_family_sweeps` array is preserved, newest first; `v2_family_sweeps_omitted`
counts rows removed from that snapshot to fit the budget. If root details still
cannot fit after all sweep rows are gone, `root_details_omitted` continues to
disclose exactly how many root rows are absent.
