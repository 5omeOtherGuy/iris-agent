# ADR-0058: Configure mutation safety and require native jj consent

**Date**: 2026-07-11
**Status**: accepted
**Deciders**: operator + agent design review

## Context

ADR-0052 made the dirty-tree guard always on and native jj detection implicit.
That policy can impose repository snapshots, operation checks, restoration, and
settlement state on users who do not want Iris to manage repository mutations.
Native jj support also changes failure and rollback behavior merely because a
compatible executable appears on `PATH`.

The operator needs explicit control without allowing project-controlled config
to weaken host policy. Native jj consent must be scoped to the workspace where
it was granted.

This ADR supersedes ADR-0052's "always-on guard" and "safety
non-configurable" decisions. Its settlement-authority and task-workflow rules
remain in force when mutation safety is enabled.

## Decision

### Add a global mutation-safety switch

`mutationSafety` is a global-only setting and defaults to `true`. Project config
cannot override it.

When enabled, the existing mutation guard remains the single enforcement path.
When disabled, Wayland supplies no `MutationGuard` to Nexus. Iris therefore
performs no guard preflight, dirty-file gate approval, snapshots, attribution,
restoration, or guard settlement checks.

The durable task workflow remains a separate `tasks` setting. It is effective
only while mutation safety is enabled. Changing mutation safety is refused while
an in-memory or durable task is active; the user must finish the task first.

### Require per-workspace consent for native jj

Wayland discovers native jj only when both conditions hold:

- a compatible `jj` command is discoverable;
- `jj root` confirms the active canonical workspace.

Discovery does not activate the backend. Consent is stored as an optional
boolean in the HOME-owned, canonical-cwd policy record:

- absent: undecided;
- `true`: enabled;
- `false`: declined or disabled.

A qualifying interactive workspace with no decision opens a one-time
approval-style prompt. It states that native mode uses jj snapshots, operation
tracking, external-operation halts, and rollback/restoration. Enable and decline
are explicit actions. Existing startup modals remain first; the jj prompt is
queued after them.

The settings panel exposes both controls. Native jj is disabled when mutation
safety is off or discovery fails. Changing native jj is refused while a task is
active. A jj workspace without native consent uses the existing degraded,
file-only guard and presents reduced guarantees rather than silently selecting a
Git backend.

## Alternatives Considered

### Keep the guard always on
- **Pros**: Preserves the strongest default guarantee.
- **Cons**: Gives users no escape from guard overhead or failure modes.
- **Why not**: The operator explicitly requires a complete off switch.

### Store native jj consent in project config
- **Pros**: Travels with the repository.
- **Cons**: A cloned repository could opt the host into mutation management.
- **Why not**: Consent belongs to the local operator, not repository content.

### Enable native jj whenever it is detected
- **Pros**: No setup prompt; strongest available integration immediately.
- **Cons**: Executable discovery silently changes snapshots, halts, and rollback.
- **Why not**: Native repository integration requires informed consent.

## Consequences

### Positive
- Users can disable every mutation-gating stage from one persistent control.
- Native jj behavior is explicit, workspace-scoped, and reversible.
- Repository config cannot disable the master guard or grant native jj consent.
- Backend changes cannot orphan an active task.

### Negative
- Disabling mutation safety removes Iris's dirty-work protection guarantees.
- Declining native jj in a jj workspace provides only degraded file protection.
- Startup needs a queued follow-up modal.

### Risks
- New guard call sites could accidentally ignore the master switch; tests must
  cover enabled and disabled execution.
- Discovery can change as `PATH` or the workspace changes. Availability is
  recomputed, while the stored decision remains scoped to the canonical cwd.
