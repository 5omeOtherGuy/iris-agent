//! `bash` — run a shell command with a timeout, process-group kill, and
//! bounded output drain/truncation.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::text::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};

const DEFAULT_BASH_TIMEOUT_SECS: u64 = 120;
// Cap on how long we wait for the output reader threads to observe EOF after the
// shell has exited or been killed. A backgrounded process that escapes the
// shell's process group (via setsid/double-fork) can keep the pipes open; rather
// than block indefinitely we return whatever was captured within this window.
const BASH_DRAIN_TIMEOUT_SECS: u64 = 5;

pub(super) const DESCRIPTION: &str = "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 1MB (whichever is hit first). If truncated, full output is saved to a temp file. `timeout` defaults to 120 seconds; set `timeout: 0` to disable.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "Bash command to execute" },
            "timeout": { "type": "integer", "description": "Timeout in seconds (default 120; set 0 to disable)" }
        },
        "required": ["command"]
    })
}

pub(super) fn execute(root: &Path, args: &Value) -> Result<String> {
    let input: BashInput =
        serde_json::from_value(args.clone()).context("bash tool arguments must include command")?;
    bash(root, &input)
}

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

fn bash(root: &Path, input: &BashInput) -> Result<String> {
    if input.command.trim().is_empty() {
        bail!("bash command must not be empty");
    }
    let timeout_secs = input.timeout.unwrap_or(DEFAULT_BASH_TIMEOUT_SECS);
    let timeout = (timeout_secs > 0).then(|| Duration::from_secs(timeout_secs));

    let mut command = Command::new(resolve_shell());
    command
        .arg("-c")
        .arg(&input.command)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Run the shell in its own process group so a timeout can terminate the
    // whole group (including backgrounded children that keep the output pipes
    // open), not just the shell leader. With process_group(0) the child's PGID
    // equals its PID.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().context("failed to spawn shell")?;

    // Drain both pipes on dedicated threads so a full pipe buffer cannot
    // deadlock the wait loop. Each thread reports its captured bytes over a
    // channel so the collection below can apply a bounded deadline instead of
    // join()-ing forever if a process keeps a pipe open (see the drain below).
    let mut stdout = child.stdout.take().context("missing bash stdout")?;
    let mut stderr = child.stderr.take().context("missing bash stderr")?;
    let (tx, rx) = std::sync::mpsc::channel::<(BashStream, Vec<u8>)>();
    let stdout_tx = tx.clone();
    std::thread::spawn(move || pump_pipe(&mut stdout, BashStream::Stdout, &stdout_tx));
    std::thread::spawn(move || pump_pipe(&mut stderr, BashStream::Stderr, &tx));

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().context("failed to wait for shell")? {
            Some(status) => break Some(status),
            None => {
                if let Some(timeout) = timeout
                    && start.elapsed() >= timeout
                {
                    // Kill the whole process group so backgrounded children
                    // holding the output pipes are terminated too, which lets
                    // the reader threads observe EOF and the drain below return.
                    kill_process_group(&mut child);
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    };

    // Accumulate chunks from both streams until the pump threads finish (the
    // channel disconnects) or the drain deadline passes. A process that escaped
    // the shell's group (setsid/double-fork) can keep a pipe open after the
    // shell exits; rather than block on it forever we return the output
    // captured so far. The streaming pump means already-written output is
    // delivered even when a later holder keeps the pipe open.
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let drain_deadline = Instant::now() + Duration::from_secs(BASH_DRAIN_TIMEOUT_SECS);
    loop {
        let remaining = drain_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok((BashStream::Stdout, chunk)) => stdout_bytes.extend_from_slice(&chunk),
            Ok((BashStream::Stderr, chunk)) => stderr_bytes.extend_from_slice(&chunk),
            Err(_) => break,
        }
    }

    let mut combined = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr_text = String::from_utf8_lossy(&stderr_bytes);
    if !stderr_text.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr_text);
    }

    let (truncated_body, truncated, dropped_lines) =
        truncate_tail(&combined, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);

    let mut out = if truncated_body.trim().is_empty() {
        "(no output)".to_string()
    } else {
        truncated_body
    };

    if truncated {
        let full_path = write_overflow_file(&combined);
        let location = full_path
            .as_ref()
            .map_or_else(|| "(unavailable)".to_string(), |p| p.display().to_string());
        out = format!(
            "[output truncated, dropped {dropped_lines} earlier line(s); full output saved to {location}]\n{out}"
        );
    }

    if timed_out {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("Command timed out after {timeout_secs} seconds"));
    } else if let Some(status) = status
        && !status.success()
    {
        let code = status.code().unwrap_or(-1);
        out.push_str(&format!("\n\nCommand exited with code {code}"));
    }

    Ok(out)
}

/// Resolve the shell used to run commands.
///
/// The tool is named `bash` and advertised as running bash, so bash-only
/// syntax (arrays, `[[ ]]`, `set -o pipefail`) must work. Prefer `/bin/bash`,
/// then `bash` discovered on `PATH`, and fall back to `sh` only when no bash is
/// available so the tool still runs on minimal systems.
fn resolve_shell() -> PathBuf {
    let direct = Path::new("/bin/bash");
    if direct.is_file() {
        return direct.to_path_buf();
    }
    if let Some(found) = find_on_path("bash") {
        return found;
    }
    PathBuf::from("sh")
}

/// Locate an executable by name in the directories listed in `PATH`.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Forcefully terminate a spawned shell and the rest of its process group.
///
/// On Unix the child is spawned with `process_group(0)`, so its PGID equals its
/// PID and a single `killpg` signals every process in that group (backgrounded
/// children included). Processes that leave the group (setsid/double-fork) can
/// still escape. On other platforms we fall back to killing the leader.
#[cfg(unix)]
fn kill_process_group(child: &mut std::process::Child) {
    let Ok(pgid) = libc::pid_t::try_from(child.id()) else {
        let _ = child.kill();
        return;
    };
    // SAFETY: `killpg` is an FFI call with no Rust memory-safety invariants.
    // `pgid` is the positive id of a live child we spawned into its own process
    // group, and `SIGKILL` is a valid signal. Failures fall back to a leader
    // kill.
    let rc = unsafe { libc::killpg(pgid, libc::SIGKILL) };
    if rc == -1 {
        let _ = child.kill();
    }
}

#[cfg(not(unix))]
fn kill_process_group(child: &mut std::process::Child) {
    let _ = child.kill();
}

#[derive(Clone, Copy)]
enum BashStream {
    Stdout,
    Stderr,
}

/// Stream a child pipe to the collector in chunks so already-written output is
/// delivered even if the pipe is later held open by an escaped process. Exits
/// on EOF, read error, or once the receiver has hung up.
fn pump_pipe(
    pipe: &mut impl Read,
    stream: BashStream,
    tx: &std::sync::mpsc::Sender<(BashStream, Vec<u8>)>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send((stream, buf[..n].to_vec())).is_err() {
                    break;
                }
            }
        }
    }
}

fn write_overflow_file(content: &str) -> Option<PathBuf> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let path = std::env::temp_dir().join(format!("iris-bash-output-{nanos}.log"));
    fs::write(&path, content).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{root_of, temp_dir};

    #[test]
    fn bash_runs_command_and_captures_output() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let out = bash(
            &root,
            &BashInput {
                command: "echo hello".into(),
                timeout: None,
            },
        )
        .unwrap();
        assert!(out.contains("hello"));
    }

    #[test]
    fn bash_simple_timeout_returns() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let started = Instant::now();
        let out = bash(
            &root,
            &BashInput {
                command: "sleep 30".into(),
                timeout: Some(1),
            },
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "bash hung past timeout"
        );
        assert!(out.contains("timed out"));
    }

    #[test]
    fn bash_timeout_kills_backgrounded_pipe_holder() {
        // A backgrounded child inherits the shell's stdout pipe. If the timeout
        // path only killed the shell leader, the reader thread would block on
        // the surviving `sleep` until it exits (~30s). A process-group kill must
        // terminate it so the call returns promptly.
        let dir = temp_dir();
        let root = root_of(&dir);
        let started = Instant::now();
        let out = bash(
            &root,
            &BashInput {
                command: "sleep 30 & echo started; wait".into(),
                timeout: Some(1),
            },
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "bash hung past timeout despite backgrounded pipe holder"
        );
        assert!(out.contains("timed out"));
    }

    #[test]
    fn bash_returns_when_backgrounded_child_keeps_pipe_open() {
        // The shell exits immediately but leaves a backgrounded child holding
        // the stdout pipe. The bounded drain must let the call return rather
        // than blocking on the reader thread until the child exits (~30s).
        let dir = temp_dir();
        let root = root_of(&dir);
        let started = Instant::now();
        let out = bash(
            &root,
            &BashInput {
                command: "sleep 30 & echo done".into(),
                timeout: None,
            },
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(BASH_DRAIN_TIMEOUT_SECS + 5),
            "bash blocked on a backgrounded pipe holder"
        );
        assert!(out.contains("done"));
    }

    #[test]
    fn bash_runs_bashisms_like_pipefail() {
        // The tool is named `bash` and advertised as running bash, so
        // bash-only syntax (here `set -o pipefail`) must work rather than fail
        // under POSIX `sh`/dash with "Illegal option -o pipefail".
        let dir = temp_dir();
        let root = root_of(&dir);
        let out = bash(
            &root,
            &BashInput {
                command: "set -o pipefail; echo ok | cat".into(),
                timeout: None,
            },
        )
        .unwrap();
        assert!(out.contains("ok"), "expected bashism to run, got: {out}");
        assert!(
            !out.contains("Command exited with code"),
            "bashism failed under the resolved shell: {out}"
        );
    }

    #[test]
    fn bash_reports_nonzero_exit() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let out = bash(
            &root,
            &BashInput {
                command: "exit 3".into(),
                timeout: None,
            },
        )
        .unwrap();
        assert!(out.contains("Command exited with code 3"));
    }
}
