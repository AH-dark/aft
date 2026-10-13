#!/usr/bin/env python3
"""Add reproducible identifier-bearing prose controls to the pinned AFT corpus."""
from __future__ import annotations

import json
from pathlib import Path

from search_quality_lib import canonical_json

HERE = Path(__file__).resolve().parent


def main() -> None:
    path = HERE / "real-query-manifest.json"
    manifest = json.loads(path.read_text())
    manifest["rows"] = [row for row in manifest["rows"] if "prose_kind" not in row]
    original = {row["episode_id"]: row for row in manifest["rows"]}
    template = original["followup-census:7972"]
    bindings = ("repo", "sha", "embedding_pack", "embedding_pack_sha256", "evidence_tree_sha256")

    def row(number: int, kind: str, query: str, answer: str, basis: str, *, include_tests: bool = False, answer_kind: str = "concept") -> dict:
        return {
            **{key: template[key] for key in bindings},
            "episode_id": f"followup-census:{number}",
            "prose_kind": kind,
            "query": query,
            "opened_file": answer,
            "answer_kind": answer_kind,
            "answer_key_basis": basis,
            "row_source": "constructed from pinned AFT repository content; not telemetry",
            "include_tests": include_tests,
            "include_tests_source": "recorded" if include_tests else "default",
            "pinned_shape": "mixed",
            "mechanism": "wrong_lane_nl",
            "census_stratum": "nl",
        }

    rows = [
        row(
            930001, "comma_free",
            template["query"].replace(",", ""),
            template["opened_file"],
            "Comma-free form of retained followup-census:7972, preserving its recorded wording and answer. At the pin subc_detach_test.rs:129-158 tests the same daemon socket-loss behavior with the real module binary, rather than the earlier in-process thread harness; :191-211 binds TcpListener and :293 accepts the module.",
            include_tests=True,
        ),
        row(
            930002, "product_word",
            "how is structured GitHub resource reading enabled by user configuration",
            "crates/aft/src/config.rs",
            "At the pin config.rs:305-324 defines GhReadConfig as a user-tier-only opt-in for structured GitHub resource reads and defaults enabled to false. GitHub is the query's only identifier-shaped token; its many unrelated verbatim mentions must not substitute for configuration relevance.",
        ),
        row(
            930003, "type_in_context",
            "how does TcpListener detect a bare socket close from the fake daemon and assert a nonzero module exit",
            "crates/aft/tests/integration/subc_detach_test.rs",
            "At the pin subc_detach_test.rs:129-158 accepts the fake daemon connection, drops its stream and asserts exit code 3; :191-211 creates the TcpListener. The listener's other consumers do not answer the connection-loss assertion.",
            include_tests=True,
        ),
        row(
            930004, "definition_lookup",
            "where is the shutdown_all method defined that shuts down every running language server",
            "crates/aft/src/lsp/manager.rs",
            "At the pin lsp/manager.rs:2294-2302 declares shutdown_all and passes all clients to shutdown_all_clients within one shared grace period. This sentence asks for the declaration, not a caller.",
            answer_kind="definition",
        ),
        row(
            930005, "definition_lookup",
            "where is kill_running_tasks_for_root defined to terminate background tasks for a deleted project",
            "crates/aft/src/bash_background/registry.rs",
            "At the pin bash_background/registry.rs:3198-3243 declares kill_running_tasks_for_root, selects active tasks with the canonical project identity and kills them with the root-reclaimed reason. This sentence asks for the declaration, not the reclamation caller.",
            answer_kind="definition",
        ),
    ]
    rows[0]["row_source"] = "comma removal only from retained followup-census:7972; not a new telemetry episode"
    manifest["rows"].extend(rows)
    path.write_bytes(canonical_json(manifest))
    print(f"identifier_prose_rows:{len(rows)}")


if __name__ == "__main__":
    main()
