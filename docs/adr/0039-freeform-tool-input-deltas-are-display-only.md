# ADR-0039: Freeform tool-input deltas are display-only

**Date**: 2026-07-04
**Status**: accepted (event seam + display-only invariant implemented; live
preview UI deferred until a freeform tool exists)
**Deciders**: iris-agent maintainers

## Context

`ProviderEvent` is deliberately minimal: `TextDelta | Completed`
(`src/nexus.rs`). Provider adapters buffer tool-call argument fragments
(Anthropic `input_json_delta`, OpenAI `tool_calls[]` deltas) and deliver
complete arguments at turn completion. That was the right contract while every
tool took JSON arguments:

- Partial JSON is unrenderable; there was no consumer for fragments.
- Buffering is what makes mid-stream retries safe: only text deltas are
  forwarded live, so a malformed stream can be retried without duplicating
  user-visible output (documented in the Anthropic adapter).

ADR-0038 introduces `apply_patch` (V4A), Iris's first freeform-text tool. Its
input is line-oriented; the ported streaming parser consumes it incrementally;
and a live patch preview is a stated product requirement. The "demonstrated
need" ADR-0038 gated streaming on now exists (#345).

## Decision

Extend the provider event contract with tool-input deltas, constrained to
display:

- **New provider event** for tool-input fragments, carrying the tool-call id,
  extending the ADR-0020 taxonomy. Nexus forwards it as a typed display event
  (ADR-0025 pattern).
- **Freeform/custom tools only.** Adapters emit deltas for freeform-text tools
  (`apply_patch`); JSON-args tools stay buffered exactly as today. Initially
  only the OpenAI Responses adapter emits the event — freeform/custom tools
  are a Responses API feature, so the chat-completions adapter does not carry
  them; every adapter's parser must tolerate (ignore or emit) the variant.
- **Display-only, non-negotiable.** Execution, approval, and transcript
  encoding consume only the completed, validated arguments. Deltas can never
  change what executes; tampering with or dropping deltas must be provably
  inert to execution.
- **Pre-approval marking.** Live preview renders as *proposed* content; the
  approval prompt still shows the canonical complete diff, and denial leaves
  the tree untouched.
- **Retry invalidation.** On provider stream retry, previously rendered
  partial preview is invalidated/cleared, never duplicated — the existing
  emitted-visible-text retry gating extends to tool-input deltas.
- **Early failure surfacing.** A parse error at line N of a streaming patch is
  shown when it occurs, not after the patch completes.

## Implementation status

Landed as a **minimal, provably-inert seam**, not the full preview:

- `ProviderEvent::ToolInputDelta { call_id, delta }` and its
  `AgentEvent`/`UiEvent` display mirrors exist. Nexus forwards them display-only
  and never writes them to `Agent.messages`, `partial`/assistant text, or the
  assembled turn's tool calls; approval and execution consume only the completed
  canonical `ToolCall`.
- The OpenAI Responses adapter maps `response.custom_tool_call_input.delta`
  (freeform/custom tools) to the event and counts it as visible output, so a
  mid-stream protocol anomaly after a shown fragment is not silently retried.
  JSON-argument (`function`) tool deltas stay buffered.
- **Deferred until `apply_patch` (V4A, ROADMAP #10) lands:** the TUI progressive
  preview state machine (partial / complete / parse-error / invalidated), the
  "proposed" preview marking, the `ToolInputInvalidated` event, and early parse
  failure. Rationale: Iris declares no freeform/custom tool today (every tool is
  `"type": "function"`), so the provider never emits the event in production and
  there is no line-oriented input to render. Building the preview now would be
  speculative UI for a tool that does not exist; the security seam is built and
  tested now so a future freeform tool inherits it. The retry-invalidation
  behavior at the seam is the visible-output retry gate above; the preview-clear
  half of "retry invalidation" is part of the deferred UI.

## Alternatives Considered

### Keep buffering everything (status quo)
- **Pros**: No contract change; retry semantics untouched.
- **Cons**: No live patch preview; long multi-file patches render as dead air
  until `*** End Patch`.
- **Why not**: A consumer now exists and the product wants it.

### Stream JSON-args tools too, with a partial-JSON parser
- **Pros**: Uniform streaming for all tools.
- **Cons**: Lenient partial-JSON parsing is complex and error-prone; no
  current consumer renders partial JSON; enlarges the retry-invalidation
  surface for no benefit.
- **Why not**: Add consumers when a need is demonstrated, as with streaming
  itself.

### Let streamed deltas drive execution (parse-as-you-execute)
- **Pros**: Lowest latency to first file change.
- **Cons**: Executes unapproved, unvalidated input; breaks the approval gate
  and atomic all-or-nothing apply; retry could execute divergent halves.
- **Why not**: Violates the approval-gate safety floor. Display-only is the
  invariant that makes streaming safe to add.

## Consequences

### Positive
- Live patch preview and early parse failure for `apply_patch` with no change
  to execution or approval semantics.
- The event contract grows by one variant with a narrow, testable meaning.
- Future freeform tools inherit streaming for free.

### Negative
- Every provider adapter must handle (at least ignore) the new variant.
- The TUI gains a progressive-preview state machine (partial, complete,
  parse-error, invalidated) that needs snapshot coverage.

### Risks
- Preview/execution divergence: a user approves based on a live preview that
  differs from what executes. Mitigation: approval always shows the canonical
  diff from completed args; the preview is visually marked proposed.
- Duplicate or stale previews across retries. Mitigation: invalidation event
  covered by fake-provider retry tests (#345 DoD).
