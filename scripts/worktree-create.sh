#!/usr/bin/env bash
# Create one task worktree from current origin/main, then copy the supported
# ignored project-instruction files from the primary checkout.
#
# Usage: bash scripts/worktree-create.sh <worktree-path> <branch>

set -euo pipefail

note() { printf 'worktree-create: %s\n' "$*"; }
die() { printf 'worktree-create: %s\n' "$*" >&2; exit 64; }

if [ "$#" -ne 2 ]; then
  die "usage: scripts/worktree-create.sh <worktree-path> <branch>"
fi
WT_PATH=$1
BRANCH=$2
case "$WT_PATH" in -*) die "worktree path must not start with '-': $WT_PATH" ;; esac
case "$BRANCH" in -*) die "branch must not start with '-': $BRANCH" ;; esac
git check-ref-format --branch "$BRANCH" >/dev/null 2>&1 \
  || die "invalid branch name: $BRANCH"

REPO_TOP=$(git rev-parse --show-toplevel 2>/dev/null) \
  || die "not inside a git repository"
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
COMMON_DIR=$(git rev-parse --git-common-dir 2>/dev/null) \
  || die "cannot resolve git common dir"
case "$COMMON_DIR" in
  /*) ;;
  *) COMMON_DIR="$REPO_TOP/$COMMON_DIR" ;;
esac
PRIMARY_TOP=$(cd "$(dirname "$COMMON_DIR")" && pwd)

WT_PARENT=$(dirname "$WT_PATH")
WT_NAME=$(basename "$WT_PATH")
if [ "$WT_NAME" = "." ] || [ "$WT_NAME" = ".." ]; then
  die "invalid worktree path: $WT_PATH"
fi
[ -d "$WT_PARENT" ] || die "worktree parent does not exist: $WT_PARENT"
WT_PARENT=$(cd "$WT_PARENT" && pwd)
WT_ABS="$WT_PARENT/$WT_NAME"
if [ -e "$WT_ABS" ] || [ -L "$WT_ABS" ]; then
  die "worktree path already exists: $WT_ABS"
fi
if git show-ref --verify --quiet "refs/heads/$BRANCH"; then
  die "branch already exists: $BRANCH"
fi

bash "$SCRIPT_DIR/worktree-preflight.sh"

INCLUDE_FILE="$PRIMARY_TOP/.worktreeinclude"
if [ ! -f "$INCLUDE_FILE" ] || [ -L "$INCLUDE_FILE" ]; then
  die "missing regular .worktreeinclude in primary checkout"
fi

COPY_PATHS=()
while IFS= read -r entry || [ -n "$entry" ]; do
  entry=${entry%$'\r'}
  case "$entry" in
    ''|'#'*) continue ;;
    /AGENTS.override.md|/AGENTS.local.md|/CLAUDE.local.md|/.pi/APPEND_SYSTEM.md)
      path=${entry#/}
      ;;
    *) die "unsupported .worktreeinclude entry: $entry" ;;
  esac

  source="$PRIMARY_TOP/$path"
  if [ ! -e "$source" ] && [ ! -L "$source" ]; then
    continue
  fi
  if git -C "$PRIMARY_TOP" cat-file -e "origin/main:$path" 2>/dev/null; then
    die "refusing overwrite conflict: origin/main already contains $path"
  fi
  [ ! -L "$source" ] || die "refusing symlink source: $path"
  [ -f "$source" ] || die "refusing non-regular source: $path"
  git -C "$PRIMARY_TOP" check-ignore --quiet -- "$path" \
    || die "supported local source is not ignored: $path"
  COPY_PATHS+=("$path")
done <"$INCLUDE_FILE"

git worktree add "$WT_ABS" -b "$BRANCH" origin/main

for path in "${COPY_PATHS[@]}"; do
  source="$PRIMARY_TOP/$path"
  destination="$WT_ABS/$path"
  if [ -e "$destination" ] || [ -L "$destination" ]; then
    die "refusing overwrite conflict after worktree creation: $path"
  fi
  parent=$(dirname "$destination")
  if [ -L "$parent" ] || { [ -e "$parent" ] && [ ! -d "$parent" ]; }; then
    die "refusing non-directory destination parent: $path"
  fi
  mkdir -p "$parent"
  cp -p "$source" "$destination"
done

note "PASS -- created $WT_ABS on $BRANCH from origin/main; copied ${#COPY_PATHS[@]} local instruction file(s)"
