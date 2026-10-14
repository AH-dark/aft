#!/usr/bin/env python3
"""Identity guards inspect real commits, with branch-specific attribution."""

from __future__ import annotations

import json
from importlib import import_module
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
COAUTHOR = "Co-authored-by: alfonso-aft <289616620+alfonso-aft@users.noreply.github.com>"
OWNER = "94532+ualtinok@users.noreply.github.com"


class LandingIdentityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="aft-identity-")
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("GIT_CONFIG_") and not key.startswith("GIT_AUTHOR_")
                    and not key.startswith("GIT_COMMITTER_")}
        self.git("init", "-qb", "main")
        self.git("config", "user.name", "ualtinok")
        self.git("config", "user.email", OWNER)
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "core.hooksPath", "/dev/null")
        self.git("commit", "--allow-empty", "-qm", "base")
        self.base = self.git("rev-parse", "HEAD")

    def git(self, *args: str, env: dict | None = None) -> str:
        return subprocess.check_output(["git", *args], cwd=self.repo,
                                       env=env or self.env, text=True).strip()

    def commit(self, message: str = "manual", *, author: str = OWNER,
               committer: str = OWNER, name: str = "ualtinok") -> None:
        env = {**self.env, "GIT_AUTHOR_EMAIL": author, "GIT_COMMITTER_EMAIL": committer,
               "GIT_AUTHOR_NAME": name, "GIT_COMMITTER_NAME": "ualtinok"}
        self.git("commit", "--allow-empty", "-qm", message, env=env)

    def check(self, branch: str = "feature/manual") -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, str(ROOT / "scripts/landing-policy.py"),
                               "--base", self.base, "--head", self.git("rev-parse", "HEAD"),
                               "--branch", branch], cwd=self.repo, env=self.env,
                              text=True, capture_output=True, check=False)

    def test_owner_land_commit_without_trailer_is_refused(self) -> None:
        self.commit()
        result = self.check("land/owner")
        self.assertEqual(result.returncode, 1)
        self.assertIn("missing alfonso-aft", result.stderr)

    def test_same_owner_commit_off_land_is_exempt(self) -> None:
        self.commit()
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("1 original commits checked", result.stdout)

    def test_bot_author_without_trailer_is_refused_on_any_branch(self) -> None:
        self.commit(name="alfonso-aft", author="289616620+alfonso-aft@users.noreply.github.com")
        result = self.check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("missing alfonso-aft", result.stderr)

    def test_land_human_author_is_not_an_attribution_bypass(self) -> None:
        self.commit(name="Contributor", author="contributor@users.noreply.github.com")
        self.assertEqual(self.check("land/contributor").returncode, 1)

    def test_canonical_trailer_accepts_agent_delivery(self) -> None:
        self.commit("delivery\n\n" + COAUTHOR)
        self.assertEqual(self.check("land/accepted").returncode, 0)

    def test_private_author_email_is_refused_and_redacted(self) -> None:
        self.commit(author="owner@private.invalid")
        result = self.check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("author email must be GitHub noreply", result.stderr)
        self.assertNotIn("owner@private.invalid", result.stderr)

    def test_private_committer_email_is_refused(self) -> None:
        self.commit(committer="owner@private.invalid")
        result = self.check()
        self.assertEqual(result.returncode, 1)
        self.assertIn("committer email must be GitHub noreply", result.stderr)

    def test_trailer_mentioned_in_body_is_not_attribution(self) -> None:
        self.commit("delivery\n\n" + COAUTHOR + "\n\nNot a trailer footer.")
        self.assertEqual(self.check("land/body").returncode, 1)

    def group_check(self, agent: bool) -> subprocess.CompletedProcess:
        fixture = import_module("test_merge_queue_workflow").MergeQueueDescriptorTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        fixture.git("config", "user.name", "ualtinok")
        fixture.git("config", "user.email", OWNER)
        fixture.member(1)
        original = next(iter(fixture.prs))
        if agent:
            fixture.prs[original][0]["head"]["ref"] = "land/member-1"
        # Deliberately private synthetic metadata must not be mistaken for an
        # original PR commit's identity. Original member heads remain public.
        fixture.git("config", "user.email", "synthetic@private.invalid")
        fixture.git("-c", "core.hooksPath=/dev/null", "commit", "--amend", "--no-edit", "--author", "GitHub <synthetic@private.invalid>", "-q")
        (fixture.repo / ".fixture-prs.json").write_text(json.dumps(fixture.prs))
        event = fixture.repo / "policy-event.json"
        event.write_text(json.dumps({"repository": {"full_name": "cortexkit/aft"},
                         "merge_group": {"base_sha": fixture.base, "head_sha": fixture.git("rev-parse", "HEAD"),
                         "head_ref": "refs/heads/gh-readonly-queue/main/pr-1-fixture"}}))
        return subprocess.run([sys.executable, str(ROOT / "scripts/landing-policy.py"),
                               "--event", "merge_group", "--event-path", str(event)],
                              cwd=fixture.repo, env=fixture.env, capture_output=True, text=True)

    def test_group_synthetic_author_is_excluded_from_original_identity_policy(self) -> None:
        result = self.group_check(agent=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("2 original commits checked", result.stdout)

    def test_group_member_land_branch_still_requires_original_trailers(self) -> None:
        result = self.group_check(agent=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("missing alfonso-aft", result.stderr)
        self.assertNotIn("email must", result.stderr)

    def test_pr_event_checks_original_head_not_synthetic_merge(self) -> None:
        self.commit("delivery\n\n" + COAUTHOR)
        event = self.repo / "event.json"
        event.write_text(json.dumps({"pull_request": {"base": {"sha": self.base},
                         "head": {"sha": self.git("rev-parse", "HEAD"), "ref": "land/event"}}}))
        result = subprocess.run([sys.executable, str(ROOT / "scripts/landing-policy.py"),
                                "--event", "pull_request", "--event-path", str(event)],
                               cwd=self.repo, env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
