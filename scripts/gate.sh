#!/usr/bin/env bash
# Single pre-PR gate: run the required Rust checks (fmt, clippy, test) with one
# combined, quiet-on-success output instead of three verbose command blocks.
# This is the local mirror of .github/workflows/ci.yml so you validate before
# pushing and rarely need to read CI logs.
#
# On success: one summary line. On the first failure: that step's captured
# output is printed and the gate stops with the step's exit code.
#
# Usage: bash scripts/gate.sh
#   -v / --verbose   stream each step's output live instead of buffering

set -uo pipefail

VERBOSE=0
for arg in "$@"; do
  case "$arg" in
    -v|--verbose) VERBOSE=1 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "gate: unknown argument: $arg" >&2; exit 64 ;;
  esac
done

REPO_TOP=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
cd "$REPO_TOP"

run_step() {
  local label="$1"; shift
  if [ "$VERBOSE" = "1" ]; then
    printf 'gate: %s...\n' "$label" >&2
    "$@"
    return $?
  fi
  local out code lines
  out=$(mktemp)
  # Capture the exit code immediately; do not test it inside an `if` first or a
  # failed condition with no `else` yields $? == 0 and masks the real failure.
  "$@" >"$out" 2>&1
  code=$?
  if [ "$code" -eq 0 ]; then
    rm -f "$out"
    return 0
  fi
  lines=$(wc -l <"$out" | tr -d ' ')
  printf 'gate: %s FAILED (exit %d)\n' "$label" "$code" >&2
  if [ "$lines" -gt 200 ]; then
    tail -n 200 "$out" >&2
    printf 'gate: (showed last 200 of %s lines; full log: %s)\n' "$lines" "$out" >&2
  else
    cat "$out" >&2
    rm -f "$out"
  fi
  return "$code"
}

run_step "fmt (cargo fmt --all --check)"           cargo fmt --all --check                  || exit $?
run_step "clippy (cargo clippy -D warnings)"       cargo clippy --all-targets -- -D warnings || exit $?
run_step "test (cargo test --locked)"              cargo test --locked                      || exit $?

printf 'gate: PASS — fmt OK, clippy OK, test OK\n'
exit 0
