use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::RuntimeError;

/// Cooperative cancellation for filesystem and git subprocess operations.
#[derive(Debug, Clone, Default)]
pub struct WorktreeCancellation {
    cancelled: Arc<AtomicBool>,
}

impl WorktreeCancellation {
    /// Requests cancellation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Returns whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Neutral subprocess request used by injectable worktree strategies.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ProcessSpec {
    /// Program name or path.
    pub program: String,
    /// Argument vector.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: Option<PathBuf>,
    /// Hard timeout.
    pub timeout: Duration,
    /// Environment additions.
    pub env: Vec<(String, String)>,
}

/// Captured subprocess output.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProcessOutput {
    /// Exit code, or `-1` when terminated by signal.
    pub status: i32,
    /// Complete standard output.
    pub stdout: Vec<u8>,
    /// Complete standard error.
    pub stderr: Vec<u8>,
}

impl ProcessOutput {
    /// Constructs a successful captured output.
    #[must_use]
    pub fn success(stdout: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 0,
            stdout: stdout.into(),
            stderr: Vec::new(),
        }
    }

    /// Requires a successful exit and returns UTF-8-lossy stdout without trailing whitespace.
    pub fn success_text(&self, program: &str) -> Result<String, RuntimeError> {
        if self.status != 0 {
            return Err(RuntimeError::Process {
                program: program.to_string(),
                message: String::from_utf8_lossy(&self.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&self.stdout).trim().to_string())
    }
}

/// Injectable blocking process runner.
pub trait ProcessRunner: Send + Sync + 'static {
    /// Runs one cancellable, bounded subprocess.
    fn run(
        &self,
        spec: &ProcessSpec,
        cancellation: &WorktreeCancellation,
    ) -> Result<ProcessOutput, RuntimeError>;
}

/// Standard-library process runner used by production services.
#[derive(Debug, Default)]
pub struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    fn run(
        &self,
        spec: &ProcessSpec,
        cancellation: &WorktreeCancellation,
    ) -> Result<ProcessOutput, RuntimeError> {
        if cancellation.is_cancelled() {
            return Err(RuntimeError::Process {
                program: spec.program.clone(),
                message: "cancelled before start".to_string(),
            });
        }
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        command.envs(spec.env.iter().map(|(key, value)| (key, value)));
        let mut child = command.spawn().map_err(|error| RuntimeError::Process {
            program: spec.program.clone(),
            message: error.to_string(),
        })?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let out_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut reader = stdout;
            let _ = reader.read_to_end(&mut bytes);
            bytes
        });
        let err_reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut reader = stderr;
            let _ = reader.read_to_end(&mut bytes);
            bytes
        });
        let deadline = Instant::now() + spec.timeout;
        let status = loop {
            if cancellation.is_cancelled() || Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let reason = if cancellation.is_cancelled() {
                    "cancelled"
                } else {
                    "timed out"
                };
                let _ = out_reader.join();
                let _ = err_reader.join();
                return Err(RuntimeError::Process {
                    program: spec.program.clone(),
                    message: reason.to_string(),
                });
            }
            match child.try_wait().map_err(|error| RuntimeError::Process {
                program: spec.program.clone(),
                message: error.to_string(),
            })? {
                Some(status) => break status.code().unwrap_or(-1),
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        let stdout = out_reader.join().unwrap_or_default();
        let stderr = err_reader.join().unwrap_or_default();
        Ok(ProcessOutput {
            status,
            stdout,
            stderr,
        })
    }
}

pub(crate) fn git_spec(
    cwd: PathBuf,
    timeout: Duration,
    args: impl IntoIterator<Item = impl Into<String>>,
) -> ProcessSpec {
    let mut all = vec!["--no-optional-locks".to_string()];
    all.extend(args.into_iter().map(Into::into));
    ProcessSpec {
        program: "git".to_string(),
        args: all,
        cwd: Some(cwd),
        timeout,
        env: vec![
            ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
            (
                "GIT_SSH_COMMAND".to_string(),
                "ssh -o BatchMode=yes".to_string(),
            ),
            ("GIT_LFS_SKIP_SMUDGE".to_string(), "1".to_string()),
        ],
    }
}
