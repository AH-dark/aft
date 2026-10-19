#!/usr/bin/env python3
"""Adversarial contract tests; run with python3 scripts/test_views_performance_gate.py."""
import importlib.util
from pathlib import Path
import io
import json
import tempfile
import time
from types import SimpleNamespace
from unittest.mock import patch
import unittest

SPEC = importlib.util.spec_from_file_location(
    "views_performance_gate", Path(__file__).with_name("views-performance-gate.py")
)
assert SPEC is not None and SPEC.loader is not None
gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(gate)


class MeasurementContractTests(unittest.TestCase):
    def test_bad_pid_is_rejected(self):
        with self.assertRaisesRegex(gate.MeasurementError, "PID"):
            gate.validate_subject(123, 456, "registry", "registry")

    def test_different_registry_is_rejected(self):
        with self.assertRaisesRegex(gate.MeasurementError, "registry"):
            gate.validate_subject(123, 123, "one", "two")

    def test_missing_counters_are_rejected(self):
        counters = dict.fromkeys(gate.COUNTERS, 0)
        del counters["source_hashes"]
        with self.assertRaisesRegex(gate.MeasurementError, "source_hashes"):
            gate.require_counters(counters)

    def test_counter_regression_is_rejected(self):
        before = dict.fromkeys(gate.COUNTERS, 2)
        after = dict.fromkeys(gate.COUNTERS, 1)
        with self.assertRaisesRegex(gate.MeasurementError, "regressed"):
            gate.counter_delta(before, after)

    def test_zero_cases_are_rejected(self):
        with self.assertRaisesRegex(gate.MeasurementError, "zero"):
            gate.validate_witnesses([], [])

    def test_skipped_intermediate_state_is_rejected(self):
        expected = gate.round_states(1, 0)
        observed = [{"id": state, "correct": True} for state in expected[1:]]
        with self.assertRaisesRegex(gate.MeasurementError, "root0:atomic_edit"):
            gate.validate_witnesses(expected, observed)

    def test_failed_witness_is_rejected(self):
        with self.assertRaisesRegex(gate.MeasurementError, "incorrect"):
            gate.validate_witnesses(["edit"], [{"id": "edit", "correct": False}])

    def test_all_states_must_be_observed_exactly_once(self):
        rows = [{"id": "edit", "correct": True}, {"id": "revert", "correct": True}]
        gate.validate_witnesses(["edit", "revert"], rows)
        with self.assertRaisesRegex(gate.MeasurementError, "duplicate"):
            gate.validate_witnesses(["edit", "revert"], rows + rows)

    def test_unknown_physical_bytes_stay_unknown(self):
        self.assertIsNone(gate.optional_delta(None, 100))
        self.assertIsNone(gate.optional_delta(100, None))
        self.assertEqual(gate.optional_delta(100, 125), 25)

    def test_percentiles_use_nearest_rank(self):
        self.assertEqual(gate.percentile([1, 2, 3, 4, 5], 95), 5)
        self.assertIsNone(gate.percentile([], 95))

    def test_plane_clocks_use_response_completion(self):
        token = "VIEWS_PERF_STATE_ALPHA"
        answers = {
            "grep": {"success": True, "complete": True, "matches": [{"line_text": token}],
                     "index": {"trigram": {"status": "ready"}}},
            "glob": {"total": 1, "complete": True},
            "search": {"results": [{"snippet": token}], "semantic_status": "disabled"},
            "callgraph": {"success": True, "name": "probe_entry",
                          "children": [{"name": "probe_alpha", "resolved": True}]},
            "zoom": {"success": True, "content": gate.BASE,
                     "annotations": {"calls_out": [{"name": "probe_alpha"}]}},
            "inspect": {"success": True, "complete": True, "details": {"todos": [token]}},
        }
        completed = {"grep": 0.1, "glob": 0.2, "search": 0.3,
                     "callgraph": 3, "zoom": 3.1, "inspect": 20}

        class TimedAcquisition(gate.Acquisition):
            def tool(self, _channel, name, _arguments, _state):
                self.queries.append({"start_s": 0, "wall_s": completed[name]})
                return answers[name]

            def sample(self, _label):
                return None

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / gate.PROBE).write_text(gate.BASE)
            acquisition = TimedAcquisition(None, root, 30, False, 60)
            acquisition.roots[1] = root
            with patch.object(gate.time, "monotonic", return_value=20):
                row = acquisition.witness(1, "bind", False, 0, require_indexed=True)
            self.assertEqual(row["planes_s"]["lexical_indexed"], 0.3)
            self.assertEqual(row["planes_s"]["callgraph"], 3)
            self.assertEqual(row["planes_s"]["inspect_todos"], 20)

    def test_timed_out_query_is_retained(self):
        class FailedBroker:
            request_timeout = 1

            def call(self, *args, **kwargs):
                raise gate.MeasurementError("public transport timeout")

        with tempfile.TemporaryDirectory() as directory:
            acquisition = gate.Acquisition(FailedBroker(), Path(directory), 1, False)
            with self.assertRaisesRegex(gate.MeasurementError, "timeout"):
                acquisition.tool(1, "inspect", {}, "edit")
            row = json.loads((Path(directory) / "queries.jsonl").read_text())
            self.assertTrue(row["error"])
            self.assertEqual(row["state"], "edit")
            self.assertEqual(len(acquisition.queries), 1)

    def test_failed_state_has_censored_freshness(self):
        with tempfile.TemporaryDirectory() as directory:
            acquisition = gate.Acquisition(None, Path(directory), 1, False)
            acquisition.active_state = {"id": "edit", "channel": 1, "since": time.monotonic() - 1,
                                        "planes_s": {"lexical_answer": 0.1}}
            acquisition.record_failed_state("state deadline")
            row = acquisition.witnesses[0]
            self.assertFalse(row["correct"])
            self.assertIsNone(row["time_to_correct_s"])
            self.assertGreaterEqual(row["elapsed_s"], 1)
            self.assertEqual(row["planes_s"], {"lexical_answer": 0.1})
            acquisition.record_failed_state("duplicate terminal")
            self.assertEqual(len(acquisition.witnesses), 1)

    def test_failed_transport_cannot_consume_late_response(self):
        broker = object.__new__(gate.Broker)
        broker.failed = True
        broker.request_timeout = 1
        broker.proc = SimpleNamespace(stdin=io.BytesIO())
        broker.read = lambda _timeout: {"late": "reply"}
        with self.assertRaisesRegex(gate.MeasurementError, "late response"):
            broker.call("health")

    def test_nonempty_answer_is_not_a_graph_witness(self):
        self.assertFalse(gate.graph_witness({"status": "building"}, "probe_beta"))
        self.assertFalse(gate.graph_witness({"name": "probe_entry", "children": [], "complete": True}, "probe_beta"))
        self.assertTrue(gate.graph_witness(
            {"name": "probe_entry", "children": [{"name": "probe_beta", "resolved": True}], "complete": True}, "probe_beta"
        ))


if __name__ == "__main__":
    unittest.main(verbosity=2)
