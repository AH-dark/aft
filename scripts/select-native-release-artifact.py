#!/usr/bin/env python3
"""Select exact-SHA tested bytes; the publishing gate separately checks main ancestry and required CI checks."""

from __future__ import annotations

import argparse
import json
import os
import subprocess


def gh(*args: str):
    return json.loads(subprocess.check_output(["gh", *args], text=True))


def choose_run(runs: list[dict], sha: str) -> list[int]:
    return sorted((run["databaseId"] for run in runs
                   if run["headSha"] == sha and run["conclusion"] == "success"
                   and (run["headBranch"] == "main" or run["headBranch"].startswith("train/"))), reverse=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("platform", choices=("macos", "windows"))
    args = parser.parse_args()
    repo = os.environ["GITHUB_REPOSITORY"]
    sha = os.environ["GITHUB_SHA"]
    name = f"{args.platform}-shipped-release-{sha}"
    runs = gh("run", "list", "--repo", repo, "--workflow", "tests.yml", "--commit", sha,
              "--event", "push", "--status", "completed", "--limit", "100", "--json",
              "databaseId,headSha,headBranch,conclusion")
    selected = None
    for run in choose_run(runs, sha):
        artifacts = gh("api", f"repos/{repo}/actions/runs/{run}/artifacts?name={name}&per_page=100")
        matches = [entry for entry in artifacts["artifacts"] if entry["name"] == name and not entry["expired"]]
        if matches:
            selected = run
            break
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a") as output:
            output.write(f"found={'true' if selected else 'false'}\n")
            output.write(f"run_id={selected or ''}\n")
    print(f"Exact shipped-profile {args.platform} artifact: {'run ' + str(selected) if selected else 'absent; source-build fallback required'}")


if __name__ == "__main__":
    main()
