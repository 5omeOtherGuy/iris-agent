---
version: alpha
name: Iris TUI
description: >-
  Terminal-native visual system for the Iris coding agent. A calm, precise,
  mechanical instrument in the Teenage Engineering industrial lineage: restrained
  grey foundation, box-drawing chrome only where it earns its place, a small
  symbol vocabulary for state, and a few sparse role-assigned accents. The medium
  is a monospace terminal cell grid, not CSS — color values below are dark-mode
  reference approximations of terminal-relative ANSI roles (see Colors).
colors:
  bg: "#1a1a1f"          # terminal default background (Reset) — adapts to user theme
  surface: "#323238"     # selection / active-row fill — Rgb(50,50,56)
  ink: "#e6e6e6"         # terminal default foreground (Reset) — primary text
  border: "#808080"      # ANSI Gray — panel & composer frames
  muted: "#6b6b6b"       # ANSI DarkGray — dim metadata, hints, hidden-line affordances
  accent: "#d78700"      # "orange": active mode, running, current edge dot — ANSI Yellow slot
  interactive: "#00afaf" # ANSI Cyan — selection, inline code, focus
  link: "#5f87ff"        # ANSI Blue — links
  success: "#5faf5f"     # ANSI Green — DONE / APPROVED / additions
  danger: "#d75f5f"      # ANSI Red — ERROR / DENIED / removals
  add-bg: "#005f00"      # Indexed(22) — diff addition background
  del-bg: "#5f0000"      # Indexed(52) — diff removal background
typography:
  marker:  { family: mono, weight: regular, color: muted,  note: "assistant '›' glyph" }
  heading: { family: mono, weight: bold,    color: ink,    note: "markdown headings in assistant text" }
  body:    { family: mono, weight: regular, color: ink }
  label:   { family: mono, weight: bold,    color: ink,    transform: uppercase, note: "panel headers: SHELL/EXPLORE/EDIT" }
  caption: { family: mono, weight: regular, color: muted,  note: "durations, telemetry, hints" }
  code:    { family: mono, weight: regular, color: interactive, note: "inline code" }
rounded:
  all: "0"               # box-drawing corners are square (┌ ┐ └ ┘). No radius exists in this medium.
spacing:
  unit: "1 terminal cell"
  pane-indent: 2         # cells: tool panels & composer indent from terminal edge
  marker-gap: 2          # cells: assistant marker → text
  panel-pad: 4           # cells: panel body left padding
  block-rhythm: 1        # blank lines between transcript blocks / panels / turns
components:
  panel:           { border: border, header: label, state: "symbol+color", pad: panel-pad }
  composer:        { border: border, status: "top frame", accent: accent, height: ">=5 rows" }
  working:         { style: "inline LED chase", led: accent, framed: false }
  turn-divider:    { rule: muted, separator: "┊", framed: false }
  context-meter:   { dots: 10, filled: muted, edge: accent, empty: muted }
  diff-row:        { add: success, add-bg: add-bg, del: danger, del-bg: del-bg }
  approval:        { review: accent, approved: success, denied: danger }
---

# Iris TUI — Design System

> This file is the impeccable-format summary of the Iris terminal visual system.
> The exhaustive, ground-truth spec is
> [docs/TUI_DESIGN_LANGUAGE.md](docs/TUI_DESIGN_LANGUAGE.md) — pane grammar, every
> tool family, golden-test requirements. Where the two ever disagree, the
> design-language doc wins; update both together. Tokens here are sourced from
> `src/ui/tui.rs` (`BORDER`, `ORANGE`, `GREEN`, `RED`, `DIFF_ADD_BG`,
> `DIFF_DEL_BG`) and the modal/markdown styles in `src/ui/`.

## Overview

Iris should feel like a precise transcript instrument — a refined command-line
interface, not a desktop app recreated in text. The emotional target is the quiet
confidence of well-made industrial equipment: calm, mechanical, honest, and
legible. Plain language flows lightly; tools become mechanical panels; state is
communicated through a small consistent symbol vocabulary; the current operation
is a tiny LED readout; the editor is a calm input module. Nothing else gets chrome.

The register is **product** — the design serves the developer's task. It is built
for experienced power users who want full information on demand but no distraction
by default, so the system is governed by **progressive disclosure**: minimal at a
glance, complete and structured when expanded.

## Colors

The palette is **terminal-relative**, not absolute. Roles are bound to ANSI named
slots so the interface inherits the user's own light or dark terminal theme rather
than imposing one. The hex values in the frontmatter are dark-mode reference
approximations only.

Role mapping (source: `src/ui/tui.rs`, `src/ui/modal.rs`, `src/ui/markdown.rs`):

| Role | ANSI slot | Used for |
| --- | --- | --- |
| `ink` | default fg (Reset) | all primary text |
| `bg` | default bg (Reset) | pane background |
| `border` | Gray | panel + composer frames |
| `muted` | DarkGray | dim metadata, hints, elision affordances |
| `accent` (orange) | Yellow slot | active mode `◉`, running, current edge dot, warnings |
| `interactive` | Cyan | selection highlight, inline code, focused items |
| `link` | Blue | hyperlinks in rendered markdown |
| `success` | Green | `◆ DONE` / `◆ APPROVED` / diff additions |
| `danger` | Red | `■ ERROR` / `■ DENIED` / diff removals |
| `add-bg` | Indexed(22) | diff addition row background |
| `del-bg` | Indexed(52) | diff removal row background |

**Strategy: restrained with a few earned accents.** A muted grey foundation
(border/muted/ink) carries the entire layout; color appears only where it is
signal — active state, success, error, diff, selection, links. There is more than
one accent, but each is role-assigned and sparse. Never color whole panels.
**Never rely on color alone** — every colored state is paired with a symbol and a
label, and the UI must be fully readable in monochrome.

## Typography

One family: the user's terminal **monospace**. There is no size axis, so hierarchy
is built from **weight, dim/bright, color, case, and the symbol/marker column** —
not type scale.

- **Assistant marker** — `›` in `muted`, sitting one column left of its text.
  Subtle, never a state dot.
- **Panel labels** — uppercase tool family (`SHELL`, `EXPLORE`, `EDIT`,
  `APPROVAL`), bold-weight, in `ink`.
- **Body** — `ink`, regular. Wrapped lines align to the transcript text column,
  not the marker.
- **Caption/metadata** — `muted`: compact durations, token telemetry, hints.
- **Inline code** — `interactive` (Cyan); markdown headings render bold in `ink`.

Wrapping is semantic: break at spaces, `/`, `&&`, and token boundaries; never
break identifiers, paths, or decimals unless unavoidable; never overflow a border.

## Layout

A single vertically scrolling transcript column with a fixed multiline composer
pinned at the bottom. No sidebars, no tabs, no bottom status bar.

- **Pane indent** — tool panels and the composer indent ~2 cells from the terminal
  edge so they read as transcript events, not full-width cards. They share one
  width.
- **Transcript grid** — outer margin · assistant marker column · marker-to-text
  gap · transcript text column. User text aligns to the same text column as
  assistant text (no marker, no label).
- **Rhythm** — exactly one blank line between transcript blocks, between adjacent
  panels, around the working indicator, around turn dividers, and between turns.
  Vary nothing else; the calm comes from consistent breathing room.
- **Panel body padding** — 4 cells inside the frame; shell output indents one
  further under its `$ command`.

## Elevation & Depth

Flat by construction — a terminal has no shadows or z-layers in the transcript.
Depth is expressed structurally instead:

- **Chrome as the only elevation cue.** Bordered box-drawing panels (tool output,
  composer) sit "above" the plain transcript; natural language stays unboxed and
  recedes.
- **Background fill** (`surface`, `Rgb(50,50,56)`) marks selection / active rows
  in pickers and modals — the one place a subtle tonal layer is used.
- **Overlays** (modals, pickers, slash menu, login) are the genuine top layer,
  drawn over the pane on demand and dismissed cleanly.

No decorative shadows, no faux-3D, no glow except the optional edge-dot pulse.

## Shapes

- **Square corners, always.** Frames use `┌ ┐ └ ┘ │ ─ ├ ┤`; corner radius does not
  exist in this medium (`rounded.all: 0`). Every bordered row is exactly one of:
  top border, header, separator, body, bottom border — never combined, never
  dangling.
- **Equal-width rows.** Every row of a panel is the same width after pane indent;
  this invariant is golden-tested.
- **Disclosure markers** — `▾` expanded (full output), `▸` collapsed (capped
  preview with an elided-lines affordance). `ctrl+o` toggles the latest foldable
  panel.

## Components

- **Tool panel** — the primary structured primitive. Header
  `│ ▾  TOOL  meta … SYMBOL STATE  ELAPSED │`. Families: `EXPLORE` (container for
  read/grep/list/find — no standalone `READ` panels), `SHELL`, `EDIT` (wrapped
  block diff, `−`/`+` markers, never a `DIFF` family), `APPROVAL`.
- **Composer** — bordered multiline editor, ≥5 rows, indented to panel width. Its
  **top frame is the statusline**: `┌─ ◉ CODE ─ GPT-5.5 XHIGH ─ CTX 300K ●●●○○○○○○○ ─┐`.
  A quiet `~/project ┊ git {branch}` workspace label sits below it. No separate
  status bar anywhere else.
- **Working indicator** — inline LED chase `●···  1:27 ┊ ESC ┊ ↑177k ↓5.7k`. Never
  framed, never a braille spinner, one line, blank line above and below.
- **Turn divider** — quiet unboxed rule after tool-backed turns:
  `── 7.6s ┊ ↑18.2k ↓846 ───`. `┊` separators, compact durations, never `T+`.
- **Context meter** — exactly 10 dots; filled = used (muted), edge dot = `accent`,
  empty = muted. An LED strip, not a monitoring bar; no rainbow coloring.
- **State symbols** — `◉` active mode · `●` live/running LED · `◆` done/approved ·
  `◇` preview/pending · `■` error/denied · `▲` review/warning · `□`
  skipped/cancelled · `○` queued/empty.

## Do's and Don'ts

**Do**
- Keep one transcript column; mark assistant turns with `›`, leave user text plain.
- Give chrome to tool output only; keep conversation unboxed and light.
- Use `EXPLORE` for read/search/list/find; `SHELL` for commands; `EDIT` for
  mutations with wrapped block diffs.
- Use the symbol vocabulary consistently and keep every panel row width-safe.
- Default to minimal; reveal full detail on `ctrl+o` (progressive disclosure).
- Use compact elapsed durations and `┊` separators.

**Don't**
- Render `USER` / `AGENT` labels or box natural-language messages.
- Add a bottom telemetry/status bar (RUN/QUEUE/TOOLS/CPU/MEM/NET).
- Use `●` for every state, or color whole panels, or rely on color alone.
- Create standalone `READ`/`GREP`/`LS` panels, or a framed `WORKING` panel.
- Use braille spinners, `T+` durations, or fixed `HH:MM:SSs` for ordinary calls.
- Recreate a GUI in text — no tabs, sidebars, decorative widgets, or rainbow
  meters.

---

*Strategy of record: [PRODUCT.md](PRODUCT.md). Exhaustive visual spec:
[docs/TUI_DESIGN_LANGUAGE.md](docs/TUI_DESIGN_LANGUAGE.md).*
