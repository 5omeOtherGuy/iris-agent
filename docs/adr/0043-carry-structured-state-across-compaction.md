# ADR-0043: Carry structured state across compaction, separate from the prose summary

**Date**: 2026-07-04
**Status**: proposed
**Deciders**: Iris maintainers, Claude agent session

## Context

Compaction (ADR-0009) replaces an inclusive covered range of `message` entries with a single
prose `summary` during context rebuild. The covered originals stay durable in the JSONL
transcript but are replaced in rebuilt context, and ADR-0009's consequences note the harness
tracks resumed history id-less, so covered turns are not id-addressable mid-session after
resume. ADR-0041 upgraded the summarizer to a provider-written structured handoff (goal,
state, decisions, touched files, next steps) that rides the warm cache, with a deterministic
excerpt fallback and a shrink guard.

Every handoff field still lives inside the prose `summary` string. The shrink guard checks
that the summary is smaller than the covered range; it does not check that any specific fact
survived. ADR-0041 records the residual risk verbatim: "a provider summary could omit
load-bearing detail," mitigated only by the handoff prompt, the shrink guard, and the durable
but hidden originals.

Iris has no structured context carry. Confirmed in the current tree:

- Touched/read paths are tracked only in `src/wayland/git_safety/` for checkpoint and
  rollback; they never enter model context.
- `plan` in `src/wayland/mod.rs` is the compaction plan (which range to compact), not an
  agent task list. There is no plan/todo state.

A reverse-engineering pass over the Cursor agent CLI shows it keeps a separate structured
state (read paths, todos/plans, file states, a self-summary count) carried verbatim across
compaction, so the summarizer cannot drop it. This ADR adopts the shape on top of Iris's
durable compaction entry.

Tracked in #371.

## Decision

Persist a small, bounded, deterministic structured carry alongside each `compaction` entry
and render it verbatim into rebuilt context next to the prose summary.

- **Source is the transcript, not the summarizer.** The carry is derived from the covered
  range's structured tool results (ADR-0021), so it cannot be lost to summary prose or a
  provider failure. It is independent of `SummarizerKind` (ADR-0041) and adds no provider
  round-trip.
- **First slice: the touched/read path set.** The workspace-relative paths the agent read or
  mutated within the covered range (read/edit/write/ls targets), deduped, order-stable, and
  bounded to the N most-recent distinct paths so the carry stays token-cheap and cannot grow
  unbounded. It is a guaranteed-retained floor, not a second summary.
- **Storage is an additive field.** The carry attaches to the existing `compaction` JSONL
  entry as an optional field; ADR-0009 already reserves this extension path (fields attach
  without changing the kind). Older readers ignore it; the entry kind and coverage semantics
  are unchanged.
- **Rebuild renders it deterministically.** `read_messages` emits `summary + carry + retained
  tail`. The carry is deterministic text counted in the entry `tokenEstimate`; the shrink
  guard covers summary plus carry.
- **Room to grow.** The field is a structured carry, not a paths-only field, so an active
  plan/todo carry attaches later without a format change once that state exists.

Wayland owns compaction and rebuild; Nexus owns the conversation state and the structured
tool-result contract the carry reads. No provider-specific data enters the carry.

## Alternatives Considered

### Alternative 1: Keep all handoff detail in the prose summary (status quo)
- **Pros**: No format change; one representation of compacted context.
- **Cons**: Load-bearing detail stays at the summarizer's mercy; the shrink guard measures
  size, not retention; ADR-0041 already flags the risk.
- **Why not**: The failure is real and the deterministic floor is cheap.

### Alternative 2: Recompute the carry at rebuild instead of persisting it
- **Pros**: No stored field.
- **Cons**: Couples rebuild to the current tool-history derivation, is non-deterministic
  across code changes, and post-resume history is id-less (ADR-0009), so derivation may be
  incomplete.
- **Why not**: Persisting is deterministic and uses the established additive-field path.

### Alternative 3: Make compacted originals fully recoverable instead (recall tool)
- **Pros**: Nothing is ever lost; the agent can page back into any covered turn.
- **Cons**: Larger surface (a tool plus id-addressable ranges), a per-recall token cost, and
  it depends on the model choosing to recall the right thing.
- **Why not**: Complementary, not a substitute. The carry is the zero-round-trip floor;
  recall (ADR-0045) covers everything below the floor on demand.

### Alternative 4: A full typed agent-state store (plans, file states, subagents, counts)
- **Pros**: Matches the complete Cursor state shape.
- **Cons**: Most of those structures do not exist in Iris yet (no plan/todo system, no
  subagents); premature.
- **Why not**: Ship the one cheap, high-value slice now; extend the same field when the state
  exists.

## Consequences

### Positive
- Load-bearing paths survive any summary, deterministically, with no extra provider
  round-trip.
- Format-compatible with ADR-0009; older sessions rebuild unchanged.
- Gives ADR-0044's benchmark a concrete A/B arm (provider + carry) and a retention-needle
  target.

### Negative
- Adds a structured field plus rebuild rendering, and a second representation of compacted
  context beside the prose summary.
- The path cap is a heuristic; a range that touches many files keeps only the most-recent N.

### Risks
- The carry could duplicate paths the summary already mentions; mitigate by treating it as a
  bounded, deduped floor, not a summary.
- Deriving paths depends on ADR-0021 structured results; mitigate by falling back to an empty
  carry when unavailable (no behavior change).
- Post-resume id-less history may under-derive the covered range; addressed for in-session
  `/resume` by #375 (PR #376) and for startup resume by #377 (sequenced first in the epic).
- Carries accumulate verbatim across compaction generations like summaries do, and paths
  can repeat between generations. Bounded per entry by the cap; the eventual ceiling is the
  deferred superseding-compaction format, not this ADR.
