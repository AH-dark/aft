# AFT rich UI v1 schema freeze — CKIOS handoff

This directory is the schema-freeze handoff. It is **not a publication**: the
production builder returns `None`. S denotes the session whose page is being
shown, carried under its stamped owner/ref/epoch. The catalog digest and both template sources
are test-only placeholders. No placeholder is reachable through production.

## Frozen operation contract

`field-table.json` pins all ten version-1 entries, including args, result/item,
bound entities, profiles, targets, limits and paging. Every result field has its
source, presence, nullability and placement recorded. `manifest.jcs.json` is the
canonical manifest; `manifest.sha256` is its committed digest. `bash-pins.json`
pins every distinct served Bash definition (Head/Worker, with/without runon).
The Bash page's reads equal the placeholder page template's reads.

| Operation | Target | Result |
|---|---|---|
| bash_status | bash_status | `{running, exitCode}` |
| bash_output | bash_status | `{items: [{stream, text, bytes}], next}`; never total |
| bash_watch | bash_watch | `{running, exitCode, waited: {reason, limit_ms}}` |
| read | read | `{text}` |
| outline | outline | `{text}` |
| zoom | zoom | `{text}` |
| search | search | `{text}` |
| grep | grep | `{text}` |
| glob | glob | `{text}` |
| callgraph | callgraph | `{text}` |

Session-read text is the dispatcher-formatted handler reply (`data.text`), not
an arbitrary handler object. Keeping a single textual field gives the seven
union-free read operations a common result without exposing changing backend
metadata or recursive callgraph/zoom shapes. It does not add `text` to a shell
success. Shell `running` is the inverse of `BgTaskStatus::is_terminal`; a missing
or unknown status is a validation failure, not a default. `exitCode` maps the
required nullable `exit_code`. Watch output previews, pattern matches and worker
kill/wait metadata are not published. Poll reasons are match, exit, timeout.

`result-vectors.json` and `handler-replies.json` are normalized schema vectors,
not claims that the still-unimplemented carrier route has run. The subsequent
carrier-serving slices must test real handler/outer-route replies against these
same schemas. Projections drop unknown keys and never default a required field.

## Inputs, targets, media, messages

Call-scoped args never contain a viewed task. `bound-task.json` freezes the
carrier-only `task` argument as the **viewed Bash call key**, not a task id or
frame key. `bash_status.args` is the empty record. `bash_output` accepts cursor
and limit (1–200). Watch accepts pattern, optional regex, optional timeoutMs
(1–20,000). Session args use only union-free forms: outline target is a string;
zoom has a batch of targets, each with a list of symbol strings. Empty batches
and symbol lists fail the served-schema check, not a fabricated schema union.

`target-arguments.json` records every target-bearing field and the precedence
between the read-only gate and the carrier's argument validation (admission
step 5: size, closed schema and remote-target checks) for URL, issue and PR forms, including outline arrays (which may be
refused before their schema check) and nested zoom targets. `ref` is excluded.
Search patterns, grep include patterns and glob patterns are not target fields.

`gate-contract.json` records evidence from **20aba8d97b, landing**:
`20aba8d97b65c788759f9ee5024d71fdb98133f5`, file
`crates/aft/src/subc/carrier_read_only.rs`. That commit is absent from the
8072df9 base; its gate is being landed separately. The canonical public gate
names include `aft_outline`, `aft_zoom`, `aft_search`, `aft_callgraph`; they map
to the bare target names above. Nothing in this slice copies, widens or runs the
gate. Its tool refusal comes before target refusal. Both use
`carrier_read_only`. Read HTTP(S) targets pass that gate but fail step 5 with
`args_invalid`; outline/zoom HTTP(S), and all three tools' issue/PR targets, fail
at the gate. Other operations' target paths fail step 5. The gate's two message
templates are preserved verbatim in the fixture, with its substitution markers.

`media-outcomes.json` has one case per image/PDF/binary × read/outline/zoom.
All nine refuse **unsupported_media** before handler execution. v1 does not
carry attachments or pretend a size/attachment summary is file content.
Ordinary text/directory reads are unaffected. The carrier-serving read slice
must install the media check before calling these handlers.

Fixed safe messages and the generic fallback are frozen in
`crates/aft/src/subc/rich_ui/operations.rs` (`FIXED_MESSAGES`, `GENERIC_MESSAGE`);
each is at most 1,024 UTF-8 bytes. Unknown codes pass through with generic text.
Handler messages, data and original error output are excluded from carrier
error replies. No session metadata (`status_bar`, `bg_completions`) is included
in any carrier reply.

## Transport, frame identity and binding

`transport-contract.json` posts:

- Chunk requests: `rich_ui.manifest_chunk {manifestDigest,index}` and
  `rich_ui.template_chunk {templateDigest,index}`, 0-based.
- Chunk replies: exactly `{digest,index,chunks,bytes,data}`, where bytes is the
  whole asset length and data is standard padded base64 of ≤131,072 bytes.
  All refusals are typed protocol Error frames carrying a code, **not** a
  carrier tool-result body; neither messages nor frames echo request data.
- Carrier body: exactly `{content,isError,structuredContent}`. Content is one
  empty text item on success, one text item containing the code on failure.
  Success structuredContent is only the validated projected result or page;
  failure is exactly `{success:false,code,message}` with fixed safe text.
- **Fresh frame key per read, or no key.** Top-level call_key belongs only to
  the carrier frame. `arguments.task` names the viewed call. Reusing a settled
  frame key replays the same body, with the retry's correlation header, not a
  fresh poll. The carrier ledger string is `reserved:occipital#` followed by the
  compact JSON array `[owner principal_id,scope_ref,epoch decimal string]`;
  ledger scope is None. S's policy-route principal is never the ledger carrier,
  and the ledger row's scope is never S's scope.
- **Bind-label equality.** On S's scope, the carrier route's project_root,
  harness and session must exactly equal S's policy route labels. Any mismatch
  makes **every** carrier call refuse `session_not_bound`, reads included.
  S is found by stamped owner/ref/epoch, never by opener-supplied labels.
  Unscoped and self-owned scoped routes serve provider reads only.

**CKIOS confirmation requested:** where does Occipital learn S's exact project
root, harness and session labels? The bind contract depends on that source.
CKIOS should write the real row/page templates against this freeze, then hand
over unchanged canonical template files, their SHA-256s and the catalog digest.
Publication also waits for CKIOS canonicalization vectors, all carrier operations
and validation, approval of agent-visible refusal wording, SUBC capability
confirmation and the pinned daemon's HELLO test. This handoff does not attach a
HELLO descriptor, serve carrier reads/chunks or enable production view records.

## Verification / regeneration

Run `cargo test -p agent-file-tools --lib -- subc::rich_ui`. To review a deliberate
new operation-version freeze, run that test with `AFT_RICH_UI_PRINT_FREEZE=1`
and `--nocapture`; the golden test prints candidate canonical bytes, digest and
pins before comparing them with committed bytes. Updating fixtures is an
explicit review action, never automatic on a passing test. Do not update an
existing frozen version just to accept a changed field.

The nested `.gitattributes` keeps JSON/template/digest bytes unchanged on
Windows checkouts. JSON assets are embedded using `include_bytes!`. The same
unit digest assertion runs on Linux, macOS and Windows CI jobs.

## `canonical_vectors.json`

RFC 8785 (JSON Canonicalization Scheme) vectors plus the manifest number rule.
Each entry is:

| key | meaning |
|---|---|
| `name` | what the case pins |
| `input` | the JSON value to canonicalize |
| `canonical` | the exact canonical bytes as a string, or `null` when the manifest number rule must refuse the input |
| `digest` | `sha256:` of `canonical`, or `null` when `canonical` is `null` |

The second case reproduces RFC 8785's section 3.2.3 property-sorting example.
The first is RFC 8785's section 3.2.2 example with its fractional numbers
replaced by the integers they round to, because a manifest object carries
integers only. The remaining cases pin member sorting by UTF-16 code unit,
string escaping, preserved array order, the manifest number rule's exact
±(2^53-1) bounds, and its refusals for a fractional and an out-of-range
number.
