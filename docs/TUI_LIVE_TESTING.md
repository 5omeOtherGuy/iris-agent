# Manual Live TUI Testing

`scripts/tui-live.sh` drives the Iris TUI in a tmux pane and captures the
rendered surface — the terminal-UI equivalent of browser screenshotting. Use it
to eyeball real rendering (live colour, the working-indicator chase, a full
streamed tool turn) against [`TUI_DESIGN_LANGUAGE.md`](TUI_DESIGN_LANGUAGE.md)
when a golden test would be awkward to read.

## When to use this — and when not

**Use it only when you have changed TUI pane rendering**: transcript messages,
tool panels (`EXPLORE`/`SHELL`/`EDIT`/`APPROVAL`), the working indicator, turn
dividers, the composer, or the symbol/colour vocabulary.

**Do not run it otherwise.** It is a manual, opt-in spot check, not part of the
routine task loop. Work that does not touch pane rendering — runtime, providers,
tools, auth, session storage, docs — has no reason to start a live session, and
it is deliberately absent from `scripts/gate.sh` and from any hook. The
automated guarantee is the golden/snapshot suite in `src/ui/tui*`; this script
complements it, it does not replace it. If you changed rendering, add or update a
golden test first, then use this to confirm it looks right in a real terminal.

## Prerequisites

- `tmux` on `PATH`.
- A build of `iris` (the script builds `target/debug/iris` by default).

## Quick start

```bash
# Build + launch iris in a managed tmux session, print the idle composer:
bash scripts/tui-live.sh start

# Type a task, wait until the turn settles, capture the result:
bash scripts/tui-live.sh send "Using only your ls tool, list the src directory."

# Re-capture the current pane, with ANSI colour, last 20 lines:
bash scripts/tui-live.sh shot --ansi --tail 20

# Send raw keys (slash menu, cancel, backspace, …):
bash scripts/tui-live.sh keys /
bash scripts/tui-live.sh keys C-c

# Quit iris and free the pane:
bash scripts/tui-live.sh stop
```

## How it works

- **Managed session vs external pane.** By default the script owns a tmux session
  named `iris-live` (override with `-s <name>` or `IRIS_LIVE_SESSION`). To drive a
  pane you already have open, pass `--pane <id>` (e.g. `%149`) or set
  `IRIS_LIVE_PANE`; `stop` then only sends `/exit` and never kills your pane.
- **Settling.** `send` waits until the pane stops changing *and* the
  working-indicator LED chase (`●··· / ·●·· / ··●· / ···●`) has left the screen,
  so you capture the finished turn, not a mid-stream frame. The context-meter
  dots use `○`/`●` without the `·` track, so they never confuse the detector.
- **Trailing blanks.** iris renders inline from the top and does not pin the
  composer to the pane bottom, so a detached pane is padded with blank rows. The
  script strips trailing blank lines so `--tail` shows real content.
- **Size.** `start --size WxH` (default `120x40`) pins the pane size; helpful for
  reproducing narrow-width wrapping.
- **Colour.** Add `--ansi` to any capture to keep escape sequences (verify the
  sparse accent palette); omit it for clean, diff-friendly plain text.

## Relationship to the golden tests

The snapshot tests in `src/ui/tui*` are the source of truth and the CI gate.
This harness is for the human-in-the-loop check the snapshots can't give you:
does it actually look right, in colour, in motion, in a real terminal. Prefer to
encode anything you confirm here as a golden test so the next change can't
regress it silently.
