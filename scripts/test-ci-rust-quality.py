#!/usr/bin/env python3
"""Offline guards for Rust CI quality steps and reviewed action dependencies."""

import importlib.util
from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[1]
WORKFLOWS = ROOT / ".github" / "workflows"
SPEC = importlib.util.spec_from_file_location(
    "workflow_gates", ROOT / "scripts" / "lib" / "workflow-gates.py"
)
assert SPEC is not None and SPEC.loader is not None
workflow_gates = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(workflow_gates)

REVIEWED_ACTIONS = {
    "SethCohen/github-releases-to-discord": "24d166886aee4646d448c8a389ff9e1ebcab3682",
    "Swatinem/rust-cache": "6323deb102c322ba6fcbdcafc7e3dddab59af2b6",
    "taiki-e/install-action": "ab6895351a88b908e96a4697860d4ffea5e57dbe",
    "mozilla-actions/sccache-action": "fc920bf0ec8de6ee65d409111f7ec508035751ba",
}


class RustWorkflowQualityTests(unittest.TestCase):
    def job(self, filename, job_id):
        return workflow_gates.parse_workflow(str(WORKFLOWS / filename))["jobs"][job_id]

    def required_command(self, job, command):
        steps = [
            step for step in job["steps"]
            if command in step.get("run", "").splitlines()
        ]
        self.assertEqual(len(steps), 1, command)
        self.assertNotIn("if", steps[0], command)
        self.assertNotIn("continue-on-error", steps[0], command)

    def test_linux_unit_job_runs_rust_quality_checks(self):
        job = self.job("_unit-suite.yml", "unit-linux")
        self.assertEqual(job["if"], "${{ github.event_name != 'pull_request' }}")
        for command in (
            "cargo fmt --all -- --check",
            "cargo clippy --workspace --lib --bins --locked",
            "python3 scripts/test-ci-rust-quality.py",
        ):
            self.required_command(job, command)
        toolchain = next(
            step for step in job["steps"]
            if step.get("uses") == "dtolnay/rust-toolchain@stable"
        )
        components = toolchain.get("with", {}).get("components", "")
        self.assertEqual({part.strip() for part in components.split(",")}, {"clippy", "rustfmt"})

    def test_pr_checks_keep_lightweight_rust_checks(self):
        job = self.job("tests.yml", "pr-checks")
        self.assertEqual(job["if"], "${{ github.event_name == 'pull_request' }}")
        self.required_command(job, "cargo fmt --all -- --check")
        self.required_command(job, "python3 scripts/test-ci-rust-quality.py")
        commands = "\n".join(step.get("run", "") for step in job["steps"])
        self.assertNotIn("cargo clippy", commands)

    def test_named_third_party_actions_use_reviewed_immutable_commits(self):
        seen = set()
        for path in sorted(WORKFLOWS.glob("*.yml")):
            for action, ref in re.findall(
                r"^\s*(?:- )?uses: ([\w./-]+)@([^\s#]+)", path.read_text(), re.MULTILINE
            ):
                if action in REVIEWED_ACTIONS:
                    seen.add(action)
                    self.assertEqual(
                        ref, REVIEWED_ACTIONS[action], f"{path.name}: {action}@{ref}"
                    )
        self.assertEqual(seen, set(REVIEWED_ACTIONS))

    def test_privileged_gate_uses_reviewed_workflow_commit(self):
        job = self.job("design-gate.yml", "design-gate")
        # Keep the newer token-only gate; the older pinned workflow required App secrets.
        self.assertEqual(
            job["uses"],
            "cortexkit/subconscious/.github/workflows/design-gate.yml@242a03bfe752409717aed4a25237ac77b9042f90",
        )
        self.assertNotIn("secrets", job)


if __name__ == "__main__":
    unittest.main(verbosity=2)
