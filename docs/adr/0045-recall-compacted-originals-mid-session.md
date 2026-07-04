# ADR-0045: Recall compacted originals mid-session

**Date**: 2026-07-04
**Status**: proposed
**Deciders**: Iris maintainers, Claude agent session

## Context

After compaction (ADR-0009) the running agent sees only the summary for a covered range. The
originals remain durable in the JSONL transcript but are replaced in rebuilt context, and
ADR-0009's consequences note the harness tracks resumed history id-less, so only post-resume
turns are re-coverable. Confirmed in the current tree: `rehydrate` exists only for git-safety
checkpoints (`src/wayland/git_safety/`), not conversation turns, and there is no recall tool.
A summary that drops a detail leaves the agent no way to retrieve it short of a full resume or
fork.

Iris already has the pattern for durable-but-hidden content retrieved on demand: oversized
tool outputs are stored behind session-scoped handles and paged in with windowing (ADR-0011).
A reverse-engineering pass over the Cursor agent CLI shows it keeps the summarized originals
and exposes them so the agent can page back into pre-compaction detail mid-session. This ADR
applies Iris's handle discipline to compacted ranges.

Tracked in #373. The read-path id fix shipped for the in-session `/resume` path (#375,
PR #376: `read_messages` and `rebuild_with_compactions` now return per-message entry ids);
the startup resume path remains id-less and is tracked in #377, sequenced first.

## Decision

Make each compacted range addressable and retrievable on demand, reusing the handle
discipline (ADR-0011).

- **Address the covered range.** On compaction, register the covered entry-id span as a
  session-scoped handle; the rebuilt summary carries a stable reference to it. The
  `compaction` entry already stores `coveredFrom`/`coveredTo` ids.
- **A read-only recall tool.** Add a `recall` tool (name per `docs/NAMING.md`) that returns
  the original turns for a handle or id span, windowed and bounded (offsets/limits, head/tail
  caps; handle-offload when large, per ADR-0011). It is a native trusted read tool (ADR-0007)
  over the session's own transcript: no workspace path or shell surface, no new safety class,
  no approval gate.
- **Search, not just paging.** The tool takes an optional filter pattern that returns
  matching turns with their entry ids, bounded in count; a windowed read then targets a hit.
  Blind offset-paging of a large covered range is not workable: the model does not know
  where a dropped detail lives, and paging to find it re-inflates context. Cursor ships the
  same shape (search over the history reference); the pi-mmr/AMP history tools model it as
  the retrieval mode.
- **Finish the read-path ids first (#377).** #375 (PR #376) surfaced per-message entry ids
  through rebuild and the in-session `/resume` swap, fixing that path's auto-compaction dead
  zone. The startup path (`Harness::resumed`, seeded `vec![None; persisted]`) still discards
  them; #377 threads the same ids through startup resume (`iris --continue`, `iris resume`),
  retiring ADR-0009's id-less limitation on both paths. Sequenced ahead of the tool,
  independent of recall.
- **Teach the model the affordance.** A system-prompt fragment (ADR-0012) documents what a
  compaction marker is and when to recall; the rebuilt summary's handle reference is the
  anchor. The reference itself is an ADR-0044 needle: it must survive rebuild verbatim, or
  the tool is unreachable exactly when needed.
- **Retrieve, do not un-compact.** Recall returns turns as tool output; it never rewrites live
  context or re-inflates the compacted range. Tool-call/tool-result pairs are returned intact.

Nexus owns the tool and its execution contract; Wayland owns the session store the tool reads.
The rebuilt-context path is unchanged except for carrying the handle reference.

## Alternatives Considered

### Alternative 1: Auto re-inflate the covered range when context has room
- **Pros**: Seamless; no tool call.
- **Cons**: Fights compaction's purpose, is non-deterministic, and re-bills the tokens
  compaction just saved.
- **Why not**: Recovery should be bounded and model-invoked, not automatic.

### Alternative 2: Expose the raw JSONL transcript path to the model (history as files)
- **Pros**: Closest to the Cursor "history as files" shape; nothing new to build.
- **Cons**: Leaks storage layout, adds a path/traversal surface, and is unbounded.
- **Why not**: A handle-scoped read tool is safer and reuses ADR-0011's windowing and caps.

### Alternative 3: Keep originals recoverable only via full session resume or fork
- **Pros**: No new tool.
- **Cons**: Heavyweight; loses mid-turn recovery, which is the point.
- **Why not**: The value is retrieving a dropped detail during the running task.

### Alternative 4: Carry the summarized originals inline in the summary
- **Pros**: Matches Cursor's literal summarized-originals field.
- **Cons**: Re-bills the covered tokens if the originals ride in context, defeating
  compaction.
- **Why not**: Iris already keeps originals durably on disk; it needs addressability and
  retrieval, not inline carriage.

## Consequences

### Positive
- Lossy compaction becomes safe: any detail the summary dropped is retrievable on demand.
- Reuses ADR-0011 handle discipline (windowing, caps, offload); read-only, so no new safety
  class.
- Id-addressed and deterministic; retires ADR-0009's id-less resume limitation.

### Negative
- Adds a tool and a read path that resolves ids to original turns, plus the handle
  registration at compaction time.
- Recovery depends on the model choosing to recall; it is an affordance, not a guarantee
  (ADR-0043's carry is the guaranteed floor).

### Risks
- A recall over a huge range could re-inflate context; mitigate with windowing, caps, and
  handle-offload (ADR-0011).
- Recall can loop: recall output inflates context, triggers auto-compaction, the summary
  drops the recalled detail, the model recalls again. Mitigate with the search mode (fetch
  the hit, not the range), bounded windows, and the ADR-0043 carry as the floor that removes
  the most common reason to recall.
- Id mapping after resume must be correct; mitigate with the read-path id fixes (#375
  shipped, #377 pending) and a resume-compact-recall test.
- Returned tool-call/tool-result pairs must stay consistent; mitigate by returning raw turns
  as output rather than re-injecting them as live context.
