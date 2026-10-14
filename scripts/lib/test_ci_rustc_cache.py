#!/usr/bin/env python3
"""Tests for the transparent, counter-only compiler-cache measurement wrapper."""

import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location("ci_rustc_cache", Path(__file__).resolve().parents[1] / "ci-rustc-cache.py")
assert SPEC is not None and SPEC.loader is not None
wrapper = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(wrapper)

LIB_ARGS = ["/rustc", "--crate-name", "aft", "--crate-type", "lib", "--emit=dep-info,metadata,link"]
ZERO = {"hits": 0, "misses": 0, "requests": 0, "write_errors": 0}


class RustcCacheWrapperTests(unittest.TestCase):
    def test_observed_library_command_is_measured_but_bin_and_tests_are_not(self):
        self.assertTrue(wrapper.is_aft_library(LIB_ARGS))
        self.assertFalse(wrapper.is_aft_library(["/rustc", "--crate-name", "aft", "--crate-type", "bin"]))
        self.assertFalse(wrapper.is_aft_library([*LIB_ARGS, "--test"]))
        self.assertFalse(wrapper.is_aft_library(["/rustc", "-vV"]))

    def test_exact_one_request_hit_and_miss_are_distinguished(self):
        self.assertEqual(wrapper.proof(ZERO, {**ZERO, "hits": 1, "requests": 1}, 0)["decision"], "hit")
        self.assertEqual(wrapper.proof(ZERO, {**ZERO, "misses": 1, "requests": 1}, 0)["decision"], "miss")

    def test_concurrent_daemon_reset_and_failed_compile_are_not_cache_hit_proofs(self):
        self.assertEqual(wrapper.proof(ZERO, {**ZERO, "hits": 2, "requests": 2}, 0)["decision"], "ambiguous")
        self.assertEqual(wrapper.proof({**ZERO, "hits": 2, "requests": 2}, ZERO, 0)["decision"], "ambiguous")
        self.assertEqual(wrapper.proof(ZERO, {**ZERO, "hits": 1, "requests": 1}, 1)["decision"], "ambiguous")
        self.assertEqual(wrapper.proof(None, ZERO, 0)["decision"], "unavailable")

    def test_compiler_failure_is_preserved_without_stats(self):
        with patch.object(wrapper, "counters", return_value=None), patch.object(wrapper.subprocess, "call", return_value=101) as compiler, patch("builtins.print"):
            self.assertEqual(wrapper.run(LIB_ARGS), 101)
        compiler.assert_called_once_with(["sccache", *LIB_ARGS])

    def test_non_library_requests_are_transparent(self):
        with patch.object(wrapper, "counters") as stats, patch.object(wrapper.subprocess, "call", return_value=0) as compiler:
            self.assertEqual(wrapper.run(["/rustc", "-vV"]), 0)
        stats.assert_not_called()
        compiler.assert_called_once_with(["sccache", "/rustc", "-vV"])

    def test_only_integer_counters_from_real_stats_schema_are_returned(self):
        raw = b'{"stats":{"cache_hits":{"counts":{"Rust":2}},"cache_misses":{"counts":{"Rust":3}},"requests_executed":5,"cache_write_errors":1},"secret":"not-logged"}'
        with patch.object(wrapper.subprocess, "check_output", return_value=raw):
            self.assertEqual(wrapper.counters(), {"hits": 2, "misses": 3, "requests": 5, "write_errors": 1})
        with patch.object(wrapper.subprocess, "check_output", return_value=raw.replace(b'"Rust":2', b'"Rust":"secret"')):
            self.assertIsNone(wrapper.counters())


if __name__ == "__main__":
    unittest.main()
