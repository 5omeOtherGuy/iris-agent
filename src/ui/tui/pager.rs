//! Alt-screen pager surface (ADR-0029).
//!
//! Two layers, split so each is testable without a TTY:
//!
//! - [`AltScreen`]: the alternate-screen lifecycle -- enter (`?1049h` + clear +
//!   home), leave (`?1049l`), and panic-safe restore. Restore runs through
//!   three independent paths that must all be idempotent: normal
//!   shutdown/`Drop`, the process panic hook, and the force-quit signal
//!   handler (`crate::signals`, which owns the async-signal-safe byte write).
//!   A single global "alt screen active" flag in `signals` arbitrates so
//!   exactly one path emits the leave sequence. Byte-golden testable over any
//!   `Write`.
//! - [`PagerSurface`]: the production full-frame renderer -- a ratatui
//!   `Terminal<CrosstermBackend<Stdout>>` drawing [`compose_frame`] output
//!   inside `?2026` synchronized-update blocks, with stock cell diffing.
//!   The frame composition ([`compose_frame`]) and cell placement
//!   ([`render_frame`]) are pure and golden-frame tested on a `TestBackend`.
//!
//! The pager renders the SAME logical document as the inline surface
//! ([`super::screen`]'s `render_document_with_hints`), sliced to the viewport:
//! session bar pinned at the top, bottom-anchored transcript tail (follow
//! view; the scroll offset lands in the next slice), working indicator and
//! composer pinned at the bottom.

use std::io::{self, Stdout, Write};
use std::sync::Once;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{MoveTo, Show};
use ratatui::crossterm::terminal::{
    BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen, disable_raw_mode,
};
use ratatui::crossterm::{execute, queue};
use ratatui::layout::Size;
use ratatui::text::Line;

use super::screen::{Screen, render_document_with_hints, session_bar_lines};

/// Owns the alternate-screen lifecycle for pager mode. The writer is a second
/// handle to the same terminal the `TerminalSurface` writes through; this type
/// only enters/leaves the alt screen and never renders content itself.
pub(crate) struct AltScreen<W: Write> {
    writer: W,
    active: bool,
}

impl<W: Write> AltScreen<W> {
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

impl<W: Write> Drop for AltScreen<W> {
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

/// Production pager renderer: alt-screen lifecycle + a ratatui `Terminal`
/// drawing full frames with stock cell diffing. Stdout-only by design; the
/// pure pieces ([`compose_frame`], [`render_frame`]) carry the tests.
pub(crate) struct PagerSurface {
    /// Alt-screen guard. Held (and dropped) alongside the terminal so leaving
    /// the alt screen is ordered after the last frame.
    alt: AltScreen<Stdout>,
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl PagerSurface {
    /// Enter the alternate screen and build the fullscreen ratatui terminal
    /// over stdout. On terminal construction failure the guard's `Drop`
    /// restores the normal screen.
    pub(crate) fn enter() -> io::Result<Self> {
        let alt = AltScreen::enter(io::stdout())?;
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        Ok(Self { alt, terminal })
    }

    /// Draw one full frame inside a `?2026` synchronized-update block. The
    /// fullscreen viewport autoresizes on each draw, so a terminal resize is
    /// just the next render.
    pub(crate) fn render(&mut self, lines: &[Line<'static>]) -> io::Result<()> {
        execute!(self.terminal.backend_mut(), BeginSynchronizedUpdate)?;
        let drawn = self
            .terminal
            .draw(|frame| render_frame(frame, lines))
            .map(|_| ());
        // Always close the sync block, even when the draw failed, so an error
        // can never leave the terminal buffering forever.
        let ended = execute!(self.terminal.backend_mut(), EndSynchronizedUpdate);
        drawn.and(ended)
    }

    /// Leave the alternate screen (idempotent; also covered by `Drop`).
    pub(crate) fn leave(&mut self) -> io::Result<()> {
        self.alt.leave()
    }
}

/// Compose the pager frame for `size` from the same logical document the
/// inline surface renders: session bar pinned at the top, bottom-anchored
/// transcript tail (follow view), working indicator + composer pinned at the
/// bottom. Slicing the shared document -- rather than re-composing regions --
/// guarantees both modes render identical logical state (ADR-0029).
pub(super) fn compose_frame(screen: &mut Screen, size: Size) -> Vec<Line<'static>> {
    let document = render_document_with_hints(screen, size);
    let height = usize::from(size.height);
    let mut frame = if document.lines.len() <= height {
        // The document pads itself to exactly the viewport height until the
        // transcript overflows, so this is the whole frame.
        document.lines
    } else {
        let bar_rows = session_bar_lines(screen, size.width).len().min(height);
        let body_rows = height - bar_rows;
        let start = document.lines.len() - body_rows;
        let mut frame = Vec::with_capacity(height);
        frame.extend_from_slice(&document.lines[..bar_rows]);
        frame.extend_from_slice(&document.lines[start..]);
        frame
    };
    // The zero-width hardware-cursor marker is stripped by the inline
    // surface's line renderer; the pager writes cells directly, so strip it
    // here (bounded scan: at most one viewport of lines).
    for line in &mut frame {
        if line
            .spans
            .iter()
            .any(|span| span.content.as_ref() == crate::ui::terminal_surface::CURSOR_MARKER)
        {
            line.spans
                .retain(|span| span.content.as_ref() != crate::ui::terminal_surface::CURSOR_MARKER);
        }
    }
    frame
}

/// Place composed lines into the frame buffer, top-aligned, truncated to the
/// frame area. Cells beyond the composed lines stay blank (ratatui resets the
/// back buffer each frame).
pub(super) fn render_frame(frame: &mut ratatui::Frame, lines: &[Line<'static>]) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    for (row, line) in lines.iter().take(usize::from(area.height)).enumerate() {
        buf.set_line(area.x, area.y + row as u16, line, area.width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::UiEvent;
    use ratatui::backend::TestBackend;

    /// The alt-screen active flag is process-global; the shared guard in
    /// `signals` serializes every test (in any module) that toggles it and
    /// resets the flag to inactive on acquisition.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::signals::alt_screen_test_guard()
    }

    fn footer_screen() -> Screen {
        let mut screen = Screen::new();
        screen.set_footer_with_context(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            Some("300k".to_string()),
            "~/repo (main)".to_string(),
        );
        screen
    }

    /// Render composed lines through a real ratatui `Terminal<TestBackend>`
    /// and return the buffer rows as strings.
    fn frame_rows(lines: &[Line<'static>], width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| render_frame(frame, lines))
            .expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn session_bar_stays_pinned_at_row_zero_through_a_10k_row_transcript() {
        let mut screen = footer_screen();
        for i in 0..10_000 {
            screen.apply(UiEvent::Notice(format!("row {i}")));
        }
        let size = Size::new(80, 24);
        let frame = compose_frame(&mut screen, size);
        assert_eq!(frame.len(), 24, "frame is exactly the viewport height");
        let rows = frame_rows(&frame, 80, 24);
        assert!(
            rows[0].contains("~/repo") && rows[0].contains("CTX"),
            "session bar pinned at row 0: {:?}",
            rows[0]
        );
        // The transcript body under the bar shows the NEWEST rows (follow).
        let body = rows[2..].join("\n");
        assert!(body.contains("row 9999"), "follow view shows the tail");
        assert!(!body.contains("row 1 "), "oldest rows are scrolled out");
    }

    #[test]
    fn composer_chrome_is_pinned_at_the_frame_bottom() {
        let mut screen = footer_screen();
        screen.apply(UiEvent::Notice("hello".to_string()));
        let frame = compose_frame(&mut screen, Size::new(60, 20));
        let rows = frame_rows(&frame, 60, 20);
        // Bottom padding row is blank; the statusline sits right above it and
        // carries the approval-policy segment.
        assert_eq!(rows[19].trim(), "");
        assert!(
            rows[18].contains("GPT-5.5") && rows[18].contains("\u{25c9}"),
            "composer statusline (mode glyph + model) at the bottom: {:?}",
            rows[18]
        );
    }

    #[test]
    fn start_page_renders_inside_the_pager_frame() {
        let mut screen = footer_screen();
        screen.show_start_page();
        let frame = compose_frame(&mut screen, Size::new(80, 30));
        assert_eq!(frame.len(), 30);
        let rows = frame_rows(&frame, 80, 30);
        let all = rows.join("\n");
        assert!(
            all.contains("Iris") || all.contains("iris"),
            "start page content present"
        );
    }

    #[test]
    fn width_height_sweep_never_overflows_or_panics() {
        for &width in &[1u16, 2, 10, 40, 80, 121] {
            for &height in &[1u16, 2, 5, 24, 50] {
                let mut screen = footer_screen();
                for i in 0..50 {
                    screen.apply(UiEvent::Notice(format!("line {i}")));
                }
                let frame = compose_frame(&mut screen, Size::new(width, height));
                assert!(
                    frame.len() <= usize::from(height),
                    "{width}x{height}: frame must fit the viewport"
                );
                // Rendering through a real terminal asserts no cell overflow.
                let _ = frame_rows(&frame, width, height);
            }
        }
    }

    #[test]
    fn enter_and_leave_emit_the_golden_sequences() {
        let _guard = lock();
        let mut surface = AltScreen::enter(Vec::new()).expect("enter");
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
        let mut surface = AltScreen::enter(Vec::new()).expect("enter");
        surface.leave().expect("leave");
        surface.writer.clear();
        surface.leave().expect("second leave");
        assert!(surface.writer.is_empty(), "second leave must be a no-op");

        // Drop after an explicit leave adds nothing; Drop without one leaves.
        let surface = AltScreen::enter(Vec::new()).expect("enter");
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
        let mut surface = AltScreen::enter(Vec::new()).expect("enter");
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
