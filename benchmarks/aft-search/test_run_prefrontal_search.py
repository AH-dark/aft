#!/usr/bin/env python3
"""Verify optional local fixture rows are announced, evaluated and cannot replace committed tasks."""
from __future__ import annotations

import contextlib
import copy
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

import run_prefrontal_search as runner

PIN = "a" * 40


def task(identifier: str) -> dict:
    return {
        "id": identifier,
        "query": f"question for {identifier}",
        "repo": "sample",
        "failure_class": "unclassified",
        "include_tests": False,
        "ground_truth": [{"file_path": "src/answer.rs", "line_start": 1, "line_end": 1, "relevance": 2}],
    }


class ExtraRowsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.extra_path = self.root / "extra.json"
        self.main_tasks = [task("main")]

    def write_extra(self, **updates: object) -> None:
        payload = {"schema_version": 1, "pinned_commit": PIN, "task_count": 1, "tasks": [task("extra")]}
        payload.update(updates)
        self.extra_path.write_text(json.dumps(payload))

    def load(self) -> tuple[list, dict]:
        return runner.load_extra_rows(self.extra_path, ["sample"], self.main_tasks, PIN)

    def test_parser_defaults_to_local_investigation_rows(self) -> None:
        self.assertEqual(Path(runner.parse_args([]).extra_fixtures), runner.PRIVATE_EXTRA_ROWS)
        self.assertEqual(runner.PRIVATE_EXTRA_ROWS.relative_to(runner.ROOT).parts[:2], (".cortexkit", "internal-docs"))

    def test_absent_extra_rows_are_announced(self) -> None:
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            rows, status = self.load()
        self.assertEqual(rows, [])
        self.assertEqual(status["status"], "absent")
        self.assertEqual(status["task_count"], 0)
        self.assertIn("prefrontal_extra_rows:absent:", output.getvalue())

    def test_present_extra_rows_load_without_mutating_main(self) -> None:
        self.write_extra()
        original = copy.deepcopy(self.main_tasks)
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            rows, status = self.load()
        self.assertEqual([row["id"] for row in rows], ["extra"])
        self.assertEqual(self.main_tasks, original)
        self.assertEqual(status["status"], "loaded")
        self.assertEqual(status["task_count"], 1)
        self.assertIn("prefrontal_extra_rows:loaded:", output.getvalue())

    def test_extra_rows_cannot_replace_main_ids(self) -> None:
        self.write_extra(tasks=[task("main")])
        with self.assertRaisesRegex(ValueError, "duplicates main"):
            self.load()

    def test_extra_rows_must_match_the_pin(self) -> None:
        self.write_extra(pinned_commit="b" * 40)
        with self.assertRaisesRegex(ValueError, "pinned_commit"):
            self.load()

    def test_extra_row_count_is_validated(self) -> None:
        self.write_extra(task_count=2)
        with self.assertRaisesRegex(ValueError, "task_count"):
            self.load()

    def test_extra_rows_reuse_repo_and_duplicate_validation(self) -> None:
        unknown = task("extra")
        unknown["repo"] = "unknown"
        for rows in [[unknown], [task("extra"), task("extra")]]:
            with self.subTest(rows=rows):
                self.write_extra(tasks=rows, task_count=len(rows))
                with self.assertRaises(ValueError):
                    self.load()

    def test_malformed_extra_rows_are_rejected_not_ignored(self) -> None:
        variants = [1, {"id": "missing-fields"}]
        for update in [
            {"query": ""},
            {"include_tests": "false"},
            {"ground_truth": ["not a truth object"]},
            {"ground_truth": [{"file_path": "../outside.rs", "line_start": 1, "line_end": 1}]},
            {"ground_truth": [{"file_path": "src/answer.rs", "line_start": 2, "line_end": 1}]},
        ]:
            variants.append({**task("extra"), **update})
        for row in variants:
            with self.subTest(row=row):
                self.write_extra(tasks=[row])
                with self.assertRaises(ValueError):
                    self.load()

    def run_report(self, *, present: bool) -> tuple[dict, list[str]]:
        if present:
            self.write_extra()
        binary = self.root / "aft"
        binary.write_text("fixture")
        corpus = self.root / "corpus.toml"
        corpus.write_text("fixture")
        fixtures = self.root / "main.json"
        fixtures.write_text(json.dumps({"pinned_commit": PIN, "tasks": self.main_tasks}))
        out = self.root / "report.json"
        args = runner.parse_args(["--binary", str(binary), "--corpus", str(corpus), "--fixtures", str(fixtures), "--extra-fixtures", str(self.extra_path), "--out", str(out)])
        evaluated = []
        client = Mock()
        client.wait_for_indexes.return_value = {}

        def evaluate(_client: object, row: dict, _root: Path, _top_k: int) -> dict:
            evaluated.append(row["id"])
            return {"task_id": row["id"], "first_relevant_rank_line_overlap": 1, "first_relevant_rank_file": 1, "failure_class": row["failure_class"]}

        with (
            patch.object(runner, "ROOT", self.root),
            patch.object(runner, "parse_corpus_toml", return_value=({}, [{"name": "sample"}])),
            patch.object(runner, "validate_corpus", return_value=self.root),
            patch.object(runner, "check_ground_truth"),
            patch.object(runner, "ensure_local_model_env", return_value={}),
            patch.object(runner, "AftClient", return_value=client),
            patch.object(runner, "git_rev", return_value=PIN),
            patch.object(runner, "binary_version", return_value="fixture"),
            patch.object(runner, "binary_sha256", return_value="fixture"),
            patch.object(runner, "evaluate_task", side_effect=evaluate),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(runner.run(args), 0)
        return json.loads(out.read_text()), evaluated

    def test_present_extra_rows_are_evaluated_and_reported(self) -> None:
        report, evaluated = self.run_report(present=True)
        self.assertEqual(evaluated, ["main", "extra"])
        self.assertEqual([row["task_id"] for row in report["tasks"]], ["main", "extra"])
        self.assertEqual(report["extra_rows"]["status"], "loaded")
        self.assertEqual(report["extra_rows"]["task_count"], 1)

    def test_absent_extra_rows_are_reported_by_the_runner(self) -> None:
        report, evaluated = self.run_report(present=False)
        self.assertEqual(evaluated, ["main"])
        self.assertEqual(report["extra_rows"]["status"], "absent")
        self.assertEqual(report["extra_rows"]["task_count"], 0)


if __name__ == "__main__":
    unittest.main()
