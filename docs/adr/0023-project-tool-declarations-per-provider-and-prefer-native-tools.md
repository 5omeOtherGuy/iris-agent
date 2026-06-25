# ADR-0023: Project tool declarations per provider and prefer native tools

**Date**: 2026-06-21
**Status**: accepted
**Deciders**: Iris maintainers, Pi agent session

## Context

Iris has one provider-neutral tool contract in Nexus and provider adapters in
Mimir that translate that contract to each model API. Today each built-in tool
provides one canonical name, description, and JSON-shaped parameter declaration.
Adapters already project that declaration to provider wire formats: OpenAI-style
providers receive `parameters`, Anthropic receives `input_schema`, and
Antigravity/Gemini receives a sanitized `parameters` value because Gemini rejects
some JSON Schema keywords.

The useful next distinction is between the canonical Iris tool contract and the
provider/model-specific declaration shown to the model. The declaration is the
structured tool object sent in the same request as the system prompt and
conversation messages; it is not the system prompt text itself. It teaches the
model the tool name, description, and accepted input shape for that provider's
API.

Some providers also offer native tools that can be better than Iris's generic
shared tool for the same user-facing capability. For example, an OpenAI model may
support an `apply_patch`-style native edit tool that should be preferred over
showing Iris's generic `edit` declaration when the provider can execute or
interpret that native surface more reliably.

## Decision

Keep one canonical Iris tool contract per capability, then project the
model-visible tool declaration per provider/model before sending a request:

- Shared tools keep one source of truth for capability, approval behavior,
  execution semantics, result contract, and safety policy.
- Provider adapters may customize the model-visible declaration for that shared
  tool: field name (`parameters` vs `input_schema`), supported schema keywords,
  description wording, enum/union simplification, or other provider/model-specific
  presentation details.
- Provider adapters may hide a shared generic declaration when a better native
  provider-specific tool is available for the same capability.
- Provider-specific native tools are allowed where they improve correctness or
  model reliability, such as preferring OpenAI `apply_patch` over the generic
  `edit` declaration for OpenAI models that support it.
- Native-provider tools must still map back into Iris's existing safety model:
  Nexus owns approval enforcement, tool scheduling, cancellation, result
  contracts, and transcript validity; concrete execution and provider wire-format
  mapping stay outside Nexus.
- Tool visibility remains separate from authorization. Hiding generic `edit`
  because native `apply_patch` is visible is a capability projection decision, not
  an approval bypass.

The projection happens in the provider tool-declaration payload, not by copying
schemas into the system prompt. The system prompt should keep broad behavioral
rules; structured tool declarations should carry tool names, descriptions, and
input schemas.

## Alternatives Considered

### Send the same raw tool declaration to every provider
- **Pros**: One path and fewer tests.
- **Cons**: Providers use different field names and schema dialects; some models
  may perform better with simpler or provider-specific wording; Gemini already
  needs sanitization.
- **Why not**: A canonical contract plus adapter projection preserves one source
  of truth while respecting provider APIs.

### Maintain fully separate hand-written schemas for every provider/model
- **Pros**: Maximum provider-specific tuning.
- **Cons**: High drift risk; the same Iris tool could quietly mean different
  things for different providers.
- **Why not**: The canonical Iris contract must remain the source of truth.
  Provider/model customization should be a projection layer with tests, not a
  parallel schema universe.

### Describe provider-specific tool behavior in the system prompt
- **Pros**: Simple to prototype and works even for providers with weak tool APIs.
- **Cons**: Wastes tokens, duplicates structured declarations, and can drift from
  the actual executable schema.
- **Why not**: Provider APIs already have structured tool declaration fields; use
  the system prompt only for broad policy and fallback guidance.

### Never use provider-native tools
- **Pros**: Uniform tool surface and simpler transcript handling.
- **Cons**: Leaves quality and reliability on the table when a provider exposes a
  stronger native capability, such as patch application.
- **Why not**: Native tools are acceptable when they are projected through Iris's
  safety/result contracts and do not bypass Nexus policy.

## Consequences

### Positive
- Iris can tune tool declarations for each provider/model without duplicating the
  underlying tool semantics.
- Provider-native capabilities can improve reliability where they exist while the
  generic shared tool remains the fallback.
- The design extends existing Gemini schema sanitization into a general projection
  principle.
- Nexus remains provider-neutral because projection and native wire-format mapping
  stay in provider/tool adapter layers.

### Negative
- Provider adapters need projection tests to prevent drift.
- The model-visible tool surface may differ by provider, so debugging must record
  or expose enough metadata to know which declaration was sent.
- Native provider tools may require mapping results and errors back into Iris's
  shared result contracts.

### Risks
- A native tool could bypass approval, path safety, or output-handle policy if it
  is treated as special execution instead of a projected capability. Mitigate by
  routing native-tool execution through the same Nexus approval/result/cancellation
  contracts or by refusing native tools that cannot be safely mediated.
- Divergent provider projections could make prompts non-portable across models.
  Mitigate with one canonical Iris contract, provider projection tests, and only
  targeted customizations justified by provider behavior.
- Provider-native tool support can change. Mitigate with capability metadata and
  fallback to the shared generic tool when native support is absent or disabled.
