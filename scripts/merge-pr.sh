#!/usr/bin/env bash
# Operator tooling runs through the real GitHub CLI (gh); the shim is only for AI agent commands.
# Gated PR merge - the ONLY sanctioned way to merge a contributor PR.
#
# Exists because of #234 (2026-08-17): a P1 bot finding landed at 12:10, the
# author pushed the fix at 12:17, and a hand merge at ~12:19 took the head
# WITHOUT the fix - forcing a post-merge follow-up PR. A merge must not race
# an iterating author or an open finding.
#
# Refuses to merge when:
#   1. any review thread is unresolved,
#   2. any review/review-comment is NEWER than the head commit (a finding the
#      author has not pushed over yet),
#   3. the head was pushed very recently (author likely mid-iteration),
#   4. CI on the head is red/pending (merge rides gh's own check gate).
#
# Usage: scripts/merge-pr.sh <pr-number> [--settle-minutes N] [--force-reason "..."]
#        [--expected-head SHA] [--legacy-merge]
set -euo pipefail

PR="${1:?usage: merge-pr.sh <pr-number> [--settle-minutes N] [--force-reason ...]}"
shift || true
SETTLE_MIN=5
FORCE_REASON=""
LEGACY_MERGE=0
EXPECTED_HEAD=""
while [ $# -gt 0 ]; do
  case "$1" in
    --settle-minutes) SETTLE_MIN="$2"; shift 2 ;;
    --force-reason) FORCE_REASON="$2"; shift 2 ;;
    --legacy-merge) LEGACY_MERGE=1; shift ;;
    --expected-head) EXPECTED_HEAD="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$SCRIPT_DIR/lib/operator-gh.sh" || exit 1

OWNER_REPO=$("$OPERATOR_GH" repo view --json nameWithOwner --jq .nameWithOwner)
OWNER="${OWNER_REPO%%/*}"
REPO="${OWNER_REPO##*/}"

refuse() {
  echo "merge-pr: REFUSED - $1" >&2
  if [ -n "$FORCE_REASON" ]; then
    echo "merge-pr: proceeding anyway (--force-reason: $FORCE_REASON)" >&2
    return 0
  fi
  exit 1
}

hard_refuse() {
  echo "merge-pr: REFUSED - $1" >&2
  exit 1
}

disarm_auto_merge() {
  local state pull_request_id queued auto_merge
  state=$("$OPERATOR_GH" api graphql -f query='
    query($owner:String!,$repo:String!,$pr:Int!){
      repository(owner:$owner,name:$repo){
        pullRequest(number:$pr){id isInMergeQueue autoMergeRequest{enabledAt}}
      }
    }' -F owner="$OWNER" -F repo="$REPO" -F pr="$PR" \
    --jq '[.data.repository.pullRequest.id, .data.repository.pullRequest.isInMergeQueue, (.data.repository.pullRequest.autoMergeRequest != null)] | @tsv') ||
    hard_refuse "could not inspect auto-merge/queue state before merge admission"
  IFS=$'\t' read -r pull_request_id queued auto_merge <<< "$state"
  [[ -n "$pull_request_id" ]] || hard_refuse "could not identify PR #$PR to disarm auto-merge"
  if [[ "$queued" == true ]]; then
    "$OPERATOR_GH" api graphql -f query='mutation($id:ID!){dequeuePullRequest(input:{pullRequestId:$id}){clientMutationId}}' \
      -F id="$pull_request_id" >/dev/null || hard_refuse "could not dequeue PR #$PR before merge admission"
    echo "merge-pr: dequeued #$PR before merge admission" >&2
  fi
  if [[ "$auto_merge" == true ]]; then
    "$OPERATOR_GH" api graphql -f query='mutation($id:ID!){disablePullRequestAutoMerge(input:{pullRequestId:$id}){clientMutationId}}' \
      -F id="$pull_request_id" >/dev/null || hard_refuse "could not disable auto-merge for PR #$PR before merge admission"
    echo "merge-pr: disabled auto-merge for #$PR before merge admission" >&2
  fi
}

wait_for_greptile_review() {
  local head_sha="$1" timeout poll started now review_status review_conclusion review_result
  timeout="${AFT_MERGE_PR_GREPTILE_TIMEOUT_SECONDS:-1200}"
  poll="${AFT_MERGE_PR_GREPTILE_POLL_SECONDS:-15}"
  started=$(date -u +%s)
  while :; do
    review_result=$("$OPERATOR_GH" api "repos/$OWNER_REPO/commits/$head_sha/check-runs?per_page=100" \
      --jq '[.check_runs[] | select(.head_sha == "'"$head_sha"'" and .name == "Greptile Review" and .app.slug == "greptile-apps")] | sort_by(.started_at // .created_at // "") | if length == 0 then ["", ""] else [.[-1].status, (.[-1].conclusion // "")] end | @tsv') ||
      hard_refuse "could not read Greptile Review status for head $head_sha"
    IFS=$'\t' read -r review_status review_conclusion <<< "$review_result"
    if [[ "$review_status" == completed ]]; then
      case "$review_conclusion" in
        success|neutral)
          echo "merge-pr: Greptile Review concluded $review_conclusion for head $head_sha" >&2
          return 0
          ;;
        *)
          hard_refuse "Greptile Review concluded ${review_conclusion:-<missing>} for head $head_sha; refusing submission"
          ;;
      esac
    fi
    now=$(date -u +%s)
    if (( now - started >= timeout )); then
      hard_refuse "Greptile Review did not complete for head $head_sha within ${timeout}s; refusing blind submission"
    fi
    sleep "$poll"
  done
}

# Clear any previous queue admission before waiting: GitHub can merge an armed
# PR while a review gate is still running, and refusals must leave it disarmed.
disarm_auto_merge

# --- 1. Wait for the automated review of the exact current head -------------
HEAD_SHA=$("$OPERATOR_GH" pr view "$PR" --json headRefOid --jq .headRefOid)
if [[ -n "$EXPECTED_HEAD" && "$HEAD_SHA" != "$EXPECTED_HEAD" ]]; then
  echo "merge-pr: REFUSED - PR head changed from the accepted delivery; not overridable" >&2
  exit 1
fi
wait_for_greptile_review "$HEAD_SHA"
CURRENT_HEAD=$("$OPERATOR_GH" pr view "$PR" --json headRefOid --jq .headRefOid)
if [[ "$CURRENT_HEAD" != "$HEAD_SHA" ]]; then
  echo "merge-pr: REFUSED - PR head changed while waiting for Greptile Review; not overridable" >&2
  exit 1
fi

# --- 2. Unresolved review threads ------------------------------------------
UNRESOLVED=0
THREAD_CURSOR=""
while :; do
  THREAD_QUERY='query($owner:String!,$repo:String!,$pr:Int!,$after:String){
    repository(owner:$owner,name:$repo){
      pullRequest(number:$pr){
        reviewThreads(first:100,after:$after){nodes{isResolved} pageInfo{hasNextPage endCursor}}
      }
    }
  }'
  THREAD_ARGS=(api graphql -f "query=$THREAD_QUERY" -F owner="$OWNER" -F repo="$REPO" -F pr="$PR")
  [[ -n "$THREAD_CURSOR" ]] && THREAD_ARGS+=(-F after="$THREAD_CURSOR")
  THREAD_PAGE=$("$OPERATOR_GH" "${THREAD_ARGS[@]}" \
    --jq '[([.data.repository.pullRequest.reviewThreads.nodes[]? | select(.isResolved==false)] | length), (.data.repository.pullRequest.reviewThreads.pageInfo.hasNextPage // false), (.data.repository.pullRequest.reviewThreads.pageInfo.endCursor // "")] | @tsv') ||
    hard_refuse "could not read review threads for PR #$PR"
  THREAD_COUNT=""
  THREAD_HAS_NEXT=""
  THREAD_NEXT_CURSOR=""
  IFS=$'\t' read -r THREAD_COUNT THREAD_HAS_NEXT THREAD_NEXT_CURSOR <<< "$THREAD_PAGE"
  [[ "$THREAD_COUNT" =~ ^[0-9]+$ ]] || hard_refuse "malformed review-thread page for PR #$PR"
  UNRESOLVED=$((UNRESOLVED + THREAD_COUNT))
  [[ "$THREAD_HAS_NEXT" == true ]] || break
  [[ -n "$THREAD_NEXT_CURSOR" ]] || hard_refuse "review-thread pagination returned no cursor for PR #$PR"
  THREAD_CURSOR="$THREAD_NEXT_CURSOR"
done
if [ "$UNRESOLVED" -ne 0 ]; then
  disarm_auto_merge
  refuse "$UNRESOLVED unresolved review thread(s); resolve or let the author finish"
fi

# --- 3. Findings newer than the head commit --------------------------------
HEAD_TS=$("$OPERATOR_GH" api "repos/$OWNER_REPO/commits/$HEAD_SHA" --jq .commit.committer.date)
# Fetch comments and reviews separately so a failed comments query cannot be
# hidden by the successful reviews query or the counting pipeline.
COMMENTS=$("$OPERATOR_GH" api "repos/$OWNER_REPO/pulls/$PR/comments" --paginate --jq '.[].created_at')
REVIEWS=$("$OPERATOR_GH" api "repos/$OWNER_REPO/pulls/$PR/reviews" --paginate --jq '.[] | select(.state != "APPROVED" and .state != "PENDING") | .submitted_at')
NEWER=$(printf '%s\n%s\n' "$COMMENTS" "$REVIEWS" | awk -v head="$HEAD_TS" '$0 > head' | wc -l | tr -d ' ')
if [ "$NEWER" -ne 0 ]; then
  # The check exists to stop merging while an author is still addressing
  # findings. When the maintainer resolved the outstanding threads with a
  # recorded rationale (so newer comments are the resolution itself, not
  # unaddressed findings), the maintainer can acknowledge them explicitly.
  # The reason is printed so the merge log carries the judgment.
  if [ -n "${AFT_MERGE_PR_ACK_FINDINGS:-}" ]; then
    echo "merge-pr: ACK - $NEWER post-head comment(s) acknowledged by maintainer: ${AFT_MERGE_PR_ACK_FINDINGS}"
  else
    refuse "$NEWER review comment(s)/review(s) are newer than the head commit - the author has not pushed over the latest finding (set AFT_MERGE_PR_ACK_FINDINGS=\"reason\" to acknowledge deliberately)"
  fi
fi

# --- 4. Author mid-iteration (head pushed too recently) --------------------
NOW_EPOCH=$(date -u +%s)
HEAD_EPOCH=$(python3 -c "import datetime;print(int(datetime.datetime.fromisoformat('$HEAD_TS'.replace('Z','+00:00')).timestamp()))")
AGE_MIN=$(( (NOW_EPOCH - HEAD_EPOCH) / 60 ))
[ "$AGE_MIN" -ge "$SETTLE_MIN" ] || refuse "head commit is ${AGE_MIN}m old (< ${SETTLE_MIN}m settle window) - author may be mid-iteration"

# --- 5. Queue admission (upstream gh enforces required checks) -------------
# REST's branch-protection response omits merge-queue policy. The PR's
# isMergeQueueEnabled GraphQL field is the effective base-ref policy used by
# the real GitHub CLI; an absent/error response never permits a legacy merge.
POLICY=$("$OPERATOR_GH" api graphql -f query='
  query($owner:String!,$repo:String!,$pr:Int!){
    repository(owner:$owner,name:$repo){
      pullRequest(number:$pr){baseRefName isMergeQueueEnabled}
    }
  }' -F owner="$OWNER" -F repo="$REPO" -F pr="$PR" \
  --jq '[.data.repository.pullRequest.baseRefName, .data.repository.pullRequest.isMergeQueueEnabled] | @tsv')
IFS=$'\t' read -r BASE_BRANCH QUEUE_ENABLED <<< "$POLICY"
if [[ "$BASE_BRANCH" != main || ("$QUEUE_ENABLED" != true && "$QUEUE_ENABLED" != false) ]]; then
  echo "merge-pr: REFUSED - unknown queue policy or non-main base; not overridable" >&2
  exit 1
fi
MERGE_ARGS=(--match-head-commit "$HEAD_SHA")
if [[ "$QUEUE_ENABLED" == true ]]; then
  if [[ "$LEGACY_MERGE" == 1 ]]; then
    echo "merge-pr: REFUSED - --legacy-merge cannot bypass enabled queue protection" >&2
    exit 1
  fi
elif [[ "$LEGACY_MERGE" == 1 ]]; then
  MERGE_ARGS+=(--merge)
else
  echo "merge-pr: REFUSED - queue is not enabled; explicit --legacy-merge is required before cutover" >&2
  exit 1
fi

AUTO_MERGE_ALLOWED=false
if [[ "$QUEUE_ENABLED" == true ]]; then
  AUTO_MERGE_ALLOWED=$("$OPERATOR_GH" api "repos/$OWNER_REPO" --jq .allow_auto_merge) ||
    hard_refuse "could not read the repository's auto-merge setting"
fi

# With auto-merge allowed, GitHub admits the PR to the queue itself once its
# required checks pass, so one read is enough: refuse a red check by name, and
# otherwise submit. Without auto-merge, a merge request on a PR whose checks are
# still running is refused by GitHub, so refuse here too and let the caller
# rerun once they pass. Never wait inside this script: the review gates above
# were read before any wait and would go stale.
if [[ "$QUEUE_ENABLED" == true ]]; then
  CHECK_EXIT=0
  if REQUIRED_CHECKS=$("$OPERATOR_GH" pr checks "$PR" --required --json name,bucket \
      --jq '.[] | [.name, .bucket] | @tsv'); then
    :
  else
    CHECK_EXIT=$?
  fi
  if [ "$CHECK_EXIT" -ne 0 ] && [ "$CHECK_EXIT" -ne 1 ] && [ "$CHECK_EXIT" -ne 8 ]; then
    hard_refuse "could not read required checks for PR #$PR"
  fi
  [ -n "$REQUIRED_CHECKS" ] || hard_refuse "no required checks found for PR #$PR"
  PENDING_NAMES=()
  while IFS=$'\t' read -r CHECK_NAME CHECK_BUCKET; do
    [ -n "$CHECK_NAME" ] && [ -n "$CHECK_BUCKET" ] ||
      hard_refuse "malformed required-check result for PR #$PR"
    case "$CHECK_BUCKET" in
      pass|skipping) ;;
      pending) PENDING_NAMES+=("$CHECK_NAME") ;;
      fail|cancel)
        hard_refuse "required check '$CHECK_NAME' did not pass ($CHECK_BUCKET)"
        ;;
      *) hard_refuse "required check '$CHECK_NAME' has unknown status '$CHECK_BUCKET'" ;;
    esac
  done <<< "$REQUIRED_CHECKS"
  if [ "$CHECK_EXIT" -eq 1 ]; then
    hard_refuse "gh pr checks failed without a named required-check result for PR #$PR"
  fi
  if [ "$CHECK_EXIT" -eq 8 ] && [ "${#PENDING_NAMES[@]}" -eq 0 ]; then
    hard_refuse "gh reported pending required checks without naming a pending check for PR #$PR"
  fi
  if [ "${#PENDING_NAMES[@]}" -gt 0 ]; then
    if [[ "$AUTO_MERGE_ALLOWED" == true ]]; then
      echo "merge-pr: required checks still running for #$PR; GitHub queues it when they pass" >&2
    else
      hard_refuse "required checks still running on PR #$PR and auto-merge is off; rerun once they pass: ${PENDING_NAMES[*]}"
    fi
  fi
fi

echo "merge-pr: admission gates satisfied (head ${AGE_MIN}m settled) - submitting #$PR at $HEAD_SHA"
# Submit with the operator's real GitHub CLI, not the agent's bot-routed shim.
GH_SHIM_BYPASS=operator "$OPERATOR_GH" pr merge "$PR" "${MERGE_ARGS[@]}"
