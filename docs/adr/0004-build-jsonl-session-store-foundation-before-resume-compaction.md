# ADR-0004: Build JSONL session store foundation before resume and compaction

**Date**: 2026-06-17
**Status**: accepted; implemented in issue #42
**Deciders**: Iris maintainers, Pi agent session

## Context

At decision time, Iris had best-effort write-only JSONL transcript logging. The harness capability matrix found session persistence was the next major gap versus pi-mono and Codex. Issue #42 defined the smallest useful slice: a JSONL-backed session store with ids, parent links, read/open/list support, and tests.

Outcome: implemented as a v2 JSONL store. `SessionLog` remains the live append handle; `SessionStore` lists sessions and opens one back in message order. `/resume`, branching/tree APIs, compaction, labels, fork/rollback, and token accounting remain intentionally deferred.

## Decision

We build a minimal JSONL Session Store Foundation before implementing `/resume`, branching UI, rollback, compaction, plugins, or a Codex-style thread store. The store should support session ids, message ids, `parent_id`, create/open/list/read/append behavior, and compatibility with current append logging.

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
- Unlocks future `/resume`, branching, compaction, token accounting, and context reconstruction.
- Keeps local session data simple and human-inspectable.
- Provides a testable contract before adding UI behavior.

### Negative
- Listing/querying may be less efficient than a database.
- Some migration may be required if future requirements exceed JSONL.

### Risks
- Broken or truncated JSONL could block session reads; mitigate by preserving valid-prefix behavior and testing partial/trailing corruption where practical.
- Parent/message id choices may constrain future branching; mitigate by following the pi-mono session shape and keeping the format minimal.

## Follow-ups (built on this foundation)

- **Resume MVP (#47/#48).** `iris-agent resume <id>` rebuilds provider context by replaying `StoredSession.messages`, reopens the same JSONL for append with a persisted cursor (continuing the log rather than rewriting it), repairs a dangling trailing tool call with a synthetic result so a crash-truncated session resumes into a provider-valid sequence, and runs in the current cwd (intentional MVP behavior, not the session's stored cwd). This is the execution of this ADR, not a new decision.
- **Compaction foundation (#49).** The `compaction` entry and context rebuild are recorded separately in ADR-0009.
