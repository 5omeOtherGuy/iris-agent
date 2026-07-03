//! Alt-screen pager surface (ADR-0029).
//!
//! S1 scope: the alternate-screen lifecycle only -- enter (`?1049h` + clear +
//! home), leave (`?1049l`), and panic-safe restore. Restore runs through three
//! independent paths that must all be idempotent: normal shutdown/`Drop`, the
//! process panic hook, and the force-quit signal handler (`crate::signals`,
//! which owns the async-signal-safe byte write). A single global "alt screen
//! active" flag in `signals` arbitrates so exactly one path emits the leave
//! sequence.
//!
//! Full-frame rendering through ratatui `Terminal` lands in the next slice;
//! until then the pager hosts the existing document renderer inside the alt
//! screen.

use std::io::{self, Write};
use std::sync::Once;

use ratatui::crossterm::cursor::{MoveTo, Show};
use ratatui::crossterm::queue;
use ratatui::crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
};

/// Owns the alternate-screen lifecycle for pager mode. The writer is a second
/// handle to the same terminal the `TerminalSurface` writes through; this type
/// only enters/leaves the alt screen and never renders content itself.
pub(crate) struct PagerSurface<W: Write> {
    writer: W,
    active: bool,
}

impl<W: Write> PagerSurface<W> {
    /// Enter the alternate screen: `?1049h`, clear, cursor home. The global
    /// active flag is set so the panic hook and the force-quit signal path
    /// know a leave is owed.
    pub(crate) fn enter(mut writer: W) -> io::Result<Self> {
        // Mark active BEFORE writing: a partial write/flush failure may still
        // have delivered `?1049h`, so a leave is owed from the first byte. On
        // failure, best-effort leave immediately and clear the pending flag.
        crate::signals::set_alt_screen_active(true);
        let entered = queue!(
            writer,
            EnterAlternateScreen,
            Clear(ClearType::All),
            MoveTo(0, 0)
        )
        .and_then(|()| writer.flush());
        if let Err(error) = entered {
            if crate::signals::take_alt_screen_active() {
                let _ = queue!(writer, LeaveAlternateScreen);
                let _ = writer.flush();
            }
            return Err(error);
        }
        Ok(Self {
            writer,
            active: true,
        })
    }

    /// Leave the alternate screen exactly once across all restore paths: the
    /// local flag makes repeated `leave`/`Drop` calls no-ops, and the global
    /// take keeps this path from double-emitting after the panic hook already
    /// restored the screen.
    pub(crate) fn leave(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        if !crate::signals::take_alt_screen_active() {
            return Ok(());
        }
        queue!(self.writer, LeaveAlternateScreen)?;
        self.writer.flush()
    }
}

impl<W: Write> Drop for PagerSurface<W> {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

/// Install the process panic hook that restores the terminal before the
/// default hook prints the panic message -- otherwise the message would be
/// written to the alternate screen and vanish with it, and the user's shell
/// would be left inside a dead alt screen. Installed once, chains the previous
/// hook, and is a strict no-op while the pager is not active.
pub(crate) fn install_panic_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = emergency_restore(&mut io::stdout());
            previous(info);
        }));
    });
}

/// Leave the alt screen and show the cursor if (and only if) the pager is
/// active; also drops raw mode so the panic output is readable. Consumes the
/// global flag, making every later restore path a no-op.
fn emergency_restore<W: Write>(writer: &mut W) -> io::Result<()> {
    if !crate::signals::take_alt_screen_active() {
        return Ok(());
    }
    let _ = disable_raw_mode();
    queue!(writer, LeaveAlternateScreen, Show)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The alt-screen active flag is process-global; the shared guard in
    /// `signals` serializes every test (in any module) that toggles it and
    /// resets the flag to inactive on acquisition.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::signals::alt_screen_test_guard()
    }

    #[test]
    fn enter_and_leave_emit_the_golden_sequences() {
        let _guard = lock();
        let mut surface = PagerSurface::enter(Vec::new()).expect("enter");
        assert_eq!(surface.writer, b"\x1b[?1049h\x1b[2J\x1b[1;1H");
        assert!(crate::signals::alt_screen_active());
        surface.writer.clear();
        surface.leave().expect("leave");
        assert_eq!(surface.writer, b"\x1b[?1049l");
        assert!(!crate::signals::alt_screen_active());
    }

    #[test]
    fn leave_is_idempotent_and_drop_emits_it_once() {
        let _guard = lock();
        let mut surface = PagerSurface::enter(Vec::new()).expect("enter");
        surface.leave().expect("leave");
        surface.writer.clear();
        surface.leave().expect("second leave");
        assert!(surface.writer.is_empty(), "second leave must be a no-op");

        // Drop after an explicit leave adds nothing; Drop without one leaves.
        let surface = PagerSurface::enter(Vec::new()).expect("enter");
        drop(surface);
        assert!(!crate::signals::alt_screen_active());
    }

    #[test]
    fn emergency_restore_leaves_alt_screen_only_while_active() {
        let _guard = lock();
        crate::signals::set_alt_screen_active(false);
        let mut idle = Vec::new();
        emergency_restore(&mut idle).expect("restore");
        assert!(idle.is_empty(), "inactive pager must not write anything");

        crate::signals::set_alt_screen_active(true);
        let mut out = Vec::new();
        emergency_restore(&mut out).expect("restore");
        assert_eq!(out, b"\x1b[?1049l\x1b[?25h");
        assert!(!crate::signals::alt_screen_active());

        // A second emergency restore (hook then Drop racing) is a no-op.
        let mut again = Vec::new();
        emergency_restore(&mut again).expect("restore");
        assert!(again.is_empty());
    }

    #[test]
    fn drop_after_panic_hook_restore_does_not_double_emit() {
        let _guard = lock();
        let mut surface = PagerSurface::enter(Vec::new()).expect("enter");
        // Simulate the panic hook having already restored the screen.
        let mut hook_out = Vec::new();
        emergency_restore(&mut hook_out).expect("restore");
        assert_eq!(hook_out, b"\x1b[?1049l\x1b[?25h");
        surface.writer.clear();
        surface.leave().expect("leave");
        assert!(
            surface.writer.is_empty(),
            "leave after the hook restored must not re-emit ?1049l"
        );
    }
}
