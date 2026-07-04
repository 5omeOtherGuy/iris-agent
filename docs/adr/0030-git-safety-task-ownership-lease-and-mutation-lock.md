# ADR-0030: Git-safety task ownership — per-task lease and repo mutation lock

**Date**: 2026-07-03
**Status**: accepted
**Deciders**: operator + agent design review (issue [#285](https://github.com/5omeOtherGuy/iris-agent/issues/285))

## Context

ADR-0028's crash-recovery design assumed one agent per repo. The harness
supports multiple agent processes plus the user working concurrently in one
repo, so multiple unsettled task records under `<git-dir>/iris/tasks/` coexist
normally. Records carry no ownership or liveness signal, and recovery
(`recover_and_expire` in `src/wayland/git_safety/settlement.rs`) has four
defects:

- it rehydrates the first-scanned non-expired record as this process's active
  task, adopting another **live** agent's task;
- it appends recovery checkpoints to that foreign chain, entangling two
  agents' work into one chain;
- ref and record writes race other processes with no lock;
- the recovery notice describes the last-scanned record while the first-scanned
  was acted on.

Each violates the ADR-0028 contract: Iris must never silently damage or
entangle a user's uncommitted work.

## Decision

### Per-task advisory flock lease

Each live task holds an advisory `flock` lease (a lock file beside its record)
for the task's lifetime. The lease proves liveness and ownership; a process
crash releases it by construction. No PID, no heartbeat, no daemon.

### Repo-scoped mutation lock

A short-lived repo-scoped lock serializes every ref write and record write
across processes. Held per operation, never across a turn.

### Recovery policy

- Recovery enumerates records and classifies each: **leased** (live foreign
  task — skipped, never adopted, never checkpointed), **lease-free**
  (recoverable orphan), or **legacy** (record predates lock metadata —
  "unknown, not auto-adopted").
- Exactly one lease-free record: auto-adopt with a notice (current UX
  preserved). More than one, or any legacy record: explicit selection required;
  until the resume-task picker ([#288](https://github.com/5omeOtherGuy/iris-agent/issues/288))
  lands, surface a notice listing task ids instead of adopting.
- `recover_and_expire()` splits into `expire_stale()`,
  `recoverable_tasks()`, and `adopt_task(task_id)`. The notice is derived from
  the record actually adopted, fixing the notice/adopt mismatch.

### Record fields

`PersistedTask` gains serde-default `owner` and `lock_protocol` fields. Old
records deserialize to defaults and classify as legacy.

## Alternatives Considered

### Alternative 1: PID + heartbeat file for liveness
- **Pros**: No lock files; readable ownership metadata.
- **Cons**: PID reuse gives false liveness; a crash leaves a stale heartbeat
  until a timeout; timeouts guess.
- **Why not**: `flock` release-on-crash is exact where heartbeats approximate.

### Alternative 2: One repo-wide lock held for the process lifetime
- **Pros**: Trivially excludes all races.
- **Cons**: Blocks legitimate concurrent agents in one repo — a supported
  workflow, and the normal one once worktree isolation (#267/#271) lands.
- **Why not**: The unit of ownership is the task, not the repo.

### Alternative 3: No lock; last-writer-wins on records and refs
- **Pros**: Zero machinery.
- **Cons**: Exactly the entanglement the contract forbids.
- **Why not**: Violates ADR-0028.

## Consequences

### Positive
- A live task cannot be adopted or checkpointed by another process.
- Crash recovery still works with zero daemons: the crashed process's lease is
  gone, so its task is recoverable.
- The API split (`recoverable_tasks` / `adopt_task`) is the seam the
  resume-task picker (ADR-0031, #288) plugs into.

### Negative
- Lock files beside the records add a small on-disk surface.
- Multiple recoverable orphans degrade to a notice until #288 ships the picker.

### Risks
- Advisory locks on exotic filesystems (some network mounts) may not enforce;
  the degrade direction is a spurious "unknown" classification, never
  adoption of a live task.
