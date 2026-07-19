use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ApplyPlanId, ArtifactId, GroupId, WorkerId, WorktreeId};

/// Current schema version for public and persisted runtime records.
pub const SCHEMA_VERSION: u32 = 1;

fn schema_version() -> u32 {
    SCHEMA_VERSION
}

/// Host-owned data carried through the neutral runtime boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct HostPayload {
    /// Payload schema selected by the host.
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    /// Stable host-defined payload kind.
    pub kind: String,
    /// Opaque host data. Runtime policy must not be encoded here.
    #[serde(default)]
    pub value: Value,
}

impl Default for HostPayload {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            kind: "none".to_string(),
            value: Value::Null,
        }
    }
}

/// Capability classes granted to a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CapabilityMode {
    /// Read-only filesystem and introspection capabilities.
    #[default]
    ReadOnly,
    /// Read plus direct filesystem mutation, without command execution.
    ReadWrite,
    /// Read plus command execution, without direct edit/write capabilities.
    Execute,
    /// Union of read, direct mutation, and command execution.
    All,
}

impl CapabilityMode {
    /// Returns whether the mode can mutate repository state.
    #[must_use]
    pub const fn is_mutation_capable(self) -> bool {
        !matches!(self, Self::ReadOnly)
    }

    /// Returns whether `self` is no broader than `parent`.
    #[must_use]
    pub const fn is_within(self, parent: Self) -> bool {
        match parent {
            Self::All => true,
            Self::ReadOnly => matches!(self, Self::ReadOnly),
            Self::ReadWrite => matches!(self, Self::ReadOnly | Self::ReadWrite),
            Self::Execute => matches!(self, Self::ReadOnly | Self::Execute),
        }
    }
}

/// Isolation selected for worker execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IsolationMode {
    /// Use a validated host workspace. Allowed only for read-only workers.
    #[default]
    None,
    /// Create or resume a managed isolated worktree.
    Worktree,
}

/// Scheduling priority. Internal priority is bounded by scheduler fairness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkerPriority {
    /// Ordinary delegated work.
    #[default]
    Normal,
    /// Urgent internal work such as compaction.
    InternalUrgent,
}

/// Runtime-understood worker category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "type", content = "name")]
#[non_exhaustive]
pub enum WorkerKind {
    /// General host worker.
    #[default]
    General,
    /// Read-oriented exploration worker.
    Explore,
    /// Review worker.
    Review,
    /// Portable model-backed compaction job.
    CompactionPortable,
    /// Provider-native compaction job.
    CompactionNative,
    /// Session/code restore job.
    Restore,
    /// Host extension category.
    Host(String),
}

/// Recovery disposition for an interrupted worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RecoveryPolicy {
    /// Mark interrupted and require an explicit compatible resume.
    #[default]
    Adoptable,
    /// Mark interrupted and discard the execution result.
    Discard,
    /// Mark failed because the job is not resumable.
    Fail,
}

/// Typed path and nesting policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkerPolicy {
    /// Effective capability grant.
    #[serde(default)]
    pub capability: CapabilityMode,
    /// Maximum grant inherited from the parent/session/profile.
    #[serde(default = "default_all")]
    pub parent_capability: CapabilityMode,
    /// Requested isolation.
    #[serde(default)]
    pub isolation: IsolationMode,
    /// Explicit validated working directory; mutually exclusive with worktree isolation.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Tool-name narrowing applied after capability filtering.
    #[serde(default)]
    pub tool_allowlist: Vec<String>,
    /// Explicitly permits read-only filesystem tools to follow paths outside the workspace.
    ///
    /// Mutation and shell writes remain confined to the effective workspace.
    #[serde(default)]
    pub allow_outside_workspace: bool,
    /// Current delegation depth.
    #[serde(default)]
    pub nesting_depth: u32,
    /// Maximum allowed delegation depth.
    #[serde(default = "default_max_depth")]
    pub max_nesting_depth: u32,
}

fn default_all() -> CapabilityMode {
    CapabilityMode::All
}

const fn default_max_depth() -> u32 {
    2
}

impl Default for WorkerPolicy {
    fn default() -> Self {
        Self {
            capability: CapabilityMode::ReadOnly,
            parent_capability: CapabilityMode::All,
            isolation: IsolationMode::None,
            cwd: None,
            tool_allowlist: Vec::new(),
            allow_outside_workspace: false,
            nesting_depth: 0,
            max_nesting_depth: default_max_depth(),
        }
    }
}

/// Hard limits enforced by the runtime or reported to the host executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[non_exhaustive]
pub struct WorkerBudgets {
    /// Wall-clock execution limit in milliseconds.
    #[serde(default)]
    pub wall_clock_ms: Option<u64>,
    /// Provider/model round limit.
    #[serde(default)]
    pub max_provider_rounds: Option<u64>,
    /// Tool round-trip limit.
    #[serde(default)]
    pub max_tool_rounds: Option<u64>,
    /// Aggregate input/output token limit.
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Maximum inline output bytes before artifact offload.
    #[serde(default)]
    pub max_inline_output_bytes: Option<usize>,
    /// Maximum stored output bytes.
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
    /// Maximum artifact count emitted by the executor.
    #[serde(default)]
    pub max_artifacts: Option<usize>,
}

/// Versioned, host-neutral worker request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkerRequest {
    /// Runtime record schema.
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    /// Worker category.
    #[serde(default)]
    pub kind: WorkerKind,
    /// Full worker prompt or instruction.
    pub prompt: String,
    /// Short operator-facing description.
    #[serde(default)]
    pub description: String,
    /// Scheduling priority.
    #[serde(default)]
    pub priority: WorkerPriority,
    /// Typed policy.
    #[serde(default)]
    pub policy: WorkerPolicy,
    /// Resource limits.
    #[serde(default)]
    pub budgets: WorkerBudgets,
    /// Recovery behavior after process restart.
    #[serde(default)]
    pub recovery: RecoveryPolicy,
    /// Optional parent worker.
    #[serde(default)]
    pub parent_worker_id: Option<WorkerId>,
    /// Optional session linkage owned by the host.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Optional route selected by the host.
    #[serde(default)]
    pub route_id: Option<String>,
    /// Optional profile selected by the host.
    #[serde(default)]
    pub profile_id: Option<String>,
    /// Explicit resume source.
    #[serde(default)]
    pub resume_from: Option<WorkerId>,
    /// Host extension payload.
    #[serde(default)]
    pub host: HostPayload,
}

impl WorkerRequest {
    /// Constructs a minimal read-only request.
    #[must_use]
    pub fn read_only(prompt: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            kind: WorkerKind::General,
            prompt: prompt.into(),
            description: String::new(),
            priority: WorkerPriority::Normal,
            policy: WorkerPolicy::default(),
            budgets: WorkerBudgets::default(),
            recovery: RecoveryPolicy::Adoptable,
            parent_worker_id: None,
            session_id: None,
            route_id: None,
            profile_id: None,
            resume_from: None,
            host: HostPayload::default(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), crate::RuntimeError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(crate::RuntimeError::InvalidRequest(format!(
                "unsupported worker schema version {}",
                self.schema_version
            )));
        }
        if self.prompt.trim().is_empty() {
            return Err(crate::RuntimeError::InvalidRequest(
                "worker prompt must not be empty".to_string(),
            ));
        }
        if !self
            .policy
            .capability
            .is_within(self.policy.parent_capability)
        {
            return Err(crate::RuntimeError::InvalidRequest(
                "worker capability exceeds parent/session/profile grant".to_string(),
            ));
        }
        if self.policy.cwd.is_some() && self.policy.isolation == IsolationMode::Worktree {
            return Err(crate::RuntimeError::InvalidRequest(
                "cwd and worktree isolation are mutually exclusive".to_string(),
            ));
        }
        if self.policy.capability.is_mutation_capable()
            && self.policy.isolation != IsolationMode::Worktree
        {
            return Err(crate::RuntimeError::InvalidRequest(
                "mutation-capable workers require worktree isolation".to_string(),
            ));
        }
        if self.policy.nesting_depth > self.policy.max_nesting_depth {
            return Err(crate::RuntimeError::InvalidRequest(
                "worker nesting depth limit exceeded".to_string(),
            ));
        }
        Ok(())
    }
}

/// Worker lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkerStatus {
    /// Durably accepted and waiting for scheduler capacity.
    Queued,
    /// Executor construction or worktree initialization is in progress.
    Initializing,
    /// Executor is running.
    Running,
    /// Worker is blocked on a host approval decision.
    WaitingForApproval,
    /// Worker completed successfully.
    Completed,
    /// Worker failed.
    Failed,
    /// Worker was cancelled.
    Cancelled,
    /// Runtime stopped before completion.
    Interrupted,
    /// Interrupted worktree state can be explicitly adopted.
    Adoptable,
}

impl WorkerStatus {
    /// Returns whether no further execution transition is expected.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Interrupted | Self::Adoptable
        )
    }
}

/// Aggregate provider/tool usage reported by an executor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Usage {
    /// Input tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// Output tokens.
    #[serde(default)]
    pub output_tokens: u64,
    /// Provider/model rounds.
    #[serde(default)]
    pub provider_rounds: u64,
    /// Tool rounds.
    #[serde(default)]
    pub tool_rounds: u64,
}

impl Usage {
    /// Constructs usage counters.
    #[must_use]
    pub const fn new(
        input_tokens: u64,
        output_tokens: u64,
        provider_rounds: u64,
        tool_rounds: u64,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            provider_rounds,
            tool_rounds,
        }
    }

    /// Total input and output tokens.
    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// Reference to full content kept outside an inline result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ArtifactRef {
    /// Content-addressed ID.
    pub id: ArtifactId,
    /// Stored byte count.
    pub bytes: usize,
    /// Optional media type.
    #[serde(default)]
    pub media_type: Option<String>,
}

/// Worktree metadata returned with a worker result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkerWorktree {
    /// Managed worktree identifier.
    pub id: WorktreeId,
    /// Host-displayable managed path.
    pub path: PathBuf,
    /// Recorded base commit.
    pub base_commit: String,
    /// Actual creation strategy.
    pub creation_mode: String,
}

impl WorkerWorktree {
    /// Constructs worker worktree metadata.
    #[must_use]
    pub fn new(
        id: WorktreeId,
        path: PathBuf,
        base_commit: impl Into<String>,
        creation_mode: impl Into<String>,
    ) -> Self {
        Self {
            id,
            path,
            base_commit: base_commit.into(),
            creation_mode: creation_mode.into(),
        }
    }
}

/// Runtime event kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "data")]
#[non_exhaustive]
pub enum WorkerEventKind {
    /// Lifecycle state changed.
    Status(WorkerStatus),
    /// Human-readable bounded progress.
    Progress { message: String },
    /// Executor is waiting for a host approval.
    ApprovalWait {
        request_id: String,
        summary: String,
        timeout_ms: Option<u64>,
    },
    /// Cumulative usage changed.
    Usage(Usage),
    /// An artifact was stored.
    Artifact(ArtifactRef),
    /// Executor emitted a host-defined event.
    Host(HostPayload),
    /// Worker reached its single terminal state.
    Completed,
}

/// Sequenced worker event persisted and replayed by the runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkerEvent {
    /// Record schema.
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    /// Worker ID.
    pub worker_id: WorkerId,
    /// Monotonic sequence within this worker.
    pub sequence: u64,
    /// Unix timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// Event payload.
    pub kind: WorkerEventKind,
}

/// Stable terminal worker result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkerResult {
    /// Record schema.
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    /// Worker ID.
    pub worker_id: WorkerId,
    /// Terminal status.
    pub status: WorkerStatus,
    /// Bounded inline summary.
    #[serde(default)]
    pub summary: String,
    /// Full output when it fits inline.
    #[serde(default)]
    pub inline_output: Option<String>,
    /// Stored full output and executor artifacts.
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
    /// Aggregate usage.
    #[serde(default)]
    pub usage: Usage,
    /// Attributed changed paths.
    #[serde(default)]
    pub changed_paths: Vec<PathBuf>,
    /// Managed worktree, when any.
    #[serde(default)]
    pub worktree: Option<WorkerWorktree>,
    /// Apply candidate created by the host adapter.
    #[serde(default)]
    pub apply_plan_id: Option<ApplyPlanId>,
    /// Host result extension.
    #[serde(default)]
    pub host: HostPayload,
    /// Failure/cancellation reason.
    #[serde(default)]
    pub message: Option<String>,
}

/// Non-consuming current view of a worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkerSnapshot {
    /// Accepted request.
    pub request: WorkerRequest,
    /// Worker ID.
    pub worker_id: WorkerId,
    /// Current status.
    pub status: WorkerStatus,
    /// Group membership.
    #[serde(default)]
    pub group_id: Option<GroupId>,
    /// Cumulative usage.
    #[serde(default)]
    pub usage: Usage,
    /// Terminal result, if available.
    #[serde(default)]
    pub result: Option<WorkerResult>,
    /// Latest event sequence.
    #[serde(default)]
    pub last_event_sequence: u64,
}

/// Filter for runtime list operations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WorkerFilter {
    /// Restrict to this status.
    #[serde(default)]
    pub status: Option<WorkerStatus>,
    /// Restrict to this group.
    #[serde(default)]
    pub group_id: Option<GroupId>,
    /// Restrict to this host session.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Include internal jobs.
    #[serde(default)]
    pub include_internal: bool,
}

/// Snapshot of a group and all member workers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupSnapshot {
    /// Group ID.
    pub group_id: GroupId,
    /// Member worker IDs in request order.
    pub workers: Vec<WorkerId>,
    /// Current member snapshots.
    pub snapshots: Vec<WorkerSnapshot>,
}

/// Result returned by group wait.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupResult {
    /// Group ID.
    pub group_id: GroupId,
    /// One terminal result per member.
    pub results: Vec<WorkerResult>,
}

/// Runtime scheduling and persistence limits.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RuntimeConfig {
    /// Durable state directory.
    pub state_dir: PathBuf,
    /// Bounded scheduler command capacity.
    pub command_capacity: usize,
    /// Maximum queued plus running workers.
    pub queue_capacity: usize,
    /// Global concurrent worker limit.
    pub global_concurrency: usize,
    /// Default per-group concurrent worker limit.
    pub per_group_concurrency: usize,
    /// Maximum consecutive urgent jobs while normal jobs are queued.
    pub max_urgent_streak: usize,
    /// Grace between cooperative cancellation and task abort.
    pub cancellation_grace: Duration,
    /// Default inline output limit.
    pub default_inline_output_bytes: usize,
    /// Event subscription channel capacity.
    pub event_channel_capacity: usize,
}

impl RuntimeConfig {
    /// Creates production-safe bounded defaults at a caller-selected state path.
    #[must_use]
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            command_capacity: 64,
            queue_capacity: 256,
            global_concurrency: 4,
            per_group_concurrency: 2,
            max_urgent_streak: 2,
            cancellation_grace: Duration::from_secs(2),
            default_inline_output_bytes: 16 * 1024,
            event_channel_capacity: 256,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_and_isolation_validation_fail_closed() {
        let mut request = WorkerRequest::read_only("work");
        request.policy.capability = CapabilityMode::All;
        request.policy.parent_capability = CapabilityMode::ReadOnly;
        assert!(
            request
                .validate()
                .unwrap_err()
                .to_string()
                .contains("exceeds")
        );

        request.policy.parent_capability = CapabilityMode::All;
        assert!(
            request
                .validate()
                .unwrap_err()
                .to_string()
                .contains("worktree")
        );

        request.policy.isolation = IsolationMode::Worktree;
        request.policy.cwd = Some(PathBuf::from("."));
        assert!(
            request
                .validate()
                .unwrap_err()
                .to_string()
                .contains("mutually")
        );

        request.policy.capability = CapabilityMode::ReadOnly;
        request.policy.isolation = IsolationMode::None;
        request.policy.cwd = None;
        request.policy.nesting_depth = 3;
        request.policy.max_nesting_depth = 2;
        assert!(
            request
                .validate()
                .unwrap_err()
                .to_string()
                .contains("depth")
        );
    }

    #[test]
    fn request_schema_has_explicit_version_and_defaults() {
        let request = WorkerRequest::read_only("inspect");
        let mut value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["schema_version"], SCHEMA_VERSION);
        value["policy"]
            .as_object_mut()
            .unwrap()
            .remove("allow_outside_workspace");
        let decoded: WorkerRequest = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, request);
    }

    #[test]
    fn request_contract_carries_resolved_tools_and_no_unsafe_budget_knobs() {
        let value = serde_json::to_value(WorkerRequest::read_only("inspect")).unwrap();
        let policy = value["policy"].as_object().unwrap();
        assert!(policy.contains_key("tools"));
        assert!(!policy.contains_key("capability"));
        assert!(!policy.contains_key("parent_capability"));
        let budgets = value["budgets"].as_object().unwrap();
        assert!(budgets.contains_key("max_provider_rounds"));
        assert!(!budgets.contains_key("max_tool_rounds"));
        assert!(!budgets.contains_key("max_tokens"));
        assert_eq!(value["system_prompt"], "");
    }
}
