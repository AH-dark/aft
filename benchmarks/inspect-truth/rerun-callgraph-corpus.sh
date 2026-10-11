#!/usr/bin/env bash
# Fresh binary collections against frozen per-pin oracles; no fetch or install.
set -euo pipefail
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
exec "${PYTHON:-python3}" - "$SCRIPT_DIR" "$@" <<'PY'
import hashlib
import importlib.util
import json
import os
import re
import shutil
import subprocess
import sys
import uuid
from contextlib import contextmanager
from pathlib import Path

REPOS = ("ripgrep", "axum", "soft-serve")
METRICS = ("status", "aft_count", "oracle_count", "agreed", "agreed_file_level",
           "aft_only", "oracle_only", "precision", "recall", "recall_in_aft_domain",
           "recall_exported", "oracle_rows_judged_out", "aft_rows_judged", "aft_rows_unjudged")


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def load_harness(directory):
    spec = importlib.util.spec_from_file_location("callgraph_corpus_harness", directory / "run.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def binary_info(path, baseline=False):
    if not path.is_file() or not os.access(path, os.X_OK):
        raise ValueError(f"not an executable binary: {path}")
    result = subprocess.run([str(path), "--version"], capture_output=True, text=True, timeout=30)
    version = (result.stdout + result.stderr).strip()
    if result.returncode or not re.search(r"\baft\s+\d", version):
        raise ValueError(f"{path}: aft --version failed: {version}")
    if baseline and not re.search(r"\b0\.58\.2\b", version):
        raise ValueError("BASE_BIN must be the campaign's 0.58.2 binary built at 13dd9cb71")
    return {"path": str(path), "version": version, "sha256": sha256(path)}


def read_record(path, pin):
    record = json.loads((path / "aft.json").read_text())
    if record.get("commit") != pin:
        raise ValueError(f"{path}: capture belongs to another corpus pin")
    return record


def inputs(h, corpus, specs, judgments):
    """Validate copied snapshots and relocate descriptors, never their entries."""
    originals, oracles, documents, hashes = {}, {}, {}, {}
    for spec in specs:
        name, pin = spec["name"], spec["commit"]
        repo = corpus / name
        if h.git_output(repo, "rev-parse", "HEAD").strip() != pin:
            raise ValueError(f"{repo}: expected pinned checkout {pin}")
        if h.git_output(repo, "status", "--porcelain", "--untracked-files=no").strip():
            raise ValueError(f"{repo}: tracked source changes; copy a clean pinned snapshot")
        original = corpus / "_results" / name / h.BASELINE_COMMIT
        read_record(original, pin)
        oracle = json.loads((original / "oracle.json").read_text())
        for language in spec["languages"]:
            if language["language"] not in oracle:
                raise ValueError(f"{original}: missing frozen {language['language']} oracle")
        originals[name], oracles[name] = original, oracle
    by_name = {spec["name"]: spec for spec in specs}
    for path in sorted(judgments.glob("*.json")):
        document = json.loads(path.read_text())
        for name in sorted({entry["repo"] for entry in document["entries"]} & set(by_name)):
            declared = Path(document["after_outputs"][name])
            if (len(declared.parts) < 3 or declared.parts[-3:-1] != ("_results", name)
                    or not re.fullmatch(r"[0-9a-f]{9,40}", declared.name)):
                raise ValueError(f"{path}: unsupported judgment-origin identity {declared}")
            relocated = corpus / "_results" / name / declared.name
            read_record(relocated, by_name[name]["commit"])
            if not (relocated / "oracle.json").is_file():
                raise ValueError(f"{relocated}: missing judgment-origin oracle.json")
            document["after_outputs"][name] = str(relocated)
        documents[path.name] = document
    for directory in set(originals.values()) | {
            Path(doc["after_outputs"][name]) for doc in documents.values()
            for name in {entry["repo"] for entry in doc["entries"]} & set(by_name)}:
        for filename in ("aft.json", "oracle.json"):
            source = directory / filename
            hashes[str(source)] = sha256(source)
    return originals, oracles, documents, hashes


@contextmanager
def preserved_config(repo):
    """The collector writes a project config; restore the copied source afterward."""
    directory = repo / ".cortexkit"
    path = directory / "aft.jsonc"
    existed = directory.exists()
    if path.is_symlink():
        raise ValueError(f"refusing to overwrite symlinked config: {path}")
    previous = path.read_bytes() if path.exists() else None
    mode = path.stat().st_mode if previous is not None else None
    try:
        yield
    finally:
        if previous is None:
            path.unlink(missing_ok=True)
        else:
            path.write_bytes(previous)
            path.chmod(mode)
        if not existed and directory.exists() and not any(directory.iterdir()):
            directory.rmdir()


def cell_diff(before, after, h):
    def indexed(results):
        return {(result["repo"], bucket["language"], bucket["category"]): bucket
                for result in results for bucket in result["buckets"]}
    old, new = indexed(before), indexed(after)
    rows = []
    for key in sorted(old.keys() | new.keys()):
        lowered = h.cell_changes(old[key], new[key]) if key in old and key in new else []
        for metric in METRICS:
            left, right = old.get(key, {}).get(metric), new.get(key, {}).get(metric)
            numeric = all(isinstance(value, (int, float)) and not isinstance(value, bool)
                          for value in (left, right))
            rows.append({"repo": key[0], "language": key[1], "category": key[2],
                         "metric": metric, "before": left, "after": right,
                         "delta": right - left if numeric else None,
                         "lowered": metric in lowered})
    return rows

def rerun(h, corpus, specs, base, candidate, output, judgments, candidate_revision=None):
    originals, oracles, documents, hashes = inputs(h, corpus, specs, judgments)
    metadata = {"baseline": binary_info(base, baseline=True), "candidate": binary_info(candidate),
                "corpus": {spec["name"]: spec["commit"] for spec in specs},
                "python": sys.version, "frozen_inputs_sha256": dict(sorted(hashes.items())), "oracle_mode": "copied per-pin snapshots"}
    for repo in (corpus / spec["name"] for spec in specs):
        if output == repo or repo in output.parents:
            raise ValueError(f"output directory must not be inside a source snapshot: {repo}")
    if output == corpus / "_results" or corpus / "_results" in output.parents:
        raise ValueError("output directory must not overwrite imported judgment origins")
    output.mkdir(parents=True, exist_ok=False)
    relocated = output / "judgments"
    relocated.mkdir()
    for name, document in documents.items():
        h.write_json(relocated / name, document)
    # Validate every original entry before starting expensive fresh collections.
    for spec in specs:
        h.load_judgments(spec, originals[spec["name"]], relocated)
    before, after = [], []
    identity = candidate_revision or "sha256:" + metadata["candidate"]["sha256"]
    for spec in specs:
        repo = corpus / spec["name"]
        for label, binary, commit, results in (("before", base, h.BASELINE_COMMIT, before),
                                               ("after", candidate, identity, after)):
            directory = output / "raw" / spec["name"] / label
            directory.mkdir(parents=True)
            with preserved_config(repo):
                record = h.collect_aft_record(repo, spec, binary, directory, 0, spec["commit"])
            record["aft_commit"] = commit
            h.write_json(directory / "aft.json", record)
            h.write_json(directory / "oracle.json", oracles[spec["name"]])
            results.append(h.score_repo(corpus, spec, out=directory,
                                        baseline_out=originals[spec["name"]], judgments_dir=relocated))
    for label, binary in (("baseline", base), ("candidate", candidate)):
        if sha256(binary) != metadata[label]["sha256"]:
            raise ValueError(f"{label} binary changed during collection: {binary}")
    for path, digest in hashes.items():
        if sha256(Path(path)) != digest:
            raise ValueError(f"immutable judgment/oracle input changed during collection: {path}")
    h.write_json(output / "results.json", {"corpus": specs, "before": before, "after": after})
    table, failures = h.render_slice(before, after, "callgraph-offline")
    unknown = [f"{result['repo']}/{bucket['language']}/{bucket['category']}"
               for result in before + after for bucket in result["buckets"] if bucket["status"] != "scored"]
    if unknown:
        table += "\n## Collection incomplete\n\nThese buckets are not accuracy measurements: " + ", ".join(unknown) + "\n"
    (output / "results.md").write_text(table)
    cells = cell_diff(before, after, h)
    h.write_json(output / "cell-diff.json", cells)
    diff = ["# Per-cell before/after diff", "", "| repo | language | category | cell | before | after | delta | lowered by existing rule |",
            "|---|---|---|---|---:|---:|---:|---|"]
    for row in cells:
        values = [row[key] for key in ("repo", "language", "category", "metric", "before", "after", "delta")]
        diff.append("| " + " | ".join("n/a" if value is None else str(value) for value in values)
                    + " | " + ("yes" if row["lowered"] else "no") + " |")
    (output / "cell-diff.md").write_text("\n".join(diff) + "\n")
    metadata.update(candidate_revision=candidate_revision, failures=failures, unknown_buckets=unknown,
                    cell_rule_passed=not failures, complete=not unknown)
    h.write_json(output / "run.json", metadata)
    print(table)
    print(f"Artifacts: {output}\nPer-cell diff: {output / 'cell-diff.md'}")
    return 2 if unknown else 1 if failures else 0


def main(directory):
    if sys.argv[2:] == ["--help"]:
        print("BASE_BIN=/path/to/aft-0.58.2 CAND_BIN=/path/to/aft CORPUS_DIR=/copied/corpus bash rerun-callgraph-corpus.sh\n"
              "Optional: OUT_DIR (new directory), CAND_REV (binary's known Git revision), PYTHON.\n"
              "Uses frozen oracles, no fetch/install/build/network. Exit: 0 pass, 1 lowered cell, 2 incomplete/invalid input.")
        return 0
    if sys.argv[2:]:
        raise ValueError("unexpected arguments; use --help or environment inputs")
    if not sys.platform.startswith("linux"):
        raise ValueError("run this collector on Linux with Linux release binaries")
    paths = {}
    for name in ("BASE_BIN", "CAND_BIN", "CORPUS_DIR"):
        if not os.environ.get(name):
            raise ValueError(f"missing {name}; use --help")
        paths[name] = Path(os.environ[name]).expanduser().resolve()
    for tool in ("git", "rg"):
        if not shutil.which(tool):
            raise ValueError(f"missing {tool} on PATH")
    revision = os.environ.get("CAND_REV")
    if revision and not re.fullmatch(r"[0-9a-f]{9,40}", revision):
        raise ValueError("CAND_REV must be the known source Git revision, not a label")
    # These runs use the collector, not fetch/collect CLI phases that can install.
    os.environ.update(CARGO_NET_OFFLINE="true", GOPROXY="off", GOSUMDB="off", GOTOOLCHAIN="local", HF_HUB_OFFLINE="1")
    h = load_harness(directory)
    _, corpus_specs = h.load_corpus()
    selected = {spec["name"]: spec for spec in corpus_specs}
    specs = [selected[name] for name in REPOS]
    corpus = paths["CORPUS_DIR"]
    if os.environ.get("OUT_DIR"):
        output = Path(os.environ["OUT_DIR"]).expanduser().resolve()
    else:
        parent = corpus / "_reruns"
        parent.mkdir(exist_ok=True)
        output = parent / ("callgraph-" + uuid.uuid4().hex[:12])
    return rerun(h, corpus, specs, paths["BASE_BIN"], paths["CAND_BIN"], output,
                 directory / "judgments", revision)


if __name__ == "__main__":
    try:
        sys.exit(main(Path(sys.argv[1])))
    except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"offline callgraph rerun failed: {error}", file=sys.stderr)
        sys.exit(2)
PY
