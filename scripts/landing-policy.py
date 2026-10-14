#!/usr/bin/env python3
"""Check original commit identities without changing the checkout or messages."""

import argparse
from importlib import import_module
import json
from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent / "lib"))
policy = import_module("landing_policy")
AGENT_ATTRIBUTION_POLICY = policy.AGENT_ATTRIBUTION_POLICY
check_commits = policy.check_commits
check_event = policy.check_event


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event")
    parser.add_argument("--event-path", type=Path)
    parser.add_argument("--base")
    parser.add_argument("--head")
    parser.add_argument("--branch")
    parser.add_argument("--coauthor", action="store_true")
    args = parser.parse_args()
    try:
        if args.coauthor:
            print(AGENT_ATTRIBUTION_POLICY["coauthor"])
            return 0
        if args.event and args.event_path:
            count = check_event(args.event, json.loads(args.event_path.read_text()))
        elif args.base and args.head and args.branch:
            count = check_commits(args.base, args.head, args.branch)
        else:
            parser.error("provide an event payload or explicit base/head/branch")
        print(f"Landing policy: {count} original commits checked")
        return 0
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        print(f"Landing policy: REFUSED: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
