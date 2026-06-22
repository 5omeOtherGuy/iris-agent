//! Iris-owned terminal surface renderer for the TUI.
//!
//! This module owns the terminal document diff/replay state that Ratatui's
//! `Terminal` previously hid behind an inline viewport. Ratatui still supplies
//! `Line`/`Span`/`Style` primitives to the UI, but Iris decides when to append,
//! patch, or fully replay the terminal surface.

use std::io::{self, Write};

use ratatui::layout::Size;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const BEGIN_SYNC: &str = "\x1b[?2026h";
const END_SYNC: &str = "\x1b[?2026l";
const DISABLE_AUTOWRAP: &str = "\x1b[?7l";
const ENABLE_AUTOWRAP: &str = "\x1b[?7h";
const CLEAR_TO_SCREEN_END: &str = "\x1b[J";

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
    pub(crate) previous_lines: Vec<String>,
    pub(crate) previous_width: u16,
    pub(crate) previous_height: u16,
    pub(crate) previous_viewport_top: usize,
    /// Logical row where the terminal cursor is expected to be after Iris's last
    /// write. It may be outside the visible viewport when content has scrolled.
    pub(crate) hardware_cursor_row: usize,
    pub(crate) first_render: bool,
}

pub(crate) struct TerminalSurface<W> {
    writer: W,
    state: RenderState,
}

struct RenderedLine {
    ansi: String,
}

impl<W: Write> TerminalSurface<W> {
    pub(crate) fn new(writer: W) -> Self {
        Self {
            writer,
            state: RenderState {
                first_render: true,
                ..RenderState::default()
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> &RenderState {
        &self.state
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
        let width = size.width.max(1);
        let height = size.height.max(1);
        let rendered = render_lines(lines, width)?;
        let new_lines: Vec<String> = rendered.into_iter().map(|line| line.ansi).collect();

        let width_changed = self.state.previous_width != 0 && self.state.previous_width != width;
        let height_changed =
            self.state.previous_height != 0 && self.state.previous_height != height;

        if self.state.first_render && !width_changed && !height_changed {
            self.write_full(new_lines, width, height, false)?;
            return Ok(RenderStats {
                kind: RenderKind::First,
            });
        }

        if width_changed || height_changed {
            self.write_full(new_lines, width, height, true)?;
            return Ok(RenderStats {
                kind: RenderKind::FullRedraw,
            });
        }

        match self.write_diff_or_replay(&new_lines, width, height)? {
            RenderKind::FullRedraw => Ok(RenderStats {
                kind: RenderKind::FullRedraw,
            }),
            kind => Ok(RenderStats { kind }),
        }
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
        lines: Vec<String>,
        width: u16,
        height: u16,
        clear: bool,
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
            self.move_to_viewport_top(&mut buffer);
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
        self.remember(lines, width, height, hardware_cursor_row);
        Ok(())
    }

    fn move_to_viewport_top(&mut self, buffer: &mut String) {
        let current_screen_row = self
            .state
            .hardware_cursor_row
            .saturating_sub(self.state.previous_viewport_top);
        if current_screen_row > 0 {
            buffer.push_str(&format!("\x1b[{current_screen_row}A"));
        }
        buffer.push('\r');
        self.state.hardware_cursor_row = self.state.previous_viewport_top;
    }

    fn write_diff_or_replay(
        &mut self,
        lines: &[String],
        width: u16,
        height: u16,
    ) -> io::Result<RenderKind> {
        let previous_len = self.state.previous_lines.len();
        let new_len = lines.len();

        let Some((first_changed, last_changed)) = changed_range(&self.state.previous_lines, lines)
        else {
            self.state.previous_width = width;
            self.state.previous_height = height;
            self.state.previous_viewport_top = viewport_top(new_len, height);
            return Ok(RenderKind::Unchanged);
        };

        let append_only =
            new_len > previous_len && first_changed == previous_len && previous_len > 0;
        if append_only {
            self.write_append(&lines[previous_len..], width, height, new_len)?;
            return Ok(RenderKind::Append);
        }

        // Shrinking or changing above the visible previous viewport cannot be
        // patched safely without risking stale rows; replay from Iris-owned state.
        // Changes that extend below the viewport are safe to write through the
        // bottom of the terminal and let the terminal scroll naturally.
        if new_len < previous_len || first_changed < self.state.previous_viewport_top {
            self.write_full(lines.to_vec(), width, height, true)?;
            return Ok(RenderKind::FullRedraw);
        }

        self.write_visible_diff(lines, width, height, first_changed, last_changed)?;
        Ok(RenderKind::Diff)
    }

    fn write_append(
        &mut self,
        appended: &[String],
        width: u16,
        height: u16,
        new_len: usize,
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
        for line in appended {
            buffer.push_str("\r\n");
            buffer.push_str(line);
            self.state.hardware_cursor_row += 1;
        }
        buffer.push_str(ENABLE_AUTOWRAP);
        buffer.push_str(END_SYNC);
        write!(self.writer, "{buffer}")?;
        self.writer.flush()?;

        let hardware_cursor_row = new_len.saturating_sub(1);
        self.state.previous_lines.extend(appended.iter().cloned());
        self.remember_metadata(width, height, hardware_cursor_row);
        Ok(())
    }

    fn write_visible_diff(
        &mut self,
        lines: &[String],
        width: u16,
        height: u16,
        first_changed: usize,
        last_changed: usize,
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
        self.remember(lines.to_vec(), width, height, last_changed);
        Ok(())
    }

    fn remember(
        &mut self,
        lines: Vec<String>,
        width: u16,
        height: u16,
        hardware_cursor_row: usize,
    ) {
        self.state.previous_lines = lines;
        self.remember_metadata(width, height, hardware_cursor_row);
    }

    fn remember_metadata(&mut self, width: u16, height: u16, hardware_cursor_row: usize) {
        self.state.previous_width = width;
        self.state.previous_height = height;
        self.state.previous_viewport_top = viewport_top(self.state.previous_lines.len(), height);
        self.state.hardware_cursor_row = hardware_cursor_row;
        self.state.first_render = false;
    }
}

fn changed_range(previous: &[String], next: &[String]) -> Option<(usize, usize)> {
    let max_len = previous.len().max(next.len());
    let mut first = None;
    let mut last = 0usize;
    for i in 0..max_len {
        let old = previous.get(i).map(String::as_str).unwrap_or("");
        let new = next.get(i).map(String::as_str).unwrap_or("");
        if old != new {
            first.get_or_insert(i);
            last = i;
        }
    }
    first.map(|first| (first, last))
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

fn render_lines(lines: &[Line<'static>], width: u16) -> io::Result<Vec<RenderedLine>> {
    let max = usize::from(width.max(1));
    lines.iter().map(|line| render_line(line, max)).collect()
}

fn render_line(line: &Line<'static>, max_width: usize) -> io::Result<RenderedLine> {
    // Autowrap is disabled while we write, so an over-wide line would otherwise
    // be silently clipped by the terminal at an arbitrary byte. Clip it here at
    // a display-width boundary instead, preserving ANSI styling and emitting a
    // trailing reset so styles never leak past the line.
    let mut out = String::new();
    let mut used = 0usize;
    for span in &line.spans {
        let style = line.style.patch(span.style);
        out.push_str("\x1b[0m");
        out.push_str(&style_sgr(style));
        if used < max_width {
            let (clipped, clipped_width) = clip_to_width(span.content.as_ref(), max_width - used);
            out.push_str(&clipped);
            used += clipped_width;
        }
    }
    out.push_str("\x1b[0m");
    Ok(RenderedLine { ansi: out })
}

fn clip_to_width(content: &str, remaining: usize) -> (String, usize) {
    let full_width = UnicodeWidthStr::width(content);
    if full_width <= remaining {
        return (content.to_string(), full_width);
    }
    let mut out = String::new();
    let mut width = 0usize;
    for ch in content.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > remaining {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    (out, width)
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
    fn width_resize_forces_coherent_full_replay() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.render(size(20, 4), &[Line::from("abcdef")])?;
        surface.writer_mut().clear();

        let stats = surface.render(size(10, 4), &[Line::from("abc"), Line::from("def")])?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        let out = output(&surface);
        assert!(out.contains(CLEAR_TO_SCREEN_END), "{out:?}");
        assert!(
            !out.contains("\x1b[2J"),
            "must not clear full screen: {out:?}"
        );
        assert!(
            !out.contains("\x1b[3J"),
            "must not clear native scrollback: {out:?}"
        );
        assert!(strip_ansi(&out).contains("abc\r\ndef"), "{out:?}");
        assert_eq!(surface.state().previous_width, 10);
        Ok(())
    }

    #[test]
    fn transcript_growth_before_bottom_chrome_patches_without_full_redraw() -> io::Result<()> {
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

        assert_eq!(stats.kind, RenderKind::Diff);
        let out = output(&surface);
        assert!(!out.contains(CLEAR_TO_SCREEN_END), "{out:?}");
        assert!(strip_ansi(&out).contains("transcript 6"), "{out:?}");
        Ok(())
    }

    #[test]
    fn full_redraw_repaints_only_visible_slice_for_long_document() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let doc: Vec<Line<'static>> = (1..=8).map(|n| Line::from(format!("line {n}"))).collect();
        surface.render(size(20, 5), &doc)?;
        // Document scrolled: the viewport top is below logical line 0.
        assert_eq!(surface.state().previous_viewport_top, 3);
        surface.writer_mut().clear();

        // Width change forces a clearing full redraw.
        let stats = surface.render(size(18, 5), &doc)?;
        assert_eq!(stats.kind, RenderKind::FullRedraw);

        let plain = strip_ansi(&output(&surface));
        // Only the visible slice (last 5 lines) is repainted; history above the
        // viewport is not rewritten, avoiding scrollback duplication.
        assert!(plain.contains("line 4"), "{plain:?}");
        assert!(plain.contains("line 8"), "{plain:?}");
        assert!(!plain.contains("line 1"), "{plain:?}");
        assert!(!plain.contains("line 3"), "{plain:?}");
        assert!(!output(&surface).contains("\x1b[2J"), "{plain:?}");
        Ok(())
    }

    #[test]
    fn height_resize_forces_coherent_full_replay() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.render(size(20, 8), &[Line::from("a"), Line::from("b")])?;
        surface.writer_mut().clear();

        let stats = surface.render(size(20, 4), &[Line::from("a"), Line::from("b")])?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        assert!(output(&surface).contains(CLEAR_TO_SCREEN_END));
        assert_eq!(surface.state().previous_height, 4);
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
    fn over_width_line_is_clipped_to_terminal_width() -> io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        surface.render(size(5, 3), &[Line::from("abcdef")])?;
        let plain = strip_ansi(&output(&surface));
        assert!(plain.contains("abcde"), "{plain:?}");
        assert!(!plain.contains("abcdef"), "{plain:?}");
        // Styles are always closed with a trailing reset even when clipped.
        assert!(output(&surface).contains("\x1b[0m"));
        Ok(())
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
