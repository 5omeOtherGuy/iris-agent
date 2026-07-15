use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{GroupId, HostPayload, InstanceId, SCHEMA_VERSION, WorkerId, WorktreeId};

fn schema_version() -> u32 {
    SCHEMA_VERSION
}

/// Actual worktree creation backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CreationMode {
    /// Detached linked `git worktree`.
    #[default]
    Linked,
    /// Direct local Btrfs subvolume snapshot.
    BtrfsSnapshot,
}

impl CreationMode {
    /// Stable persisted token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Linked => "linked",
            Self::BtrfsSnapshot => "btrfs_snapshot",
        }
    }
}

/// Requested creation strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StrategyPreference {
    /// Probe Btrfs and fall back cleanly to linked creation.
    #[default]
    Auto,
    /// Require linked worktree creation.
    Linked,
    /// Try Btrfs first, then use linked fallback.
    BtrfsPreferred,
}

/// Purpose of a managed worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorktreeKind {
    /// Delegated worker execution.
    #[default]
    Worker,
    /// Restored session.
    Restore,
    /// Pristine prewarmed capacity.
    Pool,
    /// Top-level isolated session.
    Session,
}

/// Durable managed-worktree lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorktreeStatus {
    /// Owned by a live runtime instance.
    #[default]
    Alive,
    /// Dead-owner state that passed structural validation.
    Adoptable,
    /// Explicitly retained but ignored by recovery.
    Ignored,
    /// A complete apply consumed the candidate.
    Applied,
    /// Removed record retained as a latest-wins tombstone.
    Removed,
    /// Record failed validation and was left untouched.
    Corrupt,
}

/// Versioned latest-wins worktree registry record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorktreeRecord {
    /// Registry schema.
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    /// Stable managed ID.
    pub id: WorktreeId,
    /// Canonical managed worktree path.
    pub path: PathBuf,
    /// Canonical source repository root.
    pub source_repo: PathBuf,
    /// Display repository name.
    pub repo_name: String,
    /// Purpose.
    #[serde(default)]
    pub kind: WorktreeKind,
    /// Backend actually used.
    #[serde(default)]
    pub creation_mode: CreationMode,
    /// Exact requested ref, when supplied.
    #[serde(default)]
    pub git_ref: Option<String>,
    /// Exact base commit used for creation.
    pub base_commit: String,
    /// Host session link.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Worker link.
    #[serde(default)]
    pub worker_id: Option<WorkerId>,
    /// Group link.
    #[serde(default)]
    pub group_id: Option<GroupId>,
    /// Whether the host explicitly selected this group candidate for apply.
    #[serde(default)]
    pub selected: bool,
    /// Whether a complete apply consumed this candidate before any later cleanup.
    #[serde(default)]
    pub applied_to_parent: bool,
    /// Parent worker link.
    #[serde(default)]
    pub parent_worker_id: Option<WorkerId>,
    /// Owning process ID for diagnostics.
    pub owner_pid: u32,
    /// Owning instance lease ID; PID alone is never trusted.
    pub owner_instance_id: InstanceId,
    /// Creation time, Unix milliseconds.
    pub created_at_ms: u64,
    /// Last access time, Unix milliseconds.
    #[serde(default)]
    pub last_accessed_at_ms: Option<u64>,
    /// Current latest-wins status.
    #[serde(default)]
    pub status: WorktreeStatus,
    /// Host extension metadata.
    #[serde(default)]
    pub metadata: HostPayload,
}

/// Input for creating one isolated worktree.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct WorktreeCreateRequest {
    /// Source working tree.
    pub source: PathBuf,
    /// Base commit or ref; defaults to source `HEAD`.
    pub base: Option<String>,
    /// Backend preference.
    pub strategy: StrategyPreference,
    /// Purpose.
    pub kind: WorktreeKind,
    /// Optional session link.
    pub session_id: Option<String>,
    /// Optional worker link.
    pub worker_id: Option<WorkerId>,
    /// Optional group link.
    pub group_id: Option<GroupId>,
    /// Optional parent-worker link.
    pub parent_worker_id: Option<WorkerId>,
    /// Host extension metadata.
    pub metadata: HostPayload,
}

impl WorktreeCreateRequest {
    /// Creates a worker worktree request at source `HEAD` with safe auto strategy.
    #[must_use]
    pub fn worker(source: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            base: None,
            strategy: StrategyPreference::Auto,
            kind: WorktreeKind::Worker,
            session_id: None,
            worker_id: None,
            group_id: None,
            parent_worker_id: None,
            metadata: HostPayload::default(),
        }
    }
}

/// Configuration for the reusable worktree service.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct WorktreeConfig {
    /// Managed storage root. Hosts resolve environment/config before construction.
    pub root: PathBuf,
    /// Git/filesystem subprocess timeout.
    pub process_timeout: Duration,
    /// Maximum live managed worktrees.
    pub max_worktrees: usize,
    /// Maximum pristine prewarmed worktrees.
    pub max_pool_size: usize,
    /// Additional roots that the managed root must not equal or contain (for example a home directory).
    pub protected_roots: Vec<PathBuf>,
}

impl WorktreeConfig {
    /// Creates bounded defaults for a caller-selected root.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            process_timeout: Duration::from_secs(60),
            max_worktrees: 64,
            max_pool_size: 4,
            protected_roots: Vec::new(),
        }
    }
}

/// List filter for managed worktrees.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct WorktreeFilter {
    /// Restrict by source repository.
    pub source_repo: Option<PathBuf>,
    /// Restrict by purpose.
    pub kind: Option<WorktreeKind>,
    /// Restrict by lifecycle.
    pub status: Option<WorktreeStatus>,
    /// Include removed tombstones.
    pub include_removed: bool,
}

/// Guarded removal options.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct RemoveOptions {
    /// Report without modifying filesystem or registry.
    pub dry_run: bool,
    /// Allow removal of a live owner after all structural checks pass.
    pub force: bool,
}

impl RemoveOptions {
    /// Creates options for an explicit forced removal.
    #[must_use]
    pub const fn force() -> Self {
        Self {
            dry_run: false,
            force: true,
        }
    }

    /// Creates options for a forced dry-run validation.
    #[must_use]
    pub const fn dry_run_force() -> Self {
        Self {
            dry_run: true,
            force: true,
        }
    }
}

/// Removal outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RemoveOutcome {
    /// Dry-run validated the candidate.
    WouldRemove(PathBuf),
    /// Candidate was removed and tombstoned.
    Removed(PathBuf),
    /// Candidate was already removed.
    AlreadyRemoved,
}

/// Garbage-collection report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GcReport {
    /// Candidates removed.
    pub removed: Vec<WorktreeId>,
    /// Live-owner candidates skipped.
    pub skipped_live: Vec<WorktreeId>,
    /// Dead-owner candidates marked adoptable.
    pub adoptable: Vec<WorktreeId>,
    /// Corrupt candidates left untouched.
    pub corrupt: Vec<WorktreeId>,
    /// Whether `git worktree prune` was skipped because adoptable state exists.
    pub prune_suppressed: bool,
}

/// One path attributed to a delegated worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MutationEntry {
    /// Workspace-relative path.
    pub path: PathBuf,
    /// Optional prior path for a rename. Apply normalizes this to delete/create.
    #[serde(default)]
    pub renamed_from: Option<PathBuf>,
}

impl MutationEntry {
    /// Records one attributed path.
    #[must_use]
    pub fn path(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            renamed_from: None,
        }
    }

    /// Records a rename as reviewed delete/create operations.
    #[must_use]
    pub fn rename(from: impl Into<PathBuf>, to: impl Into<PathBuf>) -> Self {
        Self {
            path: to.into(),
            renamed_from: Some(from.into()),
        }
    }
}

/// Authoritative worker mutation manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MutationManifest {
    /// Attributed paths, including untracked files.
    #[serde(default)]
    pub entries: Vec<MutationEntry>,
}

impl MutationManifest {
    /// Creates a manifest from attributed entries.
    #[must_use]
    pub fn new(entries: Vec<MutationEntry>) -> Self {
        Self { entries }
    }

    /// Returns a deduplicated set including rename sources.
    pub(crate) fn paths(&self) -> BTreeSet<PathBuf> {
        let mut paths = BTreeSet::new();
        for entry in &self.entries {
            paths.insert(entry.path.clone());
            if let Some(from) = &entry.renamed_from {
                paths.insert(from.clone());
            }
        }
        paths
    }
}
