#!/usr/bin/env bash
# Deterministic polling tests for watch-ci.sh. Fake gh responses and sleep calls
# make API usage and backoff observable without network access or wall-clock waits.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WATCH_CI="$SCRIPT_DIR/watch-ci.sh"
REAL_JQ="$(command -v jq || true)"
REAL_SLEEP="$(command -v sleep || true)"
if [ -z "$REAL_JQ" ]; then
  echo 'watch-ci.test.sh: jq is required' >&2
  exit 2
fi
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/watch-ci-test.XXXXXX")"
trap 'rm -rf "$TMP_ROOT"' EXIT

BIN="$TMP_ROOT/bin"
STATE="$TMP_ROOT/state"
REPO="$TMP_ROOT/repo"
mkdir -p "$BIN" "$STATE/responses"
git init -q "$REPO"
git -C "$REPO" config remote.origin.url https://github.com/example/repo.git
git -C "$REPO" config user.name 'Watcher Test'
git -C "$REPO" config user.email 'watcher@example.invalid'
mkdir -p "$REPO/.github/workflows"
printf 'name: Tests\n' > "$REPO/.github/workflows/tests.yml"

cat > "$BIN/gh" <<'GH'
#!/usr/bin/env bash
set -u
STATE="${WATCH_CI_TEST_STATE:?}"
printf '%s\n' "$*" >> "$STATE/calls"
if [ "${WATCH_CI_TEST_ALWAYS_ERROR:-0}" = 1 ]; then
  echo 'same fake gh error' >&2
  exit 1
fi
count=0
[ ! -f "$STATE/calls" ] || count="$(wc -l < "$STATE/calls" | tr -d ' ')"
response="$STATE/responses/$count"
[ -f "$response" ] || { echo "unexpected gh query $count: $*" >&2; exit 90; }
if [ "$(head -1 "$response")" = ERROR ]; then
  tail -n +2 "$response" >&2
  exit 1
fi
cat "$response"
GH
cat > "$BIN/sleep" <<'SLEEP'
#!/usr/bin/env bash
printf '%s\n' "$1" >> "${WATCH_CI_TEST_STATE:?}/sleeps"
if [ "${WATCH_CI_TEST_REAL_SLEEP:-0}" = 1 ]; then
  "${WATCH_CI_TEST_REAL_SLEEP_BIN:?}" "$1"
fi
SLEEP
cat > "$BIN/jq" <<'JQ'
#!/usr/bin/env bash
case "${WATCH_CI_TEST_FAIL_JQ:-}" in
  gating)
    for arg in "$@"; do
      case "$arg" in
        *'join("; ")'*) echo 'injected gating jq failure' >&2; exit 42 ;;
      esac
    done
    ;;
  failed-job)
    for arg in "$@"; do
      case "$arg" in
        *'select(.conclusion == "failure")'*) echo 'injected failed-job jq failure' >&2; exit 42 ;;
      esac
    done
    ;;
esac
exec "${WATCH_CI_TEST_REAL_JQ:?}" "$@"
JQ
chmod +x "$BIN/gh" "$BIN/sleep" "$BIN/jq"

checks=0
failures=0
ok() { checks=$((checks + 1)); printf 'watch-ci.test.sh: ok — %s\n' "$1"; }
fail() { printf 'watch-ci.test.sh: FAIL — %s\n' "$1" >&2; failures=$((failures + 1)); }

reset_case() {
  rm -f "$STATE/calls" "$STATE/sleeps" "$STATE/responses"/* "$TMP_ROOT/output"
}

response() {
  local index="$1"
  shift
  printf '%s\n' "$*" > "$STATE/responses/$index"
}

api_error() { printf 'ERROR\n' > "$STATE/responses/$1"; }

run_watch() {
  local poll_sleep="${1:-1}" max_sleep="${2:-8}" fail_jq="${3:-}" workflow="${4:-}"
  set +e
  if [ -n "$workflow" ]; then
    (cd "$REPO" && env -u WATCH_CI_WORKFLOW WATCH_CI_WORKFLOW="$workflow" PATH="$BIN:$PATH" WATCH_CI_TEST_STATE="$STATE" \
      WATCH_CI_TEST_REAL_JQ="$REAL_JQ" WATCH_CI_TEST_FAIL_JQ="$fail_jq" \
      OPERATOR_GH_FALLBACK_PATHS="$BIN/gh" WATCH_CI_POLL_SLEEP="$poll_sleep" \
      WATCH_CI_POLL_MAX_SLEEP="$max_sleep" "$WATCH_CI" 4242) \
      > "$TMP_ROOT/output" 2>&1
  else
    (cd "$REPO" && env -u WATCH_CI_WORKFLOW PATH="$BIN:$PATH" WATCH_CI_TEST_STATE="$STATE" \
      WATCH_CI_TEST_REAL_JQ="$REAL_JQ" WATCH_CI_TEST_FAIL_JQ="$fail_jq" \
      OPERATOR_GH_FALLBACK_PATHS="$BIN/gh" WATCH_CI_POLL_SLEEP="$poll_sleep" \
      WATCH_CI_POLL_MAX_SLEEP="$max_sleep" "$WATCH_CI" 4242) \
      > "$TMP_ROOT/output" 2>&1
  fi
  WATCH_RC=$?
  set -e
}

run_watch_repeated_gh_error() {
  local poll_sleep="${1:-0.01}" max_sleep="${2:-0.04}"
  set +e
  python3 - "$WATCH_CI" "$REPO" "$BIN" "$STATE" "$REAL_JQ" "$REAL_SLEEP" "$TMP_ROOT/output" "$poll_sleep" "$max_sleep" <<'PY'
import os, signal, subprocess, sys

script, repo, bin_dir, state, real_jq, real_sleep, output, poll_sleep, max_sleep = sys.argv[1:]
env = os.environ.copy()
env.pop("WATCH_CI_WORKFLOW", None)
env.update({
    "PATH": bin_dir + os.pathsep + env["PATH"],
    "WATCH_CI_TEST_STATE": state,
    "WATCH_CI_TEST_REAL_JQ": real_jq,
    "WATCH_CI_TEST_FAIL_JQ": "",
    "WATCH_CI_TEST_ALWAYS_ERROR": "1",
    "WATCH_CI_TEST_REAL_SLEEP": "1",
    "WATCH_CI_TEST_REAL_SLEEP_BIN": real_sleep,
    "OPERATOR_GH_FALLBACK_PATHS": os.path.join(bin_dir, "gh"),
    "WATCH_CI_POLL_SLEEP": poll_sleep,
    "WATCH_CI_POLL_MAX_SLEEP": max_sleep,
})
process = subprocess.Popen(
    [script, "4242"], cwd=repo, env=env,
    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    start_new_session=True,
)
try:
    captured, _ = process.communicate(timeout=2)
    result = process.returncode
except subprocess.TimeoutExpired:
    os.killpg(process.pid, signal.SIGKILL)
    captured, _ = process.communicate()
    result = 124
with open(output, "w") as stream:
    stream.write(captured)
sys.exit(result)
PY
  WATCH_RC=$?
  set -e
}

completed_success='{"status":"completed","conclusion":"success","jobs":[{"name":"Unit","databaseId":101,"conclusion":"success"}],"url":"https://github.com/example/repo/actions/runs/4242"}'

# The default workflow is tests.yml. It must not silently guess ci.yml when the
# default is absent; an explicit override remains available for callers that use
# a different workflow.
reset_case
response 1 "$completed_success"
rm "$REPO/.github/workflows/tests.yml"
run_watch
calls=0
[ ! -f "$STATE/calls" ] || calls="$(wc -l < "$STATE/calls" | tr -d ' ')"
if [ "$WATCH_RC" -eq 2 ] && grep -q 'WATCH_CI_WORKFLOW' "$TMP_ROOT/output" &&
  [ "$calls" -eq 0 ]; then
  ok 'missing default workflow refuses and names WATCH_CI_WORKFLOW'
else
  fail 'missing default workflow refuses and names WATCH_CI_WORKFLOW'
fi
printf 'name: CI\n' > "$REPO/.github/workflows/ci.yml"
reset_case
response 1 "$completed_success"
run_watch 1 8 '' ci.yml
if [ "$WATCH_RC" -eq 0 ] && grep -q 'CI_DONE run=4242 conclusion=success' "$TMP_ROOT/output"; then
  ok 'explicit workflow override watches a non-default workflow'
else
  fail 'explicit workflow override watches a non-default workflow'
fi
rm "$REPO/.github/workflows/ci.yml"
printf 'name: Tests\n' > "$REPO/.github/workflows/tests.yml"

# A successful completed run needs one combined API response, not separate
# status, URL, and jobs queries.
reset_case
response 1 "$completed_success"
run_watch
calls=0
if [ -f "$STATE/calls" ]; then
  calls="$(wc -l < "$STATE/calls" | tr -d ' ')"
fi
if [ "$WATCH_RC" -eq 0 ] && [ "$calls" -eq 1 ] &&
  grep -q -- '--json status,conclusion,jobs,url' "$STATE/calls" &&
  grep -q '^CI_RUN_URL https://github.com/example/repo/actions/runs/4242$' "$TMP_ROOT/output"; then
  ok 'one combined gh query supplies the first completed tick and run URL'
else
  fail 'one combined gh query supplies the first completed tick and run URL'
fi

# Consecutive API errors back off to the cap; a successful response resets the
# delay, including after another isolated API error.
reset_case
api_error 1
api_error 2
api_error 3
response 4 '{"status":"in_progress","conclusion":null,"jobs":[],"url":"https://github.com/example/repo/actions/runs/4242"}'
api_error 5
response 6 "$completed_success"
run_watch 1 4
delays="$(tr '\n' ' ' < "$STATE/sleeps" | sed 's/ $//')"
if [ "$WATCH_RC" -eq 0 ] && [ "$delays" = '1.000 2.000 4.000 1 1.000' ]; then
  ok 'API errors back off with a cap and a success resets the delay'
else
  fail "API errors back off with a cap and a success resets the delay (delays: $delays)"
fi

# A persistent gh error is identical on each call. The watcher must back off,
# stop after four calls, and report the final upstream diagnostic.
reset_case
run_watch_repeated_gh_error
calls="$(wc -l < "$STATE/calls" | tr -d ' ')"
delays="$(tr '\n' ' ' < "$STATE/sleeps" | sed 's/ $//')"
if [ "$WATCH_RC" -eq 5 ] && [ "$calls" -eq 4 ] && [ "$delays" = '0.010 0.020 0.040' ] &&
  grep -q 'same fake gh error' "$TMP_ROOT/output"; then
  ok 'repeated gh errors back off, stop after four attempts, and report the last error'
else
  fail "repeated gh errors are bounded with the last error (rc=$WATCH_RC calls=$calls delays=$delays)"
fi

# A completed payload with an unreadable job list fails closed even when the
# run's conclusion is success.
reset_case
response 1 '{"status":"completed","conclusion":"success","jobs":null,"url":""}'
run_watch
if [ "$WATCH_RC" -eq 3 ] && grep -q 'CI_UNDETERMINED' "$TMP_ROOT/output"; then
  ok 'unreadable completed job list exits 3'
else
  fail 'unreadable completed job list exits 3'
fi

# A completed status with no readable conclusion is equally undetermined,
# even when the jobs are present and individually green.
reset_case
response 1 '{"status":"completed","conclusion":null,"jobs":[{"name":"Unit","databaseId":101,"conclusion":"success"}],"url":""}'
run_watch
if [ "$WATCH_RC" -eq 3 ] && grep -q 'CI_UNDETERMINED' "$TMP_ROOT/output"; then
  ok 'unreadable completed conclusion exits 3'
else
  fail 'unreadable completed conclusion exits 3'
fi

# A successful but empty jobs array is not evidence that the run passed.
reset_case
response 1 '{"status":"completed","conclusion":"success","jobs":[],"url":""}'
run_watch
if [ "$WATCH_RC" -eq 3 ] && grep -q 'CI_UNDETERMINED' "$TMP_ROOT/output"; then
  ok 'empty completed job list exits 3'
else
  fail 'empty completed job list exits 3'
fi

# A run where every job was skipped did not execute a gating job and is not
# evidence of CI success, even if Actions reports the run conclusion as green.
reset_case
response 1 '{"status":"completed","conclusion":"success","jobs":[{"name":"Filtered","databaseId":101,"conclusion":"skipped"}],"url":""}'
run_watch
if [ "$WATCH_RC" -ne 0 ] && grep -q 'no gating job ran' "$TMP_ROOT/output"; then
  ok 'all-skipped completed run is not treated as green'
else
  fail 'all-skipped completed run is not treated as green'
fi

# Keep the watcher's established all-jobs-passed report. train-push's separate
# completed-conclusion guard prevents that report from authorizing a landing.
reset_case
response 1 '{"status":"completed","conclusion":"cancelled","jobs":[{"name":"Unit","databaseId":101,"conclusion":"success"}],"url":""}'
run_watch
if [ "$WATCH_RC" -eq 0 ] && grep -q 'CI_DONE run=4242 conclusion=cancelled jobs_all_passed=1' "$TMP_ROOT/output"; then
  ok 'non-success run retains watcher output while the landing guard stays authoritative'
else
  fail 'non-success run retains watcher output while the landing guard stays authoritative'
fi

# An unnamed failure remains a real red job; it must not disappear from the
# fail-fast query just because the API omitted its display name.
reset_case
response 1 '{"status":"completed","conclusion":"cancelled","jobs":[{"name":null,"databaseId":101,"conclusion":"failure"}],"url":""}'
run_watch
if [ "$WATCH_RC" -eq 1 ] && grep -q "CI_EARLY_FAIL job='<unnamed>' run=4242" "$TMP_ROOT/output"; then
  ok 'failed job with a null name remains fail-fast red'
else
  fail 'failed job with a null name remains fail-fast red'
fi

# Failures in either jq extraction are undetermined, never empty success data.
reset_case
response 1 '{"status":"completed","conclusion":"cancelled","jobs":[{"name":"Unit","databaseId":101,"conclusion":"cancelled"}],"url":""}'
run_watch 1 8 gating
if [ "$WATCH_RC" -eq 3 ] && grep -q 'CI_UNDETERMINED.*could not read job conclusions' "$TMP_ROOT/output"; then
  ok 'a gating-job jq failure exits 3'
else
  fail 'a gating-job jq failure exits 3'
fi

reset_case
response 1 '{"status":"completed","conclusion":"success","jobs":[{"name":"Unit","databaseId":101,"conclusion":"success"}],"url":""}'
run_watch 1 8 failed-job
if [ "$WATCH_RC" -eq 3 ] && grep -q 'CI_UNDETERMINED.*could not read job conclusions' "$TMP_ROOT/output"; then
  ok 'a fail-fast-job jq failure exits 3'
else
  fail 'a fail-fast-job jq failure exits 3'
fi

if [ "$failures" -ne 0 ]; then
  printf 'watch-ci.test.sh: %s check(s) failed\n' "$failures" >&2
  exit 1
fi
printf 'watch-ci.test.sh: passed (%s checks)\n' "$checks"
