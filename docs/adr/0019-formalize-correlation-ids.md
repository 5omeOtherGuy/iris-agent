# ADR-0019: Formalize operation/session/turn correlation ids

**Date**: 2026-06-21
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Issue #120 asks Iris to adopt the useful part of Flue's naming discipline for correlation ids without adopting Flue's server harness, URL-addressable agent instances, workflows, durable execution, or persistence adapters. Iris already has stable ids in several places, but the vocabulary was implicit: session ids and entry ids live in Wayland/session storage, provider tool-call ids live in Nexus messages, compaction entries live in the JSONL store, and output handles live beside a session.

## Decision

Iris uses this provider-neutral, UI-neutral vocabulary:

| Id | Owner tier | Lifetime | Persistence surface | Status | Purpose |
|---|---|---|---|---|---|
| `session_id` | Wayland (`SessionLog`) | One JSONL transcript | Session header `id`, file name, resume lookup, logs | Existing, formalized | Resume, audit, sidecar ownership, debugging |
| `message_entry_id` | Wayland (`SessionLog`) | One JSONL entry inside a session | Message entry `id` | Existing, formalized | Stable transcript positions for resume, compaction, future branching |
| `parent_id` | Wayland (`SessionLog`) | Link from an entry to previous leaf | Entry `parentId` | Existing, formalized | Tree-ready history and append-chain audit |
| `provider_turn_id` | Nexus (`Agent`) | One provider/model round trip | `AgentEvent::ProviderTurnStarted`, optional message entry `providerTurnId` | Implemented now | Correlate streamed text, assistant/tool-call messages, tool results, token/debug events |
| `operation_id` | Deferred | One submitted harness operation | None yet | Deferred | Current CLI prompt handling submits exactly one user prompt directly into one Nexus turn, so `session_id` + `provider_turn_id` + entry ids cover today's audit/debug needs. Add this when one user-visible command can contain multiple independent harness operations (for example queued/background operations, visible multi-agent delegation, or a daemon/API run). |
| `tool_call_id` | Provider via Nexus contract | One model-emitted tool call | Message `toolCallId`, tool-result pairing, live output deltas | Existing, formalized | Provider-valid tool result pairing and UI live-cell correlation |
| `compaction_id` | Wayland (`SessionLog`) | One compaction entry | Compaction entry `id` plus `coveredFrom`/`coveredTo` | Existing, formalized | Audit context rebuild and token-budget compaction decisions |
| `output_handle_id` | Wayland (`HandleStore`) | One stored oversized output payload in a session sidecar | Tool-result metadata `outputHandle.id`, `<session>.outputs/<id>.txt` | Existing, formalized | Keep full outputs retrievable while reducing provider context |

`provider_turn_id` is generated only by Nexus because Nexus owns the provider loop. It names a provider/model round trip, not a full session and not a Flue HTTP operation. It is persisted only as an optional message-entry field so old JSONL sessions remain readable. On resume, Nexus continues the local `turn_00000000` sequence after the highest persisted `providerTurnId` it can parse; legacy sessions without the field start at zero.

## Alternatives Considered

### Add a persisted `operation_id` now
- **Pros**: Closer to Flue's Session -> Operation -> Turn hierarchy.
- **Cons**: Today's terminal flow has no distinct operation object; each submitted prompt enters the harness immediately and synchronously.
- **Why not**: It would create a redundant id with no current persistence or debugging use. The future trigger is queued/background/API/delegated operations where one user action is not equivalent to one submitted harness turn.

### Persist turn ids as standalone session entries only
- **Pros**: Avoids adding an optional field to message entries.
- **Cons**: Makes correlation require cross-entry joins and adds entries that do not carry user/model state.
- **Why not**: The useful audit question is which assistant/tool messages came from a provider round trip, so the optional message field is smaller and clearer.

### Adopt Flue's full hierarchy and URL-addressable agent instances
- **Pros**: Rich server-side run vocabulary.
- **Cons**: Violates Iris's terminal-first scope and tier split; imports server/HTTP concepts Iris does not have.
- **Why not**: Issue #120 only needs naming discipline and correlation ids.

## Consequences

### Positive
- Later typed observability (#121), branch-aware history (#122), structured results (#123), token benchmarks, and delegation work can build on stable names.
- Nexus remains UI/persistence neutral: it emits `provider_turn_id`; Wayland decides how to persist it.
- Existing JSONL sessions remain backward-compatible because `providerTurnId` is optional.

### Negative
- `provider_turn_id` is currently session-local (`turn_<hex>`), not globally unique.
- Legacy entries without `providerTurnId` cannot be correlated back to a provider round trip.

### Risks
- If future branch/fork support copies entries, a session-local turn sequence may need branch-aware namespacing or a wider id. That belongs with issue #122, not this slice.
- If event schemas become public, `ProviderTurnStarted` should be promoted into the typed event-stream contract in issue #121 rather than expanded ad hoc here.
