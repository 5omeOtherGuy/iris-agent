# ADR-0063: Extract the subagent runtime and centralize worker scheduling

**Date**: 2026-07-14
**Status**: accepted; supersedes ADR-0035 where noted
**Deciders**: operator + agent implementation review (epic [#459](https://github.com/5omeOtherGuy/iris-agent/issues/459), issue [#635](https://github.com/5omeOtherGuy/iris-agent/issues/635))

## Context

The read-only subagent MVP stores provider state in Wayland and starts execution
from `wait()`. Compaction owns a second worker implementation built from OS
threads and channels. Mutable delegation now also needs worktree lifecycle,
recovery, apply, pooling, restore, and best-of-N without coupling those services
to Iris providers or terminal code. This is the independent consumer that makes
the package split criterion in ADR-0001 and `ARCHITECTURE.md` pay for itself.

## Decision

### Package and ownership

Create the public `iris-subagent-runtime` library crate. It owns host-neutral
worker scheduling, lifecycle, durability, groups, cancellation, resource limits,
artifacts, and worktree infrastructure. It imports no Iris, Nexus, Mimir,
settings, provider, environment-variable, or terminal code. Iris depends inward
on it; Wayland supplies executor and storage adapters and translates events.
Nexus continues to own each child model loop, tool scheduling, approvals,
cancellation races, and transcript validity. Iris CLI/TUI owns commands,
rendering, approval UX, winner selection, and apply authorization.

### One scheduler

Each runtime owns a dedicated thread running a Tokio current-thread runtime and
`LocalSet`. Bounded commands enter through a thread-safe handle. Executor
factories are invoked on the scheduler thread so executors may contain `!Send`
provider and agent state. Blocking provider adapters are constructed and consumed
wholly inside a scheduler-owned blocking task; this does not add a compaction
thread or result channel. `spawn()` durably accepts and queues work before it
returns; `poll()` and every wait operation only observe. Compaction submits
portable and provider-native summaries as higher-priority internal jobs and
retains range planning, its one-job slot, stale-result validation, safe-boundary
application, and fallback policy. Compaction owns no worker thread or result
channel.

### Worktree identity and markers

Mutation-capable delegated workers use managed isolation only. Linked worktrees
are created detached at the exact recorded base commit. The managed directory
and git administrative name derive from a collision-safe opaque worktree ID; no
user-visible branch is created.

Ownership metadata lives under the configured managed root at
`control/<worktree-id>.json`, with a matching marker in the linked worktree's git
administration directory. No marker is written into the checked-out tree.
Deletion validates the ID-derived path, canonical managed-root containment,
source identity, recorded path, and both marker records before invoking the
creation strategy's removal path. This marker placement supersedes ADR-0035's
`.iris-worktree` file in the child working tree.

### Activated worktree slices

ADR-0035's linked-worktree and explicit-apply safety model remains binding. Epic
#459 activates its previously later slices: direct local Btrfs snapshots with
safe linked fallback, owner leases and adoptable recovery, pristine pooling,
local/extensible restore, and best-of-N groups. Apply is an immutable,
content-digested plan and guarded parent mutation independent of whether durable
task workflow is enabled. Only complete apply consumes a candidate; partial or
skipped results remain reviewable. Group selection is durable and may change
until apply; the first complete apply settles the group. Non-winners remain
reviewable until an operator explicitly removes or returns a pristine candidate
to the pool.

### Delegated approval boundary

The model-facing `spawn_subagent` call requires one parent approval and displays
the requested capability and isolation. That decision preauthorizes gated child
tools only inside a managed worktree. A host may instead supply an `ApprovalPort`
for per-call decisions; without either managed preauthorization or a host port,
child approval requests deny. Parent apply is always a separate, non-persistable
approval with the immutable plan as its preview.

Delegated agents set their per-agent workspace policy after construction and
before execution. Filesystem tools reject absolute paths and symlink escapes by
default. `allow_outside_workspace` is an explicit request-level opt-out for
read-only tools; mutation remains confined to the worker workspace. Delegated
shell execution fails closed when kernel filesystem confinement is unavailable,
and the managed worktree apply boundary remains mandatory.

### Explicit exclusions

Do not add overlay mounts, a privileged snapshot delegate, standalone `.git`
copies, ACP compatibility, repo-remote memory identity, a production cloud
restore service, SQLite, or model-authorized apply. Overlay lifetime requires a
new mount-namespace contract; delegated snapshots require a privileged
authentication boundary; cloud restore lacks an accepted transport/auth/trust
contract. The remaining exclusions add unrelated compatibility or storage
systems and are outside epic #459.

## Alternatives Considered

### Keep scheduling and worktrees inside Wayland
- **Pros**: Fewer packages and fewer public contracts.
- **Cons**: Prevents independent use and keeps generic lifecycle tied to Iris.
- **Why not**: Worktree execution and scheduling have a host-neutral consumer
  contract, satisfying ADR-0001's threshold for a split.

### Make Nexus/provider contracts `Send`
- **Pros**: Workers could use ordinary multithreaded Tokio tasks.
- **Cons**: Widens every provider and stream contract for one feature and risks
  moving UI/provider constraints into the core loop.
- **Why not**: A scheduler-thread `LocalSet` supports current `!Send` state while
  exposing a thread-safe public handle.

### Keep compaction's worker implementation
- **Pros**: Smaller migration and independent tuning.
- **Cons**: Two schedulers, duplicated cancellation, and inconsistent recovery.
- **Why not**: Compaction policy does not require execution ownership; ordinary
  internal runtime jobs provide the same bounded scheduling contract.

### Put ownership markers in the child tree
- **Pros**: Easy discovery during a root scan.
- **Cons**: Pollutes diffs as untracked runtime state and can be edited by child
  tools.
- **Why not**: Control-root and git-admin markers are outside model-writable
  content and can be cross-validated before deletion.

## Consequences

### Positive
- Public contracts and compile-time dependency direction prove independent use.
- Background completion, compaction, groups, recovery, and shutdown share one
  bounded scheduler.
- Mutable workers cannot touch the parent before an explicit reviewed apply.
- Detached worktrees avoid branch clutter and runtime markers do not enter diffs.

### Negative
- Public versioned records require compatibility discipline.
- The scheduler thread and blocking git pool add lifecycle plumbing.
- Worktree recovery must reconcile registry, control metadata, git administration,
  and owner leases conservatively.

### Risks
- Forged persisted paths or marker records could make deletion unsafe; require
  canonical containment and matching independent records.
- Cancellation can race completion; arbitrate one terminal state and join owned
  work during shutdown.
- Apply can race parent or child edits; digest and revalidate all inputs
  immediately before transactional writes, preserving preimages for rollback.
