# ADR-0003: Keep provider adapters and auth in Mimir

**Date**: 2026-06-17
**Status**: accepted
**Deciders**: Iris maintainers

## Context

Iris supports multiple model providers with different wire formats, auth flows, token refresh paths, and endpoint configuration. `docs/ARCHITECTURE.md` and `docs/NAMING.md` keep the provider-neutral `ChatProvider` contract in Nexus while Mimir owns concrete provider adapters and auth. Provider/base-url configuration is also a trust boundary because project-local configuration must not be able to silently redirect bearer tokens.

## Decision

Concrete provider adapters, auth stores, wire formats, endpoints, and token refresh behavior live in Mimir and startup wiring. Nexus consumes only provider-neutral messages, tools, and stream events through the `ChatProvider` contract.

## Alternatives Considered

### Put provider-specific logic in Nexus
- **Pros**: Fewer modules and direct access to runtime state.
- **Cons**: Pollutes the core loop with provider endpoints, auth details, and transport quirks.
- **Why not**: Nexus must stay provider-neutral so runtime behavior is testable and reusable.

### Let provider packages own the core contract
- **Pros**: Providers could expose their preferred shape directly.
- **Cons**: The runtime would depend outward on provider-specific types.
- **Why not**: Nexus owns the stable contract; adapters translate into it.

### Allow project-local provider endpoint overrides everywhere
- **Pros**: More flexible for experiments.
- **Cons**: Cloned repos could redirect credentials or bearer tokens.
- **Why not**: Provider/base-url trust rules must remain centralized and explicit.

## Consequences

### Positive
- Nexus remains independent of provider names, endpoints, auth flows, and wire schemas.
- New providers can be added behind the same stream/event contract.
- Token and endpoint handling can be reviewed as security-sensitive Mimir code.

### Negative
- Provider-specific capabilities need explicit translation and later tool-surface planning.
- Startup wiring must select the provider before building the agent.

### Risks
- Provider-specific behavior can leak into Nexus over time; mitigate by rejecting provider names, endpoints, and auth details in Nexus/runtime code.
