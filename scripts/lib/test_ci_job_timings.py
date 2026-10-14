#!/usr/bin/env python3
"""Tests for queue, step classification, and attempt-specific CI measurements."""

import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "ci_job_timings", Path(__file__).resolve().parents[1] / "ci-job-timings.py",
)
assert SPEC is not None and SPEC.loader is not None
timings = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(timings)


class CIJobTimingTests(unittest.TestCase):
    def test_setup_actions_are_not_counted_as_test_execution(self):
        for action in ("Run Swatinem/rust-cache@v2", "Run mozilla-actions/sccache-action@v0.0.11",
                       "Run oven-sh/setup-bun@v2", "Run dtolnay/rust-toolchain@stable"):
            self.assertEqual(timings.step_category(action), "setup_and_other")

    def test_combined_compile_test_steps_are_not_presented_as_pure_tests(self):
        for step in ("Release storm tests (production-calibrated latency bounds)",
                     "Rust test gate (unit on libtest, integration on nextest, watcher isolated)",
                     "Evaluate committed search-quality reference"):
            self.assertEqual(timings.step_category(step), "compile_and_test")
        self.assertEqual(timings.step_category("Build nextest archive (macOS lib tests)"), "compile")
        self.assertEqual(timings.step_category("Run macOS E2E suite"), "test")

    def test_missing_timestamps_are_unknown_not_zero_passes(self):
        self.assertIsNone(timings.seconds("0001-01-01T00:00:00Z", "2026-10-14T00:00:00Z"))
        self.assertIsNone(timings.seconds(None, "2026-10-14T00:00:00Z"))

    def test_queue_is_job_creation_to_start_not_workflow_dependency_wait(self):
        job = {
            "name": "dependent shard", "conclusion": "success", "steps": [],
            "createdAt": "2026-10-14T00:20:00Z", "startedAt": "2026-10-14T00:20:07Z",
            "completedAt": "2026-10-14T00:25:07Z",
        }
        measured = timings.job_timings(job)
        self.assertEqual(measured["queue_seconds"], 7)
        self.assertEqual(measured["execution_seconds"], 300)

    def test_warm_wall_time_uses_attempt_start_not_original_creation(self):
        job = {
            "databaseId": 42, "name": "build", "conclusion": "success", "steps": [],
            "startedAt": "2026-10-15T00:00:07Z", "completedAt": "2026-10-15T00:02:00Z",
        }
        replies = [
            {"attempt": 2, "jobs": [job], "createdAt": "2026-10-14T00:00:00Z"},
            {"status": "completed", "conclusion": "success", "run_started_at": "2026-10-15T00:00:00Z"},
            {"total_count": 1, "jobs": [{"id": 42, "created_at": "2026-10-15T00:00:00Z",
                                         "completed_at": "2026-10-15T00:02:00Z"}]},
        ]
        with patch.object(timings, "gh_json", side_effect=replies):
            run = timings.collect("cortexkit/aft", 99, 2)
        self.assertEqual(run["wall_seconds"], 120)
        self.assertEqual(run["timings"][0]["queue_seconds"], 7)

    def test_in_progress_run_is_not_a_finished_measurement(self):
        with patch.object(timings, "gh_json", side_effect=[{"attempt": 1}, {"status": "in_progress"}]):
            with self.assertRaisesRegex(ValueError, "not completed"):
                timings.collect("cortexkit/aft", 99)


if __name__ == "__main__":
    unittest.main()
