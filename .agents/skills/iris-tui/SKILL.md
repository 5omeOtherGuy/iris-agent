---
name: iris-tui
description: Design, implement, review, or debug Iris's terminal UI from the current repository sources. Use for work under src/ui; Screen state; pager or inline rendering; transcript, tool, composer, session-bar, settings, picker, modal, or overlay surfaces; palette, symbols, layout, focus, input, golden frames, and TUI conformance. Read the live design language, ADRs, source, and tests instead of copied design-system snapshots.
---

# Iris TUI

Use the live repository as the design system. This skill intentionally carries no copy of the design language, CSS, React specimens, Rust templates, symbols, or palette. Copies drift and can make a fresh session regress the product.

## Establish the contract

Read the sources that govern the requested surface before editing:

1. Read the applicable sections of `docs/TUI_DESIGN_LANGUAGE.md` for visual and interaction intent.
2. Read `docs/ARCHITECTURE.md` for the Tier-3 UI boundary. For renderer, terminal-lifecycle, screen-mode, or widget changes, also read:
   - `docs/adr/0006-use-stable-ratatui-crossterm-and-selectively-borrow-codex-tui-patterns.md`
   - `docs/adr/0029-adopt-alt-screen-pager-tui.md`
   - `docs/adr/0033-ratatui-native-adoption-boundary.md`
3. Use `docs/CODEMAPS/INDEX.md` to locate owners, then read the live source and nearby tests in full. Trust source over the codemap if they differ.
4. Read `docs/ROADMAP.md` before implementing a broad design-language gap or a new surface. Do not pull future work into the current task.

Apply this precedence:

1. The user's requested behavior and current acceptance criteria.
2. Accepted ADRs and `docs/ARCHITECTURE.md` for ownership and renderer mechanics.
3. `docs/TUI_DESIGN_LANGUAGE.md` for intended appearance and interaction.
4. Live source and tests for the behavior and names implemented now.

The design language may lead the implementation. A difference is evidence to investigate, not permission to rewrite unrelated UI. Report stale documentation or an out-of-scope implementation gap; change only the requested behavior.

## Preserve the current architecture

- Keep terminal UI state, input, rendering, and approval UX in Tier 3. Do not move them into Nexus or Wayland.
- Keep one logical `Screen` model feeding two backends: the alt-screen pager and the inline terminal surface.
- Keep shared UI rendering as Ratatui `Line`/`Span` data. Use `Buffer`-rendering widgets only at a pager-only seam allowed by ADR-0033.
- Preserve inline native scrollback behavior and pager-owned scrollback behavior. Do not make one backend's lifecycle assumptions leak into the other.
- Reuse `src/ui/textengine.rs` for display width, wrapping, truncation, and slicing. Do not add character-count width logic or a second ANSI parser.
- Reuse `src/ui/palette.rs`, `src/ui/theme.rs`, and `src/ui/symbols.rs`. Do not copy color values or glyph literals into renderers.

Start with these owners:

| Concern | Live owner |
|---|---|
| TUI state and document composition | `src/ui/tui.rs`, `src/ui/tui/screen.rs` |
| Input and runtime event routing | `src/ui/tui_loop.rs` |
| Pager frame and scrollback | `src/ui/tui/pager.rs` |
| Inline ANSI surface | `src/ui/terminal_surface.rs` |
| Screen-mode policy | `src/ui/screen_mode.rs` |
| Transcript and tool surfaces | `src/ui/tui/transcript.rs`, `src/ui/tui/tool_render.rs`, `src/ui/tui/panel.rs` |
| Components and docked overlays | `src/ui/tui/component.rs`, `src/ui/tui/overlay.rs`, `src/ui/modal.rs` |
| Settings faceplate | `src/ui/settings_menu.rs` |
| Markdown and text measurement | `src/ui/markdown.rs`, `src/ui/textengine.rs` |
| Visual vocabulary | `src/ui/palette.rs`, `src/ui/theme.rs`, `src/ui/symbols.rs` |

Search before assuming a path or symbol still exists.

## Change workflow

1. Classify the change as design-only, shared logical rendering, pager-only, inline-only, or input/state behavior.
2. Run the narrow existing tests to establish a baseline.
3. Add or update a focused test that fails for the requested behavior. Prefer state assertions for interaction and exact rendered lines or frames for visual contracts.
4. Make the smallest change in the owning module. Reuse existing components, row models, selectors, text helpers, palette roles, and symbols.
5. Check both backends for a shared rendering change. Add backend-specific coverage only when behavior differs by design.
6. Exercise narrow widths, short heights, empty states, long content, Unicode width, running-turn state, and approval/modal focus when the change can affect them.
7. Inspect every golden change. Never refresh broad expected output to hide an unrelated delta.
8. Run the focused tests, then `bash scripts/gate.sh`.

Only after changing pane rendering, optionally inspect it in a real terminal:

```bash
bash scripts/tui-live.sh start
bash scripts/tui-live.sh send "<task>"
bash scripts/tui-live.sh shot --ansi
bash scripts/tui-live.sh stop
```

Follow `docs/TUI_LIVE_TESTING.md`. Live inspection complements tests; it does not replace them. Do not run it for docs, runtime, provider, storage, or other non-rendering work.

## Regression gates

Before finishing, confirm:

- Unrelated transcript, input, approval, and focus behavior is unchanged.
- Shared rendering still works in pager and inline modes.
- Every state remains legible without color and uses the documented symbol vocabulary.
- Width and clipping logic uses the shared text engine and stays within its assigned rows.
- Docked overlays and tool surfaces retain the design-language grammar; no generic widget silently changes framing or spacing.
- Tests cover the behavior, not an implementation accident.
- No stale skill-local design copy or generated template was introduced.
