# ADR-0052: Task workflow v2 - opt-in workflow, always-on guard, and integrated settlement

**Date**: 2026-07-07
**Status**: accepted
**Deciders**: operator + agent design review

## Context

ADR-0028 introduced one runtime object named a task. That object now carries two
different responsibilities:

- the safety guard: dirty-baseline capture, task-scoped dirty-file approvals,
  protected-file snapshots, mutation attribution, and bash violation restore;
- the workflow: durable task records, leases, checkpoint refs, recovery,
  adoption, slash commands, session linkage, badges, and final diffs.

The safety guard is the core ADR-0028 contract: Iris must never silently damage
or entangle a user's uncommitted work. That contract is not optional and cannot
depend on the system prompt.

The workflow is different. It is a user-facing undo/review feature layered on
top of the guard. It writes into `.git`, introduces vocabulary ("task",
"adopt", "settle", "unsettled"), creates recovery notices, and demands an
explicit review decision later. The current default-on workflow makes normal
flows noisy:

- a clean-repo edit followed by a user commit still leaves an orphaned task;
- successful `iris -p --approve` runs leave recoverable records for the next
  interactive session;
- accepting a task deletes the record but keeps unreachable checkpoint refs;
- `/checkpoint` sounds like "save point" but actually settles the task;
- resuming sessions and compaction do not carry enough task context;
- task-scoped approval copy says "all dirty files this task" while bash
  attribution does not honor that grant;
- the planned Milestone 4 subagent tool conflicts with the existing "task"
  term.

This ADR amends ADR-0028, ADR-0030, ADR-0031, ADR-0032, ADR-0035, and ADR-0044
where they describe task persistence, task recovery, task/session linkage,
approval-floor behavior, subagent naming, and compaction carry.

## Decision

### Split the guard from the workflow

Iris keeps the dirty-tree guard always on. The guard is in-memory by default and
enforces ADR-0028's safety contract:

- capture the dirty/untracked baseline at the first mutating tool call;
- require task-scoped approval before `edit` or `write` touches a protected
  dirty path;
- snapshot or otherwise protect dirty files around bash execution;
- detect protected-file changes after bash;
- restore or halt on unapproved violations;
- attribute approved Iris changes to the current in-memory ledger.

The guard is not a user-facing feature. It creates no durable record, holds no
task lease, writes no checkpoint refs, appends no `taskLifecycle` session
entries, and triggers no startup recovery scan when the workflow is disabled.

The **task workflow** is an opt-in feature. It adds durable records, leases,
checkpoint refs, recovery/adoption, slash commands, session linkage, task
badges, task diffs, rollback across process restarts, and checkpoint history.

Configuration:

- add `tasks: Option<bool>` to settings, merged like `microcompaction`;
- default is off;
- project config may opt in with `tasks = true`;
- `/tasks enable` and `/tasks disable` may write the project setting through
  the existing project-owned configuration path;
- workflow-off slash commands return a short enable hint instead of pretending a
  durable workflow exists.

When the workflow is off, Iris may emit a one-time-per-project discovery notice
after a mutating turn: "Iris can checkpoint its changes for undo/review
(`tasks = true`, or `/tasks enable`)." The notice is deterministic, host-side,
and throttled in the existing per-cwd policy store.

### Keep safety non-configurable

Opt-in applies only to the durable workflow. It does not apply to the dirty-file
floor.

ADR-0032's floor ordering remains: destructive checks, dirty-file checks,
sandbox checks, and repository-control checks sit above session grants, project
grants, approval presets, `auto`, and `never`.

No setting may disable the dirty-tree guard. No project file may loosen it. The
model cannot settle, adopt, or waive it.

### Settlement recognizes explicit user actions Iris can observe

ADR-0028 said tasks settle only on explicit user action. This ADR adds two
observable explicit actions.

1. **User commit or full revert settles the task.** At sync barriers and during
   recovery scans, if the task ledger is non-empty and every ledger path is
   clean according to scoped git status, the workflow settles as accepted with
   disposition `external`. A full revert to clean also settles: the user made
   the same explicit repository-state decision. Partial commits or partial
   reverts keep the task open.

2. **Successful print-run completion settles the task.** In print mode, a
   completed mutating run with the workflow enabled settles as accepted with
   disposition `print`. Failure, cancellation, or process crash leaves the
   record for recovery. With the workflow disabled, print mode remains
   guard-only and writes no task state.

Existing explicit settlement actions remain: accept and rollback. Passive
actions such as session exit, session swap, and worktree reanchor do not settle
by themselves.

### Settlement destroys unreachable checkpoint refs

Once a task record is deleted, its checkpoint refs are no longer reachable by
any recovery or UI path. Accepting a task, expiry, rollback, and any other path
that removes the durable record must therefore destroy the whole
`refs/iris/checkpoints/<task-id>/` namespace instead of keeping the last N refs.

Recovery/expiry must include an orphan-ref repair sweep:

- enumerate task checkpoint namespaces;
- preserve namespaces with a live or recorded task;
- preserve namespaces whose lease is held;
- destroy namespaces with no record and no live lease.

The old "keep last checkpoints after accept" behavior is superseded.

### `/checkpoint` means save point, not settlement

`/checkpoint` appends a labeled restore point and keeps the task open. It does
not delete the record, discard approvals, or start a fresh baseline. Rollback
may show these labels as restore-point choices.

Accepting or rolling back remains the way to finish the task. User-facing copy
must avoid using "checkpoint" for settlement.

### Task-scoped dirty approvals cover bash attribution

If a user grants a protected dirty path, or grants all current dirty files for
the task, that grant applies to the whole task. It covers `edit`, `write`, and
bash-attributed changes during a command window.

After bash:

- changed protected paths covered by the task-scoped grant are recorded as
  Iris-attributed ledger changes and checkpointed;
- changed protected paths not covered by the grant remain violations and are
  halted/restored according to the guard;
- the UI copy must describe the actual scope: "all dirty files (this task)",
  never "always".

This accepts a bounded race: a concurrent user edit to an approved dirty file
during the bash window may be attributed to Iris. The race is limited to files
the user explicitly granted to this task. Unapproved dirty files retain today's
halt/restore behavior.

### Task state is carried across compaction

ADR-0044's structured carry grows a task block when a task is open at compaction
time. The source is the guard/workflow state, not the provider summary.

The carry includes:

- the task body preview when available;
- bounded, workspace-relative ledger paths;
- enough wording for rebuilt context to tell the model that Iris has
  unreviewed changes.

The block is persisted as an additive serde-default field on compaction entries
and rendered deterministically beside the prose summary during context rebuild.
Older logs with no field render as before.

Microcompaction/folds remain unchanged. They rewrite superseded tool results and
do not own task state.

### Sessions and recovery stay user-driven

Task records remain the authoritative recovery input. Session logs remain an
audit trail and are never used for enforcement.

Resume UI should surface task linkage:

- session rows may show a task marker when their session id appears in an
  unreviewed task record;
- after resuming a session linked to exactly one recoverable task, Iris may
  offer to resume that task too;
- multiple linked tasks are never guessed between.

Adoption must be side-effect-free when a task is already active: fail before
claiming a lease, appending recovery checkpoints, or reporting success.

### Worktree reanchor must not silently orphan an active task

Reanchoring a session into another worktree while a task is active must route
through the same kind of explicit decision as branch switching: accept, roll
back, or knowingly carry/leave the task. Dropping the guard and lease by
replacing the workspace is not an implicit settlement.

Task records are per worktree git-dir. Checkpoint refs are in the shared common
ref store for linked worktrees. Documentation and repair code must not assume
records and refs are siblings.

### User-facing copy uses review language

The durable workflow is user-facing, so copy must describe what the user can do:

- prefer "unreviewed Iris changes" over "unsettled task";
- prefer "resume task" over "adopt";
- avoid "settle" in UI copy;
- recovery notices should lead with body, file count, and age, not a hex id;
- `/tasks` should be the hub for status, diff, accept, rollback, and recovery.

The internal type may remain `Task` because it is entrenched in the ADRs and
code. User-facing copy should be restrained and action-oriented.

### The Milestone 4 subagent feature must not be called "task"

The durable git-safety workflow already owns "task" in the CLI and recovery UI.
The future model-facing subagent feature reserved in ADR-0035 must not expose a
tool or UI feature named `task`. Its final name is a separate decision, but it
must avoid this collision.

## Alternatives Considered

### Alternative 1: Keep workflow default-on, make recovery quieter
- **Pros**: Smaller implementation change; preserves current rollback durability
  for every mutating user by default.
- **Cons**: Still writes `.git` state for users who never asked for a task
  workflow; still leaves print-mode and clean-commit flows as hidden workflow
  participants; quieting notices hides the state instead of explaining it.
- **Why not**: The workflow is real product surface. It should be introduced by
  opt-in, not discovered through orphan recovery.

### Alternative 2: Add a "quiet mode" for durable tasks
- **Pros**: Keeps crash-durable rollback while suppressing most UI friction.
- **Cons**: Creates invisible `.git` records and refs, then removes the main
  affordance for understanding them. It makes cleanup and recovery harder to
  reason about.
- **Why not**: If the user did not opt into durable undo/review, Iris should not
  create durable workflow state.

### Alternative 3: Give the model task tools for list, settle, and adopt
- **Pros**: The model could answer "what changed?" and could clean up its own
  tasks.
- **Cons**: Settlement and adoption are user safety decisions. Giving the model
  authority to make them violates ADR-0028 and ADR-0031's boundary: the model
  never declares task boundaries or recovery ownership.
- **Why not**: Deterministic read-only context carry is enough for coherence.
  User actions remain the settlement authority.

### Alternative 4: Make the dirty-tree guard configurable
- **Pros**: Maximum operator control; scripts could opt out of all safety
  overhead.
- **Cons**: Removes the central Iris safety claim and lets repo-local or
  session-local state silently weaken uncommitted-work protection.
- **Why not**: Opt-in is for the workflow, not the floor. The dirty guard is a
  product invariant.

### Alternative 5: Treat `git commit` as out of scope
- **Pros**: Strictly preserves ADR-0028's original explicit settlement command
  list.
- **Cons**: Ignores the main way users finish coding work. It converts a clear
  user repository decision into a later recovery prompt.
- **Why not**: A scoped clean ledger is deterministic and explicit enough. It is
  safer than waiting thirty days and expiring toward accepted.

## Consequences

### Positive
- The uncommitted-work safety guarantee remains always on.
- Users who do not opt into durable tasks get no task records, refs, leases,
  recovery nags, badges, or new vocabulary.
- Users who do opt in get a clearer workflow: checkpoints save, commits close,
  print runs do not pollute interactive recovery, and accepted tasks do not leak
  refs.
- Compaction keeps long sessions coherent without granting the model settlement
  authority.
- Future subagent naming avoids a known collision before the feature ships.

### Negative
- Implementation touches several seams: settings, git safety, session logging,
  recovery, print mode, compaction, slash commands, and TUI copy.
- The workflow-off path needs parallel tests to prove the guard still works
  while persistence stays off.
- Some existing docs and tests that assume default-on durable tasks must change.

### Risks
- The guard/workflow split may accidentally remove persistence in workflow-on
  mode or weaken protection in workflow-off mode. Tests must cover both.
- Settle-on-commit depends on accurate scoped git status over ledger paths.
  Partial commits, deletes, renames, and full reverts need explicit coverage.
- Bash approval parity accepts a bounded attribution race on explicitly approved
  dirty files. The UI and ADR text must state that tradeoff.
- Orphan-ref sweeping must be conservative around live leases and linked
  worktrees, because checkpoint refs live in the common ref store.
