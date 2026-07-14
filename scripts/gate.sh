#!/usr/bin/env bash
# Single pre-PR gate. Documentation-only changes run whitespace validation;
# all other changes run the required Rust and maintenance-script checks. This
# mirrors .github/workflows/ci.yml so you validate before pushing and rarely
# need to read CI logs.
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

# Fast docs-only path; an empty or mixed change set stays on the full gate.
if [ "$(bash scripts/change-scope.sh "${IRIS_GATE_BASE:-origin/main}" HEAD)" = true ]; then
  run_step "docs whitespace (branch)" git diff --check "${IRIS_GATE_BASE:-origin/main}...HEAD" || exit $?
  run_step "docs whitespace (staged)" git diff --cached --check                           || exit $?
  run_step "docs whitespace (working tree)" git diff --check                             || exit $?
  printf 'gate: PASS — documentation-only; whitespace OK (Rust checks skipped)\n'
  exit 0
fi

run_step "fmt (cargo fmt --all --check)"           cargo fmt --all --check                  || exit $?
run_step "clippy (cargo clippy -D warnings)"       cargo clippy --all-targets -- -D warnings || exit $?
run_step "test (cargo test --locked)"              cargo test --locked                      || exit $?
run_step "script tests (change scope)"             bash scripts/change-scope-tests.sh       || exit $?
run_step "script tests (sync primary)"             bash scripts/sync-primary-tests.sh        || exit $?

printf 'gate: PASS — fmt OK, clippy OK, test OK, scripts OK\n'
exit 0
