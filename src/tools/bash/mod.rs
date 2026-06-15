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

mod jobs;
mod sandbox;
mod session;

const DEFAULT_BASH_TIMEOUT_SECS: u64 = 120;
// Cap on how long we wait for the output reader threads to observe EOF after the
// shell has exited or been killed. A backgrounded process that escapes the
// shell's process group (via setsid/double-fork) can keep the pipes open; rather
// than block indefinitely we return whatever was captured within this window.
const BASH_DRAIN_TIMEOUT_SECS: u64 = 5;

pub(super) const DESCRIPTION: &str = "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 1MB (whichever is hit first). If truncated, full output is saved to a temp file. `timeout` defaults to 120 seconds; set `timeout: 0` to disable. Pass `session` (any id string) to run in a persistent shell where `cd`, environment, and shell variables carry across calls; with a session, `action` may be `run` (default), `reset` (start the shell fresh), or `close` (terminate it).";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "Bash command to execute" },
            "timeout": { "type": "integer", "description": "Timeout in seconds (default 120; set 0 to disable)" },
            "session": { "type": "string", "description": "Persistent shell session id; state (cd/env/vars) persists across calls with the same id" },
            "action": { "type": "string", "enum": ["run", "reset", "close"], "description": "Session action (default run); reset starts a fresh shell, close terminates it" }
        },
        "required": ["command"]
    })
}

/// Per-agent state for the bash tool: the persistent-session registry and the
/// background-job registry.
pub(crate) struct BashState {
    sessions: session::Sessions,
    jobs: jobs::Jobs,
}

impl BashState {
    pub(crate) fn new() -> Self {
        Self {
            sessions: session::Sessions::new(),
            jobs: jobs::Jobs::new(),
        }
    }
}

pub(super) fn execute(
    root: &Path,
    args: &Value,
    state: &mut BashState,
) -> Result<super::ToolOutput> {
    let parsed: BashArgs =
        serde_json::from_value(args.clone()).context("bash tool arguments must include command")?;
    let text = match parsed.action.as_deref().unwrap_or("run") {
        "run" => match &parsed.session {
            Some(id) => run_session(root, &id.clone(), parsed, state)?,
            None => {
                let command = parsed
                    .command
                    .context("bash tool arguments must include command")?;
                bash(
                    root,
                    &BashInput {
                        command,
                        timeout: parsed.timeout,
                    },
                )?
            }
        },
        action @ ("reset" | "close") => {
            let id = parsed
                .session
                .as_deref()
                .context("bash session action requires 'session'")?;
            if action == "reset" {
                state.sessions.reset(id)?;
                format!("session '{id}' reset")
            } else {
                state.sessions.close(id)?;
                format!("session '{id}' closed")
            }
        }
        "start" => {
            let command = parsed
                .command
                .filter(|c| !c.trim().is_empty())
                .context("bash job start requires a command")?;
            let id = state.jobs.start(root, &command, None)?;
            format!("started background job '{id}'; poll it with action=poll, job='{id}'")
        }
        "poll" => {
            let id = parsed.job.as_deref().context("bash poll requires 'job'")?;
            render_job(id, state.jobs.poll(id)?)
        }
        "finalize" => {
            let id = parsed
                .job
                .as_deref()
                .context("bash finalize requires 'job'")?;
            let wait = match parsed.timeout {
                Some(0) => None,
                Some(secs) => Some(Duration::from_secs(secs)),
                None => Some(Duration::from_secs(DEFAULT_BASH_TIMEOUT_SECS)),
            };
            render_job(id, state.jobs.finalize(id, wait)?)
        }
        "cancel" => {
            let id = parsed
                .job
                .as_deref()
                .context("bash cancel requires 'job'")?;
            state.jobs.cancel(id)?;
            format!("cancelled background job '{id}'")
        }
        "list" => render_job_list(state.jobs.list()),
        other => bail!("unknown bash action: {other}"),
    };
    Ok(super::ToolOutput::text(text))
}

/// Route a session-scoped `run` to the session registry.
fn run_session(root: &Path, id: &str, parsed: BashArgs, state: &mut BashState) -> Result<String> {
    let command = parsed
        .command
        .filter(|c| !c.trim().is_empty())
        .context("bash session run requires a command")?;
    let timeout_secs = parsed.timeout.unwrap_or(DEFAULT_BASH_TIMEOUT_SECS);
    let timeout = (timeout_secs > 0).then(|| Duration::from_secs(timeout_secs));
    let outcome = state.sessions.run(root, id, &command, timeout)?;
    Ok(render_session(outcome, timeout_secs))
}

/// Format a background-job update: bounded output, drop/sandbox notices, status.
fn render_job(id: &str, update: jobs::JobUpdate) -> String {
    let mut out = if update.output.trim().is_empty() {
        if update.finished {
            "(no output)".to_string()
        } else {
            "(no new output)".to_string()
        }
    } else {
        render_output(&update.output)
    };
    if update.dropped > 0 {
        out = format!(
            "[{} byte(s) dropped from the bounded output buffer]\n{out}",
            update.dropped
        );
    }
    if let Some(notice) = update.notice {
        out = format!("[{notice}]\n{out}");
    }
    let status = if update.finished {
        match update.exit_code {
            Some(code) => format!("job '{id}' finished (exit code {code})"),
            None => format!("job '{id}' finished (terminated)"),
        }
    } else {
        format!("job '{id}' running")
    };
    format!("{out}\n\n{status}")
}

/// Format the job list as one line per job.
fn render_job_list(jobs: Vec<jobs::JobInfo>) -> String {
    if jobs.is_empty() {
        return "(no background jobs)".to_string();
    }
    jobs.into_iter()
        .map(|j| {
            let state = if j.running {
                "running".to_string()
            } else {
                match j.exit_code {
                    Some(code) => format!("finished (exit {code})"),
                    None => "finished (terminated)".to_string(),
                }
            };
            let label = j.label.map(|l| format!(" {l}")).unwrap_or_default();
            let dropped = if j.dropped > 0 {
                format!(", {} dropped", j.dropped)
            } else {
                String::new()
            };
            format!("{}{label}: {state} ({} bytes{dropped})", j.id, j.produced)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Format a session command result the same way the one-shot path does: bounded
/// body, sandbox notice, then a timeout/exit-status footer.
fn render_session(outcome: session::RunOutcome, timeout_secs: u64) -> String {
    let mut out = render_output(&outcome.output);
    if let Some(notice) = outcome.notice {
        out = format!("[{notice}]\n{out}");
    }
    if outcome.timed_out {
        // timeout_secs == 0 means "no per-command limit"; the session still
        // enforces a safety hard cap, so report that bound, not 0.
        let limit = if timeout_secs == 0 {
            session::SESSION_HARD_CAP.as_secs()
        } else {
            timeout_secs
        };
        out.push_str(&format!(
            "\n\nCommand timed out after {limit} seconds; session terminated"
        ));
    } else if outcome.exit_code.is_none() {
        out.push_str("\n\nSession shell exited");
    } else if let Some(code) = outcome.exit_code
        && code != 0
    {
        out.push_str(&format!("\n\nCommand exited with code {code}"));
    }
    out
}

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct BashArgs {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    job: Option<String>,
    #[serde(default)]
    action: Option<String>,
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
    // Run the shell in its own process group so a timeout (or a force-quit) can
    // terminate the whole group, including backgrounded children that keep the
    // output pipes open, not just the shell leader.
    crate::process_group::in_own_group(&mut command);
    // Confine the command to a kernel-enforced filesystem/network policy: write
    // access limited to the workspace, networking denied by default. The status
    // is surfaced below when the kernel could not fully enforce it (never a
    // silent "sandbox off").
    let sandbox = sandbox::confine(&mut command, &sandbox::SandboxPolicy::for_workspace(root));
    if let Some(notice) = sandbox.notice() {
        tracing::warn!(%notice, "bash sandbox not fully enforced");
    }
    let mut child = command.spawn().context("failed to spawn shell")?;
    // Track this group so a force-quit SIGINT reaps it; the guard unregisters
    // when the command finishes below.
    let _group = crate::process_group::register(i32::try_from(child.id()).unwrap_or(0));

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
                    crate::process_group::kill_and_reap(&mut child);
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

    let mut out = render_output(&combined);

    if let Some(notice) = sandbox.notice() {
        out = format!("[{notice}]\n{out}");
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

/// Apply the shared output policy: keep the bounded tail, mark `(no output)`
/// when empty, and spill the full output to a temp file when truncated.
fn render_output(combined: &str) -> String {
    let (body, truncated, dropped_lines) =
        truncate_tail(combined, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut out = if body.trim().is_empty() {
        "(no output)".to_string()
    } else {
        body
    };
    if truncated {
        let location = write_overflow_file(combined)
            .map_or_else(|| "(unavailable)".to_string(), |p| p.display().to_string());
        out = format!(
            "[output truncated, dropped {dropped_lines} earlier line(s); full output saved to {location}]\n{out}"
        );
    }
    out
}

/// Resolve the shell used to run commands.
///
/// The tool is named `bash` and advertised as running bash, so bash-only
/// syntax (arrays, `[[ ]]`, `set -o pipefail`) must work. Prefer `/bin/bash`,
/// then `bash` discovered on `PATH`, and fall back to `sh` only when no bash is
/// available so the tool still runs on minimal systems.
pub(super) fn resolve_shell() -> PathBuf {
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
    fn bash_sandbox_blocks_write_outside_workspace() {
        // End-to-end: the wired-in sandbox must block a workspace escape at the
        // kernel level. The temp dirs are writable by policy, so the escape
        // targets $HOME. Skip if the kernel lacks Landlock or $HOME is unusable.
        if sandbox::detect_abi_for_test().is_none() {
            return;
        }
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let home = PathBuf::from(home);
        if home.starts_with(std::env::temp_dir()) {
            return;
        }
        let dir = temp_dir();
        let root = root_of(&dir);
        let outside = home.join(format!(
            ".iris-bash-escape-{}.txt",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let out = bash(
            &root,
            &BashInput {
                command: format!("echo escaped > {}", outside.display()),
                timeout: None,
            },
        )
        .unwrap();
        assert!(!outside.exists(), "sandbox did not block the escape: {out}");
        std::fs::remove_file(&outside).ok();
    }

    #[test]
    fn execute_session_persists_state_and_closes() {
        use serde_json::json;
        let dir = temp_dir();
        let root = root_of(&dir);
        std::fs::create_dir(root.join("sub")).unwrap();
        let mut state = BashState::new();

        // cd in one call is visible to the next call in the same session.
        execute(
            &root,
            &json!({ "command": "cd sub", "session": "s1" }),
            &mut state,
        )
        .unwrap();
        let pwd = execute(
            &root,
            &json!({ "command": "pwd", "session": "s1" }),
            &mut state,
        )
        .unwrap();
        assert!(
            pwd.content.trim_end().ends_with("/sub"),
            "pwd: {}",
            pwd.content
        );

        // A different session id is isolated (fresh cwd).
        let other = execute(
            &root,
            &json!({ "command": "pwd", "session": "s2" }),
            &mut state,
        )
        .unwrap();
        assert!(!other.content.trim_end().ends_with("/sub"));

        // close terminates the session; the next run starts fresh.
        let closed = execute(
            &root,
            &json!({ "session": "s1", "action": "close" }),
            &mut state,
        )
        .unwrap();
        assert!(closed.content.contains("closed"));
        let after = execute(
            &root,
            &json!({ "command": "pwd", "session": "s1" }),
            &mut state,
        )
        .unwrap();
        assert!(
            !after.content.trim_end().ends_with("/sub"),
            "session not reset after close"
        );
    }

    #[test]
    fn execute_background_job_start_poll_finalize() {
        use serde_json::json;
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut state = BashState::new();

        let started = execute(
            &root,
            &json!({ "action": "start", "command": "printf go; exit 4" }),
            &mut state,
        )
        .unwrap();
        // The id is echoed as job-0 for the first job.
        assert!(
            started.content.contains("job-0"),
            "start: {}",
            started.content
        );

        let listed = execute(&root, &json!({ "action": "list" }), &mut state).unwrap();
        assert!(listed.content.contains("job-0"));

        let fin = execute(
            &root,
            &json!({ "action": "finalize", "job": "job-0", "timeout": 5 }),
            &mut state,
        )
        .unwrap();
        assert!(
            fin.content.contains("go"),
            "finalize output: {}",
            fin.content
        );
        assert!(
            fin.content.contains("exit code 4"),
            "finalize: {}",
            fin.content
        );

        // Finalized jobs are gone.
        let after = execute(&root, &json!({ "action": "list" }), &mut state).unwrap();
        assert!(after.content.contains("no background jobs"));
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
