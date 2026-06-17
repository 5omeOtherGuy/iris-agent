//! Persistent shell sessions for the `bash` tool.
//!
//! A session is a long-lived `bash` co-process. State set by one command
//! (`cd`, `export`, shell variables) survives into later commands in the same
//! session, unlike the one-shot path which spawns a fresh shell per call.
//!
//! ## Sentinel protocol
//!
//! The shell merges stderr into stdout (`exec 2>&1`) so a single ordered stream
//! can be delimited. Each command is written as:
//!
//! ```text
//! {
//! <user command>
//! } </dev/null
//! __iris_rc=$?; printf '\n__IRIS_DONE_<nonce> %d\n' "$__iris_rc"
//! ```
//!
//! The `{ ... }` group runs in the current shell (so `cd`/`export` persist),
//! `</dev/null` stops commands like `cat`/`read` from consuming the control
//! pipe, and the high-entropy nonce makes the completion marker
//! collision-proof. A reader thread forwards the stream to a channel; [`run`]
//! scans for the marker, splits off the command output, and parses the exit
//! code.
//!
//! ## Lifecycle
//!
//! Sessions are created lazily on first [`run`], explicitly cleared with
//! [`reset`] (drop + recreate fresh) or [`close`], and all are torn down when
//! the registry is dropped. Each shell runs in its own process group; teardown
//! sends `SIGKILL` to the group and reaps the child so nothing leaks.
//!
//! [`run`]: Sessions::run
//! [`reset`]: Sessions::reset
//! [`close`]: Sessions::close

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;

use super::CANCEL_POLL_INTERVAL;
use super::sandbox::{self, SandboxStatus};

/// Upper bound on how long a single command may run when the caller passes no
/// timeout. Keeps a wedged session (e.g. an unterminated here-doc swallowing the
/// marker) from blocking forever; the tool layer normally passes an explicit
/// timeout well under this.
pub(super) const SESSION_HARD_CAP: Duration = Duration::from_secs(600);
const SESSION_MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Result of running one command in a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunOutcome {
    /// Combined stdout+stderr the command produced (untruncated).
    pub(crate) output: String,
    /// Bytes dropped from the head while bounding the session output buffer.
    pub(crate) output_dropped: usize,
    /// Exit status, or `None` if the shell died before reporting one
    /// (e.g. the command was `exit`, or the session was killed on timeout).
    pub(crate) exit_code: Option<i32>,
    /// Whether the command was cut off by the per-command timeout (the session
    /// is killed and will be recreated on the next run).
    pub(crate) timed_out: bool,
    /// Whether the command was interrupted by a turn-level cancellation
    /// (Ctrl-C). Like a timeout, the session shell is killed and recreated next
    /// run.
    pub(crate) cancelled: bool,
    /// Sandbox status notice when the shell could not be fully confined; `None`
    /// when fully enforced. Surfaced by the tool layer (never silently dropped).
    pub(crate) notice: Option<String>,
}

/// Registry of named persistent shell sessions.
pub(crate) struct Sessions {
    sessions: HashMap<String, Session>,
}

impl Sessions {
    pub(crate) fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    /// Run `command` in session `id`, creating the session (rooted at `root`)
    /// if it does not exist or its shell has died.
    pub(crate) fn run(
        &mut self,
        root: &Path,
        id: &str,
        command: &str,
        timeout: Option<Duration>,
        cancel: &CancellationToken,
    ) -> Result<RunOutcome> {
        let fresh = match self.sessions.get(id) {
            Some(session) => !session.alive,
            None => true,
        };
        if fresh {
            // Reap any dead shell still in the map before replacing it.
            if let Some(mut old) = self.sessions.remove(id) {
                old.kill_and_reap();
            }
            let session = Session::create(root, cancel)?;
            self.sessions.insert(id.to_string(), session);
        }

        let session = self
            .sessions
            .get_mut(id)
            .expect("session present after create");
        let mut outcome = session.run_command(command, timeout, cancel);
        // Never a silent "sandbox off": carry the notice for the tool layer to
        // surface (kept out of `output` so truncation cannot drop it).
        outcome.notice = session.status.notice();
        Ok(outcome)
    }

    /// Drop session `id` (killing its shell) so the next [`run`] starts fresh.
    pub(crate) fn reset(&mut self, id: &str) -> Result<()> {
        if let Some(mut session) = self.sessions.remove(id) {
            session.kill_and_reap();
        }
        Ok(())
    }

    /// Close session `id`, killing and reaping its shell. A no-op if absent.
    pub(crate) fn close(&mut self, id: &str) -> Result<()> {
        if let Some(mut session) = self.sessions.remove(id) {
            session.kill_and_reap();
        }
        Ok(())
    }

    #[cfg(test)]
    fn child_pid(&self, id: &str) -> Option<u32> {
        self.sessions.get(id).map(|s| s.pid())
    }
}

/// Outcome of scanning the shell's output stream for the completion marker.
enum Marker {
    /// Marker found: `output` is the command's bytes, `code` its exit status.
    Found {
        output: Vec<u8>,
        dropped: usize,
        code: Option<i32>,
    },
    /// Deadline passed before the marker arrived; `output` is what was read.
    TimedOut { output: Vec<u8>, dropped: usize },
    /// The shell's stdout closed (it exited) before a marker; `output` is
    /// whatever it printed first.
    Disconnected { output: Vec<u8>, dropped: usize },
    /// A turn-level cancellation (Ctrl-C) interrupted the wait; `output` is what
    /// was read so far.
    Cancelled { output: Vec<u8>, dropped: usize },
}

/// A single persistent shell.
struct Session {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Vec<u8>>,
    nonce: String,
    /// Bytes read past the previous command's marker, carried into the next read.
    leftover: Vec<u8>,
    alive: bool,
    status: SandboxStatus,
    /// Registry slot so a force-quit reaps this shell; dropped when killed.
    group: Option<crate::process_group::GroupGuard>,
}

impl Session {
    /// Spawn a confined `bash`, merge stderr into stdout, and sync to a clean
    /// state by consuming an initial marker.
    fn create(root: &Path, cancel: &CancellationToken) -> Result<Self> {
        let mut command = Command::new(super::resolve_shell());
        command
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::process_group::in_own_group(&mut command);
        let status = sandbox::confine(&mut command, &sandbox::SandboxPolicy::for_workspace(root));
        if let Some(notice) = status.notice() {
            tracing::warn!(%notice, "bash session sandbox not fully enforced");
        }

        let mut child = command.spawn().context("failed to spawn session shell")?;
        let group = Some(crate::process_group::register(
            i32::try_from(child.id()).unwrap_or(0),
        ));
        let stdin = child.stdin.take().context("missing session stdin")?;
        let stdout = child.stdout.take().context("missing session stdout")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || reader_loop(stdout, &tx));

        let mut session = Self {
            child,
            stdin,
            rx,
            nonce: format!("{:016x}", rand::random::<u64>()),
            leftover: Vec::new(),
            alive: true,
            status,
            group,
        };

        // Merge stderr so a single ordered stream carries everything, then sync
        // on a marker so the first user command starts from a known position.
        session
            .write_all(b"exec 2>&1\n")
            .context("failed to initialize session shell")?;
        session
            .write_marker()
            .context("failed to initialize session shell")?;
        match session.read_until_marker(Some(Instant::now() + Duration::from_secs(10)), cancel) {
            Marker::Found { .. } => Ok(session),
            _ => {
                session.kill_and_reap();
                bail!("session shell did not initialize");
            }
        }
    }

    #[cfg(test)]
    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Run one command and return its output + exit code. Caller guarantees the
    /// session is alive; a dead shell or timeout marks it dead for recreation.
    fn run_command(
        &mut self,
        command: &str,
        timeout: Option<Duration>,
        cancel: &CancellationToken,
    ) -> RunOutcome {
        if self.write_command(command).is_err() {
            // The shell died between commands; reap it and report dead so the
            // next run recreates it.
            self.kill_and_reap();
            return RunOutcome {
                output: String::new(),
                output_dropped: 0,
                exit_code: None,
                timed_out: false,
                cancelled: false,
                notice: None,
            };
        }
        let deadline = timeout
            .map(|d| Instant::now() + d)
            .or_else(|| Some(Instant::now() + SESSION_HARD_CAP));
        match self.read_until_marker(deadline, cancel) {
            Marker::Found {
                output,
                dropped,
                code,
            } => RunOutcome {
                output: String::from_utf8_lossy(&output).into_owned(),
                output_dropped: dropped,
                exit_code: code,
                timed_out: false,
                cancelled: false,
                notice: None,
            },
            Marker::TimedOut { output, dropped } => {
                self.kill_and_reap();
                RunOutcome {
                    output: String::from_utf8_lossy(&output).into_owned(),
                    output_dropped: dropped,
                    exit_code: None,
                    timed_out: true,
                    cancelled: false,
                    notice: None,
                }
            }
            Marker::Disconnected { output, dropped } => {
                self.kill_and_reap();
                RunOutcome {
                    output: String::from_utf8_lossy(&output).into_owned(),
                    output_dropped: dropped,
                    exit_code: None,
                    timed_out: false,
                    cancelled: false,
                    notice: None,
                }
            }
            Marker::Cancelled { output, dropped } => {
                self.kill_and_reap();
                RunOutcome {
                    output: String::from_utf8_lossy(&output).into_owned(),
                    output_dropped: dropped,
                    exit_code: None,
                    timed_out: false,
                    cancelled: true,
                    notice: None,
                }
            }
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stdin.write_all(bytes)?;
        self.stdin.flush()
    }

    /// Write the user command wrapped so `cd`/`export` persist in the shell and
    /// the command cannot read the control pipe, followed by the marker.
    fn write_command(&mut self, command: &str) -> io::Result<()> {
        self.write_all(format!("{{\n{command}\n}} </dev/null\n").as_bytes())?;
        self.write_marker()
    }

    /// Emit `\n__IRIS_DONE_<nonce> <exit code>\n` capturing the previous
    /// command's status.
    fn write_marker(&mut self) -> io::Result<()> {
        let line = format!(
            "__iris_rc=$?; printf '\\n__IRIS_DONE_{} %d\\n' \"$__iris_rc\"\n",
            self.nonce
        );
        self.write_all(line.as_bytes())
    }

    /// Read the output stream until the completion marker, the deadline, or the
    /// shell's stdout closing.
    fn read_until_marker(
        &mut self,
        deadline: Option<Instant>,
        cancel: &CancellationToken,
    ) -> Marker {
        let prefix = format!("\n__IRIS_DONE_{} ", self.nonce).into_bytes();
        let mut buf = std::mem::take(&mut self.leftover);
        // Rescan from a point that preserves any marker split across reads.
        let mut scan_from = 0usize;
        let mut dropped = 0usize;

        loop {
            if let Some(rel) = find(&buf[scan_from..], &prefix) {
                let start = scan_from + rel;
                let after = start + prefix.len();
                if let Some(nl) = buf[after..].iter().position(|&b| b == b'\n') {
                    let code = std::str::from_utf8(&buf[after..after + nl])
                        .ok()
                        .and_then(|s| s.trim().parse::<i32>().ok());
                    let output = buf[..start].to_vec();
                    self.leftover = buf[after + nl + 1..].to_vec();
                    return Marker::Found {
                        output,
                        dropped,
                        code,
                    };
                }
                // Prefix seen but the exit code/newline has not fully arrived;
                // keep this region in view and read more.
                scan_from = start;
            } else {
                scan_from = buf.len().saturating_sub(prefix.len().saturating_sub(1));
            }

            // Wait for the next chunk, but never longer than one cancel-poll
            // slice so a turn-level Ctrl-C is observed promptly; a slice timeout
            // (not the deadline) just re-checks cancellation and loops. The full
            // deadline is still enforced via the zero-remaining check above each
            // wait.
            let wait = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Marker::TimedOut {
                            output: buf,
                            dropped,
                        };
                    }
                    remaining.min(CANCEL_POLL_INTERVAL)
                }
                None => CANCEL_POLL_INTERVAL,
            };
            let chunk = match self.rx.recv_timeout(wait) {
                Ok(chunk) => chunk,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if cancel.is_cancelled() {
                        return Marker::Cancelled {
                            output: buf,
                            dropped,
                        };
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Marker::Disconnected {
                        output: buf,
                        dropped,
                    };
                }
            };
            buf.extend_from_slice(&chunk);
            if buf.len() > SESSION_MAX_OUTPUT_BYTES {
                let excess = buf.len() - SESSION_MAX_OUTPUT_BYTES;
                buf.drain(..excess);
                dropped += excess;
                scan_from = scan_from.saturating_sub(excess);
            }
        }
    }

    /// Kill the whole process group and reap the leader so nothing is left
    /// running or zombied. Idempotent: the first call marks the session dead and
    /// later calls (including from `Drop`) are no-ops.
    fn kill_and_reap(&mut self) {
        if !self.alive {
            return;
        }
        self.alive = false;
        crate::process_group::kill_and_reap(&mut self.child);
        // Drop the registry guard now that the group is gone.
        self.group = None;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

/// Forward a child's stdout to the channel in chunks until EOF or read error.
fn reader_loop(mut stdout: ChildStdout, tx: &mpsc::Sender<Vec<u8>>) {
    let mut buf = [0u8; 8192];
    loop {
        match stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

/// Index of the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Workspace(PathBuf);
    impl Drop for Workspace {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }
    fn workspace() -> Workspace {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("iris-session-test-{nanos}-{seq}"));
        std::fs::create_dir(&p).unwrap();
        Workspace(p)
    }

    fn pid_alive(pid: u32) -> bool {
        // kill(pid, 0): 0 if the process exists, ESRCH if not.
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[test]
    fn cd_persists_between_calls() {
        let ws = workspace();
        std::fs::create_dir(ws.0.join("sub")).unwrap();
        let mut s = Sessions::new();
        s.run(&ws.0, "a", "cd sub", None, &CancellationToken::new())
            .unwrap();
        let out = s
            .run(&ws.0, "a", "pwd", None, &CancellationToken::new())
            .unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert!(
            out.output.trim_end().ends_with("/sub"),
            "pwd: {:?}",
            out.output
        );
        s.close("a").unwrap();
    }

    #[test]
    fn env_and_shell_vars_persist_between_calls() {
        let ws = workspace();
        let mut s = Sessions::new();
        s.run(
            &ws.0,
            "a",
            "export FOO=bar",
            None,
            &CancellationToken::new(),
        )
        .unwrap();
        s.run(&ws.0, "a", "PLAIN=42", None, &CancellationToken::new())
            .unwrap();
        let foo = s
            .run(&ws.0, "a", "echo $FOO", None, &CancellationToken::new())
            .unwrap();
        let plain = s
            .run(&ws.0, "a", "echo $PLAIN", None, &CancellationToken::new())
            .unwrap();
        assert_eq!(foo.output.trim_end(), "bar");
        assert_eq!(plain.output.trim_end(), "42");
        s.close("a").unwrap();
    }

    #[test]
    fn reset_clears_cwd_and_vars() {
        let ws = workspace();
        std::fs::create_dir(ws.0.join("sub")).unwrap();
        let mut s = Sessions::new();
        s.run(
            &ws.0,
            "a",
            "cd sub; export FOO=bar",
            None,
            &CancellationToken::new(),
        )
        .unwrap();
        s.reset("a").unwrap();
        let pwd = s
            .run(&ws.0, "a", "pwd", None, &CancellationToken::new())
            .unwrap();
        let foo = s
            .run(
                &ws.0,
                "a",
                "echo \"[${FOO:-unset}]\"",
                None,
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(
            !pwd.output.trim_end().ends_with("/sub"),
            "cwd not reset: {:?}",
            pwd.output
        );
        assert_eq!(foo.output.trim_end(), "[unset]");
        s.close("a").unwrap();
    }

    #[test]
    fn reports_exit_codes_and_output() {
        let ws = workspace();
        let mut s = Sessions::new();
        let hello = s
            .run(&ws.0, "a", "echo hello", None, &CancellationToken::new())
            .unwrap();
        assert_eq!(hello.output.trim_end(), "hello");
        assert_eq!(hello.exit_code, Some(0));
        assert_eq!(
            s.run(&ws.0, "a", "false", None, &CancellationToken::new())
                .unwrap()
                .exit_code,
            Some(1)
        );
        // A child process exiting non-zero is reported without killing the
        // session shell (unlike a bare `exit`, which would terminate it).
        assert_eq!(
            s.run(
                &ws.0,
                "a",
                "bash -c 'exit 7'",
                None,
                &CancellationToken::new()
            )
            .unwrap()
            .exit_code,
            Some(7)
        );
        assert_eq!(
            s.run(
                &ws.0,
                "a",
                "echo still-alive",
                None,
                &CancellationToken::new()
            )
            .unwrap()
            .output
            .trim_end(),
            "still-alive"
        );
        s.close("a").unwrap();
    }

    #[test]
    fn session_output_is_bounded() {
        let ws = workspace();
        let mut s = Sessions::new();
        let out = s
            .run(
                &ws.0,
                "a",
                "python3 - <<'PY'\nprint('x' * (2 * 1024 * 1024))\nPY",
                Some(Duration::from_secs(5)),
                &CancellationToken::new(),
            )
            .unwrap();

        assert!(out.output.len() < 2 * 1024 * 1024, "output was not bounded");
        assert!(out.output_dropped > 0);
        s.close("a").unwrap();
    }

    #[test]
    fn close_kills_and_reaps_shell() {
        let ws = workspace();
        let mut s = Sessions::new();
        s.run(&ws.0, "a", "echo hi", None, &CancellationToken::new())
            .unwrap();
        let pid = s.child_pid("a").unwrap();
        assert!(pid_alive(pid));
        s.close("a").unwrap();
        // Give the kernel a moment to retire the pid.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !pid_alive(pid),
            "session shell {pid} still alive after close"
        );
        assert!(s.child_pid("a").is_none());
    }

    #[test]
    fn dead_session_is_recreated_without_panic() {
        let ws = workspace();
        let mut s = Sessions::new();
        s.run(
            &ws.0,
            "a",
            "export FOO=bar",
            None,
            &CancellationToken::new(),
        )
        .unwrap();
        // `exit` kills the shell mid-command: no marker, defined as dead.
        let _ = s.run(&ws.0, "a", "exit", None, &CancellationToken::new());
        // Next run must transparently recreate a fresh shell.
        let out = s
            .run(&ws.0, "a", "echo back", None, &CancellationToken::new())
            .unwrap();
        assert_eq!(out.output.trim_end(), "back");
        // Fresh shell, so the old var is gone.
        let foo = s
            .run(
                &ws.0,
                "a",
                "echo \"[${FOO:-unset}]\"",
                None,
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(foo.output.trim_end(), "[unset]");
        s.close("a").unwrap();
    }

    #[test]
    fn timeout_kills_session_and_next_run_recovers() {
        let ws = workspace();
        let mut s = Sessions::new();
        let pid = {
            s.run(&ws.0, "a", "echo warm", None, &CancellationToken::new())
                .unwrap();
            s.child_pid("a").unwrap()
        };
        let started = std::time::Instant::now();
        let out = s
            .run(
                &ws.0,
                "a",
                "sleep 30",
                Some(Duration::from_secs(1)),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(out.timed_out, "expected timeout");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "did not return promptly"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert!(!pid_alive(pid), "timed-out session shell still alive");
        // Recovery: a fresh shell handles the next command.
        let out = s
            .run(
                &ws.0,
                "a",
                "echo recovered",
                None,
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(out.output.trim_end(), "recovered");
        s.close("a").unwrap();
    }

    #[test]
    fn cancellation_stops_a_session_run_and_recovers() {
        let ws = workspace();
        let mut s = Sessions::new();
        let pid = {
            s.run(&ws.0, "a", "echo warm", None, &CancellationToken::new())
                .unwrap();
            s.child_pid("a").unwrap()
        };
        // Trip the turn token while a long command is mid-run.
        let cancel = CancellationToken::new();
        let trip = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            trip.cancel();
        });
        let started = Instant::now();
        let out = s.run(&ws.0, "a", "sleep 30", None, &cancel).unwrap();
        assert!(out.cancelled, "expected cancellation");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "cancelled run did not return promptly"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert!(!pid_alive(pid), "cancelled session shell still alive");
        // The killed session is transparently recreated on the next run.
        let out = s
            .run(
                &ws.0,
                "a",
                "echo recovered",
                None,
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(out.output.trim_end(), "recovered");
        s.close("a").unwrap();
    }
}
