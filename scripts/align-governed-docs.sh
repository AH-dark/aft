#!/usr/bin/env bash
# Compatibility entry point for operators with an old alignment command.
# Publication records now live in disposable snapshots, never train commits.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

exec node scripts/release-gate-v049.mjs --check-inputs
