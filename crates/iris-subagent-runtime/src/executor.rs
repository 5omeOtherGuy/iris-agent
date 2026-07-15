use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{
    ArtifactRef, ArtifactStore, HostPayload, RuntimeError, Usage, WorkerEventKind, WorkerRequest,
    WorkerWorktree,
};

/// Local future used by executors that intentionally need not implement `Send`.
pub type LocalExecutorFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ExecutorOutput, ExecutorError>> + 'a>>;

/// Host-neutral approval request emitted by an executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ApprovalRequest {
    /// Stable request ID.
    pub id: String,
    /// Bounded operator-facing summary.
    pub summary: String,
    /// Optional safe timeout in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl ApprovalRequest {
    /// Constructs one approval request.
    #[must_use]
    pub fn new(id: impl Into<String>, summary: impl Into<String>, timeout_ms: Option<u64>) -> Self {
        Self {
            id: id.into(),
            summary: summary.into(),
            timeout_ms,
        }
    }
}

/// Host approval decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ApprovalDecision {
    /// Permit this operation once.
    Approve,
    /// Deny the operation.
    Deny,
}

/// Future returned by [`ApprovalPort`].
pub type ApprovalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ApprovalDecision, RuntimeError>> + Send + 'a>>;

/// Object-safe host port for approval UX. Enforcement remains in the host model loop.
pub trait ApprovalPort: Send + Sync + 'static {
    /// Requests an operator decision without auto-approving isolated workers.
    fn review<'a>(&'a self, request: ApprovalRequest) -> ApprovalFuture<'a>;
}

/// Error returned by a host executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
#[non_exhaustive]
pub struct ExecutorError {
    /// Stable host-neutral message.
    pub message: String,
    /// Whether the failure was caused by cancellation.
    #[serde(default)]
    pub cancelled: bool,
}

impl ExecutorError {
    /// Constructs a non-cancellation executor error.
    #[must_use]
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cancelled: false,
        }
    }

    /// Constructs a cancellation result.
    #[must_use]
    pub fn cancelled(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cancelled: true,
        }
    }
}

/// Complete executor output before runtime shaping and artifact offload.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ExecutorOutput {
    /// Bounded summary candidate.
    pub summary: String,
    /// Complete primary output. The runtime stores it when it exceeds inline bounds.
    pub output: Vec<u8>,
    /// Additional artifacts already stored by the executor.
    pub artifacts: Vec<ArtifactRef>,
    /// Final aggregate usage.
    pub usage: Usage,
    /// Attributed changed paths.
    pub changed_paths: Vec<std::path::PathBuf>,
    /// Managed worktree metadata.
    pub worktree: Option<WorkerWorktree>,
    /// Host result extension.
    pub host: HostPayload,
}

impl ExecutorOutput {
    /// Constructs a text output with default metadata.
    #[must_use]
    pub fn text(summary: impl Into<String>, output: impl Into<Vec<u8>>) -> Self {
        Self {
            summary: summary.into(),
            output: output.into(),
            ..Self::default()
        }
    }
}

/// Object-safe worker implementation constructed and run on the scheduler thread.
pub trait WorkerExecutor: 'static {
    /// Executes one accepted request using a cancellation-aware context.
    fn execute<'a>(&'a mut self, context: WorkerContext) -> LocalExecutorFuture<'a>;
}

/// Thread-safe factory invoked only on the scheduler thread.
pub trait ExecutorFactory: Send + Sync + 'static {
    /// Builds a possibly `!Send` executor for an accepted request.
    fn create(&self, request: &WorkerRequest) -> Result<Box<dyn WorkerExecutor>, RuntimeError>;
}

impl<F> ExecutorFactory for F
where
    F: Fn(&WorkerRequest) -> Result<Box<dyn WorkerExecutor>, RuntimeError> + Send + Sync + 'static,
{
    fn create(&self, request: &WorkerRequest) -> Result<Box<dyn WorkerExecutor>, RuntimeError> {
        self(request)
    }
}

/// Cloneable cancellation view that does not expose scheduler task internals.
#[derive(Debug, Clone)]
pub struct CancellationFlag {
    token: CancellationToken,
}

impl CancellationFlag {
    /// Returns whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Returns a cancellation token suitable for host async APIs.
    #[must_use]
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    /// Resolves when cancellation is requested.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}

/// Runtime context passed to an executor.
#[derive(Clone)]
pub struct WorkerContext {
    worker_id: crate::WorkerId,
    group_id: Option<crate::GroupId>,
    request: WorkerRequest,
    cancellation: CancellationFlag,
    emit: Rc<dyn Fn(WorkerEventKind)>,
    artifacts: Arc<dyn ArtifactStore>,
}

impl WorkerContext {
    pub(crate) fn new(
        worker_id: crate::WorkerId,
        group_id: Option<crate::GroupId>,
        request: WorkerRequest,
        token: CancellationToken,
        emit: Rc<dyn Fn(WorkerEventKind)>,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Self {
        Self {
            worker_id,
            group_id,
            request,
            cancellation: CancellationFlag { token },
            emit,
            artifacts,
        }
    }

    /// Returns this worker's stable runtime ID.
    #[must_use]
    pub fn worker_id(&self) -> &crate::WorkerId {
        &self.worker_id
    }

    /// Returns the group ID when this worker belongs to a runtime group.
    #[must_use]
    pub fn group_id(&self) -> Option<&crate::GroupId> {
        self.group_id.as_ref()
    }

    /// Returns the durably accepted request.
    #[must_use]
    pub fn request(&self) -> &WorkerRequest {
        &self.request
    }

    /// Returns the cooperative cancellation view.
    #[must_use]
    pub fn cancellation(&self) -> &CancellationFlag {
        &self.cancellation
    }

    /// Emits bounded progress.
    pub fn progress(&self, message: impl Into<String>) {
        (self.emit)(WorkerEventKind::Progress {
            message: message.into(),
        });
    }

    /// Marks the worker as waiting for host approval.
    pub fn waiting_for_approval(
        &self,
        request_id: impl Into<String>,
        summary: impl Into<String>,
        timeout_ms: Option<u64>,
    ) {
        (self.emit)(WorkerEventKind::ApprovalWait {
            request_id: request_id.into(),
            summary: summary.into(),
            timeout_ms,
        });
    }

    /// Reports cumulative usage for budget accounting and observers.
    pub fn usage(&self, usage: Usage) {
        (self.emit)(WorkerEventKind::Usage(usage));
    }

    /// Emits a versioned host event.
    pub fn host_event(&self, payload: HostPayload) {
        (self.emit)(WorkerEventKind::Host(payload));
    }

    /// Stores complete bytes in the configured content-addressed store.
    pub fn store_artifact(
        &self,
        bytes: &[u8],
        media_type: Option<&str>,
    ) -> Result<ArtifactRef, RuntimeError> {
        let artifact = self.artifacts.put(bytes, media_type)?;
        (self.emit)(WorkerEventKind::Artifact(artifact.clone()));
        Ok(artifact)
    }
}
