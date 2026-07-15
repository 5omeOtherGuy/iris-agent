//! Host-neutral backend for bounded subagent scheduling and isolated worktrees.
//!
//! The runtime owns worker lifecycle, durability, cancellation, groups, output
//! storage, and worktree infrastructure. Hosts supply an [`ExecutorFactory`];
//! factories run on a dedicated scheduler thread and may construct `!Send`
//! executors. [`RuntimeHandle::spawn`] durably queues work and execution proceeds
//! independently of polling or waiting.

mod artifact;
mod error;
mod executor;
mod id;
mod model;
mod orchestration;
mod persistence;
mod runtime;
pub mod worktree;

pub use artifact::{ArtifactStore, FilesystemArtifactStore};
pub use error::RuntimeError;
pub use executor::{
    ApprovalDecision, ApprovalFuture, ApprovalPort, ApprovalRequest, CancellationFlag,
    ExecutorError, ExecutorFactory, ExecutorOutput, LocalExecutorFuture, WorkerContext,
    WorkerExecutor,
};
pub use id::{ApplyPlanId, ArtifactId, GroupId, InstanceId, WorkerId, WorktreeId};
pub use model::{
    ArtifactRef, CapabilityMode, GroupResult, GroupSnapshot, HostPayload, IsolationMode,
    RecoveryPolicy, RuntimeConfig, SCHEMA_VERSION, Usage, WorkerBudgets, WorkerEvent,
    WorkerEventKind, WorkerFilter, WorkerKind, WorkerPolicy, WorkerPriority, WorkerRequest,
    WorkerResult, WorkerSnapshot, WorkerStatus, WorkerWorktree,
};
pub use orchestration::{CandidateSelector, select_candidate};
pub use runtime::{EventSubscription, RuntimeHandle};
