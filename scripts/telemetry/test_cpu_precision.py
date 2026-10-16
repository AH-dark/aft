#!/usr/bin/env python3
"""Focused checks for fractional CPU collection and reporting."""

from __future__ import annotations

import importlib.util
import json
import sys
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

SCRIPT_DIR = Path(__file__).resolve().parent


def load_script(name: str, filename: str):
    spec = importlib.util.spec_from_file_location(name, SCRIPT_DIR / filename)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {filename}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


matrix = load_script("oss_matrix_cpu_test", "oss-matrix.py")
gate = load_script("cost_gate_cpu_test", "cost-gate.py")


class CpuPrecisionTests(unittest.TestCase):
    def test_fractional_cpu_boundary_and_baseline_keep_fraction(self) -> None:
        baseline = {"metrics": {"cpu_seconds": {"value": 3.0, "tolerance_pct": 25.0, "absolute_floor": 1.0}}}

        def run(value: float):
            row = {"cpu_s": f"{value:.2f}", "cpu_precision": "0.01-second"}
            return gate.RunData(Path("fixture.csv"), "synthetic-24k", gate.row_metrics(row), {}, True, row)

        below = run(3.74)
        above = run(3.76)
        self.assertEqual(below.metrics["cpu_seconds"], 3.74)
        self.assertEqual(gate.compare_repo("synthetic-24k", baseline, [below]), [])
        failures = gate.compare_repo("synthetic-24k", baseline, [above])
        self.assertEqual([(failure.metric, failure.observed, failure.limit) for failure in failures], [
            ("cpu_seconds", 3.76, 3.75),
        ])
        writer_metrics = {metric: 0.0 for metric in gate.DEFAULT_METRICS}
        writer_metrics["cpu_seconds"] = below.metrics["cpu_seconds"]
        writer_run = gate.RunData(Path("writer.csv"), "synthetic-24k", writer_metrics, {}, True, below.row)
        written = gate.repo_metrics_from_runs("synthetic-24k", baseline, [writer_run])
        persisted = json.loads(json.dumps(written))
        self.assertEqual(persisted["cpu_seconds"]["value"], 3.74)
        line = gate.observation_line("synthetic-24k", [below, above])
        self.assertIn("cpu_seconds[precision=0.01-second]=3.74/3.76", line)

        process = SimpleNamespace(pid=4242, poll=lambda: None)
        legacy_sampler = matrix.ProcessSampler(process)
        with patch.object(matrix.sys, "platform", "darwin"), \
                patch.object(matrix.subprocess, "run", return_value=SimpleNamespace(stdout="100 00:00:04")):
            legacy_sampler._sample_once()
            legacy_precision = legacy_sampler.cpu_precision
        self.assertEqual(legacy_sampler.cpu_seconds, 4.0)
        self.assertEqual(legacy_precision, "whole-second")
        self.assertIn("cpu_precision", matrix.CSV_FIELDS)
        legacy_row = {"cpu_s": f"{legacy_sampler.cpu_seconds:.2f}", "cpu_precision": legacy_precision}
        legacy_run = gate.RunData(Path("legacy.csv"), "synthetic-24k", gate.row_metrics(legacy_row), {}, True, legacy_row)
        self.assertIn("cpu_seconds[precision=whole-second]=4", gate.observation_line("synthetic-24k", [legacy_run]))

    def test_proc_stat_parser_handles_spaces_and_parentheses_in_comm(self) -> None:
        fields = ["S"] + ["0"] * 10 + ["374", "0"]
        stat = f"4242 (worker pool (compile) task) {' '.join(fields)}"
        self.assertEqual(matrix.parse_proc_stat_cpu_seconds(stat, clock_ticks=100), 3.74)

    def test_final_sample_occurs_before_client_close(self) -> None:
        order: list[str] = []

        class Sampler:
            def stop(self) -> None:
                order.append("final sample")

        class Client:
            def close(self) -> None:
                order.append("client close")
                return None

        matrix.close_after_final_sample(Sampler(), Client())
        self.assertEqual(order, ["final sample", "client close"])

    def test_exit_race_is_a_named_gap_not_zero_cpu(self) -> None:
        process = SimpleNamespace(pid=4242, poll=lambda: None)
        sampler = matrix.ProcessSampler(process)

        def read_proc(path: Path, encoding: str = "utf-8") -> str:
            if path.name == "stat":
                raise FileNotFoundError(path)
            return ""

        ps_result = SimpleNamespace(stdout="100 00:00:04")
        with patch.object(matrix.sys, "platform", "linux"), \
                patch.object(matrix.subprocess, "run", return_value=ps_result), \
                patch.object(Path, "read_text", read_proc):
            sampler._sample_once()

        self.assertIsNone(sampler.cpu_seconds)
        self.assertIn("CPU measurement gap: /proc/<pid>/stat unavailable during sampling", sampler.gaps)


if __name__ == "__main__":
    unittest.main(verbosity=2)
