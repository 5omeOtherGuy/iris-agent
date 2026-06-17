#!/usr/bin/env bash
# One-shot, quiet post-merge worktree cleanup. Replaces the five-command
# cleanup chain (worktree remove + branch -D + fetch + sync-primary + prune)
# with a single call that emits one status line on success.
#
# Run from OUTSIDE the worktree being removed (the primary checkout or any
# sibling worktree). sync-primary is always applied to the resolved primary
# checkout, so this works regardless of where it is invoked.
#
# Safety:
#   - Refuses to remove the primary checkout.
#   - `git branch -D` is a FORCE delete and, because the repo squash-merges,
#     a merged branch's commits are never ancestors of origin/main. To avoid
#     destroying committed-but-unmerged work, when `gh` is available this
#     refuses unless a MERGED PR exists for the branch. Bypass with --force.
#   - The deleted branch's tip SHA is printed for one-command recovery
#     (`git branch <name> <sha>`).
#
# Usage:
#   bash scripts/worktree-cleanup.sh ../iris-<slug> [<branch>] [--force]
#
# Exit codes:
#   0   cleaned up
#   11  branch has no merged PR -- or the gh query failed -- and --force not
#       given; refused (no changes made). The guard fails CLOSED: an
#       unanswerable "is it merged?" is treated as "not merged".
#   12  target is the primary checkout; refused
#   13  primary main could not be synced (reconcile manually)
#   64  usage error
#   20  git failure

set -euo pipefail

note() { printf 'cleanup: %s\n' "$*"; }
warn() { printf 'cleanup: %s\n' "$*" >&2; }

WT_PATH=""
BRANCH=""
FORCE=0
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    -h|--help) sed -n '2,29p' "$0"; exit 0 ;;
    -*) warn "unknown option: $arg"; exit 64 ;;
    *)
      if [ -z "$WT_PATH" ]; then WT_PATH="$arg"
      elif [ -z "$BRANCH" ]; then BRANCH="$arg"
      else warn "unexpected argument: $arg"; exit 64
      fi
      ;;
  esac
done

if [ -z "$WT_PATH" ]; then
  warn "usage: scripts/worktree-cleanup.sh <worktree-path> [branch] [--force]"
  exit 64
fi

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

if [ ! -e "$WT_PATH" ]; then
  warn "worktree path does not exist: $WT_PATH"
  exit 64
fi
WT_ABS=$(cd "$WT_PATH" && pwd)

# Guard: never remove the primary checkout (its .git is a real directory).
if [ -d "$WT_ABS/.git" ]; then
  warn "refusing to remove the primary checkout ($WT_ABS)"
  exit 12
fi

# Resolve the primary checkout so sync-primary always targets it, even when
# cleanup runs from a sibling worktree.
COMMON_DIR=$(git rev-parse --git-common-dir 2>/dev/null || true)
[ -n "$COMMON_DIR" ] || { warn "cannot resolve git common dir"; exit 20; }
case "$COMMON_DIR" in
  /*) ;;
  *) COMMON_DIR="$(git rev-parse --show-toplevel)/$COMMON_DIR" ;;
esac
PRIMARY_TOP=$(cd "$(dirname "$COMMON_DIR")" && pwd)

# Infer the branch from the worktree if not supplied.
if [ -z "$BRANCH" ]; then
  BRANCH=$(git -C "$WT_ABS" symbolic-ref --quiet --short HEAD 2>/dev/null || true)
fi

# Capture the branch tip BEFORE any deletion, for recoverability.
TIP=""
if [ -n "$BRANCH" ]; then
  TIP=$(git rev-parse --short "$BRANCH" 2>/dev/null || true)
fi

# Refuse to force-delete a branch with no merged PR (unless --force).
if [ "$FORCE" = "0" ] && [ -n "$BRANCH" ] && [ "$BRANCH" != "main" ] && command -v gh >/dev/null 2>&1; then
  merged=$(gh pr list --head "$BRANCH" --state merged --json number --jq 'length' 2>/dev/null || echo "")
  case "$merged" in
    ''|*[!0-9]*)
      warn "could not query merged PRs for branch '$BRANCH' (gh failed: auth/network?); refusing to force-delete"
      warn "branch tip is ${TIP:-unknown}; re-run with --force to drop it anyway"
      exit 11
      ;;
  esac
  if [ "$merged" = "0" ]; then
    open=$(gh pr list --head "$BRANCH" --state open --json number --jq 'length' 2>/dev/null || echo "?")
    warn "no MERGED PR found for branch '$BRANCH' (open PRs: $open); refusing to force-delete"
    warn "branch tip is ${TIP:-unknown}; re-run with --force to drop it anyway"
    exit 11
  fi
fi

git worktree remove "$WT_ABS"
if [ -n "$BRANCH" ] && [ "$BRANCH" != "main" ]; then
  git branch -D "$BRANCH" >/dev/null 2>&1 || warn "could not delete local branch '$BRANCH' (already gone?)"
fi
git fetch origin --prune --quiet 2>/dev/null || warn "git fetch origin failed; primary may be stale"
git worktree prune

# sync-primary is the load-bearing step. Run it AGAINST the primary checkout so
# it never no-ops as "called from a worktree" (which would exit 12 and collide
# with this script's own primary-refusal code).
set +e
( cd "$PRIMARY_TOP" && bash "$SCRIPT_DIR/sync-primary.sh" >/dev/null 2>&1 )
sync_code=$?
set -e

RECOVER=""
[ -n "$BRANCH" ] && [ -n "$TIP" ] && RECOVER=" (recover branch: git branch $BRANCH $TIP)"
if [ "$sync_code" -eq 0 ]; then
  PRIMARY_AT=$(git -C "$PRIMARY_TOP" rev-parse --short main 2>/dev/null || echo "?")
  note "PASS -- removed $WT_PATH${BRANCH:+ (branch $BRANCH @ ${TIP:-?})}; primary main synced to $PRIMARY_AT$RECOVER"
else
  warn "removed $WT_PATH${BRANCH:+ (branch $BRANCH @ ${TIP:-?})} but sync-primary failed (code $sync_code)"
  warn "run 'bash scripts/sync-primary.sh' in $PRIMARY_TOP and reconcile$RECOVER"
  exit 13
fi
exit 0
