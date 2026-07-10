#!/usr/bin/env bash
# Run the release build for the clean, current primary `main` checkout.
#
# Install with a symlink so this script can continue to locate the repository:
#   ln -s /path/to/iris-agent/scripts/iris-dev.sh ~/.local/bin/iris-dev
#
# The cached executable is rebuilt only when `main` or the Rust toolchain
# changes. Pass --force-refresh as the first argument to rebuild unconditionally.

set -euo pipefail

note() { printf 'iris-dev: %s\n' "$*" >&2; }
fail() { note "$1"; exit "${2:-20}"; }

FORCE=0
if [ "${1:-}" = "--force-refresh" ]; then
  FORCE=1
  shift
fi

# Follow symlinks so ~/.local/bin/iris-dev can point at this repo-owned file.
SOURCE=${BASH_SOURCE[0]}
while [ -L "$SOURCE" ]; do
  SOURCE_DIR=$(cd -P "$(dirname "$SOURCE")" >/dev/null 2>&1 && pwd)
  SOURCE=$(readlink "$SOURCE")
  case "$SOURCE" in
    /*) ;;
    *) SOURCE="$SOURCE_DIR/$SOURCE" ;;
  esac
done
SCRIPT_DIR=$(cd -P "$(dirname "$SOURCE")" >/dev/null 2>&1 && pwd)
REPO_HINT=${IRIS_DEV_REPO:-"$SCRIPT_DIR/.."}

REPO_TOP=$(git -C "$REPO_HINT" rev-parse --show-toplevel 2>/dev/null) \
  || fail "cannot locate the Iris repository; symlink this script or set IRIS_DEV_REPO"
COMMON_DIR=$(git -C "$REPO_TOP" rev-parse --git-common-dir 2>/dev/null) \
  || fail "cannot resolve the repository's shared Git directory"
case "$COMMON_DIR" in
  /*) ;;
  *) COMMON_DIR="$REPO_TOP/$COMMON_DIR" ;;
esac
PRIMARY_TOP=$(cd "$(dirname "$COMMON_DIR")" >/dev/null 2>&1 && pwd -P)
[ -d "$PRIMARY_TOP/.git" ] \
  || fail "cannot resolve the primary checkout from $REPO_TOP"

BRANCH=$(git -C "$PRIMARY_TOP" symbolic-ref --quiet --short HEAD 2>/dev/null || true)
[ "$BRANCH" = "main" ] \
  || fail "primary checkout must be on main (found ${BRANCH:-detached HEAD})" 11

DIRTY=$(git -C "$PRIMARY_TOP" status --porcelain=v1 2>/dev/null) \
  || fail "cannot inspect the primary checkout"
[ -z "$DIRTY" ] \
  || fail "primary checkout is dirty; commit or remove its changes before launching" 10

git -C "$PRIMARY_TOP" show-ref --verify --quiet refs/remotes/origin/main \
  || fail "origin/main is unavailable; run bash scripts/sync-primary.sh" 11
MAIN=$(git -C "$PRIMARY_TOP" rev-parse refs/heads/main)
ORIGIN_MAIN=$(git -C "$PRIMARY_TOP" rev-parse refs/remotes/origin/main)
[ "$MAIN" = "$ORIGIN_MAIN" ] \
  || fail "primary main is stale; run bash scripts/sync-primary.sh" 11

for COMMAND in cargo rustc flock install; do
  command -v "$COMMAND" >/dev/null 2>&1 \
    || fail "required command is unavailable: $COMMAND"
done

RUSTC_VERSION=$(cd "$PRIMARY_TOP" && rustc --version)
CARGO_VERSION=$(cd "$PRIMARY_TOP" && cargo --version)
BUILD_KEY="$MAIN|$RUSTC_VERSION|$CARGO_VERSION|release-locked"
CACHE_DIR=${IRIS_DEV_CACHE_DIR:-"${XDG_CACHE_HOME:-$HOME/.cache}/iris-agent/iris-dev"}
BIN_PATH="$CACHE_DIR/iris"
STAMP_PATH="$CACHE_DIR/build-key"
LOCK_PATH="$CACHE_DIR/build.lock"
mkdir -p "$CACHE_DIR"

# Serialize the stamp check and cache replacement. Recheck after acquiring the
# lock because another launcher may have completed the build while we waited.
exec 9>"$LOCK_PATH"
flock 9
CURRENT_KEY=$(cat "$STAMP_PATH" 2>/dev/null || true)
if [ "$FORCE" = "1" ] || [ ! -x "$BIN_PATH" ] || [ "$CURRENT_KEY" != "$BUILD_KEY" ]; then
  note "building main@$(git -C "$PRIMARY_TOP" rev-parse --short "$MAIN")"
  CARGO_TARGET_DIR="$PRIMARY_TOP/target" \
    cargo build --manifest-path "$PRIMARY_TOP/Cargo.toml" --release --locked --bin iris

  AFTER=$(git -C "$PRIMARY_TOP" rev-parse HEAD)
  AFTER_DIRTY=$(git -C "$PRIMARY_TOP" status --porcelain=v1 2>/dev/null) \
    || fail "cannot verify the primary checkout after building"
  if [ "$AFTER" != "$MAIN" ] || [ -n "$AFTER_DIRTY" ]; then
    fail "primary main changed during the build; retry the launch" 11
  fi

  BUILT_BIN="$PRIMARY_TOP/target/release/iris"
  [ -x "$BUILT_BIN" ] || fail "cargo completed without producing $BUILT_BIN"

  TMP_BIN="$CACHE_DIR/.iris.$$"
  TMP_STAMP="$CACHE_DIR/.build-key.$$"
  trap 'rm -f "$TMP_BIN" "$TMP_STAMP"' EXIT
  install -m 0755 "$BUILT_BIN" "$TMP_BIN"
  mv -f "$TMP_BIN" "$BIN_PATH"
  printf '%s\n' "$BUILD_KEY" >"$TMP_STAMP"
  mv -f "$TMP_STAMP" "$STAMP_PATH"
  trap - EXIT
fi

flock -u 9
exec 9>&-
exec "$BIN_PATH" "$@"
