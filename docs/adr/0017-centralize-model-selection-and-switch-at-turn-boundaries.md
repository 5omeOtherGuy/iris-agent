# ADR-0017: Centralize model selection and switch at turn boundaries

**Date**: 2026-06-21
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Iris now supports multiple Mimir providers and runtime model/reasoning changes from the TUI and text command path. Before this decision, provider adapters each owned pieces of model/default/base-url resolution, which made settings precedence and reasoning support easy to drift. Runtime switching also needs to preserve Nexus's simple turn loop: a provider/model must not change while a provider stream or tool batch is in flight.

## Decision

Centralize provider/model/base-url/reasoning resolution in Mimir (`selection`, `model_capabilities`, `model_catalog`) and make the CLI own runtime switching as a safe turn-boundary operation. `/model`, `/reasoning`, picker actions, Ctrl+P cycling, and Shift+Tab effort cycling rebuild a provider from the resolved `ModelSelection`, validate/clamp reasoning through the capability table, install it only between turns, and record a `modelSelection` session audit entry when a switch succeeds.

Use a small hand-maintained model catalog for the models Iris can actually route today. The catalog filters picker candidates by credential presence and can hide gated models behind an explicit opt-in; it is not a generated provider registry.

## Alternatives Considered

### Keep model/default resolution inside each provider adapter
- **Pros**: Fewer shared types; adapters can stay self-contained.
- **Cons**: Duplicates precedence rules, makes `/model` switching provider-specific, and lets reasoning/default behavior drift between adapters.
- **Why not**: Runtime switching needs one neutral selection object and one validation path so the CLI can rebuild any provider consistently.

### Let Nexus own provider routing and switching
- **Pros**: Centralizes all runtime state in the core loop.
- **Cons**: Pulls provider construction, auth, settings, and UI command concerns into Tier 1, violating the tier split.
- **Why not**: Nexus should remain the provider-neutral loop. Provider construction and terminal commands belong in Tier 3, with Wayland providing persistence around the loop.

### Use a generated model registry now
- **Pros**: More complete model metadata and less manual drift as providers change.
- **Cons**: Requires generation inputs, update policy, and a larger metadata surface before Iris needs it.
- **Why not**: The current supported provider/model set is small. A hand-maintained catalog is easier to review and keeps the first runtime-switching slice narrow.

### Switch providers mid-turn
- **Pros**: A user command could take effect immediately.
- **Cons**: Would race with in-flight provider streams, tool calls, approval state, and transcript validity.
- **Why not**: Turn-boundary switching is predictable, preserves transcript semantics, and matches the existing cancellation/tool-execution model.

## Consequences

### Positive
- One precedence path controls startup and runtime provider/model/reasoning selection.
- The CLI can offer `/model`, `/reasoning`, picker, and cycle shortcuts without leaking provider-specific rules into Nexus.
- Session logs retain an auditable record of provider/model/reasoning changes.
- Providers receive only resolved strings and optional reasoning effort, keeping adapters focused on wire-format mapping.

### Negative
- The model catalog and capability table must be maintained when provider models change.
- Unknown future models can be selected by exact id in text commands, but they may lack friendly picker metadata until the catalog is updated.

### Risks
- Catalog drift can hide a supported model or advertise one that is no longer available; mitigate with focused catalog/provider matrix tests and conservative opt-ins for gated models.
- Reasoning capability drift can produce provider errors; mitigate by validating startup settings and clamping interactive switches through `model_capabilities`.
- A future per-turn provider router could outgrow CLI-owned switching; mitigate by promoting the selection object and audit event, not provider construction, into the future routing seam.
