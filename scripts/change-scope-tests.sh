#!/usr/bin/env bash
set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
CLASSIFIER="$ROOT/scripts/change-scope.sh"

expect_scope() {
  local expected=$1
  shift
  local actual
  actual=$(bash "$CLASSIFIER" --paths "$@")
  if [ "$actual" != "$expected" ]; then
    printf 'change-scope-tests: expected %s for paths:' "$expected" >&2
    printf ' %q' "$@" >&2
    printf '; got %s\n' "$actual" >&2
    exit 1
  fi
}

expect_scope true README.md docs/adr/0062-example.md docs/assets/hero-light.svg
expect_scope true 'docs/example with spaces.md'
expect_scope false
expect_scope false src/lib.rs
expect_scope false README.md src/lib.rs
expect_scope false docs/benchmarks/campaign.toml
expect_scope false .github/workflows/ci.yml

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
git init --quiet -b main "$TMP/repo"
git -C "$TMP/repo" config user.email test@example.invalid
git -C "$TMP/repo" config user.name test
mkdir -p "$TMP/repo/docs"
printf '# readme\n' >"$TMP/repo/README.md"
printf 'fn main() {}\n' >"$TMP/repo/main.rs"
git -C "$TMP/repo" add .
git -C "$TMP/repo" commit --quiet -m initial
git -C "$TMP/repo" branch base
printf '\nMore docs.\n' >>"$TMP/repo/README.md"
[ "$(cd "$TMP/repo" && bash "$CLASSIFIER" base HEAD)" = true ]
printf '// code\n' >>"$TMP/repo/main.rs"
[ "$(cd "$TMP/repo" && bash "$CLASSIFIER" base HEAD)" = false ]

WORKFLOW="$ROOT/.github/workflows/ci.yml"
GATE="$ROOT/scripts/gate.sh"
grep -q 'scripts/change-scope.sh' "$WORKFLOW"
grep -q "needs.scope.outputs.docs-only != 'true'" "$WORKFLOW"
grep -q 'scripts/change-scope.sh' "$GATE"
grep -q 'docs-only' "$GATE"

printf 'change-scope-tests: PASS\n'
