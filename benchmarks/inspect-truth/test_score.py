"""Offline regression fixtures for the inspect-truth collection and score gate."""

import importlib.util
import io
import json
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from typing import Any, Dict
from unittest.mock import Mock, patch

MODULE = importlib.util.spec_from_file_location("inspect_truth", Path(__file__).with_name("run.py"))
assert MODULE is not None and MODULE.loader is not None
harness = importlib.util.module_from_spec(MODULE)
MODULE.loader.exec_module(harness)


def row(symbol="X", category="dead_code"):
    return {"path": "src/a.rs", "symbol": symbol, "line": 1, "category": category}


class ScoreFixtureBase(unittest.TestCase):
    def setUp(self):
        target = harness.CHECKOUT_ROOT / "target"
        target.mkdir(exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(dir=target)
        self.root = Path(self.temp.name)
        self.spec: Dict[str, Any] = {"name": "fixture", "commit": "pin", "languages": [
            {"language": "rust", "include_paths": ["src/"], "oracle": {}}]}
        self.before = self.output("before", [])
        self.after = self.output("after", [])
        self.judgments = self.root / "judgments"
        self.judgments.mkdir()

    def tearDown(self):
        self.temp.cleanup()

    def output(self, name, rows, oracle=None, summary=None):
        out = self.root / name
        out.mkdir(exist_ok=True)
        items = [{"file": r["path"], "symbol": r["symbol"], "line": r["line"]} for r in rows]
        (out / "aft.json").write_text(json.dumps({"commit": "pin", "items": {"dead_code": items},
            "project_summary": {"dead_code": summary or {"count": len(rows)}}}))
        (out / "oracle.json").write_text(json.dumps({"rust": {
            "buckets": {"dead_code": oracle or []}, "unused_files": []}}))
        return out

    def judge(self, symbol="X", side="aft", category="dead_code", after=None, filename="A.json"):
        entry = {"repo": "fixture", "category": category, "side": side, "path": "src/a.rs",
                 "symbol": symbol, "verdict": "aft_correct" if side == "aft" else "oracle_wrong",
                 "note": "Fixture row checked independently."}
        (self.judgments / filename).write_text(json.dumps({
            "after_outputs": {"fixture": str(after or self.after)}, "entries": [entry]}))

    def score(self, out=None):
        return harness.score_repo(self.root, self.spec, out=out or self.after,
            baseline_out=self.before, judgments_dir=self.judgments)

    def bucket(self, result):
        return next(b for b in result["buckets"] if b["category"] == "dead_code")


class ScoreFixtures(ScoreFixtureBase):
    def test_typeorm_fixture_exclusions_are_applied_and_restored(self):
        spec = next(s for s in harness.load_corpus()[1] if s["name"] == "typeorm")
        repo = self.root / "source"
        repo.mkdir()
        ignore = repo / ".aftignore"
        original = b"# existing project rules\nkeep-out/\n"
        ignore.write_bytes(original)
        expected = "packages/codemod/test/transforms/v1/fixtures/column-readonly/column-readonly-metadata-args-namespace.input.ts"
        session = Mock()
        def configured(*args):
            self.assertIn(expected, ignore.read_text())
            return {"success": True}
        session.configure.side_effect = configured
        with patch.object(harness, "tracked_files", return_value=[]), \
             patch.object(harness, "AftSession", return_value=session), \
             patch.object(harness, "wait_until_ready", return_value={"summary": {}}), \
             patch.object(harness, "collect_category", return_value=[]):
            record = harness.collect_aft_record(repo, spec, Path("aft"), self.after, 0, "pin")
        self.assertEqual(ignore.read_bytes(), original)
        self.assertEqual(len(record["measurement_exclusions"]), 4)

    def test_typeorm_fixture_exclusions_restore_on_collection_error(self):
        spec = next(s for s in harness.load_corpus()[1] if s["name"] == "typeorm")
        repo = self.root / "source"
        repo.mkdir()
        session = Mock()
        session.configure.side_effect = RuntimeError("collection failed")
        with patch.object(harness, "tracked_files", return_value=[]), \
             patch.object(harness, "AftSession", return_value=session):
            with self.assertRaisesRegex(RuntimeError, "collection failed"):
                harness.collect_aft_record(repo, spec, Path("aft"), self.after, 0, "pin")
        self.assertFalse((repo / ".aftignore").exists())

    def test_judgment_before_only(self):
        self.output("before", [row()])
        self.judge()
        result = self.score()
        self.assertEqual(result["judgments_unapplied"]["A.json"], 1)
        self.assertEqual(self.bucket(result)["aft_rows_judged"], 0)

    def test_judgment_after_only(self):
        self.output("after", [row()])
        self.judge()
        bucket = self.bucket(self.score())
        self.assertEqual((bucket["aft_rows_judged"], bucket["aft_rows_unjudged"]), (1, 0))

    def test_judgment_absent_from_both_errors(self):
        self.judge()
        with self.assertRaisesRegex(ValueError, "A.json.*absent"):
            self.score()

    def test_judgment_sequential_A_B(self):
        self.output("after", [row()])
        self.judge()
        self.score()
        later = self.output("B", [])
        result = self.score(later)
        self.assertEqual(result["judgments_unapplied"]["A.json"], 1)
        self.assertEqual(len(json.loads((self.judgments / "A.json").read_text())["entries"]), 1)

    def test_oracle_judgments_drop_rows_before_matching(self):
        self.output("after", [row()], oracle=[row()])
        self.judge(side="oracle")
        bucket = self.bucket(self.score())
        self.assertEqual((bucket["oracle_rows_judged_out"], bucket["oracle_count"], bucket["agreed"]), (1, 0, 0))

    def test_judgments_are_category_scoped(self):
        self.output("after", [row()], oracle=[row()])
        self.judge()
        entries = harness.load_judgments(self.spec, self.before, self.judgments)
        kept, judged_out, judged = harness.apply_judgments("unused_exports", [row()], [row()], entries)
        self.assertEqual((len(kept), judged_out, judged), (1, 0, 0))

    def test_language_unsupported_is_scoreable(self):
        self.output("after", [row()], oracle=[row()], summary={"count": 1, "complete": False,
            "gaps": [{"kind": "language_unsupported", "language": "python", "files": 3}]})
        bucket = self.bucket(self.score())
        self.assertEqual(bucket["status"], "scored")
        self.assertEqual(bucket["precision"], 1.0)
        self.assertEqual(bucket["languages_unsupported"], [{"language": "python", "files": 3}])

    def test_analysis_incomplete_blocks_scoring(self):
        self.output("after", [row()], oracle=[row()], summary={"count": 1, "complete": False,
            "gaps": [{"kind": "analysis_incomplete", "files": 3}]})
        bucket = self.bucket(self.score())
        self.assertEqual(bucket["status"], "unknown")
        self.assertIsNone(bucket["aft_count"])
        self.assertIsNone(bucket["precision"])

    def test_tier2_unavailable_and_terminal_unavailable_are_unknown(self):
        for summary in ({"status": "unavailable"}, {"count": 0, "gaps": [{"kind": "tier2_unavailable"}]}):
            with self.subTest(summary=summary):
                self.output("after", [], summary=summary)
                self.assertIsNone(self.bucket(self.score())["aft_count"])

    def test_cell_rule_defined_to_na_precision_only_fully_judged(self):
        before = {"precision": 0.5, "recall": None, "aft_count": 2, "aft_rows_judged": 2}
        after = {"precision": None, "recall": None, "aft_count": 0}
        self.assertEqual(harness.cell_changes(before, after), [])
        before["aft_rows_judged"] = 1
        self.assertIn("precision", harness.cell_changes(before, after))

    def test_cell_rule_recall_na_and_lowering_fail(self):
        self.assertEqual(harness.cell_changes({"precision": 0.8, "recall": 0.5},
            {"precision": 0.7, "recall": None, "aft_count": 0}), ["precision", "recall"])

    def test_no_baseline_is_new(self):
        self.spec["baseline"] = False
        result = harness.score_repo(self.root, self.spec, out=self.after, judgments_dir=self.judgments)
        self.assertEqual(result["baseline"], "new, no baseline")

    def test_baseline_truncated_scopes_excluded_from_both_sides(self):
        self.output("before", [row()], oracle=[row()])
        record = json.loads((self.before / "aft.json").read_text())
        record["truncated_scopes"] = [{"category": "dead_code", "path": "src/a.rs", "count": 134, "returned": 100}]
        (self.before / "aft.json").write_text(json.dumps(record))
        self.output("after", [row()], oracle=[row()])
        result = self.score()
        bucket = self.bucket(result)
        self.assertEqual((bucket["aft_count"], bucket["oracle_count"]), (0, 0))
        self.assertEqual(len(result["baseline_aft_truncated_scopes"]), 1)


class CollectionFixtures(unittest.TestCase):
    def response(self, items, total, envelope=None, summary=None):
        details = {"dead_code": items}
        if envelope is not None:
            details["dead_code_list_envelope"] = envelope
        return {"success": True, "summary": {"dead_code": summary or {"count": total}}, "details": details}

    def test_offset_paging_only_when_envelope_advertises_it(self):
        first = self.response([{"file": "a.rs", "symbol": "A", "line": 1}], 2,
            {"total": 2, "offset": 0, "next_offset": 1})
        second = self.response([{"file": "a.rs", "symbol": "B", "line": 2}], 2,
            {"total": 2, "offset": 1, "next_offset": None})
        session = Mock()
        session.inspect.side_effect = [first, second]
        self.assertEqual(len(harness.collect_category(session, "dead_code", [], {}, [])), 2)
        self.assertEqual(session.inspect.call_args.kwargs["offset"], 1)

    def test_unknown_offset_ignored_uses_scope_fallback(self):
        session = Mock()
        session.inspect.side_effect = [self.response([], 2, {"total": 2}),
            self.response([{"symbol": "A"}], 1), self.response([{"symbol": "B"}], 1)]
        rows = harness.collect_category(session, "dead_code", ["a.rs", "b.rs"], {}, [])
        self.assertEqual(len(rows), 2)
        self.assertTrue(all("offset" not in call.kwargs for call in session.inspect.call_args_list))

    def test_offset_paging_reads_exact_total_object_not_summary_count(self):
        first = self.response([{"file": "a.rs", "symbol": "A", "line": 1}], 1,
            {"total": {"kind": "exact", "value": 2}, "offset": 0, "next_offset": 1})
        second = self.response([{"file": "a.rs", "symbol": "B", "line": 2}], 1,
            {"total": {"kind": "exact", "value": 2}, "offset": 1, "next_offset": None})
        session = Mock()
        session.inspect.side_effect = [first, second]
        rows = harness.collect_category(session, "dead_code", [], {}, [])
        self.assertEqual([item["symbol"] for item in rows], ["A", "B"])
        self.assertEqual(session.inspect.call_args.kwargs["offset"], 1)

    def test_offset_paging_rejects_missing_rows_with_exact_total_object(self):
        session = Mock()
        session.inspect.return_value = self.response(
            [{"file": "a.rs", "symbol": "A", "line": 1}], 1,
            {"total": {"kind": "exact", "value": 2}, "offset": 0, "next_offset": None})
        with self.assertRaisesRegex(RuntimeError, "collected 1 of"):
            harness.collect_category(session, "dead_code", [], {}, [])

    def test_pending_retried_but_terminal_unavailable_not_retried(self):
        session = Mock()
        session.inspect.side_effect = [self.response([], None, summary={"status": "pending"}),
            self.response([], 0), self.response([], None, summary={"status": "unavailable"})]
        with patch.object(harness.time, "sleep"):
            ready = harness.wait_until_ready(session, 60)
            terminal = harness.wait_until_ready(session, 60)
        self.assertEqual(ready["summary"]["dead_code"]["count"], 0)
        self.assertEqual(terminal["summary"]["dead_code"]["status"], "unavailable")
        self.assertEqual(session.inspect.call_count, 3)

    def test_pending_page_retries_the_same_offset(self):
        session = Mock()
        session.inspect.side_effect = [self.response([{"file": "a.rs", "symbol": "A", "line": 1}], 2,
            {"total": 2, "offset": 0, "next_offset": 1}),
            self.response([], None, summary={"status": "pending"}),
            self.response([{"file": "a.rs", "symbol": "B", "line": 2}], 2,
                {"total": 2, "offset": 1, "next_offset": None})]
        with patch.object(harness.time, "sleep"):
            self.assertEqual(len(harness.collect_category(session, "dead_code", [], {}, [])), 2)
        self.assertEqual([c.kwargs.get("offset") for c in session.inspect.call_args_list], [None, 1, 1])

    def test_baseline_building_gap_is_retried(self):
        session = Mock()
        session.inspect.side_effect = [self.response([], None, summary={"unavailable": True,
            "gaps": [{"kind": "analysis_incomplete", "reason": "inspect_phase_timeout; builder_state=building"}]}),
            self.response([], 0)]
        with patch.object(harness.time, "sleep"):
            ready = harness.wait_until_ready(session, 60)
        self.assertEqual(ready["summary"]["dead_code"].get("count"), 0)

    def test_pagination_rejects_overlap_or_changed_total(self):
        for second in (
            self.response([{"file": "a.rs", "symbol": "A", "line": 1}], 2,
                {"total": 2, "offset": 1, "next_offset": None}),
            self.response([{"file": "a.rs", "symbol": "B", "line": 2}], 3,
                {"total": 3, "offset": 1, "next_offset": None}),
        ):
            with self.subTest(second=second):
                session = Mock()
                session.inspect.side_effect = [self.response([{"file": "a.rs", "symbol": "A", "line": 1}], 2,
                    {"total": 2, "offset": 0, "next_offset": 1}), second]
                with self.assertRaises(RuntimeError):
                    harness.collect_category(session, "dead_code", [], {}, [])


class CheckoutFixtures(ScoreFixtureBase):
    def test_local_checkout_restores_config_and_status_even_on_error(self):
        self.spec["local_checkout"] = True
        config = self.root / ".cortexkit" / "aft.jsonc"
        config.parent.mkdir()
        config.write_bytes(b"// Tracked configuration\n{}\n")
        before = config.read_bytes()
        record = {}
        with patch.object(harness, "CHECKOUT_ROOT", self.root), patch.object(harness, "git_output", return_value=" M run.py\n"):
            with self.assertRaisesRegex(RuntimeError, "collection failed"):
                with harness.local_hygiene(self.root, self.spec) as record:
                    config.write_bytes(b"overwritten")
                    raise RuntimeError("collection failed")
        self.assertEqual(config.read_bytes(), before)
        self.assertEqual(record["git_status_before"], record["git_status_after"])

    def test_local_checkout_removes_a_new_config(self):
        self.spec["local_checkout"] = True
        config = self.root / ".cortexkit" / "aft.jsonc"
        with patch.object(harness, "CHECKOUT_ROOT", self.root), patch.object(harness, "git_output", return_value=""):
            with harness.local_hygiene(self.root, self.spec):
                config.parent.mkdir()
                config.write_text("temporary")
        self.assertFalse(config.exists())
        self.assertFalse(config.parent.exists())

    def test_local_checkout_does_not_hide_install_changes(self):
        self.spec["local_checkout"] = True
        with patch.object(harness, "CHECKOUT_ROOT", self.root), patch.object(harness, "git_output", side_effect=["", " M tracked.txt\n"]):
            with self.assertRaisesRegex(RuntimeError, "changed git status"):
                with harness.local_hygiene(self.root, self.spec):
                    pass

    def test_install_guard_is_per_language_directory(self):
        self.spec["languages"][0].update(install=["cargo", "unused"], install_cwd="rust")
        self.spec["languages"].append({"language": "typescript", "include_paths": ["src/"],
            "oracle": {}, "install": ["bun", "install"], "install_cwd": "ts"})
        (self.root / "fixture" / "rust" / "node_modules").mkdir(parents=True)
        with patch.object(harness, "run", return_value=(0, "", "", 0)) as runner:
            harness.install(self.root, self.spec)
        runner.assert_called_once_with(["bun", "install"], cwd=self.root / "fixture" / "ts", env=None)

    def test_local_checkout_pin_is_read_from_git(self):
        self.spec["local_checkout"] = True
        with patch.object(harness, "git_output", return_value="actual-head\n") as git:
            self.assertEqual(harness.corpus_pin(self.root, self.spec), "actual-head")
        git.assert_called_once_with(harness.CHECKOUT_ROOT, "rev-parse", "HEAD")

    def test_collections_preserve_baseline_and_use_fresh_storage_on_retry(self):
        session = Mock()
        session.configure.side_effect = [{"success": False}, {"success": True}, {"success": True}]
        session.calls = 1
        session.inspect.return_value = {"success": True,
            "summary": {c: {"count": 0} for c in harness.AFT_CATEGORIES}, "details": {}}
        out = harness.raw_dir(self.root, self.spec)
        (self.root / "fixture").mkdir()
        with patch.object(harness, "corpus_pin", return_value="pin"), patch.object(harness, "tracked_files", return_value=[]), \
                patch.object(harness, "AftSession", return_value=session) as sessions:
            with self.assertRaisesRegex(RuntimeError, "configure failed"):
                harness.collect_aft(self.root, self.spec, Path("aft"), 0)
            harness.collect_aft(self.root, self.spec, Path("aft"), 0)
            first = (out / "aft.json").read_bytes()
            harness.collect_aft(self.root, self.spec, Path("different-aft"), 0)
            self.assertEqual((out / "aft.json").read_bytes(), first)
            harness.collect_aft(self.root, self.spec, Path("aft"), 0, "aaaaaaaaa")
        self.assertEqual(sessions.call_count, 3)
        self.assertNotEqual(sessions.call_args_list[0].args[2], sessions.call_args_list[1].args[2])
        self.assertNotEqual(sessions.call_args_list[1].args[2], sessions.call_args_list[2].args[2])

    def test_one_bucket_set_per_language(self):
        self.spec["languages"].append({"language": "typescript", "include_paths": ["src/"], "oracle": {}})
        snapshot = json.loads((self.after / "oracle.json").read_text())
        snapshot["typescript"] = {"buckets": {"dead_code": []}}
        (self.after / "oracle.json").write_text(json.dumps(snapshot))
        self.assertEqual([b["language"] for b in self.score()["buckets"]], ["rust", "typescript"])

    def test_oracles_are_cached_once_per_corpus_pin(self):
        with patch.object(harness, "corpus_pin", return_value="pin"), \
                patch.object(harness, "run_language_oracles") as runner, \
                patch.object(harness, "normalize_oracles", return_value={"buckets": {"dead_code": []}}), \
                patch.object(harness, "install"):
            first = harness.run_oracles(self.root, self.spec, False)
            second = harness.run_oracles(self.root, self.spec, False, "aaaaaaaaa")
        self.assertEqual(first, second)
        self.assertEqual(runner.call_count, 1)

    def test_aft_stderr_backpressure_does_not_block_responses(self):
        binary = self.root / "fake-aft"
        binary.write_text("#!/usr/bin/env python3\nimport sys, json\n"
            "for line in sys.stdin:\n"
            "    request = json.loads(line)\n"
            "    sys.stderr.write('warning\\n' * 200000)\n"
            "    sys.stderr.flush()\n"
            "    print(json.dumps({'id':request['id'], 'success':True}), flush=True)\n")
        binary.chmod(0o755)
        driver = (f"import sys; sys.path.insert(0, {str(harness.HERE)!r}); import run; "
                  f"from pathlib import Path; s=run.AftSession(Path({str(binary)!r}), "
                  f"Path({str(self.root)!r}), Path({str(self.root / 'storage')!r})); "
                  "assert s.configure()['success']; s.close()")
        try:
            proc = subprocess.run([sys.executable, "-c", driver], capture_output=True, text=True, timeout=10)
        except subprocess.TimeoutExpired:
            self.fail("Undrained AFT stderr blocked the configure response")
        self.assertEqual(proc.returncode, 0, proc.stderr)


class OfflineCallgraphRerunFixtures(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        script = Path(__file__).with_name("rerun-callgraph-corpus.sh").read_text()
        python = script.split("<<'PY'\n", 1)[1].rsplit("\nPY\n", 1)[0]
        self.driver = {"__name__": "offline_rerun_fixture"}
        exec(compile(python, "rerun-callgraph-corpus.sh", "exec"), self.driver)
        self.corpus = self.root / "copied-corpus"
        self.judgments = self.root / "judgments"
        self.judgments.mkdir()
        self.specs, self.originals, entries, outputs = [], {}, [], {}
        for name in self.driver["REPOS"]:
            lang = "go" if name == "soft-serve" else "rust"
            path = "src/a.go" if lang == "go" else "src/a.rs"
            repo = self.corpus / name
            (repo / "src").mkdir(parents=True)
            (repo / path).write_text("pub fn X() {}\npub fn Y() {}\n")
            (repo / ".cortexkit").mkdir()
            (repo / ".cortexkit" / "aft.jsonc").write_bytes(b"original config\n")
            spec = {"name": name, "commit": "pin-" + name, "languages": [
                {"language": lang, "include_paths": ["src/"], "oracle": {}}]}
            self.specs.append(spec)
            row = {"file": path, "symbol": "X", "line": 1, "kind": "function"}
            for revision in (harness.BASELINE_COMMIT, "aaaaaaaaa"):
                directory = self.corpus / "_results" / name / revision
                directory.mkdir(parents=True)
                record = {"commit": spec["commit"], "items": {"dead_code": [row]},
                          "project_summary": {c: {"count": 1 if c == "dead_code" else 0}
                                              for c in harness.AFT_CATEGORIES}}
                (directory / "aft.json").write_text(json.dumps(record))
                oracle = {lang: {"buckets": {"dead_code": [], "todos": []}}}
                (directory / "oracle.json").write_text(json.dumps(oracle))
                for source in directory.iterdir():
                    self.originals[source] = source.read_bytes()
            entries.append({"repo": name, "category": "dead_code", "side": "aft",
                            "path": path, "symbol": "X", "verdict": "aft_false_positive",
                            "note": "fixture row judgment"})
            outputs[name] = "target/old-origin/_results/" + name + "/aaaaaaaaa"
        (self.judgments / "fixture.json").write_text(json.dumps({"entries": entries, "after_outputs": outputs}))
        self.base, self.candidate = self.root / "base", self.root / "candidate"
        self.base.write_bytes(b"base binary fixture")
        self.candidate.write_bytes(b"candidate binary fixture")
        self.output = self.root / "new-output"

    def tearDown(self):
        self.temp.cleanup()

    def run_driver(self, candidate_rows=0, unknown=False, tamper=False):
        sessions = []
        def session(binary, repo, storage):
            rows = []
            count = 1 if binary == self.base else candidate_rows
            extension = "go" if repo.name == "soft-serve" else "rs"
            for symbol in ["X", "Y"][:count]:
                rows.append({"file": f"src/a.{extension}", "symbol": symbol, "line": 1, "kind": "function"})
            response = {"success": True, "summary": {c: {"count": len(rows) if c == "dead_code" else 0}
                        for c in harness.AFT_CATEGORIES}, "details": {"dead_code": rows}}
            if unknown and binary == self.candidate:
                response["summary"]["dead_code"] = {"status": "unavailable"}
            client = Mock()
            client.configure.return_value = {"success": True}
            client.inspect.return_value = response
            client.calls = 1
            sessions.append((binary, repo, storage))
            if tamper and len(sessions) == 1:
                if tamper == "binary":
                    self.candidate.write_bytes(self.candidate.read_bytes() + b"changed")
                else:
                    original = self.corpus / "_results" / "ripgrep" / harness.BASELINE_COMMIT / "oracle.json"
                    original.write_bytes(original.read_bytes() + b"\n")
            return client
        def git(repo, *args):
            if args == ("rev-parse", "HEAD"):
                return "pin-" + repo.name
            if args == ("status", "--porcelain", "--untracked-files=no"):
                return ""
            raise AssertionError(f"unexpected Git command: {args}")
        info = lambda binary, baseline=False: {"path": str(binary), "version": "aft 0.58.2" if baseline else "aft 0.59.0", "sha256": self.driver["sha256"](binary)}
        with patch.object(harness, "git_output", side_effect=git), \
                patch.object(harness, "tracked_files", return_value=["src/a.rs"]), \
                patch.object(harness, "AftSession", side_effect=session), \
                patch.object(harness, "fetch", side_effect=AssertionError("no fetch")), \
                patch.object(harness, "install", side_effect=AssertionError("no install")), \
                patch.object(harness, "run_oracles", side_effect=AssertionError("no oracle rerun")), \
                patch.dict(self.driver, binary_info=info), redirect_stdout(io.StringIO()):
            code = self.driver["rerun"](harness, self.corpus, self.specs, self.base,
                                        self.candidate, self.output, self.judgments)
        return code, sessions

    def test_fresh_both_sides_preserve_origins_and_relocate_judgments(self):
        code, sessions = self.run_driver()
        self.assertEqual(code, 0)
        self.assertEqual(len(sessions), 6)
        self.assertEqual([row[0] for row in sessions], [self.base, self.candidate] * 3)
        self.assertEqual(len({row[2] for row in sessions}), 6)
        for source, previous in self.originals.items():
            self.assertEqual(source.read_bytes(), previous)
        for spec in self.specs:
            self.assertEqual((self.corpus / spec["name"] / ".cortexkit" / "aft.jsonc").read_bytes(), b"original config\n")
        relocated = json.loads((self.output / "judgments" / "fixture.json").read_text())
        original = json.loads((self.judgments / "fixture.json").read_text())
        self.assertEqual(relocated["entries"], original["entries"])
        self.assertTrue(all(str(self.corpus) in value for value in relocated["after_outputs"].values()))
        cells = json.loads((self.output / "cell-diff.json").read_text())
        counts = [cell for cell in cells if cell["category"] == "dead_code" and cell["metric"] == "aft_count"]
        self.assertEqual([(cell["before"], cell["after"], cell["delta"]) for cell in counts], [(1, 0, -1)] * 3)

    def test_missing_origin_fails_before_output_or_collection(self):
        (self.corpus / "_results" / "ripgrep" / "aaaaaaaaa" / "aft.json").unlink()
        with self.assertRaises(FileNotFoundError):
            self.run_driver()
        self.assertFalse(self.output.exists())

    def test_lowered_cell_returns_failure_without_changing_scorer(self):
        for spec in self.specs:
            original = self.corpus / "_results" / spec["name"] / harness.BASELINE_COMMIT / "oracle.json"
            oracle = json.loads(original.read_text())
            lang = spec["languages"][0]["language"]
            oracle[lang]["buckets"]["dead_code"] = [{"category": "dead_code", "path": "src/a.go" if lang == "go" else "src/a.rs", "symbol": "X", "line": 1}]
            original.write_text(json.dumps(oracle))
        code, _ = self.run_driver(candidate_rows=2)
        self.assertEqual(code, 1)
        run = json.loads((self.output / "run.json").read_text())
        self.assertFalse(run["cell_rule_passed"])
        self.assertTrue(any(cell["metric"] == "precision" and cell["lowered"] for cell in json.loads((self.output / "cell-diff.json").read_text())))

    def test_unknown_bucket_returns_incomplete_not_success(self):
        code, _ = self.run_driver(unknown=True)
        self.assertEqual(code, 2)
        self.assertFalse(json.loads((self.output / "run.json").read_text())["complete"])

    def test_changed_binary_is_not_reported_as_a_success(self):
        with self.assertRaisesRegex(ValueError, "candidate binary changed"):
            self.run_driver(tamper="binary")
        self.assertFalse((self.output / "results.md").exists())

    def test_changed_immutable_input_is_not_reported_as_a_success(self):
        with self.assertRaisesRegex(ValueError, "immutable judgment/oracle input changed"):
            self.run_driver(tamper=True)
        self.assertFalse((self.output / "results.md").exists())

    def test_collector_config_restored_on_exception(self):
        repo = self.corpus / "ripgrep"
        with self.assertRaisesRegex(RuntimeError, "interrupted"):
            with self.driver["preserved_config"](repo):
                (repo / ".cortexkit" / "aft.jsonc").write_bytes(b"temporary\n")
                raise RuntimeError("interrupted")
        self.assertEqual((repo / ".cortexkit" / "aft.jsonc").read_bytes(), b"original config\n")


if __name__ == "__main__":
    unittest.main()
