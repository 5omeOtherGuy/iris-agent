#!/usr/bin/env bash
set -uo pipefail

is_docs_path() {
  case "$1" in
    AGENTS.md|CLAUDE.md) return 1 ;;
    docs/*.md|openwiki/*.md|showcase/*.md|.github/*.md) return 0 ;;
    */*.md) return 1 ;;
    *.md) return 0 ;;
    docs/assets/*.svg|docs/assets/*.png|docs/assets/*.gif|docs/assets/*.jpg|docs/assets/*.jpeg|docs/assets/*.webp) return 0 ;;
    *) return 1 ;;
  esac
}

classify_paths() {
  [ "$#" -gt 0 ] || { printf 'false\n'; return; }

  local path
  for path in "$@"; do
    if ! is_docs_path "$path"; then
      printf 'false\n'
      return
    fi
  done
  printf 'true\n'
}

if [ "${1:-}" = "--paths" ]; then
  shift
  classify_paths "$@"
  exit 0
fi

base=${1:-origin/main}
head=${2:-HEAD}

# Missing history and an all-zero push base are classified conservatively so
# they run the full gate.
if [ -z "$base" ] || [[ "$base" =~ ^0+$ ]] \
  || ! git rev-parse --verify --quiet "${base}^{commit}" >/dev/null \
  || ! git rev-parse --verify --quiet "${head}^{commit}" >/dev/null; then
  printf 'false\n'
  exit 0
fi

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

if ! git diff --no-renames --name-only -z "$base...$head" >"$tmp"; then
  printf 'false\n'
  exit 0
fi

# Local runs include staged, unstaged, and untracked work. CI passes immutable
# commit SHAs, so its checkout state cannot affect classification.
if [ "$head" = HEAD ]; then
  git diff --cached --no-renames --name-only -z >>"$tmp"
  git diff --no-renames --name-only -z >>"$tmp"
  git ls-files --others --exclude-standard -z >>"$tmp"
fi

paths=()
while IFS= read -r -d '' path; do
  paths+=("$path")
done <"$tmp"
classify_paths "${paths[@]}"
