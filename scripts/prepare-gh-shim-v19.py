#!/usr/bin/env python3
"""Prepare unsigned v19 from the reviewed unsigned v18 payload.

v19 adds the reviewed `pr ready` governed tuple, four admin tuples, two
private-vulnerability-reporting API rules, and one repository binding. The
baseline digest and formatting checks keep every unrelated v18 byte stable.
"""
import argparse
import hashlib
import json
from pathlib import Path

BASELINE_SHA256 = "eed64429a85eddaa81fa0e2456354915915183618b876dc3fa3c5e23a4aa1195"
ADDITIONS_PATH = Path(__file__).parent.parent / "crates/aft/tests/fixtures/gh_shim/v19-additions.json"
NEW_BINDING = ("cortexkit/common-auth", "agent_0f4d7713e64e7cc4")


def encode(value):
    return json.dumps(value, indent=2, ensure_ascii=False) + "\n"


def prepare(v18_path: Path, output: Path, issued_at_unix_secs: int) -> bytes:
    raw = v18_path.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    if digest != BASELINE_SHA256:
        raise ValueError(f"expected reviewed v18 baseline sha256 {BASELINE_SHA256}, got {digest}")
    text = raw.decode("utf-8")
    manifest = json.loads(text)
    if manifest["manifest_version"] != 18:
        raise ValueError("expected the reviewed unsigned v18 baseline")
    if encode(manifest) != text:
        raise ValueError("baseline formatting differs; review before regenerating")
    if issued_at_unix_secs <= manifest["issued_at_unix_secs"]:
        raise ValueError("issued_at_unix_secs must be later than the v18 baseline")

    additions = json.loads(ADDITIONS_PATH.read_text(encoding="utf-8"))
    if additions["manifest_version"] != 19:
        raise ValueError("expected v19 additions")
    if set(additions["tiers"]) != {"governed", "admin"}:
        raise ValueError("v19 additions must contain only governed and admin tier rows")
    if list(additions["canonicalization"]) != ["pr ready"]:
        raise ValueError("v19 additions must canonicalize only pr ready")

    existing_tuples = {
        row["tuple"] for tier_rows in manifest["tiers"].values() for row in tier_rows
    }
    for tier in ("governed", "admin"):
        for row in additions["tiers"][tier]:
            if row["tuple"] in existing_tuples:
                raise ValueError(f"baseline or additions already declares {row['tuple']}")
            existing_tuples.add(row["tuple"])
    if "pr ready" in manifest["canonicalization"]:
        raise ValueError("baseline already canonicalizes pr ready")
    for rule in additions["api_rules"]:
        if any(
            existing["method"] == rule["method"]
            and existing["path_glob"] == rule["path_glob"]
            for existing in manifest["api_rules"]
        ):
            raise ValueError(f"baseline already declares {rule['method']} {rule['path_glob']}")
    binding_repo, binding_agent = NEW_BINDING
    if binding_repo in manifest["bindings"]:
        raise ValueError(f"baseline already binds {binding_repo}")

    original_bindings = dict(manifest["bindings"])
    manifest["manifest_version"] = additions["manifest_version"]
    manifest["issued_at_unix_secs"] = issued_at_unix_secs
    for tier in ("governed", "admin"):
        manifest["tiers"][tier].extend(additions["tiers"][tier])
    manifest["api_rules"].extend(additions["api_rules"])
    manifest["canonicalization"]["pr ready"] = additions["canonicalization"]["pr ready"]
    manifest["bindings"][binding_repo] = binding_agent
    if any(manifest["bindings"].get(repo) != agent for repo, agent in original_bindings.items()):
        raise AssertionError("v19 preparation changed an existing v18 binding")

    payload = encode(manifest).encode("utf-8")
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(payload)
    print(f"{len(payload)} bytes  {hashlib.sha256(payload).hexdigest()}  {output}")
    return payload


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "v18_unsigned",
        type=Path,
        nargs="?",
        default=Path(".alfonso/ceremonies/gh-routing-manifest-v18/unsigned-v18.json"),
        help="reviewed unsigned v18 payload bytes",
    )
    parser.add_argument(
        "output",
        type=Path,
        nargs="?",
        default=Path(".alfonso/ceremonies/gh-routing-manifest-v19/unsigned-v19.json"),
        help="unsigned output path (default: private .alfonso/ceremonies directory)",
    )
    parser.add_argument("--issued-at-unix-secs", type=int, required=True)
    args = parser.parse_args()
    prepare(args.v18_unsigned, args.output, args.issued_at_unix_secs)
