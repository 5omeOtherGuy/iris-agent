#!/usr/bin/env bash
# Read-only drift probe. Exits 0 if the primary checkout's local `main`
# equals `refs/remotes/origin/main`, nonzero otherwise. Intended for:
#   - the git pre-commit / pre-push hooks (fatal)
#   - the worktree preflight (verify primary freshness without mutating)
#
# Does NOT run `git fetch`. The caller decides whether the remote ref
# should be refreshed first.
#
# Exit codes:
#   0   primary is at origin/main (or this is a worktree, not the primary)
#   30  primary is behind origin/main
#   31  primary has diverged from origin/main
#   32  primary is ahead of origin/main (uncommon; usually fine, but loud)
#   33  no local main / no origin/main ref to compare against
#   34  not inside a git repository

set -euo pipefail

QUIET=0
for arg in "$@"; do
  case "$arg" in
    -q|--quiet) QUIET=1 ;;
    -h|--help)
      sed -n '2,17p' "$0"
      exit 0
      ;;
  esac
done

emit() {
  if [ "$QUIET" = "0" ]; then
    printf '%s\n' "$*" >&2
  fi
}

if ! REPO_TOP=$(git rev-parse --show-toplevel 2>/dev/null); then
  emit "check-primary-fresh: not inside a git repository"
  exit 34
fi

# In a worktree, .git is a file, not a dir. We only diagnose primary
# drift; worktrees are expected to ride their feature branch.
if [ ! -d "$REPO_TOP/.git" ]; then
  exit 0
fi

if ! git -C "$REPO_TOP" show-ref --verify --quiet refs/heads/main; then
  emit "check-primary-fresh: no local 'main' branch; cannot compare"
  exit 33
fi
if ! git -C "$REPO_TOP" show-ref --verify --quiet refs/remotes/origin/main; then
  emit "check-primary-fresh: no refs/remotes/origin/main; run 'git fetch origin' first"
  exit 33
fi

LOCAL=$(git -C "$REPO_TOP" rev-parse refs/heads/main)
REMOTE=$(git -C "$REPO_TOP" rev-parse refs/remotes/origin/main)

if [ "$LOCAL" = "$REMOTE" ]; then
  exit 0
fi

BEHIND=$(git -C "$REPO_TOP" rev-list --count refs/heads/main..refs/remotes/origin/main)
AHEAD=$(git -C "$REPO_TOP" rev-list --count refs/remotes/origin/main..refs/heads/main)

if [ "$AHEAD" -gt 0 ] && [ "$BEHIND" -gt 0 ]; then
  emit "check-primary-fresh: primary main has diverged from origin/main (ahead $AHEAD, behind $BEHIND)"
  emit "  run 'bash scripts/sync-primary.sh' after reconciling, or fix manually"
  exit 31
fi

if [ "$BEHIND" -gt 0 ]; then
  emit "check-primary-fresh: primary main is BEHIND origin/main by $BEHIND commit(s)"
  emit "  run 'bash scripts/sync-primary.sh' to fast-forward (working tree must be clean)"
  exit 30
fi

if [ "$AHEAD" -gt 0 ]; then
  emit "check-primary-fresh: primary main is ahead of origin/main by $AHEAD commit(s) (unpushed?)"
  exit 32
fi

exit 0
