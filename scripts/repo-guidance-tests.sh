#!/usr/bin/env bash
set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
CHECK="$ROOT/scripts/check-repo-guidance.sh"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fail() {
  printf 'repo-guidance-tests: %s\n' "$*" >&2
  exit 1
}

make_valid() {
  local dir=$1
  mkdir -p "$dir/.agents/skills/example" "$dir/.claude/skills"
  printf '# Agent guide\n' >"$dir/AGENTS.md"
  printf '@AGENTS.md\n' >"$dir/CLAUDE.md"
  printf '%s\n' \
    '/AGENTS.override.md' \
    '/AGENTS.local.md' \
    '/CLAUDE.local.md' \
    '/.pi/APPEND_SYSTEM.md' >"$dir/.worktreeinclude"
  cat >"$dir/.agents/skills/example/SKILL.md" <<'EOF'
---
name: example
description: Run the example workflow.
---
Use the example.
EOF
  ln -s ../../.agents/skills/example "$dir/.claude/skills/example"
}

expect_invalid() {
  local dir=$1 label=$2
  if bash "$CHECK" "$dir" >/dev/null 2>&1; then
    fail "checker accepted $label"
  fi
}

bash "$CHECK" "$ROOT" >/dev/null

valid="$TMP/valid"
make_valid "$valid"
bash "$CHECK" "$valid" >/dev/null

bad_name="$TMP/bad-name"
make_valid "$bad_name"
sed -i.bak 's/name: example/name: other/' "$bad_name/.agents/skills/example/SKILL.md"
rm "$bad_name/.agents/skills/example/SKILL.md.bak"
expect_invalid "$bad_name" 'mismatched skill name'

broken="$TMP/broken"
make_valid "$broken"
rm "$broken/.claude/skills/example"
ln -s ../../.agents/skills/missing "$broken/.claude/skills/example"
expect_invalid "$broken" 'broken Claude projection'

pi_copy="$TMP/pi-copy"
make_valid "$pi_copy"
mkdir -p "$pi_copy/.pi/skills/example"
printf 'duplicate\n' >"$pi_copy/.pi/skills/example/SKILL.md"
expect_invalid "$pi_copy" 'redundant Pi skill projection'

bad_import="$TMP/bad-import"
make_valid "$bad_import"
printf '# duplicate instructions\n' >"$bad_import/CLAUDE.md"
expect_invalid "$bad_import" 'non-import Claude guide'

too_long="$TMP/too-long"
make_valid "$too_long"
{
  printf '# Agent guide\n'
  i=0
  while [ "$i" -lt 200 ]; do
    printf -- '- rule %s\n' "$i"
    i=$((i + 1))
  done
} >"$too_long/AGENTS.md"
expect_invalid "$too_long" 'guide over 200 lines'

printf 'repo-guidance-tests: PASS\n'
