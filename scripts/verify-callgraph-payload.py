#!/usr/bin/env python3
"""Build base/tip test binaries and compare independent cold corpus outputs.

Run inside an isolated remote job: the base build temporarily overwrites four
working-tree source files, restores them before building tip, and does not change
the Git index or commit history.
The identical test harness runs against both implementations. Each test process
exits, closing its SQLite connections, before the next invocation opens a database.
"""

import json
import hashlib
import os
import pathlib
import shutil
import subprocess
import sys


ROOT = pathlib.Path.cwd()
OUTPUT = ROOT / "target/callgraph-payload-differential"
BASE = sys.argv[1]
FILES = [
    "crates/aft/src/callgraph_store/join.rs",
    "crates/aft/src/callgraph_store/dead_code_projection.rs",
    "crates/aft/src/blob_store/mod.rs",
    "crates/aft/src/views/callgraph.rs",
]
TEST = "callgraph_store::dead_code_projection::payload_parity_tests::payload_whole_corpus_cold_build_preserves_tables_and_answers"


def build(name):
    command = ["cargo", "test", "-p", "agent-file-tools", "--lib", "--no-run",
               "--config", "profile.test.package.agent-file-tools.opt-level=1",
               "--message-format=json"]
    print(f"building {name}: {' '.join(command)}", flush=True)
    with (OUTPUT / f"{name}-build.jsonl").open("w") as log:
        result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
    if result.returncode:
        print((OUTPUT / f"{name}-build.jsonl").read_text())
        result.check_returncode()
    executable = None
    for line in (OUTPUT / f"{name}-build.jsonl").read_text().splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if event.get("reason") == "compiler-artifact" and event.get("executable") and event["target"]["name"] == "aft":
            executable = pathlib.Path(event["executable"])
    assert executable, "cargo emitted no aft lib test executable"
    destination = OUTPUT / f"{name}-tests"
    shutil.copy2(executable, destination)
    print(f"built {name}: {destination}", flush=True)
    return destination


def run(name, executable, corpus, phase):
    env = dict(os.environ, AFT_EXTRACT_CORPUS=str(corpus), AFT_PAYLOAD_VARIANT="slim",
               AFT_PAYLOAD_DUMP=str(OUTPUT / name), AFT_PAYLOAD_PHASE=phase)
    command = [str(executable), TEST, "--exact", "--ignored", "--nocapture", "--test-threads=1"]
    print(f"running {name} {phase}: {' '.join(command)}", flush=True)
    with (OUTPUT / f"{name}-{phase}.log").open("w") as log:
        process = subprocess.Popen(command, env=env, cwd=ROOT / "crates/aft", stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, text=True)
        for line in process.stdout:
            log.write(line)
            log.flush()
            print(line, end="", flush=True)
        code = process.wait()
    assert code == 0, f"{name} corpus test exited {code}"


def main():
    OUTPUT.mkdir(parents=True, exist_ok=True)
    subprocess.run(["rustc", "--version"], check=True)
    corpus = OUTPUT / "corpus"
    corpus.mkdir(exist_ok=True)
    archive = subprocess.Popen(["git", "archive", BASE], stdout=subprocess.PIPE)
    extracted = subprocess.run(["tar", "-x", "-C", str(corpus)], stdin=archive.stdout)
    archive.stdout.close()
    assert archive.wait() == 0 and extracted.returncode == 0, "corpus archive failed"
    originals = {path: (ROOT / path).read_bytes() for path in FILES}
    try:
        for path in FILES:
            contents = subprocess.check_output(["git", "show", f"{BASE}:{path}"])
            if path.endswith("dead_code_projection.rs"):
                # Only the proof harness is added to base; production remains
                # the exact base implementation, including full AST encoding.
                contents += b'\n#[cfg(test)]\n#[path = "payload_parity_tests.rs"]\nmod payload_parity_tests;\n'
            (ROOT / path).write_bytes(contents)
        base_executable = build("base")
    finally:
        for path, contents in originals.items():
            (ROOT / path).write_bytes(contents)
    tip_executable = build("tip")
    subprocess.run([str(tip_executable), TEST.replace("payload_whole_corpus_", "payload_"),
                    "--exact", "--nocapture", "--test-threads=1"], check=True)
    run("base", base_executable, corpus, "graph")
    run("tip", tip_executable, corpus, "graph")
    counts = {}
    for artifact in ("tables.json", "dead-code.txt"):
        base = (OUTPUT / "base" / artifact).read_bytes()
        tip = (OUTPUT / "tip" / artifact).read_bytes()
        assert base == tip, f"cold corpus outputs differ: {artifact}"
        counts[artifact] = len(base)
    evidence = {}
    for side in ("base", "tip"):
        tables = json.loads((OUTPUT / side / "tables.json").read_bytes())
        evidence[side] = {name: {"rows": len(rows), "sha256": hashlib.sha256(
            json.dumps(rows, ensure_ascii=False, separators=(",", ":")).encode()).hexdigest()}
            for name, rows in tables.items()}
    result = {"base": BASE, "equal_bytes": counts, "table_sets": len(tables),
              "rows": sum(map(len, tables.values())), "tables": evidence}
    (OUTPUT / "graph-result.json").write_text(json.dumps(result, indent=2) + "\n")
    print("PAYLOAD CORPUS TABLES AND DEAD-CODE PASSED " + json.dumps(result), flush=True)
    run("base", base_executable, corpus, "queries")
    run("tip", tip_executable, corpus, "queries")
    for artifact in ("queries.txt", "sample.json"):
        base = (OUTPUT / "base" / artifact).read_bytes()
        tip = (OUTPUT / "tip" / artifact).read_bytes()
        assert base == tip, f"cold corpus outputs differ: {artifact}"
        counts[artifact] = len(base)
    sample = json.loads((OUTPUT / "tip/sample.json").read_bytes())
    sample.pop("selected")
    result.update(query_sample=sample, query_depths=[1, 1, 1])
    (OUTPUT / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print("PAYLOAD CORPUS DIFFERENTIAL PASSED " + json.dumps(result), flush=True)


if __name__ == "__main__":
    main()
