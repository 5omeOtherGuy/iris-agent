# ADR-0001: Keep Nexus, Wayland, and Iris as in-crate tiers

**Date**: 2026-06-17
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Iris needs a stable ownership split between the runtime core, harness/session concerns, and terminal UI. `docs/ARCHITECTURE.md` defines Nexus as the provider-, UI-, persistence-, and workspace-neutral core; Wayland as the harness; and Iris CLI as the terminal frontend. The capability matrix confirmed this split is implemented, but Wayland is still intentionally thin.

## Decision

We keep Nexus, Wayland, and Iris as in-crate tiers for now. Nexus owns the core model/tool loop, Wayland owns harness concerns such as workspace/tool state/session wiring, and Iris CLI owns terminal UX.

## Alternatives Considered

### Monolithic agent runtime
- **Pros**: Fastest first implementation and fewer seams.
- **Cons**: UI, provider, tool, and session policy would mix.
- **Why not**: It makes safety policy harder to test and violates the documented dependency direction.

### Split Nexus, Wayland, and Iris into separate crates now
- **Pros**: Stronger compile-time boundaries.
- **Cons**: More packaging and API churn before a second frontend or published runtime exists.
- **Why not**: The current in-crate module boundaries are enough; split later when reuse justifies it.

### Let the CLI own harness/runtime policy
- **Pros**: Simpler while terminal is the only frontend.
- **Cons**: Future frontends would duplicate policy and session logic.
- **Why not**: Harness/runtime behavior should not be terminal-specific.

## Consequences

### Positive
- Clear ownership boundaries without crate-level overhead.
- Easier future crate split because the dependency direction is already explicit.
- Reviews can flag UI/provider/session leakage into Nexus.

### Negative
- Some plumbing is required between tiers.
- Boundaries depend on review discipline until separate crates exist.

### Risks
- Tier leakage during feature work; mitigate by checking `docs/ARCHITECTURE.md`, `docs/NAMING.md`, and the capability matrix before runtime changes.
