# TUI Behavior

Iris has a terminal-first interface with a text fallback. The core loop emits
events; UI layers render those events.

## Front-end selection

Interactive TTYs use the TUI. Pipes, CI, `TERM=dumb`, `--plain`,
`IRIS_PLAIN=1`, `NO_COLOR`, or TUI startup failures use the text fallback.

Alt-screen behavior is controlled by `tui.altScreen`, `--no-alt-screen`,
`IRIS_NO_ALT_SCREEN`, and startup environment. Inline mode keeps transcript
output in native scrollback. Pager mode uses a full-frame ratatui surface.

## Rendering

The TUI renders streamed assistant text, reasoning panels where available, tool
lifecycle panels, diff previews, approval prompts, model/settings/login/trust
modals, task and session surfaces, the git console, the directory tree, and
slash-command palettes.

The text fallback prints streamed assistant deltas and final tool lifecycle
lines. It ignores live TUI-only deltas and reports selector/modal commands as
TUI-only.

## Slash palette

The slash registry only exposes backed commands. TUI-only commands open selectors
or modals. Shared command handling keeps TUI and text behavior consistent where a
command is available in both modes.

Pager-only commands include transcript `/find` and `/mouse`. `/git` opens the
git console; `/tree` opens a directory picker that can reference files into the
composer.

## Mid-run input

In the TUI, the composer stays live during a running turn. Enter queues steering
input for the next provider request. Alt+Enter queues a follow-up to inject only
when the agent would otherwise stop. Ctrl-C clears queued input and cancels the
turn.

The text fallback does not support mid-run steering.

## Clipboard

`/copy` writes the last assistant reply through the platform clipboard when
possible. OSC 52 is used as a fallback, including over SSH.

## Debug snapshots

`/debug` writes a debug snapshot of rendered screen state and conversation
context to the Iris debug log.

## Settings

The settings modal edits supported persisted knobs, including model defaults,
reasoning, scoped models, prompt cache retention, microcompaction, bash-tool
mode, max tool round-trips, verification command/attempts, worktree root, theme,
scroll speed, reduced motion, and alt-screen policy. Some changes take effect at
the next safe turn boundary or next session start.
