# ADR-0027: Repurpose the trust store as a per-cwd project permission policy

**Date**: 2026-07-02
**Status**: proposed (extends ADR-0005 and ADR-0010; implements #209)
**Deciders**: Iris maintainers, Pi agent session

## Context

ADR-0010 makes allow-always session-scoped and opts `write`/`edit` and destructive `bash` out of persistence, so every file mutation re-prompts every session, forever. Principled, but a daily grind that pushes users toward less careful agents (#209).

ADR-0026 frees the per-cwd trust store from its fragment-gating role. The store is a proven substrate: `~/.iris/trust.json`, keyed by the canonical (symlink-resolved) directory, HOME-owned, atomic writes, fail-closed reads, `IRIS_TRUST_PATH` override. The `/trust` command and its TUI modal, mid-session apply at the inter-turn boundary, and provider rebuild also remain. #209 already names "the trust store from the project-trust work" as where a project-scoped approval policy should live.

## Decision

Reuse the trust store's persistence model as a per-cwd project permission policy. Replace the tri-state trust value with a policy object keyed by canonical cwd:

- Per-tool approval defaults (`write`/`edit`).
- Per-command `bash` allows (exact command or prefix).
- Sandbox posture.

Keep the command name `/trust`, with `/permissions` as an alias. Enforcement stays in Nexus (ADR-0005); the store is data the gate reads, never logic.

Layer precedence, most specific wins:

```
session   (ephemeral; today's allow-always)
   └── project   (persistent; per-cwd, canonical-dir keyed)
          └── global default   (shipped safe baseline)
```

Granularity is per-cwd, not per-git-root: sibling directories do not share policy.

### Invariants (blocking; an approval-gate bypass is a security-critical defect)

1. **The policy store is HOME-owned and canonical-cwd-keyed — never a repo-committed file.** A cloned repo cannot ship a config that pre-approves its own tools; that is an approval-gate bypass, a worse form of the injection surface ADR-0026 removes.
2. **Destructive-command re-prompt (`rm`/`dd`/`mkfs`/...) stays unconditional and non-persistable.** The ADR-0010 floor holds.
3. **Loosening the sandbox per project is a downgrade: explicit user action only, never repo-controlled, never automatic.** Tightening per project is always allowed.
4. **Nothing self-waives.** Policy loosens only through deliberate user action (the ADR-0014 principle: classification is a request, not a grant).

## Alternatives Considered

### Keep allow-always session-only (status quo: ADR-0010)
- **Pros**: Simplest; no cross-session persistence risk.
- **Cons**: The daily re-prompt grind (#209) persists.
- **Why not**: The grind pushes users toward less careful tools; persistence is the point of #209.

### Store the policy in a repo `.iris/settings.json`
- **Pros**: Shareable across a team; travels with the repo.
- **Cons**: A clone pre-approves itself — the exact approval-gate bypass invariant 1 forbids.
- **Why not**: Reintroduces the injection surface ADR-0026 removes, now as privilege escalation.

### Build a new policy store instead of reusing the trust store
- **Pros**: Clean naming, no legacy `trust.json` baggage.
- **Cons**: Duplicates the per-cwd canonical-keyed persistence and the `/trust` command/modal plumbing.
- **Why not**: The trust store already is that substrate; #209 names it. Reuse over reinvention.

## Consequences

### Positive
- Persistent per-project approvals end the re-prompt grind (#209) without weakening the destructive floor.
- Reuses a proven per-cwd store and the `/trust` plumbing freed by ADR-0026.
- Sandbox posture becomes per-project; tightening is free.

### Negative
- `/trust` semantics broaden from a yes/no trust decision to a policy editor; docs and the modal change.
- A `/settings` (or `/trust` list) surface to review and revoke stored grants is now required.

### Risks
- A loosening scoped to repo control by mistake would bypass approvals. Mitigate: invariant 1 plus a test that a repo-shipped file cannot grant.
- Accidental sandbox loosening. Mitigate: invariant 3 — explicit, never automatic.
- Per-cwd keying surprises users who expect a whole repo to share policy. Accepted: per-cwd is the existing store's model and the explicit choice; a per-git-root option can follow if demand appears.
