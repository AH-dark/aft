#!/usr/bin/env bash
# PR creation/edits use the bot-routed gh shim on PATH; merge-pr handles
# operator-authorized queue admission with the real GitHub CLI.
set -euo pipefail
exec python3 "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/land_pr.py" "$@"
