# ADR-0011: Store oversized tool outputs behind session-scoped handles

**Date**: 2026-06-17
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Large tool outputs such as test logs, broad greps, and long shell output can dominate provider context if Iris replays the full payload on every model round-trip and every resume. The full output is still user-visible state and must stay retrievable; truncating it would lose evidence. Iris also needs to preserve the tier split: Nexus may own the model-facing result shape and offload policy, but filesystem storage belongs in the Wayland harness.

Issue #61 needs the smallest durable handle foundation before selective dereferencing, indexing, binary artifacts, or a TUI attachment browser exist.

## Decision

Successful tool results whose content exceeds the fixed inline threshold are stored out of provider context when a session-backed output store is available. The transcript keeps the normal tool-result envelope, but the content is replaced by a bounded head/tail preview and metadata containing an `outputHandle` with the handle id, byte count, and line count.

The Wayland harness derives a session-scoped handle store beside the JSONL transcript (`<session>.jsonl` -> `<session>.outputs/`). Stored outputs are content-addressed using a truncated SHA-256 handle, so identical content deduplicates and the same output maps to a stable id across resume. Nexus receives this store through the `ToolOutputStore` contract; it never writes files directly.

If no durable store exists, the output is below the threshold, or storing fails, Iris keeps the full output inline. The observer/UI still receives the full output even when the provider transcript receives only the compact preview. `HandleStore::get` is the retrieval seam, but a model-facing dereference tool and TUI browser are deferred.

## Alternatives Considered

### Always keep full outputs inline
- **Pros**: Simplest transcript shape; no sidecar files.
- **Cons**: Replays giant logs/search results into every provider call and resume.
- **Why not**: It defeats the token-efficiency workstream and makes long sessions degrade quickly.

### Truncate oversized outputs in the transcript
- **Pros**: Small model context and no separate store.
- **Cons**: Loses data the user/model may need later; cannot recover exact output.
- **Why not**: Token reduction must not be data loss. The full payload stays durable behind the handle.

### Use a global cache, database, or blob store
- **Pros**: Easier cross-session indexing and sharing later.
- **Cons**: New operational surface, cleanup policy, and migration burden.
- **Why not**: Session-local sidecars are enough for the current durable transcript and share its lifetime.

### Add the model-facing dereference tool now
- **Pros**: The model could request the full payload immediately.
- **Cons**: Requires prompt policy, permissions, UI affordances, and context-budget behavior beyond the storage foundation.
- **Why not**: Store first; dereference is a later selective-context slice.

## Consequences

### Positive
- Large outputs stop ballooning provider context while remaining retrievable.
- The same transcript shape works for inline and offloaded results.
- Storage stays in Wayland; Nexus owns only the contract and model-facing policy.

### Negative
- Sessions with offloaded outputs now have sidecar directories to preserve during cleanup/export.
- The inline threshold and preview sizes are fixed constants until real usage justifies settings.
- The model cannot dereference a handle yet.

### Risks
- A future cleanup/export path could copy JSONL without its `.outputs/` sidecar; mitigate by treating the sidecar as part of the session bundle.
- A forged handle id from an edited transcript must not escape the store; mitigate by validating ids before reads.
- Sidecar writes can fail; current behavior falls back to full inline output so the tool result is never lost.
