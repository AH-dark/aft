#!/usr/bin/env bash
# Offline Git/gh integration tests; every origin is a private temporary bare repo.
set -euo pipefail
python3 --version
python3 -m unittest discover -s "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib" -p test_pr_landing.py -v
