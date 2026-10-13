#!/usr/bin/env python3
"""Unit tests for GitHub Actions branch-filter glob semantics."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import re
import unittest


MODULE_PATH = Path(__file__).with_name("workflow-gates.py")
SPEC = importlib.util.spec_from_file_location("workflow_gates", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
workflow_gates = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(workflow_gates)

WORKFLOW_TEXT = (
    Path(__file__).resolve().parents[2] / ".github/workflows/_unit-suite.yml"
).read_text()
GATE_TEXT = (Path(__file__).resolve().parents[2] / "scripts/rust-test-gate.sh").read_text()
PROOF_TEXT = (Path(__file__).resolve().parents[2] / "scripts/rust-test-inventory-proof.py").read_text()


def workflow_job(job_id: str) -> str:
    match = re.search(
        rf"(?ms)^  {re.escape(job_id)}:\n(.*?)(?=^  [a-z0-9_-]+:\n|\Z)",
        WORKFLOW_TEXT,
    )
    if match is None:
        raise AssertionError(f"workflow job {job_id!r} is missing")
    return match.group(1)


class BranchPatternTests(unittest.TestCase):
    def test_single_star_does_not_match_slash(self) -> None:
        self.assertTrue(workflow_gates.branch_patterns_include("train/fix", ["train/*"]))
        self.assertFalse(workflow_gates.branch_patterns_include("train/feature/fix", ["train/*"]))

    def test_double_star_matches_nested_branches(self) -> None:
        self.assertTrue(workflow_gates.branch_patterns_include("train/feature/fix", ["train/**"]))

    def test_question_mark_matches_exactly_one_non_slash_character(self) -> None:
        self.assertTrue(workflow_gates.branch_patterns_include("train/a", ["train/?"]))
        self.assertFalse(workflow_gates.branch_patterns_include("train/ab", ["train/?"]))
        self.assertFalse(workflow_gates.branch_patterns_include("train/a/b", ["train/?"]))

    def test_character_class_and_range(self) -> None:
        self.assertTrue(workflow_gates.branch_patterns_include("release/7", ["release/[0-9]"]))
        self.assertFalse(workflow_gates.branch_patterns_include("release/a", ["release/[0-9]"]))

    def test_leading_negation_excludes_then_later_positive_reincludes(self) -> None:
        patterns = ["train/**", "!train/private/**"]
        self.assertFalse(workflow_gates.branch_patterns_include("train/private/secret", patterns))
        self.assertTrue(workflow_gates.branch_patterns_include(
            "train/private/approved", [*patterns, "train/private/approved"],
        ))


class MacOSUnitWorkflowTests(unittest.TestCase):
    def test_required_macos_check_aggregates_every_lane(self) -> None:
        summary = workflow_job("unit-macos")
        expected_lanes = {
            "unit-macos-nextest-archive",
            "unit-macos-libtest-archive",
            "unit-macos-inventory-proof",
            "unit-macos-libtest-shard",
            "unit-macos-nextest",
            "unit-macos-watcher",
            "unit-macos-conformance",
        }

        self.assertIn("name: Unit tests (macOS)", summary)
        self.assertRegex(summary, r"(?m)^    if: always\(\)$")
        for lane in expected_lanes:
            self.assertIn(f"- {lane}", summary)
            self.assertIn(f"needs.{lane}.result", summary)
        self.assertIn('if [[ "$result" != "success" ]]', summary)

    def test_macos_rust_buckets_and_bun_suite_run_in_parallel_lanes(self) -> None:
        integration_archive = workflow_job("unit-macos-nextest-archive")
        self.assertIn("kind(test) - binary(=watcher_integration)", integration_archive)
        self.assertIn("macos-integration-inventory.json", integration_archive)
        self.assertIn("macos-doctest-inventory.txt", integration_archive)

        libtest_archive = workflow_job("unit-macos-libtest-archive")
        self.assertIn("(kind(lib) | kind(bin))", libtest_archive)
        self.assertIn("macos-libtest-inventory.json", libtest_archive)

        inventory = workflow_job("unit-macos-inventory-proof")
        self.assertIn("AFT_NEXTEST_ARCHIVE_LIB_BIN_FILE", inventory)
        self.assertIn("AFT_NEXTEST_ARCHIVE_INTEGRATION_FILE", inventory)
        self.assertIn("AFT_NEXTEST_SOURCE_INVENTORY_LIB_BIN_FILE", inventory)
        self.assertIn("AFT_NEXTEST_SOURCE_INVENTORY_INTEGRATION_FILE", inventory)
        self.assertIn("AFT_WATCHER_TEST_INVENTORY_FILE", inventory)
        self.assertIn("scripts/rust-test-inventory-proof.py", inventory)

        libtest = workflow_job("unit-macos-libtest-shard")
        self.assertIn("unit-macos-libtest-archive", libtest)
        self.assertIn("macos-libtest-archive", libtest)
        self.assertIn("max-parallel: 3", libtest)
        self.assertIn("AFT_GATE_PHASES: ${{ matrix.shard == 1 && 'lib,lib-shard' || 'lib-shard' }}", libtest)
        self.assertIn("AFT_NEXTEST_PARTITION: hash:${{ matrix.shard }}/3", libtest)
        self.assertIn("AFT_NEXTEST_EXTRACT_TO: ${{ github.workspace }}", libtest)

        integration = workflow_job("unit-macos-nextest")
        self.assertIn("max-parallel: 3", integration)
        self.assertIn("AFT_GATE_PHASES: nextest", integration)
        self.assertIn("AFT_NEXTEST_PARTITION: hash:${{ matrix.shard }}/3", integration)
        self.assertIn("macos-nextest-archive", integration)

        watcher = workflow_job("unit-macos-watcher")
        self.assertIn("AFT_GATE_PHASES: watcher", watcher)

        conformance = workflow_job("unit-macos-conformance")
        self.assertIn("AFT_GATE_PHASES: conformance", conformance)
        self.assertIn("bun run test:unit", conformance)

    def test_macos_libtest_archives_are_prewarmed_before_keychain_test(self) -> None:
        prewarm_start = GATE_TEXT.index("if phase_enabled lib-shard")
        prewarm = GATE_TEXT.index("extract nextest archive for macOS libtest prewarming")
        warm = GATE_TEXT.index("./scripts/warm-macos-nextest-archive.sh", prewarm_start)
        keychain = GATE_TEXT.index("if phase_enabled lib; then")

        self.assertLess(prewarm_start, prewarm)
        self.assertLess(prewarm, keychain)
        self.assertLess(warm, keychain)
        self.assertIn("extract_nextest_archive_for_prewarm", GATE_TEXT[prewarm_start:keychain])

    def test_macos_backtrace_probe_runs_once_in_archive_builder_not_shards(self) -> None:
        probe = "cargo test -p agent-file-tools --lib panic_hook_preserves_default_backtrace_behavior_and_persists"
        archive = workflow_job("unit-macos-libtest-archive")
        shard = workflow_job("unit-macos-libtest-shard")

        self.assertEqual(archive.count(probe), 1)
        self.assertNotIn(probe, shard)
        self.assertIn(probe, GATE_TEXT)
        self.assertIn('"$(uname)" != "Darwin"', GATE_TEXT)
        self.assertNotIn(
            "cargo nextest run --archive-file $AFT_NEXTEST_ARCHIVE_FILE -E kind(lib) panic_hook_preserves_default_backtrace_behavior_and_persists",
            GATE_TEXT,
        )
        self.assertIn(
            "serial_backtrace_candidates",
            PROOF_TEXT,
        )
        self.assertIn("serial_tests_in_unit_partitions", PROOF_TEXT)
        self.assertIn("Serial-probe inventory:", PROOF_TEXT)

    def test_macos_inventory_proof_checks_source_archives_and_partitions(self) -> None:
        inventory = workflow_job("unit-macos-inventory-proof")
        for variable in (
            "AFT_NEXTEST_ARCHIVE_LIB_BIN_FILE",
            "AFT_NEXTEST_ARCHIVE_INTEGRATION_FILE",
            "AFT_NEXTEST_SOURCE_INVENTORY_LIB_BIN_FILE",
            "AFT_NEXTEST_SOURCE_INVENTORY_INTEGRATION_FILE",
            "AFT_WATCHER_TEST_INVENTORY_FILE",
            "AFT_DOCTEST_INVENTORY_FILE",
        ):
            self.assertIn(variable, inventory)
        self.assertIn("lib_bin_archive_errors = partition_coverage_errors(", PROOF_TEXT)
        self.assertIn("integration_archive_errors = partition_coverage_errors(", PROOF_TEXT)
        self.assertIn("or any(source_archive_errors)", PROOF_TEXT)
        self.assertIn("or partition_errors", PROOF_TEXT)
        self.assertIn(
            "partition_coverage_errors(unit_bin_shardable_entries, unit_bin_partitions)",
            PROOF_TEXT,
        )
        self.assertIn(
            "partition_coverage_errors(nextest_shardable_entries, nextest_partitions)",
            PROOF_TEXT,
        )


if __name__ == "__main__":
    unittest.main()
