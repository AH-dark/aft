#!/usr/bin/env python3
"""Delegate rustc to sccache and report only counters for the AFT library request."""

from __future__ import annotations

import json
import subprocess
import sys


def is_aft_library(args: list[str]) -> bool:
    options = dict(zip(args, args[1:]))
    return options.get("--crate-name") == "aft" and options.get("--crate-type") == "lib" and "--test" not in args


def counters() -> dict | None:
    try:
        data = json.loads(subprocess.check_output(
            ["sccache", "--show-stats", "--stats-format=json"], stderr=subprocess.DEVNULL,
        ))["stats"]
        result = {
            "hits": data["cache_hits"]["counts"].get("Rust", 0),
            "misses": data["cache_misses"]["counts"].get("Rust", 0),
            "requests": data["requests_executed"],
            "write_errors": data["cache_write_errors"],
        }
        return result if all(type(value) is int for value in result.values()) else None
    except (subprocess.CalledProcessError, OSError, ValueError, KeyError, TypeError):
        return None


def proof(before: dict | None, after: dict | None, exit_code: int) -> dict:
    if before is None or after is None:
        return {"decision": "unavailable", "compiler_exit": exit_code}
    delta = {key: after[key] - before[key] for key in before}
    decision = "ambiguous"
    # Dependencies finish before the final AFT library request in this build.
    # If other requests finish in the window, or a restarted sccache daemon has
    # lower counters, the observed totals cannot prove this library was a hit.
    if exit_code == 0 and delta["requests"] == 1 and delta["write_errors"] >= 0:
        if delta["hits"] == 1 and delta["misses"] == 0:
            decision = "hit"
        elif delta["hits"] == 0 and delta["misses"] == 1:
            decision = "miss"
    return {"decision": decision, "compiler_exit": exit_code, **delta}


def run(args: list[str]) -> int:
    measured = is_aft_library(args)
    before = counters() if measured else None
    result = subprocess.call(["sccache", *args])
    if measured:
        # Do not publish raw debug logs or compiler environments: they can
        # contain credential-bearing cache URLs. Only integer counters leave.
        print("AFT_LIBRARY_CACHE_PROOF " + json.dumps(proof(before, counters(), result)), file=sys.stderr)
    return result


if __name__ == "__main__":
    sys.exit(run(sys.argv[1:]))
