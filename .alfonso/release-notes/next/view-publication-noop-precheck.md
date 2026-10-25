- View publications now discard provably irrelevant file notifications before
  acquiring a cold-build slot or recomputing a manifest. The precheck validates
  the published generation, HEAD, Git index contents and artifact readiness;
  tracked files, directory notifications, rescans, pending semantic work and
  uncertain state still take the full publication path. Skipped batches log
  `reason=non_manifest_paths` separately from derived-build reasons.
