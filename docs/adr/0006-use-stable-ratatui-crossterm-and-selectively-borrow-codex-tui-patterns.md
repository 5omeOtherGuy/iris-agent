# ADR-0006: Use stable Ratatui/Crossterm and selectively borrow Codex TUI patterns

**Date**: 2026-06-17
**Status**: accepted; terminal-lifecycle rule amended by [ADR-0029](0029-adopt-alt-screen-pager-tui.md) (the alt-screen pager mode uses ratatui `Terminal`; this ADR remains binding for the inline fallback mode)
**Deciders**: Iris maintainers, Pi agent session

## Context

Iris needs a full-screen terminal UI, but Nexus must remain independent of rendering and terminal input ownership. Codex has a mature TUI, but its implementation is large and coupled to Codex internals and forked terminal dependencies. Iris already has a smaller UI seam and should not import app-scale Codex architecture just to get terminal rendering.

## Decision

We build an Iris-native TUI on stable crates.io Ratatui and Crossterm. We selectively borrow UI patterns from Codex, such as transcript layout and markdown/code rendering ideas, but do not copy its fork-heavy TUI stack wholesale.

Ratatui is a UI-primitives dependency, not the production terminal driver. Iris owns the terminal surface lifecycle in `src/ui/terminal_surface.rs`: previous rendered lines, synchronized output, append/diff updates, and full replay from `Screen` state on resize or unsafe shrink. Production TUI code does not delegate terminal lifecycle to Ratatui `Terminal`, `Viewport::Inline`, `Terminal::draw`, `Terminal::insert_before`, or an alternate screen.

## Alternatives Considered

### Copy Codex TUI wholesale
- **Pros**: Mature UI behavior and many solved details.
- **Cons**: Large coupled codebase, forked dependencies, and app-server/auth/protocol assumptions that Iris does not have.
- **Why not**: Too much architecture for Iris's current scale.

### Keep only the text REPL
- **Pros**: Minimal code and fewer terminal edge cases.
- **Cons**: Poor tool approval, transcript, and diff readability as Iris grows.
- **Why not**: A usable local coding agent needs a better terminal interaction surface.

### Use Codex's forked Ratatui/Crossterm dependencies
- **Pros**: Closer parity with Codex UI behavior.
- **Cons**: Unstable dependency surface and unnecessary coupling.
- **Why not**: Stable crates.io releases are sufficient for Iris's TUI needs.

### Delegate the live surface to Ratatui `Terminal` inline viewports
- **Pros**: Less Iris-owned terminal diff code.
- **Cons**: Inline viewport construction fixes lifecycle policy too early, couples scrollback commits to Ratatui backend behavior, and makes resize replay depend on terminal probing/viewport internals instead of Iris state.
- **Why not**: Iris needs resize and transcript replay to be explicit, testable, and owned by the Tier-3 TUI seam.

## Consequences

### Positive
- Keeps UI code sized to Iris.
- Avoids vendoring Codex internals and forked dependencies.
- Preserves the Nexus-to-UI seam.
- Keeps transcript replay in Iris state, so resize redraws are coherent without asking Ratatui to recover hidden viewport state.

### Negative
- Iris must reimplement some terminal polish itself.
- Codex UI behavior cannot be copied mechanically.
- Iris now owns terminal line-diff correctness for the TUI surface.

### Risks
- Terminal input/rendering can leak back into Nexus; mitigate by keeping rendering behind UI events and approval seams.
- Terminal escape-sequence bugs can corrupt the visible surface; mitigate with narrow `TerminalSurface` tests for first render, append, diff, resize replay, shrink clearing, and width bounds.
