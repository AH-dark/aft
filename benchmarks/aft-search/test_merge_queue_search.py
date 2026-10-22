from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "merge_queue_search", ROOT / "scripts/merge-queue-search.py"
)
assert SPEC is not None and SPEC.loader is not None
merge_queue_search = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(merge_queue_search)


class MemberPrRetryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.pr = {
            "number": 23,
            "head": {"sha": "a" * 40, "ref": "feature"},
            "base": {"ref": "main", "repo": {"full_name": "cortexkit/aft"}},
        }

    def test_empty_lookup_retries_until_association_appears(self) -> None:
        responses = iter(([], [self.pr]))
        calls: list[tuple[str, str]] = []
        delays: list[float] = []

        def lookup(repository: str, head: str) -> list[dict]:
            calls.append((repository, head))
            return next(responses)

        result = merge_queue_search.member_pr(
            "cortexkit/aft", self.pr["head"]["sha"], lookup=lookup, sleep=delays.append
        )

        self.assertEqual(result, self.pr)
        self.assertEqual(calls, [("cortexkit/aft", self.pr["head"]["sha"])] * 2)
        self.assertEqual(delays, [5])

    def test_persistent_empty_lookup_fails_after_bounded_retries(self) -> None:
        calls: list[tuple[str, str]] = []
        delays: list[float] = []

        def lookup(repository: str, head: str) -> list[dict]:
            calls.append((repository, head))
            return []

        with self.assertRaisesRegex(
            merge_queue_search.InputFault,
            r"ambiguous_queue_member:.*:matches=0:attempts=5$",
        ):
            merge_queue_search.member_pr(
                "cortexkit/aft",
                self.pr["head"]["sha"],
                lookup=lookup,
                sleep=delays.append,
            )

        self.assertEqual(len(calls), 5)
        self.assertEqual(delays, [5, 10, 15, 30])

    def test_multiple_matches_fail_without_retry(self) -> None:
        calls: list[tuple[str, str]] = []
        delays: list[float] = []
        fallback_calls: list[tuple[str, int]] = []

        def lookup(repository: str, head: str) -> list[dict]:
            calls.append((repository, head))
            return [self.pr, {**self.pr, "number": 24}]

        with self.assertRaisesRegex(
            merge_queue_search.InputFault,
            r"ambiguous_queue_member:.*:matches=2:attempts=1$",
        ):
            merge_queue_search.member_pr(
                "cortexkit/aft",
                self.pr["head"]["sha"],
                queue_ref_pr_number=23,
                lookup=lookup,
                queue_ref_lookup=lambda repository, number: fallback_calls.append((repository, number)),
                sleep=delays.append,
            )

        self.assertEqual(len(calls), 1)
        self.assertEqual(delays, [])
        self.assertEqual(fallback_calls, [])

    def test_lookup_http_error_is_not_retried(self) -> None:
        calls = 0
        delays: list[float] = []

        def lookup(_repository: str, _head: str) -> list[dict]:
            nonlocal calls
            calls += 1
            raise merge_queue_search.InputFault("queue_pr_lookup_failed:head:unauthorized")

        with self.assertRaisesRegex(
            merge_queue_search.InputFault, r"queue_pr_lookup_failed:head:unauthorized$"
        ):
            merge_queue_search.member_pr(
                "cortexkit/aft",
                self.pr["head"]["sha"],
                lookup=lookup,
                sleep=delays.append,
            )

        self.assertEqual(calls, 1)
        self.assertEqual(delays, [])

    def test_queue_ref_pr_is_used_only_when_head_matches(self) -> None:
        calls = 0

        def lookup(_repository: str, _head: str) -> list[dict]:
            nonlocal calls
            calls += 1
            return []

        result = merge_queue_search.member_pr(
            "cortexkit/aft",
            self.pr["head"]["sha"],
            queue_ref_pr_number=23,
            lookup=lookup,
            queue_ref_lookup=lambda _repository, _number: self.pr,
            sleep=lambda _seconds: None,
        )

        self.assertEqual(result, self.pr)
        self.assertEqual(calls, 5)

    def test_queue_ref_pr_with_different_head_does_not_override_fault(self) -> None:
        calls = 0

        def lookup(_repository: str, _head: str) -> list[dict]:
            nonlocal calls
            calls += 1
            return []

        different_head = {**self.pr, "head": {**self.pr["head"], "sha": "b" * 40}}
        with self.assertRaisesRegex(
            merge_queue_search.InputFault,
            r"ambiguous_queue_member:.*:matches=0:attempts=5$",
        ):
            merge_queue_search.member_pr(
                "cortexkit/aft",
                self.pr["head"]["sha"],
                queue_ref_pr_number=23,
                lookup=lookup,
                queue_ref_lookup=lambda _repository, _number: different_head,
                sleep=lambda _seconds: None,
            )

        self.assertEqual(calls, 5)


class GhLookupRetryTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tempdir.cleanup)
        self.root = Path(self.tempdir.name)
        self.responses_path = self.root / "responses.json"
        self.calls_path = self.root / "calls.jsonl"
        gh = self.root / "gh"
        gh.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "root = os.environ['GH_FIXTURE_DIR']\n"
            "with open(os.path.join(root, 'calls.jsonl'), 'a+') as calls:\n"
            "    calls.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            "    calls.seek(0)\n"
            "    call_number = sum(1 for _ in calls)\n"
            "with open(os.path.join(root, 'responses.json')) as source:\n"
            "    responses = json.load(source)\n"
            "response = responses[min(call_number - 1, len(responses) - 1)]\n"
            "sys.stdout.write(response.get('stdout', ''))\n"
            "sys.stderr.write(response.get('stderr', ''))\n"
            "sys.exit(response.get('returncode', 0))\n"
        )
        gh.chmod(0o755)
        self.env = patch.dict(
            os.environ,
            {
                "GH_FIXTURE_DIR": str(self.root),
                "PATH": f"{self.root}{os.pathsep}{os.environ['PATH']}",
            },
        )
        self.env.start()
        self.addCleanup(self.env.stop)

    def configure_responses(self, responses: list[dict]) -> None:
        self.responses_path.write_text(json.dumps(responses))

    def call_count(self) -> int:
        return len(self.calls_path.read_text().splitlines())

    def test_empty_member_reply_retries_until_valid_json(self) -> None:
        expected = [{"number": 23}]
        self.configure_responses(
            [{"stdout": ""}, {"stdout": json.dumps(expected)}]
        )
        delays: list[float] = []

        result = merge_queue_search.lookup_member_prs(
            "cortexkit/aft", "a" * 40, sleep=delays.append
        )

        self.assertEqual(result, expected)
        self.assertEqual(self.call_count(), 2)
        self.assertEqual(delays, [2])

    def test_empty_queue_ref_replies_exhaust_retries(self) -> None:
        self.configure_responses([{"stdout": ""}] * 6)
        delays: list[float] = []

        with self.assertRaisesRegex(
            merge_queue_search.InputFault,
            r"queue_pr_lookup_failed:23:retries_exhausted:attempts=6:",
        ):
            merge_queue_search.lookup_queue_ref_pr(
                "cortexkit/aft", 23, sleep=delays.append
            )

        self.assertEqual(self.call_count(), 6)
        self.assertEqual(delays, [2, 5, 10, 20, 30])

    def test_http_404_is_not_retried(self) -> None:
        self.configure_responses(
            [{"returncode": 1, "stderr": "gh: Not Found (HTTP 404)"}]
        )
        delays: list[float] = []

        with self.assertRaisesRegex(
            merge_queue_search.InputFault,
            r"queue_pr_lookup_failed:a{40}:gh: Not Found \(HTTP 404\)",
        ):
            merge_queue_search.lookup_member_prs(
                "cortexkit/aft", "a" * 40, sleep=delays.append
            )

        self.assertEqual(self.call_count(), 1)
        self.assertEqual(delays, [])

    def test_http_502_retries_and_succeeds(self) -> None:
        expected = {"number": 23}
        self.configure_responses(
            [
                {"returncode": 1, "stderr": "gh: Bad Gateway (HTTP 502)"},
                {"stdout": json.dumps(expected)},
            ]
        )
        delays: list[float] = []

        result = merge_queue_search.lookup_queue_ref_pr(
            "cortexkit/aft", 23, sleep=delays.append
        )

        self.assertEqual(result, expected)
        self.assertEqual(self.call_count(), 2)
        self.assertEqual(delays, [2])

    def test_nonzero_json_parse_error_retries(self) -> None:
        expected = {"number": 23}
        self.configure_responses(
            [
                {"returncode": 1, "stderr": "unexpected end of JSON input"},
                {"stdout": json.dumps(expected)},
            ]
        )
        delays: list[float] = []

        result = merge_queue_search.lookup_queue_ref_pr(
            "cortexkit/aft", 23, sleep=delays.append
        )

        self.assertEqual(result, expected)
        self.assertEqual(self.call_count(), 2)
        self.assertEqual(delays, [2])

    def test_timeout_retries_then_succeeds(self) -> None:
        expected = {"number": 23}
        timeout = merge_queue_search.subprocess.TimeoutExpired("gh api", 30)
        success = merge_queue_search.subprocess.CompletedProcess(
            args=["gh", "api"], returncode=0, stdout=json.dumps(expected), stderr=""
        )
        delays: list[float] = []

        with patch.object(
            merge_queue_search.subprocess, "run", side_effect=[timeout, success]
        ) as run:
            result = merge_queue_search.lookup_queue_ref_pr(
                "cortexkit/aft", 23, sleep=delays.append
            )

        self.assertEqual(result, expected)
        self.assertEqual(run.call_count, 2)
        self.assertEqual(delays, [2])

    def test_timeout_exhaustion_reports_attempts_and_decodes_stderr(self) -> None:
        timeouts = [
            merge_queue_search.subprocess.TimeoutExpired("gh api", 30, stderr=None),
        ] + [
            merge_queue_search.subprocess.TimeoutExpired(
                "gh api", 30, stderr=b"partial\xff"
            )
            for _ in range(5)
        ]
        delays: list[float] = []

        with patch.object(
            merge_queue_search.subprocess, "run", side_effect=timeouts
        ) as run:
            with self.assertRaisesRegex(
                merge_queue_search.InputFault,
                r"queue_pr_lookup_failed:23:retries_exhausted:attempts=6:"
                r"gh api timed out after 30 seconds",
            ) as error:
                merge_queue_search.lookup_queue_ref_pr(
                    "cortexkit/aft", 23, sleep=delays.append
                )

        self.assertIn("partial\ufffd", str(error.exception))
        self.assertEqual(run.call_count, 6)
        self.assertEqual(delays, [2, 5, 10, 20, 30])

    def test_member_lookup_http_500_retries_and_succeeds(self) -> None:
        expected = [{"number": 23}]
        self.configure_responses(
            [
                {"returncode": 1, "stderr": "gh: Internal Server Error (HTTP 500)"},
                {"stdout": json.dumps(expected)},
            ]
        )
        delays: list[float] = []

        result = merge_queue_search.lookup_member_prs(
            "cortexkit/aft", "a" * 40, sleep=delays.append
        )

        self.assertEqual(result, expected)
        self.assertEqual(self.call_count(), 2)
        self.assertEqual(delays, [2])

    def test_member_lookup_persistent_5xx_exhausts_after_six_attempts(self) -> None:
        self.configure_responses(
            [
                {"returncode": 1, "stderr": "gh: Bad Gateway (HTTP 502)"},
                {"returncode": 1, "stderr": "gh: Service Unavailable (HTTP 503)"},
                {"returncode": 1, "stderr": "gh: Gateway Timeout (HTTP 504)"},
                {"returncode": 1, "stderr": "gh: Internal Server Error (HTTP 500)"},
            ]
        )
        delays: list[float] = []

        with self.assertRaisesRegex(
            merge_queue_search.InputFault,
            r"queue_pr_lookup_failed:a{40}:retries_exhausted:attempts=6:"
            r"gh: Internal Server Error \(HTTP 500\)$",
        ):
            merge_queue_search.lookup_member_prs(
                "cortexkit/aft", "a" * 40, sleep=delays.append
            )

        self.assertEqual(self.call_count(), 6)
        self.assertEqual(delays, [2, 5, 10, 20, 30])

    def test_member_lookup_http_401_and_403_are_not_retried(self) -> None:
        for stderr in (
            "gh: Bad credentials (HTTP 401)",
            "gh: Resource not accessible by integration (HTTP 403)",
        ):
            with self.subTest(stderr=stderr):
                self.calls_path.unlink(missing_ok=True)
                self.configure_responses([{"returncode": 1, "stderr": stderr}])
                delays: list[float] = []

                with self.assertRaises(merge_queue_search.InputFault) as error:
                    merge_queue_search.lookup_member_prs(
                        "cortexkit/aft", "a" * 40, sleep=delays.append
                    )

                self.assertNotIn("retries_exhausted", str(error.exception))
                self.assertEqual(self.call_count(), 1)
                self.assertEqual(delays, [])

    def test_member_lookup_requests_one_page_without_pagination(self) -> None:
        self.configure_responses([{"stdout": "[]"}])

        merge_queue_search.lookup_member_prs(
            "cortexkit/aft", "a" * 40, sleep=lambda _seconds: None
        )

        calls = [json.loads(line) for line in self.calls_path.read_text().splitlines()]
        self.assertEqual(
            calls,
            [["api", f"repos/cortexkit/aft/commits/{'a' * 40}/pulls?per_page=100"]],
        )
        self.assertNotIn("--paginate", calls[0])
        self.assertNotIn("--slurp", calls[0])

    def test_member_lookup_refuses_a_full_page(self) -> None:
        self.configure_responses(
            [{"stdout": json.dumps([{"number": n} for n in range(100)])}]
        )

        with self.assertRaisesRegex(
            merge_queue_search.InputFault,
            r"queue_pr_lookup_failed:a{40}:page_full:count=100:",
        ):
            merge_queue_search.lookup_member_prs(
                "cortexkit/aft", "a" * 40, sleep=lambda _seconds: None
            )

        self.assertEqual(self.call_count(), 1)

    def test_member_lookup_accepts_ninety_nine_results(self) -> None:
        expected = [{"number": n} for n in range(99)]
        self.configure_responses([{"stdout": json.dumps(expected)}])

        result = merge_queue_search.lookup_member_prs(
            "cortexkit/aft", "a" * 40, sleep=lambda _seconds: None
        )

        self.assertEqual(result, expected)


if __name__ == "__main__":
    unittest.main()
