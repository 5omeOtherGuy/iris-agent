# ADR-0033: Define the ratatui-native adoption boundary for the TUI

**Date**: 2026-07-04
**Status**: accepted
**Deciders**: operator + agent design review

## Context

A full-TUI review (2026-07-04) inventoried every hand-rolled UI mechanism
against ratatui 0.30 and its ecosystem. The findings split into three groups:
deliberate architecture that outperforms the native alternative, accidental
duplication inside Iris's own code, and genuinely available native/ecosystem
adoptions. Without a recorded boundary, each future TUI change re-litigates
"why not use the ratatui widget" — or worse, adopts one that breaks the
dual-backend rendering model.

The constraint that frames every call: ratatui's interactive widgets
(`List`, `Table`, `Block`, `Clear`, `Scrollbar`) render into a cell
`Buffer`. Iris renders one logical `Screen` into `Vec<Line>` consumed by two
backends — the Iris-owned inline ANSI-stream surface (ADR-0006) and the
alt-screen pager's `Buffer` (ADR-0029). Widget adoption is therefore possible
only where the render path is pager-only, or where the widget is a state/text
primitive rather than a `Buffer` renderer.

## Decision

The boundary, by verdict:

**Keep (hand-rolled is justified and stays):**

- `terminal_surface.rs` inline renderer. Ratatui has no
  append/diff/replay model that preserves native scrollback; its inline
  viewport couples lifecycle to backend probing (rejected in ADR-0006).
  The `Rc<str>` stable-prefix reuse keeps per-frame cost proportional to
  the changed tail, not transcript length.
- `textengine` wrap/width/truncate. Grapheme-safe, URL-token-preserving,
  indent-preserving, ANSI-stripping width — `Paragraph::Wrap` covers none
  of this, and ratatui's docs defer complex wrapping to external crates.
- Markdown renderer (`pulldown-cmark` → spans). Theme injection, box
  tables with proportional column fitting, and width threading exceed
  `tui-markdown`.
- Working-indicator spinner (reduced-motion contract, telemetry-fused
  line) and pager `ScrollState` (follow/overscroll/reveal semantics;
  `tui-scrollview` is archived).

**Consolidate (Iris-internal duplication, not a ratatui gap):**

- One selection-state primitive: `Selector`. The palette and session-menu
  modes migrate off their private index+wrap copies (#320). Ratatui
  `ListState` is not adopted: overlays render as `Vec<Line>` on both
  backends, so a `Buffer` widget cannot own them.
- One width source of truth: `markdown.rs` folds its char-based
  truncation/width math into the grapheme-based engine (#319).

**Adopt (native or ecosystem, at existing seams):**

- Already adopted and staying: `ratatui-textarea` (composer editing/wrap),
  `ansi-to-tui` (live tool-output ANSI → spans), stock ratatui `Terminal`
  cell-diffing for the pager (ADR-0029).
- Syntax highlighting: `syntect` implements the existing
  `HighlightFn` seam in `markdown.rs` (#324). The seam contract does not
  change; the plain text UI stays unhighlighted.
- Hyperlinks are spans-first (#325): link targets travel as structured
  span metadata, never as escape bytes inside strings. OSC 8 is emitted at
  serialization time — directly by `render_line` on the inline surface;
  in pager mode via mouse hit-testing or the cell-splitting workaround
  (ratatui has no native OSC 8; decided in-slice). The plain text UI is
  excluded by its ANSI-free contract. Consequence: the reserved
  string-based `ansi_aware` wrap machinery is deleted (#318) — spans wrap
  before serialization, so escape-aware string wrapping has no caller.

**Explicitly not adopted:**

- `Scrollbar`/`ScrollbarState`: text indicators are the documented design
  language; a change is a design decision (#326), not a refactor.
- `Block`/`Clear`/`Rect::centered*`: overlays are docked, not floating,
  and hand-draw their frame per the design language.
- `Layout` beyond the existing chrome split: `compose_frame`'s
  tail-overflow draining has no `Layout` equivalent; migration is backlog,
  not planned.

## Alternatives Considered

### Alternative 1: Adopt ratatui widgets wholesale, demote inline mode to a degraded fallback now
- **Pros**: `List`/`Table`/`Scrollbar`/`Block` become usable in the primary
  surface; less Iris-owned widget code.
- **Cons**: Forks every overlay into per-backend render paths or degrades
  inline mode ahead of need; ADR-0029 already names fallback-demotion as a
  deliberate future step, not a side effect.
- **Why not**: The dual-backend `Vec<Line>` model is load-bearing today.
  Revisit if inline mode is formally reduced to fallback-only.

### Alternative 2: Keep the string-based `ansi_aware` machinery for hyperlinks
- **Pros**: Already written and tested; mirrors pi-mono's proven approach.
- **Cons**: pi-mono wraps ANSI strings because its pipeline is string-based;
  Iris wraps structured spans before serialization. Keeping it means
  escape-aware width math on every wrap for a path nothing uses.
- **Why not**: Wrong layer for a spans-first architecture; ~550 dead lines
  with a duplicate ANSI scanner already drifting.

### Alternative 3: No recorded boundary; decide per PR
- **Pros**: No doc to maintain.
- **Cons**: The review found exactly the drift this produces (duplicate
  scanners, four selection machines, stale comments contradicting code).
- **Why not**: The boundary is the review's main deliverable.

## Consequences

### Positive
- Every keep/consolidate/adopt call is recorded with its reason; future TUI
  PRs cite this ADR instead of re-deriving the constraint.
- The IDE-grade transcript work (#324, #325) proceeds on defined seams.
- ~550 lines of reserved dead code removed; selection/width duplication has
  a named owner (`Selector`, `textengine`).

### Negative
- Hyperlink capability must be built spans-first rather than reusing the
  deleted string machinery.
- The boundary must be amended if inline mode is demoted to fallback-only
  (that decision flips the widget calculus for overlays).

### Risks
- Pager-mode hyperlinks depend on an undecided mechanism (hit-testing vs
  cell-splitting); mitigated by scoping inline mode first (#325).
- Span-metadata links add a contract to `render_line`; mitigated by the
  no-escapes-in-`Screen`-state invariant being unit-testable.
