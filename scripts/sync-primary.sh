#!/usr/bin/env bash
# Safely fast-forward the primary checkout's `main` branch to `origin/main`.
#
# Purpose: the worktree workflow merges PRs server-side via `gh pr merge`
# without ever advancing the primary checkout's local `main` ref. Over time
# the primary drifts behind `origin/main` and any agent that reads its
# working tree sees stale code. This script is the sanctioned reconcile.
#
# Behavior:
#   - Refuses to run from inside a worktree that is NOT the primary checkout
#     (primary = the directory containing `.git/` as a real dir, not a
#     `.git` file pointing at a worktree gitdir).
#   - Refuses if the working tree is dirty (any tracked/untracked changes).
#   - Refuses if local `main` has diverged from `origin/main` (would need
#     a non-fast-forward merge; that is a human decision, not automation's).
#   - Fetches `origin --prune`.
#   - If local `main` is already at or ahead of `origin/main`, exits 0 with
#     a one-line "already current" notice.
#   - Otherwise switches to `main` (if needed) and fast-forwards it, then
#     restores the prior HEAD if we switched off something other than main.
#
# Exit codes:
#   0   synced or already current
#   10  working tree dirty; reconcile or stash before retrying
#   11  local main has diverged from origin/main; manual merge required
#   12  not the primary checkout (called from a worktree); no-op
#   13  no `main` branch locally; nothing to sync
#   20  underlying git command failed
#
# Quiet by default; pass `--verbose` to see every git call.

set -euo pipefail

VERBOSE=0
for arg in "$@"; do
  case "$arg" in
    -v|--verbose) VERBOSE=1 ;;
    -h|--help)
      sed -n '2,32p' "$0"
      exit 0
      ;;
    *)
      echo "sync-primary.sh: unknown argument: $arg" >&2
      exit 64
      ;;
  esac
done

log() {
  if [ "$VERBOSE" = "1" ]; then
    printf 'sync-primary: %s\n' "$*" >&2
  fi
}

note() { printf 'sync-primary: %s\n' "$*"; }
warn() { printf 'sync-primary: %s\n' "$*" >&2; }

# Locate the repo top-level. Fail fast if we're not inside one.
if ! REPO_TOP=$(git rev-parse --show-toplevel 2>/dev/null); then
  warn "not inside a git repository"
  exit 20
fi
cd "$REPO_TOP"

# Distinguish the primary checkout (`.git` is a directory) from a worktree
# (`.git` is a file containing `gitdir: <path>`).
if [ ! -d "$REPO_TOP/.git" ]; then
  warn "called from a worktree, not the primary checkout ($REPO_TOP)"
  warn "no-op; the primary is the directory whose .git/ is a real directory"
  exit 12
fi

# Refuse on dirty tree. We will not stash for the user; that is an explicit
# human choice. The whole point is to never silently mutate the primary.
if [ -n "$(git status --porcelain=v1 2>/dev/null)" ]; then
  warn "working tree is dirty; refusing to fast-forward"
  warn "resolve uncommitted changes in $REPO_TOP first, then re-run"
  exit 10
fi

# Verify a local main branch exists.
if ! git show-ref --verify --quiet refs/heads/main; then
  warn "no local 'main' branch in this repository; nothing to do"
  exit 13
fi

log "fetching origin..."
if ! git fetch origin --prune --quiet 2>/dev/null; then
  warn "git fetch origin --prune failed"
  exit 20
fi

LOCAL=$(git rev-parse refs/heads/main)
REMOTE=$(git rev-parse refs/remotes/origin/main)

if [ "$LOCAL" = "$REMOTE" ]; then
  note "primary main already at origin/main ($(git rev-parse --short refs/heads/main))"
  exit 0
fi

# Behind, ahead, or diverged?
BEHIND=$(git rev-list --count refs/heads/main..refs/remotes/origin/main)
AHEAD=$(git rev-list --count refs/remotes/origin/main..refs/heads/main)

if [ "$AHEAD" -gt 0 ] && [ "$BEHIND" -gt 0 ]; then
  warn "local main has diverged from origin/main (ahead $AHEAD, behind $BEHIND)"
  warn "manual merge/rebase required; refusing to touch primary"
  exit 11
fi

if [ "$AHEAD" -gt 0 ] && [ "$BEHIND" = "0" ]; then
  note "local main is ahead of origin/main by $AHEAD commit(s); nothing to fast-forward"
  exit 0
fi

# Pure behind case: safe to fast-forward.
CUR_BRANCH=$(git symbolic-ref --quiet --short HEAD || echo "")

log "behind origin/main by $BEHIND commit(s); fast-forwarding..."

# If the primary checkout is currently on `main`, do it in place. If it
# is on a different branch, update the ref directly without checking out
# (avoids unnecessary working-tree churn and is safe because the tree is
# already clean).
if [ "$CUR_BRANCH" = "main" ]; then
  if ! git merge --ff-only refs/remotes/origin/main --quiet; then
    warn "git merge --ff-only failed unexpectedly"
    exit 20
  fi
else
  # Update the local main ref to point at origin/main. This is a pure
  # ref update, so the index/working tree are not touched.
  if ! git update-ref refs/heads/main "$REMOTE" "$LOCAL"; then
    warn "git update-ref failed unexpectedly"
    exit 20
  fi
fi

NEW=$(git rev-parse --short refs/heads/main)
note "primary main fast-forwarded to $NEW (was $(git rev-parse --short "$LOCAL"); $BEHIND commit(s))"
exit 0
