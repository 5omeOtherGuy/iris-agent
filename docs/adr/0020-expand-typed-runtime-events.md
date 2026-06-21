# ADR-0020: Expand typed runtime events without adopting an event platform

**Date**: 2026-06-21
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Issue #121 asks Iris to adopt the useful part of Flue's rich typed event model for observability, token work, compaction visibility, output-handle visibility, debugging, and future delegation. ADR-0019 already formalized the correlation ids used by those events. Iris must keep the Nexus / Wayland / Iris tier split: Nexus owns provider-neutral runtime facts and observer contracts, Wayland owns harness/session facts, and Iris CLI owns rendering.

Current coverage before this decision:

- Provider turn start was typed through `ProviderTurnStarted` with a Nexus-owned `provider_turn_id`.
- Assistant text deltas/end, final assistant text, tool proposed/started/denied/result/error/output-delta, notices, and turn completion were already present for terminal rendering.
- Auto-compaction only emitted a human notice, even though Wayland knew the compaction id, covered entry ids, covered-message count, token estimates, and budget.
- Large output handle offload was only visible in the model-facing tool-result JSON, not as a typed metadata event.
- Exact provider token/cost usage is not exposed reliably by all current providers; Iris currently has conservative estimates in session/compaction paths only.

## Decision

Iris expands the existing in-process typed stream rather than adding a new event system:

- Nexus emits provider-turn lifecycle events: `ProviderTurnStarted`, `ProviderTurnCompleted`, `ProviderTurnCancelled`, and `ProviderTurnError`, all correlated by `provider_turn_id`.
- Nexus emits metadata-only `ToolLifecycle` events for proposed, approval-requested, approved, denied, started, succeeded, errored, and cancelled states. These events carry `provider_turn_id`, `tool_call_id`, tool name, and state only; existing UI display events remain for compatibility.
- Nexus emits metadata-only `OutputHandleStored` when an oversized successful tool output is stored behind a session-scoped handle. It carries `provider_turn_id`, `tool_call_id`, `output_handle_id`, byte count, and line count, but not the full output or head/tail preview.
- Wayland emits `CompactionApplied` through the same `AgentObserver` seam after durable auto-compaction. It carries `compaction_id`, covered entry-id bounds, covered-message count, original/summary token estimates, and budget, but not the generated summary text.
- Existing text/TUI rendering intentionally ignores the new observability-only events unless a future UI needs them. The old rendering events remain unchanged.

## Redaction and payload rules

Typed observability events must not carry provider request/response payloads, auth headers, secrets, raw tool arguments, full large outputs, or full handle-backed output bodies. Provider error events carry the same boundary error string Iris already surfaces to users; provider adapters remain responsible for sanitizing external diagnostics before they enter Nexus. Output-handle events are metadata-only. Compaction events carry token/coverage metadata only and do not include summary content.

## Alternatives Considered

### Adopt Flue's `observe(...)`, durable streams, or HTTP event API

- **Pros**: Rich replay/export surface and familiar operation stream shape.
- **Cons**: Adds server/runtime architecture Iris does not have.
- **Why not**: Issue #121 needs typed runtime facts, not a daemon, database, durable cursor, or public SDK.

### Replace existing UI events with one new event contract

- **Pros**: Cleaner long-term surface.
- **Cons**: Larger blast radius across terminal rendering and tests.
- **Why not**: Existing UI behavior is stable; additive observability metadata is the smallest useful slice.

### Emit exact provider usage/cost events now

- **Pros**: Better benchmarks.
- **Cons**: Current provider adapters do not expose a uniform reliable exact usage contract.
- **Why not**: Iris uses existing estimates in compaction events and defers exact provider usage until adapters expose it consistently.

## Consequences

### Positive

- Debugging and future token benchmarks can correlate provider turns, tool lifecycle, output handles, and compactions using ADR-0019 ids.
- Terminal UI compatibility is preserved because display events remain unchanged.
- Wayland can surface harness facts through the existing observer seam without moving persistence into Nexus.

### Negative

- There are now parallel display events and observability metadata events for tools.
- Event schema stability is still `pub(crate)`; a future public event export would need an explicit compatibility policy.

### Risks

- Future structured result cleanup (#123) may want to fold some metadata into a richer result contract; current additive events avoid blocking that work.
- Exact token/cost events remain deferred until provider adapters expose reliable usage data.
