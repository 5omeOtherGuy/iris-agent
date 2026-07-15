//! Managed isolated worktrees, strategies, recovery, apply, and restore.

mod apply;
mod process;
mod registry;
mod restore;
mod service;
mod types;

pub use apply::{
    ApplyChangeKind, ApplyConflict, ApplyConflictKind, ApplyDisposition, ApplyFileKind,
    ApplyFileState, ApplyOperation, ApplyOptions, ApplyPlan, ApplyResult,
};
pub use process::{
    ProcessOutput, ProcessRunner, ProcessSpec, SystemProcessRunner, WorktreeCancellation,
};
pub use restore::{
    RestoreBundle, RestoreEntry, RestoreEntryKind, RestoreRequest, RestoreResult, RestoreSource,
    RestoreTrust,
};
pub use service::{ProcessLiveness, SystemProcessLiveness, WorktreeService};
pub use types::{
    CreationMode, GcReport, MutationEntry, MutationManifest, RemoveOptions, RemoveOutcome,
    StrategyPreference, WorktreeConfig, WorktreeCreateRequest, WorktreeFilter, WorktreeKind,
    WorktreeRecord, WorktreeStatus,
};
