#!/usr/bin/env python3
"""Unit tests for GitHub Actions branch-filter glob semantics."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import unittest


MODULE_PATH = Path(__file__).with_name("workflow-gates.py")
SPEC = importlib.util.spec_from_file_location("workflow_gates", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
workflow_gates = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(workflow_gates)


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


if __name__ == "__main__":
    unittest.main()
