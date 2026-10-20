# bash.kill_scope wire examples

`wire.json` contains complete management request/reply bodies. Replace `TASK_ID` with the task ID returned by AFT. The principal belongs to the authenticated calling route; it is not a request parameter. The embedded scope uses the existing `ScopeStamp` encoding.

`already_finished` counts retained, matching terminal tasks. `not_found` is zero because the request selects a scope, not individual task IDs. A second call returns no killed tasks, but may count tasks killed by the first call as already finished.

For remote tasks, `cancel: "sent"` means cancellation was sent, not that the runner confirmed a terminal outcome. AFT stops its local task immediately. Once a job ID is known, it sends `exec.cancel`; before acceptance, it can withdraw the active submission with a call-key Cancel. The latter has `job_id: null`. A remote task still waiting for route setup or otherwise unable to send cancellation within the shared 10-second deadline reports `job_id: null` when no ID is known and `cancel: "failed: did not settle within 10 seconds"`. Pending submission setup may still finish later; the worker retains cancellation intent and does not submit the paused command after setup completes.
