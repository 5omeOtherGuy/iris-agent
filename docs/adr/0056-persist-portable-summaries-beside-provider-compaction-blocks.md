# ADR-0056: Persist portable summaries beside provider compaction blocks

**Date**: 2026-07-10
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Provider-side compaction can return continuation state that only the producing
adapter understands. Persisting that state alone would make resume depend on
the original provider and model. Flattening it into text alone would discard
the provider optimization. Iris also requires the live and resumed contexts to
remain byte-identical before provider translation.

## Decision

Compaction entries may carry additive `providerBlocks`, but every entry still
carries a self-sufficient text `summary`.

- Nexus exposes only `ProviderCompactionCapability::{None, OpaqueBlocks}` and a
  provider-neutral output containing text, opaque JSON values, and normalized
  usage. Provider names and wire fields remain in Mimir.
- `compaction.providerNative` is global-only, accepts `off|auto`, and defaults
  to `auto`. `auto` attempts a provider route only when the active adapter
  reports capability for the planned range.
- The parent-owned Wayland engine remains the only mutation point. Native and
  portable workers share planning, revalidation, carry, recall, persistence,
  apply, and lifecycle events.
- Entries persist `origin: "providerNative"`, exactly one adapter envelope in
  `providerBlocks`, normalized `workerUsage`, and portable text. Rebuild attaches
  both to the synthetic summary message.
- Mimir replays a block only for its recorded adapter and exact model. A model
  or provider switch ignores it and sends the portable text. Any selection
  change while a native job runs discards that job.
- A native result with empty portable text or other than one opaque block is
  rejected. Deterministic compaction remains available after any native error.

Anthropic's `compact_20260112` context-management edit and
`compact-2026-01-12` beta remain implemented for live probes. The Claude Code
OAuth lane returned `400 invalid_request_error`, so the adapter does not
advertise native capability; `auto` selects the portable provider worker
without paying for a known-failing request. Re-enable advertising only after a
live lane returns one valid block and portable text.

OpenAI Codex uses a `compaction_trigger` input item to obtain one encrypted
compaction item. Because that item is opaque, the same worker makes a separate
Responses request for a portable text summary before returning success. A
native request rejected for a model disables that capability for the process;
the portable provider worker remains available. The two requests share the
normal retry and one-refresh authentication policy, and their reported usage is
combined on the durable compaction entry. OpenAI publishes no minimum input
floor for this route, so the model-aware Wayland ladder owns timing.

For OpenAI native compaction, an ordinary unsupported-feature `400` disables the
model for the process. A classified context-overflow `400` does not: overflow
describes this request size, not a missing model capability.

## Alternatives Considered

### Persist only opaque blocks

- **Why not**: Cross-provider and cross-model resume would lose usable context.

### Persist only text

- **Why not**: Same-provider continuation would discard the optimization this
  slice exists to evaluate.

### Let Mimir mutate live history

- **Why not**: It would create a second mutation owner and break live/resume
  equivalence, recall registration, and entry ordering.

### Require explicit provider-native opt-in

- **Why not**: Native compaction is the established default in the reference
  coding agents. Capability checks and per-process rejection caching preserve a
  safe portable fallback on unsupported lanes.

## Consequences

### Positive

- Native continuity is additive; portable recovery remains authoritative.
- Existing compaction invariants and session readers continue to work.
- Provider capability failures degrade without failing the user turn.

### Negative

- Same-provider requests carry both the native block and portable carry/recall
  text.
- Session entries are larger and contain provider-specific opaque JSON.
- A first `auto` attempt may pay for a rejected capability probe.

### Risks

- Provider beta shapes and supported models may change. Pin request/response
  construction tests and keep live probes double-gated.
- Opaque blocks may contain sensitive continuation state. They stay in the same
  user-owned session log as the original transcript and are never interpreted
  above Mimir.
- A backend can reject a documented feature for a selected lane. Do not
  advertise that model until a live probe passes; cache later rejections for the
  process.
