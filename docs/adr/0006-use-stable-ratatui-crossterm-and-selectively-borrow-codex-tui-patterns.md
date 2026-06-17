# ADR-0006: Use stable Ratatui/Crossterm and selectively borrow Codex TUI patterns

**Date**: 2026-06-17
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Iris needs a full-screen terminal UI, but Nexus must remain independent of rendering and terminal input ownership. Codex has a mature TUI, but its implementation is large and coupled to Codex internals and forked terminal dependencies. Iris already has a smaller UI seam and should not import app-scale Codex architecture just to get terminal rendering.

## Decision

We build an Iris-native TUI on stable crates.io Ratatui and Crossterm. We selectively borrow UI patterns from Codex, such as transcript layout and markdown/code rendering ideas, but do not copy its fork-heavy TUI stack wholesale.

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

## Consequences

### Positive
- Keeps UI code sized to Iris.
- Avoids vendoring Codex internals and forked dependencies.
- Preserves the Nexus-to-UI seam.

### Negative
- Iris must reimplement some terminal polish itself.
- Codex UI behavior cannot be copied mechanically.

### Risks
- Terminal input/rendering can leak back into Nexus; mitigate by keeping rendering behind UI events and approval seams.
