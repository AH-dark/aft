# Rich UI fixtures

Byte-pinned inputs for AFT's published rich-UI manifest. Every file here is
read with `include_bytes!` and marked `-text` in `.gitattributes`, so a Windows
checkout with `core.autocrlf=true` cannot rewrite the bytes their digests cover.

## `canonical_vectors.json`

RFC 8785 (JSON Canonicalization Scheme) vectors plus the manifest number rule.
Each entry is:

| key | meaning |
|---|---|
| `name` | what the case pins |
| `input` | the JSON value to canonicalize |
| `canonical` | the exact canonical bytes as a string, or `null` when the manifest number rule must refuse the input |
| `digest` | `sha256:` of `canonical` |

The first two cases are the examples RFC 8785 publishes in sections 3.2.2 and
3.2.3 (its expected output is also where the "ECMAScript-compatible number
serialization" and "property sorting" byte strings come from). The rest pin the
rules AFT adds on top of the RFC: object members sorted by UTF-16 code unit
(every number below stays inside ±(2^53-1), so the same bytes serve as the
entry's number-rule vectors), RFC 8785 string escaping, array order preserved,
and a refusal for a fractional or out-of-range number in a manifest object.

AFT does not write a template source yet, so the template directory has only
this README: the placeholder template files arrive with the slice that builds
templates, and they follow the same byte-pinning rules.
