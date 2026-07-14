# ADR-0054: Use a model-aware auto-compaction trigger ladder

**Date**: 2026-07-10
**Updated**: 2026-07-14
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

Mimir resolves one provider-neutral policy for the active selection: raw model
capacity, displayed capacity, model output limit, Iris output reserve, summary
headroom, and hard application threshold. Iris reserves
`min(model output limit, 20,000)` tokens. Provider profiles are:

- OpenAI Codex: display 95% of raw capacity and apply at 90% of raw capacity;
  summary headroom is the space between that hard threshold and the
  output-reserved ceiling;
- Anthropic: display raw capacity and apply at raw capacity minus the Iris output
  reserve and Claude Code's 13,000-token compaction headroom;
- providers without an authoritative CLI profile: display catalog capacity and
  preserve Iris's 8,192-token summary-headroom fallback. Diagnostics label this
  as fallback policy rather than official behavior.

Wayland evaluates pressure against that policy:

- `warn`: 0.60 of displayed capacity;
- `start`: 0.72 of displayed capacity;
- `hard`: the provider profile's hard threshold.

This removes the former `max(fraction * window, window - buffer)` rule, which
could postpone preparation until close to hard pressure. An explicit hard
fraction overrides the profile threshold. Windows below four summary reserves
use deterministic excerpts only. The retained tail is
`min(keepRecentTokens, displayed capacity / 4)`.

Context measurement uses the last provider-reported total plus local estimates
for messages appended after that response. A context rewrite invalidates the
provider anchor. Usage-blind lanes and resumed sessions use local estimates until
a provider reports usage.

At pre-turn, post-turn, and continuing between-round-trip boundaries:

- `warn` emits `ContextPressure` without mutation;
- `start` starts at most one background job, or applies deterministic excerpts
  when model-backed work is unavailable;
- a completed background summary remains attached to its frozen snapshot and
  does not apply before `hard` or an explicit manual compaction;
- `hard` consumes a ready result, otherwise waits at most `hardWaitMs`, then
  cancels the worker and runs the fallback ladder;
- `maxConsecutiveFailures` opens a model-backed breaker while deterministic
  compaction remains available.

An explicit `contextTokenBudget` is an upper-bound clamp over displayed and hard
capacity. If model metadata is unknown, it is the policy capacity. If both are
absent, the legacy 128,000-token fallback remains. Persisted values, including
the former 235,808-token generated default, remain explicit clamps rather than
being rewritten. Values below the summary reserve are invalid;
`compaction.enabled=false` is the off switch.

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

- Unconfigured sessions use the selected provider/model policy.
- `/context` distinguishes raw capacity, displayed capacity, reserves,
  preparation, hard application, clamp provenance, and fallback authority.
- Model switches recompute the policy before the next request.
- Tiny-window sessions trade summary quality for deterministic progress.
- ADR-0055 supplies the provider-neutral mid-turn governor seam. Outside the
  hard tier, worker start and readiness polling do not wait for model traffic or
  mutate context.
