# ADR-0054: Use a model-aware auto-compaction trigger ladder

**Date**: 2026-07-10
**Status**: accepted
**Deciders**: Iris maintainers

## Context

Auto-compaction used one conversation-only estimate and an absolute 128,000-token
threshold. It excluded system instructions, tool declarations, and reasoning,
ignored each model's window, and cancelled unfinished background work at the next
pre-turn boundary.

The runtime already receives provider usage and Mimir already owns model facts.
Wayland needs a provider-neutral trigger that uses those facts without moving
provider ids inward.

## Decision

Mimir resolves a numeric context window and maximum-output reserve for the active
selection. Wayland subtracts that reserve and an 8,192-token summary reserve,
then evaluates a three-rung ladder over the effective window:

- `warn`: 0.60;
- `start`: 0.72;
- `hard`: 0.90.

Each threshold is `max(fraction * window, window - buffer)`. Buffers are six,
four, and two summary reserves for `warn`, `start`, and `hard`. These multipliers
make the spec's scaled-buffer rule concrete and keep two summary reserves at the
hard boundary. Windows below four summary reserves use deterministic excerpts
only. The retained tail is `min(keepRecentTokens, window / 4)`.

Context measurement uses the last provider-reported total plus local estimates
for messages appended after that response. A context rewrite invalidates the
provider anchor. Usage-blind lanes and resumed sessions use local estimates until
a provider reports usage.

At pre-turn, post-turn, and continuing between-round-trip boundaries:

- `warn` emits `ContextPressure` without mutation;
- `start` starts one background job, or applies deterministic excerpts when
  model-backed work is unavailable;
- `hard` waits at most `hardWaitMs` for the active job, then cancels it and runs
  the deterministic ladder;
- `maxConsecutiveFailures` opens a model-backed breaker while deterministic
  compaction remains available.

An explicit `contextTokenBudget` clamps the model-derived effective window. If
the model window is unknown, it is the effective window. If both are absent,
the legacy 128,000-token fallback remains. Values below the summary reserve are
invalid; `compaction.enabled=false` is the off switch.

The parent remains the only context mutation point. Automatic compaction errors
emit a notice and never fail the user's turn.

The default retained tail is 8,000 tokens. These values replace the provisional
0.55/0.65/0.85 and 20,000-token defaults after the slice-9 production-seam
benchmark. Over a four-generation deterministic scenario, the new policy used
four rather than six generations, improved average total-context reduction from
48.5% to 58.3%, and improved the shallowest reduction from 41.2% to 54.6% while
preserving both the planted fact and recall-loop hit. A live Haiku probe reduced
the shallowest apply from 7.6% total-context reclamation under the provisional
policy to 49.1%, with two compactions and all protocol gates passing. The
committed evidence and regeneration commands are in
`docs/benchmarks/auto-compaction-v2-tuning.md`.

## Alternatives Considered

### Keep the absolute 128,000-token threshold

- **Why not**: It overflows smaller lanes and compacts large lanes prematurely.

### Estimate the entire prompt locally

- **Why not**: Provider totals already include prompt components that local
  conversation accounting cannot see accurately.

### Wait without a deadline at the hard boundary

- **Why not**: A stalled worker would stall the next user turn.

## Consequences

- Unconfigured sessions use the selected model's effective window.
- `/context` labels measurement provenance, ladder thresholds, and job state.
- Model switches recompute the window before the next request.
- Tiny-window sessions trade summary quality for deterministic progress.
- ADR-0055 supplies the provider-neutral mid-turn governor seam. Outside the
  hard tier, worker start and ready-result drain do not wait for model traffic.
