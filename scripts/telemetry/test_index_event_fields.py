#!/usr/bin/env python3
"""Read the same bounded long-root record asserted by the Rust formatter test."""

import importlib.util
import sys
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent


def load_script(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, SCRIPT_DIR / filename)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {filename}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


gate = load_script("cost_gate_index_event_test", "cost-gate.py")
census = load_script("census_index_event_test", "index-census.py")
matrix = load_script("matrix_index_event_test", "oss-matrix.py")
index_event_root_matches = census.index_event_root_matches

FIXTURE = SCRIPT_DIR / "fixtures" / "index-event-long-root.txt"


class IndexEventFieldsTests(unittest.TestCase):
    def test_cost_gate_reads_all_measurements_from_bounded_long_root_record(self):
        line = FIXTURE.read_text().strip()
        self.assertLessEqual(len(line.encode()), 300)
        events = gate.parse_index_events(str(FIXTURE))
        self.assertEqual(events["semantic.first_query.count"], 1)
        for field, expected in (
            ("elapsed_ms", 5678),
            ("ready_to_first_query_ms", 9012),
            ("completed", 345),
            ("total", 678),
        ):
            self.assertEqual(events[f"semantic.first_query.{field}.p50"], expected)
            self.assertEqual(events[f"semantic.first_query.{field}.max"], expected)

    def test_census_and_matrix_parse_the_bounded_record(self):
        line = FIXTURE.read_text().strip()
        fields = census.parse_index_event_fields(line)
        self.assertEqual(fields, matrix.index_event_fields(line))
        self.assertEqual(fields["ready_to_first_query_ms"], "9012")
        self.assertEqual(fields["root"], ".../f2f0351c/typescript-eslint/worker")

    def test_hashed_root_attribution_distinguishes_equal_suffixes(self):
        suffix = "/typescript-eslint/worker"
        root = "/" + "a" * (300 - 1 - len(suffix)) + suffix
        logged = ".../f2f0351c/typescript-eslint/worker"
        self.assertTrue(index_event_root_matches(logged, root))
        self.assertTrue(index_event_root_matches(".../f2f0351c/worker", root))
        self.assertFalse(index_event_root_matches(logged, root.replace("a", "b")))
        self.assertFalse(index_event_root_matches(".../f2f0351c/other", root))
        self.assertTrue(index_event_root_matches("C:/work_tree/repo", "C:\\work tree\\repo"))


if __name__ == "__main__":
    unittest.main()
