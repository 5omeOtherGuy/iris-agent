//! Graceful SIGINT (Ctrl-C) handling for the interactive REPL.
//!
//! The agent loop is synchronous and blocks in provider HTTP calls and `bash`
//! children (the latter run in their own process group, so a terminal Ctrl-C
//! does not reach them). Rather than let the default disposition kill the
//! process mid-turn, the first Ctrl-C only sets an interrupt flag that the tool
//! loop checks between round-trips, ending the turn cleanly and returning to the
//! prompt. A second Ctrl-C restores the default handler and re-raises, so the
//! user can always force-quit even while blocked.
//!
//! Force-quit reaping: every `bash` child runs in its own process group, so a
//! second Ctrl-C must SIGKILL those groups before re-raising, or a long-running
//! child (timeout shell, persistent session, background job) would be orphaned.
//! The handler calls [`crate::process_group::kill_all_from_signal`], which is
//! async-signal-safe (atomic loads plus `kill(-pgid)`, no allocation or
//! locking).
//!
//! File writes are atomic (temp-file + rename), so an interrupt at any point
//! cannot leave a partially written file.

use std::sync::atomic::{AtomicBool, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static RESTORE_TERMINAL_ON_FORCE_QUIT: AtomicBool = AtomicBool::new(false);

// Async-signal-safe terminal cleanup for a TUI force-quit path: show cursor,
// disable common mouse/bracketed-paste modes, and leave the alternate screen.
// This is deliberately raw ANSI bytes so the signal handler can use `write(2)`
// instead of running crossterm/Drop code.
const TUI_FORCE_QUIT_RESTORE: &[u8] =
    b"\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?1049l";

/// Install the SIGINT handler. Call once at startup.
pub(crate) fn install() {
    let handler = handle_sigint as extern "C" fn(libc::c_int);
    // SAFETY: `handle_sigint` performs only async-signal-safe work: an atomic
    // store, and on a repeat interrupt `signal`/`raise`.
    unsafe {
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }
}

extern "C" fn handle_sigint(_signal: libc::c_int) {
    if record_interrupt(&INTERRUPTED) {
        // Second Ctrl-C: reap every tracked child process group so no shell is
        // orphaned, then restore the default disposition and re-raise so a
        // process blocked in a provider call or `bash` child can still be
        // force-quit.
        // SAFETY: all three calls are async-signal-safe (the reap does only
        // atomic loads and `kill(-pgid)`).
        restore_terminal_from_signal();
        crate::process_group::kill_all_from_signal();
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::raise(libc::SIGINT);
        }
    }
}

/// Set `flag`, returning `true` if it was already set (a repeat interrupt).
///
/// `Relaxed` is sufficient: the flag carries no data and synchronizes no other
/// memory; the handler and the loop only need this single boolean to be
/// atomic and eventually visible.
fn record_interrupt(flag: &AtomicBool) -> bool {
    flag.swap(true, Ordering::Relaxed)
}

fn restore_terminal_from_signal() {
    if RESTORE_TERMINAL_ON_FORCE_QUIT.load(Ordering::Relaxed) {
        // SAFETY: `write` is async-signal-safe; pointer/length come from a
        // static byte string.
        let _ = unsafe {
            libc::write(
                libc::STDOUT_FILENO,
                TUI_FORCE_QUIT_RESTORE.as_ptr().cast(),
                TUI_FORCE_QUIT_RESTORE.len(),
            )
        };
    }
}

/// Enable emergency terminal escape cleanup before a repeat Ctrl-C re-raises.
pub(crate) fn enable_terminal_restore_on_force_quit() {
    RESTORE_TERMINAL_ON_FORCE_QUIT.store(true, Ordering::Relaxed);
}

/// Disable emergency terminal cleanup once the TUI has restored normally.
pub(crate) fn disable_terminal_restore_on_force_quit() {
    RESTORE_TERMINAL_ON_FORCE_QUIT.store(false, Ordering::Relaxed);
}

/// Record a terminal-driver Ctrl-C. Raw mode delivers Ctrl-C as a key event
/// rather than raising SIGINT, so the TUI read loop calls this to set the same
/// interrupt flag the per-turn watcher polls. A repeat reaps tracked child
/// process groups (matching the SIGINT handler) but does not re-raise, since the
/// read loop, not a signal, is in control.
pub(crate) fn interrupt_from_terminal() {
    if record_interrupt(&INTERRUPTED) {
        crate::process_group::kill_all_from_signal();
    }
}

/// Whether a Ctrl-C is pending since the last [`reset`].
pub(crate) fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::Relaxed)
}

/// Clear the interrupt flag. Called at the start of each turn.
pub(crate) fn reset() {
    INTERRUPTED.store(false, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_force_quit_restore_flag_toggles() {
        disable_terminal_restore_on_force_quit();
        assert!(!RESTORE_TERMINAL_ON_FORCE_QUIT.load(Ordering::Relaxed));
        enable_terminal_restore_on_force_quit();
        assert!(RESTORE_TERMINAL_ON_FORCE_QUIT.load(Ordering::Relaxed));
        disable_terminal_restore_on_force_quit();
    }

    #[test]
    fn record_interrupt_flags_first_press_and_detects_repeat() {
        let flag = AtomicBool::new(false);
        // First press: not a repeat, flag now set.
        assert!(!record_interrupt(&flag));
        assert!(flag.load(Ordering::Relaxed));
        // Second press: reported as a repeat, which triggers the hard exit.
        assert!(record_interrupt(&flag));
    }
}
