#!/usr/bin/env bash
# Squash-merge a PR only after Codex has reviewed it. Wraps `gh pr merge
# <N> --squash` with a pre-merge gate: the PR must carry at least one
# review from the Codex bot (`chatgpt-codex-connector`, posted in response
# to `@codex review` / auto-review on push).
#
# Run from the primary checkout or any worktree.
#
# Safety: the gate fails CLOSED. If `gh` is missing or the reviews query
# fails, the PR is treated as un-reviewed and the merge is refused. Bypass
# a genuinely-unreviewable case with --force.
#
# ponytail: existence check only -- it does not verify the review covers
# the current head SHA. If stale reviews (review predates later pushes)
# become a problem, compare the latest review's commit_id to the PR head
# via `gh api repos/{owner}/{repo}/pulls/{N}/reviews`.
#
# Usage:
#   bash scripts/pr-merge.sh [<PR-number>] [--force]
#   # PR number defaults to the PR for the current branch.
#
# Exit codes:
#   0   merged
#   11  no Codex review -- or the gh query failed -- and --force not given; refused
#   20  gh merge failed
#   64  usage error / no PR resolved

set -euo pipefail

CODEX_REVIEWER="chatgpt-codex-connector"

note() { printf 'pr-merge: %s\n' "$*"; }
warn() { printf 'pr-merge: %s\n' "$*" >&2; }

PR=""
FORCE=0
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    -*) warn "unknown option: $arg"; exit 64 ;;
    *)
      if [ -z "$PR" ]; then PR="$arg"
      else warn "unexpected argument: $arg"; exit 64
      fi
      ;;
  esac
done

if ! command -v gh >/dev/null 2>&1; then
  warn "gh not found; cannot verify Codex review"
  [ "$FORCE" = "1" ] || exit 11
fi

# Resolve PR number from the current branch when not given.
if [ -z "$PR" ]; then
  if ! PR=$(gh pr view --json number --jq .number 2>/dev/null) || [ -z "$PR" ]; then
    warn "no PR number given and none found for the current branch"
    exit 64
  fi
fi

# Gate: at least one review from the Codex bot. Fail closed on query error.
if [ "$FORCE" = "1" ]; then
  note "--force: skipping Codex-review gate for PR #$PR"
else
  reviewers=$(gh pr view "$PR" --json reviews \
    --jq '[.reviews[].author.login] | unique | .[]' 2>/dev/null) || {
    warn "could not read reviews for PR #$PR; refusing (gate fails closed)"
    exit 11
  }
  if ! printf '%s\n' "$reviewers" | grep -qx "$CODEX_REVIEWER"; then
    warn "PR #$PR has no Codex review ($CODEX_REVIEWER); refusing to merge"
    warn "  request one: comment '@codex review' on the PR, then retry"
    warn "  override (not recommended): rerun with --force"
    exit 11
  fi
  note "Codex review present on PR #$PR"
fi

gh pr merge "$PR" --squash || exit 20
note "merged PR #$PR (squash)"
