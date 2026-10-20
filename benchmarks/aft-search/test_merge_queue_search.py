from __future__ import annotations

import importlib.util
from pathlib import Path
import unittest


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


if __name__ == "__main__":
    unittest.main()
