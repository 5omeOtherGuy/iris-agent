# ADR-0047: Count and surface a compaction generation ordinal

**Date**: 2026-07-04
**Status**: proposed
**Deciders**: Iris maintainers, Claude agent session

## Context

Iris supports multiple compactions per session (ADR-0009). Confirmed in the current tree:
`plan_compaction` (`src/wayland/mod.rs`) covers only contiguous runs of durable-id `message`
entries and stops at non-id boundaries, so a prior summary is never re-covered. Compactions
cover disjoint original-message ranges and summaries accumulate verbatim; there is no
summary-of-summaries.

Nothing counts compactions. `CompactionApplied` (`src/nexus.rs`) carries `compaction_id`, the
covered range, `covered_messages`, and token estimates, but no generation ordinal, and the
`compaction` entry stores no depth. Three gaps follow: the ADR-0045 benchmark cannot ask
whether retention degrades with compaction depth; long-horizon accumulation of verbatim
summaries has no signal; and a future generational policy has no durable ordinal to key on.

A reverse-engineering pass over the Cursor agent CLI shows it carries a self-summary count in
its per-turn state, fed back across compaction. This ADR adopts the counter, scoped to Iris's
current design.

Tracked in #374.

## Decision

Add a 1-based compaction generation ordinal: the count of prior `compaction` entries in the
session plus one.

- **Surface it on the event.** `CompactionApplied` gains a `generation` field; the Nth
  compaction in a session reports N.
- **Persist it, recompute when absent.** The ordinal attaches to the `compaction` entry as an
  additive optional field (ADR-0009 extension path); older readers ignore it, and a session
  without the field derives the ordinal from the order of its compaction entries.
- **Feed the benchmark.** The ordinal is available to ADR-0045 as an A/B dimension, so
  retention and token delta can be reported by depth.

The ordinal does not change range selection or summary content, and it is not a re-compaction
policy: Iris does not re-summarize prior summaries today. It is instrumentation plus a durable
ordinal that a later generational policy can use.

## Alternatives Considered

### Alternative 1: Derive the count at read time only, do not persist
- **Pros**: No stored field.
- **Cons**: The live `CompactionApplied` event still needs the number without walking the
  transcript; recompute-only serves rebuild but not the event cheaply.
- **Why not**: Persisting is near-zero cost and serves both the event and resume; recompute
  stays the fallback.

### Alternative 2: A full per-turn state block (Cursor's ConversationStateStructure)
- **Pros**: Matches the complete Cursor state shape (todos, plans, mode, subagents, count).
- **Cons**: Most of those fields do not exist in Iris (no plan/todo system, no modes/subagents
  yet); premature. The path/read-state slice already has a home in ADR-0044.
- **Why not**: Ship the one cheap counter; extend when the state exists.

### Alternative 3: Defer until a re-compaction policy needs it
- **Pros**: YAGNI.
- **Cons**: ADR-0045 wants the depth dimension now, and the counter is near-zero cost.
- **Why not**: The instrumentation value is immediate; the policy value is a free option later.

## Consequences

### Positive
- The benchmark (ADR-0045) can report retention and token delta by compaction depth.
- Long-horizon accumulation of verbatim summaries becomes visible.
- A durable ordinal is ready for a future generational policy, at no behavior cost now.

### Negative
- One more additive field on the entry and one more field on the event to thread through.

### Risks
- The recompute-on-resume count must match the live count; mitigate by deriving from ordered
  prior compaction entries and testing resume-continues-count.
- The ordinal could be misread as re-compaction depth; mitigate by documenting that summaries
  are not re-covered today (`plan_compaction`), so the ordinal counts disjoint compactions.
