# ADR-0007: Use native trusted tools with read-before-mutate safety

**Date**: 2026-06-17
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Iris exposes filesystem tools directly to the model, so tool behavior is both a product API and a data-loss boundary. Earlier sessions compared shelling out to external tools, content-hash anchored editing, and Claude-style exact-string editing. The current roadmap and code use native Rust tools for read/write/edit/grep/find/ls, with observation-based stale-file protection and atomic writes for mutations.

## Decision

Iris keeps its core filesystem tools native and trusted. `edit` follows the Claude-compatible exact-string contract, mutating tools use atomic writes where applicable, and existing-file mutations require prior observation/freshness checks before writing.

## Alternatives Considered

### Shell out to external `rg`, `fd`, or `ls`
- **Pros**: Less code and mature command behavior.
- **Cons**: External binary dependency, platform variance, and weaker structured metadata/control.
- **Why not**: Native tools keep Iris single-binary-friendly and policy-controlled.

### Use content-hash anchored edits first
- **Pros**: Token-efficient and robust for large edits when implemented well.
- **Cons**: More bespoke model-facing protocol and duplicate/stale-anchor complexity.
- **Why not**: Claude-style exact-string edit is simpler, already familiar to models, and enough for the current milestone.

### Allow blind overwrites of existing files
- **Pros**: Fewer checks.
- **Cons**: Higher data-loss risk when model context is stale.
- **Why not**: Read-before-mutate is a cheap safety rule at the trust boundary.

## Consequences

### Positive
- Tool outputs and metadata are predictable and testable.
- Mutations are safer by default.
- Iris avoids depending on user-installed binaries for core behavior.

### Negative
- Iris maintains more tool code itself.
- Native tools may lag feature-rich external CLIs.

### Risks
- Observation/freshness logic can become inconsistent across tools; mitigate with shared helpers and tests for read/write/edit paths.
- Exact-string edit may be less token-efficient than future anchored edits; keep content-hash anchored edits as a later roadmap item.
