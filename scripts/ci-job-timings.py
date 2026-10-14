#!/usr/bin/env python3
"""Collect reproducible Actions job/step timings without conflating queue and execution."""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime
import json
from pathlib import Path
import subprocess


def seconds(start: str | None, end: str | None) -> float | None:
    if not start or not end or start.startswith("0001") or end.startswith("0001"):
        return None
    return max(0.0, (datetime.fromisoformat(end.replace("Z", "+00:00"))
                     - datetime.fromisoformat(start.replace("Z", "+00:00"))).total_seconds())


def step_category(name: str) -> str:
    lower = name.lower()
    # These shell gates compile and execute in one step. Step timestamps alone
    # cannot separate their compilation from test execution.
    if any(label in lower for label in ("rust test gate", "rust conformance gate",
                                       "rust watcher gate", "rust watcher + conformance gate",
                                       "release storm tests", "evaluate committed search-quality reference",
                                       "capture watcher inventory")):
        return "compile_and_test"
    if lower.startswith("post "):
        return "cleanup"
    if lower.startswith("run ") and "@" in lower:
        return "setup_and_other"
    if "archive" in lower and lower.startswith("build"):
        return "compile"
    if lower.startswith(("cargo build", "cargo check", "build aft", "build shared", "build nextest")):
        return "compile"
    if lower.startswith(("run ", "rust libtest shard", "rust integration shard", "bun test")):
        return "test"
    return "setup_and_other"


def job_timings(job: dict) -> dict:
    buckets = {name: 0.0 for name in ("compile", "test", "compile_and_test", "cleanup", "setup_and_other")}
    for step in job["steps"]:
        duration = seconds(step.get("startedAt"), step.get("completedAt"))
        if duration is not None and step.get("conclusion") != "skipped":
            buckets[step_category(step["name"])] += duration
    execution = seconds(job.get("startedAt"), job.get("completedAt"))
    return {
        "name": job["name"], "conclusion": job["conclusion"],
        "queue_seconds": seconds(job.get("createdAt"), job.get("startedAt")),
        "execution_seconds": execution, **buckets,
        "unattributed_seconds": None if execution is None else max(0.0, execution - sum(buckets.values())),
    }


def gh_json(*args: str):
    return json.loads(subprocess.check_output(["gh", *args], text=True))


def collect(repo: str, run_id: int, attempt: int | None = None) -> dict:
    args = ["run", "view", str(run_id), "--repo", repo, "--json",
            "attempt,conclusion,createdAt,databaseId,headBranch,headSha,startedAt,status,updatedAt,jobs"]
    if attempt is not None:
        args.extend(["--attempt", str(attempt)])
    run = gh_json(*args)
    attempt = attempt or run["attempt"]
    endpoint = f"repos/{repo}/actions/runs/{run_id}/attempts/{attempt}"
    metadata = gh_json("api", endpoint)
    if metadata["status"] != "completed":
        raise ValueError(f"run {run_id} attempt {attempt} is not completed")
    page = gh_json("api", f"{endpoint}/jobs?per_page=100")
    raw_jobs = list(page["jobs"])
    page_number = 2
    while len(raw_jobs) < page["total_count"]:
        raw_jobs.extend(gh_json("api", f"{endpoint}/jobs?per_page=100&page={page_number}")["jobs"])
        page_number += 1
    by_id = {job["id"]: job for job in raw_jobs}
    for job in run["jobs"]:
        job["createdAt"] = by_id[job["databaseId"]]["created_at"]
    # Reruns retain the original created_at. Use the attempt's run_started_at,
    # not the first attempt's creation date, when comparing cold and warm runs.
    run["attempt"] = attempt
    run["conclusion"] = metadata["conclusion"]
    run["status"] = metadata["status"]
    run["attemptStartedAt"] = metadata["run_started_at"]
    run["attemptCompletedAt"] = max(job["completed_at"] for job in raw_jobs if job["completed_at"])
    run["wall_seconds"] = seconds(run["attemptStartedAt"], run["attemptCompletedAt"])
    run["timings"] = [job_timings(job) for job in run["jobs"]]
    return run


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default="cortexkit/aft")
    parser.add_argument("--run", type=int, help="Collect one completed run instead of recent main/train runs")
    parser.add_argument("--attempt", type=int, help="Attempt of --run (e.g. 2 for a warm rerun)")
    parser.add_argument("--recent", type=int, default=15)
    parser.add_argument("--output", type=Path, required=True, help="JSON evidence file outside build/cache directories")
    args = parser.parse_args()
    if args.attempt and not args.run:
        parser.error("--attempt requires --run")
    if args.recent < 1:
        parser.error("--recent must be positive")
    if args.run:
        runs = [collect(args.repo, args.run, args.attempt)]
    else:
        candidates = gh_json("run", "list", "--repo", args.repo, "--workflow", "tests.yml",
                             "--status", "completed", "--limit", "100", "--json", "databaseId,headBranch")
        ids = [run["databaseId"] for run in candidates
               if run["headBranch"] == "main" or run["headBranch"].startswith("train/")][:args.recent]
        if len(ids) < args.recent:
            raise ValueError(f"only {len(ids)} main/train runs found; requested {args.recent}")
        with ThreadPoolExecutor(max_workers=4) as pool:
            runs = list(pool.map(lambda run_id: collect(args.repo, run_id), ids))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(runs, indent=2) + "\n")
    print(f"Collected {len(runs)} completed runs, {sum(len(run['jobs']) for run in runs)} jobs: {args.output}")
    for run in runs:
        print(f"{run['databaseId']} attempt {run['attempt']}: {run['wall_seconds'] / 60:.2f} min, {run['conclusion']}")


if __name__ == "__main__":
    main()
