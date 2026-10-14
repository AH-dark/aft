#!/usr/bin/env python3
"""Preserve native release profile, bounded phases and tested artifact publication."""

import importlib.util
from pathlib import Path
import unittest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("workflow_gates", Path(__file__).with_name("workflow-gates.py"))
assert SPEC is not None and SPEC.loader is not None
parser = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(parser)


class NativeReleaseWorkflowTests(unittest.TestCase):
    def workflow(self, name):
        return parser.parse_workflow(str(ROOT / ".github/workflows" / name))

    def test_each_native_release_library_has_its_own_unchanged_budget(self):
        jobs = self.workflow("_e2e-suite.yml")["jobs"]
        for suffix, limit, target in (("macos", 45, ""), ("windows", 30, "x86_64-pc-windows-msvc")):
            with self.subTest(platform=suffix):
                lib = jobs[f"release-library-{suffix}"]
                self.assertEqual(int(lib["timeout-minutes"]), limit)
                self.assertEqual(lib["if"], "inputs.cargo_profile == 'release'")
                commands = "\n".join(step.get("run", "") for step in lib["steps"])
                self.assertIn("cargo build --release --lib", commands)
                self.assertIn("pack-library", commands)
                if target:
                    self.assertIn(target, commands)
                consumer = jobs[f"e2e-{suffix}"]
                self.assertEqual(consumer["name"], "E2E (macOS native)" if suffix == "macos" else "E2E (Windows native)")
                self.assertEqual(consumer["needs"], f"release-library-{suffix}")
                self.assertIn("inputs.cargo_profile != 'release'", consumer["if"])
                self.assertIn("result == 'success'", consumer["if"])
                build = next(step for step in consumer["steps"] if step.get("name", "").startswith("Build AFT binary"))
                self.assertIn("--bin aft", build["run"])
                self.assertIn("--timings", build["run"])
                restore = next(step for step in consumer["steps"] if step.get("name") == "Verify and restore release library inputs")
                self.assertIn("unpack-library", restore["run"])

    def test_tested_shipped_bytes_are_uploaded_without_changing_release_profile(self):
        jobs = self.workflow("_e2e-suite.yml")["jobs"]
        for suffix in ("macos", "windows"):
            steps = jobs[f"e2e-{suffix}"]["steps"]
            test_index = next(i for i, step in enumerate(steps) if step.get("name") == ("Run macOS E2E suite" if suffix == "macos" else "Run Windows E2E suite"))
            bundle_index = next(i for i, step in enumerate(steps) if "pack-binary" in step.get("run", ""))
            self.assertGreater(bundle_index, test_index)
            artifact = next(step for step in steps if step.get("uses") == "actions/upload-artifact@v4" and step.get("with", {}).get("name", "").startswith(f"{suffix}-shipped-release"))
            self.assertIn("${{ github.sha }}", artifact["with"]["name"])
        profile = (ROOT / "Cargo.toml").read_text()
        self.assertIn("lto = true", profile)
        self.assertIn("codegen-units = 1", profile)

    def test_publishing_reuses_exact_tested_native_artifacts_or_split_fallback(self):
        jobs = self.workflow("release.yml")["jobs"]
        for job, platform in (("build-darwin-arm64", "macos"), ("build-win32-x64", "windows")):
            steps = jobs[job]["steps"]
            commands = "\n".join(step.get("run", "") for step in steps)
            self.assertIn(f"select-native-release-artifact.py {platform}", commands)
            self.assertIn("unpack-binary", commands)
            download = next(step for step in steps if step.get("uses") == "actions/download-artifact@v4")
            self.assertIn("steps.tested.outputs.run_id", download["with"]["run-id"])
            self.assertIn("github-token", download["with"])
            library = next(step for step in steps if "cargo build --release --lib" in step.get("run", ""))
            binary = next(step for step in steps if "cargo build --release --bin aft" in step.get("run", ""))
            self.assertEqual(library["if"], "steps.tested.outputs.found != 'true'")
            self.assertEqual(binary["if"], "steps.tested.outputs.found != 'true'")

    def test_measurement_refs_use_release_without_changing_train_profile(self):
        e2e = self.workflow("tests.yml")["jobs"]["e2e"]
        choice = e2e["with"]["cargo_profile"]
        self.assertIn("refs/heads/main", choice)
        self.assertIn("refs/heads/ci-speed/release-", choice)
        self.assertIn("'release' || 'stage'", choice)


if __name__ == "__main__":
    unittest.main()
