#!/usr/bin/env bash
# Validate the tracked public instruction and repository-skill layout.

set -euo pipefail

ROOT=${1:-$(git rev-parse --show-toplevel 2>/dev/null)}
fail() { printf 'check-repo-guidance: %s\n' "$*" >&2; exit 1; }

[ -d "$ROOT" ] || fail "repository root is not a directory: $ROOT"
for path in AGENTS.md CLAUDE.md .worktreeinclude; do
  if [ ! -f "$ROOT/$path" ] || [ -L "$ROOT/$path" ]; then
    fail "$path must be a regular file"
  fi
done

lines=$(wc -l <"$ROOT/AGENTS.md" | tr -d ' ')
bytes=$(wc -c <"$ROOT/AGENTS.md" | tr -d ' ')
[ "$lines" -le 200 ] || fail "AGENTS.md exceeds 200 lines ($lines)"
[ "$bytes" -le 32768 ] || fail "AGENTS.md exceeds 32 KiB ($bytes bytes)"
if grep -Eq '/home/|/Users/|someotherguy|standing rule from ' "$ROOT/AGENTS.md"; then
  fail "AGENTS.md contains machine-specific or private guidance"
fi
[ "$(cat "$ROOT/CLAUDE.md")" = '@AGENTS.md' ] \
  || fail "CLAUDE.md must contain only @AGENTS.md"

expected_include=$(mktemp)
trap 'rm -f "$expected_include"' EXIT
printf '%s\n' \
  '/AGENTS.override.md' \
  '/AGENTS.local.md' \
  '/CLAUDE.local.md' \
  '/.pi/APPEND_SYSTEM.md' >"$expected_include"
cmp -s "$expected_include" "$ROOT/.worktreeinclude" \
  || fail ".worktreeinclude must list only the supported local instruction files"

canonical="$ROOT/.agents/skills"
projections="$ROOT/.claude/skills"
if [ ! -d "$canonical" ] || [ -L "$canonical" ]; then
  fail ".agents/skills must be a regular directory"
fi
if [ ! -d "$projections" ] || [ -L "$projections" ]; then
  fail ".claude/skills must be a regular directory"
fi

skill_count=0
for skill_dir in "$canonical"/*; do
  if [ ! -e "$skill_dir" ] && [ ! -L "$skill_dir" ]; then
    continue
  fi
  if [ ! -d "$skill_dir" ] || [ -L "$skill_dir" ]; then
    fail "canonical skill must be a regular directory: ${skill_dir#"$ROOT/"}"
  fi
  name=${skill_dir##*/}
  [[ "$name" =~ ^[a-z0-9]+(-[a-z0-9]+)*$ ]] \
    || fail "invalid skill directory name: $name"
  skill="$skill_dir/SKILL.md"
  if [ ! -f "$skill" ] || [ -L "$skill" ]; then
    fail "missing regular .agents/skills/$name/SKILL.md"
  fi
  first=$(sed -n '1p' "$skill")
  [ "$first" = '---' ] || fail "$name SKILL.md lacks YAML frontmatter"
  metadata_name=$(awk '
    NR == 1 { next }
    /^---[[:space:]]*$/ { exit }
    /^name:[[:space:]]*/ { sub(/^name:[[:space:]]*/, ""); print; exit }
  ' "$skill")
  description=$(awk '
    NR == 1 { next }
    /^---[[:space:]]*$/ { exit }
    /^description:[[:space:]]*/ { sub(/^description:[[:space:]]*/, ""); print; exit }
  ' "$skill")
  [ "$metadata_name" = "$name" ] \
    || fail "skill directory/name mismatch: $name != ${metadata_name:-missing}"
  [ -n "$description" ] || fail "skill $name is missing description metadata"

  projection="$projections/$name"
  [ -L "$projection" ] || fail "missing Claude projection for skill $name"
  [ "$(readlink "$projection")" = "../../.agents/skills/$name" ] \
    || fail "Claude projection for $name is not the canonical relative link"
  [ -f "$projection/SKILL.md" ] \
    || fail "broken Claude projection for skill $name"
  skill_count=$((skill_count + 1))
done
[ "$skill_count" -gt 0 ] || fail "no canonical repository skills found"

for projection in "$projections"/*; do
  if [ ! -e "$projection" ] && [ ! -L "$projection" ]; then
    continue
  fi
  name=${projection##*/}
  [ -L "$projection" ] || fail "Claude skill projection must be a symlink: $name"
  if [ ! -d "$canonical/$name" ] || [ -L "$canonical/$name" ]; then
    fail "Claude projection has no canonical skill: $name"
  fi
done

if [ -d "$ROOT/.pi/skills" ] \
  && [ -n "$(find "$ROOT/.pi/skills" -mindepth 1 -print -quit 2>/dev/null)" ]; then
  fail "redundant .pi/skills entries are not allowed"
fi

printf 'check-repo-guidance: PASS -- %s canonical skill(s), projections resolve\n' "$skill_count"
