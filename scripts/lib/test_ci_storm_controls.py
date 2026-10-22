#!/usr/bin/env python3
"""Guard the optimized storm profile and its failing deadline controls."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("storm_controls", ROOT / "scripts/ci-storm-controls.py")
assert SPEC is not None and SPEC.loader is not None
controls = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(controls)
GATE_SPEC = importlib.util.spec_from_file_location("workflow_gates", ROOT / "scripts/lib/workflow-gates.py")
assert GATE_SPEC is not None and GATE_SPEC.loader is not None
gates = importlib.util.module_from_spec(GATE_SPEC)
GATE_SPEC.loader.exec_module(gates)


def output(expected, sibling, diagnostic):
    return (
        f"FAIL [ 2.002s] (1/2) agent-file-tools::integration {expected}\n"
        f"PASS [ 0.200s] (2/2) agent-file-tools::integration {sibling}\n"
        f"thread panicked: {diagnostic}\n"
        "Summary [ 2.202s] 2 tests run: 1 passed, 1 failed, 2492 skipped\n"
    )


class StormControlTests(unittest.TestCase):
    def test_inventory_rejects_missing_or_debug_ignored_traffic_storm(self):
        names = [controls.MAIN_TEST, controls.ACK_TEST, controls.READ_TEST] + [f"case{i}" for i in range(16)]
        cases = {name: {"ignored": False, "filter-match": {"status": "matches"}} for name in names}
        document = {"rust-suites": {"integration": {"testcases": cases}}}
        with patch("builtins.print"):
            controls.check_inventory(document)
        cases[controls.MAIN_TEST]["ignored"] = True
        with self.assertRaises(RuntimeError):
            controls.check_inventory(document)

    def test_accepts_only_the_named_deadline_failure_and_green_sibling(self):
        for _, expected, sibling, diagnostic in controls.CONTROLS:
            controls.validate_failure(output(expected, sibling, diagnostic), 100, expected, sibling, diagnostic)

    def test_rejects_a_silent_pass_build_error_missing_test_or_timeout(self):
        _, expected, sibling, diagnostic = controls.CONTROLS[0]
        good = output(expected, sibling, diagnostic)
        for text, code in ((good, 0), (good, 101), (good.replace("2 tests run", "0 tests run"), 100),
                           (good.replace(expected, "different_test"), 100),
                           (good.replace(diagnostic, "unrelated panic"), 100),
                           (good.replace("PASS [", "TIMEOUT ["), 100)):
            with self.subTest(code=code, text=text):
                with self.assertRaises(RuntimeError):
                    controls.validate_failure(text, code, expected, sibling, diagnostic)

    def test_controls_use_distinct_fixture_paths_and_no_cargo_features(self):
        calls = []
        def run(command, **kwargs):
            calls.append((command, kwargs))
            control = kwargs["env"]["AFT_STORM_POSITIVE_CONTROL"]
            _, expected, sibling, diagnostic = next(row for row in controls.CONTROLS if row[0] == control)
            class Result:
                returncode = 100
                stdout = output(expected, sibling, diagnostic)
            return Result()
        with tempfile.TemporaryDirectory() as directory, patch.object(controls.subprocess, "run", side_effect=run), patch("builtins.print"):
            controls.run_controls("ci-storm", Path(directory))
            self.assertTrue((Path(directory) / "controls.json").is_file())
        self.assertEqual(len(calls), 2)
        self.assertEqual({kwargs["env"]["AFT_STORM_POSITIVE_CONTROL"] for _, kwargs in calls}, {"bind-ack", "interactive-read"})
        for command, _ in calls:
            self.assertIn("--no-fail-fast", command)
            self.assertEqual(command[command.index("--profile") + 1], "default")
            self.assertEqual(command[command.index("--cargo-profile") + 1], "ci-storm")
            self.assertNotIn("--features", command)

    def test_storm_profile_does_not_relax_bounds_or_change_production(self):
        manifest = (ROOT / "Cargo.toml").read_text()
        profile = manifest.split("[profile.ci-storm]\n", 1)[1].split("\n[", 1)[0]
        for setting in ('inherits = "release"', 'opt-level = 3', 'lto = "thin"', 'codegen-units = 16', 'debug-assertions = false'):
            self.assertIn(setting, profile)
        production = manifest.split("[profile.release]\n", 1)[1].split("\n[", 1)[0]
        self.assertIn("lto = true", production)
        self.assertIn("codegen-units = 1", production)
        source = (ROOT / "crates/aft/tests/integration/subc_storm_test.rs").read_text()
        self.assertIn('storm_positive_control_is("bind-ack")', source)
        self.assertIn('storm_positive_control_is("interactive-read")', source)
        self.assertIn("thread::sleep(Duration::from_secs(3));", source)
        self.assertIn("tokio::time::sleep(Duration::from_secs(3)).await;", source)
        self.assertIn("const BIND_ACK_BOUND: Duration = Duration::from_secs(2 * DEBUG_BOUND_MULTIPLIER as u64);", source)
        self.assertIn("const WATCHER_SLICE_READ_BOUND: Duration = Duration::from_secs(2 * DEBUG_BOUND_MULTIPLIER as u64);", source)


class StormWorkflowTests(unittest.TestCase):
    def test_required_storm_job_uses_release_derived_validation_on_free_runner(self):
        suite = gates.parse_workflow(str(ROOT / ".github/workflows/_unit-suite.yml"))
        job = suite["jobs"]["unit-linux-storm"]
        self.assertEqual(job["name"], "Release storm (Linux)")
        self.assertEqual(job["runs-on"], "ubuntu-22.04")
        self.assertEqual(job["timeout-minutes"], "40")
        self.assertTrue(any(step.get("uses") == "./.github/actions/storm-suite" for step in job["steps"]))
        self.assertEqual(job["env"]["CARGO_PROFILE_CI_STORM_LTO"], "thin")

    def test_both_lto_options_run_the_same_inventory_and_positive_controls(self):
        suite = gates.parse_workflow(str(ROOT / ".github/workflows/_unit-suite.yml"))
        probe = suite["jobs"]["unit-linux-storm-profile-probe"]
        self.assertEqual(probe["if"], "${{ inputs.storm_profile_probe && github.event_name != 'pull_request' }}")
        self.assertEqual(probe["env"]["CARGO_PROFILE_CI_STORM_LTO"], "false")
        self.assertEqual(probe["runs-on"], "ubuntu-22.04")
        self.assertTrue(any(step.get("uses") == "./.github/actions/storm-suite" for step in probe["steps"]))
        action = gates.parse_workflow(str(ROOT / ".github/actions/storm-suite/action.yml"))
        commands = "\n".join(step.get("run", "") for step in action["runs"]["steps"])
        self.assertIn("--profile ci-storm --locked -p agent-file-tools --test integration --no-run --timings", commands)
        self.assertIn("--profile default --cargo-profile ci-storm", commands)
        self.assertIn("test(subc_storm)", commands)
        self.assertIn("--check-inventory", commands)
        self.assertIn("python3 scripts/ci-storm-controls.py --cargo-profile ci-storm", commands)
        self.assertNotIn("--features", commands)


if __name__ == "__main__":
    unittest.main()
