# ADR-0015: Assign fragments to config-defined named slots instead of numeric slots

**Date**: 2026-06-18
**Status**: proposed
**Deciders**: Iris maintainers, Pi agent session

## Context

ADR-0012 (#74) gave each system-prompt fragment a numeric `slot: N` that defines one global, absolute ordering. That was correct for the static MVP. ADR-0013 (#73) then makes fragment inclusion conditional per `{provider, model, thinking_level, mode}`, so the *active set* of fragments varies per turn. Under that model, absolute integers stop working: each mode/model activates a different subset, a single global integer cannot guarantee the intended order in every subset, two independently authored fragments that both pick the same number fall to an unintended tiebreak, a fragment shared by several modes/models cannot carry a different position for each, and choosing non-colliding integers across global + repo + multiple authors is the BASIC-line-number / CSS-z-index problem (insert one fragment, renumber its neighbors). See #76.

## Decision

Replace numeric `slot: N` with **config-assigned named slots**. A central ordering config owns an ordered list of named slots (sections); each fragment declares membership by **slot name** (`slot: workflow`), not a number. Slot order lives in one place and is reordered there with no fragment edits, leaving room for per-mode/model slot ordering when modes land. Within a slot the existing tiebreak is kept (global before repo, then alphabetical), and anchored fragments (`identity` first; `available_tools` / `available_tool_guidelines` / `tool_use` last) stay anchored regardless of config. This revises the slot mechanism of ADR-0012 and composes with the ADR-0013 selector schema.

## Alternatives Considered

### Keep numeric `slot: N`
- **Pros**: Already implemented (#74); zero migration.
- **Cons**: Does not compose with conditional subsets; magic-number coordination; cannot express per-mode/model position; renumber-on-insert.
- **Why not**: The defect this ADR exists to fix; it worsens as modes/models multiply.

### Per-fragment relative anchoring (`before:`/`after:` another fragment)
- **Pros**: No central config; local intent.
- **Cons**: Can form cycles; global order is hard to reason about; conditional subsets can drop an anchor target.
- **Why not**: More failure modes than a central ordered list; defer relative anchoring as a possible later refinement.

### Implicit ordering (source/author order only)
- **Pros**: Simplest; no slot concept.
- **Cons**: No cross-source control; global vs repo ordering becomes accidental.
- **Why not**: Loses deliberate section ordering, which is the point of slots.

## Consequences

### Positive
- Ordering composes with the selector schema: stable section membership, central order.
- One place to reason about and reorder sections; no magic-number coordination.
- Opens a clean path to per-mode/model slot ordering without touching fragments.

### Negative
- A slot-order config to maintain, and a one-time migration of the shipped defaults from numbers to names.

### Risks
- Fragment slot names drifting from the config; mitigate by validating slot names at load and rejecting unknown names.
- Over-design if per-mode ordering is built before modes exist; defer per-mode overrides and relative anchoring until needed.
