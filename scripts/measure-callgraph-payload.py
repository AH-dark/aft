#!/usr/bin/env python3
"""Compare codecs on payload_corpus_measurement's immutable JSON payloads.

Requires msgpack 1.1.2 and zstandard 0.25.0. Timings use process CPU time,
exclude file I/O, and include JSON parsing after decompression. These are
same-runtime codec comparisons, not estimates of Rust serde performance.
"""

import json
import pathlib
import sys
import time
import zlib

import msgpack
import zstandard


def measure(root, language=None):
    root = pathlib.Path(root)
    index = json.loads((root / "index.json").read_bytes())
    stats = {}
    fields = dict.fromkeys(("ast_nodes", "symbols", "refs", "imports", "dispatch", "other"), 0)
    compressor = zstandard.ZstdCompressor(level=3)
    decompressor = zstandard.ZstdDecompressor()

    def json_encode(value):
        return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()

    selected = []
    for number, entry in enumerate(index):
        if language and entry["language"] != language:
            continue
        selected.append(entry)
        raw = (root / f"{number}.json").read_bytes()
        value = json.loads(raw)
        remaining = dict(value)
        remaining.pop("ast_nodes", None)
        subtotal = 0
        for field in fields:
            if field != "other":
                length = len(json_encode(value[field])) if field in value else 0
                fields[field] += length
                subtotal += length
        fields["other"] += len(raw) - subtotal
        for drop, payload in ((False, value), (True, remaining)):
            for binary in (False, True):
                for compression in ("none", "zstd", "zlib"):
                    # zlib tests the compressor already in Cargo.lock; zstd is
                    # included to quantify whether a new dependency is needed.
                    name = ("no_ast/" if drop else "full/") + ("msgpack/" if binary else "json/") + compression
                    started = time.process_time_ns()
                    encoded = msgpack.packb(payload, use_bin_type=True) if binary else json_encode(payload)
                    if compression == "zstd":
                        encoded = compressor.compress(encoded)
                    elif compression == "zlib":
                        encoded = zlib.compress(encoded)
                    encode_ns = time.process_time_ns() - started
                    started = time.process_time_ns()
                    decoded = encoded
                    if compression == "zstd":
                        decoded = decompressor.decompress(decoded)
                    elif compression == "zlib":
                        decoded = zlib.decompress(decoded)
                    decoded = msgpack.unpackb(decoded, raw=False) if binary else json.loads(decoded)
                    decode_ns = time.process_time_ns() - started
                    assert decoded == payload, (entry["path"], name)
                    result = stats.setdefault(name, {"bytes": 0, "encode_cpu_ns": 0, "decode_cpu_ns": 0, "sizes": []})
                    result["bytes"] += len(encoded)
                    result["encode_cpu_ns"] += encode_ns
                    result["decode_cpu_ns"] += decode_ns
                    result["sizes"].append(len(encoded))

    for result in stats.values():
        sizes = sorted(result.pop("sizes"))
        result.update(median=sizes[len(sizes) // 2], p90=sizes[(len(sizes) * 9) // 10], maximum=sizes[-1])
    print(json.dumps({
        "corpus": str(root), "language": language, "files": len(selected), "fields": fields,
        "extraction_cpu_ns": sum(e["extraction_cpu_ns"] for e in selected),
        "rust_decode_cpu_ns": sum(e["decode_cpu_ns"] for e in selected),
        "largest": sorted(selected, key=lambda e: e["bytes"], reverse=True)[:10],
        "codecs": stats,
    }, indent=2))


if __name__ == "__main__":
    measure(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else None)
