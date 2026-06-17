# ADR-0009: Persist compaction as a session entry and rebuild context through the summary

**Date**: 2026-06-17
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

The JSONL session store (ADR-0004) and linear resume now exist. Issue #49 needs the first context-compaction slice: a resumed session should be able to rebuild provider context through a persisted summary instead of replaying every covered turn. It must not break the existing JSONL format or normal resume, and it must leave room for later auto-thresholds, token budgets, branch summaries, and alternate summarizers without rewriting stored sessions.

## Decision

Compaction is a durable `compaction` entry in the same JSONL transcript. The entry records an inclusive range of covered `message` entry ids (`coveredFrom`/`coveredTo`), the `summary` text, a `createdAt` timestamp, and a `tokenEstimate` placeholder. During read/resume rebuild, each covered range is replaced in place by a single summary message; the original covered turns are not replayed alongside it. Storage and rebuild are independent of how the summary text is produced (a manual/internal append path now; a provider/local/remote summarizer later), so the summarizer is the only swappable part. Overlapping or missing-id coverage is rejected as invalid session data; the auto-trigger is deferred.

## Alternatives Considered

### Rewrite or drop the covered history in place
- **Pros**: Smallest on-disk footprint; nothing to reconcile at read time.
- **Cons**: Loses the original turns, is not auditable, and is not reversible.
- **Why not**: The transcript is the durable record; compaction is a read-time view, not history destruction.

### Separate compaction store or a Codex-style ThreadStore
- **Pros**: Mature persistence, fork, and thread concepts.
- **Cons**: A second storage system and far more architecture than this milestone needs.
- **Why not**: A single JSONL entry reuses the existing store and ids (ADR-0004); a thread store is out of scope.

### Key coverage on array positions instead of entry ids
- **Pros**: No id lookup during rebuild.
- **Cons**: Positions are not stable across reads (skipped/truncated lines shift them).
- **Why not**: Durable entry ids already exist and give deterministic, stable coverage.

### Couple storage/rebuild to a specific summarizer (e.g. the provider)
- **Pros**: One fewer seam now.
- **Cons**: Locks the format to one summary source; swapping summarizers later means a format change.
- **Why not**: Keeping the produced-summary text opaque to storage is what makes the summarizer exchangeable.

## Consequences

### Positive
- Auto-compaction thresholds, token-budget policy, branch summaries, and alternate summarizers can attach without rewriting existing entries.
- Resume rebuilds context through the summary; covered turns are never duplicated.
- Reuses the existing JSONL store, ids, and `parentId` chain (ADR-0004); no new dependency.

### Negative
- Only a manual/internal trigger exists this slice, so the write path has no production caller yet (one scoped `allow(dead_code)`).
- A `tokenEstimate` placeholder is stored before any token convention exists.

### Risks
- A covered range that splits a tool-call/tool-result pair could leave a dangling half; today the manual path chooses clean boundaries, and a future automatic summarizer must add pair-aware range validation.
- Invalid coverage (overlap/missing id) fails the read; mitigated by rejecting it deterministically and testing compacted rebuild, multiple compactions, overlap, missing-id, and resume-after-compaction.
