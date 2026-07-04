//! One-time runtime probe for terminal ZWJ emoji shaping (issue #351).
//!
//! On terminals whose font stack does not shape ZWJ emoji sequences, a family
//! cluster (`MAN ZWJ WOMAN ZWJ GIRL`) is drawn as several side-by-side faces
//! (~6 columns) while Iris, ratatui, and `unicode-width` all model it as 2
//! columns. That width mismatch drifts table columns on first paint and corrupts
//! the pager's cell-diff repaints on scroll. There is no escape code or terminfo
//! capability that reports shaping -- it is a font property -- so the only honest
//! signal is to measure the rendered width at startup.
//!
//! The probe prints the cluster at a known column, asks the terminal where the
//! cursor landed (a DSR cursor-position round-trip), and compares the reported
//! width against the modeled width. The result is recorded in
//! [`crate::ui::textengine`] and consumed by `normalize_zwj` at the transcript
//! entry points, plus reported by the `/terminal-setup` doctor.
//!
//! Reuse ladder: the DSR round-trip is delegated to `crossterm::cursor::position`
//! (the production seam), which reads the reply through crossterm's own internal
//! event source. That keeps the probe on the same fd discipline as the event
//! loop -- any non-CPR bytes typed during the probe are buffered by crossterm and
//! delivered later, never dropped -- instead of hand-rolling a stdin CPR read
//! that would race crossterm for the tty. The escape/measure/erase sequence is
//! kept behind the [`CursorProbe`] seam so it is unit-tested with a scripted
//! response and no TTY.

use std::io::{self, Write};

use crate::ui::textengine::ZwjShaping;

/// Probe glyph: a family ZWJ sequence (MAN + ZWJ + WOMAN + ZWJ + GIRL).
/// `unicode-width` 0.2 models it as 2 columns; a non-shaping font draws it as
/// three faces (~6 columns).
const PROBE_CLUSTER: &str = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
/// Modeled cluster width (UAX #11 + `unicode-width` 0.2 ZWJ handling).
const MODELED_WIDTH: u16 = 2;

/// Terminal IO seam for the probe so the escape/measure/erase sequence is
/// unit-testable with a scripted cursor response instead of a real TTY.
pub(crate) trait CursorProbe {
    /// Write raw bytes to the terminal and flush.
    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Read the current cursor column (0-indexed) via a DSR round-trip. `Err`
    /// on timeout or an unreadable/garbled reply.
    fn cursor_col(&mut self) -> io::Result<u16>;
}

/// Run the probe sequence over the [`CursorProbe`] seam and classify the
/// terminal:
///
///  1. save the cursor and CR to column 0 -- a known start column, so a single
///     cursor report is enough (`actual_width == reported_col - 0`);
///  2. print the ZWJ cluster;
///  3. read the cursor column, i.e. the cluster's rendered width;
///  4. erase the scratch (CR + erase-line) and restore the saved cursor. No
///     newline is ever printed, so the scratch never enters scrollback (inline)
///     or the pager frame (the probe runs before alt-screen entry).
///
/// Failure toward no behavior change: any IO error, timeout, or a nonsensical
/// 0-width reply yields [`ZwjShaping::Unknown`], which suppresses substitution.
pub(crate) fn probe_shaping(io: &mut impl CursorProbe) -> ZwjShaping {
    // DECSC (save cursor), then CR to a known column 0.
    if io.write_bytes(b"\x1b7\r").is_err() {
        return ZwjShaping::Unknown;
    }
    if io.write_bytes(PROBE_CLUSTER.as_bytes()).is_err() {
        // Best-effort cleanup even on a failed write: erase line + DECRC.
        let _ = io.write_bytes(b"\r\x1b[2K\x1b8");
        return ZwjShaping::Unknown;
    }
    let verdict = match io.cursor_col() {
        Ok(col) => classify(col),
        Err(_) => ZwjShaping::Unknown,
    };
    // Erase the scratch glyph (CR + erase entire line) and restore the cursor.
    let _ = io.write_bytes(b"\r\x1b[2K\x1b8");
    verdict
}

/// Classify a rendered cursor column. The cluster is printed at column 0, so the
/// reported column equals its rendered width.
fn classify(rendered_col: u16) -> ZwjShaping {
    match rendered_col {
        MODELED_WIDTH => ZwjShaping::Shaped,
        // A 0 reply is nonsensical (nothing advanced): treat as unreadable.
        0 => ZwjShaping::Unknown,
        actual => ZwjShaping::Unshaped { actual },
    }
}

/// Rich-TTY startup runner: probe the real terminal and record the verdict in
/// [`crate::ui::textengine`]. Called once from
/// [`crate::ui::tui::TuiUi::new`](crate::ui::tui) after raw mode is enabled and
/// before the first frame / alt-screen entry. Never called on the
/// `--plain`/non-TTY path (that entry point is not reached there).
pub(crate) fn run_startup_probe() {
    let verdict = if under_multiplexer() {
        // Under tmux/Zellij a DSR reply is unreliable for width measurement:
        // the multiplexer may answer with its own logical cursor column (it
        // models the cluster as 2 regardless of the outer font), or pass
        // through an absolute outer-screen column that is not pane-relative.
        // Both directions produce false verdicts, so stay honest: Unknown, no
        // substitution.
        ZwjShaping::Unknown
    } else {
        probe_shaping(&mut StdoutProbe)
    };
    crate::ui::textengine::set_zwj_shaping(verdict);
}

/// Whether a terminal multiplexer owns the pane (same env signals the
/// capability snapshot in [`crate::ui::terminal_env`] reads).
fn under_multiplexer() -> bool {
    std::env::var_os("TMUX").is_some() || std::env::var_os("ZELLIJ").is_some()
}

/// Production [`CursorProbe`]: writes to stdout and reads the cursor via
/// `crossterm::cursor::position`, which performs the DSR round-trip through
/// crossterm's internal event source.
///
/// Trade-off: under raw mode `position()` uses crossterm's fixed ~2s internal
/// timeout rather than the ~250ms budget the issue suggests. On the
/// near-universal terminals that answer DSR this returns immediately; only a
/// total non-responder (no DSR support at all) waits, once, at startup, before
/// the probe fails to [`ZwjShaping::Unknown`]. Reusing crossterm is worth that
/// bound: a hand-rolled stdin read would fight crossterm for the tty fd.
struct StdoutProbe;

impl CursorProbe for StdoutProbe {
    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut out = io::stdout();
        out.write_all(bytes)?;
        out.flush()
    }

    fn cursor_col(&mut self) -> io::Result<u16> {
        ratatui::crossterm::cursor::position().map(|(col, _row)| col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted probe: records every byte written and returns one canned cursor
    /// reply, so the sequence logic is exercised without a TTY.
    struct ScriptedProbe {
        writes: Vec<u8>,
        reply: Option<io::Result<u16>>,
    }

    impl ScriptedProbe {
        fn new(reply: io::Result<u16>) -> Self {
            Self {
                writes: Vec::new(),
                reply: Some(reply),
            }
        }

        fn written(&self) -> String {
            String::from_utf8_lossy(&self.writes).into_owned()
        }
    }

    impl CursorProbe for ScriptedProbe {
        fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.writes.extend_from_slice(bytes);
            Ok(())
        }

        fn cursor_col(&mut self) -> io::Result<u16> {
            self.reply
                .take()
                .expect("cursor_col queried more than once")
        }
    }

    #[test]
    fn modeled_width_reply_is_shaped_and_scratch_is_written_then_erased() {
        let mut probe = ScriptedProbe::new(Ok(MODELED_WIDTH));
        let verdict = probe_shaping(&mut probe);
        assert_eq!(verdict, ZwjShaping::Shaped);
        let written = probe.written();
        // The probe printed the cluster and then erased the scratch line.
        assert!(
            written.contains(PROBE_CLUSTER),
            "cluster not printed: {written:?}"
        );
        assert!(
            written.contains("\x1b[2K"),
            "scratch not erased: {written:?}"
        );
        assert!(
            written.contains("\x1b8"),
            "cursor not restored: {written:?}"
        );
    }

    #[test]
    fn wider_reply_is_unshaped_with_measured_width() {
        let mut probe = ScriptedProbe::new(Ok(6));
        assert_eq!(
            probe_shaping(&mut probe),
            ZwjShaping::Unshaped { actual: 6 }
        );
    }

    #[test]
    fn zero_width_reply_is_unknown() {
        // A 0-column reply means nothing advanced -- garbage, not a real width.
        let mut probe = ScriptedProbe::new(Ok(0));
        assert_eq!(probe_shaping(&mut probe), ZwjShaping::Unknown);
    }

    #[test]
    fn read_error_is_unknown_and_still_erases_scratch() {
        // Timeout / unreadable reply fails toward no behavior change, and the
        // scratch glyph is still cleaned up.
        let mut probe = ScriptedProbe::new(Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "no cursor report",
        )));
        assert_eq!(probe_shaping(&mut probe), ZwjShaping::Unknown);
        assert!(probe.written().contains("\x1b[2K"));
    }

    #[test]
    fn classify_maps_widths_to_verdicts() {
        assert_eq!(classify(2), ZwjShaping::Shaped);
        assert_eq!(classify(0), ZwjShaping::Unknown);
        assert_eq!(classify(6), ZwjShaping::Unshaped { actual: 6 });
        assert_eq!(classify(4), ZwjShaping::Unshaped { actual: 4 });
    }
}
