#!/usr/bin/env bash
# One-shot, quiet worktree preflight. Fetches origin and reports PRIMARY
# checkout freshness in one line, so you know it is safe to branch a new
# worktree off origin/main.
#
# Works correctly from inside a worktree: it resolves the primary checkout
# via `git rev-parse --git-common-dir` and runs the freshness probe there.
# (Running check-primary-fresh.sh from a worktree returns a no-op PASS,
# which would hide a stale primary -- exactly the case that matters in
# parallel sessions.)
#
# On a fresh primary: one PASS line, exit 0.
# On drift: a STOP line plus the exact reconcile command, nonzero exit
# (mirrors check-primary-fresh.sh codes 30/31).
#
# Usage: bash scripts/worktree-preflight.sh
#   --no-fetch   skip `git fetch` (use the already-known remote ref)
#   -v           also print the worktree list

set -euo pipefail

FETCH=1
SHOW_WORKTREES=0
for arg in "$@"; do
  case "$arg" in
    --no-fetch) FETCH=0 ;;
    -v|--verbose) SHOW_WORKTREES=1 ;;
    -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
    *) echo "preflight: unknown argument: $arg" >&2; exit 64 ;;
  esac
done

WT_TOP=$(git rev-parse --show-toplevel 2>/dev/null) || { echo "preflight: not inside a git repository" >&2; exit 20; }
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)

# Resolve the primary checkout = parent of the shared git common dir.
COMMON_DIR=$(git rev-parse --git-common-dir 2>/dev/null || true)
[ -n "$COMMON_DIR" ] || { echo "preflight: cannot resolve git common dir" >&2; exit 20; }
case "$COMMON_DIR" in
  /*) ;;
  *) COMMON_DIR="$WT_TOP/$COMMON_DIR" ;;
esac
PRIMARY_TOP=$(cd "$(dirname "$COMMON_DIR")" && pwd)

if [ "$FETCH" = "1" ]; then
  git fetch origin --prune --quiet 2>/dev/null || {
    echo "preflight: git fetch origin failed" >&2
    exit 20
  }
fi

if [ "$SHOW_WORKTREES" = "1" ]; then
  git worktree list >&2
fi

# Probe the PRIMARY checkout, not the current worktree. Running the probe with
# cwd = primary makes its `git rev-parse --show-toplevel` resolve to a real
# `.git` directory, so it actually compares primary main vs origin/main.
set +e
( cd "$PRIMARY_TOP" && bash "$SCRIPT_DIR/check-primary-fresh.sh" --quiet )
code=$?
set -e

case "$code" in
  0)  echo "preflight: PASS -- primary main == origin/main; clear to create a worktree" ;;
  30) echo "preflight: STOP -- primary behind origin/main; run 'bash scripts/sync-primary.sh' in $PRIMARY_TOP" >&2 ;;
  31) echo "preflight: STOP -- primary diverged from origin/main; reconcile manually in $PRIMARY_TOP" >&2 ;;
  32) echo "preflight: NOTE -- primary ahead of origin/main (unpushed?); usually fine" ;;
  33) echo "preflight: STOP -- missing local main or origin/main ref" >&2 ;;
  *)  echo "preflight: STOP -- primary freshness check failed (code $code)" >&2 ;;
esac

# Codes 0 and 32 are non-blocking; everything else blocks.
case "$code" in
  0|32) exit 0 ;;
  *) exit "$code" ;;
esac
