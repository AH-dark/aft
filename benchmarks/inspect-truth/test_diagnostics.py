#!/usr/bin/env python3
"""Diagnostics-only collector checks; no corpus or native binary required."""

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import diagnostics


class DiagnosticsComparisonTests(unittest.TestCase):
    def data(self, complete=True, gaps=None):
        return {"sample": ["src/a.ts"], "items": [], "pages": [{
            "success": True, "summary": {"complete": complete, "gaps": gaps or []}}]}

    def oracle(self, status="collected"):
        return {"status": status, "items": [], "failures": []}

    def test_missing_producer_is_unknown_not_zero(self):
        gap = {"kind": "failed_producer", "producer": "typescript", "reason": "unavailable"}
        result = diagnostics.compare("fixture", "typescript", Path("."),
                                     self.data(False, [gap]), self.oracle())
        self.assertEqual(result["status"], "unknown")
        self.assertIsNone(result["aft_count"])
        self.assertEqual(result["observed_aft_count"], 0)
        self.assertEqual(result["producer_gaps"], [gap])

    def test_completed_empty_sample_is_scoreable(self):
        result = diagnostics.compare("fixture", "typescript", Path("."), self.data(), self.oracle())
        self.assertEqual(result["status"], "scored")
        self.assertEqual(result["aft_count"], 0)

    def test_complete_flag_may_be_omitted_on_success(self):
        data = self.data()
        del data["pages"][0]["summary"]["complete"]
        result = diagnostics.compare("fixture", "typescript", Path("."), data, self.oracle())
        self.assertEqual(result["status"], "scored")

    def test_unavailable_oracle_is_not_a_zero_reference(self):
        result = diagnostics.compare("fixture", "typescript", Path("."),
                                     self.data(), self.oracle("unavailable"))
        self.assertEqual(result["status"], "unknown")
        self.assertIsNone(result["precision"])
        self.assertEqual(result["oracle_status"], "unavailable")

    def test_real_footer_masks_missing_producer(self):
        result = diagnostics.render_footer({"cache_role": "primary", "status_bar": {
            "errors": 7, "warnings": 3, "diagnostics": "producer_missing"}})
        self.assertIn("E? W?", result)
        self.assertNotIn("E7", result)

    def test_real_footer_retains_authoritative_numbers(self):
        result = diagnostics.render_footer({"cache_role": "primary", "status_bar": {
            "errors": 7, "warnings": 3}})
        self.assertIn("E7 W3", result)

    def test_same_mode_skip_is_limited_to_own_config(self):
        with tempfile.TemporaryDirectory(dir=diagnostics.truth.CHECKOUT_ROOT / "target") as directory:
            root = Path(directory)
            config = root / ".cortexkit" / "aft.jsonc"
            config.parent.mkdir()
            config.write_text("{}")
            other = root / "other"
            other.write_text("{}")
            with patch.object(diagnostics.truth, "CHECKOUT_ROOT", root), patch.object(Path, "chmod") as original:
                with diagnostics.same_mode_restore(True) as skipped:
                    config.chmod(config.stat().st_mode)
                    other.chmod(other.stat().st_mode)
                    config.chmod(0o600)
                self.assertEqual(skipped, [str(config)])
                self.assertEqual(original.call_count, 2)


if __name__ == "__main__":
    unittest.main()
