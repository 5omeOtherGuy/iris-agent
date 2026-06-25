# ADR-0024: Introduce a reusable TUI Component/Container/overlay/focus abstraction

**Date**: 2026-06-26
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

The Tier-3 TUI rendered through three hardcoded paths that each re-implemented
"turn data into `Vec<Line>`" their own way:

- natural-language transcript rows (`tui/pane.rs`, `tui/rows.rs`),
- bordered tool panels (`tui/panel.rs`),
- modal/picker and slash palette, composited only inside the composer chrome by
  `tui/screen.rs::render_editor_chrome`, with input routed by implicit state
  checks scattered through `tui_loop.rs` (`modal_open()`, `palette.is_active`).

Adding a new transcript section, tool renderer, or overlay meant editing the
composition root by hand. pi-mono's `packages/tui/src/tui.ts` solves the same
problem with a small `Component`/`Container` contract, an explicit focus model,
and an overlay stack composited over base content. Iris wants that pluggability
without importing app-scale architecture.

ADR-0006 governs this area: build Iris-native, sized to Iris; Ratatui supplies
primitives; Iris owns the terminal surface (`terminal_surface.rs`). Introducing
a render abstraction is a TUI-architecture decision that touches ADR-0006's
boundary, so it needs its own record.

## Decision

Add one render abstraction in Tier-3 `src/ui/tui/` and run the existing
renderers on it, reimplemented idiomatically in Rust with `Line<'static>` as the
render unit (replacing pi-mono's `string[]`):

- **`tui/component.rs`** — a `Component` trait (`render(&self, width: usize) ->
  Vec<Line<'static>>`), a `Container` that composites ordered children, a
  borrowed `composite()` helper for hot paths, and a `CURSOR_MARKER` /
  `take_cursor_position` cursor-placement seam.
- **`tui/overlay.rs`** — an explicit `FocusTarget` (Editor < Palette < Modal)
  that is the single source of truth for input routing and docked-overlay
  selection, plus the palette's `Component` render face and the docked menu
  paint helper.

Migrations that landed: transcript rows (`TranscriptRow: Component`;
`Transcript::render` composites visible rows via `composite`), the modal
(`Modal: Component`) and the slash palette (`PaletteView: Component`) routed
through `Screen::focus()`, and the composition root assembling the final
document through a root `Container`. `tui_loop.rs` routes input via
`Screen::focus()` instead of ad-hoc state checks.

This amends ADR-0006 rather than overturning it. The abstraction sits ABOVE
`terminal_surface.rs`, which is unchanged and still owns all diff/append/replay.
No new dependencies; no pi-mono/Codex code vendored.

**Divergence recorded: docked overlays, not floating.** pi-mono overlays float
and are composited over the base document at anchor/margin positions. Iris's
overlays are docked: they reserve a region above the composer and the editor
shifts down. We keep the docked model (it is the current, correct visual grammar
in `docs/TUI_DESIGN_LANGUAGE.md`) and defer a true floating anchor/margin
compositor, overlay handles (hide/show/focus/unfocus), and multiple simultaneous
overlays until Iris has a floating UI to justify them.

## Alternatives Considered

### Keep the three fixed paths
- **Pros**: No refactor; zero regression risk.
- **Cons**: Every new section/overlay keeps editing the composition root; no
  shared contract for the deferred full-TUI editor, autocomplete, or theming.
- **Why not**: The duplication is the problem this work exists to remove.

### Full pi-mono `packages/tui` port (component catalog + floating overlays)
- **Pros**: Closest parity; ready-made floating overlay/handle machinery.
- **Cons**: Large surface (image/graphics, autocomplete, kill-ring, undo,
  configurable keybindings, theming), a second terminal-diff engine, likely
  visual drift, and abstraction with no current Iris caller (floating overlays).
- **Why not**: Violates ADR-0006 "sized to Iris" and the no-stub rule; most of
  the catalog is separate roadmap work.

### Model the modal/palette as fake floating overlays
- **Pros**: Surface-matches pi-mono's overlay API.
- **Cons**: Iris overlays reserve layout space; presenting that as floating
  compositing is misleading and risks changing where they render.
- **Why not**: Docked overlays are the honest, byte-identical model.

## Consequences

### Positive
- New transcript sections, tool renderers, and overlays are pluggable behind one
  `Component` trait without touching the composition root.
- Focus is explicit and single-sourced (`Screen::focus()`), replacing scattered
  implicit checks in `tui_loop.rs`.
- Foundation for the deferred full-TUI editor, autocomplete, markdown/status, and
  theming work.
- ADR-0006 preserved: `TerminalSurface` untouched, Ratatui still primitives-only,
  rendering stays in Tier-3.

### Negative
- The root `Container` composes only the viewport-bounded tail (working
  indicator + composer chrome, which holds the docked overlays); the large
  transcript is moved into the document, not cloned, and transcript rows
  composite through `render_into` with no per-row allocation. The remaining
  per-frame copy is small and constant-bounded.
- Two pi-mono hooks (`handleInput`, `invalidate`) and the floating overlay
  compositor are intentionally absent until a real caller exists.

### Risks
- Menu-region byte-identical regressions (height math, inner-rect insets,
  `Paragraph` clipping); mitigated by preserving the exact inset math and by the
  existing modal/palette/resize-replay tests plus new component/overlay tests.
- Focus-precedence drift (Editor < Palette < Modal must hold, and an active
  palette must still suppress global idle chords); covered by `tui_loop` tests.
