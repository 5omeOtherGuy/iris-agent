# ADR-0013: Shared selector schema for dynamic system-prompt and tool-surface assembly

**Date**: 2026-06-18
**Status**: proposed (fragment provenance narrowed to internal-only by ADR-0026; selectors still apply)
**Deciders**: Iris maintainers, Pi agent session

## Context

We want both the system prompt and the model-visible tool surface assembled for the turn's resolved `{provider, model, thinking_level, mode}` (modes per pi-mmr, later). Prompt fragments (ADR-0012) and tools both need conditional inclusion. Iris already separates the model-visible tool set (`Tools::iter`) from the execution registry (`Tools::by_name`) via `plan_surface` / `ProviderCapabilities` in `src/nexus.rs`, and #18 adds WASM plugin tools with a manifest. The full design is captured in issue #73.

## Decision

Adopt one positive-only selector schema, used by both fragments and tool declarations: frontmatter/manifest fields `provider`, `model`, `thinking_level`, `mode` — each a string or non-empty list; glob via `globset`; absent field matches all; any-of within a field, AND across fields; no regex, negation, or specificity ranking in v1. A `mode` resolves first into the concrete axes, then selectors match the resolved context with no implicit inference. The selector feeds the existing `plan_surface`/`iter`/`by_name` seam and the ADR-0012 assembler through a shared `ResolutionContext`. Near-term, tools are not selector-filtered (they use load/don't-load plus the existing `native_edit` capability hiding); the tool selector is a modes-milestone concern. WASM manifests (#18) gain the same selector fields rather than a competing standard.

## Alternatives Considered

### Separate mechanisms for fragment inclusion vs tool visibility
- **Pros**: Each can evolve independently.
- **Cons**: Two schemas, two parsers, inevitable drift.
- **Why not**: One schema is simpler and keeps fragments, built-ins, command tools, and plugins consistent.

### Codex per-turn context-diff injection for the dynamic axes
- **Pros**: Cache-preserving across turns.
- **Cons**: Heavy machinery; needs mutable-settings persistence Iris lacks.
- **Why not**: Deferred (see ADR-0012); the selector + full rebuild is sufficient now.

### Expressive predicate DSL (regex, negation, precedence)
- **Pros**: Maximum flexibility.
- **Cons**: Complexity and ambiguous precedence; harder to test and reason about.
- **Why not**: YAGNI; positive any-of/AND with globs covers the known cases.

## Consequences

### Positive
- One consistent selector across fragments, built-ins, command tools, and plugins.
- Reuses the existing visibility/execution tool seam instead of new parallel machinery.
- #18 manifests gain selectors without a second standard.

### Negative
- A YAML + glob matching layer; frontmatter/manifest grow.

### Risks
- Mode-composition ambiguity if inference were allowed; avoided by resolve-then-match.
- Declaration sprawl if tool selectors are built too early; bounded by deferring tool selection to the modes milestone. The authorization boundary for selectors is fixed separately in ADR-0014.
