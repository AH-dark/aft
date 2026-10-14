"""Idempotent agent delivery submission and exact queue-attempt observation."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
from importlib import import_module
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
from urllib.parse import quote

from landing_policy import AGENT_ATTRIBUTION_POLICY, ROOT, check_commits, git


def log(text: str) -> None:
    print(f"land-pr: {text}", file=sys.stderr, flush=True)


def gh(*args: str) -> str:
    # Removing an inherited administration bypass keeps PR creation bot-routed.
    env = {key: value for key, value in os.environ.items() if key != "GH_SHIM_BYPASS"}
    return subprocess.check_output(["gh", *args], env=env, text=True).strip()


def api(path: str) -> dict:
    return json.loads(gh("api", path))


def pages(path: str) -> list[dict]:
    return [item for page in json.loads(gh("api", "--paginate", "--slurp", path)) for item in page]


def snapshot(repo: str, number: int) -> dict:
    owner, name = repo.split("/")
    response = json.loads(gh(
        "api", "graphql", "-f", "query=query($owner:String!,$repo:String!,$pr:Int!){"
        "repository(owner:$owner,name:$repo){pullRequest(number:$pr){"
        "state headRefOid baseRefName isMergeQueueEnabled isInMergeQueue mergeCommit{oid} "
        "autoMergeRequest{enabledAt} mergeQueueEntry{enqueuedAt state headCommit{oid}} "
        "timelineItems(last:1,itemTypes:[REMOVED_FROM_MERGE_QUEUE_EVENT]){nodes{"
        "... on RemovedFromMergeQueueEvent{createdAt reason beforeCommit{oid}}}}}}}",
        "-F", f"owner={owner}", "-F", f"repo={name}", "-F", f"pr={number}",
    ))
    return response["data"]["repository"]["pullRequest"]


def save(path: Path, state: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(state, sort_keys=True) + "\n")
    temporary.chmod(0o600)
    temporary.replace(path)


def stamp(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))




def workflow_runs(repo: str, head: str) -> list[dict]:
    response = json.loads(gh("api", "--paginate", "--slurp",
                            f"repos/{repo}/actions/workflows/tests.yml/runs?event=merge_group&head_sha={head}&per_page=100"))
    return [run for page in response for run in page["workflow_runs"]]


def report_removed(repo: str, state: dict, removal: dict | None) -> None:
    heads = list(state.get("groups", []))
    candidate = ((removal or {}).get("beforeCommit") or {}).get("oid")
    if candidate and candidate not in heads:
        cursor = candidate
        for _ in range(100):
            parents = api(f"repos/{repo}/git/commits/{cursor}")["parents"]
            if len(parents) != 2:
                break
            if parents[1]["sha"] == state["head"]:
                heads.append(candidate)
                break
            cursor = parents[0]["sha"]
    found = False
    for head in heads:
        for run in workflow_runs(repo, head):
            if (run["head_sha"] != head or run["event"] != "merge_group"
                    or stamp(run["created_at"]) < stamp(state["admitted_at"])
                    or run["conclusion"] not in {"failure", "timed_out", "cancelled"}):
                continue
            log(f"failing group {head}: {run['html_url']} ({run['conclusion']})")
            jobs = pages(f"repos/{repo}/actions/runs/{run['id']}/jobs?per_page=100")
            bad = [job for job in jobs if job["conclusion"] in {"failure", "timed_out", "cancelled"}]
            for job in bad:
                log(f"  {job['name']}: {job['conclusion']}")
            if not bad:
                log("  no failing job was reported; inspect missing checks or queue timeout")
            found = True
    if not found:
        log("removed from queue; exact failing group/run could not be established (no stale run attributed)")


def wait_merged(repo: str, state: dict, path: Path, deadline: float, poll: float) -> str:
    while time.monotonic() < deadline:
        current = snapshot(repo, state["pr"])
        if current["headRefOid"] != state["head"] or current["baseRefName"] != "main":
            raise ValueError("PR head/base changed after admission; refusing stale delivery")
        if current["state"] == "MERGED":
            oid = (current["mergeCommit"] or {}).get("oid", "")
            if re.fullmatch(r"[0-9a-f]{40}", oid) is None:
                raise ValueError("merged PR has no exact final merge OID")
            state["merge_oid"] = oid
            save(path, state)
            log(f"merged #{state['pr']} at {oid}")
            return oid
        if current["state"] != "OPEN":
            raise ValueError("PR closed without merging")
        entry = current["mergeQueueEntry"]
        if entry:
            state["seen_queued"] = True
            group = entry["headCommit"]["oid"]
            if group not in state["groups"]:
                state["groups"].append(group)
            save(path, state)
        removals = current["timelineItems"]["nodes"]
        removal = removals[-1] if removals and stamp(removals[-1]["createdAt"]) >= stamp(state["admitted_at"]) else None
        if not current["isInMergeQueue"] and (state.get("seen_queued") or removal):
            state["removed"] = True
            save(path, state)
            report_removed(repo, state, removal)
            raise ValueError(f"PR #{state['pr']} removed from queue: {(removal or {}).get('reason', 'entry disappeared')}")
        time.sleep(poll)
    raise ValueError(f"timed out waiting for PR #{state['pr']}; rerun to resume this same PR")


def submit(args: argparse.Namespace) -> str:
    os.chdir(git("rev-parse", "--show-toplevel"))
    if re.fullmatch(r"[a-z0-9][a-z0-9._-]{0,99}", args.name) is None:
        raise ValueError("delivery name must be a simple lowercase branch suffix")
    if git("status", "--porcelain"):
        raise ValueError("working tree must be clean before landing")
    branch = f"land/{args.name}"
    path = Path(git("rev-parse", "--git-path", f"aft-land/{args.name}.json"))
    state = json.loads(path.read_text()) if path.exists() else {}
    repo = gh("repo", "view", "--json", "nameWithOwner", "--jq", ".nameWithOwner")
    owner = repo.split("/")[0]
    prs = [pr for pr in pages(f"repos/{repo}/pulls?state=all&base=main&head={quote(owner + ':' + branch, safe='')}&per_page=100")
           if pr["head"]["ref"] == branch and pr["head"]["repo"]["full_name"] == repo
           and pr["base"]["ref"] == "main"]
    if len(prs) > 1:
        raise ValueError("ambiguous existing PRs for delivery branch")
    if prs and prs[0]["state"] == "closed":
        current = snapshot(repo, prs[0]["number"])
        if current["state"] != "MERGED" or current["headRefOid"] != git("rev-parse", "HEAD"):
            raise ValueError("delivery branch already closed or merged at a different head")
        state.update(pr=prs[0]["number"], head=current["headRefOid"], repo=repo)
        state["merge_oid"] = current["mergeCommit"]["oid"]
        if re.fullmatch(r"[0-9a-f]{40}", state["merge_oid"]) is None:
            raise ValueError("merged PR has no exact final merge OID")
        save(path, state)
        return state["merge_oid"]
    git("fetch", "--quiet", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main")
    head = git("rev-parse", "HEAD")
    base = git("merge-base", "origin/main", head)
    if not prs and base != git("rev-parse", "origin/main"):
        raise ValueError("first submission must be based on current origin/main")
    if git("branch", "--show-current") != branch:
        exists = subprocess.run(["git", "show-ref", "--verify", "--quiet", f"refs/heads/{branch}"], check=False)
        if exists.returncode == 0 and git("rev-parse", branch) != head:
            raise ValueError("local delivery branch already points at another commit")
        git("checkout", branch) if exists.returncode == 0 else git("checkout", "-b", branch)
    quality = import_module("merge-queue-search")
    changed = quality.paths(base, head)
    descriptor_path = Path(f"benchmarks/aft-search/slice-descriptors/land-{args.name}.json")
    if args.descriptor:
        descriptor = json.loads(args.descriptor.read_text())
    elif descriptor_path.exists():
        descriptor = json.loads(descriptor_path.read_text())
    else:
        descriptor = {"slice_class": "non_ranking", "targeted_mechanism": "none",
                      "kind": "harness", "fixtures": ["harness-goldens"]}
    quality.resolve_descriptor(descriptor, changed)
    encoded = json.dumps(descriptor, sort_keys=True, indent=2) + "\n"
    if not descriptor_path.exists() or json.loads(descriptor_path.read_text()) != descriptor:
        descriptor_path.parent.mkdir(parents=True, exist_ok=True)
        descriptor_path.write_text(encoded)
        git("add", str(descriptor_path))
        git("commit", "-m", f"landing: describe {args.name}", "-m",
            f"Co-authored-by: {AGENT_ATTRIBUTION_POLICY['coauthor']}")
    head = git("rev-parse", "HEAD")
    check_commits(base, head, branch)
    # Require the governed checkout binding before any bot-authored PR write.
    if gh("--co-author-line") != AGENT_ATTRIBUTION_POLICY["coauthor"]:
        raise ValueError("governed gh shim must be bound to alfonso-aft for PR speech")
    state.update(repo=repo, branch=branch, head=head)
    save(path, state)
    git("push", "origin", f"HEAD:refs/heads/{branch}")
    if not prs:
        body = args.body_file.read_text() if args.body_file else "Accepted agent delivery; branch-keyed search descriptor included."
        try:
            gh("pr", "create", "--base", "main", "--head", branch,
               "--title", args.title or f"land: {args.name}", "--body", body)
        except subprocess.CalledProcessError:
            # A concurrent creator may have won; exact re-query still must find
            # one matching PR, rather than treating any create failure as green.
            pass
        prs = pages(f"repos/{repo}/pulls?state=open&base=main&head={quote(owner + ':' + branch, safe='')}&per_page=100")
        prs = [pr for pr in prs if pr["head"]["ref"] == branch and pr["head"]["repo"]["full_name"] == repo]
        if len(prs) != 1:
            raise ValueError("could not establish exactly one created delivery PR")
    state["pr"] = prs[0]["number"]
    save(path, state)
    if args.title or args.body_file:
        update = ["pr", "edit", str(state["pr"])]
        if args.title:
            update.extend(["--title", args.title])
        if args.body_file:
            update.extend(["--body", args.body_file.read_text()])
        gh(*update)
    current = snapshot(repo, state["pr"])
    if current["headRefOid"] != head:
        raise ValueError("remote PR head does not match the pushed delivery")
    deadline = time.monotonic() + float(os.environ.get("LAND_PR_TIMEOUT_SECONDS", "14400"))
    poll = float(os.environ.get("LAND_PR_POLL_SECONDS", "30"))
    if not current["isInMergeQueue"] and not current["autoMergeRequest"]:
        date = stamp(api(f"repos/{repo}/commits/{head}")["commit"]["committer"]["date"])
        while (datetime.now(timezone.utc) - date).total_seconds() < 300:
            if time.monotonic() >= deadline:
                raise ValueError("settle timeout; resume the same PR later")
            time.sleep(poll)
        state.update(admitted_at=datetime.now(timezone.utc).isoformat(), groups=[], seen_queued=False, removed=False)
        save(path, state)
        command = [str(ROOT / "scripts/merge-pr.sh"), str(state["pr"]), "--expected-head", head]
        if args.legacy_merge:
            command.append("--legacy-merge")
        subprocess.run(command, stdout=sys.stderr, check=True)
    elif "admitted_at" not in state:
        state.update(admitted_at=(current["mergeQueueEntry"] or {}).get("enqueuedAt") or current["autoMergeRequest"]["enabledAt"],
                     groups=[], seen_queued=False)
        save(path, state)
    return wait_merged(repo, state, path, deadline, poll)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("name")
    parser.add_argument("--descriptor", type=Path)
    parser.add_argument("--title")
    parser.add_argument("--body-file", type=Path)
    parser.add_argument("--legacy-merge", action="store_true")
    try:
        print(submit(parser.parse_args()), flush=True)
        return 0
    except (ValueError, KeyError, TypeError, OSError, subprocess.CalledProcessError) as error:
        log(f"REFUSED: {error}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
