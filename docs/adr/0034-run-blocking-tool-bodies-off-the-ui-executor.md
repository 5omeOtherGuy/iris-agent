# ADR-0034: Run blocking tool bodies off the UI executor with channel-bridged streaming

**Date**: 2026-07-04
**Status**: accepted (extends ADR-0002; amends ADR-0008's bash state ownership)
**Deciders**: operator + agent live-verification session (Milestone 6 follow-up)

## Context

The TUI runs on a current-thread Tokio runtime: the session loop, the turn
future, and every tool future share one thread. `BashTool::execute` called the
synchronous `bash::execute` (20ms poll loop, pump threads) directly inside its
future, so while a shell command ran the turn loop's `tokio::select!` was never
polled. Live verification of Milestone 6 (ADR-0029) showed the consequence:
`ToolStarted`/`ToolOutputDelta` events queued unapplied, the live exec cell
never rendered, and the working indicator froze until the command finished —
in both pager and inline modes. The read-only tools already wrapped their
blocking bodies in `spawn_blocking`; bash did not, and ADR-0002 only noted
"some blocking operations still need careful wrapping" without deciding how.

## Decision

Blocking tool bodies must not run on the UI executor. Concretely (PR #301):

- `bash::execute` runs on `tokio::task::spawn_blocking`, matching the
  read-only tools' pattern in `src/tools/registry.rs`.
- The UI output sink is not `Send`; the blocking body streams chunks over a
  `tokio::sync::mpsc` channel and the async side forwards them to the real
  sink in a `select!` loop while awaiting the join handle. The bash wait loop
  emits deltas at its poll cadence during execution, not only after
  completion. Once the channel closes, the recv branch is disabled (no
  polling a closed receiver on a current-thread runtime).
- `ToolState.bash` becomes `Arc<Mutex<BashState>>`. `spawn_blocking` tasks
  cannot be aborted: on cancellation the future is dropped while the detached
  task finishes, and shared ownership keeps persistent sessions/jobs from
  being lost with the dropped future. The process itself is still stopped by
  bash's own `CancellationToken` path (process-group kill, ADR-0008).
- A poisoned bash state mutex is a tool error ("restart the session"), never
  silently recovered — a panic mid-mutation may have left sessions/jobs
  inconsistent.

Bash stays exclusive (sequential-by-default, ADR-0002), so the lock does not
contend in normal operation.

## Alternatives Considered

### Alternative 1: Multi-thread Tokio runtime for the TUI
- **Pros**: No per-tool wrapping; blocking bodies stop starving the loop.
- **Cons**: Loses the single-threaded reasoning model the TUI loop is built
  on; every shared UI structure would need `Send + Sync` review; hides rather
  than fixes blocking-in-async bugs.
- **Why not**: Larger blast radius than wrapping the one offending tool, and
  the architecture doc's async model assumes the current-thread loop.

### Alternative 2: Rewrite bash on `tokio::process` (fully async)
- **Pros**: No blocking body at all; native async waits.
- **Cons**: Rewrites the hardened session/process-group/Landlock machinery
  (ADR-0008) that is deliberately synchronous and well-tested.
- **Why not**: Disproportionate risk for the same observable behavior.

### Alternative 3: Keep blocking, accept frozen UI during exec
- **Pros**: No change.
- **Cons**: Live exec streaming (a Milestone 6 claim) stays dead code; the
  spinner freezes; long commands look like hangs.
- **Why not**: Contradicts shipped UX and the existing streaming renderer.

## Consequences

### Positive
- Live exec output streams into the SHELL panel while a command runs; spinner
  and timer keep ticking (verified live post-merge).
- The rule is now explicit: any future tool with a blocking body follows the
  same `spawn_blocking` + channel-bridge pattern.

### Negative
- Bash state is behind a lock and shared with a possibly-detached task;
  reasoning about state after cancellation is subtler than exclusive
  ownership.
- Delta ordering and completion draining depend on the bridge loop; the
  registry tests (executor-liveness, mid-command deltas, closed-channel
  completion, session persistence) are the guard.

### Risks
- A cancelled `spawn_blocking` task keeps running until the process-group
  kill lands; a fast follow-up bash call could briefly contend the state
  lock. Mitigated by bash's exclusive scheduling and the kill path.
- Poison-as-error means a panic in bash internals disables shell sessions
  until restart; honest failure was chosen over silent inconsistency.
