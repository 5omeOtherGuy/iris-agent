# iris-subagent-runtime

Host-neutral worker scheduling and managed worktree infrastructure for coding-agent hosts.

The crate owns bounded scheduling, lifecycle, cancellation, durability, groups, artifact storage,
worktree creation/recovery, reviewed apply, pooling, and restore. It does not select providers,
construct terminal UI, parse Iris settings, or run a model loop. Hosts provide an
`ExecutorFactory`; the factory runs on the scheduler thread and may construct a `!Send` executor.

`RuntimeHandle::spawn` persists the accepted request and queued event, then queues execution before
returning. Polling and waiting only observe. Results are retained for repeated waits, and shutdown
cancels and joins owned work.

Run the independent example:

```sh
cargo run -p iris-subagent-runtime --example standalone
```

Minimal shape:

```rust,no_run
use std::sync::Arc;
use iris_subagent_runtime::{ExecutorFactory, RuntimeConfig, RuntimeHandle, WorkerRequest};

# fn factory() -> Arc<dyn ExecutorFactory> { todo!() }
let runtime = RuntimeHandle::start(
    RuntimeConfig::new(".worker-state"),
    factory(),
    None,
)?;
let id = runtime.spawn(WorkerRequest::read_only("inspect the repository"))?;
let result = runtime.wait_blocking(&id)?;
runtime.shutdown()?;
# Ok::<(), iris_subagent_runtime::RuntimeError>(())
```

Mutable delegated workers are host-authorized and use the `worktree` module. Worktree apply writes
reviewed filesystem paths only; it does not stage, commit, merge, rebase, or cherry-pick.

## Supported worktree slices

- Detached linked worktrees with durable JSONL registry and guarded removal.
- Local Btrfs snapshots when eligible, with linked-worktree fallback.
- Immutable reviewed apply plans with dirty-parent, base-drift, and symlink checks.
- Explicit adoption, pristine pooling, local restore, remote restore ports, and best-of-N groups.

## Deferred

Overlay mounts, privileged snapshot delegates, production cloud restore transport, and automatic
model-authorized apply are outside the crate contract.
