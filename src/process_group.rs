//! Centralized process-group spawning, termination, and force-quit reaping.
//!
//! Every shell the `bash` tool spawns (one-shot, persistent session, background
//! job) runs in its own process group so a single signal reaches the leader and
//! any children it backgrounds. This module is the single owner of that policy:
//!
//! - [`in_own_group`] puts a [`Command`] in a fresh process group.
//! - [`kill`] / [`kill_and_reap`] terminate a group and reap the leader.
//! - [`register`] records a live group's pgid in a fixed, lock-free table and
//!   returns a [`GroupGuard`] that unregisters on drop.
//! - [`kill_all_from_signal`] SIGKILLs every registered group using only
//!   async-signal-safe operations, so the force-quit SIGINT handler can reap the
//!   whole tree before the process dies (no orphaned children).

use std::process::{Child, Command};
use std::sync::atomic::{AtomicI32, Ordering};

/// Capacity of the live-group table. Far above the number of shells a single
/// synchronous agent can have running at once; registration past it is
/// best-effort (the group simply is not force-reaped).
const MAX_TRACKED: usize = 256;

/// Fixed, lock-free registry of live process-group ids (`0` = empty slot). A
/// plain array of atomics so [`kill_all_from_signal`] can scan it from a signal
/// handler without allocating or locking.
static GROUPS: [AtomicI32; MAX_TRACKED] = [const { AtomicI32::new(0) }; MAX_TRACKED];

/// Put `command` in its own process group so its PGID equals its PID. No-op on
/// non-Unix platforms.
pub(crate) fn in_own_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

/// SIGKILL a process group by id. No-op for non-positive ids or on non-Unix.
///
/// Uses `kill(-pgid, SIGKILL)` rather than `killpg`: it is equivalent (signals
/// every process in the group) but `kill` is on the POSIX async-signal-safe
/// list, so [`kill_all_from_signal`] may call this from a signal handler.
pub(crate) fn kill(pgid: i32) {
    #[cfg(unix)]
    if pgid > 0 {
        // SAFETY: FFI call with no Rust memory invariants; `SIGKILL` is valid
        // and a stale group simply yields `ESRCH`.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

/// Kill a spawned child's whole group and reap the leader so no zombie remains.
pub(crate) fn kill_and_reap(child: &mut Child) {
    if let Ok(pgid) = i32::try_from(child.id()) {
        kill(pgid);
    }
    // Also kill the leader directly: if it escaped its group (setsid), the group
    // signal misses it, and `wait` below would block forever otherwise.
    let _ = child.kill();
    let _ = child.wait();
}

/// Record `pgid` as a live group and return a guard that unregisters it on
/// drop. Non-positive ids and a full table return an inert guard.
pub(crate) fn register(pgid: i32) -> GroupGuard {
    if pgid > 0 {
        for (slot, cell) in GROUPS.iter().enumerate() {
            if cell
                .compare_exchange(0, pgid, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return GroupGuard { slot: Some(slot) };
            }
        }
    }
    GroupGuard { slot: None }
}

/// SIGKILL every registered process group. Async-signal-safe: only atomic loads
/// and `killpg`, no allocation or locking. Intended for the force-quit handler.
pub(crate) fn kill_all_from_signal() {
    for cell in GROUPS.iter() {
        // Acquire pairs with the release stores in `register`/`GroupGuard::drop`
        // so a handler running on another thread sees newly registered groups.
        let pgid = cell.load(Ordering::Acquire);
        if pgid != 0 {
            kill(pgid);
        }
    }
}

/// RAII handle that clears a group's registry slot when dropped.
pub(crate) struct GroupGuard {
    slot: Option<usize>,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            GROUPS[slot].store(0, Ordering::Release);
        }
    }
}

#[cfg(all(test, unix))]
fn tracked() -> Vec<i32> {
    GROUPS
        .iter()
        .map(|cell| cell.load(Ordering::Relaxed))
        .filter(|&pgid| pgid != 0)
        .collect()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    fn alive(pid: i32) -> bool {
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[test]
    fn register_tracks_then_guard_untracks() {
        // A sentinel above the typical pid range so it cannot collide with a
        // real group another test registers.
        let sentinel = 1_999_991;
        assert!(!tracked().contains(&sentinel));
        let guard = register(sentinel);
        assert!(tracked().contains(&sentinel));
        drop(guard);
        assert!(!tracked().contains(&sentinel));
    }

    #[test]
    fn kill_terminates_a_grouped_child() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        in_own_group(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let pid = child.id() as i32;
        assert!(alive(pid));
        kill(pid);
        let _ = child.wait();
        assert!(!alive(pid), "grouped child {pid} survived kill");
    }

    #[test]
    fn force_quit_reaps_a_backgrounded_grandchild() {
        use std::io::Read;
        use std::process::Stdio;
        // A shell that backgrounds a child and exits leaves the grandchild in
        // the same process group. Reaping the group (as force-quit does) must
        // kill the grandchild, not just the already-gone leader. The sleep is
        // redirected off the pipe so reading `$!` returns once the leader exits.
        let mut cmd = Command::new("/bin/bash");
        cmd.arg("-c")
            .arg("sleep 30 >/dev/null 2>&1 & echo $!")
            .stdout(Stdio::piped());
        in_own_group(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let pgid = child.id() as i32; // leader pid == process-group id
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        let grandchild: i32 = out.trim().parse().unwrap();
        let _ = child.wait(); // reap the leader, which has exited
        assert!(alive(grandchild), "backgrounded sleep should still run");

        kill(pgid);
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !alive(grandchild),
            "backgrounded grandchild survived group kill"
        );
    }

    // No direct test for `kill_all_from_signal`: it scans the global registry
    // and SIGKILLs every tracked group, so calling it under `cargo test`'s
    // parallel execution would kill groups other tests registered. Its body is
    // a thin loop over the `kill` path that the targeted-kill tests above cover.
}
