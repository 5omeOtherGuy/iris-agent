# ADR-0057: Cover the current turn under hard pressure and escalate the fallback ladder

**Date**: 2026-07-10
**Status**: accepted
**Deciders**: Iris maintainers

## Context

The between-round-trip governor (ADR-0055) planned every coverable range with
one turn-respecting rule: when the keep-tail cut lands mid-turn, walk the range
end back to the turn's opening user message so the in-flight turn is never split.

That rule makes auto-compaction go fully inert inside one long agentic turn.
Once every pre-turn message is compacted, the only user message left is the
current turn's opener, so the walk-back collapses every plan to that opener --
a non-shrinking one-message range -- and `plan()` returns `None` for the rest
of the turn. The Start tier then schedules no job, the hard tier's excerpts
ladder breaks on the first `None`, and reactive overflow recovery uses the same
planner. A live stress session accrued ~193k tokens across ~10 pair-closed
boundaries in 93 seconds with zero compactions; the estimate reached 228k on a
~128k effective budget.

A second gap sat under the same hard boundary: when a model-backed subagent
summary times out at the hard wait or fails to shrink, the only fallback was
deterministic excerpts, which collapsed ~59k tokens to ~1k. Provider-native
compaction already existed as a first-class capability seam (ADR-0056) but was
never reached from the subagent fallback path.

## Decision

At the **hard tier only**, the planner skips the turn walk-back so the current
turn's completed content becomes coverable. Every other guard is unchanged in
both modes: the keep-tail loop, the persisted bound `k.min(n)`, entry-id
contiguity, and the pair-safety trims (start skips leading tool fragments; end
backs off so no tool-call/result pair is split). Coverage necessarily includes
the turn's opening user message, because ranges are contiguous and prior
summaries must stay coverable; the subagent summarizer already preserves goals
in a dedicated Goal section, so the opener's intent survives the summary.

Hard mode is wired into exactly the three hard-pressure call sites: the
governor's hard arm (both the newly added hard-mode background scheduling and
the deterministic excerpts backstop) and reactive overflow recovery, which
fires only on a provider context-window failure and is hard by definition.
Start, Warn, and model-requested compaction keep the turn-respecting planner.

When no job is running at the hard boundary and a model worker is available,
the hard arm starts one hard-mode job and resolves it under the existing bounded
hard wait, so a model-backed summary can still win before the deterministic
backstop even after Start could not schedule.

The subagent fallback becomes a three-rung ladder: **subagent -> provider-native
-> deterministic excerpts**. When a subagent summary times out at the hard wait,
fails, or does not shrink, the governor attempts provider-native compaction
first only when `compaction.providerNative=auto`, and otherwise falls through to
excerpts. After that opt-in, capability is read solely through
`ChatProvider::compaction_capability`; no provider name or wire field crosses the tier boundary, and the primary summarizer-mode setting
(excerpts/provider/subagent) is unchanged -- the ladder is only the fallback
chain for the subagent path.

The provider-native rung runs off the loop on its own OS thread (adapters may
use a blocking client) and applies through the same parent-owned path a subagent
summary uses. Blocking at the boundary is accepted because this runs only at
hard pressure; the wait reuses the hard-wait budget and polls the turn token, so
it stays bounded and cancellable. A native fallback success resets the
model-backed circuit breaker exactly like a subagent apply; excerpts keeps
today's failure-counter semantics. The per-turn model-backed compaction cap
still bounds native attempts. Distinct lifecycle messages record which rung
fired.

## Alternatives Considered

### Keep the turn-respecting walk-back at every tier

- **Why not**: It is the root cause -- context runs away unbounded within one
  long turn once pre-turn history is exhausted.

### Cover the current turn at every tier

- **Why not**: Start and Warn have room to wait for a turn boundary; only hard
  pressure justifies rewriting the in-flight turn's completed content.

### Split a tool-call/result pair to cover more

- **Why not**: Pair integrity is a hard invariant (ADR-0048); the property tests
  enforce it. Hard mode changes only the turn walk-back, never the pair trims.

### Keep deterministic excerpts as the sole subagent fallback

- **Why not**: Excerpts are extremely lossy; provider-native compaction already
  exists as a first-class seam and preserves far more at hard pressure.

### Ignore the provider-native opt-in for the fallback rung

- **Why not**: It would invoke native compaction after the operator explicitly
  left it off. The toggle governs every native request; without it the ladder
  moves directly from the portable worker to deterministic excerpts.

## Consequences

- Compaction can land mid-turn, covering current-turn content, so context can
  never run away unbounded within one turn.
- The opening user message of the covered turn is absorbed into the summary;
  goal text survives because the summarizer emits a Goal section.
- The hard boundary may block on one bounded, cancellable provider-native
  request before deterministic relief; non-hard boundaries are unchanged.
- Pair-splitting never occurs (property tests stay green) and live == resumed
  stays byte-exact; compaction failure remains turn-non-fatal.
- Refines the hard-tier behavior of ADR-0055 and reaches the provider-native
  seam of ADR-0056 from the subagent fallback path.
