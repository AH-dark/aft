# Rich UI fixtures

Byte-pinned inputs for AFT's published rich-UI manifest. The vector JSON is read
with `include_bytes!`, and this directory is marked `-text` in `.gitattributes`,
so a Windows checkout with `core.autocrlf=true` cannot rewrite the bytes its
digests cover.

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

No template source is published yet, so this directory holds no template file;
template files added here follow the same byte-pinning rules.
