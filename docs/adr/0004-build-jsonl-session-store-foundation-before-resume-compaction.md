# ADR-0004: Build JSONL session store foundation before resume and compaction

**Date**: 2026-06-17
**Amended**: 2026-07-10
**Status**: accepted; implemented in issue #42
**Deciders**: Iris maintainers, Pi agent session

## Context

At decision time, Iris had best-effort write-only JSONL transcript logging. The harness capability matrix found session persistence was the next major gap versus pi-mono and Codex. Issue #42 defined the smallest useful slice: a JSONL-backed session store with ids, parent links, read/open/list support, and tests.

Outcome: implemented as a v2 JSONL store. `SessionLog` remains the live append handle; `SessionStore` lists sessions and opens one back in message order. Resume, token accounting, compaction entries, and auto-compaction now build on this foundation; branching/tree APIs, labels, fork/rollback, and the in-session resume picker remain deferred.

## Decision

We build a minimal JSONL Session Store Foundation before implementing `/resume`, branching UI, rollback, compaction, plugins, or a Codex-style thread store. The store supports session ids, message ids, `parentId`, create/open/list/read/append behavior, and compatibility with current append logging.

The harness owns write cadence. It appends each completed provider round trip
before the next provider request and records every assigned message id in its
parallel entry-id map. The group ends on an answered assistant response or on
complete assistant-tool-call/tool-result pairs. The after-turn diff remains the
final and error backstop. All writes are best-effort: persistence failure warns
but never fails the user's turn. Nexus exposes only an inert commit-boundary
observer hook; it has no session, JSONL, or entry-id knowledge.

## Alternatives Considered

### Keep write-only `SessionLog`
- **Pros**: No new storage code.
- **Cons**: Cannot support resume, branching, compaction, token accounting, or context reconstruction.
- **Why not**: It blocks the next harness milestones.

### Build full resume, branching, and compaction in one pass
- **Pros**: Delivers the visible feature set sooner.
- **Cons**: Larger blast radius and unclear storage contract.
- **Why not**: The storage foundation should be testable before UI and compaction behavior depends on it.

### Adopt Codex ThreadStore now
- **Pros**: Mature persistence, resume, fork, and thread concepts.
- **Cons**: Too much app-scale architecture for Iris's current milestone.
- **Why not**: Iris needs a small pi-style JSONL foundation first.

### Use a database
- **Pros**: Querying and metadata indexing are easier later.
- **Cons**: Adds dependency and operational complexity.
- **Why not**: JSONL is sufficient, inspectable, and already matches the current transcript direction.

## Consequences

### Positive
- Unlocked resume, token accounting, compaction entries, auto-compaction, and context reconstruction; still leaves room for branching and rollback.
- Keeps local session data simple and human-inspectable.
- Provides a testable contract before adding UI behavior.
- Preserves completed round trips across a mid-turn crash and gives later
  compaction durable coverage ids without splitting tool pairs.

### Negative
- Listing/querying may be less efficient than a database.
- Some migration may be required if future requirements exceed JSONL.
- Long tool loops perform a few extra flushed appends; provider latency dwarfs
  this local write cost.

### Risks
- Broken or truncated JSONL could block session reads; mitigate by preserving valid-prefix behavior and testing partial/trailing corruption where practical.
- Parent/message id choices may constrain future branching; mitigate by following the pi-mono session shape and keeping the format minimal.

## Follow-ups (built on this foundation)

- **Resume MVP (#47/#48).** `iris-agent resume <id>` rebuilds provider context by replaying `StoredSession.messages`, reopens the same JSONL for append with a persisted cursor (continuing the log rather than rewriting it), repairs a dangling trailing tool call with a synthetic result so a crash-truncated session resumes into a provider-valid sequence, and runs in the current cwd (intentional MVP behavior, not the session's stored cwd). This is the execution of this ADR, not a new decision.
- **Incremental persistence.** Completed provider round trips flush before the
  next request. Crash-mid-turn coverage proves the persisted prefix contains
  pair-closed tool groups, stable ids, and byte-equivalent live/resumed context.
  Dangling-call repair remains the fallback for older or physically truncated
  logs.
- **Compaction foundation and auto-trigger (#49/#55).** The `compaction` entry, context rebuild, and budget-triggered harness compaction are recorded separately in ADR-0009.
- **Large-output handles (#61).** Session-scoped output sidecars are recorded separately in ADR-0011.
