# CI ownership of OpenCode tests

The reusable `.github/workflows/_unit-suite.yml` runs root `test:unit` on
Linux and macOS. Root `test:unit` invokes each package's `test:unit`; this
package selects `src/__tests__`, `test`, and the legacy `__tests__` directory, excluding
`src/__tests__/e2e/**`. The dedicated plugin e2e matrix selects that excluded
subtree (OpenCode NDJSON and subc lanes). The Windows bash-permission job also
runs `src/__tests__/e2e/bash.test.ts`. `beta-pin-gate.yml` additionally runs the
acceptance and real-host load matrices on matching pull-request path changes
or manual dispatch; it is not the owner of the whole `test/` tree.

The legacy `__tests__/disabled-tools.test.ts` also runs in these unit jobs.
All automatically discovered tests below `test/` run in the unit jobs:

- `cancellation/effect-cancellation.test.ts`
- `entry/server-effect.test.ts`
- `entry/config-state-isolation.test.ts`
- `matrix/acceptance-matrix.test.ts`
- `permissions/ask-site-inventory.test.ts`
- `../src/__tests__/v2-permission-assert.test.ts`
- `rpc/contract.test.ts`
- `rpc/register.test.ts`
- `tool-surface/v2-bash-permission.test.ts`
- `tool-surface/v2-behavior.test.ts`
- `tool-surface/v2-result-envelope.test.ts`
- `tool-surface/v2-tool-surface.test.ts`
- `tui/background-status-refresh.test.tsx`
- `tui/v2-entry.test.ts`
- `tui/v2-setup.test.tsx`
- `tui/v2-status.test.ts`
- `wakes/runtime-consumer-locations.test.ts`
- `wakes/session-delivery.test.ts`

`load-matrix/load-matrix.ts` runs in the path-filtered `beta-pin-gate.yml`,
not the general unit jobs. Run it locally with `bun run test:load-matrix`: it
downloads real V1 and V2 OpenCode hosts, requires Node 24 and subc-core, and
checks operator database and log snapshots. Its nonstandard filename
deliberately prevents automatic `bun test` discovery.

`src/__tests__/test-discovery.test.ts` checks the package inventory against the
CI unit script and checks the generic e2e command and beta-pin probe invocation.
New conventional test files outside the selected directories, and new
nonstandard `bun:test` probes under `test/`, fail the guard until a CI command
owns them.
