# ADR-0012: Harness-owned fragment/slot system-prompt assembly

**Date**: 2026-06-18
**Status**: accepted (numeric `slot` ordering revised by ADR-0015; user/repo `.md` file loading superseded by ADR-0026 — fragments are now internal-only)
**Deciders**: Iris maintainers, Pi agent session

## Context

The system prompt was built by a single hardcoded `build_iris_system_prompt(workspace)` in `src/mimir/providers/mod.rs`: static text plus cwd, constructed once at provider construction. It carried no project instructions (AGENTS.md), no date, no skills, and a hardcoded tool-list array that drifts from the registry. It also lived in the provider tier, which violates the tier split (ADR-0001, ADR-0003) since prompt assembly is a harness concern. pi-mono composes its prompt from ordered sections plus auto-injected project docs/date/cwd and rebuilds it per turn (idempotent, cache-stable).

## Decision

Replace the hardcoded builder with a Tier-2 harness-owned assembler (`src/wayland/system_prompt/`) that composes the prompt from droppable `.md` fragment files (YAML frontmatter: `name` -> XML tag, `slot: N` -> sort key; body = content), discovered from global `~/.iris/fragments` then repo `.iris/fragments`. `identity` is anchored first; `available_tools`, `available_tool_guidelines`, `tool_use` are anchored last; user fragments fill the middle by `slot` then alphabetically (global before repo); a non-slottable, non-editable dynamic-context block (AGENTS.md/CLAUDE.md as `<project_context>`, then cwd + date) injects automatically pi-style; an empty-body fragment emits nothing. Providers receive the finished string and only wrap it in their envelope. The assembler is a pure function so per-turn re-assembly is a later no-restructure step. Implemented in #74.

## Alternatives Considered

### Keep the hardcoded builder in the provider tier
- **Pros**: Simplest; no new parser or loader.
- **Cons**: No project instructions, drifting tool list, tier violation.
- **Why not**: Fails pi-parity and the tier boundary; the model never sees `AGENTS.md`.

### Codex distributed assembly with per-turn context-diff injection
- **Pros**: Cache-preserving; ready for mutable mid-session settings.
- **Cons**: Heavy machinery (TurnContextItem persistence, delta builders) for settings Iris does not have yet.
- **Why not**: Over-engineered for the current stage; deferred until modes/mutable settings land.

### Build once, but in the harness
- **Pros**: Less work than per-turn rebuild.
- **Cons**: Will not reflect mutable context once settings can change mid-session.
- **Why not**: Adopted as the near-term shape — the assembler is kept pure, so upgrading to rebuild-per-turn is trivial later; build-once is acceptable now.

## Consequences

### Positive
- Project `AGENTS.md` reaches the model; the tool list derives from the live registry instead of a static array.
- Users configure the prompt by dropping/editing `.md` files (skills/agents UX).
- Tier boundary restored: assembly in Wayland, providers only wrap.
- Foundation for the shared selector schema (ADR-0013) and the later mode system.

### Negative
- A fragment loader, frontmatter parser, and ordering rules to maintain.
- Requires a YAML frontmatter parser dependency (`serde_yml`/`serde_yaml_ng`; original `serde_yaml` is archived).

### Risks
- Untrusted repo `.iris/fragments` / project-doc content is injected into the prompt; mitigated in #74 by reading every folded-in file through a bounded, symlink-refusing reader and resolving the repo fragments dir through the workspace path sandbox, plus tests for ordering, anchoring, and empty-body skip.
