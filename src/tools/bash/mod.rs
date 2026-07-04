//! `bash` — run a shell command with a timeout, process-group kill, and
//! bounded output drain/truncation.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::text::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_tail};

mod filter;
mod jobs;
mod sandbox;
mod session;

pub(crate) use sandbox::platform_can_sandbox;

// How long a bounded wait (session marker read, job finalize) may block before
// re-checking the turn cancellation token. Small enough that a Ctrl-C is
// observed promptly, large enough not to busy-poll.
pub(super) const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);
// Cap on how long we wait for the output reader threads to observe EOF after the
// shell has exited or been killed. A backgrounded process that escapes the
// shell's process group (via setsid/double-fork) can keep the pipes open; rather
// than block indefinitely we return whatever was captured within this window.
const BASH_DRAIN_TIMEOUT_SECS: u64 = 5;
// Peak-memory cap on captured output PER STREAM during a one-shot run. The pump
// threads stop forwarding (so the channel and the accumulator Vecs stop growing)
// once this is reached, but keep draining the pipe to EOF so a flooding child
// (`yes`, `cat /dev/zero`) never blocks and its exit status is still collected.
// The display window is `DEFAULT_MAX_BYTES` (50 KiB); 4 MiB per stream leaves a
// comfortable tail for that while bounding peak capture to ~8 MiB across both
// streams. This stays a memory-safety rail and is intentionally unchanged.
const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;

pub(super) const DESCRIPTION: &str = "Execute a bash command in the current working directory. Returns stdout and stderr. Output from known-noisy commands (build/test runners, installers, linters) is filtered to keep errors, failures, and summaries; pass `raw: true` to get the unfiltered output for one call. Output is truncated to last 2000 lines or 50KB (whichever is hit first). No timeout by default; set `timeout` (seconds) to bound a call. Pass `session` (any id string) to run in a persistent shell where `cd`, environment, and shell variables carry across calls. `action` may be `run` (default), `reset`, `close`, `start` a background job, `poll`, `finalize`, `cancel`, or `list` jobs.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": { "type": "string", "description": "Bash command to execute" },
            "timeout": { "type": "integer", "description": "Timeout in seconds (optional; no timeout when unset)" },
            "session": { "type": "string", "description": "Persistent shell session id; state (cd/env/vars) persists across calls with the same id" },
            "job": { "type": "string", "description": "Background job id for poll/finalize/cancel" },
            "action": { "type": "string", "enum": ["run", "reset", "close", "start", "poll", "finalize", "cancel", "list"], "description": "Action (default run): run/reset/close a session, or start/poll/finalize/cancel/list background jobs" },
            "raw": { "type": "boolean", "description": "Bypass output filtering for this call and return the full raw output" }
        }
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
    cancel: &CancellationToken,
    sink: Option<&dyn crate::nexus::ToolOutputSink>,
) -> Result<super::ToolOutput> {
    let parsed: BashArgs =
        serde_json::from_value(args.clone()).context("bash tool arguments must include command")?;
    // Destructure into owned fields so each arm can move what it needs without
    // cloning the session id to escape a borrow of `parsed`.
    let BashArgs {
        command,
        timeout,
        session,
        job,
        action,
        raw,
    } = parsed;
    // Exit code + wall-clock duration for the command-running arms (one-shot and
    // persistent session); `None` for the management arms (reset/close/jobs),
    // which carry no command status. Surfaced as `ToolOutput` metadata that
    // Nexus lifts onto the `ToolResult` event for the live exec cell.
    let mut exec: Option<(Option<i32>, Duration)> = None;
    let text = match action.as_deref().unwrap_or("run") {
        "run" => match session {
            Some(id) => {
                let (text, code, duration) =
                    run_session(root, &id, command, timeout, raw, state, cancel)?;
                exec = Some((code, duration));
                text
            }
            None => {
                let command = command.context("bash tool arguments must include command")?;
                let run = bash(
                    root,
                    &BashInput {
                        command,
                        timeout,
                        raw,
                    },
                    cancel,
                    sink,
                )?;
                exec = Some((run.exit_code, run.duration));
                run.text
            }
        },
        act @ ("reset" | "close") => {
            let id = session
                .as_deref()
                .context("bash session action requires 'session'")?;
            if act == "reset" {
                state.sessions.reset(id)?;
                format!("session '{id}' reset")
            } else {
                state.sessions.close(id)?;
                format!("session '{id}' closed")
            }
        }
        "start" => {
            let command = command
                .filter(|c| !c.trim().is_empty())
                .context("bash job start requires a command")?;
            let id = state.jobs.start(root, &command, None)?;
            format!("started background job '{id}'; poll it with action=poll, job='{id}'")
        }
        "poll" => {
            let id = job.as_deref().context("bash poll requires 'job'")?;
            render_job(id, state.jobs.poll(id)?)
        }
        "finalize" => {
            let id = job.as_deref().context("bash finalize requires 'job'")?;
            // No default wait: unset (or 0) blocks until the job completes
            // (cancellation-aware); a positive value bounds the wait.
            let wait = timeout.filter(|&secs| secs > 0).map(Duration::from_secs);
            // Capture the job's command before finalize removes the entry so
            // the output filter (ADR-0037) can dispatch on it.
            let job_command = state.jobs.command_of(id);
            let mut update = state.jobs.finalize(id, wait, cancel)?;
            let mut filter_notice = None;
            if update.finished
                && let Some(cmd) = job_command
            {
                let exit_ok = update.exit_code == Some(0);
                let (filtered, notice) =
                    filter_for_display(&cmd, std::mem::take(&mut update.output), exit_ok, raw);
                update.output = filtered;
                filter_notice = notice;
            }
            let text = render_job(id, update);
            match filter_notice {
                Some(notice) => format!("[{notice}]\n{text}"),
                None => text,
            }
        }
        "cancel" => {
            let id = job.as_deref().context("bash cancel requires 'job'")?;
            state.jobs.cancel(id)?;
            format!("cancelled background job '{id}'")
        }
        "list" => render_job_list(state.jobs.list()),
        other => bail!("unknown bash action: {other}"),
    };
    let mut output = super::ToolOutput::text(text);
    if let Some((code, duration)) = exec {
        output = output.with(
            "durationMs",
            json!(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
        );
        output = output.with("exitCode", code.map_or(Value::Null, |code| json!(code)));
    }
    Ok(output)
}

/// Route a session-scoped `run` to the session registry. The turn token is
/// passed through so a Ctrl-C interrupts a long-running command (the session
/// shell is killed and recreated on the next run, like a timeout).
fn run_session(
    root: &Path,
    id: &str,
    command: Option<String>,
    timeout: Option<u64>,
    raw: bool,
    state: &mut BashState,
    cancel: &CancellationToken,
) -> Result<(String, Option<i32>, Duration)> {
    let command = command
        .filter(|c| !c.trim().is_empty())
        .context("bash session run requires a command")?;
    // No default per-command timeout: unset (or 0) means no caller limit
    // (matching pi-mono). The session still enforces its wedge-safety hard cap
    // (`SESSION_HARD_CAP`) internally. A positive value is the per-call limit.
    let timeout = timeout.filter(|&secs| secs > 0).map(Duration::from_secs);
    let timeout_secs = timeout.map_or(0, |d| d.as_secs());
    // ponytail: session-path LIVE streaming is deferred (issue #90 follow-up).
    // The persistent shell interleaves a `__IRIS_DONE_<nonce>` completion marker
    // in the same stdout stream, so forwarding raw chunks would leak the
    // sentinel into the live display. Exit code + duration metadata is surfaced
    // here; live deltas for this path are the named follow-up. Upgrade path =
    // stream only the marker-safe prefix of the session buffer.
    let start = Instant::now();
    let mut outcome = state.sessions.run(root, id, &command, timeout, cancel)?;
    let duration = start.elapsed();
    let exit_code = outcome.exit_code;
    // Filter the captured output (ADR-0037) before the shared render path;
    // truncation, notices, and the exit/timeout footers are applied after and
    // are never altered by filtering.
    let exit_ok = !outcome.cancelled && !outcome.timed_out && outcome.exit_code == Some(0);
    let (filtered, filter_notice) =
        filter_for_display(&command, std::mem::take(&mut outcome.output), exit_ok, raw);
    outcome.output = filtered;
    let mut text = render_session(outcome, timeout_secs);
    if let Some(notice) = filter_notice {
        text = format!("[{notice}]\n{text}");
    }
    Ok((text, exit_code, duration))
}

/// Apply the ADR-0037 output filter for display. Returns the (possibly
/// reduced) output plus a provenance notice for the caller to prepend. `raw`
/// bypasses filtering for one call; every quality guard (fail-safe on filter
/// error/panic, empty-result guard, no-op detection) returns the input
/// unchanged with no notice.
fn filter_for_display(
    command: &str,
    output: String,
    exit_ok: bool,
    raw: bool,
) -> (String, Option<String>) {
    if raw {
        return (output, None);
    }
    match filter::filter_output(command, &output, exit_ok) {
        Some(filtered) => {
            let notice = format!(
                "output filtered ({}): {} -> {} line(s); rerun with raw=true for full output",
                filtered.name,
                output.lines().count(),
                filtered.text.lines().count(),
            );
            (filtered.text, Some(notice))
        }
        None => (output, None),
    }
}

/// Format a background-job update: bounded output, drop/sandbox notices, status.
fn render_job(id: &str, update: jobs::JobUpdate) -> String {
    let mut out = if update.output.is_empty() {
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
    if outcome.output_dropped > 0 {
        out = format!(
            "[output truncated: {} byte(s) dropped]\n{out}",
            outcome.output_dropped
        );
    }
    if let Some(notice) = outcome.notice {
        out = format!("[{notice}]\n{out}");
    }
    if outcome.cancelled {
        out.push_str("\n\nCommand cancelled by user; session terminated");
    } else if outcome.timed_out {
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
    /// Bypass output filtering (ADR-0037) for this call.
    #[serde(default)]
    raw: bool,
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
    /// Bypass output filtering (ADR-0037) for this call.
    #[serde(default)]
    raw: bool,
}

/// One-shot `bash` result: rendered output plus the command's exit code and
/// wall-clock duration, surfaced as `ToolOutput` metadata for the live exec
/// cell. `exit_code` is `None` when the shell reported no status (cancelled,
/// timed out, or killed).
struct BashRun {
    text: String,
    exit_code: Option<i32>,
    duration: Duration,
}

fn bash(
    root: &Path,
    input: &BashInput,
    cancel: &CancellationToken,
    sink: Option<&dyn crate::nexus::ToolOutputSink>,
) -> Result<BashRun> {
    if input.command.trim().is_empty() {
        bail!("bash command must not be empty");
    }
    // No default timeout: unset (or 0) means run with no time limit (matching
    // pi-mono); a positive value is the per-call limit in seconds.
    let timeout = input
        .timeout
        .filter(|&secs| secs > 0)
        .map(Duration::from_secs);
    let timeout_secs = timeout.map_or(0, |d| d.as_secs());

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
    let (tx, rx) = std::sync::mpsc::channel::<PumpMsg>();
    let stdout_tx = tx.clone();
    std::thread::spawn(move || pump_pipe(&mut stdout, BashStream::Stdout, &stdout_tx));
    std::thread::spawn(move || pump_pipe(&mut stderr, BashStream::Stderr, &tx));

    // Accumulators for the final model-facing output. Filled live from the pump
    // channel during the wait below and by the post-exit drain, so a chunk is
    // forwarded to the sink the moment it is produced (not batched at the end).
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut capture_truncated = false;

    let start = Instant::now();
    let mut timed_out = false;
    let mut cancelled = false;
    let status = loop {
        match child.try_wait().context("failed to wait for shell")? {
            Some(status) => break Some(status),
            None => {
                // A turn-level Ctrl-C cancels the child token: terminate the
                // whole group like a timeout does, so the call returns promptly.
                if cancel.is_cancelled() {
                    crate::process_group::kill_and_reap(&mut child);
                    cancelled = true;
                    break None;
                }
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
                // While the child runs, forward whatever it has produced so far
                // instead of idle-sleeping: this is what makes the exec cell
                // stream live. The bounded `recv_timeout` preserves the old
                // ~20ms re-check cadence when the child is quiet, and a
                // `Disconnected` channel (both pumps finished before the child
                // exited) falls back to the plain poll sleep.
                match rx.recv_timeout(Duration::from_millis(20)) {
                    Ok(msg) => consume_pump_msg(
                        msg,
                        sink,
                        &mut stdout_bytes,
                        &mut stderr_bytes,
                        &mut capture_truncated,
                    ),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
        }
    };

    // Drain the remaining buffered chunks until the pump threads finish (the
    // channel disconnects) or the drain deadline passes. A process that escaped
    // the shell's group (setsid/double-fork) can keep a pipe open after the
    // shell exits; rather than block on it forever we return the output
    // captured so far. The streaming pump means already-written output is
    // delivered even when a later holder keeps the pipe open.
    //
    // ponytail: pump-thread + FD leak ceiling. If a backgrounded child uses
    // setsid/double-fork to escape the group and holds a pipe open past the
    // drain deadline, the detached pump thread stays blocked in `pipe.read()`
    // and leaks one thread + FD until that child finally closes the pipe. There
    // is no clean std-only way to interrupt a blocking pipe read: closing the
    // fd from another thread races fd reuse and does not wake the read on Linux,
    // and non-blocking pipes would force a busy-poll loop on every command for
    // this rare case. Upgrade path: an interruptible reader built on
    // poll/epoll + a self-pipe wakeup (libc) or a small poll crate.
    let drain_deadline = Instant::now() + Duration::from_secs(BASH_DRAIN_TIMEOUT_SECS);
    loop {
        let remaining = drain_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(msg) => consume_pump_msg(
                msg,
                sink,
                &mut stdout_bytes,
                &mut stderr_bytes,
                &mut capture_truncated,
            ),
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

    // Filter the captured output (ADR-0037) after capture, before the
    // truncate-tail backstop. Exit codes, timeout/cancel footers, and the
    // notices below are appended afterwards and never altered by filtering.
    let exit_ok = !cancelled && !timed_out && status.is_some_and(|s| s.success());
    let (combined, filter_notice) =
        filter_for_display(&input.command, combined, exit_ok, input.raw);

    let mut out = render_output(&combined);

    if let Some(notice) = filter_notice {
        out = format!("[{notice}]\n{out}");
    }

    if capture_truncated {
        out = format!(
            "[output truncated: exceeded {} MiB capture cap per stream]\n{out}",
            MAX_CAPTURE_BYTES / (1024 * 1024)
        );
    }

    if let Some(notice) = sandbox.notice() {
        out = format!("[{notice}]\n{out}");
    }

    if cancelled {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Command cancelled by user");
    } else if timed_out {
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

    Ok(BashRun {
        text: out,
        exit_code: status.and_then(|status| status.code()),
        duration: start.elapsed(),
    })
}

/// Apply the shared output policy: keep the bounded tail, mark `(no output)`
/// when empty, and report truncation without writing a temp-file spill.
fn render_output(combined: &str) -> String {
    let (body, truncated, dropped_lines) =
        truncate_tail(combined, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut out = if body.trim().is_empty() {
        "(no output)".to_string()
    } else {
        body
    };
    if truncated {
        out = format!("[output truncated, dropped {dropped_lines} earlier line(s)]\n{out}");
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

/// Message from a pump thread: a captured chunk, or a one-shot signal that the
/// per-stream capture cap was hit and later bytes were dropped.
enum PumpMsg {
    Chunk(BashStream, Vec<u8>),
    Truncated,
}

/// Forward one pump message to the live sink and accumulate it for the final
/// result. Shared by the wait loop (live streaming) and the post-exit drain so
/// both paths apply identical decode/accumulate/truncation handling.
///
/// ponytail: per-chunk lossy UTF-8 can emit a replacement char when a multibyte
/// sequence straddles a chunk boundary -- display-only, so acceptable; the
/// accumulated bytes are decoded whole for the model-facing output.
fn consume_pump_msg(
    msg: PumpMsg,
    sink: Option<&dyn crate::nexus::ToolOutputSink>,
    stdout_bytes: &mut Vec<u8>,
    stderr_bytes: &mut Vec<u8>,
    capture_truncated: &mut bool,
) {
    match msg {
        PumpMsg::Chunk(stream, chunk) => {
            if let Some(sink) = sink {
                sink.emit_chunk(&String::from_utf8_lossy(&chunk));
            }
            match stream {
                BashStream::Stdout => stdout_bytes.extend_from_slice(&chunk),
                BashStream::Stderr => stderr_bytes.extend_from_slice(&chunk),
            }
        }
        PumpMsg::Truncated => *capture_truncated = true,
    }
}

/// Stream a child pipe to the collector in chunks so already-written output is
/// delivered even if the pipe is later held open by an escaped process. Forwards
/// at most `MAX_CAPTURE_BYTES` to bound peak memory, then keeps reading to EOF
/// (so the child never blocks on a full pipe) without forwarding. Exits on EOF,
/// read error, or once the receiver has hung up.
fn pump_pipe(pipe: &mut impl Read, stream: BashStream, tx: &std::sync::mpsc::Sender<PumpMsg>) {
    let mut buf = [0u8; 8192];
    let mut forwarded = 0usize;
    let mut truncated = false;
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if forwarded >= MAX_CAPTURE_BYTES {
                    // Cap reached: keep draining to EOF, stop forwarding.
                    truncated = true;
                    continue;
                }
                let take = n.min(MAX_CAPTURE_BYTES - forwarded);
                if tx
                    .send(PumpMsg::Chunk(stream, buf[..take].to_vec()))
                    .is_err()
                {
                    break;
                }
                forwarded += take;
                if take < n {
                    truncated = true;
                }
            }
        }
    }
    if truncated {
        let _ = tx.send(PumpMsg::Truncated);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{root_of, temp_dir};
    use std::cell::RefCell;

    /// Records every chunk forwarded to the streaming sink (single-threaded;
    /// `bash()` forwards on the calling thread).
    struct RecordingSink {
        chunks: RefCell<Vec<String>>,
    }
    impl RecordingSink {
        fn new() -> Self {
            Self {
                chunks: RefCell::new(Vec::new()),
            }
        }
        fn joined(&self) -> String {
            self.chunks.borrow().concat()
        }
    }
    impl crate::nexus::ToolOutputSink for RecordingSink {
        fn emit_chunk(&self, chunk: &str) {
            self.chunks.borrow_mut().push(chunk.to_string());
        }
    }

    #[test]
    fn bash_streams_chunks_to_sink_while_accumulating_final_output() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let sink = RecordingSink::new();
        let run = bash(
            &root,
            &BashInput {
                command: "printf 'one\\ntwo\\n'".into(),
                timeout: None,
                raw: false,
            },
            &CancellationToken::new(),
            Some(&sink),
        )
        .unwrap();
        // The sink saw the live chunks as they arrived.
        assert!(sink.joined().contains("one"), "sink missing live output");
        assert!(sink.joined().contains("two"), "sink missing live output");
        // The final accumulated output is still complete.
        assert!(run.text.contains("one") && run.text.contains("two"));
    }

    #[test]
    fn bash_without_sink_behaves_unchanged() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let run = bash(
            &root,
            &BashInput {
                command: "echo hi".into(),
                timeout: None,
                raw: false,
            },
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert!(run.text.contains("hi"));
    }

    #[test]
    fn execute_run_attaches_exit_code_and_duration_metadata() {
        use serde_json::json;
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut state = BashState::new();
        // Non-zero exit: metadata must carry the exact code and a duration.
        let out = execute(
            &root,
            &json!({ "command": "exit 3" }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert_eq!(out.metadata.get("exitCode"), Some(&json!(3)));
        assert!(
            out.metadata
                .get("durationMs")
                .and_then(Value::as_u64)
                .is_some(),
            "durationMs metadata missing: {:?}",
            out.metadata
        );
    }

    #[test]
    fn execute_session_run_attaches_exit_code_and_duration_metadata() {
        use serde_json::json;
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut state = BashState::new();
        // The persistent-session path carries exit code + duration metadata too.
        // Use a subshell so the non-zero status is reported without killing the
        // session shell (a bare `exit` would close the shell -> no exit code).
        let out = execute(
            &root,
            &json!({ "command": "(exit 5)", "session": "s1", "timeout": 5 }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert_eq!(out.metadata.get("exitCode"), Some(&json!(5)));
        assert!(
            out.metadata
                .get("durationMs")
                .and_then(Value::as_u64)
                .is_some(),
            "durationMs metadata missing on session path: {:?}",
            out.metadata
        );
    }

    #[test]
    fn bash_truncation_does_not_spill_full_output_to_temp_file() {
        let combined = format!("old\n{}", "x\n".repeat(DEFAULT_MAX_LINES + 1));

        let out = render_output(&combined);

        assert!(out.contains("output truncated"), "{out}");
        assert!(!out.contains("full output saved to"), "{out}");
        assert!(!out.contains("iris-bash-output-"), "{out}");
    }

    #[test]
    fn bash_runs_command_and_captures_output() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let out = bash(
            &root,
            &BashInput {
                command: "echo hello".into(),
                timeout: None,
                raw: false,
            },
            &CancellationToken::new(),
            None,
        )
        .unwrap()
        .text;
        assert!(out.contains("hello"));
    }

    #[test]
    fn bash_unset_timeout_imposes_no_default_limit() {
        // With no `timeout`, the command runs to completion with no default
        // time limit (pi-mono parity): a quick command succeeds and reports no
        // timeout footer.
        let dir = temp_dir();
        let root = root_of(&dir);
        let run = bash(
            &root,
            &BashInput {
                command: "echo ok".into(),
                timeout: None,
                raw: false,
            },
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert!(run.text.contains("ok"), "out: {}", run.text);
        assert!(!run.text.contains("timed out"), "out: {}", run.text);
        assert_eq!(run.exit_code, Some(0));
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
                raw: false,
            },
            &CancellationToken::new(),
            None,
        )
        .unwrap()
        .text;
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
                raw: false,
            },
            &CancellationToken::new(),
            None,
        )
        .unwrap()
        .text;
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
                raw: false,
            },
            &CancellationToken::new(),
            None,
        )
        .unwrap()
        .text;
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
                raw: false,
            },
            &CancellationToken::new(),
            None,
        )
        .unwrap()
        .text;
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
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let out = bash(
            &root,
            &BashInput {
                command: format!("echo escaped > {}", outside.display()),
                timeout: None,
                raw: false,
            },
            &CancellationToken::new(),
            None,
        )
        .unwrap()
        .text;
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
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        let pwd = execute(
            &root,
            &json!({ "command": "pwd", "session": "s1" }),
            &mut state,
            &CancellationToken::new(),
            None,
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
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert!(!other.content.trim_end().ends_with("/sub"));

        // close terminates the session; the next run starts fresh.
        let closed = execute(
            &root,
            &json!({ "session": "s1", "action": "close" }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert!(closed.content.contains("closed"));
        let after = execute(
            &root,
            &json!({ "command": "pwd", "session": "s1" }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert!(
            !after.content.trim_end().ends_with("/sub"),
            "session not reset after close"
        );
    }

    #[test]
    fn execute_session_reports_bounded_output() {
        use serde_json::json;
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut state = BashState::new();

        let out = execute(
            &root,
            &json!({
                "command": "python3 - <<'PY'\nprint('x' * (2 * 1024 * 1024))\nPY",
                "session": "s1",
                "timeout": 5
            }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();

        assert!(out.content.contains("output truncated"), "{}", out.content);
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
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        // The id is echoed as job-0 for the first job.
        assert!(
            started.content.contains("job-0"),
            "start: {}",
            started.content
        );

        let listed = execute(
            &root,
            &json!({ "action": "list" }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert!(listed.content.contains("job-0"));

        let fin = execute(
            &root,
            &json!({ "action": "finalize", "job": "job-0", "timeout": 5 }),
            &mut state,
            &CancellationToken::new(),
            None,
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
        let after = execute(
            &root,
            &json!({ "action": "list" }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert!(after.content.contains("no background jobs"));
    }

    #[test]
    fn bash_bounds_high_volume_output() {
        // A flooding command (20 MiB of zeros) must not be captured in full:
        // peak memory is bounded by MAX_CAPTURE_BYTES, the user sees a
        // truncation marker, and the exit status (0) is still reported.
        let dir = temp_dir();
        let root = root_of(&dir);
        let out = bash(
            &root,
            &BashInput {
                command: "head -c 20000000 /dev/zero".into(),
                timeout: Some(30),
                raw: false,
            },
            &CancellationToken::new(),
            None,
        )
        .unwrap()
        .text;
        // The capture-specific marker proves the per-stream cap fired (the pump
        // stopped forwarding), not merely the display-tail truncation.
        assert!(
            out.contains("capture cap per stream"),
            "expected a capture truncation marker, got: {}",
            &out[..out.len().min(200)]
        );
        // Bounded well under the 20 MiB the command produced.
        assert!(
            out.len() < MAX_CAPTURE_BYTES + 64 * 1024,
            "captured output was not bounded: {} bytes",
            out.len()
        );
        // Exit code 0 -> no failure footer.
        assert!(
            !out.contains("Command exited with code"),
            "unexpected nonzero exit footer: {}",
            &out[out.len().saturating_sub(200)..]
        );
    }

    /// A command whose last segment dispatches the `shellcheck` filter but
    /// whose output comes from a local shell function -- deterministic
    /// end-to-end filtering without shellcheck installed.
    const FILTERED_CMD: &str =
        "shellcheck() { printf 'In x line 1:\\n\\nfoo\\n'; }; shellcheck x.sh";

    #[test]
    fn bash_filters_matching_output_and_reports_provenance() {
        use serde_json::json;
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut state = BashState::new();
        let out = execute(
            &root,
            &json!({ "command": FILTERED_CMD }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert!(
            out.content
                .contains("output filtered (shellcheck): 3 -> 2 line(s)"),
            "missing provenance notice: {}",
            out.content
        );
        assert!(out.content.contains("In x line 1:\nfoo"), "{}", out.content);
        // Exit code metadata is untouched by filtering.
        assert_eq!(out.metadata.get("exitCode"), Some(&json!(0)));
    }

    #[test]
    fn bash_raw_true_bypasses_filtering() {
        use serde_json::json;
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut state = BashState::new();
        let out = execute(
            &root,
            &json!({ "command": FILTERED_CMD, "raw": true }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert!(
            !out.content.contains("output filtered"),
            "raw:true must bypass filtering: {}",
            out.content
        );
        // The blank line the filter would strip is still present.
        assert!(
            out.content.contains("In x line 1:\n\nfoo"),
            "raw output altered: {}",
            out.content
        );
    }

    #[test]
    fn bash_filtering_preserves_error_lines_and_exit_footer() {
        use serde_json::json;
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut state = BashState::new();
        // Last segment dispatches the cargo-test filter; the function emits
        // compiler noise plus an error line and fails with 101.
        let cmd = "cargo() { echo '   Compiling foo v0.1.0'; echo 'error[E0308]: mismatched types'; return 101; }; cargo test";
        let out = execute(
            &root,
            &json!({ "command": cmd }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert!(
            out.content.contains("error[E0308]: mismatched types"),
            "error line lost: {}",
            out.content
        );
        assert!(
            !out.content.contains("Compiling foo"),
            "noise not stripped: {}",
            out.content
        );
        assert!(
            out.content.contains("Command exited with code 101"),
            "exit footer lost: {}",
            out.content
        );
        assert_eq!(out.metadata.get("exitCode"), Some(&json!(101)));
    }

    #[test]
    fn session_run_applies_output_filter() {
        use serde_json::json;
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut state = BashState::new();
        let out = execute(
            &root,
            &json!({ "command": FILTERED_CMD, "session": "s1", "timeout": 10 }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert!(
            out.content.contains("output filtered (shellcheck)"),
            "session path missing filter: {}",
            out.content
        );
        // raw bypass works on the session path too.
        let raw = execute(
            &root,
            &json!({ "command": FILTERED_CMD, "session": "s1", "timeout": 10, "raw": true }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert!(!raw.content.contains("output filtered"), "{}", raw.content);
    }

    #[test]
    fn job_finalize_applies_output_filter() {
        use serde_json::json;
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut state = BashState::new();
        execute(
            &root,
            &json!({ "action": "start", "command": FILTERED_CMD }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        let fin = execute(
            &root,
            &json!({ "action": "finalize", "job": "job-0", "timeout": 10 }),
            &mut state,
            &CancellationToken::new(),
            None,
        )
        .unwrap();
        assert!(
            fin.content.contains("output filtered (shellcheck)"),
            "finalized job missing filter: {}",
            fin.content
        );
        assert!(
            fin.content.contains("job 'job-0' finished (exit code 0)"),
            "job status altered: {}",
            fin.content
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
                raw: false,
            },
            &CancellationToken::new(),
            None,
        )
        .unwrap()
        .text;
        assert!(out.contains("Command exited with code 3"));
    }
}
