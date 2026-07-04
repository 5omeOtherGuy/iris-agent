# ADR-0038: Per-model edit surfaces share one mutation core

**Date**: 2026-07-04
**Status**: proposed
**Deciders**: iris-agent maintainers

## Context

ADR-0023 allows provider adapters to present model-specific tool surfaces and to
prefer provider-native tools behind Iris's safety contracts. Epic #10 applies
this to file editing: models are most reliable with the edit format they were
trained on. Evidence from the reference harnesses:

- OpenAI post-trains GPT/Codex models on the V4A patch grammar and ships
  `apply_patch` as a first-class tool (`~/vendor/codex/codex-rs/apply-patch`,
  Apache-2.0: batch + streaming parser, fuzzy context seek, ~1,000 production
  lines, ~58% inline tests).
- Anthropic trains Claude on exact-string `str_replace` editing; Claude Code's
  edit tool adds a quote/whitespace tolerance layer on top of that contract.
- pi-mono has the largest matching-tolerance layer of the references but the
  hardest contract (batch `edits[]` with original-file uniqueness and
  non-overlap), and observed edit-failure rates are not better for it.

Conclusion drawn from that comparison: contract familiarity dominates
harness-side recovery. A tolerance layer reduces failures within a contract; it
does not compensate for an unfamiliar contract.

`apply_patch` is not only a familiarity play. The V4A grammar has capability
the exact-string contract structurally lacks: file lifecycle in the edit
surface (`*** Add File` / `*** Delete File` / `*** Move to`), multi-file
multi-hunk atomic application, context-anchored hunks (`@@` scope + context
lines) instead of whole-file string uniqueness — decisive in repetitive code —
and changed-lines-only output where `edit` emits unchanged text twice (in
`old_string` and `new_string`). The capability and token cases stand
independent of training affinity.

Iris's `edit` already implements the Anthropic-shaped contract with a tolerance
layer: exact match first, then a fuzzy fallback folding Unicode spaces, quotes,
dashes, and trailing whitespace, with uniqueness enforced under normalization,
plus BOM and line-ending preservation (`src/tools/edit.rs`). What Iris lacks is
the OpenAI-native surface and the measurement to justify either.

## Decision

Focus edit surfaces on the two active provider families — Anthropic-shaped
`edit` (exists) and OpenAI `apply_patch` (V4A, to build) — sharing one mutation
core.

- **One mutation core in Nexus.** Read-before-mutate (`ObservedFiles`), atomic
  write, workspace path safety, diff preview + approval, and the
  `ToolOutput`/result contract are implemented once. An edit surface is a
  front-end: contract parser + tolerance layer; everything after the parse is
  shared.
- **Per-surface tolerance layers, same rules for each:**
  - Fuzzy recovery runs only after exact/context match fails.
  - Ambiguity under normalization fails loudly; a fuzzy match never picks one
    of several candidates.
  - Tolerance is not advertised in the tool description.
- **Conditional feedback.** Exact-match success returns the terse result
  (ADR-0036: success is cheap). When a tolerant (non-exact) path fired, the
  result echoes a compact snippet of the applied region so the model's view of
  the file cannot drift silently. Failures return complete diagnostics.
- **Failure-class telemetry.** Edit results record the failure or recovery
  class (not-found, not-unique, stale-file, tolerant-match-fired) keyed by
  active model. This data gates tolerance-layer changes and the epic #10
  benchmark requirement.
- **Selection and fallback.** The per-provider tool-projection registry
  (ADR-0023; prerequisite work in epic #10) advertises `apply_patch` on
  OpenAI/Codex routes. Generic `edit` remains the canonical surface for all
  other routes and the fallback when the native surface is absent, disabled,
  or fails. Benchmarks decide routing (which model gets which surface) and
  validate tolerance changes — per epic #10 — not whether `apply_patch`
  exists; its capability case is independent of familiarity.
- **`apply_patch` build strategy.** Port the codex-apply-patch core (V4A
  grammar parser, fuzzy context seek, apply + unified-diff reporting, inline
  tests) with Apache-2.0 attribution. The port includes the streaming parser:
  the V4A grammar implementation lives in it and the batch API is a thin
  wrapper — they are one parser. Iris drives it batch-style for execution;
  the incremental interface feeds the display-only live preview (ADR-0039).
  Shell-heredoc invocation detection stays out of scope until a need is
  demonstrated. On the wire it is a freeform/custom tool on the Responses
  API, mapped by that Mimir adapter only (chat-completions does not carry
  custom tools); the projection registry keeps it invisible on all other
  routes.

## Alternatives Considered

### One universal edit contract for all models
- **Pros**: Single surface, simplest transcripts and tests.
- **Cons**: Forces some models to emit a format they were not trained on and
  leans on fuzzy recovery to compensate; the reference comparison shows that
  trade loses.
- **Why not**: Contract familiarity is the dominant reliability factor.

### Fully separate edit stacks per provider
- **Pros**: Maximum per-provider tuning.
- **Cons**: Duplicated safety machinery; drift between surfaces; two sets of
  mutation semantics to test.
- **Why not**: ADR-0023 requires one canonical capability contract; only the
  parser and tolerance layer justify divergence.

### Adopt a batch multi-edit contract (pi-mono shape)
- **Pros**: Fewer round trips for multi-site edits.
- **Cons**: Adds failure modes models actually hit (overlap, sibling
  uniqueness); no trained-model affinity for the shape.
- **Why not**: The hardest contract with no familiarity win; observed failure
  rates do not support it.

### Silent tolerance (no conditional echo)
- **Pros**: Cheapest success payload in all cases.
- **Cons**: After a tolerant match the model's mental copy of the file is
  subtly wrong; the next edit builds on it; a misplaced edit costs far more
  than the echo saves.
- **Why not**: Spend a few dozen tokens exactly where ambiguity was
  introduced; nowhere else.

## Consequences

### Positive
- Each supported model family gets its trained format; reliability work is
  measured, not assumed.
- Safety and result contracts stay single-sourced in Nexus; adding a future
  surface (e.g. a Gemini diff-fenced variant) is parser + tolerance only.
- Telemetry turns tolerance-layer tuning and benchmark gates into data
  decisions.

### Negative
- Two model-facing edit formats to document, test, and debug; transcripts must
  record which surface was active.
- Vendored V4A core needs periodic re-sync with upstream fixes.

### Risks
- A tolerant match applies an edit in the wrong place. Mitigation: fuzzy only
  after exact fails, uniqueness under normalization, conditional echo, and
  failure-class telemetry to catch regressions.
- The projection registry could drift into per-provider behavior forks beyond
  declarations. Mitigation: projection tests per ADR-0023; surfaces share the
  mutation core by construction.
