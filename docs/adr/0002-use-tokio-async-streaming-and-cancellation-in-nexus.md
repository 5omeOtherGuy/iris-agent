# ADR-0002: Use Tokio async streaming and cancellation in Nexus

**Date**: 2026-06-17
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Nexus must keep the model stream, tool execution, cancellation, and transcript state valid under Ctrl-C and tool failures. `docs/ROADMAP.md` records that the runtime completion work adopted Codex-style Rust async mechanics while preserving pi-mono's contracts-in/events-out shape. Blocking whole-turn calls were enough for early prototyping but not for a reliable agent runtime.

## Decision

Nexus uses Tokio async streaming, `CancellationToken`, `tokio::select!`, child tool tokens, safe parallel/exclusive tool execution, and synthetic cancelled tool results to preserve transcript validity.

## Alternatives Considered

### Blocking whole-turn provider calls
- **Pros**: Simple control flow.
- **Cons**: Cancellation is delayed and tool/provider races are hard to represent correctly.
- **Why not**: Iris needs responsive interruption and valid transcripts after cancelled turns.

### Custom runtime or bespoke async wrapper
- **Pros**: Could hide async complexity behind local abstractions.
- **Cons**: Reinvents mature Rust runtime behavior.
- **Why not**: Tokio and Codex's runtime patterns already cover the hard parts.

### Copy Codex runtime wholesale
- **Pros**: Mature implementation with broad behavior coverage.
- **Cons**: Pulls in app-scale task, thread, plugin, guardian, and network machinery too early.
- **Why not**: Iris needs Codex-style mechanics, not Codex's full product architecture.

## Consequences

### Positive
- Cancellation can race model streams, approvals, and tool execution.
- Tool results remain transcript-valid even when a turn is interrupted.
- Safe-parallel tools can run concurrently while mutating/gated tools stay exclusive.

### Negative
- Async control flow is more complex than blocking calls.
- Some blocking operations still need careful wrapping or documented ceilings.

### Risks
- Blocking terminal approval or provider idle reads can still delay cancellation; mitigate with focused tests and by keeping known ceilings documented in `docs/ROADMAP.md`.
