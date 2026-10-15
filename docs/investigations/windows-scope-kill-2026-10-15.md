# Windows scope revocation measurements — 2026-10-15

## Result

PowerShell scope revocation was **not reliably slow** on the rebuilt Windows
runner. Five isolated PowerShell runs and three isolated `bash`-tool control runs
passed. The explicit Windows process-tree termination phase took **65–147 ms**
for PowerShell and **41–139 ms** for the controls. A concurrent family run also
passed both original failing tests; their termination phases were 696 and 666 ms.
No observed kill or terminal-publication phase took multiple seconds.

The concurrent run did expose a different family member failing **before any
kill was invoked**. Its frame dump identifies a held `scope.describe` reply sent
761 ms after the request was received, although the fixture gives that RPC only
200 ms. This is a test correlation-lifetime problem, not evidence of slow
`taskkill`. It is not yet proof that the original two CI failures had that cause.

This commit changes only failure diagnostics and records measurements. It does
not change product termination, scope RPC handling, test budgets, sleeps, or
assertions. It preserves the measured baseline commit and all its ancestors;
the diagnostics and this report are an additive follow-up.

## Environment and invocation

- Measured source: `4ff28c3bbeb29c7a4efd7666ca7f51009a16ec04`, the previous isolated
  worker delivery. The final isolated rebind diagnostic run adds only frame
  dumping on assertion unwinding.
- `runon: "windows"`: fixed four-vCPU slot on the rebuilt MOTOR image.
- `rustc 1.99.0 (b940084d7 2026-09-28)`;
  `cargo 1.99.0 (5f94df478 2026-08-27)`; `pwsh 7.6.7`.
- Runner-provided `CARGO_TARGET_DIR=C:\t`; no short-path target override.
- `AFT_TEST_SCOPE_DIAGNOSTICS=1`, `test-timing-hooks`, real native MSVC binaries,
  real PowerShell processes, and real daemon frames.
- Samples are separate executions, not retry-until-green logic. Every execution
  that reached a test is reported below, including the two failing family runs.
  No CPU load generator was used.

The local, Git-ignored native-gate script,
`.cortexkit/internal-docs/investigations/native-gate-commands.sh`, is not copied
into remote job snapshots. Its Windows branch was therefore transported through
Bash stdin and executed from the snapshot's Cargo root. The updated local script
uses the runner's target directory and does not require Git metadata when already
at that root. The image has no `cargo-fmt.exe`; formatting is not a prerequisite
for this native measurement mode. The missing ignored file, unavailable remote
Git metadata, and missing rustfmt component caused three pre-test setup failures;
none ran a test or contributes a timing sample.

The PowerShell command was:

```sh
AFT_TEST_SCOPE_DIAGNOSTICS=1 cargo test -p agent-file-tools \
  --features test-timing-hooks --test rest -- \
  scope_end_kill::goodbye_ended_kills_powershell_through_the_same_stamped_registry \
  --exact --nocapture
```

The control command replaced the filter with
`scope_end_kill::goodbye_ended_kills_exact_owner_and_refuses_prepared_call`.
The control uses the `bash` tool API; on this Windows image the daemon's shell
selection can choose PowerShell for its command. It is not a claim to have run a
POSIX Bash interpreter.

## Per-step measurements

All phase columns are integer milliseconds from the existing diagnostics.

- **Describe**: `goodbye_describe_queued` to accepted `describe_ended`, from UTC
  log timestamps. This includes the RPC and answer processing, not just wire time.
- **Queue**: reported `revoke_admitted.queue_ms`, from actor-job submission.
- **Prepare**: `kill_started` to `signal_started`, from monotonic elapsed fields.
- **Taskkill**: `signal_started` to `signal_returned`, which brackets
  `terminate_piped_task`, including the synchronous `taskkill` command.
- **Reap**: `signal_returned` to `child_reaped`; zero means under the logger's
  millisecond resolution, not an assertion of zero work.
- **Terminal work**: `child_reaped` to `terminal_snapshot`. This is an upper
  interval containing marker/metadata persistence, deferred DB work, output
  rendering, completion/notification preparation and snapshot construction; it
  does **not** isolate the JSON/DB write alone.
- **Kill total**: `terminal_snapshot.elapsed_ms`, from `kill_started`.
- **Revoke total**: `revoke_finished.elapsed_ms`, from actor-job submission,
  including its queue time and ledger work.
- **Test** excludes Cargo compilation; it includes fixture startup and route binds.

| Sample | Test | Describe | Queue | Prepare | Taskkill | Reap | Terminal work | Kill total | Revoke total | Test seconds |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| P1 | PowerShell | 15 | 40 | 20 | 139 | 4 | 32 | 195 | 235 | 6.88 |
| P2 | PowerShell | 117 | 39 | 20 | 147 | 1 | 36 | 204 | 245 | 2.24 |
| P3 | PowerShell | 174 | 51 | 39 | 72 | 1 | 56 | 168 | 220 | 3.09 |
| P4 | PowerShell | 8 | 100 | 23 | 102 | 4 | 62 | 191 | 292 | 2.70 |
| P5 | PowerShell | 6 | 65 | 24 | 65 | 0 | 42 | 131 | 198 | 2.36 |
| B1 | bash tool | 2 | 0 | 21 | 41 | 4 | 29 | 95 | 96 | 4.40 |
| B2 | bash tool | 7 | 1 | 26 | 72 | 0 | 33 | 131 | 134 | 3.49 |
| B3 | bash tool | 136 | 2 | 17 | 139 | 6 | 37 | 199 | 202 | 3.62 |

All eight executions reported `1 passed; 0 failed`; all measured terminal
snapshots were `Killed`, all direct-child reaps were observed, and all
`natural_marker` fields were `None`. The PowerShell taskkill median was 102 ms;
its revoke-total range was 198–292 ms. The larger whole-test durations are not
kill durations: fixture configure/bind and process startup precede these phases.
For example P1 logged a 2,211 ms bind reply and an approximately 800 ms fallback
spawn before Goodbye was sent.

The runner rejected `CREATE_BREAKAWAY_FROM_JOB` and the existing fallback spawned
without breakaway. That warning is about launch containment. The measured scope
kill path invokes `taskkill /PID <pid> /T /F` and waits for its status; there is no
explicit job-object teardown API in that path. Thus these numbers must not be
labelled job-object teardown timings. See
`terminate_piped_task` in `crates/aft/src/bash_background/registry.rs` and
`terminate_process`/`terminate_pid` in `crates/aft/src/bash_background/process.rs`.

## Concurrent family run and a located pre-kill failure

One additional existing-suite run used:

```sh
AFT_TEST_SCOPE_DIAGNOSTICS=1 cargo test -p agent-file-tools \
  --features test-timing-hooks --test rest -- scope_end_kill:: \
  --test-threads=32 --nocapture
```

It finished in 11.88 s with **8 passed, 1 failed**. Both originally reported
Goodbye tests passed. Their phase deltas were:

| Test | Describe | Queue | Prepare | Taskkill | Reap | Terminal work | Kill total | Revoke total |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| PowerShell Goodbye | 56 | 155 | 19 | 696 | 1 | 57 | 773 | 929 |
| Exact-owner Goodbye | 25 | 4 | 11 | 666 | 0 | 34 | 711 | 717 |

The suite failure was
`scope_end_kill::rebind_live_epoch_move_survives_then_old_answer_spares_new_epoch`:
its five-second filesystem-status loop never saw the old task become `killed`.
The failing daemon emitted neither `describe_ended` nor `kill_started`.

To identify the stage rather than speculate about Windows termination, failure
unwinding now prints the frame history for assertions outside `reply()` as well.
It also prints frames already received into the channel but not consumed by a
fixture helper. Unread frames are labelled as queued at failure, not assigned an
invented arrival timestamp. No callback, timeout or assertion changed.

Two targeted diagnostic executions of that already failing rebind test also
failed, in 8.89 s and 8.20 s respectively. They are recorded failures, not retries
counted as successes. In the first, correlation 4 was held for 761 ms. In the
final frame dump, the same ordering was:

```text
at_ms=1792027224764 received type=Request channel=0 epoch=0 corr=4 body={"op":"scope.describe","owner":{"kind":"reserved","module_id":"owner"},"ref":"same-ref"}
at_ms=1792027225475 sent type=Response channel=0 epoch=0 corr=4 body={"op":"scope.describe","status":"ended","scope_epoch":1,"owner_synced":true,"owner_configured":true,"daemon_incarnation":"test-daemon"}
```

Between those lines, the fixture started its new epoch-2 shell, answered its
permission question, and waited for its running record. The RPC reply was
therefore held **711 ms**, 511 ms beyond the fixture's 200-ms describe timeout.
The failure dump then showed unread `scope.describe` requests with correlations
**5, 6, 7, 8, 9, 10 and 11**, still waiting in the fixture. There was no accepted
end/revocation/kill diagnostic phase for this daemon. The observed wait is
**before revocation admission and before taskkill**, not a slow terminal record.

The source explains the sequence:

1. `Daemon::spawn` in `crates/aft/tests/scope_end_kill.rs` sets
   `AFT_TEST_SCOPE_DESCRIBE_TIMEOUT_MS=200` and `AFT_TEST_SCOPE_POLL_MS=500`.
2. The rebind test deliberately saves its second `describe()` frame before
   starting and awaiting the new shell, and answers that saved frame only after
   the new task is running.
3. `DaemonControl::describe` in `crates/aft/src/subc/readiness.rs` times out the
   request. Its `PendingGuard::drop` removes the correlation waiter; a late reply
   no longer resolves it. `abandoned_request_leaves_no_pending_entry_for_a_late_reply` is the
   existing readiness contract test for abandoned correlations.
4. `scope_describe` treats the timeout as non-definitive and retries on its poll
   interval. This accounts for the later correlations. `Daemon::next_matching`
   automatically answers only `supervisor.live_roots` and `catalog.update`, not
   `scope.describe`; the status loop does not consume the replacement requests.
5. Without an accepted `Ended`, no `revoke` or Windows kill begins, so waiting
   five seconds for `killed` cannot repair that expired request.

This locates a real failure in the same suite and supplies a concrete signature
to check in the original Goodbye failures. It does **not** establish that the
original PowerShell 10-second poll or exact-owner 15-second reply timeout had the
same cause: neither of those two failed in this sample set. In particular, a
200-ms timeout may be polled late by the runtime; exceeding 200 ms alone is not
the entire proof. The missing accepted-end phase plus the replacement
correlations distinguish this run from successful late processing.

## Proposed follow-up, not implemented

Do not increase the 200-ms test timeout or the production 10-second RPC timeout,
and do not replace the fast measured kill path with speculative job-object
termination changes.

For the rebind fixture, use a per-request test clock/release hook that keeps the
intentionally held describe correlation alive until the new epoch's running
record has been observed and the saved answer is released. Then await an
accepted-answer/revocation event before asserting the old task's terminal
record. Keep separate explicit expiry controls for the tests whose subject is
scope timeout/retry behavior. A regression control should force expiry of the
held request and reproduce the unaccepted end plus replacement correlations.

For `goodbye_ended_kills_powershell_through_the_same_stamped_registry` and
`goodbye_ended_kills_exact_owner_and_refuses_prepared_call`, retain the sent,
received and unread-frame dump from `Daemon::reply` or assertion unwinding and
check the next failing run: no accepted end means investigate RPC lifetime/dispatch; admitted revoke
with a long signal interval means investigate taskkill; a fast signal followed
by a long reap/terminal interval points to those later stages. The current
source's scope-source collection, semaphore acquisition, actor/DB waits,
`taskkill.status()` and child `wait()` are not all bounded by the describe RPC
clock. Finite fast samples do not prove those paths can never stall.

## Reproduction ledger and verification limits

The observations are preserved here, outside regenerable build output. Runner
job output was read directly; stderr included the existing complete phase log.
P1–P5 identify the PowerShell rows in the timing table; B1–B3 identify its
`bash`-tool control rows.

| Samples/check | Runner job | Outcome |
| --- | --- | --- |
| P1 | `01a13d00-c761-770c-ad8d-010f8d550bbd` | 1 passed |
| P2, P3, B1 | `01a13d07-2443-73a7-8f3f-16a3e7faf817` | 3 separate executions passed |
| P4, P5, B2, B3 | `01a13d0c-fdce-757c-a3ae-d0f50e3ce31b` | 4 separate executions passed |
| Concurrent family | `01a13d12-ae2a-7263-a596-7d825a075b7a` | 8 passed; rebind test failed |
| Rebind history dump | `01a13d19-baf2-7230-bde4-bee19d989dcd` | 0 passed; rebind test failed; held reply 761 ms |
| Rebind unread-frame dump | `01a13d22-a9d2-71cb-999a-139a2bc35f2e` | 0 passed; rebind test failed; held reply 711 ms and replacement correlations 5–11 |

The original kill tests' passing samples and the newly identified rebind failure
are deliberately not combined into a green suite claim. No product fix, RPC
lifetime fix, widened budget, skip, or automatic retry is delivered here.

Final follow-up checks: the Linux `scope_end_kill` target passed all eight tests
with four test threads, and the existing
`subc::readiness::tests::abandoned_request_leaves_no_pending_entry_for_a_late_reply`
passed. `cargo fmt --all -- --check` passed with rustfmt 1.10.0-stable. The new
unwinding diagnostics compiled and printed the recorded replacement correlations
in the native Windows failure above. That native rebind failure remains an
explicit open fixture issue rather than a green gate. No mutation control or
production behavior change was needed for this measurement-only follow-up.

### Recorded phase inputs

For independent arithmetic checks, these are the recorded UTC queue/accepted-end
pairs and the task-relative `elapsed_ms` fields in order
`signal_started / signal_returned / child_reaped / terminal_snapshot`.
All timestamps below are on 2026-10-15; they are not end-to-end test durations.

```text
P1 bash-01777886dbfd4290 00:45:20.300 -> 00:45:20.315  elapsed=20/159/163/195  revoke=235 queue=40
P2 bash-e096d6451189228c 00:52:02.652 -> 00:52:02.769  elapsed=20/167/168/204  revoke=245 queue=39
P3 bash-7581f12e5b57abd8 00:52:08.726 -> 00:52:08.900  elapsed=39/111/112/168  revoke=220 queue=51
P4 bash-5e6f0cdd07d70fb1 00:57:28.128 -> 00:57:28.136  elapsed=23/125/129/191  revoke=292 queue=100
P5 bash-1582903c8b435fdc 00:57:39.171 -> 00:57:39.177  elapsed=24/89/89/131   revoke=198 queue=65
B1 bash-461023980a39b178 00:52:14.476 -> 00:52:14.478  elapsed=21/62/66/95    revoke=96  queue=0
B2 bash-5e4d13b495d95469 00:57:43.486 -> 00:57:43.493  elapsed=26/98/98/131   revoke=134 queue=1
B3 bash-417aee352ef455d4 00:57:47.754 -> 00:57:47.890  elapsed=17/156/162/199  revoke=202 queue=2
concurrent PowerShell bash-698acb6e7874c6a1 01:04:41.229 -> 01:04:41.285 elapsed=19/715/716/773 revoke=929 queue=155
concurrent exact-owner bash-6dd88d2e222c1366 01:04:42.108 -> 01:04:42.133 elapsed=11/677/677/711 revoke=717 queue=4
```
