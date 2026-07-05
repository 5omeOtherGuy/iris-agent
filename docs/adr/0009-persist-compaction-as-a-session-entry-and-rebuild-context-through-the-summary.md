# ADR-0009: Persist compaction as a session entry and rebuild context through the summary

**Date**: 2026-06-17
**Status**: accepted; implemented in issues #49 and #55
**Deciders**: Iris maintainers, Pi agent session

## Context

The JSONL session store (ADR-0004) and linear resume now exist. Issue #49 needs the first context-compaction slice: a resumed session should be able to rebuild provider context through a persisted summary instead of replaying every covered turn. It must not break the existing JSONL format or normal resume, and it must leave room for later branch summaries and alternate summarizers without rewriting stored sessions.

Issue #55 adds the first production trigger on that storage shape: the Wayland harness reads the configured context token budget and compacts at safe turn boundaries before the next provider request.

## Decision

Compaction is a durable `compaction` entry in the same JSONL transcript. The entry records an inclusive range of covered `message` entry ids (`coveredFrom`/`coveredTo`), the `summary` text, a `createdAt` timestamp, and a `tokenEstimate`. During read/resume rebuild, each covered range is replaced in place by a single summary message; the original covered turns are not replayed alongside it. Storage and rebuild are independent of how the summary text is produced (a deterministic internal summarizer now; a provider/local/remote summarizer later), so the summarizer is the only swappable part. Overlapping or missing-id coverage is rejected as invalid session data.

The first auto-trigger lives in the Wayland harness. Before a provider request, it compares the current context estimate to `contextTokenBudget`; if over budget, it selects an older persisted/id-bearing range, keeps a recent tail under a low-water target, avoids splitting tool-call/tool-result pairs, appends the compaction entry, and replaces live context with `summary + retained tail` so live and resumed context agree.

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
- Auto-compaction thresholds and token-budget policy attach without rewriting existing entries.
- Resume rebuilds context through the summary; covered turns are never duplicated.
- Reuses the existing JSONL store, ids, and `parentId` chain (ADR-0004); no new dependency.

### Negative
- The current summarizer is deterministic bounded excerpts, not a provider-quality summary.
- Resolved (#375, #377): both resume paths now thread durable per-message entry ids into the harness -- the in-session `/resume` swap (`swap_session`) and the startup path (`Harness::resumed`, `iris --continue` / `iris resume`). A resumed prefix is therefore coverable, so a near-budget resumed session auto-compacts and `/compact`s its loaded history instead of being tracked id-less. Summary positions stay `None`, so a prior summary is never re-covered.

### Risks
- A covered range that splits a tool-call/tool-result pair could leave a dangling half; mitigate by keeping range choice pair-aware in the harness and adding the same validation to any future manual/provider compaction entry point.
- Invalid coverage (overlap/missing id) fails the read; mitigated by rejecting it deterministically and testing compacted rebuild, multiple compactions, overlap, missing-id, resume-after-compaction, and auto-compaction at turn boundaries.
