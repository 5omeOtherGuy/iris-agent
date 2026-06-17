# ADR-0005: Nexus owns tool approval and execution policy

**Date**: 2026-06-17
**Status**: accepted
**Deciders**: Iris maintainers

## Context

Iris tools can read, mutate files, and run shell commands. Safety depends on consistent approval, cancellation, transcript recording, and sequential-vs-parallel execution behavior. The runtime completion work moved this policy into Nexus while keeping concrete tool behavior and terminal UX outside the core.

## Decision

Nexus owns tool approval enforcement, session allow-policy, cancellation races, transcript-valid synthetic results, sequential-default scheduling, and safe-parallel batching. Tools classify themselves with metadata; UI prompts and renders events but does not own policy.

## Alternatives Considered

### UI owns approval policy
- **Pros**: Terminal prompts and decisions live together.
- **Cons**: Future frontends would duplicate safety behavior.
- **Why not**: Approval is runtime safety policy, not terminal display logic.

### Tools self-enforce approval and scheduling
- **Pros**: Less central runtime code.
- **Cons**: Each tool could interpret safety and cancellation differently.
- **Why not**: Nexus must guarantee global transcript validity and consistent policy.

### Run all read-only-looking tools in parallel by default
- **Pros**: More concurrency.
- **Cons**: Misclassified or stateful tools could race.
- **Why not**: Sequential by default is safer; tools must explicitly declare concurrency safety.

## Consequences

### Positive
- One enforcement point for tool safety.
- UI/frontends cannot bypass approval policy by accident.
- Cancellation and synthetic results are consistent across tools.

### Negative
- Nexus has more runtime responsibility.
- Tool metadata must be accurate for safe approval and scheduling.

### Risks
- Incorrect tool classification can create approval or concurrency bugs; mitigate with tests for approval, cancellation, sequential default, and safe parallel execution.
- Core can grow too broad; mitigate by keeping concrete tool bodies in tool modules and host/workspace state in Wayland.
