//! Iris-owned terminal surface renderer for the TUI.
//!
//! This module owns the terminal document diff/replay state that Ratatui's
//! `Terminal` previously hid behind an inline viewport. Ratatui still supplies
//! `Line`/`Span`/`Style` primitives to the UI, but Iris decides when to append,
//! patch, or fully replay the terminal surface.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ratatui::layout::Size;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr;

const BEGIN_SYNC: &str = "\x1b[?2026h";
const END_SYNC: &str = "\x1b[?2026l";
const DISABLE_AUTOWRAP: &str = "\x1b[?7l";
const ENABLE_AUTOWRAP: &str = "\x1b[?7h";
const CLEAR_TO_SCREEN_END: &str = "\x1b[J";
/// Erase the entire display and home the cursor. Emitted only on a true resize
/// redraw (see [`TerminalSurface::write_resize_redraw`]); never on first render
/// or append/diff updates.
const CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";
/// Erase the saved-lines (native scrollback) buffer. Appended to
/// [`CLEAR_SCREEN`] only when Iris rows have scrolled into native scrollback
/// and the rebuild would otherwise duplicate them; a session that never
/// scrolled keeps the user's pre-Iris pane history intact across resizes.
const CLEAR_SCROLLBACK: &str = "\x1b[3J";
const SHOW_CURSOR: &str = "\x1b[?25h";
const HIDE_CURSOR: &str = "\x1b[?25l";

/// Zero-width internal marker a focused composer/editor emits at its cursor
/// position. It is an APC (Application Program Command) sequence terminals
/// ignore, but Iris never writes it to the terminal: [`render_line`] detects a
/// span whose content is exactly this marker, records the cursor column, and
/// strips it from the rendered output. The marker only ever travels as a
/// structured ratatui span, so width accounting and the diff source of truth
/// stay free of escape noise. Mirrors pi-mono's `CURSOR_MARKER` contract.
pub(crate) const CURSOR_MARKER: &str = "\x1b_iris:c\x07";

/// Environment override for the over-width crash/debug log directory. Defaults
/// to the Iris data dir (`~/.iris`). Used to keep the diagnostic under the Iris
/// data convention rather than `.pi`, and to redirect it in tests.
const CRASH_LOG_DIR_ENV: &str = "IRIS_CRASH_LOG_DIR";
const CRASH_LOG_FILE: &str = "tui-crash.log";

/// Document-row and display-column of the focused-editor cursor marker.
type CursorPos = (usize, usize);

/// One rendered terminal line (ANSI styling included). Reference-counted so the
/// per-frame stable-prefix reuse in [`render_lines`] is a pointer copy instead
/// of an O(line length) `String` clone: on a long retained transcript the
/// unchanged prefix dominates every frame, and cloning it used to be the single
/// largest per-frame cost. Shared lines also let the diff short-circuit on
/// pointer equality before falling back to content comparison.
pub(crate) type SurfaceLine = Rc<str>;

/// Equality with a pointer fast path: prefix lines reused from the previous
/// frame are the same allocation, so the common case never touches the bytes.
fn line_eq(a: &SurfaceLine, b: &SurfaceLine) -> bool {
    Rc::ptr_eq(a, b) || a == b
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderKind {
    First,
    FullRedraw,
    Append,
    Diff,
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderStats {
    pub(crate) kind: RenderKind,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct RenderState {
    /// Previous rendered terminal lines, including ANSI styling but excluding
    /// row separators. This is the diff source of truth.
    pub(crate) previous_lines: Vec<SurfaceLine>,
    pub(crate) previous_width: u16,
    pub(crate) previous_height: u16,
    pub(crate) previous_viewport_top: usize,
    /// Logical row where the terminal cursor is expected to be after Iris's last
    /// write. It may be outside the visible viewport when content has scrolled.
    pub(crate) hardware_cursor_row: usize,
    pub(crate) previous_volatile_tail: usize,
    pub(crate) first_render: bool,
    /// Sticky: set once the document has ever scrolled past the viewport, i.e.
    /// Iris rows have entered native scrollback. Until then a resize redraw can
    /// repaint in place instead of clearing scrollback, preserving whatever the
    /// user had in the terminal (shell history, other program output) before
    /// Iris started -- the common case in a freshly split tmux pane.
    pub(crate) scrolled: bool,
}

pub(crate) struct TerminalSurface<W> {
    writer: W,
    state: RenderState,
    /// When set, a height-only resize does not trigger a scrollback-clearing
    /// full redraw (Termux toggles terminal height when the soft keyboard shows
    /// or hides; a full redraw on every toggle would replay the whole history).
    termux: bool,
    /// When true the hardware cursor is shown at the IME marker position. Default
    /// off: Iris draws its own reversed block cursor, so the hardware cursor is
    /// only *positioned* (for IME candidate windows) and kept hidden. Mirrors
    /// pi-mono's `showHardwareCursor` (opt-in).
    show_hardware_cursor: bool,
    /// Tracks whether the hardware cursor is currently shown, so we never emit a
    /// redundant hide/show on renders that do not change cursor visibility.
    cursor_visible: bool,
    /// Directory for the over-width crash/debug log. `None` disables logging
    /// (best-effort) when no data dir can be resolved.
    crash_log_dir: Option<PathBuf>,
}

impl<W: Write> TerminalSurface<W> {
    pub(crate) fn new(writer: W) -> Self {
        Self {
            writer,
            state: RenderState {
                first_render: true,
                ..RenderState::default()
            },
            termux: is_termux_env(),
            show_hardware_cursor: false,
            cursor_visible: false,
            crash_log_dir: resolve_crash_log_dir(),
        }
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> &RenderState {
        &self.state
    }

    #[cfg(test)]
    pub(crate) fn set_termux(&mut self, termux: bool) {
        self.termux = termux;
    }

    #[cfg(test)]
    pub(crate) fn set_show_hardware_cursor(&mut self, show: bool) {
        self.show_hardware_cursor = show;
    }

    #[cfg(test)]
    pub(crate) fn set_crash_log_dir(&mut self, dir: Option<PathBuf>) {
        self.crash_log_dir = dir;
    }

    #[cfg(test)]
    pub(crate) fn writer_ref(&self) -> &W {
        &self.writer
    }

    pub(crate) fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    pub(crate) fn render(
        &mut self,
        size: Size,
        lines: &[Line<'static>],
    ) -> io::Result<RenderStats> {
        self.render_with_volatile_tail(size, lines, 0)
    }

    pub(crate) fn render_with_volatile_tail(
        &mut self,
        size: Size,
        lines: &[Line<'static>],
        volatile_tail: usize,
    ) -> io::Result<RenderStats> {
        self.render_with_hints(size, lines, volatile_tail, 0)
    }

    pub(crate) fn render_with_hints(
        &mut self,
        size: Size,
        lines: &[Line<'static>],
        volatile_tail: usize,
        stable_prefix: usize,
    ) -> io::Result<RenderStats> {
        let width = size.width.max(1);
        let height = size.height.max(1);
        let reusable_prefix = if !self.state.first_render && self.state.previous_width == width {
            stable_prefix.min(self.state.previous_lines.len())
        } else {
            0
        };
        let (new_lines, cursor) = render_lines(
            lines,
            width,
            self.crash_log_dir.as_deref(),
            &self.state.previous_lines,
            reusable_prefix,
        )?;
        let volatile_tail = volatile_tail.min(new_lines.len());

        let width_changed = self.state.previous_width != 0 && self.state.previous_width != width;
        let height_changed =
            self.state.previous_height != 0 && self.state.previous_height != height;

        // A true resize redraw clears native scrollback and rebuilds the whole
        // surface from Iris state. A height-only change under Termux is excluded:
        // its soft keyboard toggles height constantly, and clearing scrollback on
        // every toggle would churn the entire history. Width changes always need
        // a full redraw because wrapping changes.
        let resize_redraw = width_changed || (height_changed && !self.termux);

        let kind = if self.state.first_render && !width_changed && !height_changed {
            self.write_full(new_lines, width, height, false, volatile_tail)?;
            RenderKind::First
        } else if resize_redraw {
            if self.resize_can_repaint_in_place(&new_lines, width, height) {
                self.write_full(new_lines, width, height, true, volatile_tail)?;
            } else {
                // Until the document has scrolled, nothing of Iris's is in
                // native scrollback, so the rebuild only needs to erase the
                // screen; wiping scrollback too would destroy the user's
                // pre-Iris pane history (tmux resizes on every split/drag).
                let clear_scrollback = self.state.scrolled;
                self.write_resize_redraw(
                    new_lines,
                    width,
                    height,
                    volatile_tail,
                    clear_scrollback,
                )?;
            }
            RenderKind::FullRedraw
        } else {
            self.write_diff_or_replay(new_lines, width, height, volatile_tail, reusable_prefix)?
        };

        // Position the hardware cursor for IME on every render, including
        // `Unchanged`: when only the cursor moves within an otherwise-identical
        // row, the stripped line strings do not change, so cursor repositioning
        // must be independent of the changed-range diff.
        self.position_hardware_cursor(cursor)?;

        Ok(RenderStats { kind })
    }

    pub(crate) fn finish(&mut self) -> io::Result<()> {
        // Defensive cleanup for an interrupted prior render: make sure terminal
        // modes Iris toggles during drawing are restored even on the normal
        // finish path.
        write!(self.writer, "{ENABLE_AUTOWRAP}{END_SYNC}")?;
        if !self.state.previous_lines.is_empty() {
            let target = self.state.previous_lines.len().saturating_sub(1);
            let mut buffer = String::new();
            move_to_row(
                &mut buffer,
                &mut self.state.hardware_cursor_row,
                self.state.previous_viewport_top,
                target,
            );
            buffer.push_str("\r\n");
            write!(self.writer, "{buffer}")?;
            self.state.hardware_cursor_row = target.saturating_add(1);
        }
        self.writer.flush()
    }

    fn write_full(
        &mut self,
        lines: Vec<SurfaceLine>,
        width: u16,
        height: u16,
        clear: bool,
        volatile_tail: usize,
    ) -> io::Result<()> {
        let mut buffer = String::from(BEGIN_SYNC);
        buffer.push_str(DISABLE_AUTOWRAP);
        // On a clearing redraw (resize/shrink) repaint only the visible viewport
        // slice. Moving to the previous viewport top and rewriting the whole
        // document from logical line 0 would scroll long documents, duplicating
        // the history that already lives above the viewport. We clear from the
        // viewport top downward (never the rows above it, i.e. the user's
        // scrollback) and repaint the slice that fits on screen.
        let start = if clear {
            let clear_top = viewport_top(self.state.previous_lines.len(), height);
            self.move_to_viewport_top(&mut buffer, clear_top);
            buffer.push_str(CLEAR_TO_SCREEN_END);
            viewport_top(lines.len(), height)
        } else {
            0
        };
        for (offset, line) in lines[start..].iter().enumerate() {
            if offset > 0 {
                buffer.push_str("\r\n");
            }
            buffer.push_str(line);
        }
        buffer.push_str(ENABLE_AUTOWRAP);
        buffer.push_str(END_SYNC);
        write!(self.writer, "{buffer}")?;
        self.writer.flush()?;
        let hardware_cursor_row = lines.len().saturating_sub(1);
        self.remember(lines, width, height, hardware_cursor_row, volatile_tail);
        Ok(())
    }

    /// True resize redraw: clear the screen (from an absolute home, so terminal
    /// reflow of on-screen rows cannot skew relative positioning) and rebuild
    /// the entire surface from Iris state starting at logical line 0. Rows past
    /// the viewport scroll off the top into native scrollback as they are
    /// written. `clear_scrollback` additionally wipes native scrollback first:
    /// required once Iris rows have scrolled into it (rewriting every line lets
    /// the history re-enter the freshly-cleared scrollback instead of
    /// duplicating it), and deliberately withheld before then so a resize never
    /// destroys the user's pre-Iris pane history. First render and append/diff
    /// updates never reach this path, so normal scrollback is preserved.
    fn write_resize_redraw(
        &mut self,
        lines: Vec<SurfaceLine>,
        width: u16,
        height: u16,
        volatile_tail: usize,
        clear_scrollback: bool,
    ) -> io::Result<()> {
        let mut buffer = String::from(BEGIN_SYNC);
        buffer.push_str(DISABLE_AUTOWRAP);
        buffer.push_str(CLEAR_SCREEN);
        if clear_scrollback {
            buffer.push_str(CLEAR_SCROLLBACK);
        }
        for (offset, line) in lines.iter().enumerate() {
            if offset > 0 {
                buffer.push_str("\r\n");
            }
            buffer.push_str(line);
        }
        buffer.push_str(ENABLE_AUTOWRAP);
        buffer.push_str(END_SYNC);
        write!(self.writer, "{buffer}")?;
        self.writer.flush()?;
        let hardware_cursor_row = lines.len().saturating_sub(1);
        self.remember(lines, width, height, hardware_cursor_row, volatile_tail);
        Ok(())
    }

    /// Whether a resize redraw can repaint in place ([`Self::write_full`] with
    /// `clear`) instead of homing to the screen origin and rebuilding
    /// ([`Self::write_resize_redraw`]). In-place is the gentlest path: it keeps
    /// native scrollback AND whatever occupied the screen above the document
    /// (the shell prompt Iris was started under). It is only trustworthy when:
    ///
    /// - the document never scrolled (sticky [`RenderState::scrolled`]) and
    ///   still fits the new viewport, so the in-place repaint covers all of it;
    /// - on narrowing, no previously written row is wider than the new width.
    ///   Terminals and multiplexers that reflow on resize (tmux, kitty) rewrap
    ///   such rows, which shifts the cursor-relative position of the document
    ///   top that the in-place repaint navigates by.
    fn resize_can_repaint_in_place(
        &self,
        new_lines: &[SurfaceLine],
        width: u16,
        height: u16,
    ) -> bool {
        if self.state.scrolled
            || self.state.previous_viewport_top != 0
            || viewport_top(new_lines.len(), height) != 0
        {
            return false;
        }
        if width >= self.state.previous_width {
            return true;
        }
        let max = usize::from(width.max(1));
        self.state
            .previous_lines
            .iter()
            .all(|line| UnicodeWidthStr::width(strip_ansi_for_log(line).as_str()) <= max)
    }

    fn move_to_viewport_top(&mut self, buffer: &mut String, viewport_top: usize) {
        let current_screen_row = self.state.hardware_cursor_row.saturating_sub(viewport_top);
        if current_screen_row > 0 {
            buffer.push_str(&format!("\x1b[{current_screen_row}A"));
        }
        buffer.push('\r');
        self.state.hardware_cursor_row = viewport_top;
    }

    fn write_diff_or_replay(
        &mut self,
        lines: Vec<SurfaceLine>,
        width: u16,
        height: u16,
        volatile_tail: usize,
        stable_prefix: usize,
    ) -> io::Result<RenderKind> {
        // A height-only change can reach here without a resize redraw (Termux).
        // Recompute the effective previous viewport top for the new height before
        // any movement/"above viewport" math, otherwise a shrink could patch rows
        // that are no longer visible and corrupt the viewport. Mirrors pi-mono's
        // `prevViewportTop = heightChanged ? max(0, prevBufferLen - height) : ...`.
        if self.state.previous_height != 0 && self.state.previous_height != height {
            let prev_buffer_len =
                self.state.previous_viewport_top + usize::from(self.state.previous_height.max(1));
            self.state.previous_viewport_top =
                prev_buffer_len.saturating_sub(usize::from(height.max(1)));
        }
        let previous_len = self.state.previous_lines.len();
        let new_len = lines.len();
        let previous_volatile_tail = self.state.previous_volatile_tail.min(previous_len);
        let previous_stable_len = previous_len.saturating_sub(previous_volatile_tail);
        let new_stable_len = new_len.saturating_sub(volatile_tail.min(new_len));
        let stable_prefix = stable_prefix
            .min(previous_stable_len)
            .min(new_stable_len)
            .min(previous_len)
            .min(new_len);

        let Some((first_changed, last_changed)) =
            changed_range_from(&self.state.previous_lines, &lines, stable_prefix)
        else {
            self.state.previous_width = width;
            self.state.previous_height = height;
            self.state.previous_viewport_top = viewport_top(new_len, height);
            self.state.previous_volatile_tail = volatile_tail;
            if self.state.previous_viewport_top > 0 {
                self.state.scrolled = true;
            }
            return Ok(RenderKind::Unchanged);
        };

        let stable_append = if new_stable_len > previous_stable_len {
            previous_stable_len <= stable_prefix
                || self.state.previous_lines[..previous_stable_len]
                    .iter()
                    .zip(&lines[..previous_stable_len])
                    .all(|(a, b)| line_eq(a, b))
        } else {
            false
        };
        if stable_append && previous_volatile_tail > 0 {
            if previous_stable_len < self.state.previous_viewport_top {
                self.write_full(lines.to_vec(), width, height, true, volatile_tail)?;
                return Ok(RenderKind::FullRedraw);
            }
            self.write_append_replacing_volatile(
                lines,
                previous_stable_len,
                width,
                height,
                volatile_tail,
            )?;
            return Ok(RenderKind::Append);
        }

        let append_only =
            new_len > previous_len && first_changed == previous_len && previous_len > 0;
        if append_only {
            self.write_append(lines, previous_len, width, height, volatile_tail)?;
            return Ok(RenderKind::Append);
        }

        // Non-append length changes (for example opening slash/settings chrome
        // above the editor) must not be patched line-by-line: the changed range
        // can extend below the visible terminal footprint, and writing it with
        // CRLF would make the terminal scroll and copy the visible viewport into
        // native scrollback. Repaint the visible slice in place instead.
        // Changes above the previous viewport need the same coherent replay to
        // avoid stale rows after resize/history reflow.
        if new_len != previous_len || first_changed < self.state.previous_viewport_top {
            let new_viewport_top = viewport_top(new_len, height);
            if new_viewport_top > self.state.previous_viewport_top {
                self.write_scrolling_replay(lines, width, height, new_viewport_top, volatile_tail)?;
            } else {
                self.write_full(lines, width, height, true, volatile_tail)?;
            }
            return Ok(RenderKind::FullRedraw);
        }

        self.write_visible_diff(
            lines,
            width,
            height,
            first_changed,
            last_changed,
            volatile_tail,
        )?;
        Ok(RenderKind::Diff)
    }

    fn write_scrolling_replay(
        &mut self,
        lines: Vec<SurfaceLine>,
        width: u16,
        height: u16,
        new_viewport_top: usize,
        volatile_tail: usize,
    ) -> io::Result<()> {
        let mut buffer = String::from(BEGIN_SYNC);
        buffer.push_str(DISABLE_AUTOWRAP);
        move_to_row(
            &mut buffer,
            &mut self.state.hardware_cursor_row,
            self.state.previous_viewport_top,
            self.state.previous_viewport_top,
        );
        buffer.push('\r');
        for (offset, line) in lines[self.state.previous_viewport_top..].iter().enumerate() {
            if offset > 0 {
                buffer.push_str("\r\n");
                self.state.hardware_cursor_row += 1;
            }
            buffer.push_str("\x1b[2K");
            buffer.push_str(line);
        }
        buffer.push_str(ENABLE_AUTOWRAP);
        buffer.push_str(END_SYNC);
        write!(self.writer, "{buffer}")?;
        self.writer.flush()?;
        let hardware_cursor_row = lines.len().saturating_sub(1);
        self.remember(lines, width, height, hardware_cursor_row, volatile_tail);
        self.state.previous_viewport_top = new_viewport_top;
        Ok(())
    }

    fn write_append_replacing_volatile(
        &mut self,
        lines: Vec<SurfaceLine>,
        start: usize,
        width: u16,
        height: u16,
        volatile_tail: usize,
    ) -> io::Result<()> {
        let mut buffer = String::from(BEGIN_SYNC);
        buffer.push_str(DISABLE_AUTOWRAP);
        move_to_row(
            &mut buffer,
            &mut self.state.hardware_cursor_row,
            self.state.previous_viewport_top,
            start,
        );
        buffer.push('\r');
        buffer.push_str(CLEAR_TO_SCREEN_END);
        for (offset, line) in lines[start..].iter().enumerate() {
            if offset > 0 {
                buffer.push_str("\r\n");
            }
            buffer.push_str("\x1b[2K");
            buffer.push_str(line);
        }
        buffer.push_str(ENABLE_AUTOWRAP);
        buffer.push_str(END_SYNC);
        write!(self.writer, "{buffer}")?;
        self.writer.flush()?;
        let hardware_cursor_row = lines.len().saturating_sub(1);
        self.remember(lines, width, height, hardware_cursor_row, volatile_tail);
        Ok(())
    }

    fn write_append(
        &mut self,
        lines: Vec<SurfaceLine>,
        previous_len: usize,
        width: u16,
        height: u16,
        volatile_tail: usize,
    ) -> io::Result<()> {
        let mut buffer = String::from(BEGIN_SYNC);
        buffer.push_str(DISABLE_AUTOWRAP);
        let previous_last = self.state.previous_lines.len().saturating_sub(1);
        move_to_row(
            &mut buffer,
            &mut self.state.hardware_cursor_row,
            self.state.previous_viewport_top,
            previous_last,
        );
        for line in &lines[previous_len..] {
            buffer.push_str("\r\n");
            buffer.push_str(line);
            self.state.hardware_cursor_row += 1;
        }
        buffer.push_str(ENABLE_AUTOWRAP);
        buffer.push_str(END_SYNC);
        write!(self.writer, "{buffer}")?;
        self.writer.flush()?;

        let hardware_cursor_row = lines.len().saturating_sub(1);
        self.remember(lines, width, height, hardware_cursor_row, volatile_tail);
        Ok(())
    }

    fn write_visible_diff(
        &mut self,
        lines: Vec<SurfaceLine>,
        width: u16,
        height: u16,
        first_changed: usize,
        last_changed: usize,
        volatile_tail: usize,
    ) -> io::Result<()> {
        let mut buffer = String::from(BEGIN_SYNC);
        buffer.push_str(DISABLE_AUTOWRAP);
        move_to_row(
            &mut buffer,
            &mut self.state.hardware_cursor_row,
            self.state.previous_viewport_top,
            first_changed,
        );
        buffer.push('\r');
        for (offset, line) in lines[first_changed..=last_changed].iter().enumerate() {
            if offset > 0 {
                buffer.push_str("\r\n");
                self.state.hardware_cursor_row += 1;
            }
            buffer.push_str("\x1b[2K");
            buffer.push_str(line);
        }
        buffer.push_str(ENABLE_AUTOWRAP);
        buffer.push_str(END_SYNC);
        write!(self.writer, "{buffer}")?;
        self.writer.flush()?;
        self.remember(lines, width, height, last_changed, volatile_tail);
        Ok(())
    }

    /// Position the hardware cursor for IME candidate-window placement after a
    /// render. When a focused composer/editor emitted a [`CURSOR_MARKER`], move
    /// the hardware cursor to its (row, col); otherwise ensure it is hidden. By
    /// default the cursor is only *positioned* and kept hidden (Iris draws its
    /// own reversed block cursor); `show_hardware_cursor` opts into showing it.
    fn position_hardware_cursor(&mut self, cursor: Option<CursorPos>) -> io::Result<()> {
        let total = self.state.previous_lines.len();
        match cursor {
            Some((row, col)) if total > 0 => {
                let target_row = row.min(total.saturating_sub(1));
                let mut buffer = String::new();
                let current = self.state.hardware_cursor_row;
                if target_row > current {
                    buffer.push_str(&format!("\x1b[{}B", target_row - current));
                } else if current > target_row {
                    buffer.push_str(&format!("\x1b[{}A", current - target_row));
                }
                // Absolute column (1-indexed); independent of the viewport.
                buffer.push_str(&format!("\x1b[{}G", col + 1));
                if self.show_hardware_cursor {
                    if !self.cursor_visible {
                        buffer.push_str(SHOW_CURSOR);
                        self.cursor_visible = true;
                    }
                } else if self.cursor_visible {
                    buffer.push_str(HIDE_CURSOR);
                    self.cursor_visible = false;
                }
                write!(self.writer, "{buffer}")?;
                self.writer.flush()?;
                self.state.hardware_cursor_row = target_row;
            }
            _ => {
                if self.cursor_visible {
                    write!(self.writer, "{HIDE_CURSOR}")?;
                    self.writer.flush()?;
                    self.cursor_visible = false;
                }
            }
        }
        Ok(())
    }

    fn remember(
        &mut self,
        lines: Vec<SurfaceLine>,
        width: u16,
        height: u16,
        hardware_cursor_row: usize,
        volatile_tail: usize,
    ) {
        self.state.previous_lines = lines;
        self.remember_metadata(width, height, hardware_cursor_row, volatile_tail);
    }

    fn remember_metadata(
        &mut self,
        width: u16,
        height: u16,
        hardware_cursor_row: usize,
        volatile_tail: usize,
    ) {
        self.state.previous_width = width;
        self.state.previous_height = height;
        self.state.previous_viewport_top = viewport_top(self.state.previous_lines.len(), height);
        self.state.hardware_cursor_row = hardware_cursor_row;
        self.state.previous_volatile_tail = volatile_tail.min(self.state.previous_lines.len());
        self.state.first_render = false;
        if self.state.previous_viewport_top > 0 {
            self.state.scrolled = true;
        }
    }
}

fn changed_range_from(
    previous: &[SurfaceLine],
    next: &[SurfaceLine],
    start: usize,
) -> Option<(usize, usize)> {
    let differs = |i: usize| match (previous.get(i), next.get(i)) {
        (Some(a), Some(b)) => !line_eq(a, b),
        (None, None) => false,
        _ => true,
    };
    let max_len = previous.len().max(next.len());
    let first = (start.min(max_len)..max_len).find(|&i| differs(i))?;
    let last = (first..max_len)
        .rev()
        .find(|&i| differs(i))
        .unwrap_or(first);
    Some((first, last))
}

fn viewport_top(line_count: usize, height: u16) -> usize {
    let height = usize::from(height.max(1));
    line_count.saturating_sub(height)
}

fn move_to_row(
    buffer: &mut String,
    hardware_cursor_row: &mut usize,
    viewport_top: usize,
    target: usize,
) {
    let current_screen_row = hardware_cursor_row.saturating_sub(viewport_top);
    let target_screen_row = target.saturating_sub(viewport_top);
    if target_screen_row > current_screen_row {
        buffer.push_str(&format!("\x1b[{}B", target_screen_row - current_screen_row));
    } else if current_screen_row > target_screen_row {
        buffer.push_str(&format!("\x1b[{}A", current_screen_row - target_screen_row));
    }
    *hardware_cursor_row = target;
}

/// Render every logical line to an ANSI string and locate the focused-editor
/// cursor marker, if present. Returns the rendered lines and the cursor
/// `(row, col)` (column = display width of the text before the marker).
///
/// Over-width safety: higher-level layout (`screen.rs`) is responsible for
/// wrapping content to the terminal width. If a rendered line still exceeds the
/// width, that is a renderer bug. Rather than silently clipping (which hides the
/// bug and can still corrupt output at wide-char/ANSI boundaries), we write a
/// diagnostic crash log and fail before any corrupting write reaches the
/// terminal. Mirrors pi-mono's fail-fast over-width guard.
fn render_lines(
    lines: &[Line<'static>],
    width: u16,
    crash_log_dir: Option<&Path>,
    previous_lines: &[SurfaceLine],
    stable_prefix: usize,
) -> io::Result<(Vec<SurfaceLine>, Option<CursorPos>)> {
    let max = usize::from(width.max(1));
    let stable_prefix = stable_prefix.min(previous_lines.len());
    let mut out = Vec::with_capacity(stable_prefix + lines.len());
    // Reference-counted reuse: the stable prefix is shared with the previous
    // frame (pointer copies), so per-frame cost is proportional to the changed
    // suffix, not the whole retained document.
    out.extend(previous_lines[..stable_prefix].iter().cloned());
    let mut cursor: Option<(usize, usize)> = None;
    for (offset, line) in lines.iter().enumerate() {
        let row = stable_prefix + offset;
        let rendered = render_line(line, max);
        if let Some(col) = rendered.cursor_col {
            cursor = Some((row, col));
        }
        if rendered.width > max {
            let rendered_for_log =
                render_lines_for_log(&previous_lines[..stable_prefix], lines, max);
            write_overwidth_crash_log(crash_log_dir, max, &rendered_for_log, row, rendered.width);
            let log_hint = crash_log_dir
                .map(|dir| {
                    format!(
                        "; diagnostic log written to {}",
                        dir.join(CRASH_LOG_FILE).display()
                    )
                })
                .unwrap_or_default();
            return Err(io::Error::other(format!(
                "rendered line {row} exceeds terminal width ({} > {max}); this is a renderer \
                 bug (a line was not truncated to the terminal width before reaching the \
                 terminal surface){log_hint}",
                rendered.width
            )));
        }
        out.push(SurfaceLine::from(rendered.text));
    }
    Ok((out, cursor))
}

fn render_lines_for_log(
    stable_prefix: &[SurfaceLine],
    lines: &[Line<'static>],
    max: usize,
) -> Vec<String> {
    let mut rendered = Vec::with_capacity(stable_prefix.len() + lines.len());
    rendered.extend(stable_prefix.iter().map(|line| line.to_string()));
    rendered.extend(lines.iter().map(|line| render_line(line, max).text));
    rendered
}

struct RenderedLine {
    text: String,
    /// Total display width of the visible content (markers excluded).
    width: usize,
    /// Display-width column of the cursor marker on this line, if it carried one.
    cursor_col: Option<usize>,
}

fn render_line(line: &Line<'static>, _max_width: usize) -> RenderedLine {
    // Autowrap is disabled while we write. We do NOT clip here: over-width lines
    // are caught and reported by `render_lines` instead of being silently hidden.
    // We accumulate the visible display width so the caller can detect overflow,
    // and strip any zero-width cursor marker (recording its column) so it never
    // reaches the terminal or the diff source of truth.
    let mut out = String::new();
    let mut used = 0usize;
    let mut cursor_col: Option<usize> = None;
    for span in &line.spans {
        if span.content.as_ref() == CURSOR_MARKER {
            cursor_col = Some(used);
            continue;
        }
        let style = line.style.patch(span.style);
        out.push_str("\x1b[0m");
        out.push_str(&style_sgr(style));
        out.push_str(span.content.as_ref());
        used += UnicodeWidthStr::width(span.content.as_ref());
    }
    out.push_str("\x1b[0m");
    RenderedLine {
        text: out,
        width: used,
        cursor_col,
    }
}

/// Resolve the over-width crash/debug log directory: `IRIS_CRASH_LOG_DIR`
/// override, else the Iris data dir (`~/.iris`). Returns `None` when neither is
/// resolvable, which disables (best-effort) logging.
fn resolve_crash_log_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(CRASH_LOG_DIR_ENV)
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(Path::new(&home).join(".iris"))
}

/// Best-effort write of the over-width diagnostic. Failure to log never masks
/// the original over-width error: the caller fails regardless.
fn write_overwidth_crash_log(
    dir: Option<&Path>,
    width: usize,
    rendered: &[String],
    bad_index: usize,
    bad_width: usize,
) {
    let Some(dir) = dir else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut body = String::new();
    body.push_str(&format!(
        "Over-width render at line {bad_index} (visible width {bad_width} > terminal width \
         {width}).\nThis is likely a TUI component not truncating its output to the terminal \
         width.\n\n=== All rendered lines (ANSI-stripped) ===\n"
    ));
    for (idx, line) in rendered.iter().enumerate() {
        let plain = strip_ansi_for_log(line);
        let line_width = UnicodeWidthStr::width(plain.as_str());
        body.push_str(&format!("[{idx}] (w={line_width}) {plain}\n"));
    }
    let _ = std::fs::write(dir.join(CRASH_LOG_FILE), body);
}

fn strip_ansi_for_log(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Skip the rest of the escape: APC strings end with BEL/ST, CSI/SGR
            // end with an alphabetic byte. Drop until a terminator.
            for next in chars.by_ref() {
                if next == '\x07' || next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn is_termux_env() -> bool {
    std::env::var_os("TERMUX_VERSION").is_some_and(|value| !value.is_empty())
}

fn style_sgr(style: Style) -> String {
    let mut codes: Vec<String> = Vec::new();
    if let Some(fg) = style.fg {
        codes.push(color_code(fg, false));
    }
    if let Some(bg) = style.bg {
        codes.push(color_code(bg, true));
    }
    for (modifier, code) in [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::SLOW_BLINK, "5"),
        (Modifier::RAPID_BLINK, "6"),
        (Modifier::REVERSED, "7"),
        (Modifier::HIDDEN, "8"),
        (Modifier::CROSSED_OUT, "9"),
    ] {
        if style.add_modifier.intersects(modifier) && !style.sub_modifier.intersects(modifier) {
            codes.push(code.to_string());
        }
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

fn color_code(color: Color, background: bool) -> String {
    let base = if background { 10 } else { 0 };
    match color {
        Color::Reset => {
            if background {
                "49".to_string()
            } else {
                "39".to_string()
            }
        }
        Color::Black => (30 + base).to_string(),
        Color::Red => (31 + base).to_string(),
        Color::Green => (32 + base).to_string(),
        Color::Yellow => (33 + base).to_string(),
        Color::Blue => (34 + base).to_string(),
        Color::Magenta => (35 + base).to_string(),
        Color::Cyan => (36 + base).to_string(),
        Color::Gray => (37 + base).to_string(),
        Color::DarkGray => (90 + base).to_string(),
        Color::LightRed => (91 + base).to_string(),
        Color::LightGreen => (92 + base).to_string(),
        Color::LightYellow => (93 + base).to_string(),
        Color::LightBlue => (94 + base).to_string(),
        Color::LightMagenta => (95 + base).to_string(),
        Color::LightCyan => (96 + base).to_string(),
        Color::White => (97 + base).to_string(),
        Color::Rgb(r, g, b) => {
            if background {
                format!("48;2;{r};{g};{b}")
            } else {
                format!("38;2;{r};{g};{b}")
            }
        }
        Color::Indexed(index) => {
            if background {
                format!("48;5;{index}")
            } else {
                format!("38;5;{index}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::{Line, Span};

    fn size(width: u16, height: u16) -> Size {
        Size::new(width, height)
    }

    fn output(surface: &TerminalSurface<Vec<u8>>) -> String {
        String::from_utf8(surface.writer_ref().clone()).expect("utf8 output")
    }

    fn strip_ansi(input: &str) -> String {
        let mut out = String::new();
        let mut chars = input.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn first_render_writes_synchronized_document_without_clear() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let stats = surface.render(size(20, 5), &[Line::from("hello"), Line::from("world")])?;

        assert_eq!(stats.kind, RenderKind::First);
        let out = output(&surface);
        assert!(out.starts_with(BEGIN_SYNC));
        assert!(out.ends_with(END_SYNC));
        assert!(out.contains(DISABLE_AUTOWRAP));
        assert!(out.contains(ENABLE_AUTOWRAP));
        assert!(!out.contains(CLEAR_TO_SCREEN_END));
        assert!(strip_ansi(&out).contains("hello\r\nworld"));
        assert_eq!(surface.state().previous_width, 20);
        assert_eq!(surface.state().previous_height, 5);
        assert_eq!(surface.state().previous_viewport_top, 0);
        assert_eq!(surface.state().hardware_cursor_row, 1);
        assert!(!surface.state().first_render);
        Ok(())
    }

    #[test]
    fn normal_diff_update_rewrites_only_the_visible_changed_line() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.render(size(20, 5), &[Line::from("alpha"), Line::from("beta")])?;
        surface.writer_mut().clear();

        let stats = surface.render(size(20, 5), &[Line::from("alpha"), Line::from("gamma")])?;

        assert_eq!(stats.kind, RenderKind::Diff);
        let out = output(&surface);
        assert!(out.contains("\x1b[2K"), "{out:?}");
        assert!(strip_ansi(&out).contains("gamma"), "{out:?}");
        assert!(!out.contains(CLEAR_TO_SCREEN_END), "{out:?}");
        assert_eq!(surface.state().previous_lines.len(), 2);
        Ok(())
    }

    #[test]
    fn append_update_adds_new_lines_without_clearing_history() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.render(size(20, 3), &[Line::from("one")])?;
        surface.writer_mut().clear();

        let stats = surface.render(
            size(20, 3),
            &[Line::from("one"), Line::from("two"), Line::from("three")],
        )?;

        assert_eq!(stats.kind, RenderKind::Append);
        let out = output(&surface);
        assert!(strip_ansi(&out).contains("two\r\nthree"), "{out:?}");
        assert!(!out.contains(CLEAR_TO_SCREEN_END), "{out:?}");
        assert_eq!(surface.state().previous_viewport_top, 0);
        Ok(())
    }

    #[test]
    fn trailing_blank_line_addition_is_detected_as_append() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.render(size(20, 3), &[Line::from("a")])?;
        surface.writer_mut().clear();

        let stats = surface.render(size(20, 3), &[Line::from("a"), Line::from("")])?;

        assert_eq!(stats.kind, RenderKind::Append);
        assert_eq!(surface.state().previous_lines.len(), 2);
        Ok(())
    }

    #[test]
    fn trailing_blank_line_removal_is_detected_as_full_redraw() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.render(size(20, 3), &[Line::from("a"), Line::from("")])?;
        surface.writer_mut().clear();

        let stats = surface.render(size(20, 3), &[Line::from("a")])?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        assert_eq!(surface.state().previous_lines.len(), 1);
        Ok(())
    }

    #[test]
    fn append_replaces_volatile_tail_before_writing_new_history() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.render_with_volatile_tail(
            size(30, 4),
            &[
                Line::from("history one"),
                Line::from("old editor"),
                Line::from("old status"),
            ],
            2,
        )?;
        surface.writer_mut().clear();

        let stats = surface.render_with_volatile_tail(
            size(30, 4),
            &[
                Line::from("history one"),
                Line::from("history two"),
                Line::from("new editor"),
                Line::from("new status"),
            ],
            2,
        )?;

        let raw = output(&surface);
        let out = strip_ansi(&raw);
        assert_eq!(stats.kind, RenderKind::Append);
        assert!(raw.contains(CLEAR_TO_SCREEN_END), "{raw:?}");
        assert!(!out.contains("old editor"), "{out:?}");
        assert!(!out.contains("old status"), "{out:?}");
        assert!(out.contains("history two"), "{out:?}");
        assert!(out.contains("new editor"), "{out:?}");
        Ok(())
    }

    #[test]
    fn volatile_tail_taller_than_viewport_does_not_append_past_visible_rows() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let previous: Vec<Line<'static>> = std::iter::once(Line::from("stable 0"))
            .chain((1..=9).map(|n| Line::from(format!("old volatile {n}"))))
            .collect();
        surface.render_with_volatile_tail(size(30, 6), &previous, 9)?;
        assert_eq!(surface.state().previous_viewport_top, 4);
        surface.writer_mut().clear();

        let next: Vec<Line<'static>> = [Line::from("stable 0"), Line::from("stable 1")]
            .into_iter()
            .chain((1..=9).map(|n| Line::from(format!("new volatile {n}"))))
            .collect();
        surface.render_with_volatile_tail(size(30, 6), &next, 9)?;

        let plain = strip_ansi(&output(&surface));
        let content_rows: Vec<&str> = plain
            .lines()
            .filter(|line| line.contains("stable") || line.contains("volatile"))
            .collect();
        assert!(
            content_rows.len() <= 6,
            "wrote more rows than the viewport and would scroll: {plain:?}"
        );
        assert_eq!(surface.state().previous_viewport_top, 5);
        assert_eq!(
            surface.state().hardware_cursor_row,
            next.len().saturating_sub(1)
        );
        Ok(())
    }

    #[test]
    fn width_resize_forces_coherent_full_replay() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.render(size(20, 4), &[Line::from("abcdef")])?;
        surface.writer_mut().clear();

        let stats = surface.render(size(10, 4), &[Line::from("abc"), Line::from("def")])?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        let out = output(&surface);
        // The document never scrolled and every previous row fits the new
        // width, so the resize repaints in place: native scrollback (the
        // user's shell history before Iris started) survives the resize.
        assert!(!out.contains("\x1b[3J"), "{out:?}");
        assert!(out.contains(CLEAR_TO_SCREEN_END), "{out:?}");
        assert!(strip_ansi(&out).contains("abc\r\ndef"), "{out:?}");
        assert_eq!(surface.state().previous_width, 10);
        Ok(())
    }

    #[test]
    fn short_session_width_grow_preserves_scrollback() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.render(size(20, 6), &[Line::from("hello"), Line::from("hi")])?;
        surface.writer_mut().clear();

        let stats = surface.render(size(30, 6), &[Line::from("hello"), Line::from("hi")])?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        let out = output(&surface);
        assert!(!out.contains("\x1b[2J"), "{out:?}");
        assert!(!out.contains("\x1b[3J"), "{out:?}");
        assert!(strip_ansi(&out).contains("hello"), "{out:?}");
        assert!(!surface.state().scrolled);
        Ok(())
    }

    #[test]
    fn short_session_height_resize_preserves_scrollback() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let doc = [Line::from("a"), Line::from("b")];
        surface.render(size(20, 8), &doc)?;
        surface.writer_mut().clear();

        // Height-only resize with the document still fitting: repaint in place,
        // never clear native scrollback.
        let stats = surface.render(size(20, 5), &doc)?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        let out = output(&surface);
        assert!(!out.contains("\x1b[2J"), "{out:?}");
        assert!(!out.contains("\x1b[3J"), "{out:?}");
        Ok(())
    }

    #[test]
    fn width_shrink_below_previous_row_width_clears_screen_but_not_scrollback() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        // Row wider than the new width: a reflowing terminal (tmux, kitty)
        // rewraps it on shrink, so the in-place repaint cannot trust the
        // cursor-relative document top and must rebuild from the screen origin.
        // Nothing of Iris's is in native scrollback yet, so only the screen is
        // cleared: the user's pre-Iris pane history survives.
        surface.render(size(20, 6), &[Line::from("abcdefghijkl")])?;
        surface.writer_mut().clear();

        let stats = surface.render(size(10, 6), &[Line::from("abcdefghij"), Line::from("kl")])?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        let out = output(&surface);
        assert!(out.contains(CLEAR_SCREEN), "{out:?}");
        assert!(!out.contains(CLEAR_SCROLLBACK), "{out:?}");
        Ok(())
    }

    #[test]
    fn never_scrolled_resize_overflow_rebuilds_without_clearing_scrollback() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        // Fits the tall screen; after the height shrink it no longer fits, so
        // the in-place repaint is off the table. The rebuild homes to the
        // screen origin and lets the overflow scroll off naturally -- there are
        // no Iris rows in scrollback to duplicate, so scrollback is kept.
        let doc: Vec<Line<'static>> = (1..=8).map(|n| Line::from(format!("line {n}"))).collect();
        surface.render(size(20, 10), &doc)?;
        assert!(!surface.state().scrolled);
        surface.writer_mut().clear();

        let stats = surface.render(size(20, 5), &doc)?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        let out = output(&surface);
        assert!(out.contains(CLEAR_SCREEN), "{out:?}");
        assert!(!out.contains(CLEAR_SCROLLBACK), "{out:?}");
        // The whole document was rewritten and the surface now scrolls.
        assert!(strip_ansi(&out).contains("line 1"), "{out:?}");
        assert!(strip_ansi(&out).contains("line 8"), "{out:?}");
        assert!(surface.state().scrolled);
        Ok(())
    }

    #[test]
    fn resize_after_scrolled_history_always_clears_scrollback() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        // Scroll Iris rows into native scrollback...
        let long: Vec<Line<'static>> = (1..=8).map(|n| Line::from(format!("line {n}"))).collect();
        surface.render(size(20, 5), &long)?;
        assert!(surface.state().scrolled);
        // ...then shrink the document back to fitting. The flag is sticky:
        // scrollback still holds Iris rows a preserving repaint would leave
        // stale/duplicated, so a resize must keep taking the clearing rebuild.
        surface.render(size(20, 5), &[Line::from("short")])?;
        surface.writer_mut().clear();

        let stats = surface.render(size(15, 5), &[Line::from("short")])?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        assert!(output(&surface).contains(CLEAR_SCROLLBACK));
        Ok(())
    }

    #[test]
    fn non_append_growth_replays_from_previous_viewport_to_preserve_scrollback() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let initial = vec![
            Line::from("transcript 1"),
            Line::from("transcript 2"),
            Line::from("transcript 3"),
            Line::from("transcript 4"),
            Line::from("transcript 5"),
            Line::from("chrome 1"),
            Line::from("chrome 2"),
        ];
        surface.render(size(20, 5), &initial)?;
        surface.writer_mut().clear();

        let next = vec![
            Line::from("transcript 1"),
            Line::from("transcript 2"),
            Line::from("transcript 3"),
            Line::from("transcript 4"),
            Line::from("transcript 5"),
            Line::from("transcript 6"),
            Line::from("chrome 1"),
            Line::from("chrome 2"),
        ];
        let stats = surface.render(size(20, 5), &next)?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        let out = output(&surface);
        assert!(!out.contains(CLEAR_TO_SCREEN_END), "{out:?}");
        let plain = strip_ansi(&out);
        assert!(plain.contains("transcript 3"), "{plain:?}");
        assert!(plain.contains("transcript 6"), "{plain:?}");
        assert!(plain.contains("chrome 2"), "{plain:?}");
        assert!(!plain.contains("transcript 1"), "{plain:?}");
        assert_eq!(surface.state().previous_viewport_top, 3);
        Ok(())
    }

    #[test]
    fn scrolling_replay_clears_stale_cells_before_shorter_rows() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let grey = Style::default().bg(Color::Rgb(50, 50, 56));
        let initial = vec![
            Line::from("history 1"),
            Line::from("history 2"),
            Line::from("history 3"),
            Line::from("history 4"),
            Line::from(vec![Span::styled("old shaded editor row", grey)]),
            Line::from("old working details (17s tokens effort)"),
        ];
        surface.render(size(80, 4), &initial)?;
        surface.writer_mut().clear();

        let next = vec![
            Line::from("history 1"),
            Line::from("history 2"),
            Line::from("history 3"),
            Line::from("history 4"),
            Line::from("new explored"),
            Line::from("short"),
            Line::from("editor"),
        ];
        let stats = surface.render(size(80, 4), &next)?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        let out = output(&surface);
        assert!(out.matches("\x1b[2K").count() >= 5, "{out:?}");
        assert!(
            !strip_ansi(&out).contains("old shaded editor row"),
            "{out:?}"
        );
        assert!(!strip_ansi(&out).contains("working details"), "{out:?}");
        Ok(())
    }

    #[test]
    fn width_resize_rebuilds_full_document_and_clears_scrollback() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let doc: Vec<Line<'static>> = (1..=8).map(|n| Line::from(format!("line {n}"))).collect();
        surface.render(size(20, 5), &doc)?;
        // Document scrolled: the viewport top is below logical line 0.
        assert_eq!(surface.state().previous_viewport_top, 3);
        surface.writer_mut().clear();

        // Width change forces a scrollback-clearing full redraw that rebuilds the
        // entire document from Iris state, so history re-enters the freshly
        // cleared scrollback rather than being lost or duplicated.
        let stats = surface.render(size(18, 5), &doc)?;
        assert_eq!(stats.kind, RenderKind::FullRedraw);

        let raw = output(&surface);
        assert!(raw.contains(CLEAR_SCREEN), "{raw:?}");
        assert!(raw.contains(CLEAR_SCROLLBACK), "{raw:?}");
        let plain = strip_ansi(&raw);
        // Every line is rewritten, including the history above the viewport.
        assert!(plain.contains("line 1"), "{plain:?}");
        assert!(plain.contains("line 8"), "{plain:?}");
        // viewport_top tracks the last `height` lines of the rebuilt document.
        assert_eq!(surface.state().previous_viewport_top, 3);
        assert_eq!(surface.state().previous_width, 18);
        Ok(())
    }

    #[test]
    fn height_resize_clears_scrollback_and_rebuilds_full_document() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let doc: Vec<Line<'static>> = (1..=10).map(|n| Line::from(format!("line {n}"))).collect();
        surface.render(size(20, 4), &doc)?;
        assert_eq!(surface.state().previous_viewport_top, 6);
        surface.writer_mut().clear();

        // Non-Termux height change is a true resize: clear scrollback + rebuild.
        let stats = surface.render(size(20, 8), &doc)?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        let raw = output(&surface);
        assert!(raw.contains(CLEAR_SCREEN), "{raw:?}");
        assert!(raw.contains(CLEAR_SCROLLBACK), "{raw:?}");
        let plain = strip_ansi(&raw);
        let rows: Vec<&str> = plain.lines().map(|line| line.trim_matches('\r')).collect();
        assert!(rows.contains(&"line 1"), "{plain:?}");
        assert!(rows.contains(&"line 10"), "{plain:?}");
        assert_eq!(surface.state().previous_height, 8);
        assert_eq!(surface.state().previous_viewport_top, 2);
        Ok(())
    }

    #[test]
    fn termux_height_only_resize_does_not_clear_scrollback() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.set_termux(true);
        let doc = [Line::from("a"), Line::from("b"), Line::from("c")];
        surface.render(size(20, 8), &doc)?;
        surface.writer_mut().clear();

        // Height-only change under Termux (soft keyboard toggle): must NOT take
        // the scrollback-clearing resize path. Content unchanged -> Unchanged.
        let stats = surface.render(size(20, 4), &doc)?;

        let raw = output(&surface);
        assert_ne!(stats.kind, RenderKind::FullRedraw, "{raw:?}");
        assert!(!raw.contains("\x1b[2J"), "{raw:?}");
        assert!(!raw.contains("\x1b[3J"), "{raw:?}");
        assert_eq!(surface.state().previous_height, 4);
        Ok(())
    }

    #[test]
    fn termux_width_change_still_takes_the_resize_rebuild() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.set_termux(true);
        // Row wider than the new width, so the in-place repaint is off the
        // table and the width change must clear the screen + rebuild, even
        // under Termux (its height-only exemption never covers width).
        surface.render(size(20, 8), &[Line::from("abcdefghijkl")])?;
        surface.writer_mut().clear();

        let stats = surface.render(size(10, 8), &[Line::from("abcdefghij"), Line::from("kl")])?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        assert!(output(&surface).contains(CLEAR_SCREEN));
        Ok(())
    }

    #[test]
    fn shrinking_content_replays_to_avoid_stale_rows() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.render(
            size(20, 5),
            &[
                Line::from("old editor"),
                Line::from("old palette"),
                Line::from("prompt"),
            ],
        )?;
        surface.writer_mut().clear();

        let stats = surface.render(size(20, 5), &[Line::from("prompt")])?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        let plain = strip_ansi(&surface.state().previous_lines.join("\n"));
        assert!(!plain.contains("old editor"));
        assert!(!plain.contains("old palette"));
        assert!(output(&surface).contains(CLEAR_TO_SCREEN_END));
        Ok(())
    }

    #[test]
    fn over_width_line_errors_and_writes_crash_log_instead_of_clipping() {
        let dir = std::env::temp_dir().join(format!("iris-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut surface = TerminalSurface::new(Vec::new());
        surface.set_crash_log_dir(Some(dir.clone()));

        let result = surface.render(size(5, 3), &[Line::from("abcdef")]);

        // Over-width is a renderer bug: fail before writing corrupting output,
        // never silently clip.
        let error = result.expect_err("over-width render must error");
        assert!(
            error.to_string().contains("exceeds terminal width"),
            "{error}"
        );
        // Nothing corrupting was written to the terminal.
        assert!(output(&surface).is_empty(), "{:?}", output(&surface));
        // A diagnostic log was written under the Iris-owned crash dir.
        let log = std::fs::read_to_string(dir.join(CRASH_LOG_FILE)).expect("crash log written");
        assert!(log.contains("abcdef"), "{log}");
        assert!(log.contains("Over-width render"), "{log}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn exact_width_line_is_not_treated_as_over_width() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        // "abcde" is exactly width 5 and must render without error.
        surface.render(size(5, 3), &[Line::from("abcde")])?;
        assert!(strip_ansi(&output(&surface)).contains("abcde"));
        Ok(())
    }

    #[test]
    fn focused_cursor_marker_is_stripped_and_positions_hardware_cursor() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let editor = Line::from(vec![
            Span::raw("ab"),
            Span::raw(CURSOR_MARKER),
            Span::raw("c"),
        ]);
        let stats = surface.render(size(20, 5), &[Line::from("prompt"), editor])?;

        assert_eq!(stats.kind, RenderKind::First);
        let raw = output(&surface);
        // The marker never reaches the terminal output or the diff source.
        assert!(!raw.contains(CURSOR_MARKER), "marker leaked: {raw:?}");
        assert!(
            !surface
                .state()
                .previous_lines
                .iter()
                .any(|l| l.contains(CURSOR_MARKER)),
            "marker leaked into previous_lines"
        );
        // Visible content is intact (marker is zero-width).
        assert!(strip_ansi(&raw).contains("abc"), "{raw:?}");
        // The hardware cursor is positioned to the marker column (after "ab" ->
        // column index 2 -> 1-indexed 3) on the editor row.
        assert!(
            raw.contains("\x1b[3G"),
            "cursor column move missing: {raw:?}"
        );
        Ok(())
    }

    #[test]
    fn cursor_positioned_even_on_unchanged_render() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        // Identical document + marker on both renders: the stripped row strings
        // do not change, so the changed-range diff yields `Unchanged`. The
        // hardware cursor must still be positioned (independent of the diff).
        let doc = || {
            vec![
                Line::from("prompt"),
                Line::from(vec![Span::raw("abc"), Span::raw(CURSOR_MARKER)]),
            ]
        };
        surface.render(size(20, 5), &doc())?;
        surface.writer_mut().clear();

        let stats = surface.render(size(20, 5), &doc())?;

        assert_eq!(stats.kind, RenderKind::Unchanged);
        let raw = output(&surface);
        // Marker after "abc" -> column index 3 -> 1-indexed 4.
        assert!(raw.contains("\x1b[4G"), "cursor not repositioned: {raw:?}");
        Ok(())
    }

    #[test]
    fn show_hardware_cursor_emits_show_sequence_at_marker() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.set_show_hardware_cursor(true);
        let editor = Line::from(vec![Span::raw("x"), Span::raw(CURSOR_MARKER)]);
        surface.render(size(20, 5), &[editor])?;
        assert!(
            output(&surface).contains(SHOW_CURSOR),
            "{:?}",
            output(&surface)
        );
        Ok(())
    }

    #[test]
    fn wide_glyph_over_width_errors_before_terminal_write() {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.set_crash_log_dir(None);

        let result = surface.render(size(5, 3), &[Line::from("\u{4e2d}\u{6587}\u{5b57}")]);

        let error = result.expect_err("wide glyph overflow must error");
        assert!(
            error.to_string().contains("exceeds terminal width"),
            "{error}"
        );
        assert!(output(&surface).is_empty(), "{:?}", output(&surface));
    }

    #[test]
    fn styled_line_renders_color_and_modifiers_as_ansi() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let line = Line::from(vec![Span::styled(
            "ok",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )]);
        surface.render(size(10, 3), &[line])?;
        let out = output(&surface);
        assert!(
            out.contains("\x1b[32;1m") || out.contains("\x1b[1;32m"),
            "{out:?}"
        );
        assert!(out.contains("ok"));
        Ok(())
    }

    #[test]
    fn finish_moves_below_document_with_one_newline() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.render(size(20, 5), &[Line::from("hello")])?;
        surface.writer_mut().clear();

        surface.finish()?;

        let out = output(&surface);
        assert!(out.starts_with(ENABLE_AUTOWRAP));
        assert!(out.contains(END_SYNC));
        assert!(out.ends_with("\r\n"));
        assert_eq!(surface.state().hardware_cursor_row, 1);
        Ok(())
    }
}
