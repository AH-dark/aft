#!/usr/bin/env python3
"""Resolve queue members from merge parents and retain per-PR search contracts.

The prototype supports the queue's merge method only. Unknown topology or PR
association is an input fault, not permission to guess which descriptor applies.
"""

from __future__ import annotations

import argparse
from importlib import import_module
import json
from pathlib import Path
import re
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "benchmarks/aft-search"))
quality = import_module("search_quality")
quality_lib = import_module("search_quality_lib")
descriptor_labels = quality.descriptor_labels
InputFault = quality_lib.InputFault
resolve_descriptor = quality_lib.resolve_descriptor


def git(*args: str) -> str:
    result = subprocess.run(["git", *args], text=True, capture_output=True, check=False)
    if result.returncode:
        raise InputFault(f"unresolvable_queue_diff:{result.stderr.strip()}")
    return result.stdout.strip()


def paths(base: str, head: str) -> list[str]:
    return git("diff", "--name-only", "--no-renames", f"{base}...{head}").splitlines()


def resolve_git_descriptor(descriptor: dict, changed: list[str], base: str, head: str) -> tuple[dict, bool]:
    notes: list[str] = []
    try:
        return resolve_descriptor(descriptor, changed, schema_blobs=quality_lib.read_tool_schema_blobs(base, head, changed, root=Path.cwd()), notes=notes)
    finally:
        for note in notes:
            print(note, file=sys.stderr)


PR_LOOKUP_BACKOFF_SECONDS = (5, 10, 15, 30)


def lookup_member_prs(repository: str, head: str) -> list[dict]:
    result = subprocess.run(
        ["gh", "api", "--paginate", "--slurp",
         f"repos/{repository}/commits/{head}/pulls?per_page=100"],
        text=True, capture_output=True, check=False,
    )
    if result.returncode:
        raise InputFault(f"queue_pr_lookup_failed:{head}:{result.stderr.strip()}")
    pages = json.loads(result.stdout)
    return [pr for page in pages for pr in page]


def lookup_queue_ref_pr(repository: str, number: int) -> dict:
    result = subprocess.run(
        ["gh", "api", f"repos/{repository}/pulls/{number}"],
        text=True, capture_output=True, check=False,
    )
    if result.returncode:
        raise InputFault(f"queue_pr_lookup_failed:{number}:{result.stderr.strip()}")
    return json.loads(result.stdout)


def is_member_pr(pr: dict, repository: str, head: str) -> bool:
    return (
        pr["head"]["sha"] == head
        and pr["base"]["ref"] == "main"
        and pr["base"]["repo"]["full_name"] == repository
    )


def member_pr(
    repository: str,
    head: str,
    *,
    queue_ref_pr_number: int | None = None,
    lookup=lookup_member_prs,
    queue_ref_lookup=lookup_queue_ref_pr,
    sleep=time.sleep,
) -> dict:
    for attempt in range(1, len(PR_LOOKUP_BACKOFF_SECONDS) + 2):
        matches = [
            pr for pr in lookup(repository, head)
            if is_member_pr(pr, repository, head)
        ]
        if len(matches) == 1:
            return matches[0]
        if len(matches) > 1:
            raise InputFault(
                f"ambiguous_queue_member:{head}:matches={len(matches)}:attempts={attempt}"
            )
        if attempt <= len(PR_LOOKUP_BACKOFF_SECONDS):
            sleep(PR_LOOKUP_BACKOFF_SECONDS[attempt - 1])

    if queue_ref_pr_number is not None:
        pr = queue_ref_lookup(repository, queue_ref_pr_number)
        # The queue ref names only its final PR; its API record is safe to use
        # only when the PR still points at this merge's exact second-parent SHA.
        if (
            pr is not None
            and pr.get("number") == queue_ref_pr_number
            and is_member_pr(pr, repository, head)
        ):
            return pr

    raise InputFault(
        f"ambiguous_queue_member:{head}:matches=0:attempts={len(PR_LOOKUP_BACKOFF_SECONDS) + 1}"
    )


def member_descriptor(head: str, branch: str, group_head: str) -> tuple[str, dict]:
    git("check-ref-format", "--branch", branch)
    candidates = []
    for label in descriptor_labels(branch):
        path = f"benchmarks/aft-search/slice-descriptors/{label}.json"
        exists = subprocess.run(
            ["git", "cat-file", "-e", f"{head}:{path}"],
            capture_output=True, check=False,
        )
        if exists.returncode == 0:
            candidates.append(path)
    if len(candidates) != 1:
        raise InputFault(f"queue_member_descriptor:{branch}:matches={len(candidates)}")
    path = candidates[0]
    descriptor = json.loads(git("show", f"{head}:{path}"))
    if not isinstance(descriptor, dict):
        raise InputFault(f"malformed_queue_descriptor:{path}")
    # A later PR must not replace an earlier PR's descriptor at the same path.
    if descriptor != json.loads(git("show", f"{group_head}:{path}")):
        raise InputFault(f"queue_descriptor_overwritten:{path}")
    return path, descriptor


def queue_members(base: str, head: str, head_ref: str | None, repository: str) -> list[dict]:
    for sha in (base, head):
        if re.fullmatch(r"[0-9a-f]{40}", sha) is None:
            raise InputFault("queue_requires_full_base_and_head_shas")
    if git("rev-parse", "HEAD") != head:
        raise InputFault("queue_checkout_head_mismatch")
    if git("merge-base", base, head) != base:
        raise InputFault("queue_base_not_ancestor")
    tail = None
    if head_ref is not None:
        tail = re.fullmatch(r"(?:refs/heads/)?gh-readonly-queue/main/pr-(\d+)-.+", head_ref)
        if tail is None:
            raise InputFault(f"unknown_queue_ref:{head_ref}")
    members = []
    previous = base
    commits = git("rev-list", "--first-parent", "--reverse", f"{base}..{head}").splitlines()
    for index, commit in enumerate(commits):
        parents = git("show", "-s", "--format=%P", commit).split()
        # Only two-parent merges bind the synthetic commit to an exact PR head.
        if len(parents) != 2 or parents[0] != previous:
            raise InputFault(f"unsupported_queue_topology:{commit}")
        queue_ref_pr_number = int(tail[1]) if tail is not None and index == len(commits) - 1 else None
        pr = member_pr(repository, parents[1], queue_ref_pr_number=queue_ref_pr_number)
        if any(member["number"] == pr["number"] for member in members):
            raise InputFault(f"duplicate_queue_member:{pr['number']}")
        members.append({"number": pr["number"], "branch": pr["head"]["ref"], "head": parents[1]})
        previous = commit
    if not members or (tail is not None and members[-1]["number"] != int(tail[1])):
        raise InputFault("queue_ref_member_mismatch")
    return members


def plan(base: str, head: str, head_ref: str, repository: str) -> dict:
    members = queue_members(base, head, head_ref, repository)
    descriptors = {}
    for member in members:
        path, descriptor = member_descriptor(member["head"], member["branch"], head)
        member_base = git("merge-base", base, member["head"])
        resolved, missing = resolve_git_descriptor(descriptor, paths(member_base, member["head"]), member_base, member["head"])
        if missing:
            raise InputFault(f"missing_queue_descriptor:{member['number']}")
        member.update(descriptor=path, slice_class=resolved["slice_class"])
        descriptors[path] = descriptor
    ranking = [member for member in members if member["slice_class"] == "ranking"]
    if len(ranking) > 1:
        raise InputFault("multiple_ranking_queue_members:land these ranking PRs separately")
    # Keep the single ranking PR's targeted_mechanism and optional exemptions
    # unchanged when evaluating the combined tree. Other descriptors have been
    # validated against their own PR diffs as non_ranking or engine_unwired.
    # Without a ranking PR, evaluate each applicable descriptor separately so
    # one PR's expectations cannot silently replace another's.
    selected = ranking or [member for member in members if member["slice_class"] == "engine_unwired"] or members
    evaluations = list(dict.fromkeys(member["descriptor"] for member in selected))
    group_paths = paths(base, head)
    for path in evaluations:
        resolve_git_descriptor(descriptors[path], group_paths, base, head)
    return {"base": base, "head": head, "members": members, "evaluations": evaluations}



def evaluate(candidate: dict, mode: str) -> None:
    descriptors = candidate["evaluations"]
    common = ["--mode", mode, "--base-ref", candidate["base"], "--head", candidate["head"]]
    first = [str(ROOT / "scripts/telemetry/cost-gate.sh"), "--search-quality", *common]
    if mode == "evaluate":
        first.extend(["--descriptor", descriptors[0]])
    subprocess.run(first, check=True)
    if mode == "evaluate":
        # The score measures the combined queue tree. Compare that same score
        # under each remaining descriptor instead of replaying the corpus; the
        # descriptors may require different search-result comparisons.
        for descriptor in descriptors[1:]:
            subprocess.run(
                [sys.executable, str(ROOT / "benchmarks/aft-search/search_quality.py"),
                 *common, "--descriptor", descriptor, "--score",
                 str(ROOT / "benchmarks/aft-search/.bench/search-quality/score.json")],
                check=True,
            )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", required=True)
    parser.add_argument("--head", required=True)
    parser.add_argument("--head-ref", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--mode", choices=("evaluate", "verify"), default="evaluate")
    parser.add_argument("--plan-only", action="store_true")
    parser.add_argument("--plan-output", type=Path)
    args = parser.parse_args()
    try:
        candidate = plan(args.base, args.head, args.head_ref, args.repository)
        text = json.dumps(candidate, sort_keys=True)
        print(f"merge_queue_search_plan:{text}", flush=True)
        if args.plan_output:
            args.plan_output.write_text(text + "\n")
        if not args.plan_only:
            evaluate(candidate, args.mode)
        return 0
    except subprocess.CalledProcessError as error:
        return error.returncode
    except (InputFault, OSError, ValueError, KeyError, TypeError) as error:
        print(f"merge_queue_search_input_fault:{error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
