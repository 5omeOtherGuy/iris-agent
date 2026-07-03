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

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);
static RESTORE_TERMINAL_ON_FORCE_QUIT: AtomicBool = AtomicBool::new(false);
// Whether the alt-screen pager is currently active (ADR-0029). Owned here so
// the async-signal-safe force-quit path, the panic hook, and normal Drop
// restore can arbitrate who emits the single `?1049l` leave sequence.
static ALT_SCREEN_ACTIVE: AtomicBool = AtomicBool::new(false);
// The terminal's pre-raw-mode `termios`, leaked so the signal handler can read
// it without allocation. Null until the TUI captures it at startup.
static SAVED_TERMIOS: AtomicPtr<libc::termios> = AtomicPtr::new(std::ptr::null_mut());

// Async-signal-safe terminal cleanup for a TUI force-quit path: show cursor and
// disable common mouse/focus-reporting/bracketed-paste modes.
// This is deliberately raw ANSI bytes so the signal handler can use `write(2)`
// instead of running crossterm/Drop code.
const TUI_FORCE_QUIT_RESTORE: &[u8] =
    b"\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1006l\x1b[?2004l\x1b[<1u";

// Leave the alternate screen (pager mode only). Written before the mode resets
// above so they apply to the restored normal screen. Kept separate and gated:
// emitting `?1049l` when the alt screen was never entered can move the cursor
// (DECRC restore), so inline mode must never send it.
const ALT_SCREEN_LEAVE: &[u8] = b"\x1b[?1049l";

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
    if !RESTORE_TERMINAL_ON_FORCE_QUIT.load(Ordering::Relaxed) {
        return;
    }
    // Restore cooked-mode termios first so the shell regains echo and line
    // editing -- `Drop` does not run on a signal-killed process, so the escape
    // write alone would leave the tty in raw mode. `tcsetattr` is POSIX
    // async-signal-safe.
    // Leave the alt screen first (pager mode only) so every subsequent byte
    // lands on the restored normal screen.
    if ALT_SCREEN_ACTIVE.swap(false, Ordering::Relaxed) {
        // SAFETY: `write` is async-signal-safe; pointer/length come from a
        // static byte string.
        let _ = unsafe {
            libc::write(
                libc::STDOUT_FILENO,
                ALT_SCREEN_LEAVE.as_ptr().cast(),
                ALT_SCREEN_LEAVE.len(),
            )
        };
    }
    let termios = SAVED_TERMIOS.load(Ordering::Acquire);
    if !termios.is_null() {
        // SAFETY: `termios` is a live, leaked pointer from
        // `save_termios_for_force_quit`; `tcsetattr` is async-signal-safe.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, termios);
        }
    }
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

/// Capture the terminal's current (pre-raw-mode) `termios` so the force-quit
/// signal handler can restore cooked mode. Call once, before enabling raw mode.
/// A no-op if stdin is not a terminal or a prior session already captured it.
pub(crate) fn save_termios_for_force_quit() {
    let mut term = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `tcgetattr` only writes through `term` for the duration of the
    // call; we act on the result solely when it reports success.
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, term.as_mut_ptr()) } != 0 {
        return;
    }
    // SAFETY: `tcgetattr` returned 0, so `term` is initialized.
    let ptr = Box::into_raw(Box::new(unsafe { term.assume_init() }));
    // Store exactly once; if a prior session already saved, drop the new box.
    if SAVED_TERMIOS
        .compare_exchange(
            std::ptr::null_mut(),
            ptr,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        // SAFETY: `ptr` came from `Box::into_raw` above and was not stored.
        drop(unsafe { Box::from_raw(ptr) });
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

/// Mark the alt-screen pager active/inactive. Set by the pager surface on
/// enter/leave so the force-quit and panic paths know a `?1049l` is owed.
pub(crate) fn set_alt_screen_active(active: bool) {
    ALT_SCREEN_ACTIVE.store(active, Ordering::Relaxed);
}

/// Whether the alt screen is currently marked active (test observability).
#[cfg(test)]
pub(crate) fn alt_screen_active() -> bool {
    ALT_SCREEN_ACTIVE.load(Ordering::Relaxed)
}

/// Atomically claim the pending alt-screen leave: returns `true` exactly once
/// per enter, so racing restore paths (Drop, panic hook, signal) emit exactly
/// one leave sequence.
pub(crate) fn take_alt_screen_active() -> bool {
    ALT_SCREEN_ACTIVE.swap(false, Ordering::Relaxed)
}

/// Record a terminal-driver Ctrl-C. Raw mode delivers Ctrl-C as a key event
/// rather than raising SIGINT, so the TUI read loop calls this to set the same
/// interrupt flag the per-turn watcher polls. A repeat reaps tracked child
/// process groups (matching the SIGINT handler) but does not re-raise, since the
/// read loop, not a signal, is in control.
pub(crate) fn interrupt_from_terminal() {
    if record_interrupt(&INTERRUPTED) {
        restore_terminal_from_signal();
        crate::process_group::kill_all_from_signal();
        std::process::exit(130);
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
