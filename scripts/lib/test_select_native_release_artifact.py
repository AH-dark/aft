#!/usr/bin/env python3
"""Select artifact reuse only from an exact successful main/train push candidate."""

import importlib.util
from pathlib import Path
import unittest


SPEC = importlib.util.spec_from_file_location("select_native", Path(__file__).resolve().parents[1] / "select-native-release-artifact.py")
assert SPEC is not None and SPEC.loader is not None
selector = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(selector)


class NativeReleaseSelectionTests(unittest.TestCase):
    def test_only_exact_successful_main_or_train_candidates_are_selected(self):
        def run(identifier, branch="main", sha="a" * 40, result="success"):
            return {"databaseId": identifier, "headBranch": branch, "headSha": sha, "conclusion": result}
        rows = [run(1), run(2, "train/fix"), run(3, "ci-speed/release-probe"),
                run(4, "main", "b" * 40), run(5, result="failure")]
        self.assertEqual(selector.choose_run(rows, "a" * 40), [2, 1])

    def test_no_successful_candidate_requires_source_build_fallback(self):
        self.assertEqual(selector.choose_run([], "a" * 40), [])


if __name__ == "__main__":
    unittest.main()
