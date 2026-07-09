# ADR-0053: Load Codex skills as contextual messages

**Date**: 2026-07-09
**Status**: accepted
**Deciders**: Iris maintainers

## Context

Iris needs Codex-compatible local skills without reopening the repo-controlled
system-fragment surface removed by ADR-0026. Codex uses progressive disclosure:
the model first sees bounded skill metadata, then receives the full `SKILL.md`
only when a skill is selected. Skill changes must appear without restarting.

## Decision

Wayland owns native skills. It discovers and validates Codex-format filesystem
skills, budgets model-visible metadata to 2% of the configured context window,
and refreshes the catalog at each turn boundary.

- Changed catalogs enter the conversation as hidden `Developer` messages.
- Explicitly selected skill bodies enter as hidden `User` messages immediately
  before the visible user prompt.
- Nexus exposes only a generic `submit_turn_with_context` seam and does not know
  about skills.
- Contextual messages persist in the session transcript so resume reproduces
  the provider-visible conversation.
- OpenAI receives the native developer role. Providers without one map the
  contextual message to a user text block; no adapter raises it into the system
  prompt.
- Filesystem discovery, optional metadata, and user enable rules follow Codex's
  native local contract, including its bundled `.system` root and legacy paths.
- Loaded skill directories become trusted extra roots for the read tool only.
  Canonical containment blocks sibling traversal; mutation stays confined to
  the workspace.

The TUI owns discovery UX (`$` and `/skills`) and inserts path-qualified mentions
so duplicate names remain selectable.

## Alternatives Considered

### Add skill metadata to the system prompt

- **Why not**: Repo skills would regain the highest-authority injection path
  removed by ADR-0026.

### Put full skill bodies in every request

- **Why not**: Breaks progressive disclosure and wastes context on unused
  workflows.

### Teach Nexus about skills

- **Why not**: Violates the tier boundary. Discovery, files, refresh policy, and
  contextual assembly belong to Wayland.

## Consequences

- Existing Codex filesystem skills and enable settings work without conversion.
- Metadata stays bounded; full instructions load only on selection.
- Catalog edits are visible at the next turn boundary.
- Transcripts contain hidden contextual messages in addition to visible turns.
- Plugin-distributed, remote-resource, and MCP-dependency installation remain
  separate extension concerns; this decision covers native filesystem skills.
