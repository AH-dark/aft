#!/usr/bin/env python3
"""Collect seeded diagnostics and the real OpenCode footer, without a baseline run."""

from __future__ import annotations

import argparse
from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import subprocess
import sys

import run as truth


@contextmanager
def same_mode_restore(enabled: bool):
    """Avoid only an unnecessary chmod on the read-only remote bind mount."""
    original = Path.chmod
    config = truth.CHECKOUT_ROOT / ".cortexkit" / "aft.jsonc"
    skipped = []

    def chmod(self: Path, mode: int, *, follow_symlinks: bool = True) -> None:
        if self == config and self.stat().st_mode == mode:
            skipped.append(str(self))
            return
        if follow_symlinks:
            return original(self, mode)
        return original(self, mode, follow_symlinks=False)

    if enabled:
        Path.chmod = chmod
    try:
        yield skipped
    finally:
        Path.chmod = original


def render_footer(response: dict) -> str:
    # Use the production parser's handling of missing diagnostic tools before
    # formatting; passing raw numeric counts would bypass that handling.
    script = """
import { coerceAftStatus } from './packages/opencode-plugin/src/shared/status.ts';
import { formatAftStatusSegment } from './packages/opencode-plugin/src/tui/v2-status.ts';
const status = JSON.parse(await Bun.stdin.text());
console.log(formatAftStatusSegment(coerceAftStatus(status)));
"""
    result = subprocess.run(["bun", "--eval", script], cwd=truth.CHECKOUT_ROOT,
                            input=json.dumps(response), text=True, capture_output=True, check=True)
    return result.stdout.strip()


def oracle_records(root: Path, lang: dict, out: Path) -> dict:
    """Reuse the reference commands and output normalization in run.py."""
    repo = truth.repo_path(root, lang)
    runs = []
    failures = []
    for prefix in lang.get("fallow_roots", [""]):
        directory = out / (prefix or "project")
        directory.mkdir(parents=True)
        try:
            invocation = {**lang, "oracle_cwd": prefix}
            if prefix:
                invocation["tsc_projects"] = [str(Path(project).relative_to(prefix))
                                              for project in lang.get("tsc_projects", [])
                                              if Path(project).is_relative_to(prefix)]
            truth.run_language_oracles(root, invocation, True, directory)
            if lang["language"] == "go":
                meta = json.loads((directory / "oracle-meta.json").read_text())
                if meta.get("go_vet", {}).get("exit") == 0 and not (directory / "go-vet.txt").read_text().strip():
                    # An empty successful vet report needs no parsing. Avoid
                    # coupling it to staticcheck's unrelated unused-code output.
                    normalized = {"buckets": {"diagnostics": []}}
                else:
                    normalized = truth.normalize_oracles(repo / prefix, invocation, [("", directory)])
            else:
                normalized = truth.normalize_oracles(repo / prefix, invocation, [("", directory)])
            meta = json.loads((directory / "oracle-meta.json").read_text())
            producer = {"typescript": "tsc", "rust": "cargo_check",
                        "go": "go_vet", "python": "pyright"}[lang["language"]]
            reports = meta.get(producer)
            reports = reports if isinstance(reports, list) else [reports]
            # tsc and pyright exit 1 when they found diagnostics, not only when
            # they failed. Their reports are retained for inspection.
            if not reports or any(report is None for report in reports):
                raise RuntimeError(f"{producer} did not return a report")
            if any(report["exit"] not in (0, 1, 2) for report in reports):
                raise RuntimeError(f"{producer} failed: {reports}")
            if lang["language"] == "python":
                document = json.loads((directory / "pyright.json").read_text())
                if "summary" not in document:
                    raise RuntimeError("pyright did not return a completed analysis summary")
            diagnostics = normalized["buckets"]["diagnostics"]
            if any(report["exit"] for report in reports) and not diagnostics:
                raise RuntimeError(f"{producer} exited nonzero without file diagnostics: {reports}")
            for item in diagnostics:
                item["path"] = str(Path(prefix) / item["path"])
            runs.extend(diagnostics)
        except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
            failures.append({"root": prefix or ".", "reason": str(error)})
    return {"status": "unavailable" if failures else "collected", "failures": failures, "items": runs}


def compare(name: str, lang: str, repo: Path, diagnostics: dict, oracle: dict) -> dict:
    comparison = truth.score_diagnostics(name, lang, repo, diagnostics, oracle["items"])
    gaps = [gap for page in diagnostics["pages"]
            for gap in (page.get("summary") or {}).get("gaps", [])]
    incomplete = any(not (page.get("summary") or {}).get("complete", True)
                     or not page.get("success") for page in diagnostics["pages"])
    # Keep observed match counts, but never call a missing producer's empty list
    # an authoritative zero. This wrapper leaves run.py's scorer unchanged and
    # marks an incomplete comparison unknown.
    comparison["observed_aft_count"] = comparison["aft_count"]
    comparison["producer_gaps"] = gaps
    comparison["oracle_status"] = oracle["status"]
    comparison["oracle_failures"] = oracle["failures"]
    if incomplete or not diagnostics["pages"] or oracle["status"] != "collected":
        comparison.update(status="unknown", aft_count=None, precision=None, recall=None)
    return comparison


def collect(root: Path, spec: dict, binary: Path, out: Path, size: int) -> dict:
    repo = truth.repo_path(root, spec)
    pin = truth.corpus_pin(root, spec)
    record = {"repo": spec["name"], "pin": pin, "languages": {}}
    out.mkdir(parents=True)
    truth.install(root, spec)
    # Cargo reference checks finish before AFT can start rust-analyzer.
    oracles = {lang["language"]: oracle_records(root, lang, out / "oracles" / lang["language"])
               for lang in truth.language_specs(spec)}
    config = repo / ".cortexkit" / "aft.jsonc"
    original = config.read_bytes() if config.exists() else None
    config.parent.mkdir(exist_ok=True)
    try:
        with truth.local_hygiene(root, spec) as hygiene:
            config.write_text(json.dumps(truth.AFT_PROJECT_CONFIG) + "\n")
            # An explicit empty user config enables the configured producers;
            # a developer's disabled-server list must not leak into this pass.
            user_config = out / "enabled-user-config.jsonc"
            truth.write_json(user_config, {})
            session = truth.AftSession(binary, repo, out / "aft-storage")
            try:
                configured = session.configure(user_config)
                if not configured.get("success") or configured.get("config_dropped_keys"):
                    raise RuntimeError(f"configure failed: {configured}")
                files = truth.tracked_files(repo)
                record["status_before"] = session.client.request("status", timeout_s=600)
                for lang in truth.language_specs(spec):
                    try:
                        diagnostics = truth.collect_aft_diagnostics(session, lang, files, size)
                    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
                        diagnostics = {"sample": truth.diagnostics_sample_files(lang, files, size),
                                       "items": [], "pages": [{"success": False, "summary": {
                                           "complete": False, "gaps": [{"kind": "collection_failed",
                                                                       "reason": str(error)}]}}]}
                    record["languages"][lang["language"]] = {
                        "diagnostics": diagnostics, "oracle": oracles[lang["language"]],
                        "comparison": compare(spec["name"], lang["language"], repo, diagnostics,
                                              oracles[lang["language"]])}
                try:
                    record["project_diagnostics"] = session.inspect(["diagnostics"], timeout_s=900)
                    record["status_after"] = session.client.request("status", timeout_s=600)
                    record["footer_after"] = render_footer(record["status_after"])
                except (OSError, RuntimeError, subprocess.SubprocessError) as error:
                    record["footer_failure"] = str(error)
                record["footer_before"] = render_footer(record["status_before"])
            finally:
                session.close()
        record["hygiene"] = hygiene
    finally:
        if original is None:
            config.unlink(missing_ok=True)
        else:
            config.write_bytes(original)
    truth.write_json(out / "diagnostics.json", record)
    return record


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--aft-bin", type=Path, required=True)
    parser.add_argument("--corpus-root", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--repo", action="append")
    parser.add_argument("--sample", type=int, default=60)
    parser.add_argument("--same-mode-config-restore", action="store_true")
    args = parser.parse_args()
    _, corpus = truth.load_corpus()
    selected = [spec for spec in corpus if not args.repo or spec["name"] in args.repo]
    if not selected or (args.repo and set(args.repo) - {spec["name"] for spec in corpus}):
        parser.error("unknown corpus repo")
    if args.sample <= 0:
        parser.error("--sample must be positive")
    out = args.output_dir.resolve()
    out.mkdir(parents=True, exist_ok=False)
    binary = args.aft_bin.resolve()
    result = {"aft_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "aft_version": truth.run([str(binary), "--version"], truth.CHECKOUT_ROOT)[1].strip(),
              "source_commit": truth.git_output(truth.CHECKOUT_ROOT, "rev-parse", "HEAD").strip(),
              "sample_size": args.sample, "repositories": []}
    with same_mode_restore(args.same_mode_config_restore) as skipped:
        for spec in selected:
            truth.log(f"{spec['name']}: diagnostics/footer collection")
            try:
                record = collect(args.corpus_root.resolve(), spec, binary, out / spec["name"], args.sample)
            except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
                record = {"repo": spec["name"], "collection_failure": str(error)}
                truth.write_json(out / spec["name"] / "failure.json", record)
            result["repositories"].append(record)
            truth.write_json(out / "results.json", result)
        result["same_mode_chmod_skips"] = skipped
    truth.write_json(out / "results.json", result)
    failed = sum(capture_incomplete(record) for record in result["repositories"])
    print(f"Collected {len(selected)} repositories; {failed} with incomplete diagnostics/footer coverage")
    return 1 if failed else 0


def capture_incomplete(record: dict) -> bool:
    """Whether a repository's capture fails the gate.

    A missing flag counts as incomplete: a timed-out project request can come
    back as success:false with no summary at all.
    """
    if "collection_failure" in record or "footer_failure" in record:
        return True
    project = record.get("project_diagnostics")
    if not isinstance(project, dict) or project.get("success") is not True:
        return True
    if project.get("complete") is False:
        return True
    # The server writes summary.diagnostics.complete only as false; a complete
    # summary omits the flag. A missing summary still fails the gate.
    summary = (project.get("summary") or {}).get("diagnostics")
    if not isinstance(summary, dict) or summary.get("complete") is False:
        return True
    return any(value["comparison"]["status"] == "unknown"
               for value in record.get("languages", {}).values())


if __name__ == "__main__":
    sys.exit(main())
