# ADR-0029: Adopt an alt-screen pager TUI with an Iris-owned scrollback pane

**Date**: 2026-07-03
**Status**: accepted (amends ADR-0006's terminal-lifecycle rule); ratatui widget/ecosystem adoption boundary defined in [ADR-0033](0033-ratatui-native-adoption-boundary.md)
**Deciders**: operator + agent design review

## Context

The split-statusline redesign requires a session bar that is genuinely pinned
at the top of the pane. Under the current scrollback-append surface
(ADR-0006: Iris-owned inline `terminal_surface.rs`, history flows into native
terminal scrollback, only the bottom volatile tail is repainted), a
viewport-pinned top row is impossible without polluting native scrollback:
every scroll pushes screen row 1 into history, and DECSTBM scroll regions
discard (not archive) scrolled-out lines in xterm/VTE/kitty/alacritty.

Grok Build CLI (binary 0.2.82, verified by runtime escape capture and embedded
doc extraction — see `.iris-reference/grok-pager-dossier.md`) demonstrates the
working alternative: a full-frame **alternate-screen ratatui app** with an
app-owned scrollback pane, stock ratatui cell-diffing, an
`alt_screen = auto|always|never` policy with multiplexer auto-degrade, a
mouse-capture runtime toggle to restore native select/copy, and OSC 52 + tmux
clipboard fallbacks. Iris's state model is already compatible: `Screen`
retains the full transcript (up to `MAX_TRANSCRIPT_ROWS`) and re-renders at
any width, and `TerminalSurface` owns every terminal write, so the render
backend is a swappable seam.

## Decision

Iris adopts a grok-style alt-screen pager as the default rich TUI:

- **Screen-mode policy**: `alt_screen = "auto" | "always" | "never"` (config)
  plus a `--no-alt-screen` CLI flag. `auto` = alt screen on plain terminals
  and normal tmux; inline in tmux control mode, Zellij, and dumb terminals.
  The existing inline terminal-surface renderer is retained as the fallback
  mode, and `--plain` remains the accessible text path.
- **Rendering**: in pager mode, Iris renders full frames from the same logical
  `Screen` state through ratatui `Terminal` (alternate screen, synchronized
  output, cell diffing). Fixed regions: session bar (viewport-pinned, rows
  0–1), scrollback pane (transcript with an Iris-owned scroll offset), working
  indicator, composer. The design language (symbols, palette, hairlines,
  no-box rules) is unchanged.
- **Scrollback pane**: Iris-owned scroll state with follow mode
  (follow-by-overscroll re-engage, follow indicator, anchored folds), per-row
  layout caching with visible-range-only rendering, and keyboard scrolling
  (PageUp/PageDown, line scroll, ends). Native terminal scrollback is not used
  in pager mode.
- **Mouse and clipboard**: mouse capture on by default in pager mode (wheel
  scroll first; in-app text selection is a follow-up slice). A runtime toggle
  (key + slash command) disables mouse reporting to restore terminal-native
  select/copy. Copy paths: native clipboard, OSC 52, tmux buffer fallback.
- **Focus model**: Tab toggles prompt ↔ scrollback focus; typing a printable
  character always returns focus to the prompt; Esc is never a focus or nav
  key (cancel/clear semantics keep priority).
- **ADR-0006 amendment**: ADR-0006's rule that production TUI code never uses
  ratatui `Terminal` or the alternate screen is superseded **for pager mode
  only**. ADR-0006 remains binding for the inline fallback mode, and its
  component/overlay abstractions (ADR-0024) carry over unchanged.

Sequencing, slices, and acceptance criteria live in the roadmap
(Milestone 6 — Alt-Screen Pager TUI) and the implementation plan
(`~/pi/plans/project/alt-screen-pager-tui-2026-07-03.md`).

## Alternatives Considered

### Alternative 1: Keep scrollback-append, pin the session bar in the bottom tail
- **Pros**: Tiny change; preserves native scrollback/selection; no new input
  machinery.
- **Cons**: Session bar not at pane top; spec's design intent unmet; no path
  to in-app scrolling, search, or sticky headers.
- **Why not**: Operator chose the full pager capability set; the bottom-tail
  pin does not generalize.

### Alternative 2: DECSTBM scroll region above a top margin
- **Pros**: True top pin on the primary screen; no full-frame renderer.
- **Cons**: Lines scrolled out of a non-full-screen region are discarded, not
  added to scrollback, in all mainstream terminals — silent history loss.
- **Why not**: Breaks the one guarantee the inline mode exists to give.

### Alternative 3: Repaint a bar at viewport top after every scroll
- **Pros**: No mode switch.
- **Cons**: Every scroll pushes a stale bar row into native scrollback and
  destroys the transcript row underneath it; flicker under load.
- **Why not**: Scrollback pollution is unacceptable and unfixable.

## Consequences

### Positive
- Viewport-pinned session bar (and sticky user-prompt headers later) become
  trivial layout facts.
- In-app scrolling, follow mode, transcript search, and a block fullscreen
  viewer become possible features.
- Rendering simplifies in pager mode: stock ratatui diffing replaces bespoke
  append/diff/replay paths; resize is a re-render, not a replay heuristic.
- The existing `Screen` state model is reused as-is; both modes render the
  same logical document.

### Negative
- Two render backends must be maintained (pager + inline fallback) until the
  inline mode is deliberately reduced to a fallback-only surface.
- Native terminal selection/copy and scrollback are lost while mouse capture
  is on; mitigated by the runtime toggle, OSC 52 copy, and `/copy`/`/export`.
- More input surface: scroll state, focus model, mouse events, per-terminal
  key fallbacks — all must be tested without a TTY (pure state + rendered
  frame assertions).

### Risks
- Terminal-compatibility matrix (tmux control mode, Zellij, screen/byobu,
  macOS Terminal OSC 52, kitty keyboard protocol) — mitigated by the `auto`
  policy, a capability doctor command, and honest degradation notices,
  mirroring grok's shipped behavior.
- Scope creep toward grok's full feature set (block drag, images, dashboards)
  — the roadmap slices gate each addition; the dossier's copy/adapt/skip table
  is the boundary.
- Performance on 10k-row transcripts — mitigated by per-row layout caching and
  visible-range rendering (grok's proven pattern), plus the existing
  `MAX_TRANSCRIPT_ROWS` cap.
