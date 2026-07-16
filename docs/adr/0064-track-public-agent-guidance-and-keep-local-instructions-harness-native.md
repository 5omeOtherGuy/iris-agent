# ADR-0064: Track public agent guidance and keep local instructions harness-native

**Date**: 2026-07-15
**Status**: accepted; implemented by issue #642 (amends ADR-0026)
**Deciders**: Iris maintainers, user, Iris agent session

## Context

Iris repository instructions and several repo-specific skills currently live in ignored or harness-specific paths. Plain Git worktrees receive tracked files only, while Codex, Claude Code, Pi, and Iris use different instruction filenames and local-layer semantics. Copying one private file into every harness would either drift or misrepresent replacement as addition.

## Decision

Track `AGENTS.md` as the self-contained public repository instruction source. Track a regular `CLAUDE.md` that imports it, keep canonical repo skills under `.agents/skills/`, and add only the harness projections that are required and verified.

Keep personal project instructions ignored and use harness-native layers:

- `AGENTS.override.md` replaces the same-directory public base in Codex and Iris.
- Iris appends the first non-empty local candidate from `AGENTS.local.md`, then `CLAUDE.local.md`.
- Claude Code uses additive `CLAUDE.local.md`.
- Pi uses trusted project `.pi/APPEND_SYSTEM.md` for an additive local prompt.

For each directory, Iris selects the first non-empty regular base from `AGENTS.override.md`, `AGENTS.md`, then `CLAUDE.md`, followed by the local candidate. Existing root-to-leaf ordering, bounded reads, regular-file checks, project symlink refusal, and warning behavior remain unchanged.

Tracked instructions, skills, and projections are the portable worktree baseline. Harness-managed worktrees may copy ignored local files through `.worktreeinclude`; plain Git worktrees use a repository wrapper that copies only regular files and refuses overwrite conflicts.

## Implementation

- `AGENTS.md`, `CLAUDE.md`, `.agents/skills/`, and `.claude/skills/` are the tracked public baseline. No `.pi/skills` projections remain.
- `src/wayland/system_prompt/` implements the base/local candidate order, bounded regular-file reads, diagnostics, and shared-hub-aware onboarding.
- `.worktreeinclude` names the four supported ignored instruction files. `scripts/worktree-create.sh` runs preflight, creates from `origin/main`, and copies only those regular sources.
- `scripts/check-repo-guidance.sh` validates guide size, skill metadata, projections, and duplicate-root absence in the local gate and CI.

## Alternatives Considered

### Keep the full repository guide ignored
- **Pros**: No public configuration change.
- **Cons**: Fresh clones and worktrees lack project guidance; agents can operate under different rules.
- **Why not**: Project rules and skills must travel with the code they govern.

### Duplicate instructions and skills per harness
- **Pros**: Uses each harness's native directory directly.
- **Cons**: Copies drift and require parallel review.
- **Why not**: One canonical source plus verified adapters is smaller and auditable.

### Standardize one local filename across every harness
- **Pros**: One apparent user-facing convention.
- **Cons**: Codex replacement, Claude additive loading, and Pi prompt append are not equivalent.
- **Why not**: A portable name cannot create unsupported semantics.

## Consequences

### Positive
- Fresh clones and ordinary worktrees receive public guidance and repo skills.
- Public, local, replacing, and additive layers have explicit ownership.
- Iris aligns with Codex replacement behavior without losing an additive local layer.
- Canonical skill sources stop drifting across harness directories.

### Negative
- Harness-specific adapter files remain necessary.
- Local instructions need separate propagation because Git does not copy ignored files into plain worktrees.
- Claude Code and Pi cannot provide exact parity with both Iris local modes.

### Risks
- A local override can hide public guidance. Reserve it for deliberate full replacement; prefer additive local files for personal notes.
- Managed-worktree copy behavior can be mistaken for plain Git behavior. Verify both paths independently.
- Tracked symlink projections can fail on unsupported filesystems. Use imports or managed projections when link support is not verified.
