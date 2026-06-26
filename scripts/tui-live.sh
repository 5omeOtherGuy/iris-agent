#!/usr/bin/env bash
# tui-live.sh — drive the Iris TUI in a tmux pane and capture the rendered
# surface. The terminal-UI equivalent of browser screenshotting: it launches a
# real `iris` binary in a tmux pane, types tasks into it, waits until the screen
# settles, and prints the captured pane (plain text, or with ANSI colour).
#
# Use it to eyeball transcript / tool-panel / composer rendering against
# docs/TUI_DESIGN_LANGUAGE.md when a golden test would be awkward (live colour,
# the working-indicator chase, a full streamed tool turn). It does NOT replace
# the golden/snapshot tests in src/ui/tui*; it complements them. See
# docs/TUI_LIVE_TESTING.md.
#
# Subcommands:
#   start [opts]                 build (unless --no-build) + launch iris in a pane
#   send "<prompt>" [--ansi] [--tail N]
#                                type a task + Enter, wait until settled, capture
#   shot [--ansi] [--tail N]     capture the pane now
#   keys <tmux-keys…>            send raw tmux keys (e.g. C-c, BSpace, /, Enter)
#   stop                         quit iris (/exit) and free the pane
#
# Options for `start`:
#   --bin <path>   iris binary to run (default: target/debug/iris)
#   --release      build/run target/release/iris (implies the release bin)
#   --no-build     do not cargo build; use the binary as-is
#   --size <WxH>   tmux pane size when creating the session (default 120x40)
#   --pane <id>    drive an EXISTING tmux pane (e.g. %149) instead of a managed
#                  session; `stop` then only sends /exit and never kills it
#
# Target selection (all subcommands):
#   --pane <id> / $IRIS_LIVE_PANE   an existing pane id (external; never killed)
#   -s <name>   / $IRIS_LIVE_SESSION  managed session name (default: iris-live)
#
# Env knobs:
#   IRIS_LIVE_TIMEOUT   max settle polls at 0.4s each (default 150 ≈ 60s)
#
# Exit codes: 0 ok · 64 usage · 20 tmux/git failure · 30 timeout waiting to render

set -euo pipefail

note() { printf 'tui-live: %s\n' "$*"; }
die()  { printf 'tui-live: %s\n' "$*" >&2; exit "${2:-64}"; }

command -v tmux >/dev/null 2>&1 || die "tmux not found on PATH" 20

SESSION="${IRIS_LIVE_SESSION:-iris-live}"
PANE="${IRIS_LIVE_PANE:-}"
ANSI=0
TAIL=""
POLL_MAX="${IRIS_LIVE_TIMEOUT:-150}"

# --- target resolution -------------------------------------------------------
# An explicit pane wins (external, caller-owned). Otherwise the managed session,
# addressed by name so tmux routes to its active pane.
_target() { [ -n "$PANE" ] && printf '%s' "$PANE" || printf '%s' "$SESSION"; }
_exists() {
  if [ -n "$PANE" ]; then tmux list-panes -a -F '#{pane_id}' 2>/dev/null | grep -qx "$PANE"
  else tmux has-session -t "$SESSION" 2>/dev/null; fi
}

# --- capture helpers ---------------------------------------------------------
_raw_shot() {
  local t; t=$(_target)
  if [ "$ANSI" = 1 ]; then tmux capture-pane -t "$t" -e -p; else tmux capture-pane -t "$t" -p; fi
}
# Strip trailing all-blank lines. iris renders inline from the top and does not
# pin the composer to the pane bottom, so a detached pane pads the bottom with
# blank rows that would otherwise swallow `--tail` output.
_strip_trailing_blanks() { awk 'NF{last=NR} {buf[NR]=$0} END{for (i = 1; i <= last; i++) print buf[i]}'; }
_shot() {
  if [ -n "$TAIL" ]; then _raw_shot | _strip_trailing_blanks | tail -n "$TAIL"
  else _raw_shot | _strip_trailing_blanks; fi
}

# Wait until the pane stops changing (stable ~1.6s) AND the working-indicator
# LED chase (●··· / ·●·· / ··●· / ···●) is no longer on screen. The chase is the
# tell that a turn is still streaming; the context-meter dots use ○/● without
# the `·` track, so they never trip this.
_settle() {
  local t prev="" out i stable=0
  t=$(_target)
  for ((i = 1; i <= POLL_MAX; i++)); do
    out=$(tmux capture-pane -t "$t" -p 2>/dev/null || true)
    if printf '%s' "$out" | grep -qE '●·{1,3}|·{1,3}●'; then stable=0; sleep 0.4; continue; fi
    if [ "$out" = "$prev" ]; then stable=$((stable + 1)); else stable=0; fi
    prev="$out"
    ((stable >= 4)) && return 0
    sleep 0.4
  done
  return 0 # settle is best-effort; capture whatever is on screen
}

# Wait for a substring to appear (used after launch).
_wait_for() {
  local needle="$1" t out i
  t=$(_target)
  for ((i = 1; i <= POLL_MAX; i++)); do
    out=$(tmux capture-pane -t "$t" -p 2>/dev/null || true)
    printf '%s' "$out" | grep -qiF "$needle" && return 0
    sleep 0.25
  done
  return 30
}

# --- subcommands -------------------------------------------------------------
cmd_start() {
  local bin="" release=0 build=1 size="120x40"
  while [ $# -gt 0 ]; do
    case "$1" in
      --bin) bin="$2"; shift 2 ;;
      --release) release=1; shift ;;
      --no-build) build=0; shift ;;
      --size) size="$2"; shift 2 ;;
      --pane) PANE="$2"; shift 2 ;;
      -s|--session) SESSION="$2"; shift 2 ;;
      *) die "start: unknown argument: $1" ;;
    esac
  done

  local root; root=$(git rev-parse --show-toplevel 2>/dev/null) || die "not in a git repo" 20
  if [ -z "$bin" ]; then
    local profile_dir="debug"; [ "$release" = 1 ] && profile_dir="release"
    bin="$root/target/$profile_dir/iris"
    if [ "$build" = 1 ]; then
      note "building iris ($profile_dir)…"
      if [ "$release" = 1 ]; then ( cd "$root" && cargo build --bin iris --release ) >/dev/null
      else ( cd "$root" && cargo build --bin iris ) >/dev/null; fi
    fi
  fi
  [ -x "$bin" ] || die "iris binary not found/executable: $bin (try without --no-build)" 20

  if [ -n "$PANE" ]; then
    _exists || die "pane $PANE does not exist" 20
    note "launching $bin in existing pane $PANE"
  else
    tmux has-session -t "$SESSION" 2>/dev/null && die "session '$SESSION' already running; stop it first" 20
    local w="${size%x*}" h="${size#*x}"
    tmux new-session -d -s "$SESSION" -x "$w" -y "$h" || die "could not create tmux session" 20
    # A session created inside an existing tmux server ignores -x/-y unless its
    # window size is pinned manually; force it so captures are the chosen width.
    tmux set-option -t "$SESSION" window-size manual 2>/dev/null || true
    tmux resize-window -t "$SESSION" -x "$w" -y "$h" 2>/dev/null || true
    note "launching $bin in managed session '$SESSION' (${size})"
  fi
  tmux send-keys -t "$(_target)" -l -- "$bin"
  tmux send-keys -t "$(_target)" Enter
  if _wait_for "Give Iris a task"; then note "composer ready"; _shot; else die "iris did not render within timeout" 30; fi
}

cmd_send() {
  local prompt=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --ansi) ANSI=1; shift ;;
      --tail) TAIL="$2"; shift 2 ;;
      --pane) PANE="$2"; shift 2 ;;
      -s|--session) SESSION="$2"; shift 2 ;;
      -*) die "send: unknown option: $1" ;;
      *) prompt="$1"; shift ;;
    esac
  done
  [ -n "$prompt" ] || die "send: missing \"<prompt>\""
  _exists || die "no live pane/session; run 'tui-live.sh start' first" 20
  tmux send-keys -t "$(_target)" -l -- "$prompt"
  tmux send-keys -t "$(_target)" Enter
  _settle
  _shot
}

cmd_shot() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --ansi) ANSI=1; shift ;;
      --tail) TAIL="$2"; shift 2 ;;
      --pane) PANE="$2"; shift 2 ;;
      -s|--session) SESSION="$2"; shift 2 ;;
      *) die "shot: unknown argument: $1" ;;
    esac
  done
  _exists || die "no live pane/session; run 'tui-live.sh start' first" 20
  _shot
}

cmd_keys() {
  # passthrough; allows -s/--pane before the keys
  while [ $# -gt 0 ]; do
    case "$1" in
      --pane) PANE="$2"; shift 2 ;;
      -s|--session) SESSION="$2"; shift 2 ;;
      *) break ;;
    esac
  done
  [ $# -gt 0 ] || die "keys: nothing to send"
  _exists || die "no live pane/session; run 'tui-live.sh start' first" 20
  tmux send-keys -t "$(_target)" "$@"
}

cmd_stop() {
  while [ $# -gt 0 ]; do
    case "$1" in
      --pane) PANE="$2"; shift 2 ;;
      -s|--session) SESSION="$2"; shift 2 ;;
      *) die "stop: unknown argument: $1" ;;
    esac
  done
  _exists || { note "nothing to stop"; return 0; }
  tmux send-keys -t "$(_target)" -l -- "/exit"; tmux send-keys -t "$(_target)" Enter
  if [ -n "$PANE" ]; then
    note "sent /exit to external pane $PANE (left open)"
  else
    sleep 0.5
    tmux kill-session -t "$SESSION" 2>/dev/null || true
    note "stopped managed session '$SESSION'"
  fi
}

usage() { sed -n '2,46p' "$0"; }

[ $# -gt 0 ] || { usage; exit 64; }
sub="$1"; shift
case "$sub" in
  start) cmd_start "$@" ;;
  send)  cmd_send "$@" ;;
  shot)  cmd_shot "$@" ;;
  keys)  cmd_keys "$@" ;;
  stop)  cmd_stop "$@" ;;
  -h|--help|help) usage ;;
  *) die "unknown subcommand: $sub (try --help)" ;;
esac
