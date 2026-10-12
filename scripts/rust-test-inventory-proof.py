#!/usr/bin/env python3
"""Prove that every Rust test lands in exactly one split-gate bucket.

This script captures the current workspace inventory from `cargo test --workspace
-- --list`, then compares it against the split buckets used by
`scripts/rust-test-gate.sh`:

* `cargo nextest list --workspace --lib --bins`
* `cargo nextest list --workspace -E 'kind(test) - binary(=watcher_integration)'`
* `cargo test -p agent-file-tools --test watcher_integration`

When `AFT_NEXTEST_ARCHIVE_FILE` is set, the archived test inventory is also
compared by test identity with the two non-watcher nextest buckets.
Doctests are checked as a separate cargo bucket only when the baseline inventory
contains doctest cases.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
HASH_SUFFIX_RE = re.compile(r"-[0-9a-f]+(?:\.exe)?$")
RUNNING_RE = re.compile(r"^\s*Running (.+) \((.+)\)$")
COUNT_RE = re.compile(r"^\d+ tests?, \d+ benchmarks$")
TEST_LINE_RE = re.compile(r"^(.*): test$")
# A rustc diagnostic's source excerpt: `8845 |  code` and `|  ^^^ label` lines.
WARNING_EXCERPT_RE = re.compile(r"^(?:\d+\s*)?\|\s")


@dataclass(frozen=True)
class CargoEntry:
    kind: str
    suite: str
    source: str
    name: str

    @property
    def key(self) -> str:
        if self.kind in {"lib", "bin", "test"}:
            return f"{self.kind}|{self.suite}|{self.name}"
        return f"{self.kind}|{self.suite}|{self.source}|{self.name}"


@dataclass(frozen=True)
class NextestEntry:
    kind: str
    suite: str
    name: str

    @property
    def key(self) -> str:
        return f"{self.kind}|{self.suite}|{self.name}"


def run_command(command: list[str], merge_stderr: bool = True) -> str:
    completed = subprocess.run(
        command,
        cwd=ROOT,
        stdout=subprocess.PIPE,
        # Cargo writes each `Running ...` suite boundary to stderr but writes
        # that suite's test names to stdout. Merge the streams to preserve their
        # ordering, then let the parser ignore ordinary compile progress. JSON
        # listings keep stderr apart, because replayed compiler warnings would
        # otherwise precede the JSON document.
        stderr=subprocess.STDOUT if merge_stderr else subprocess.PIPE,
        text=True,
        check=False,
        # ANSI color wraps cargo's output and silently breaks the line parsers
        # below when a CI environment exports CARGO_TERM_COLOR=always.
        env={**os.environ, "CARGO_TERM_COLOR": "never"},
    )
    filtered_output = completed.stdout or ""
    if completed.returncode != 0:
        sys.stderr.write(f"command failed ({completed.returncode}): {' '.join(command)}\n")
        sys.stderr.write(filtered_output)
        sys.stderr.write(completed.stderr or "")
        sys.exit(completed.returncode)
    return filtered_output

def suite_name_from_artifact(artifact_path: str) -> str:
    artifact = Path(artifact_path).name
    return HASH_SUFFIX_RE.sub("", artifact)


def parse_cargo_list(output: str) -> list[CargoEntry]:
    current_kind: str | None = None
    current_suite: str | None = None
    current_source: str | None = None
    entries: list[CargoEntry] = []

    for raw_line in output.splitlines():
        line = raw_line.rstrip("\n")
        stripped = line.strip()
        if not stripped or stripped.startswith(
            ("Compiling ", "Checking ", "Finished `", "Blocking waiting for file lock")
        ):
            continue

        # Compiler warnings (for example the macOS linker's `__eh_frame section too
        # large` note once the lib test binary passes 16 MB of unwind data) print
        # a `warning:` line plus `-->`, source-excerpt, `|` and `= note:`
        # continuation lines before the listing. sccache prints its own
        # `sccache: warning:` lines when its server restarts mid-build. None of
        # them carry test entries.
        if (
            stripped.startswith(("warning:", "= note:", "= help:", "sccache: ", "--> ", "..."))
            or stripped == "|"
            or WARNING_EXCERPT_RE.match(stripped)
        ):
            continue

        if stripped.startswith("Doc-tests "):
            current_kind = "doc"
            current_suite = stripped.removeprefix("Doc-tests ")
            current_source = current_suite
            continue

        running_match = RUNNING_RE.match(line)
        if running_match:
            source = running_match.group(1)
            artifact_path = running_match.group(2)
            suite = suite_name_from_artifact(artifact_path)
            kind = "test"
            if source.startswith("unittests "):
                unit_source = source.removeprefix("unittests ")
                kind = "lib" if unit_source == "src/lib.rs" else "bin"
                source = unit_source
            current_kind = kind
            current_suite = suite
            current_source = source
            continue

        if COUNT_RE.match(stripped):
            continue

        test_match = TEST_LINE_RE.match(stripped)
        if test_match and current_kind and current_suite and current_source:
            entries.append(
                CargoEntry(
                    kind=current_kind,
                    suite=current_suite,
                    source=current_source,
                    name=test_match.group(1),
                )
            )
            continue

        raise RuntimeError(f"unrecognized cargo list line: {line}")

    return entries


def parse_nextest_list(
    output: str,
    allowed_suite_kinds: set[str] | None = None,
    matching_tests_only: bool = False,
) -> list[NextestEntry]:
    allowed_suite_kinds = allowed_suite_kinds or {"test"}
    payload = json.loads(output)
    entries: list[NextestEntry] = []
    for suite_data in payload.get("rust-suites", {}).values():
        suite_kind = suite_data.get("kind")
        if suite_kind not in allowed_suite_kinds:
            continue
        suite = suite_data["binary-name"]
        for name, testcase in suite_data.get("testcases", {}).items():
            if testcase.get("kind") != "test":
                continue
            if (
                matching_tests_only
                and testcase.get("filter-match", {}).get("status") != "matches"
            ):
                continue
            entries.append(NextestEntry(kind=suite_kind, suite=suite, name=name))
    return entries


def partition_nextest_entries(
    command: list[str], allowed_suite_kinds: set[str] | None = None
) -> list[list[NextestEntry]]:
    entries = []
    for shard in range(1, 4):
        command_args = [*command]
        if "--" in command_args:
            separator = command_args.index("--")
            command_args[separator:separator] = ["--partition", f"hash:{shard}/3"]
        else:
            command_args.extend(["--partition", f"hash:{shard}/3"])
        entries.append(
            parse_nextest_list(
                run_command(command_args, merge_stderr=False),
                allowed_suite_kinds,
                matching_tests_only=True,
            )
        )
    return entries


def partition_coverage_errors(
    expected_entries: list[NextestEntry], partitions: list[list[NextestEntry]]
) -> tuple[list[str], list[str], list[str]]:
    expected = Counter(entry.key for entry in expected_entries)
    actual = Counter(entry.key for partition in partitions for entry in partition)
    missing = sorted(key for key, count in expected.items() if actual[key] < count)
    duplicate = sorted(key for key, count in actual.items() if count > expected[key])
    extra = sorted(key for key in actual if key not in expected)
    return missing, duplicate, extra


def summarize(counter: Counter[str]) -> list[str]:
    return [f"{bucket}={counter[bucket]}" for bucket in sorted(counter)]


baseline_output = run_command(["cargo", "test", "--workspace", "--", "--list"])
baseline_entries = parse_cargo_list(baseline_output)

unit_bin_command = [
    "cargo",
    "nextest",
    "list",
    "--profile",
    "gate",
    "--workspace",
    "--lib",
    "--bins",
    "-T",
    "json",
    "--cargo-quiet",
]
unit_bin_output = run_command(unit_bin_command, merge_stderr=False)
unit_bin_entries = parse_nextest_list(unit_bin_output, {"lib", "bin"})
unit_bin_partition_command = [
    *unit_bin_command,
    "--",
    "--skip",
    "platform_verifier_tls_client_subprocess",
]
unit_bin_partition_output = run_command(unit_bin_partition_command, merge_stderr=False)
unit_bin_shardable_entries = parse_nextest_list(
    unit_bin_partition_output, {"lib", "bin"}, matching_tests_only=True
)
unit_bin_partitions = partition_nextest_entries(
    unit_bin_partition_command, {"lib", "bin"}
)
serial_tls_entries = [
    entry
    for entry in unit_bin_entries
    if entry.kind == "lib"
    and entry.name.endswith("platform_verifier_tls_client_subprocess")
]

doc_baseline_entries = [entry for entry in baseline_entries if entry.kind == "doc"]
doc_entries: list[CargoEntry] = []
if doc_baseline_entries:
    doc_output = run_command(["cargo", "test", "--workspace", "--doc", "--", "--list"])
    doc_entries = [entry for entry in parse_cargo_list(doc_output) if entry.kind == "doc"]

nextest_command = [
    "cargo",
    "nextest",
    "list",
    "--profile",
    "gate",
    "--workspace",
    "-E",
    "kind(test) - binary(=watcher_integration)",
    "-T",
    "json",
    "--cargo-quiet",
]
nextest_output = run_command(nextest_command, merge_stderr=False)
nextest_entries = parse_nextest_list(nextest_output)
nextest_shardable_entries = parse_nextest_list(nextest_output, matching_tests_only=True)
nextest_partitions = partition_nextest_entries(nextest_command)

archive_path = os.environ.get("AFT_NEXTEST_ARCHIVE_FILE")
archive_entries: list[NextestEntry] | None = None
if archive_path:
    archive_output = run_command(
        [
            "cargo",
            "nextest",
            "list",
            "--archive-file",
            archive_path,
            "--profile",
            "gate",
            "-E",
            "(kind(lib) | kind(bin) | kind(test)) - binary(=watcher_integration)",
            "-T",
            "json",
        ],
        merge_stderr=False,
    )
    archive_entries = parse_nextest_list(archive_output, {"lib", "bin", "test"})

watcher_output = run_command([
    "cargo",
    "test",
    "-p",
    "agent-file-tools",
    "--test",
    "watcher_integration",
    "--",
    "--list",
])
watcher_entries = [entry for entry in parse_cargo_list(watcher_output) if entry.kind == "test"]

baseline_by_bucket = Counter(entry.kind for entry in baseline_entries)
coverage: dict[str, list[str]] = {}

for entry in unit_bin_entries:
    coverage.setdefault(entry.key, []).append("nextest-lib-bin")
for entry in doc_entries:
    coverage.setdefault(entry.key, []).append("cargo-doc")
for entry in nextest_entries:
    coverage.setdefault(entry.key, []).append("nextest-integration")
for entry in watcher_entries:
    coverage.setdefault(entry.key, []).append("cargo-watcher")

baseline_keys = {entry.key for entry in baseline_entries}
covered_keys = set(coverage)
uncovered = sorted(key for key in baseline_keys if key not in covered_keys)
double_covered = sorted(
    (key, buckets) for key, buckets in coverage.items() if key in baseline_keys and len(buckets) > 1
)
extra_keys = sorted(key for key in covered_keys if key not in baseline_keys)
watcher_in_nextest = sorted(
    entry.key for entry in nextest_entries if entry.suite == "watcher_integration"
)
unit_partition_missing, unit_partition_duplicate, unit_partition_extra = partition_coverage_errors(
    unit_bin_shardable_entries, unit_bin_partitions
)
nextest_partition_missing, nextest_partition_duplicate, nextest_partition_extra = partition_coverage_errors(
    nextest_shardable_entries, nextest_partitions
)
watcher_in_nextest_partitions = sorted(
    entry.key
    for partition in nextest_partitions
    for entry in partition
    if entry.suite == "watcher_integration"
)
archive_inventory_errors = ([], [], [])
if archive_entries is not None:
    archive_inventory_errors = partition_coverage_errors(
        unit_bin_entries + nextest_entries, [archive_entries]
    )

print("Baseline counts:", ", ".join(summarize(baseline_by_bucket)))
print(
    "Split counts:",
    ", ".join(
        [
            f"nextest-lib-bin={len(unit_bin_entries)}",
            f"cargo-doc={len(doc_entries)}",
            f"nextest-integration={len(nextest_entries)}",
            f"cargo-watcher={len(watcher_entries)}",
        ]
    ),
)
print(f"Watcher tests in nextest inventory: {len(watcher_in_nextest)}")
print(f"Doctest cases in baseline inventory: {len(doc_baseline_entries)}")
print(
    "Hash partition counts: "
    f"nextest-lib-bin={','.join(str(len(partition)) for partition in unit_bin_partitions)}; "
    f"nextest-integration={','.join(str(len(partition)) for partition in nextest_partitions)}"
)
if archive_entries is not None:
    print(f"Archive inventory entries: {len(archive_entries)}")

if (
    uncovered
    or double_covered
    or extra_keys
    or watcher_in_nextest
    or len(serial_tls_entries) != 1
    or unit_partition_missing
    or unit_partition_duplicate
    or unit_partition_extra
    or nextest_partition_missing
    or nextest_partition_duplicate
    or nextest_partition_extra
    or watcher_in_nextest_partitions
    or any(archive_inventory_errors)
):
    print("\nInventory proof failed.")
    if uncovered:
        print(f"  uncovered ({len(uncovered)}):")
        for key in uncovered[:20]:
            print(f"    {key}")
    if double_covered:
        print(f"  double-covered ({len(double_covered)}):")
        for key, buckets in double_covered[:20]:
            print(f"    {key} -> {', '.join(buckets)}")
    if extra_keys:
        print(f"  extra split-only entries ({len(extra_keys)}):")
        for key in extra_keys[:20]:
            print(f"    {key}")
    if watcher_in_nextest:
        print(f"  watcher leaked into nextest ({len(watcher_in_nextest)}):")
        for key in watcher_in_nextest[:20]:
            print(f"    {key}")
    if len(serial_tls_entries) != 1:
        print(
            "  serial platform-verifier TLS test entries: "
            f"expected 1, found {len(serial_tls_entries)}"
        )
    for label, missing, duplicate, extra in (
        (
            "nextest lib/bin partitions",
            unit_partition_missing,
            unit_partition_duplicate,
            unit_partition_extra,
        ),
        (
            "nextest integration partitions",
            nextest_partition_missing,
            nextest_partition_duplicate,
            nextest_partition_extra,
        ),
    ):
        for category, keys in (("uncovered", missing), ("repeated", duplicate), ("extra", extra)):
            if keys:
                print(f"  {label} {category} ({len(keys)}):")
                for key in keys[:20]:
                    print(f"    {key}")
    if watcher_in_nextest_partitions:
        print(f"  watcher leaked into nextest partitions ({len(watcher_in_nextest_partitions)}):")
        for key in watcher_in_nextest_partitions[:20]:
            print(f"    {key}")
    for category, keys in zip(("uncovered", "repeated", "extra"), archive_inventory_errors):
        if keys:
            print(f"  archive inventory {category} ({len(keys)}):")
            for key in keys[:20]:
                print(f"    {key}")
    sys.exit(1)

print("\nInventory proof passed: every baseline test is covered exactly once.")
