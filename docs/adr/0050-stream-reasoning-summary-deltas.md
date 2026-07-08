# ADR-0050: Stream reasoning summary deltas as a display event

**Date**: 2026-07-05
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

ADR-0025 added `AgentEvent::AssistantReasoning { text, redacted }`, a block-level
display event emitted once per reasoning block at turn completion. It explicitly
rejected streaming reasoning as deltas because "the provider path assembles
reasoning into blocks at completion; there are no reasoning deltas to forward
here" and said to "revisit if a provider surfaces reasoning deltas."

The OpenAI Codex Responses provider (Iris's only provider today) does surface
them: `response.reasoning_summary_text.delta` carries incremental
human-readable summary text, and `response.reasoning_summary_part.added` marks a
new summary part. These stream before the answer text and do not interleave with
it. Encrypted/redacted reasoning sends no summary deltas (only
`reasoning.encrypted_content`, stored for replay, never shown). This is the
revisit ADR-0025 anticipated.

## Decision

Stream the reasoning **summary** as a display event, in addition to (never
instead of) the persisted reasoning row and the terminal block-level event.

- **Provider-neutral events.** Add `ProviderEvent::ReasoningDelta(String)` /
  `ReasoningSectionBreak` and their `AgentEvent::AssistantReasoningDelta(String)`
  / `AssistantReasoningSectionBreak` display mirrors (and `UiEvent` mirrors). The
  Mimir OpenAI adapter maps `reasoning_summary_text.delta` -> `ReasoningDelta`
  and `reasoning_summary_part.added` -> `ReasoningSectionBreak`. The contract
  degrades to today's block rail for any provider that emits no deltas.
- **Raw reasoning uses a separate channel.**
  `response.reasoning_summary_text.delta` maps to `ReasoningDelta`.
  `response.reasoning_text.delta` maps only to `RawReasoningDelta`, with matching
  Nexus/UI events. Raw deltas are display-only and explicit; they are never
  accumulated into stored reasoning text, assistant text, or continuity replay.
  The completion fallback remains summary-only: `extract_reasoning_text` reads
  the `summary` parts, so raw `content` does not enter storage on that path.
- **Storage is untouched (ADR-0016).** The `Role::AssistantReasoning` row, its
  `continuity`, `redacted` flag, and `ModelOrigin` are written exactly as before,
  exactly once. Emission stays additive.
- **Suppress the duplicate terminal display event.** When a turn streams its
  summary live (`saw_reasoning_delta`), Nexus suppresses the terminal
  `AssistantReasoning` display event for the non-redacted summary so the
  front-end does not show the finished thinking block twice. Redacted blocks are
  never streamed, so their placeholder is always emitted; storage is unchanged
  either way. (Assumes the provider's one-visible-summary-per-turn shape; a
  regression test covers the streamed-summary + redacted-block case.)
- **Retry safety.** A forwarded reasoning summary delta is visible output, so it
  disables silent retry of a mid-stream protocol anomaly exactly like an answer
  text delta (`emitted_visible_output`), preventing a replay from duplicating
  shown reasoning.
- **Rendering owned by the TUI.** Reasoning summaries stream before the answer,
  and the transient preview can only render below committed rows, so the live
  trace is committed as a thinking block on the first non-reasoning event (an
  idempotent guard covering the first answer delta, tool, completion, cancel, or
  error). The block then renders above the answer that streams afterwards. The
  non-interactive text fallback ignores the streamed events, as it does the
  block event.

## Alternatives Considered

### Keep block-level only (ADR-0025 status quo)
- **Pros**: No new events; no cross-tier change.
- **Cons**: No live thinking; reasoning appears only after the model finished
  thinking, which reads as a stall on long reasoning turns.
- **Why not**: The provider now surfaces the deltas ADR-0025 said to revisit.

### Add an explicit `AssistantReasoningEnd` event
- **Pros**: Front-end finalization is explicit, not inferred.
- **Cons**: A third event to route through every tier for a boundary the TUI can
  already detect (any non-reasoning event ends the trace).
- **Why not**: The idempotent finalize guard covers every path with less surface.

### Reuse the summary delta channel for raw reasoning
- **Pros**: Fewer event variants.
- **Cons**: Loses provenance, weakens the summary-safe contract, and can merge
  summary and raw text without a source marker.
- **Why not**: Raw content is larger and more sensitive. It must stay explicit
  through the provider, runtime, and UI contracts.

## Consequences

### Positive
- Live "thinking" on providers that surface reasoning summaries, with graceful
  degradation to the block rail elsewhere.
- ADR-0016 storage/continuity/redaction semantics are unchanged.

### Negative
- Two finalize paths in the TUI: streamed-trace commit (on the first
  non-reasoning event) and the block-level splice fallback.
- A turn may show a streamed summary block and, separately, a redacted
  placeholder block when both occur in one turn.

### Risks
- The terminal-event suppression assumes one visible summary per turn (true for
  the OpenAI Codex provider); a future provider with multiple independent
  visible summaries in one turn would need a per-block match instead of the
  `saw_reasoning_delta` boolean. Covered for the current provider by the
  streamed-summary + redacted-block regression test.
