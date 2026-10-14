#!/usr/bin/env python3
"""Real scratch Git refs and a stateful fake gh exercise PR/release lifecycle."""

from __future__ import annotations

from datetime import datetime, timezone
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
OWNER = "94532+ualtinok@users.noreply.github.com"
COAUTHOR = "Co-authored-by: alfonso-aft <289616620+alfonso-aft@users.noreply.github.com>"

FAKE_GH = r'''
import datetime, json, os, pathlib, subprocess, sys
a = sys.argv[1:]
p = pathlib.Path(os.environ['PR_TEST_STATE'])
s = json.loads(p.read_text())
with p.with_suffix('.calls').open('a') as f:
    f.write(json.dumps({'args': a, 'bypass': os.environ.get('GH_SHIM_BYPASS', '')}) + '\n')
def persist(): p.write_text(json.dumps(s))
def g(*args): return subprocess.check_output(['git', *args], text=True).strip()
def head():
    r = subprocess.run(['git', '--git-dir', s['origin'], 'rev-parse', 'refs/heads/' + s['branch']], text=True, capture_output=True)
    return r.stdout.strip() if r.returncode == 0 else g('rev-parse', 'HEAD')
def answer(v):
    data = json.dumps(v)
    if '--jq' in a:
        print(subprocess.check_output(['jq', '-r', a[a.index('--jq')+1]], input=data, text=True).strip())
    else: print(data)
    sys.exit(0)
def now(): return datetime.datetime.now(datetime.timezone.utc).isoformat()
def pr():
    return {'number': 7, 'state': 'closed' if s.get('merged') else 'open',
            'head': {'sha': head(), 'ref': s['branch'], 'repo': {'full_name': 'example/repo'}},
            'base': {'ref': 'main', 'repo': {'full_name': 'example/repo'}}}
if a == ['--co-author-line']:
    print('alfonso-aft <289616620+alfonso-aft@users.noreply.github.com>'); sys.exit(0)
if a[:2] == ['repo', 'view']: answer({'nameWithOwner': 'example/repo'})
if a[:2] == ['pr', 'view']: answer({'headRefOid': 'f'*40 if s.get('wrong_head') else head()})
if a[:2] == ['pr', 'create']:
    assert os.environ.get('GH_SHIM_BYPASS', '') == '', 'speech bypassed shim'
    assert '--base' in a and a[a.index('--base')+1] == 'main'
    s['created'] = s.get('created', 0) + 1; s['exists'] = True; persist()
    print('https://github.com/example/repo/pull/7'); sys.exit(0)
if a[:2] == ['pr', 'edit']: sys.exit(0)
if a[:2] == ['pr', 'merge']:
    assert os.environ.get('GH_SHIM_BYPASS') == 'operator'
    assert '--match-head-commit' in a and a[a.index('--match-head-commit')+1] == head()
    assert '--admin' not in a
    h = head(); base = g('--git-dir', s['origin'], 'rev-parse', 'main')
    tree = g('--git-dir', s['origin'], 'rev-parse', h+'^{tree}')
    oid = subprocess.check_output(['git', '--git-dir', s['origin'], 'commit-tree', tree,
                                  '-p', base, '-p', h, '-m', 'GitHub integration'], text=True,
                                  env={**os.environ, 'GIT_AUTHOR_NAME': 'GitHub', 'GIT_AUTHOR_EMAIL': 'noreply@github.com',
                                       'GIT_COMMITTER_NAME': 'GitHub', 'GIT_COMMITTER_EMAIL': 'noreply@github.com'}).strip()
    s['group'] = oid; s['polls'] = 0; s['admitted'] = True
    if not s.get('remove'):
        g('--git-dir', s['origin'], 'update-ref', 'refs/heads/main', oid)
        s['merged'] = True; s['merge_oid'] = oid
    persist(); sys.exit(0)
if a[:2] == ['run', 'list']:
    oid = s.get('merge_oid', '0'*40)
    answer([{'databaseId': 99, 'headSha': oid, 'headBranch': 'main', 'event': 'push',
             'status': 'completed', 'conclusion': 'success', 'attempt': 1,
             'url': 'https://github.com/example/repo/actions/runs/99'}])
if a[:2] == ['run', 'view']:
    answer({'jobs': [{'name': 'Landing policy', 'status': 'completed',
                     'conclusion': 'failure' if s.get('failed_required') else 'success'}]})
if not a or a[0] != 'api': raise SystemExit('unhandled fake gh: '+str(a))
if 'graphql' in a:
    q = a[a.index('-f')+1]
    if 'reviewThreads' in q:
        answer({'data': {'repository': {'pullRequest': {'reviewThreads': {'nodes':
                [{'isResolved': False}]*s.get('unresolved', 0)}}}}})
    if 'mergeQueueEntry' not in q:
        answer({'data': {'repository': {'pullRequest': {'baseRefName': 'main',
                'isMergeQueueEnabled': s.get('queue', True)}}}})
    removed = s.get('remove') and s.get('admitted') and s.get('polls', 0) > 0
    queued = s.get('remove') and s.get('admitted') and not removed
    value = {'state': 'MERGED' if s.get('merged') else 'OPEN', 'baseRefName': 'main',
             'headRefOid': 'f'*40 if s.get('wrong_head') else head(),
             'isMergeQueueEnabled': s.get('queue', True), 'isInMergeQueue': bool(queued),
             'autoMergeRequest': None, 'mergeCommit': {'oid': s.get('merge_oid')} if s.get('merged') else None,
             'mergeQueueEntry': {'headCommit': {'oid': s['group']}, 'enqueuedAt': now(), 'state': 'AWAITING_CHECKS'} if queued else None,
             'timelineItems': {'nodes': [{'createdAt': now(), 'reason': 'required check failed',
                                         'beforeCommit': {'oid': s['group']}}] if removed else []}}
    if s.get('admitted'): s['polls'] = s.get('polls', 0)+1; persist()
    answer({'data': {'repository': {'pullRequest': value}}})
endpoint = next(x for x in a[1:] if x.startswith('repos/'))
slurp = '--slurp' in a
if '/protection/required_status_checks' in endpoint: answer({'contexts': ['Landing policy']})
if '/pulls?' in endpoint: answer([[pr()] if s.get('exists') else []] if slurp else [pr()])
if endpoint.endswith('/comments'):
    if s.get('fail_comments'): raise SystemExit('HTTP 502')
    answer([{'created_at': '2025-01-01T00:00:00Z'}] if s.get('findings') else [])
if endpoint.endswith('/reviews'): answer([])
if '/commits/' in endpoint and '/git/commits/' not in endpoint:
    answer({'commit': {'committer': {'date': now() if s.get('recent') else '2000-01-01T00:00:00Z'}}})
if '/git/commits/' in endpoint:
    answer({'parents': [{'sha': x} for x in g('show', '-s', '--format=%P', endpoint.split('/')[-1]).split()]})
if '/actions/workflows/' in endpoint:
    run = {'id': 42, 'head_sha': s['group'], 'event': 'merge_group', 'conclusion': 'failure',
           'created_at': '2000-01-01T00:00:00Z' if s.get('stale_run') else now(),
           'html_url': 'https://github.com/example/repo/actions/runs/42'}
    answer([{'workflow_runs': [run]}])
if '/actions/runs/42/jobs' in endpoint:
    answer([[{'name': 'Unit / Unit tests (Linux)', 'conclusion': 'failure'}]])
raise SystemExit('unhandled fake endpoint: '+endpoint)
'''


class PRFixture(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="aft-pr-landing-")
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.repo = root / "repo"
        self.repo.mkdir()
        self.origin = root / "origin.git"
        self.state_path = root / "state.json"
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("GIT_CONFIG_") and not key.startswith("GIT_AUTHOR_")
                    and not key.startswith("GIT_COMMITTER_")}
        self.env.update(HOME=str(root / "home"), PR_TEST_STATE=str(self.state_path),
                        LAND_PR_POLL_SECONDS="0", LAND_PR_TIMEOUT_SECONDS="5",
                        OPERATOR_GH_FALLBACK_PATHS="")
        Path(self.env["HOME"]).mkdir()
        for file in ("merge-pr.sh", "land-pr.sh", "landing-policy.py", "merge-queue-search.py",
                     "release.sh", "release-ci-gate.sh", "lib/land_pr.py", "lib/landing_policy.py",
                     "lib/release_queue.py", "lib/operator-gh.sh"):
            dest = self.repo / "scripts" / file
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / "scripts" / file, dest)
        for file in ("search_quality.py", "search_quality_lib.py"):
            dest = self.repo / "benchmarks/aft-search" / file
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / "benchmarks/aft-search" / file, dest)
        watch = self.repo / "scripts/watch-ci.sh"
        watch.write_text('#!/usr/bin/env bash\nset -euo pipefail\n[[ "$WATCH_CI_EVENT" == push && "$WATCH_CI_BRANCH" == main ]]\nprintf "%s\\n" "$1" > "$(dirname "$PR_TEST_STATE")/watched-oid"\n')
        watch.chmod(0o755)
        self.git("init", "-qb", "main")
        self.git("config", "user.name", "ualtinok")
        self.git("config", "user.email", OWNER)
        self.git("config", "commit.gpgsign", "false")
        self.git("config", "tag.gpgsign", "false")
        self.git("config", "core.hooksPath", "/dev/null")
        (self.repo / ".gitignore").write_text("__pycache__/\n*.pyc\n")
        for package in ("aft-bridge", "opencode-plugin", "pi-plugin"):
            path = self.repo / "packages" / package / "package.json"
            path.parent.mkdir(parents=True)
            path.write_text(json.dumps({"version": "1.0.0"}))
        checks = self.repo / ".github/required-checks.txt"
        checks.parent.mkdir()
        checks.write_text("Landing policy\n")
        self.git("add", ".")
        self.git("commit", "-qm", "base")
        self.base = self.git("rev-parse", "HEAD")
        self.git("init", "--bare", "-q", str(self.origin))
        self.git("remote", "add", "origin", str(self.origin))
        self.git("push", "-q", "origin", "main")
        self.git("fetch", "-q", "origin")
        self.git("checkout", "-qb", "accepted")
        self.git("commit", "--allow-empty", "-qm", "accepted delivery\n\n" + COAUTHOR)
        self.git("push", "-q", "origin", "HEAD:refs/heads/land/delivery")
        self.state = {"origin": str(self.origin), "branch": "land/delivery", "queue": True}
        self.write_state()
        bin_dir = root / "bin"
        bin_dir.mkdir()
        executable = bin_dir / "gh"
        executable.write_text(f"#!{sys.executable}\n" + FAKE_GH)
        executable.chmod(0o755)
        self.env["PATH"] = str(bin_dir) + os.pathsep + self.env["PATH"]

    def git(self, *args: str) -> str:
        return subprocess.check_output(["git", *args], cwd=self.repo, env=self.env,
                                       text=True, stderr=subprocess.DEVNULL).strip()

    def write_state(self, **values: object) -> None:
        self.state.update(values)
        self.state_path.write_text(json.dumps(self.state))

    def run_script(self, script: str, *args: str, env: dict | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", str(self.repo / "scripts" / script), *args], cwd=self.repo,
                              env={**self.env, **(env or {})}, text=True, capture_output=True)

    def calls(self) -> list[dict]:
        return [json.loads(line) for line in self.state_path.with_suffix(".calls").read_text().splitlines()]

    def merges(self) -> list[dict]:
        return [call for call in self.calls() if call["args"][:2] == ["pr", "merge"]]

    def merge(self, *args: str, env: dict | None = None) -> subprocess.CompletedProcess:
        return self.run_script("merge-pr.sh", "7", *args, env=env)


class MergeAdmissionTests(PRFixture):
    def test_queue_admission_preserves_head_pin_and_operator_boundary(self) -> None:
        head = self.git("rev-parse", "HEAD")
        result = self.merge()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.merges()[0]["args"], ["pr", "merge", "7", "--match-head-commit", head])
        self.assertEqual(self.merges()[0]["bypass"], "operator")

    def test_unresolved_threads_refuse_admission(self) -> None:
        self.write_state(unresolved=1)
        self.assertNotEqual(self.merge().returncode, 0)
        self.assertEqual(self.merges(), [])

    def test_post_head_findings_require_acknowledgment(self) -> None:
        self.write_state(findings=True)
        self.assertNotEqual(self.merge().returncode, 0)
        self.assertEqual(self.merges(), [])

    def test_explicit_findings_acknowledgment_still_works(self) -> None:
        self.write_state(findings=True)
        result = self.merge(env={"AFT_MERGE_PR_ACK_FINDINGS": "resolved and reviewed"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ACK", result.stdout)

    def test_five_minute_settle_window_is_retained(self) -> None:
        self.write_state(recent=True)
        result = self.merge()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("settle window", result.stderr)

    def test_force_reason_remains_deliberate_admission_override(self) -> None:
        self.write_state(unresolved=1, recent=True, findings=True)
        result = self.merge("--force-reason", "operator reviewed all findings")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("proceeding anyway", result.stdout + result.stderr)

    def test_legacy_requires_explicit_disabled_queue_policy(self) -> None:
        self.write_state(queue=False)
        self.assertNotEqual(self.merge().returncode, 0)
        result = self.merge("--legacy-merge")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--merge", self.merges()[0]["args"])

    def test_force_reason_cannot_bypass_enabled_queue_with_legacy(self) -> None:
        result = self.merge("--legacy-merge", "--force-reason", "not an admin bypass")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.merges(), [])

    def test_unknown_queue_policy_fails_closed(self) -> None:
        self.write_state(queue=None)
        self.assertNotEqual(self.merge("--legacy-merge").returncode, 0)
        self.assertEqual(self.merges(), [])

    def test_accepted_head_change_is_not_force_overridable(self) -> None:
        result = self.merge("--expected-head", "f" * 40, "--force-reason", "cannot waive SHA")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.merges(), [])

    def test_failed_comments_query_is_not_hidden_by_pipeline(self) -> None:
        self.write_state(fail_comments=True)
        self.assertNotEqual(self.merge().returncode, 0)
        self.assertEqual(self.merges(), [])


class DeliveryTests(PRFixture):
    def test_delivery_is_idempotent_and_reports_final_merge_oid(self) -> None:
        result = self.run_script("land-pr.sh", "delivery", env={"GH_SHIM_BYPASS": "operator"})
        self.assertEqual(result.returncode, 0, result.stderr)
        final = result.stdout.strip()
        self.assertEqual(len(final), 40)
        self.assertNotEqual(final, self.git("rev-parse", "HEAD"))
        again = self.run_script("land-pr.sh", "delivery")
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertEqual(again.stdout.strip(), final)
        created = [call for call in self.calls() if call["args"][:2] == ["pr", "create"]]
        self.assertEqual(len(created), 1)
        self.assertEqual(created[0]["bypass"], "")
        self.assertTrue((self.repo / "benchmarks/aft-search/slice-descriptors/land-delivery.json").exists())

    def test_queue_removal_reports_exact_run_and_failing_job(self) -> None:
        self.write_state(remove=True)
        result = self.run_script("land-pr.sh", "delivery")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("actions/runs/42", result.stderr)
        self.assertIn("Unit / Unit tests (Linux): failure", result.stderr)

    def test_stale_failed_group_is_not_attributed_to_new_admission(self) -> None:
        self.write_state(remove=True, stale_run=True)
        result = self.run_script("land-pr.sh", "delivery")
        self.assertNotEqual(result.returncode, 0)
        self.assertNotIn("actions/runs/42", result.stderr)
        self.assertIn("no stale run attributed", result.stderr)


class ReleaseQueueTests(PRFixture):
    def prepare(self) -> None:
        self.git("checkout", "-q", "main")
        self.write_state(branch="land/release-1.0.1")
        helper = self.repo / "scripts/lib/release_queue.py"
        subprocess.run([sys.executable, str(helper), "start", "1.0.1"], cwd=self.repo,
                       env=self.env, check=True, capture_output=True)
        for package in ("aft-bridge", "opencode-plugin", "pi-plugin"):
            (self.repo / "packages" / package / "package.json").write_text(json.dumps({"version": "1.0.1"}))
        self.git("add", "packages")
        self.git("commit", "-qm", "release: v1.0.1\n\n" + COAUTHOR)
        subprocess.run([sys.executable, str(helper), "checkpoint", "1.0.1"], cwd=self.repo,
                       env=self.env, check=True, capture_output=True)

    def test_release_tags_final_merge_oid_and_resumes_same_pr(self) -> None:
        self.prepare()
        source = self.git("rev-parse", "HEAD")
        result = self.run_script("release.sh", "1.0.1")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        final = self.git("rev-list", "-n", "1", "v1.0.1")
        self.assertNotEqual(final, source)
        self.assertEqual(final, self.git("rev-parse", "origin/main"))
        again = self.run_script("release.sh", "1.0.1")
        self.assertEqual(again.returncode, 0, again.stdout + again.stderr)
        self.assertEqual(len([call for call in self.calls() if call["args"][:2] == ["pr", "create"]]), 1)

    def test_release_refuses_required_job_failure_before_tagging(self) -> None:
        self.prepare()
        self.write_state(failed_required=True)
        result = self.run_script("release.sh", "1.0.1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Landing policy", result.stderr)
        exists = subprocess.run(["git", "show-ref", "--verify", "--quiet", "refs/tags/v1.0.1"], cwd=self.repo)
        self.assertNotEqual(exists.returncode, 0)


if __name__ == "__main__":
    unittest.main()
