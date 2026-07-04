//! Advisory `flock` leases and the repo mutation lock (issue #285, ADR-0030).
//!
//! Iris supports multiple agent processes plus the user in one repo, so several
//! unsettled task records under `<git-dir>/iris/tasks/` coexist normally. A
//! record alone carries no liveness or ownership signal, so recovery cannot tell
//! a crashed orphan from a live foreign task. This module supplies the two
//! primitives ADR-0030 settles on:
//!
//! - a **per-task advisory lease** -- an exclusive `flock` on
//!   `<git-dir>/iris/tasks/<task-id>.lock`, held for the task's lifetime by
//!   keeping the open file. It proves liveness and ownership; a process crash
//!   releases it by construction (the OS closes the fd). Recovery adopts only
//!   lease-free tasks and skips live foreign ones.
//! - a **repo-scoped mutation lock** -- a short exclusive `flock` on
//!   `<git-dir>/iris/mutation.lock`, taken briefly around each record/ref write
//!   sequence so concurrent processes serialize instead of tearing shared state.
//!
//! `flock` is advisory and unix-only, which matches Iris (unix-only) and reuses
//! the already-direct `libc` dependency -- no new locking crate (ADR-0030).
//! Advisory locks may not enforce on exotic filesystems (some network mounts);
//! the degrade direction is a spurious "not lease-free" classification (never
//! adopting a live task) and a best-effort mutation lock, both safe.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a non-blocking lease probe/claim keeps re-checking a lock that
/// reports contention before it trusts the "held" answer (ADR-0030).
///
/// `flock` is per open-file-description, so `fork()` duplicates a lease fd into a
/// child, which keeps the lease held until the child `exec`s and its `O_CLOEXEC`
/// fds close. A process that both owns a lease and spawns a subprocess (git, a
/// shell tool) therefore has a brief window where an *unrelated* child pins the
/// lease fd, during which a single `LOCK_NB` probe would wrongly read a
/// lease-free orphan as "held" -- misclassifying it as a live foreign task and
/// skipping recovery. Re-probing across this window distinguishes that transient
/// pin (which clears at the child's `exec`) from a genuinely live owner (which
/// holds its lease continuously and still reads held after the window). The
/// window only adds latency to the rare recovery-time "held" case; it never
/// changes the classification, so a live foreign task is still never adopted.
const LEASE_PROBE_SETTLE: Duration = Duration::from_millis(250);

/// The `lock_protocol` value stamped on every record this build writes. A record
/// whose `lock_protocol` is `None` predates the lease protocol (legacy) and is
/// never auto-adopted.
pub(super) const LOCK_PROTOCOL: &str = "flock-v1";

/// The task-records directory, `<git-dir>/iris/tasks/`. The shared sibling of
/// the `refs/iris/*` chain, so a new session in the same repo finds the
/// unsettled tasks (recovery is per-repo, not per-session).
fn tasks_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("iris").join("tasks")
}

/// The lease lock-file path for `task_id`: `<git-dir>/iris/tasks/<task-id>.lock`,
/// beside its `<task-id>.json` record. Shared helper so tests target the exact
/// production path.
pub(super) fn lease_path(git_dir: &Path, task_id: &str) -> PathBuf {
    tasks_dir(git_dir).join(format!("{task_id}.lock"))
}

/// The repo mutation-lock path: `<git-dir>/iris/mutation.lock`. One per repo,
/// serializing every record/ref write sequence across processes.
pub(super) fn mutation_lock_path(git_dir: &Path) -> PathBuf {
    git_dir.join("iris").join("mutation.lock")
}

/// An held advisory `flock`. Dropping it closes the underlying file, which
/// releases the lock -- so a lease survives exactly as long as the owning
/// [`super::Task`] and a crash releases it for free.
pub(super) struct FlockGuard {
    // Held only for its `Drop`: closing the fd releases the advisory lock.
    _file: File,
}

/// Open (creating if needed) the lock file, ensuring its parent dir exists.
fn open_lock_file(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Try to take the exclusive lock without blocking. `Ok(Some(guard))` = acquired
/// (the file is lease-free); `Ok(None)` = another process holds it
/// (`EWOULDBLOCK`); `Err` = a real IO/lock error.
pub(super) fn try_exclusive(path: &Path) -> io::Result<Option<FlockGuard>> {
    let file = open_lock_file(path)?;
    loop {
        // SAFETY: `file` owns a valid fd for the duration of the call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(Some(FlockGuard { _file: file }));
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // Interrupted by a signal before acquiring: retry rather than
            // misreport contention or a hard error.
            Some(libc::EINTR) => continue,
            // LOCK_NB contention reports EWOULDBLOCK (== EAGAIN on Linux).
            Some(libc::EWOULDBLOCK) => return Ok(None),
            _ => return Err(err),
        }
    }
}

/// Take the exclusive lock, blocking until it is available. Used for the
/// short-lived mutation lock around a record/ref write sequence.
pub(super) fn exclusive_blocking(path: &Path) -> io::Result<FlockGuard> {
    let file = open_lock_file(path)?;
    loop {
        // SAFETY: `file` owns a valid fd for the duration of the call.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc == 0 {
            return Ok(FlockGuard { _file: file });
        }
        let err = io::Error::last_os_error();
        // A blocking `flock` interrupted by a signal must retry: returning the
        // error here would let `with_mutation_lock` proceed WITHOUT the lock and
        // silently bypass cross-process serialization (ADR-0030).
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(err);
    }
}

/// Take the exclusive lock without blocking, but re-probe across the
/// [`LEASE_PROBE_SETTLE`] window before reporting contention, so a transient
/// `fork()`+`exec()` fd-inheritance pin from an unrelated child does not read a
/// genuinely orphaned lease as held. `Ok(Some)` = acquired (lease-free);
/// `Ok(None)` = a live owner still holds it after the settle window; `Err` = a
/// real IO/lock error. Used by the recovery classification/claim paths
/// (`is_lease_free`, `adopt_task`, `expire_stale`); the plain single-shot
/// [`try_exclusive`] stays the primitive for a brand-new (always-free) lease.
pub(super) fn try_exclusive_settled(path: &Path) -> io::Result<Option<FlockGuard>> {
    let deadline = Instant::now() + LEASE_PROBE_SETTLE;
    loop {
        match try_exclusive(path)? {
            Some(guard) => return Ok(Some(guard)),
            None => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// Whether `path`'s lock is currently free (no live process holds it). Probes
/// via [`try_exclusive_settled`] (riding out a transient fork-window pin) and
/// immediately releases, so it is a probe, not a claim -- the caller re-acquires
/// and holds the lease in [`try_exclusive_settled`] when it actually adopts. A
/// probe IO error is treated as "not free" (the safe direction: never adopt a
/// task we cannot prove is orphaned).
pub(super) fn is_lease_free(path: &Path) -> bool {
    matches!(try_exclusive_settled(path), Ok(Some(_)))
}

/// Run `f` while holding the repo mutation lock, so concurrent processes
/// serialize their record/ref writes (ADR-0030). Best-effort: if the lock file
/// cannot be opened/locked (exotic filesystem), `f` still runs -- degrading to
/// the pre-lock last-writer-wins behavior rather than dropping the write.
pub(super) fn with_mutation_lock<T>(git_dir: &Path, f: impl FnOnce() -> T) -> T {
    let path = mutation_lock_path(git_dir);
    match exclusive_blocking(&path) {
        Ok(_guard) => f(),
        Err(error) => {
            tracing::warn!(error = %error, path = %path.display(), "mutation lock unavailable; proceeding without it");
            f()
        }
    }
}

/// A stable, opaque owner id for this process, stamped on records so a human (or
/// the #288 picker) can attribute an orphaned record. Liveness is proven by the
/// lease, not this id (PID reuse would make a PID alone unreliable), so it is
/// informational: `<pid>-<random>`.
pub(super) fn process_owner() -> String {
    use std::sync::OnceLock;
    static OWNER: OnceLock<String> = OnceLock::new();
    OWNER
        .get_or_init(|| format!("{}-{:08x}", std::process::id(), rand::random::<u32>()))
        .clone()
}
