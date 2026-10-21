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
    def test_semantic_witness_accepts_captured_targeted_ready_reply(self):
        capture = json.loads((Path(__file__).parent / "views-performance-baselines/slice-4a-witness-ready.json").read_text())
        self.assertEqual(gate.SEMANTIC_QUERY, capture["request"]["query"])
        self.assertTrue(gate.semantic_witness(capture["reply"], Path(capture["probe_path"]), capture["marker"]))

    def test_semantic_witness_rejects_captured_building_reply(self):
        capture = json.loads((Path(__file__).parent / "views-performance-baselines/slice-4a-witness-not-ready.json").read_text())
        self.assertFalse(gate.semantic_witness(capture["reply"], Path(capture["probe_path"]), capture["marker"]))

    def test_semantic_witness_requires_public_semantic_lane(self):
        capture = json.loads((Path(__file__).parent / "views-performance-baselines/slice-4a-witness-ready.json").read_text())
        answer = capture["reply"]
        for lane in [None, {}, {"status": "building"}]:
            with self.subTest(lane=lane):
                answer["lanes"] = {} if lane is None else {"semantic": lane}
                self.assertFalse(gate.semantic_witness(answer, Path(capture["probe_path"]), capture["marker"]))

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
            "glob": {"success": True, "total": 1, "complete": True,
                     "index": {"trigram": {"status": "ready"}}},
            "search": {"success": True, "results": [{"snippet": token}], "semantic_status": "disabled",
                       "lanes": {"trigram": {"status": "ready"}}},
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
                if name == "inspect" and getattr(self, "fail_inspect", False):
                    raise gate.MeasurementError("inspect timed out")
                self.queries.append({"start_s": 0, "wall_s": completed[name]})
                return answers[name]

            def sample(self, _label):
                return None

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / gate.PROBE).write_text(gate.BASE)
            answers["grep"]["matches"][0]["file"] = str(root / gate.PROBE)
            answers["glob"]["files"] = [str(root / gate.PROBE)]
            answers["search"]["results"][0]["file"] = str(root / gate.PROBE)
            acquisition = TimedAcquisition(None, root, 30, False, 60)
            acquisition.roots[1] = root
            with patch.object(gate.time, "monotonic", return_value=20):
                row = acquisition.witness(1, "bind", False, 0, require_indexed=True)
            self.assertEqual(row["planes_s"]["lexical_indexed"], 0.3)
            self.assertEqual(row["planes_s"]["callgraph"], 3)
            self.assertEqual(row["planes_s"]["inspect_todos"], 20)
            acquisition.fail_inspect = True
            with patch.object(gate.time, "monotonic", return_value=20):
                with self.assertRaisesRegex(gate.MeasurementError, "inspect timed out"):
                    acquisition.witness(1, "edit", False, 0)
                acquisition.record_failed_state("inspect timed out")
            partial = acquisition.witnesses[-1]
            self.assertFalse(partial["correct"])
            self.assertEqual(partial["planes_s"]["lexical_indexed"], 0.3)
            self.assertEqual(partial["planes_s"]["callgraph"], 3)
            self.assertNotIn("inspect_todos", partial["planes_s"])

    def test_lexical_witness_rejects_sibling_rows(self):
        path = Path("/intended") / gate.PROBE
        sibling = str(Path("/sibling") / gate.PROBE)
        token = "VIEWS_PERF_STATE_ALPHA"
        grep = {"success": True, "complete": True, "matches": [{"file": str(path), "line_text": token}]}
        glob = {"success": True, "complete": True, "total": 1, "files": [str(path)]}
        search = {"success": True, "complete": False, "results": [{"file": str(path), "snippet": token}]}
        self.assertTrue(gate.lexical_witness(grep, glob, search, path, token))
        for answer, row in [(grep, grep["matches"][0]), (search, search["results"][0])]:
            with self.subTest(answer=answer):
                row["file"] = sibling
                self.assertFalse(gate.lexical_witness(grep, glob, search, path, token))
                row["file"] = str(path)
        glob["files"] = [sibling]
        self.assertFalse(gate.lexical_witness(grep, glob, search, path, token))
        glob["files"] = [str(path)]
        search["success"] = False
        self.assertFalse(gate.lexical_witness(grep, glob, search, path, token))

    def test_semantic_witness_requires_current_state(self):
        path = Path("/intended") / gate.PROBE
        token = "VIEWS_PERF_STATE_BETA"
        row = {"file": str(path), "snippet": token}
        answer = {"success": True, "complete": True, "semantic_status": "ready", "results": [row],
                  "lanes": {"semantic": {"status": "ready"}}}
        self.assertTrue(gate.semantic_witness(answer, path, token))
        row["snippet"] = "VIEWS_PERF_STATE_ALPHA"
        self.assertFalse(gate.semantic_witness(answer, path, token))
        row["snippet"] = token
        row["file"] = str(Path("/sibling") / gate.PROBE)
        self.assertFalse(gate.semantic_witness(answer, path, token))

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

    def test_provisioning_failure_still_writes_a_failed_result(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            args = SimpleNamespace(
                semantic=False, model="m", subjects=tmp / "subjects", source=tmp / "src",
                corpus="corpus", state_timeout=1, ready_timeout=1, request_timeout=1,
                graph_inspect=False, warmup_minutes=0, activity_minutes=0, quiet_minutes=0,
            )
            args.subjects.mkdir()
            out = tmp / "out" / "arm"
            with patch.object(gate, "provision", side_effect=gate.MeasurementError("clone failed")):
                result = gate.run_arm(args, {"commit": "abc"}, 1, True, out)
            self.assertEqual(result["status"], "failed")
            self.assertIn("clone failed", result["failure"])
            written = json.loads((out / "result.json").read_text())
            self.assertEqual(written["status"], "failed")
            self.assertIsNone(written["inventory"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
