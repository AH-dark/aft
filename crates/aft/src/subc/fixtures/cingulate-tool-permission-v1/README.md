# Cingulate `tool_permission` contract vectors

`vectors/` is an unmodified copy of Cingulate's `test-vectors/tool-permission-v1/`
directory at cingulate commit `baba4382d1457b76d2590c201981c6c03f141eb3`
("mason: implement scoped AFT permission asks"). `vectors/SHA256SUMS` is
Cingulate's own manifest; AFT's tests check every listed file against it before
using the vectors.

AFT's consent client (`crates/aft/src/subc/consent.rs`) is tested against these
files in `consent_tests.rs`:

- AFT's encoded `consent.request`, `consent.await` and `consent.withdraw`
  envelopes must equal the `*-request.json` bytes (worker-open, replay,
  await-pending, await-settled, withdrawn).
- AFT's decoder must accept the `*-reply.json` bytes (worker-open, replay,
  await-pending, await-settled, withdrawn, and the ask records embedded in the
  head-not-held and escalated replies).
- `bind-stamps.json` supplies the worker and head scope stamps; only the worker
  stamp's asks carry `head_policy`.

To update, copy the directory again from the Cingulate commit that changes the
contract, update the commit named above, and rerun
`cargo test -p agent-file-tools --lib -- consent_vectors`.
