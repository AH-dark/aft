"""Keep the response comparison sensitive to findings, gaps, and text."""

import copy
import unittest

from scoped_parity import canonical


class ScopedParityTests(unittest.TestCase):
    def test_parity_preserves_object_field_order(self):
        self.assertNotEqual(canonical({"first": 1, "second": 2}),
                            canonical({"second": 2, "first": 1}))

    def test_canonical_ignores_only_request_id_and_last_run(self):
        before = {"id": "one", "scanner_state": {"tier2_last_run": 1, "state": "ready"}, "complete": False}
        after = copy.deepcopy(before)
        after["id"] = "two"
        after["scanner_state"]["tier2_last_run"] = 2
        self.assertEqual(canonical(before), canonical(after))
        self.assertEqual(before["id"], "one", "normalizing must not mutate raw evidence")
        after["complete"] = True
        self.assertNotEqual(canonical(before), canonical(after))

    def test_parity_rejects_finding_changes(self):
        before = {"summary": {"dead_code": {"count": 1}}, "text": "one finding", "gaps": [{"reason": "unknown"}]}
        for key, value in [("summary", {"dead_code": {"count": 0}}), ("text", "clean"), ("gaps", [])]:
            with self.subTest(key=key):
                after = {**before, key: value}
                self.assertNotEqual(canonical(before), canonical(after))


if __name__ == "__main__":
    unittest.main()
