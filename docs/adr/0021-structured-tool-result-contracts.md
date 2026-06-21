# ADR-0021: Define structured tool-result contracts without a schema platform

**Date**: 2026-06-21
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Issue #123 asks Iris to adopt the useful part of Flue's typed result discipline while staying Rust-native and terminal-first. Flue separates model-facing result content from host metadata through schema-backed tools/actions/results. Pi persists tool result messages with tool-call ids, content, error state, usage, and optional details. Iris already has `ToolOutput { content, metadata }`, synthetic denied/cancelled results, session JSONL persistence, output handles, ADR-0019 correlation ids, and ADR-0020 typed events, but the result contract was implicit.

## Decision

Iris keeps one narrow provider-neutral tool-result envelope owned by Nexus:

| Case | Stable model-facing JSON | Notes |
|---|---|---|
| Success | `{ "ok": true, "content": string }` plus optional `{ "metadata": object }` | `content` is the text the model should reason over. `metadata` is host/tool metadata, not a replacement for content. |
| Tool error | `{ "ok": false, "error": string }` | Used when a tool executes and fails or argument parsing fails. |
| Denied | `{ "ok": false, "error": "tool call denied by user", "denied": true }` | A pre-execution approval refusal, distinct from a tool failure. |
| Cancelled | `{ "ok": false, "error": "tool call cancelled by user", "cancelled": true }` | A user/turn interrupt, distinct from denial and tool failure. |
| Handle-backed large success | Success envelope whose `content` is a bounded preview and whose `metadata.outputHandle` is `{ "id": string, "bytes": number, "lines": number }` | The full body is stored in the session sidecar, not in metadata/events/context. |

Nexus now represents that envelope with a Rust-native `ToolResultContract` used by serialization, and represents output-handle metadata with a typed `OutputHandleMetadata` used by both the model-facing metadata object and the `OutputHandleStored` observability event. Concrete tool modules still own their local metadata fields (`read` bytes/lines/truncated, `ls` entries/truncated, `grep` metrics, `write` bytes, `edit` occurrences, `bash` execution details). Nexus removes display-only `exitCode`/`durationMs` from the model-facing success metadata before serialization and lifts them onto UI/events only.

Wayland/session persistence remains outside Nexus. JSONL message entries persist the provider-visible tool-result string as before, with optional `providerTurnId` from ADR-0019. Because the envelope remains the same and new contract types are internal serialization helpers, old sessions without new optional fields still load and resume.

Future delegated work should reuse the same separation: model-facing summary/content first, bounded host metadata second, and correlation through the ids defined in ADR-0019. A future worker/subagent summary may add metadata such as worker id, status, token estimates, output handle id, or covered task summary, but it should not require a new result envelope.

## Redaction and boundedness rules

- Metadata must be JSON-serializable, bounded, and safe to persist/render.
- Metadata must not contain raw large outputs, provider request/response payloads, auth material, secrets, or full handle-backed bodies.
- Search metadata omits raw query terms where the tool already treats them as sensitive/noisy.
- Output-handle metadata carries only id/bytes/lines. The preview remains model-facing content, and the full body remains in the session-scoped handle store.
- ADR-0020 events align with these shapes: `OutputHandleStored` carries the same id/bytes/lines and never includes body or preview; `ToolLifecycle` carries ids/state only.

## Alternatives Considered

### Adopt Flue Valibot schemas or generate JSON Schema/OpenAPI

- **Pros**: Strong public schema story for tools/actions/results.
- **Cons**: Pulls Iris toward TypeScript/server/plugin architecture it does not have.
- **Why not**: Issue #123 needs a small Rust contract for existing tools and future delegation, not a schema platform.

### Add new public plugin result APIs now

- **Pros**: Could lock extension authors to a stable contract early.
- **Cons**: Iris has no plugin API yet, so this would freeze guesses.
- **Why not**: Keep the internal contract narrow until extension boundaries exist.

### Change model-facing success content into fully typed per-tool objects

- **Pros**: More regular per-tool data for future consumers.
- **Cons**: High provider-behavior risk and broad tool churn.
- **Why not**: Existing providers already receive consistent JSON; preserving content while clarifying metadata is the smallest safe step.

### Store full large outputs in metadata

- **Pros**: One object contains everything.
- **Cons**: Defeats token reduction and risks leaking large/sensitive payloads into events and logs.
- **Why not**: ADR-0011 requires full bodies to stay behind handles.

## Consequences

### Positive

- Tool-result success/error/denied/cancelled shapes are explicit and tested.
- Output-handle metadata is typed and shared between serialization and events.
- Provider-facing behavior is preserved while future context/delegation work gets a stable semantic contract.
- The result contract composes with ADR-0019 ids and ADR-0020 typed events.

### Negative

- Per-tool metadata is still intentionally heterogeneous; consumers must inspect optional fields by tool.
- The contract is internal and `pub(crate)`, so it is not yet a public extension schema.

### Risks

- Future subagents/workers may need additional summary metadata; mitigate by adding optional metadata fields under the same envelope rather than replacing it.
- If Iris later exposes public plugin APIs, this internal contract will need a formal compatibility policy and possibly versioning.
- Existing metadata maps are only as safe as each tool's field choices; reviews must keep enforcing bounded, redacted metadata.
