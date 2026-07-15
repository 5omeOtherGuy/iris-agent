//! Background jobs for the `bash` tool.
//!
//! A job is a confined shell command that runs detached: [`start`](Jobs::start)
//! returns a job id immediately, [`poll`](Jobs::poll) streams output produced so
//! far, [`finalize`](Jobs::finalize) waits (bounded) for completion and the exit
//! code, [`list`](Jobs::list) enumerates jobs, and [`cancel`](Jobs::cancel)
//! kills one.
//!
//! Each job has a reader thread (stdout, with stderr merged via `exec 2>&1`)
//! appending into a bounded byte ring, and a waiter thread that blocks on the
//! child, records the exit status, and notifies a condvar (no busy-wait). The
//! ring keeps only the most recent bytes; overflow advances a dropped-byte
//! counter so callers can detect gaps. Output is addressed by an absolute byte
//! cursor, so polls return only what is new since the previous poll.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::Path;
use std::process::{ChildStdout, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use super::CANCEL_POLL_INTERVAL;
use super::sandbox;

/// Default per-job output ring capacity (bytes). This is a peak-memory bound on
/// a background job's retained output, NOT a display cap: it is intentionally
/// decoupled from `DEFAULT_MAX_BYTES` (the 50KB inline display window) so
/// lowering the display cap does not shrink how much job output is captured. A
/// job cannot grow memory without bound; the ring keeps the most recent ~1MB.
const DEFAULT_JOB_CAPACITY: usize = 1_000_000;

/// Cap on retained finished jobs. Bounds registry memory if the model starts
/// many jobs without finalizing them; the oldest finished jobs are evicted.
const MAX_RETAINED_FINISHED: usize = 64;

/// Lock the shared state, tolerating poisoning so one panicked worker thread
/// cannot crash every later tool call.
fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

/// Incremental view of a job returned by [`Jobs::poll`]/[`Jobs::finalize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobUpdate {
    /// Output produced since the previous poll (lossy UTF-8).
    pub(crate) output: String,
    /// Total bytes dropped from the ring over the job's life (gap indicator).
    pub(crate) dropped: u64,
    pub(crate) finished: bool,
    pub(crate) exit_code: Option<i32>,
    pub(crate) running: bool,
    /// Sandbox status notice when the shell could not be fully confined.
    pub(crate) notice: Option<String>,
}

/// One row of [`Jobs::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobInfo {
    pub(crate) id: String,
    pub(crate) label: Option<String>,
    pub(crate) running: bool,
    pub(crate) produced: u64,
    pub(crate) dropped: u64,
    pub(crate) exit_code: Option<i32>,
    pub(crate) notice: Option<String>,
}

/// Registry of background jobs.
pub(crate) struct Jobs {
    map: HashMap<String, Job>,
    cap: usize,
    seq: u64,
}

impl Jobs {
    pub(crate) fn new() -> Self {
        Self {
            map: HashMap::new(),
            cap: DEFAULT_JOB_CAPACITY,
            seq: 0,
        }
    }

    #[cfg(test)]
    fn with_capacity(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            cap,
            seq: 0,
        }
    }

    /// Spawn `command` detached and return its job id immediately.
    pub(crate) fn start(
        &mut self,
        root: &Path,
        command: &str,
        label: Option<String>,
    ) -> Result<String> {
        // Merge stderr into stdout so a single ordered stream feeds the ring.
        let wrapped = format!("exec 2>&1\n{command}");
        let mut cmd = Command::new(super::resolve_shell());
        cmd.arg("-c")
            .arg(&wrapped)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::process_group::in_own_group(&mut cmd);
        let policy = sandbox::policy_for_current_agent(root);
        let status = sandbox::confine(&mut cmd, &policy);
        sandbox::require_for_current_agent(&status)?;
        if let Some(notice) = status.notice() {
            tracing::warn!(%notice, "bash job sandbox not fully enforced");
        }

        let mut child = cmd.spawn().context("failed to spawn job shell")?;
        let pgid = i32::try_from(child.id()).context("job pid out of range")?;
        let stdout = child.stdout.take().context("missing job stdout")?;
        // Track this group for force-quit reaping; the worker drops the guard
        // once it has reaped the child below.
        let group = crate::process_group::register(pgid);

        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                buf: VecDeque::new(),
                dropped: 0,
                finished: false,
                exit_code: None,
                cap: self.cap,
                notice: status.notice(),
            }),
            cv: Condvar::new(),
        });

        // One worker per job: drain stdout to EOF, *then* reap and record the
        // exit code. Draining first guarantees all output is in the ring before
        // `finished` flips, so a finalize cannot snapshot a half-drained stream.
        let worker_shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let _group = group; // unregister once this worker reaps the child
            reader_loop(stdout, &worker_shared);
            // child.wait() reaps the process, so no zombie remains.
            let code = child.wait().ok().and_then(|s| s.code());
            let mut inner = lock(&worker_shared.inner);
            inner.finished = true;
            inner.exit_code = code;
            drop(inner);
            worker_shared.cv.notify_all();
        });

        let id = format!("job-{}", self.seq);
        self.seq += 1;
        self.map.insert(
            id.clone(),
            Job {
                shared,
                pgid,
                label,
                command: command.to_string(),
                read_cursor: 0,
            },
        );
        self.prune_finished();
        Ok(id)
    }

    /// Evict the oldest finished jobs beyond [`MAX_RETAINED_FINISHED`].
    fn prune_finished(&mut self) {
        let mut finished: Vec<u64> = self
            .map
            .iter()
            .filter(|(_, job)| lock(&job.shared.inner).finished)
            .filter_map(|(id, _)| job_seq(id))
            .collect();
        if finished.len() <= MAX_RETAINED_FINISHED {
            return;
        }
        finished.sort_unstable();
        let cutoff = finished.len() - MAX_RETAINED_FINISHED;
        for seq in finished.into_iter().take(cutoff) {
            self.map.remove(&format!("job-{seq}"));
        }
    }

    /// The command a live job was started with (for filter dispatch).
    pub(crate) fn command_of(&self, id: &str) -> Option<String> {
        self.map.get(id).map(|job| job.command.clone())
    }

    /// Return output produced since the previous poll plus liveness/exit state.
    pub(crate) fn poll(&mut self, id: &str) -> Result<JobUpdate> {
        let job = self.map.get_mut(id).context("unknown job")?;
        let inner = lock(&job.shared.inner);
        let (output, next) = read_from(&inner, job.read_cursor);
        let update = inner.to_update(output);
        drop(inner);
        job.read_cursor = next;
        Ok(update)
    }

    /// Wait up to `wait` (or unbounded if `None`) for completion, draining the
    /// remaining output. Removes the job once finished. A turn-level Ctrl-C
    /// (`cancel`) stops the wait and returns the partial update; the background
    /// job keeps running (only an explicit `action=cancel` kills it).
    pub(crate) fn finalize(
        &mut self,
        id: &str,
        wait: Option<Duration>,
        cancel: &CancellationToken,
    ) -> Result<JobUpdate> {
        // Clone the Arc so the condvar wait does not borrow `self`.
        let (shared, mut cursor) = {
            let job = self.map.get(id).context("unknown job")?;
            (Arc::clone(&job.shared), job.read_cursor)
        };
        let deadline = wait.map(|d| Instant::now() + d);

        let mut inner = lock(&shared.inner);
        while !inner.finished {
            if cancel.is_cancelled() {
                // Finalize interrupted; leave the job running and report its
                // current (still-running) state.
                break;
            }
            // Cap each wait at one cancel-poll slice so cancellation is observed
            // promptly even on an unbounded finalize; the full deadline is still
            // enforced by the zero-remaining break.
            let slice = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    remaining.min(CANCEL_POLL_INTERVAL)
                }
                None => CANCEL_POLL_INTERVAL,
            };
            let (guard, _) = shared
                .cv
                .wait_timeout(inner, slice)
                .unwrap_or_else(|e| e.into_inner());
            inner = guard;
        }
        let (output, next) = read_from(&inner, cursor);
        cursor = next;
        let update = inner.to_update(output);
        drop(inner);

        if update.finished {
            self.map.remove(id);
        } else if let Some(job) = self.map.get_mut(id) {
            job.read_cursor = cursor;
        }
        Ok(update)
    }

    /// Kill a running job's process group.
    pub(crate) fn cancel(&mut self, id: &str) -> Result<()> {
        let job = self.map.get(id).context("unknown job")?;
        // Only signal jobs still running: a finished job's pgid may have been
        // reused by an unrelated process group.
        //
        // ponytail: a narrow TOCTOU remains between the worker's child.wait()
        // freeing the pgid and it setting `finished`; subsystem 4's centralized
        // process-group primitive is where to close it if it ever matters.
        if !lock(&job.shared.inner).finished {
            crate::process_group::kill(job.pgid);
        }
        Ok(())
    }

    /// Enumerate all known jobs.
    pub(crate) fn list(&self) -> Vec<JobInfo> {
        let mut jobs: Vec<JobInfo> = self
            .map
            .iter()
            .map(|(id, job)| {
                let inner = lock(&job.shared.inner);
                JobInfo {
                    id: id.clone(),
                    label: job.label.clone(),
                    running: !inner.finished,
                    produced: inner.produced(),
                    dropped: inner.dropped,
                    exit_code: inner.exit_code,
                    notice: inner.notice.clone(),
                }
            })
            .collect();
        jobs.sort_by(|a, b| a.id.cmp(&b.id));
        jobs
    }

    #[cfg(test)]
    fn pgid(&self, id: &str) -> Option<i32> {
        self.map.get(id).map(|j| j.pgid)
    }
}

/// Registry-side handle for one job.
struct Job {
    shared: Arc<Shared>,
    pgid: i32,
    label: Option<String>,
    /// The command the job runs; used to dispatch the output filter
    /// (ADR-0037) when the job is finalized.
    command: String,
    /// Absolute byte offset of the next unread output byte.
    read_cursor: u64,
}

/// State shared with a job's reader and waiter threads.
struct Shared {
    inner: Mutex<Inner>,
    /// Notified when the job finishes.
    cv: Condvar,
}

struct Inner {
    buf: VecDeque<u8>,
    /// Total bytes dropped from the front of the ring (== absolute offset of
    /// `buf.front()`).
    dropped: u64,
    finished: bool,
    exit_code: Option<i32>,
    cap: usize,
    notice: Option<String>,
}

impl Inner {
    fn produced(&self) -> u64 {
        self.dropped + self.buf.len() as u64
    }

    fn to_update(&self, output: String) -> JobUpdate {
        JobUpdate {
            output,
            dropped: self.dropped,
            finished: self.finished,
            exit_code: self.exit_code,
            running: !self.finished,
            notice: self.notice.clone(),
        }
    }

    /// Append bytes, dropping the oldest once the ring exceeds its cap.
    fn append(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes);
        while self.buf.len() > self.cap {
            self.buf.pop_front();
            self.dropped += 1;
        }
    }
}

/// Parse the numeric suffix of a `job-<n>` id.
fn job_seq(id: &str) -> Option<u64> {
    id.strip_prefix("job-").and_then(|n| n.parse().ok())
}

/// Decoded bytes from absolute `cursor` to the current end, plus the new cursor.
/// A cursor pointing at already-dropped bytes resumes at the oldest available.
///
// ponytail: lossy UTF-8 can emit replacement chars when a multi-byte sequence
// is split across a poll boundary or dropped from the ring front; a stateful
// decoder is the upgrade path if that ever matters.
fn read_from(inner: &Inner, cursor: u64) -> (String, u64) {
    let from = cursor.max(inner.dropped);
    let skip = (from - inner.dropped) as usize;
    let bytes: Vec<u8> = inner.buf.iter().skip(skip).copied().collect();
    (
        String::from_utf8_lossy(&bytes).into_owned(),
        inner.produced(),
    )
}

/// Drain a job's merged stdout into its ring until EOF or a read error.
fn reader_loop(mut stdout: ChildStdout, shared: &Arc<Shared>) {
    let mut buf = [0u8; 8192];
    loop {
        match stdout.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => lock(&shared.inner).append(&buf[..n]),
        }
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        // Kill any still-running jobs; their waiter threads then reap the
        // children and exit. Finished jobs are already reaped.
        for job in self.map.values() {
            if !lock(&job.shared.inner).finished {
                crate::process_group::kill(job.pgid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{root_of, temp_dir};

    fn pid_alive(pid: i32) -> bool {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    fn poll_until<F: Fn(&JobUpdate) -> bool>(jobs: &mut Jobs, id: &str, pred: F) -> JobUpdate {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let upd = jobs.poll(id).unwrap();
            if pred(&upd) || Instant::now() >= deadline {
                return upd;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn start_returns_immediately() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut jobs = Jobs::new();
        let started = Instant::now();
        let id = jobs.start(&root, "sleep 2; echo done", None).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "start blocked instead of returning immediately"
        );
        assert!(jobs.poll(&id).unwrap().running);
        jobs.cancel(&id).unwrap();
    }

    #[test]
    fn poll_streams_output_incrementally() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut jobs = Jobs::new();
        let id = jobs
            .start(&root, "printf one; sleep 1; printf two", None)
            .unwrap();
        // First chunk arrives before the job finishes.
        let first = poll_until(&mut jobs, &id, |u| u.output.contains("one"));
        assert_eq!(first.output, "one");
        assert!(first.running);
        // finalize drains only the new bytes since the last poll.
        let fin = jobs
            .finalize(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .unwrap();
        assert_eq!(fin.output, "two");
        assert!(fin.finished);
        assert_eq!(fin.exit_code, Some(0));
    }

    #[test]
    fn finalize_reports_exit_code_and_full_output() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut jobs = Jobs::new();
        let id = jobs.start(&root, "echo hi; exit 3", None).unwrap();
        let fin = jobs
            .finalize(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .unwrap();
        assert_eq!(fin.output.trim_end(), "hi");
        assert_eq!(fin.exit_code, Some(3));
        assert!(fin.finished);
        // The job is removed once finalized.
        assert!(jobs.poll(&id).is_err());
    }

    #[test]
    fn list_reports_running_then_finished() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut jobs = Jobs::new();
        let id = jobs.start(&root, "echo a; sleep 2", None).unwrap();
        let listed = jobs.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert!(listed[0].running);
        jobs.cancel(&id).unwrap();
    }

    #[test]
    fn cancel_kills_running_job() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut jobs = Jobs::new();
        let id = jobs.start(&root, "sleep 30", None).unwrap();
        let pgid = jobs.pgid(&id).unwrap();
        assert!(pid_alive(pgid));
        jobs.cancel(&id).unwrap();
        // finalize observes completion (the waiter reaps the killed child).
        let fin = jobs
            .finalize(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .unwrap();
        assert!(fin.finished);
        std::thread::sleep(Duration::from_millis(50));
        assert!(!pid_alive(pgid), "cancelled job {pgid} still alive");
    }

    #[test]
    fn cancellation_interrupts_finalize_without_killing_job() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut jobs = Jobs::new();
        let id = jobs.start(&root, "sleep 30; echo done", None).unwrap();
        // Trip the turn token while finalize is waiting on the long job.
        let cancel = CancellationToken::new();
        let trip = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            trip.cancel();
        });
        let started = Instant::now();
        let upd = jobs
            .finalize(&id, Some(Duration::from_secs(30)), &cancel)
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancelled finalize did not return promptly"
        );
        assert!(
            !upd.finished,
            "job should still be running after a cancelled finalize"
        );
        assert!(
            jobs.list().iter().any(|j| j.id == id),
            "job should remain registered after a cancelled finalize"
        );
        jobs.cancel(&id).unwrap();
    }

    #[test]
    fn cancel_finished_job_is_a_noop() {
        // A finished job's pgid may be reused; cancel must not signal it.
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut jobs = Jobs::new();
        let id = jobs.start(&root, "echo done", None).unwrap();
        let finished = poll_until(&mut jobs, &id, |u| u.finished);
        assert!(finished.finished);
        // Still in the map (poll does not evict); cancel is a no-op, not an error.
        assert!(jobs.cancel(&id).is_ok());
    }

    #[test]
    fn ring_buffer_drops_oldest_and_counts_dropped() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let mut jobs = Jobs::with_capacity(16);
        // Produce 100 bytes into a 16-byte ring.
        let id = jobs
            .start(&root, "printf '%0.sX' $(seq 1 100)", None)
            .unwrap();
        let fin = jobs
            .finalize(&id, Some(Duration::from_secs(5)), &CancellationToken::new())
            .unwrap();
        assert!(fin.finished);
        assert_eq!(fin.exit_code, Some(0));
        assert!(fin.dropped >= 84, "expected drops, got {}", fin.dropped);
        assert!(
            fin.output.len() <= 16,
            "output exceeds ring cap: {} bytes",
            fin.output.len()
        );
        assert!(fin.output.chars().all(|c| c == 'X'));
    }
}
