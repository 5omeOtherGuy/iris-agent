use std::path::PathBuf;

use thiserror::Error;

/// Typed failures returned by the runtime and worktree services.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RuntimeError {
    /// A request violates a typed runtime policy.
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// A persisted or caller-supplied opaque identifier is invalid.
    #[error("invalid {kind} value: {value}")]
    InvalidId {
        /// Identifier type.
        kind: &'static str,
        /// Rejected value.
        value: String,
    },
    /// The bounded command or worker queue cannot accept more work.
    #[error("runtime backpressure: {queue} capacity {capacity} reached")]
    Backpressure {
        /// Queue that rejected the operation.
        queue: &'static str,
        /// Configured bound.
        capacity: usize,
    },
    /// A worker or group was not found.
    #[error("{kind} not found: {id}")]
    NotFound {
        /// Record kind.
        kind: &'static str,
        /// Missing ID.
        id: String,
    },
    /// A host executor could not be created.
    #[error("executor factory failed: {0}")]
    ExecutorFactory(String),
    /// Runtime persistence failed before or during a lifecycle transition.
    #[error("persistence failed at {}: {source}", path.display())]
    Persistence {
        /// Affected path.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A persisted record is malformed or unsupported.
    #[error("corrupt persisted record at {}: {message}", path.display())]
    CorruptRecord {
        /// Affected path.
        path: PathBuf,
        /// Validation failure.
        message: String,
    },
    /// The scheduler thread has stopped accepting commands.
    #[error("runtime is shut down")]
    Shutdown,
    /// A blocking wait exceeded its caller-provided bound.
    #[error("wait timed out")]
    WaitTimeout,
    /// A filesystem path escaped a validated root or had an unsupported shape.
    #[error("unsafe path {}: {reason}", path.display())]
    UnsafePath {
        /// Rejected path.
        path: PathBuf,
        /// Validation reason.
        reason: String,
    },
    /// A git or filesystem subprocess failed.
    #[error("process `{program}` failed: {message}")]
    Process {
        /// Program name.
        program: String,
        /// Failure detail.
        message: String,
    },
    /// A worktree operation is unsupported for the detected repository.
    #[error("unsupported workspace: {0}")]
    UnsupportedWorkspace(String),
    /// A worktree or apply precondition changed after review.
    #[error("conflict: {0}")]
    Conflict(String),
    /// An artifact store failed.
    #[error("artifact store failed: {0}")]
    Artifact(String),
    /// Runtime thread startup or join failed.
    #[error("runtime thread failed: {0}")]
    Thread(String),
}

impl RuntimeError {
    pub(crate) fn persistence(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Persistence {
            path: path.into(),
            source,
        }
    }
}
