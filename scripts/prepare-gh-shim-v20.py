#!/usr/bin/env python3
"""Prepare unsigned v20 from the reviewed unsigned v19 payload, without signing.

Only worker_reads, manifest_version and issued_at_unix_secs change. Every v19
row and binding remains intact; head sessions keep using the original classes.
"""
import argparse
import hashlib
import json
from pathlib import Path

BASELINE_SHA256 = "12c14ca1c8779b358998b000ef6210c75147ceec1f8c318fd5e7be42799a3573"
ADDITIONS_PATH = Path(__file__).parent.parent / "crates/aft/tests/fixtures/gh_shim/v20-additions.json"


def encode(value):
    return json.dumps(value, indent=2, ensure_ascii=False) + "\n"


def prepare(v19_path: Path, output: Path, issued_at_unix_secs: int) -> bytes:
    raw = v19_path.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    if digest != BASELINE_SHA256:
        raise ValueError(f"expected reviewed v19 baseline sha256 {BASELINE_SHA256}, got {digest}")
    text = raw.decode("utf-8")
    manifest = json.loads(text)
    if manifest["manifest_version"] != 19:
        raise ValueError("expected the reviewed unsigned v19 baseline")
    if encode(manifest) != text:
        raise ValueError("baseline formatting differs; review before regenerating")
    if issued_at_unix_secs <= manifest["issued_at_unix_secs"]:
        raise ValueError("issued_at_unix_secs must be later than the v19 baseline")
    additions = json.loads(ADDITIONS_PATH.read_text(encoding="utf-8"))
    if set(additions) != {"manifest_version", "worker_reads"} or additions["manifest_version"] != 20:
        raise ValueError("v20 additions must contain only version and worker_reads")
    if "worker_reads" in manifest:
        raise ValueError("baseline already declares worker_reads")
    if not additions["worker_reads"]:
        raise ValueError("worker_reads must not be empty")
    tuples = [row["tuple"] for row in additions["worker_reads"]]
    if len(tuples) != len(set(tuples)):
        raise ValueError("duplicate worker read tuple")
    manifest["manifest_version"] = 20
    manifest["issued_at_unix_secs"] = issued_at_unix_secs
    manifest["worker_reads"] = additions["worker_reads"]
    payload = encode(manifest).encode("utf-8")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(payload)
    digest = hashlib.sha256(payload).hexdigest()
    output.with_suffix(".sha256").write_text(f"{digest}  {output.name}\n", encoding="utf-8")
    print(f"{len(payload)} bytes  {digest}  {output}")
    return payload


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("v19_unsigned", type=Path, nargs="?", default=Path(".alfonso/ceremonies/gh-routing-manifest-v19/unsigned-v19.json"))
    parser.add_argument("output", type=Path, nargs="?", default=Path(".alfonso/ceremonies/gh-routing-manifest-v20/unsigned-v20.json"))
    parser.add_argument("--issued-at-unix-secs", type=int, required=True)
    args = parser.parse_args()
    prepare(args.v19_unsigned, args.output, args.issued_at_unix_secs)
