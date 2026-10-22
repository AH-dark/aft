#!/usr/bin/env python3
"""Require calibrated storm deadlines to reject delayed fixture responses."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ACK_TEST = "subc_storm_test::subc_storm_slow_config_read_does_not_delay_bind_ack"
READ_TEST = "subc_storm_test::max_paths_single_event"
MAIN_TEST = "subc_storm_test::subc_storm_rebinds_stay_live_under_build_and_tool_traffic"
CONTROLS = (
    ("bind-ack", ACK_TEST, READ_TEST, "timed out waiting for RouteBindAck"),
    ("interactive-read", READ_TEST, ACK_TEST, "timed out waiting for tool response"),
)
ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


def check_inventory(document: dict) -> None:
    names = {
        name
        for suite in document["rust-suites"].values()
        for name, case in suite["testcases"].items()
        if not case["ignored"] and case["filter-match"]["status"] == "matches"
    }
    if len(names) != 19 or not {MAIN_TEST, ACK_TEST, READ_TEST}.issubset(names):
        raise RuntimeError(f"expected all 19 calibrated storm cases, including the traffic storm and controls; got {sorted(names)}")
    print("Storm inventory: all 19 calibrated cases selected, including the traffic storm", flush=True)


def validate_failure(output: str, returncode: int, expected: str, sibling: str, diagnostic: str) -> None:
    """A build error, missing test, hang or unrelated panic is not a control pass."""
    plain = ANSI.sub("", output)
    failed = set(re.findall(r"^\s*FAIL\s+\[[^\]]+\]\s+(?:\(\s*\d+/\d+\)\s+)?\S+\s+(\S+)\s*$", plain, re.MULTILINE))
    passed = set(re.findall(r"^\s*PASS\s+\[[^\]]+\]\s+(?:\(\s*\d+/\d+\)\s+)?\S+\s+(\S+)\s*$", plain, re.MULTILINE))
    if returncode != 100:
        raise RuntimeError(f"expected nextest test-failure exit100, got {returncode}")
    if failed != {expected} or sibling not in passed:
        raise RuntimeError(f"expected only {expected} to fail and {sibling} to pass; failed={sorted(failed)}, passed={sorted(passed)}")
    if not re.search(r"Summary\s+\[[^\]]+\]\s+2 tests run:.*1 passed.*1 failed", plain):
        raise RuntimeError("control must execute both named tests with exactly one failure")
    if diagnostic not in plain:
        raise RuntimeError(f"missing original deadline diagnostic: {diagnostic}")
    if re.search(r"^\s*(?:TIMEOUT|ABORT)\s+\[", plain, re.MULTILINE):
        raise RuntimeError("a harness timeout/abort is not a deadline-assertion failure")


def run_controls(profile: str, output_dir: Path) -> None:
    output_dir.mkdir(parents=True, exist_ok=True)
    command = [
        "cargo", "nextest", "run", "--no-fail-fast", "--profile", "default",
        "--cargo-profile", profile, "--locked", "-p", "agent-file-tools",
        "--test", "integration", "-E", f"test(={ACK_TEST}) | test(={READ_TEST})",
    ]
    evidence = []
    for control, expected, sibling, diagnostic in CONTROLS:
        env = {**os.environ, "AFT_STORM_POSITIVE_CONTROL": control}
        print(f"AFT_STORM_POSITIVE_CONTROL={control}: expect FAIL {expected}; PASS {sibling}", flush=True)
        result = subprocess.run(command, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        print(result.stdout, flush=True)
        (output_dir / f"{control}.log").write_text(result.stdout)
        validate_failure(result.stdout, result.returncode, expected, sibling, diagnostic)
        evidence.append({"control": control, "failed": expected, "passed": sibling, "exit_code": result.returncode, "diagnostic": diagnostic})
    (output_dir / "controls.json").write_text(json.dumps(evidence, indent=2) + "\n")
    print("Storm deadline controls passed: 2 delayed paths rejected; each sibling remained green", flush=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cargo-profile", default="ci-storm")
    parser.add_argument("--output-dir", type=Path, default=Path("target/storm-controls"))
    parser.add_argument("--check-inventory", type=Path)
    args = parser.parse_args()
    print(f"Python {sys.version.split()[0]}; storm control profile={args.cargo_profile}", flush=True)
    if args.check_inventory:
        check_inventory(json.loads(args.check_inventory.read_text()))
        return
    run_controls(args.cargo_profile, args.output_dir)


if __name__ == "__main__":
    main()
