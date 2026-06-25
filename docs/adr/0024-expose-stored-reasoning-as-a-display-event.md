# ADR-0024: Expose stored reasoning as a display event

**Date**: 2026-06-26
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

ADR-0016 made provider reasoning a flattened transcript row
(`Role::AssistantReasoning`) carrying opaque `continuity`, a `redacted` flag, and
a `ModelOrigin`, so multi-turn thinking + tool-use round trips replay correctly.
That decision was about *storage and provider replay*: reasoning was captured and
persisted, but never surfaced to a front-end. In the runtime loop the captured
reasoning blocks were only pushed onto `self.messages`; no `AgentEvent` was
emitted, so the TUI could not show the model's thinking at all.

ADR-0020 expanded typed runtime events but drew a deliberate line: the new
events were *observability metadata only* (tool lifecycle, output handles,
compaction), explicitly carrying no payloads/secrets, and "existing text/TUI
rendering intentionally ignores the new observability-only events unless a future
UI needs them." Reasoning display was out of that scope.

Bringing assistant-content rendering toward pi-mono parity now requires a
front-end to render a "thinking" block. The open question is how reasoning
reaches the UI without (a) changing the ADR-0016 storage/continuity/redacted
semantics, (b) altering what is sent to the provider, or (c) violating the
ADR-0020 rule that *observability* events stay payload-free.

## Decision

Add a provider/UI-neutral, **display** event `AgentEvent::AssistantReasoning {
text, redacted }` and its `UiEvent` mirror, emitted at the exact site where
reasoning was previously only stored, **in addition to** (never instead of) the
persisted reasoning row.

- **It is a display event, not an observability event.** It belongs to the same
  family as `AssistantText` (which already carries model output text), not the
  ADR-0020 metadata family. Carrying reasoning text is therefore consistent with
  the existing display-event contract; the ADR-0020 payload-free rule governs
  *observability* events and is unchanged.
- **Block-level, not streamed.** Reasoning is assembled into whole blocks at turn
  completion (`AssistantTurn.reasoning`); the provider stream does not surface
  incremental reasoning deltas on this path. The event is emitted once per block,
  mirroring how reasoning is captured, and is ordered before the assistant text
  event of the same turn (matching the ADR-0016 reasoning-before-text ordering).
- **Storage is untouched.** The persisted `Role::AssistantReasoning` row, its
  `continuity`, `redacted` flag, and `ModelOrigin` are written exactly as before.
  Emission is purely additive; ADR-0016 replay correctness is preserved.
- **Redacted reasoning never leaves Nexus as text.** A `redacted` block is
  emitted with empty `text` and `redacted: true`; the original (opaque) content
  is still stored for replay but is never carried on the event or rendered. The
  front-end shows only that redacted reasoning occurred.
- **Rendering is owned by the TUI.** Nexus emits neutral data; the CLI renders a
  collapsible "thinking" panel (collapsed by default to a `Thinking...` label,
  expandable through the existing panel toggle, dim+italic markdown trace). The
  non-interactive text fallback intentionally ignores the event.
- **Stop reasons stay on the existing path.** Truncation/refusal continue to
  surface through `completion_reason_notice` → `Notice`, and abort/error through
  the cancellation/turn-error paths; no new stop-reason channel is introduced.

## Alternatives Considered

### Reuse `ProviderTurnCompleted.completion_reason`/an observability event for reasoning text
- **Pros**: No new variant.
- **Cons**: Would put model-authored text on an ADR-0020 observability event,
  breaking its payload-free contract.
- **Why not**: Reasoning text is display content, not metadata.

### Render reasoning straight from the stored `Role::AssistantReasoning` rows
- **Pros**: No new event.
- **Cons**: Couples Tier-3 rendering to Nexus storage internals and the
  ADR-0016 replay representation; redaction/ordering would have to be re-derived
  in the UI.
- **Why not**: Violates the tier split; the observer event seam is the contract.

### Stream reasoning as deltas
- **Pros**: Live thinking.
- **Cons**: The provider path assembles reasoning into blocks at completion;
  there are no reasoning deltas to forward here.
- **Why not**: Would invent a stream that does not exist; revisit if a provider
  surfaces reasoning deltas.

## Consequences

### Positive
- The TUI can render a thinking block with hide/show without touching storage,
  provider requests, or ADR-0016 continuity.
- Establishes the reasoning display seam that future surfaces can consume.

### Negative
- There is now both a stored reasoning row and a display reasoning event for the
  same blocks (mirroring the existing stored-message / display-event duality).

### Risks
- Redaction correctness depends on the emit-site guard; covered by a test that
  asserts redacted text never reaches the event while the stored row keeps it.
