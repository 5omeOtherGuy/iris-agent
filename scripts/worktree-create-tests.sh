#!/usr/bin/env bash
set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
WRAPPER="$ROOT/scripts/worktree-create.sh"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fail() {
  printf 'worktree-create-tests: %s\n' "$*" >&2
  exit 1
}

setup_repo() {
  local fixture="$TMP/$1"
  git init --quiet --bare --initial-branch=main "$fixture/origin.git"
  git clone --quiet "$fixture/origin.git" "$fixture/primary"
  git -C "$fixture/primary" config user.email test@example.invalid
  git -C "$fixture/primary" config user.name test
  mkdir -p "$fixture/primary/.agents/skills/example"
  printf '# Public guidance\n' >"$fixture/primary/AGENTS.md"
  printf '@AGENTS.md\n' >"$fixture/primary/CLAUDE.md"
  printf '%s\n' \
    '/AGENTS.override.md' \
    '/AGENTS.local.md' \
    '/CLAUDE.local.md' \
    '/.pi/APPEND_SYSTEM.md' >"$fixture/primary/.worktreeinclude"
  cat >"$fixture/primary/.agents/skills/example/SKILL.md" <<'EOF'
---
name: example
description: Example skill.
---
Example.
EOF
  cat >"$fixture/primary/.gitignore" <<'EOF'
/AGENTS.override.md
/AGENTS.local.md
/CLAUDE.local.md
/.pi/APPEND_SYSTEM.md
/private.txt
EOF
  git -C "$fixture/primary" add .
  git -C "$fixture/primary" commit --quiet -m initial
  git -C "$fixture/primary" push --quiet -u origin main
  printf '%s\n' "$fixture/primary"
}

expect_refusal() {
  local primary=$1 target=$2 branch=$3
  if (cd "$primary" && bash "$WRAPPER" "$target" "$branch" >/dev/null 2>&1); then
    fail "expected refusal for $branch"
  fi
  if [ -e "$target" ] || [ -L "$target" ]; then
    fail "refusal left target $target"
  fi
  if git -C "$primary" show-ref --verify --quiet "refs/heads/$branch"; then
    fail "refusal left branch $branch"
  fi
}

primary=$(setup_repo success)
printf 'replace public\n' >"$primary/AGENTS.override.md"
printf 'iris local\n' >"$primary/AGENTS.local.md"
printf 'claude local\n' >"$primary/CLAUDE.local.md"
mkdir -p "$primary/.pi"
printf 'pi local\n' >"$primary/.pi/APPEND_SYSTEM.md"
printf 'do not copy\n' >"$primary/private.txt"
target="$TMP/success/worktree with spaces"
(cd "$primary" && bash "$WRAPPER" "$target" test/guidance-copy >/dev/null)
for path in AGENTS.md CLAUDE.md .worktreeinclude .agents/skills/example/SKILL.md; do
  [ -f "$target/$path" ] || fail "tracked file missing from managed plain worktree: $path"
done
for path in AGENTS.override.md AGENTS.local.md CLAUDE.local.md .pi/APPEND_SYSTEM.md; do
  cmp "$primary/$path" "$target/$path" || fail "local instruction was not copied exactly: $path"
done
[ ! -e "$target/private.txt" ] || fail "unsupported ignored file was copied"

plain="$TMP/success/plain"
git -C "$primary" worktree add --quiet -b test/plain "$plain" origin/main
for path in AGENTS.md CLAUDE.md .worktreeinclude .agents/skills/example/SKILL.md; do
  [ -f "$plain/$path" ] || fail "tracked file missing from ordinary Git worktree: $path"
done
for path in AGENTS.override.md AGENTS.local.md CLAUDE.local.md .pi/APPEND_SYSTEM.md; do
  if [ -e "$plain/$path" ] || [ -L "$plain/$path" ]; then
    fail "ordinary Git worktree unexpectedly copied ignored file: $path"
  fi
done

primary=$(setup_repo symlink)
printf 'outside\n' >"$TMP/symlink/outside"
ln -s "$TMP/symlink/outside" "$primary/AGENTS.local.md"
expect_refusal "$primary" "$TMP/symlink/target" test/refuse-symlink

primary=$(setup_repo nonregular)
mkdir -p "$primary/.pi/APPEND_SYSTEM.md"
expect_refusal "$primary" "$TMP/nonregular/target" test/refuse-nonregular

primary=$(setup_repo conflict)
printf 'tracked local\n' >"$primary/AGENTS.local.md"
git -C "$primary" add -f AGENTS.local.md
git -C "$primary" commit --quiet -m conflict
git -C "$primary" push --quiet
expect_refusal "$primary" "$TMP/conflict/target" test/refuse-overwrite

printf 'worktree-create-tests: PASS\n'
