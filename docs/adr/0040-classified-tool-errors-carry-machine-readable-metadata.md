# ADR-0040: Classified tool errors carry machine-readable metadata

**Date**: 2026-07-04
**Status**: accepted
**Deciders**: iris-agent maintainers

## Context

ADR-0021 fixed the tool-result envelope: a tool error serializes as
`{ "ok": false, "error": string }`. That `error` is prose only. Edit-failure
classes — not-found, not-unique, stale-file (ADR-0038, issue #341, PR #358) —
exist solely inside that string, so nothing downstream can partition failures by
class across transcripts without parsing English.

ADR-0038 wants failure-class telemetry keyed by model; the envelope had no
channel to carry it. The success side already separates model-facing content
from bounded host metadata (ADR-0021); the error side had no equivalent, and the
Denied/Cancelled arms already set a precedent by adding a flag beside `error`.

## Decision

Extend the ADR-0021 contract with an opt-in metadata channel on the tool-error
arm; no envelope replacement, no tool-signature change.

- **`ClassifiedError` (Nexus).** A `std::error::Error` carrying a short `class`
  token, its human-readable message, and a compact `fields` map. A tool opts in
  by returning it through the existing `anyhow::Result<ToolOutput>`; every other
  tool keeps plain `bail!`.
- **Wire shape.** `ToolResultContract::into_wire_value` downcasts the tool
  error. When it is a `ClassifiedError`, the error object gains
  `"metadata": { "class": ..., ...fields }` beside the unchanged `error` string.
  An unclassified error stays byte-identical to today's `{ "ok": false,
  "error": ... }`. Denied/Cancelled are untouched.
- **First consumer.** `edit`'s failure paths emit `not-found`, `not-unique`
  (with an `occurrences` field), and `stale-file` (with a `reason` field). The
  prose stays exactly as informative as before — metadata is additive.
- **Boundedness.** Error metadata obeys ADR-0036: `class` is short and `fields`
  carry only classification a consumer can act on, never large or sensitive
  detail. It rides the existing persistence path, so classified failures are
  queryable in stored transcripts.

## Alternatives Considered

### Change every tool's result type to a structured error
- **Pros**: Uniform machine-readable errors everywhere.
- **Cons**: Signature churn across every tool and a migration sweep; forces
  structure onto errors that have nothing to classify.
- **Why not**: The value is concentrated in a few failure classes; opt-in
  through the existing `anyhow::Result` gets it with a downcast and no sweep.

### Encode the class as a prefix inside the `error` string
- **Pros**: Zero envelope change.
- **Cons**: Still string-parsing; couples consumers to prose wording; pollutes
  the model-facing message.
- **Why not**: A structured field is what telemetry and scripts need; the prose
  should stay prose.

### Add a new top-level error-envelope variant per class
- **Pros**: Very explicit shapes.
- **Cons**: Multiplies envelope variants and breaks the single-envelope
  discipline of ADR-0021.
- **Why not**: One optional metadata object composes with the existing envelope
  and the Denied/Cancelled flag precedent.

## Consequences

### Positive
- Failure classes are queryable across transcripts (ADR-0038 telemetry) without
  parsing English.
- Unclassified errors are provably unchanged on the wire; adoption is per-tool
  and incremental.
- Composes with ADR-0021's envelope and the existing persistence path.

### Negative
- Error metadata is heterogeneous like success metadata: consumers inspect
  optional fields per class.
- Each opting-in tool owns its class tokens and field choices.

### Risks
- Class tokens could drift or collide across tools. Mitigation: keep the set
  small, documented at the emit site, and covered by wire-shape tests.
- Over-stuffed error metadata would waste context. Mitigation: ADR-0036 applies
  to error payloads; reviews keep fields compact.
