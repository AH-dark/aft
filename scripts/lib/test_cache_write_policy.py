#!/usr/bin/env python3
"""Keep branch-private caches from evicting the shared main baseline."""

import importlib.util
from pathlib import Path
import re
import unittest


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("workflow_gates", Path(__file__).with_name("workflow-gates.py"))
assert SPEC is not None and SPEC.loader is not None
parser = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(parser)

MAIN_ONLY = "${{ github.ref == 'refs/heads/main' }}"
MAIN_BACKEND = "${{ github.ref == 'refs/heads/main' && 'READ_WRITE' || 'READ_ONLY' }}"
NON_MAIN_REFS = (
    "refs/heads/train/cache-policy",
    "refs/pull/123/merge",
    "refs/heads/gh-readonly-queue/main/pr-123-0123456789abcdef",
    "refs/heads/ci-speed/release-measurement",
    "refs/tags/v0.59.0",
    "refs/heads/main-lookalike",
)


def steps():
    for path in sorted((ROOT / ".github/workflows").glob("*.yml")):
        workflow = parser.parse_workflow(str(path))
        for job_id, job in workflow.get("jobs", {}).items():
            for step in job.get("steps", []):
                yield path.name, job_id, workflow, job, step


class CacheWritePolicyTests(unittest.TestCase):
    def test_rust_cache_saves_only_on_main_without_restricting_restores(self):
        found = 0
        for file, job_id, _, _, step in steps():
            if not step.get("uses", "").startswith("Swatinem/rust-cache@"):
                continue
            found += 1
            with self.subTest(file=file, job=job_id):
                policy = step.get("with", {}).get("save-if", "")
                self.assertEqual(policy, MAIN_ONLY)
                match = re.fullmatch(r"\$\{\{ github\.ref == '([^']+)' \}\}", policy)
                assert match is not None
                target = match.group(1)
                self.assertEqual(target, "refs/heads/main")
                for ref in NON_MAIN_REFS:
                    self.assertNotEqual(ref, target)
                self.assertNotIn("github.ref", step.get("if", ""))
        self.assertGreater(found, 0)

    def test_sccache_backend_is_read_only_off_main_even_with_env_overrides(self):
        found = 0
        for file, job_id, workflow, job, step in steps():
            if not step.get("uses", "").startswith("mozilla-actions/sccache-action@"):
                continue
            found += 1
            with self.subTest(file=file, job=job_id):
                env = {**workflow.get("env", {}), **job.get("env", {}), **step.get("env", {})}
                self.assertEqual(env.get("SCCACHE_GHA_RW_MODE"), MAIN_BACKEND)
                for later in job["steps"]:
                    overrides = later.get("env", {})
                    if "SCCACHE_GHA_RW_MODE" in overrides:
                        self.assertEqual(overrides["SCCACHE_GHA_RW_MODE"], MAIN_BACKEND)
                    self.assertNotIn("SCCACHE_GHA_RW_MODE=", later.get("run", ""))
                self.assertNotIn("github.ref", step.get("if", ""))
        self.assertGreater(found, 0)

    def test_action_cache_saves_only_on_main_and_keeps_restore_keys_identical(self):
        found = 0
        for file, job_id, _, job, step in steps():
            uses = step.get("uses", "")
            with self.subTest(file=file, job=job_id, action=uses):
                self.assertFalse(uses.startswith("actions/cache@"), "composite cache action saves unconditionally")
                if not uses.startswith("actions/cache/save@"):
                    continue
                found += 1
                match = re.fullmatch(
                    r"\$\{\{ github\.ref == 'refs/heads/main' && steps\.([\w-]+)\.outputs\.cache-hit != 'true' \}\}",
                    step.get("if", ""),
                )
                self.assertIsNotNone(match, "cache save must reject every non-main ref")
                assert match is not None
                restore = next(s for s in job["steps"] if s.get("id") == match.group(1))
                self.assertTrue(restore["uses"].startswith("actions/cache/restore@"))
                self.assertEqual(step["with"]["path"], restore["with"]["path"])
                self.assertEqual(step["with"]["key"], "${{ steps." + match.group(1) + ".outputs.cache-primary-key }}")
                self.assertNotIn("github.ref", restore.get("if", ""))
                self.assertGreater(job["steps"].index(step), job["steps"].index(restore))
        self.assertGreater(found, 0)


if __name__ == "__main__":
    unittest.main()
