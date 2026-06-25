//! Terminal front-end state and rendering (Tier 3) built on Iris-owned terminal
//! surface lifecycle plus Ratatui UI primitives.
//!
//! Layering: [`Screen`] owns all replayable UI state (transcript, editor,
//! spinner, slash palette, modal). Ratatui remains a text/style/layout/widget
//! toolkit (`Line`, `Span`, `Buffer`, `Layout`, `Paragraph`, and
//! `ratatui-textarea`), but [`TuiUi`] no longer delegates terminal lifecycle,
//! diffing, terminal-surface replay, or resize behavior to Ratatui `Terminal`. The
//! production terminal surface lives in [`crate::ui::terminal_surface`] and
//! redraws from this Iris-owned state on resize.
//!
//! Concurrency / cancellation: raw mode is entered ONCE for the whole session,
//! so Ctrl-C arrives as a key event, never SIGINT; the loop (not this module)
//! reads keys and cancels the turn token. This module performs no terminal
//! reads and holds no channels, so its state transitions and logical document
//! output are unit-testable without a TTY.

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use ansi_to_tui::IntoText;
use anyhow::Result;
use ratatui::buffer::Buffer;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode, size as terminal_size};
use ratatui::layout::{Constraint, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui_textarea::{TextArea, WrapMode};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::nexus::{ApprovalDecision, ProviderUsage, ToolCall};
use crate::tool_display::{
    display_path, exploration_summary, is_exploration_tool, run_target, summarize,
};
use crate::ui::modal::Modal;
use crate::ui::slash::{self, Palette, SlashCommand};
use crate::ui::terminal_surface::TerminalSurface;
use crate::ui::{TurnErrorKind, UiEvent};

mod pane;

/// Editor box grows with content up to this many text rows, then scrolls
/// internally (keeps the transcript from being squeezed by a huge paste).
const MAX_EDITOR_ROWS: u16 = 10;

/// Above-editor menu height cap, including the blank row above and below.
const MAX_MENU_ROWS: u16 = 16;
const MIN_EDITOR_H: u16 = 5;
const GLOBAL_STATUS_H: u16 = 1;

/// Compact inline footprint for a short session. Once the transcript grows past
/// this, Iris naturally scrolls with the terminal; before then it stays near the
/// bottom instead of immediately occupying the whole terminal height.
const MIN_INLINE_DOCUMENT_ROWS: u16 = 16;

/// Safety valve for long-running sessions: keep rendering and retained
/// transcript state bounded. The terminal's own scrollback already contains
/// earlier emitted rows; Iris keeps the recent tail for resize replay.
const MAX_TRANSCRIPT_ROWS: usize = 10_000;
const MAX_STREAMING_MARKDOWN_BYTES: usize = 64 * 1024;

/// Flood guard: cap a tool result at this many physical (wrapped) rows in the
/// transcript so a few very long lines cannot flood the viewport/scrollback.
/// Tuned to Codex's compact exec cell: a finalized result keeps a head and a
/// tail slice with a `… +N lines` marker between (see [`Transcript::push_tool_output`]).
/// The model still receives the full output; only the terminal preview is
/// bounded, and the omitted logical-line count is reported.
const MAX_TOOL_OUTPUT_ROWS: usize = 8;
const PANEL_BODY_SIDE_PADDING: usize = 1;
const PANEL_BODY_BORDER_WIDTH: usize = 2;
const PANEL_BODY_CHROME_WIDTH: usize = PANEL_BODY_BORDER_WIDTH + PANEL_BODY_SIDE_PADDING * 2;

const BORDER: Color = Color::Rgb(82, 84, 86);
const TEXT: Color = Color::Rgb(205, 205, 199);
const MUTED: Color = Color::Rgb(125, 126, 123);
const ORANGE: Color = Color::Rgb(255, 111, 31);
const GREEN: Color = Color::Rgb(109, 196, 119);
const RED: Color = Color::Rgb(221, 93, 69);
const DIFF_ADD_BG: Color = Color::Rgb(25, 45, 31);
const DIFF_DEL_BG: Color = Color::Rgb(55, 31, 30);
const COMPOSER_HINT: &str = "↵ to send  •  shift+↵ for new line  •  / for commands";

const X_PADDING: usize = 2;
const BOX_X_PADDING: usize = X_PADDING;
const TEXT_X_PADDING: usize = X_PADDING;
const TEXT_COLUMN_X_PADDING: usize = BOX_X_PADDING + TEXT_X_PADDING;
const BOX_X_PADDING_U16: u16 = X_PADDING as u16;
const TEXT_X_PADDING_U16: u16 = X_PADDING as u16;

/// Secondary guard: truncate any single output line to this many characters
/// before wrapping, so one pathological line cannot dominate the row budget.
const MAX_TOOL_OUTPUT_LINE_CHARS: usize = 2000;

/// Cap on the live exec stream buffer re-rendered under the gutter on each
/// delta. Only the tail (flood-capped to `MAX_TOOL_OUTPUT_ROWS`) is shown and
/// the authoritative full output arrives with the final `ToolResult`, so
/// trimming the head here only bounds the per-delta re-render cost; it never
/// reaches the model.
const MAX_EXEC_STREAM_BYTES: usize = 64 * 1024;

/// Braille spinner frames; cycled by the render tick while a turn computes.
/// Not emojis: single-cell Unicode glyphs that render on any UTF-8 terminal.
const SPINNER_FRAMES: &[&str] = &[
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];

fn ok_style() -> Style {
    Style::default().fg(GREEN)
}
fn err_style() -> Style {
    Style::default().fg(RED)
}
fn dim_style() -> Style {
    Style::default().fg(MUTED)
}
fn prompt_style() -> Style {
    Style::default().fg(ORANGE)
}
fn tool_header_style() -> Style {
    Style::default().fg(TEXT)
}

fn status_dot_style(running: bool, failed: bool) -> Style {
    if failed && !running {
        err_style()
    } else if running {
        prompt_style()
    } else {
        ok_style()
    }
}

/// Render instrument-panel runtime as a fixed clock field (`00:01:48s`).
fn format_panel_duration(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}s")
}

/// Format an elapsed turn duration compactly (Codex's `fmt_elapsed_compact`):
/// `45s`, `1m 11s`, `1h 03m 09s`. Used by the active-turn status and the
/// turn-end "Worked for" rule.
fn format_elapsed_compact(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!(
            "{}h {:02}m {:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }
}
/// Display width of a string as the terminal renders it, reused for word-wrap.
/// Control chars count as zero (they are not emitted).
fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Display width of a single char, clamped to at least 1 so a zero-width or
/// control char still advances the wrap and never loops forever.
fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0).max(1)
}

/// Truncate `text` to at most `max` characters (on a char boundary).
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

/// Truncate `text` to at most `max` terminal columns (display width), stopping on
/// a char boundary. Unlike [`truncate_chars`], this accounts for wide/CJK glyphs
/// so the result never exceeds `max` columns.
fn truncate_to_width(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = char_width(c);
        if used + w > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out
}

/// Clamp one logical tool-output line so it wraps to at most `max_rows` physical
/// rows at `width` (accounting for panel body chrome), appending an ellipsis
/// when content is dropped. This keeps the head/tail fold a HARD physical-row cap
/// even when a single line (e.g. a minified blob) would otherwise wrap to far
/// more rows than its slice budget.
fn clamp_output_line(raw: &str, width: usize, max_rows: usize) -> String {
    let line = truncate_chars(raw, MAX_TOOL_OUTPUT_LINE_CHARS);
    let usable = width.saturating_sub(PANEL_BODY_CHROME_WIDTH).max(1);
    let max_cols = usable.saturating_mul(max_rows.max(1));
    if display_width(&line) <= max_cols {
        return line;
    }
    format!(
        "{}\u{2026}",
        truncate_to_width(&line, max_cols.saturating_sub(1))
    )
}

/// Estimate the physical rows a tool-output line occupies after wrapping to
/// `width`, accounting for the panel borders and body padding. At least one row. ANSI
/// escapes are counted as visible width (a conservative over-estimate that only
/// makes the flood cap trip slightly earlier), which is fine for a guard.
fn wrapped_row_estimate(line: &str, width: usize) -> usize {
    let usable = width.saturating_sub(PANEL_BODY_CHROME_WIDTH).max(1);
    display_width(line).div_ceil(usable).max(1)
}

/// One styled logical transcript row. Most rows are plain text + style; ANSI
/// tool output stores a parsed ratatui line so the escape styling survives.
#[derive(Clone)]
struct TranscriptRow {
    text: String,
    style: Style,
    continuation_prefix: Option<&'static str>,
    line: Option<Line<'static>>,
    /// Word-aware (space-breaking, URL-safe) wrap for this row's styled `line`
    /// instead of the default ANSI char-hard-wrap. Set for Markdown prose; the
    /// gutter/ANSI tool-output rows keep char-wrap so their leading-space
    /// prefixes are never collapsed.
    word_wrap: bool,
    /// Full-width background fill applied to every physical row this logical row
    /// wraps to. `None` for ordinary rows; panel bodies use this for diff rows.
    background: Option<Color>,
    /// A horizontal-rule row (Codex's turn separator). When set, `text` is the
    /// optional centered label and the row renders as `─ label ─────` to width.
    hrule: bool,
    chrome: Option<ChromeRow>,
}

impl TranscriptRow {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            continuation_prefix: None,
            line: None,
            word_wrap: false,
            background: None,
            hrule: false,
            chrome: None,
        }
    }

    fn chrome(chrome: ChromeRow) -> Self {
        Self::chrome_with_text(chrome, String::new(), Style::default())
    }

    fn chrome_with_text(chrome: ChromeRow, text: String, style: Style) -> Self {
        Self {
            text,
            style,
            continuation_prefix: None,
            line: None,
            word_wrap: false,
            background: None,
            hrule: false,
            chrome: Some(chrome),
        }
    }

    fn render(&self, width: usize, out: &mut Vec<Line<'static>>) {
        if let Some(chrome) = &self.chrome {
            if let ChromeRow::Body { line, bg } = chrome {
                panel_body_lines(width, line.clone(), *bg, out);
                return;
            }
            out.push(chrome.render(width));
            return;
        }
        if self.hrule {
            out.push(inset_rule_line(width, &self.text));
            return;
        }
        let boxed = self.background.is_some();
        let box_width = if boxed {
            width.saturating_sub(BOX_X_PADDING * 2).max(1)
        } else {
            width
        };
        let content_padding = row_text_padding(self);
        let render_width = box_width
            .saturating_sub(content_padding.saturating_mul(2))
            .max(1);
        let start = out.len();
        match &self.line {
            Some(line) if self.word_wrap => {
                if let Some(prefix) = self.continuation_prefix {
                    push_wrapped_line_wordwise_with_prefix(line, render_width, prefix, out);
                } else {
                    push_wrapped_line_wordwise(line, render_width, out);
                }
            }
            Some(line) => push_wrapped_line(line, render_width, self.continuation_prefix, out),
            None => push_wrapped_row(
                &self.text,
                self.style,
                render_width,
                self.continuation_prefix,
                out,
            ),
        }
        if content_padding > 0 {
            for physical in &mut out[start..] {
                pad_line_left(physical, content_padding);
            }
        }
        if let Some(bg) = self.background {
            for physical in &mut out[start..] {
                apply_width_bg(physical, bg, box_width);
                pad_line_left(physical, BOX_X_PADDING);
                pad_line_right(physical, BOX_X_PADDING);
            }
        }
    }
}

#[derive(Clone)]
enum ChromeRow {
    Top,
    Header {
        expanded: bool,
        title: &'static str,
        meta: String,
        right: Vec<(String, Style)>,
    },
    Separator,
    Bottom,
    Body {
        line: Line<'static>,
        bg: Option<Color>,
    },
}

impl ChromeRow {
    fn render(&self, width: usize) -> Line<'static> {
        match self {
            ChromeRow::Top => panel_rule_line(width, '┌', '┐'),
            ChromeRow::Header {
                expanded,
                title,
                meta,
                right,
            } => panel_header_line(width, *expanded, title, meta, right),
            ChromeRow::Separator => panel_rule_line(width, '├', '┤'),
            ChromeRow::Bottom => panel_rule_line(width, '└', '┘'),
            ChromeRow::Body { line, bg } => panel_body_line(width, line.clone(), *bg),
        }
    }
}

fn pad_line_left(line: &mut Line<'static>, padding: usize) {
    if padding > 0 {
        line.spans
            .insert(0, Span::styled(" ".repeat(padding), line.style));
    }
}

fn pad_line_right(line: &mut Line<'static>, padding: usize) {
    if padding > 0 {
        line.spans
            .push(Span::styled(" ".repeat(padding), line.style));
    }
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn truncate_line(line: &mut Line<'static>, max: usize) {
    let max = max.max(1);
    let mut used = 0;
    let mut spans = Vec::new();
    for span in std::mem::take(&mut line.spans) {
        if used >= max {
            break;
        }
        let content = truncate_to_width(span.content.as_ref(), max - used);
        used += display_width(&content);
        if !content.is_empty() {
            spans.push(Span::styled(content, span.style));
        }
    }
    line.spans = spans;
}

/// Build a dim full-width horizontal rule, optionally wrapping a centered label
/// (`─ Worked for 2m 12s ───────`). Codex's `FinalMessageSeparator`.
fn hrule_line(label: &str, width: usize) -> Line<'static> {
    let width = width.max(1);
    if label.is_empty() {
        return Line::from(Span::styled("\u{2500}".repeat(width), dim_style()));
    }
    let text = truncate_to_width(&format!("\u{2500} {label} \u{2500}"), width);
    let fill = width.saturating_sub(display_width(&text));
    Line::from(Span::styled(
        format!("{text}{}", "\u{2500}".repeat(fill)),
        dim_style(),
    ))
}

fn border_style() -> Style {
    Style::default().fg(BORDER)
}

fn panel_style() -> Style {
    Style::default().fg(TEXT)
}

fn panel_outer_padding(width: usize) -> usize {
    if width <= BOX_X_PADDING.saturating_mul(2).saturating_add(1) {
        0
    } else {
        BOX_X_PADDING
    }
}

fn panel_width(width: usize) -> usize {
    width
        .saturating_sub(panel_outer_padding(width).saturating_mul(2))
        .max(1)
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

fn take_spans_to_width(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut used = 0usize;
    let mut out = Vec::new();
    for span in spans {
        if used >= max {
            break;
        }
        let content = truncate_to_width(span.content.as_ref(), max - used);
        used += display_width(&content);
        if !content.is_empty() {
            out.push(Span::styled(content, span.style));
        }
    }
    out
}

fn panel_rule_line(width: usize, left: char, right: char) -> Line<'static> {
    let outer = panel_outer_padding(width);
    let rule_width = panel_width(width);
    let rule = match rule_width {
        0 => String::new(),
        1 => left.to_string(),
        2 => format!("{left}{right}"),
        n => format!("{left}{}{right}", "─".repeat(n - 2)),
    };
    let mut line = Line::from(vec![
        Span::raw(" ".repeat(outer)),
        Span::styled(rule, border_style()),
        Span::raw(" ".repeat(outer)),
    ]);
    truncate_line(&mut line, width.max(1));
    line
}

fn panel_header_line(
    width: usize,
    expanded: bool,
    title: &'static str,
    meta: &str,
    right: &[(String, Style)],
) -> Line<'static> {
    let panel_width = panel_width(width);
    let inner_width = panel_width.saturating_sub(2);
    let arrow = if expanded { "▾" } else { "▸" };
    let title_width = title.len().max(7);
    let mut left = vec![
        Span::styled(format!(" {arrow}  "), dim_style()),
        Span::styled(format!("{title:<title_width$}"), panel_style()),
    ];
    let meta = strip_ansi_for_text(meta);
    if !meta.is_empty() {
        left.push(Span::styled(format!("  {meta}"), dim_style()));
    }
    let right_full: Vec<Span<'static>> = right
        .iter()
        .map(|(text, style)| Span::styled(strip_ansi_for_text(text), *style))
        .collect();
    let right = take_spans_to_width(right_full, inner_width / 2);
    let right_width = spans_width(&right);
    let left = take_spans_to_width(left, inner_width.saturating_sub(right_width));
    let left_width = spans_width(&left);
    let spacer = inner_width
        .saturating_sub(left_width)
        .saturating_sub(right_width);
    let outer = panel_outer_padding(width);
    let mut spans = vec![
        Span::raw(" ".repeat(outer)),
        Span::styled("│", border_style()),
    ];
    spans.extend(left);
    spans.push(Span::styled(" ".repeat(spacer), panel_style()));
    spans.extend(right);
    spans.push(Span::styled("│", border_style()));
    spans.push(Span::raw(" ".repeat(outer)));
    let mut line = Line::from(spans);
    truncate_line(&mut line, width.max(1));
    line
}

fn panel_body_line(width: usize, mut line: Line<'static>, bg: Option<Color>) -> Line<'static> {
    let panel_width = panel_width(width).max(PANEL_BODY_BORDER_WIDTH);
    let body_width = panel_width.saturating_sub(PANEL_BODY_CHROME_WIDTH).max(1);
    truncate_line(&mut line, body_width);
    if let Some(bg) = bg {
        apply_width_bg(&mut line, bg, body_width);
    } else {
        let used = display_width(&line_text(&line));
        if used < body_width {
            line.spans.push(Span::styled(
                " ".repeat(body_width - used),
                Style::default(),
            ));
        }
    }
    let outer = panel_outer_padding(width);
    let side_padding = " ".repeat(PANEL_BODY_SIDE_PADDING);
    let mut spans = vec![
        Span::raw(" ".repeat(outer)),
        Span::styled("│", border_style()),
        Span::styled(side_padding.clone(), panel_style()),
    ];
    spans.extend(line.spans);
    spans.push(Span::styled(side_padding, panel_style()));
    spans.push(Span::styled("│", border_style()));
    spans.push(Span::raw(" ".repeat(outer)));
    let mut line = Line::from(spans);
    truncate_line(&mut line, width.max(1));
    line
}

fn panel_body_lines(
    width: usize,
    line: Line<'static>,
    bg: Option<Color>,
    out: &mut Vec<Line<'static>>,
) {
    let panel_width = panel_width(width).max(PANEL_BODY_BORDER_WIDTH);
    let body_width = panel_width.saturating_sub(PANEL_BODY_CHROME_WIDTH).max(1);
    let mut wrapped = Vec::new();
    push_wrapped_line(&line, body_width, None, &mut wrapped);
    for physical in wrapped {
        out.push(panel_body_line(width, physical, bg));
    }
}

fn inset_rule_line(width: usize, label: &str) -> Line<'static> {
    let rule_width = width.saturating_sub(BOX_X_PADDING * 2).max(1);
    let mut line = hrule_line(label, rule_width);
    pad_line_left(&mut line, BOX_X_PADDING);
    pad_line_right(&mut line, BOX_X_PADDING);
    line
}

/// Apply a background fill to one already-wrapped physical line, then pad to
/// `width` with a trailing background span (ratatui only colours the cells a
/// span occupies).
fn apply_width_bg(line: &mut Line<'static>, bg: Color, width: usize) {
    for span in &mut line.spans {
        span.style = span.style.bg(bg);
    }
    let used = display_width(&line_text(line));
    if used < width {
        line.spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
}

/// A block-separator row: the empty plain row `push_blank` inserts between
/// top-level blocks. Distinguished from a Markdown-internal blank line (which
/// carries a styled `line`) and from a turn-rule row so block grouping remains
/// stable while the terminal surface replays from Iris state.
fn is_separator_row(row: &TranscriptRow) -> bool {
    !row.hrule
        && row.chrome.is_none()
        && row.text.is_empty()
        && row.line.is_none()
        && row.background.is_none()
}

fn row_text_padding(row: &TranscriptRow) -> usize {
    if row.background.is_some() {
        usize::from(!row.text.is_empty()) * TEXT_X_PADDING
    } else if is_separator_row(row) {
        0
    } else {
        TEXT_COLUMN_X_PADDING
    }
}

fn push_span_char(spans: &mut Vec<Span<'static>>, ch: char, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push(ch);
        return;
    }
    spans.push(Span::styled(ch.to_string(), style));
}

fn push_wrapped_line(
    line: &Line<'static>,
    width: usize,
    continuation_prefix: Option<&'static str>,
    out: &mut Vec<Line<'static>>,
) {
    // Ponytail: ANSI rows keep color and hard-wrap only; upgrade to
    // span-aware word wrap when long colorized logs prove worth the extra code.
    let width = width.max(1);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0;

    for span in &line.spans {
        for ch in span.content.chars() {
            let cw = char_width(ch);
            if cur_w > 0 && cur_w + cw > width {
                out.push(Line::from(std::mem::take(&mut spans)));
                cur_w = 0;
                if let Some(prefix) = continuation_prefix {
                    let prefix = truncate_to_width(prefix, width.saturating_sub(1));
                    if !prefix.is_empty() {
                        cur_w = display_width(&prefix);
                        spans.push(Span::styled(prefix, dim_style()));
                    }
                }
            }
            push_span_char(&mut spans, ch, span.style);
            cur_w += cw;
        }
    }

    out.push(Line::from(spans));
}

fn styled_physical_row(
    cells: &[(char, Style)],
    cursor: &mut usize,
    physical: &str,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for rc in physical.chars() {
        while *cursor < cells.len() && cells[*cursor].0 != rc {
            *cursor += 1;
        }
        let style = cells.get(*cursor).map_or(Style::default(), |(_, st)| *st);
        push_span_char(&mut spans, rc, style);
        *cursor += 1;
    }
    Line::from(spans)
}

fn push_wrapped_line_wordwise_with_prefix(
    line: &Line<'static>,
    width: usize,
    continuation_prefix: &'static str,
    out: &mut Vec<Line<'static>>,
) {
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
        .collect();
    if cells.is_empty() {
        out.push(Line::default());
        return;
    }
    let text: String = cells.iter().map(|(ch, _)| *ch).collect();
    let width = width.max(1);
    let first = wrap_to_width(&text, width)
        .into_iter()
        .next()
        .unwrap_or_default();
    let mut cursor = 0usize;
    out.push(styled_physical_row(&cells, &mut cursor, &first));
    if cursor < cells.len() && cells[cursor].0 == ' ' {
        cursor += 1;
    }
    if cursor >= cells.len() {
        return;
    }
    let continuation_width = width
        .saturating_sub(display_width(continuation_prefix))
        .max(1);
    let remainder: String = cells[cursor..].iter().map(|(ch, _)| *ch).collect();
    for physical in wrap_to_width(&remainder, continuation_width) {
        if physical.is_empty() {
            continue;
        }
        let mut line = styled_physical_row(&cells, &mut cursor, &physical);
        pad_line_left(&mut line, display_width(continuation_prefix));
        out.push(line);
        if cursor < cells.len() && cells[cursor].0 == ' ' {
            cursor += 1;
        }
    }
}

/// Word-aware, URL-safe wrap of a styled line, preserving each span's style.
/// Reuses [`wrap_to_width`] for the break positions (so it breaks at spaces and
/// keeps long URL/path tokens intact), then re-applies styles by walking the
/// original chars in parallel. Used for Markdown prose; ANSI/gutter rows keep
/// the char-hard-wrap [`push_wrapped_line`] so their leading-space prefixes are
/// never collapsed.
fn push_wrapped_line_wordwise(line: &Line<'static>, width: usize, out: &mut Vec<Line<'static>>) {
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
        .collect();
    if cells.is_empty() {
        out.push(Line::default());
        return;
    }
    let text: String = cells.iter().map(|(ch, _)| *ch).collect();
    let mut cursor = 0;
    for physical in wrap_to_width(&text, width.max(1)) {
        out.push(styled_physical_row(&cells, &mut cursor, &physical));
    }
}

fn push_wrapped_row(
    text: &str,
    style: Style,
    width: usize,
    continuation_prefix: Option<&'static str>,
    out: &mut Vec<Line<'static>>,
) {
    let rows = wrap_to_width(text, width);
    let Some(first) = rows.first() else {
        return;
    };
    out.push(Line::from(Span::styled(first.clone(), style)));
    let Some(prefix) = continuation_prefix else {
        for physical in rows.into_iter().skip(1) {
            out.push(Line::from(Span::styled(physical, style)));
        }
        return;
    };
    if first.is_empty() {
        return;
    }

    let continuation_width = width.saturating_sub(display_width(prefix)).max(1);
    let remainder = text
        .strip_prefix(first)
        .unwrap_or_default()
        .strip_prefix(' ')
        .unwrap_or_default();
    for physical in wrap_to_width(remainder, continuation_width) {
        if !physical.is_empty() {
            out.push(Line::from(vec![
                Span::styled(prefix, dim_style()),
                Span::styled(physical, style),
            ]));
        }
    }
}

/// The currently-streaming exec block (issue #90 sub-item 1). `bash` is
/// exclusive, so at most one is ever open. `body_start` is the row index of the
/// block body (its `Running`/`Ran` header). `output` is the bounded live tail
/// re-rendered (and flood-capped) under the gutter on each delta.
struct ActiveExec {
    call: ToolCall,
    output: String,
    body_start: usize,
    started: Instant,
}

struct ActiveExploration {
    call_id: String,
    row: usize,
    started: Instant,
    duration: Option<Duration>,
    failed: bool,
    done: bool,
}

struct ActiveTool {
    call: ToolCall,
    body_start: usize,
    started: Instant,
}

fn tool_panel_title(call: &ToolCall) -> &'static str {
    match call.name.as_str() {
        "read" | "grep" | "find" | "ls" => "EXPLORE",
        "write" | "edit" => "EDIT",
        _ => "TOOL",
    }
}

fn tool_path_arg(call: &ToolCall) -> Option<&str> {
    call.arguments
        .get("file_path")
        .or_else(|| call.arguments.get("path"))
        .and_then(|value| value.as_str())
}

fn tool_panel_meta(call: &ToolCall) -> String {
    tool_path_arg(call)
        .map(display_path)
        .unwrap_or_else(|| summarize(call))
}

fn explore_panel_meta(call: &ToolCall) -> String {
    tool_path_arg(call)
        .map(display_path)
        .unwrap_or_else(|| "workspace".to_string())
}

fn explore_body(call: &ToolCall) -> String {
    exploration_summary(call)
}

/// Transcript state and width-aware rendering, separate from editor/spinner UI.
#[derive(Default)]
struct Transcript {
    rows: Vec<TranscriptRow>,
    /// Live assistant text being streamed; rendered after committed rows and
    /// committed exactly once on `AssistantTextEnd`.
    streaming: Option<String>,
    /// The open live exec cell, if a streaming tool is running.
    active_exec: Option<ActiveExec>,
    active_explorations: Vec<ActiveExploration>,
    active_tool: Option<ActiveTool>,
    exploring_open: bool,
    /// Last width the transcript was rendered/flushed at, so width-aware
    /// shaping in the width-agnostic `apply` path (the tool-output flood cap)
    /// uses a realistic column count. Zero until the first render.
    last_width: usize,
}

impl Transcript {
    /// Append a blank separator row before a new top-level block, unless the
    /// transcript is empty or already ends in a real separator row.
    fn push_blank(&mut self) {
        self.exploring_open = false;
        match self.rows.last() {
            None => {}
            Some(last) if is_separator_row(last) => {}
            _ => self
                .rows
                .push(TranscriptRow::new(String::new(), Style::default())),
        }
    }

    /// Finish any live stream and open a fresh block with a leading separator.
    fn begin_block(&mut self) {
        self.finish_stream();
        self.push_blank();
    }

    /// Push each line of `text` into the transcript with one style.
    fn push(&mut self, text: &str, style: Style) {
        for line in text.split('\n') {
            self.rows.push(TranscriptRow::new(line, style));
        }
    }

    fn push_assistant_text(&mut self, text: &str) {
        pane::push_assistant_rows(&mut self.rows, text);
    }

    /// Commit any in-flight streamed assistant text into the transcript.
    fn finish_stream(&mut self) {
        if let Some(text) = self.streaming.take()
            && !text.is_empty()
        {
            self.push_assistant_text(&text);
            self.push_blank();
        }
    }

    fn record_approval(&mut self, call: &ToolCall, decision: ApprovalDecision) {
        let scope = match decision {
            ApprovalDecision::Allow => "this time",
            ApprovalDecision::AllowAlways => "this session",
            ApprovalDecision::Deny => return,
        };
        self.begin_block();
        self.push_approval_panel(approval_line(call, scope), false);
    }

    fn push_approval_panel(&mut self, line: Line<'static>, failed: bool) {
        self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
        self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
            expanded: true,
            title: "APPROVAL",
            meta: "decision".to_string(),
            right: vec![
                (
                    "●".to_string(),
                    if failed { err_style() } else { ok_style() },
                ),
                (
                    if failed {
                        " DENIED      "
                    } else {
                        " RECORDED    "
                    }
                    .to_string(),
                    panel_style(),
                ),
            ],
        }));
        self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
        let text = line_text(&line);
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Body { line, bg: None },
            text,
            panel_style(),
        ));
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
    }

    /// Fallback (non-streamed) result render, used when no live exec cell was
    /// opened: exploration tools group under `Explored`; everything else gets a
    /// `• Ran`/`✗ Ran` header with the same status-colored bullet + duration as
    /// the streamed finalize, so both paths look identical.
    fn push_tool_result(
        &mut self,
        call: &ToolCall,
        content: &str,
        exit_code: Option<i32>,
        duration: Option<std::time::Duration>,
    ) {
        if is_exploration_tool(call) {
            self.push_explored_result(call, duration);
            return;
        }
        let failed = exit_code.is_some_and(|code| code != 0);
        self.begin_block();
        if call.name == "bash" {
            self.push_shell_panel(call, content, false, failed, duration, None);
        } else {
            self.push_generic_tool_panel(call, content, false, failed, duration, None);
        }
    }

    fn push_tool_error(&mut self, call: &ToolCall, message: &str) {
        self.begin_block();
        if call.name == "bash" {
            self.push_shell_panel(call, "", false, true, None, Some(message));
        } else {
            self.push_generic_tool_panel(call, "", false, true, None, Some(message));
        }
    }

    fn push_panel_body(&mut self, text: &str, style: Style) {
        for line in text.split('\n') {
            let line = strip_ansi_for_text(line);
            self.rows.push(TranscriptRow::chrome_with_text(
                ChromeRow::Body {
                    line: Line::from(Span::styled(line.clone(), style)),
                    bg: None,
                },
                line,
                style,
            ));
        }
    }

    fn push_tool_panel_body(&mut self, text: &str, style: Style) {
        for line in text.split('\n') {
            let line = strip_ansi_for_text(line);
            self.rows.push(TranscriptRow::chrome_with_text(
                ChromeRow::Body {
                    line: Line::from(Span::styled(line.clone(), style)),
                    bg: None,
                },
                line,
                style,
            ));
        }
    }

    fn push_shell_header(
        &mut self,
        running: bool,
        failed: bool,
        duration: Option<std::time::Duration>,
        started: Option<Instant>,
        target: &str,
    ) {
        let state = if running {
            " RUNNING"
        } else if failed {
            " ERROR"
        } else {
            " DONE"
        };
        let elapsed = if running {
            started
                .map(|started| format_panel_duration(started.elapsed()))
                .unwrap_or_else(|| "00:00:00s".to_string())
        } else {
            duration
                .map(format_panel_duration)
                .or_else(|| started.map(|started| format_panel_duration(started.elapsed())))
                .unwrap_or_else(|| "00:00:00s".to_string())
        };
        let plain = if running {
            format!("• Running {target}")
        } else if failed {
            format!("✗ Ran {target}")
        } else {
            format!("• Ran {target}")
        };
        self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Header {
                expanded: true,
                title: "SHELL",
                meta: "bash".to_string(),
                right: vec![
                    ("●".to_string(), status_dot_style(running, failed)),
                    (state.to_string(), panel_style()),
                    (format!("     {elapsed:>10}  "), dim_style()),
                ],
            },
            plain,
            tool_header_style(),
        ));
        self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
    }

    fn push_shell_panel(
        &mut self,
        call: &ToolCall,
        content: &str,
        running: bool,
        failed: bool,
        duration: Option<std::time::Duration>,
        error: Option<&str>,
    ) {
        let target = run_target(call);
        self.push_shell_header(running, failed, duration, None, &target);
        self.push_tool_panel_body(&format!("$ {target}"), panel_style());
        if !content.is_empty() {
            self.push_tool_output(content);
        } else if error.is_none() {
            self.push_tool_panel_body("(no output)", dim_style());
        }
        if let Some(error) = error {
            self.push_tool_panel_body(&format!("error: {error}"), err_style());
        }
        if running {
            self.push_tool_panel_body("$ █", panel_style());
        }
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
    }

    fn push_generic_tool_header(
        &mut self,
        call: &ToolCall,
        running: bool,
        failed: bool,
        duration: Option<std::time::Duration>,
        started: Option<Instant>,
    ) {
        let state = if running {
            " RUNNING"
        } else if failed {
            " ERROR"
        } else {
            " DONE"
        };
        let elapsed = if running {
            started
                .map(|started| format_panel_duration(started.elapsed()))
                .unwrap_or_else(|| "00:00:00s".to_string())
        } else {
            duration
                .map(format_panel_duration)
                .or_else(|| started.map(|started| format_panel_duration(started.elapsed())))
                .unwrap_or_else(|| "00:00:00s".to_string())
        };
        let title = tool_panel_title(call);
        let meta = tool_panel_meta(call);
        self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Header {
                expanded: true,
                title,
                meta: meta.clone(),
                right: vec![
                    ("●".to_string(), status_dot_style(running, failed)),
                    (state.to_string(), panel_style()),
                    (format!("     {elapsed:>10}  "), dim_style()),
                ],
            },
            if running {
                format!("• Running {meta}")
            } else if failed {
                format!("✗ Ran {meta}")
            } else {
                format!("• Ran {meta}")
            },
            tool_header_style(),
        ));
        self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
    }

    fn push_generic_tool_panel(
        &mut self,
        call: &ToolCall,
        content: &str,
        running: bool,
        failed: bool,
        duration: Option<std::time::Duration>,
        error: Option<&str>,
    ) {
        self.push_generic_tool_header(call, running, failed, duration, None);
        if !content.is_empty() {
            self.push_tool_output(content);
        } else if error.is_none() {
            self.push_tool_panel_body("(no output)", dim_style());
        }
        if let Some(error) = error {
            self.push_tool_panel_body(&format!("error: {error}"), err_style());
        }
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
    }

    /// Open a live exec block: a `• Running {target}` header under a fresh
    /// separator, tracked as the active cell so deltas and the final result
    /// finalize it in place.
    fn begin_exec(&mut self, call: ToolCall) {
        self.begin_block();
        let body_start = self.rows.len();
        let started = Instant::now();
        let target = run_target(&call);
        self.push_shell_header(true, false, None, Some(started), &target);
        self.push_tool_panel_body(&format!("$ {target}"), panel_style());
        self.push_tool_panel_body("$ █", panel_style());
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        self.active_exec = Some(ActiveExec {
            call,
            output: String::new(),
            body_start,
            started,
        });
    }

    fn begin_tool(&mut self, call: ToolCall) {
        self.begin_block();
        let body_start = self.rows.len();
        let started = Instant::now();
        self.push_generic_tool_header(&call, true, false, None, Some(started));
        self.push_tool_panel_body("running…", dim_style());
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        self.active_tool = Some(ActiveTool {
            call,
            body_start,
            started,
        });
    }

    fn panel_end_from(&self, start: usize) -> usize {
        self.rows[start..]
            .iter()
            .position(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::Bottom)))
            .map_or(start, |offset| start + offset + 1)
    }

    fn active_tool_panel_end(&self, active: &ActiveTool) -> usize {
        self.panel_end_from(active.body_start)
    }

    fn replace_active_tool_panel(&mut self, active: &ActiveTool, replacement: Vec<TranscriptRow>) {
        let end = self.active_tool_panel_end(active);
        self.rows.splice(active.body_start..end, replacement);
    }

    fn active_exec_panel_end(&self, active: &ActiveExec) -> usize {
        self.panel_end_from(active.body_start)
    }

    fn replace_active_exec_panel(&mut self, active: &ActiveExec, replacement: Vec<TranscriptRow>) {
        let end = self.active_exec_panel_end(active);
        self.rows.splice(active.body_start..end, replacement);
    }

    fn collect_rows(&mut self, write: impl FnOnce(&mut Self)) -> Vec<TranscriptRow> {
        let start = self.rows.len();
        write(self);
        self.rows.split_off(start)
    }

    fn finalized_tool_rows(
        &mut self,
        call: &ToolCall,
        content: &str,
        duration: Option<std::time::Duration>,
        started: Instant,
    ) -> Vec<TranscriptRow> {
        self.collect_rows(|this| {
            this.push_generic_tool_header(call, false, false, duration, Some(started));
            if !content.is_empty() {
                this.push_tool_output(content);
            } else {
                this.push_tool_panel_body("(no output)", dim_style());
            }
            this.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        })
    }

    fn errored_tool_rows(
        &mut self,
        call: &ToolCall,
        message: &str,
        started: Instant,
    ) -> Vec<TranscriptRow> {
        self.collect_rows(|this| {
            this.push_generic_tool_header(call, false, true, None, Some(started));
            this.push_tool_panel_body(&format!("error: {message}"), err_style());
            this.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        })
    }

    fn finalize_active_tool(
        &mut self,
        call: &ToolCall,
        content: &str,
        duration: Option<std::time::Duration>,
    ) -> bool {
        let Some(active) = self.active_tool.take() else {
            return false;
        };
        if active.call.id != call.id {
            self.active_tool = Some(active);
            return false;
        }
        let rows = self.finalized_tool_rows(call, content, duration, active.started);
        self.replace_active_tool_panel(&active, rows);
        true
    }

    fn finalize_active_tool_error(&mut self, call: &ToolCall, message: &str) -> bool {
        let Some(active) = self.active_tool.take() else {
            return false;
        };
        if active.call.id != call.id {
            self.active_tool = Some(active);
            return false;
        }
        let rows = self.errored_tool_rows(call, message, active.started);
        self.replace_active_tool_panel(&active, rows);
        true
    }

    fn clear_active_tool_for_preview(&mut self, call: &ToolCall) {
        if self
            .active_tool
            .as_ref()
            .is_some_and(|active| active.call.id == call.id)
            && let Some(active) = self.active_tool.take()
        {
            self.replace_active_tool_panel(&active, Vec::new());
        }
    }

    fn running_exec_rows(&mut self, active: &ActiveExec) -> Vec<TranscriptRow> {
        self.collect_rows(|this| {
            let target = run_target(&active.call);
            this.push_shell_header(true, false, None, Some(active.started), &target);
            this.push_tool_panel_body(&format!("$ {target}"), panel_style());
            this.push_tool_output_tail(&active.output);
            this.push_tool_panel_body("$ █", panel_style());
            this.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        })
    }

    fn finalized_exec_rows(
        &mut self,
        call: &ToolCall,
        content: &str,
        exit_code: Option<i32>,
        duration: Option<std::time::Duration>,
        started: Instant,
    ) -> Vec<TranscriptRow> {
        self.collect_rows(|this| {
            let target = run_target(call);
            let failed = exit_code.is_some_and(|code| code != 0);
            this.push_shell_header(false, failed, duration, Some(started), &target);
            this.push_tool_panel_body(&format!("$ {target}"), panel_style());
            this.push_tool_output(content);
            this.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        })
    }

    fn errored_exec_rows(
        &mut self,
        call: &ToolCall,
        message: &str,
        streamed_output: &str,
        started: Instant,
    ) -> Vec<TranscriptRow> {
        self.collect_rows(|this| {
            let target = run_target(call);
            this.push_shell_header(false, true, None, Some(started), &target);
            this.push_tool_panel_body(&format!("$ {target}"), panel_style());
            if !streamed_output.is_empty() {
                this.push_tool_output_tail(streamed_output);
            }
            this.push_tool_panel_body(&format!("error: {message}"), err_style());
            this.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        })
    }

    /// Re-render the open exec block in place from its bounded output buffer: the
    /// `Running` header followed by the flood-capped live tail.
    fn relayout_active_running(&mut self) {
        let Some(active) = self.active_exec.take() else {
            return;
        };
        let rows = self.running_exec_rows(&active);
        self.replace_active_exec_panel(&active, rows);
        self.active_exec = Some(active);
    }

    /// Finalize the open exec block in place (no new separator): rewrite the
    /// header to `• Ran`/`✗ Ran` with the status-colored bullet and duration,
    /// then render the authoritative final output.
    fn finalize_active(
        &mut self,
        call: &ToolCall,
        content: &str,
        exit_code: Option<i32>,
        duration: Option<std::time::Duration>,
    ) {
        let Some(active) = self.active_exec.take() else {
            return;
        };
        if active.call.id != call.id {
            self.active_exec = Some(active);
            return;
        }
        let rows = self.finalized_exec_rows(call, content, exit_code, duration, active.started);
        self.replace_active_exec_panel(&active, rows);
    }

    /// Finalize the open exec block as an error/cancellation in place: a red
    /// `✗ Ran` header, whatever streamed so far (so a cancelled command keeps
    /// its partial output), then the error line.
    fn finalize_active_error(&mut self, call: &ToolCall, message: &str) {
        let Some(active) = self.active_exec.take() else {
            return;
        };
        if active.call.id != call.id {
            self.active_exec = Some(active);
            return;
        }
        let rows = self.errored_exec_rows(call, message, &active.output, active.started);
        self.replace_active_exec_panel(&active, rows);
    }

    /// The width to assume when shaping rows during `apply` (before a frame has
    /// set the real width). Falls back to a sane 80 columns.
    fn wrap_width(&self) -> usize {
        if self.last_width == 0 {
            80
        } else {
            self.last_width
        }
    }

    /// Push one gutter-prefixed tool-output line, preserving ANSI styling and
    /// hard-wrapping so leading indentation/aligned columns survive. `first`
    /// selects the `  └ ` head gutter vs the `    ` continuation gutter.
    fn push_output_line(&mut self, raw: &str, first: bool) {
        let line = truncate_chars(raw, MAX_TOOL_OUTPUT_LINE_CHARS);
        let legacy = if first {
            format!("  └ {line}")
        } else {
            format!("    {line}")
        };
        if line.contains("\x1b[") {
            self.rows.push(TranscriptRow::chrome_with_text(
                ChromeRow::Body {
                    line: tool_output_line("", &line),
                    bg: None,
                },
                strip_ansi_for_text(&legacy),
                dim_style(),
            ));
        } else {
            self.rows.push(TranscriptRow::chrome_with_text(
                ChromeRow::Body {
                    line: Line::from(Span::styled(line.to_string(), dim_style())),
                    bg: None,
                },
                legacy,
                dim_style(),
            ));
        }
    }

    fn push_tool_output(&mut self, content: &str) {
        if content.is_empty() {
            self.push_panel_body("(no output)", dim_style());
            return;
        }
        // Flood-safe AND compact (Codex parity): wrap each line to the transcript
        // width FIRST, then keep a head slice and a tail slice that together fit
        // the physical-row budget, with a `… +N lines` marker between. Showing
        // the tail keeps a command's final/summary line visible instead of only
        // its head. The omitted count is logical lines. The live cell still uses
        // the tail-only variant while output is still growing.
        let width = self.wrap_width();
        let lines: Vec<&str> = content.lines().collect();
        let cost = |raw: &str| {
            wrapped_row_estimate(&truncate_chars(raw, MAX_TOOL_OUTPUT_LINE_CHARS), width)
        };
        let total_rows: usize = lines.iter().map(|raw| cost(raw)).sum();
        if total_rows <= MAX_TOOL_OUTPUT_ROWS {
            for (i, raw) in lines.iter().enumerate() {
                self.push_output_line(raw, i == 0);
            }
            return;
        }
        // One row is reserved for the ellipsis marker; the rest splits in half.
        let budget = MAX_TOOL_OUTPUT_ROWS.saturating_sub(1).max(1);
        let head_budget = budget / 2;
        let tail_budget = budget - head_budget;
        let mut head_rows = 0usize;
        let mut head_end = 0usize;
        // Always keep at least the first line so a single over-budget line never
        // collapses the cell to just a marker (and so the head gutter is always
        // emitted); only later lines are gated on the head budget.
        while head_end < lines.len() {
            let rows = cost(lines[head_end]);
            if head_end > 0 && head_rows + rows > head_budget {
                break;
            }
            head_rows += rows;
            head_end += 1;
        }
        let mut tail_rows = 0usize;
        let mut tail_start = lines.len();
        while tail_start > head_end {
            let rows = cost(lines[tail_start - 1]);
            if tail_rows + rows > tail_budget {
                break;
            }
            tail_rows += rows;
            tail_start -= 1;
        }
        // Clamp each shown line to its slice budget so a single over-budget line
        // (kept for visibility) cannot blow past the physical-row cap.
        for (i, raw) in lines[..head_end].iter().enumerate() {
            let clamped = clamp_output_line(raw, width, head_budget);
            self.push_output_line(&clamped, i == 0);
        }
        let hidden = tail_start - head_end;
        if hidden > 0 {
            self.push_panel_body(&format!("… +{hidden} lines"), dim_style());
        }
        for raw in &lines[tail_start..] {
            let clamped = clamp_output_line(raw, width, tail_budget);
            self.push_output_line(&clamped, false);
        }
    }

    /// TAIL-capped tool output for the LIVE streaming cell: show the most recent
    /// physical rows so a growing stream scrolls instead of freezing on its
    /// head, with a leading `… +N earlier lines` note when output was dropped.
    fn push_tool_output_tail(&mut self, content: &str) {
        if content.is_empty() {
            self.push_panel_body("(no output)", dim_style());
            return;
        }
        let width = self.wrap_width();
        let lines: Vec<&str> = content.lines().collect();
        // Walk from the end, accumulating physical rows until the budget, so the
        // newest output is what stays visible.
        let mut physical = 0usize;
        let mut take = 0usize;
        for raw in lines.iter().rev() {
            let rows =
                wrapped_row_estimate(&truncate_chars(raw, MAX_TOOL_OUTPUT_LINE_CHARS), width);
            if take > 0 && physical + rows > MAX_TOOL_OUTPUT_ROWS {
                break;
            }
            physical += rows;
            take += 1;
        }
        let start = lines.len() - take;
        if start > 0 {
            self.push_panel_body(&format!("… +{start} earlier lines"), dim_style());
        }
        for (offset, raw) in lines[start..].iter().enumerate() {
            // Use the head gutter for the very first visible row only when no
            // earlier-lines note took it. Clamp each shown line so one very long
            // line cannot blow past the physical-row cap.
            let clamped = clamp_output_line(raw, width, MAX_TOOL_OUTPUT_ROWS);
            self.push_output_line(&clamped, start == 0 && offset == 0);
        }
    }

    fn explore_header_right(
        running: bool,
        failed: bool,
        duration: Option<Duration>,
    ) -> Vec<(String, Style)> {
        let state = if running {
            " RUNNING     "
        } else if failed {
            " ERROR       "
        } else {
            " DONE        "
        };
        let elapsed = duration
            .map(format_panel_duration)
            .unwrap_or_else(|| "00:00:00s".to_string());
        vec![
            ("●".to_string(), status_dot_style(running, failed)),
            (state.to_string(), panel_style()),
            (format!("{elapsed}  "), dim_style()),
        ]
    }

    fn current_explore_header_row(&self) -> Option<usize> {
        self.rows.iter().rposition(|row| {
            matches!(
                row.chrome.as_ref(),
                Some(ChromeRow::Header {
                    title: "EXPLORE",
                    ..
                })
            )
        })
    }

    fn active_exploration_duration(&self, running: bool) -> Option<Duration> {
        if running {
            let started = self
                .active_explorations
                .iter()
                .map(|active| active.started)
                .min()?;
            return Some(started.elapsed());
        }
        self.active_explorations
            .iter()
            .filter_map(|active| active.duration)
            .max()
            .or_else(|| {
                self.active_explorations
                    .iter()
                    .map(|active| active.started)
                    .min()
                    .map(|started| started.elapsed())
            })
    }

    fn set_explore_header(
        &mut self,
        call: &ToolCall,
        running: bool,
        failed: bool,
        duration: Option<Duration>,
    ) {
        let Some(header) = self.current_explore_header_row() else {
            return;
        };
        let meta = match self.rows[header].chrome.as_ref() {
            Some(ChromeRow::Header { meta, .. }) => meta.clone(),
            _ => explore_panel_meta(call),
        };
        self.rows[header] = TranscriptRow::chrome(ChromeRow::Header {
            expanded: true,
            title: "EXPLORE",
            meta,
            right: Self::explore_header_right(running, failed, duration),
        });
    }

    fn update_explore_header_from_active(&mut self, call: &ToolCall) {
        let running = self.active_explorations.iter().any(|active| !active.done);
        let failed = self.active_explorations.iter().any(|active| active.failed);
        let duration = self.active_exploration_duration(running);
        self.set_explore_header(call, running, failed && !running, duration);
        if !running {
            self.active_explorations.clear();
        }
    }

    fn pop_trailing_explore_bottom(&mut self) {
        if matches!(
            self.rows.last().and_then(|row| row.chrome.as_ref()),
            Some(ChromeRow::Bottom)
        ) {
            self.rows.pop();
        }
    }

    fn push_explore_body(&mut self, call: &ToolCall, failed: bool, duration: Option<Duration>) {
        if self.exploring_open {
            self.pop_trailing_explore_bottom();
            self.set_explore_header(call, false, failed, duration);
        } else {
            self.push_blank();
            self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
                expanded: true,
                title: "EXPLORE",
                meta: explore_panel_meta(call),
                right: Self::explore_header_right(false, failed, duration),
            }));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
        }
        let text = explore_body(call);
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Body {
                line: Line::from(Span::styled(
                    text.clone(),
                    if failed { err_style() } else { dim_style() },
                )),
                bg: None,
            },
            text,
            if failed { err_style() } else { dim_style() },
        ));
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        self.exploring_open = true;
    }

    fn push_explored_result(&mut self, call: &ToolCall, duration: Option<Duration>) {
        self.finish_stream();
        if self.finish_exploration(call, explore_body(call), dim_style(), duration, false) {
            return;
        }
        self.push_explore_body(call, false, duration);
    }

    fn push_explored_start(&mut self, call: &ToolCall) {
        self.finish_stream();
        let started = Instant::now();
        if self.exploring_open {
            self.pop_trailing_explore_bottom();
        } else {
            self.push_blank();
            self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
                expanded: true,
                title: "EXPLORE",
                meta: explore_panel_meta(call),
                right: Self::explore_header_right(true, false, Some(Duration::ZERO)),
            }));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
        }
        let row = self.rows.len();
        let text = explore_body(call);
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Body {
                line: Line::from(Span::styled(text.clone(), dim_style())),
                bg: None,
            },
            text,
            dim_style(),
        ));
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        self.exploring_open = true;
        self.active_explorations.push(ActiveExploration {
            call_id: call.id.clone(),
            row,
            started,
            duration: None,
            failed: false,
            done: false,
        });
        self.update_explore_header_from_active(call);
    }

    fn replace_explore_body_at(&mut self, row: usize, text: String, style: Style) -> bool {
        let Some(slot) = self.rows.get_mut(row) else {
            return false;
        };
        if !matches!(slot.chrome.as_ref(), Some(ChromeRow::Body { .. })) {
            return false;
        }
        *slot = TranscriptRow::chrome_with_text(
            ChromeRow::Body {
                line: Line::from(Span::styled(text.clone(), style)),
                bg: None,
            },
            text,
            style,
        );
        true
    }

    fn finish_exploration(
        &mut self,
        call: &ToolCall,
        text: String,
        style: Style,
        duration: Option<Duration>,
        failed: bool,
    ) -> bool {
        let Some(pos) = self
            .active_explorations
            .iter()
            .position(|active| active.call_id == call.id)
        else {
            return false;
        };
        let row = self.active_explorations[pos].row;
        self.active_explorations[pos].duration = duration;
        self.active_explorations[pos].failed = failed;
        self.active_explorations[pos].done = true;
        let replaced = self.replace_explore_body_at(row, text, style);
        debug_assert!(replaced);
        self.update_explore_header_from_active(call);
        true
    }

    fn push_explored_error(&mut self, call: &ToolCall, message: &str) -> bool {
        self.finish_stream();
        self.finish_exploration(call, format!("error: {message}"), err_style(), None, true)
    }

    /// Apply one semantic event to the transcript rows.
    fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::ProviderTurnStarted { .. }
            | UiEvent::ProviderTurnCompleted { .. }
            | UiEvent::ProviderTurnCancelled { .. }
            | UiEvent::ProviderTurnError { .. }
            | UiEvent::ToolLifecycle { .. }
            | UiEvent::OutputHandleStored { .. }
            | UiEvent::CompactionApplied { .. } => {}
            UiEvent::AssistantTextDelta(delta) => {
                if self.streaming.is_none() {
                    self.push_blank();
                }
                self.streaming
                    .get_or_insert_with(String::new)
                    .push_str(&delta);
            }
            UiEvent::AssistantTextEnd(text) => {
                // A non-empty end event is authoritative. Some providers only
                // send deltas and finish with an empty end marker; in that case
                // commit the accumulated stream instead of dropping it.
                let text = if text.is_empty() {
                    self.streaming.take().unwrap_or_default()
                } else {
                    self.streaming = None;
                    text
                };
                if !text.is_empty() {
                    self.push_blank();
                    self.push_assistant_text(&text);
                    self.push_blank();
                }
            }
            UiEvent::AssistantText(text) => {
                self.finish_stream();
                if !text.is_empty() {
                    self.push_blank();
                    self.push_assistant_text(&text);
                    self.push_blank();
                }
            }
            UiEvent::SessionStarted => {
                self.finish_stream();
            }
            UiEvent::ToolProposed(_) => {
                // Non-gated tools show only their result row; nothing to render.
                self.finish_stream();
            }
            UiEvent::ToolStarted(call) => {
                if is_exploration_tool(&call) {
                    self.push_explored_start(&call);
                } else if call.name == "bash" {
                    self.begin_exec(call);
                } else {
                    self.begin_tool(call);
                }
            }
            UiEvent::ToolOutputDelta { call_id, chunk } => {
                if self
                    .active_exec
                    .as_ref()
                    .is_some_and(|a| a.call.id == call_id)
                {
                    if let Some(active) = self.active_exec.as_mut() {
                        active.output.push_str(&chunk);
                        // Bound the re-rendered buffer to its tail; only a few
                        // rows (MAX_TOOL_OUTPUT_ROWS) ever show and the full
                        // output arrives with the result.
                        if active.output.len() > MAX_EXEC_STREAM_BYTES {
                            let cut = active.output.len() - MAX_EXEC_STREAM_BYTES;
                            let cut = active.output.ceil_char_boundary(cut);
                            active.output.drain(..cut);
                        }
                    }
                    self.relayout_active_running();
                }
            }
            UiEvent::ToolAutoApproved(call) => {
                self.record_approval(&call, ApprovalDecision::AllowAlways);
            }
            UiEvent::DiffPreview { call, diff } => {
                self.clear_active_tool_for_preview(&call);
                self.begin_block();
                self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
                self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
                    expanded: true,
                    title: tool_panel_title(&call),
                    meta: tool_panel_meta(&call),
                    right: vec![
                        ("●".to_string(), dim_style()),
                        (" PREVIEW     ".to_string(), panel_style()),
                    ],
                }));
                self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
                self.rows.extend(diff_table_rows(&diff));
                self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
            }
            UiEvent::ToolDenied(call) => {
                self.begin_block();
                let mut spans = vec![Span::styled("✗", err_style()), Span::raw(" Denied ")];
                spans.extend(ansi_spans(&run_target(&call), Style::default()));
                self.push_approval_panel(Line::from(spans), true);
            }
            UiEvent::ToolResult {
                call,
                content,
                exit_code,
                duration,
            } => {
                if self
                    .active_exec
                    .as_ref()
                    .is_some_and(|a| a.call.id == call.id)
                {
                    self.finalize_active(&call, &content, exit_code, duration);
                } else if !self.finalize_active_tool(&call, &content, duration) {
                    self.push_tool_result(&call, &content, exit_code, duration);
                }
            }
            UiEvent::ToolError { call, message } => {
                if self
                    .active_exec
                    .as_ref()
                    .is_some_and(|a| a.call.id == call.id)
                {
                    self.finalize_active_error(&call, &message);
                } else if self.finalize_active_tool_error(&call, &message) {
                } else if !is_exploration_tool(&call) || !self.push_explored_error(&call, &message)
                {
                    self.push_tool_error(&call, &message);
                }
            }
            UiEvent::Notice(message) => {
                self.begin_block();
                self.push(&format!("note: {message}"), dim_style());
            }
            UiEvent::TurnError { kind, message } => {
                self.begin_block();
                match kind {
                    TurnErrorKind::Auth => {
                        self.push(&format!("auth error: {message}"), err_style());
                        self.push(
                            "authentication required; re-run the login command",
                            err_style(),
                        );
                    }
                    TurnErrorKind::Provider => {
                        self.push(&format!("provider error: {message}"), err_style());
                    }
                }
            }
            UiEvent::TurnComplete => {
                self.finish_stream();
            }
        }
        self.trim_history();
    }

    fn trim_history(&mut self) {
        if self.rows.len() <= MAX_TRANSCRIPT_ROWS
            || self.streaming.is_some()
            || self.active_exec.is_some()
            || self.active_tool.is_some()
            || !self.active_explorations.is_empty()
        {
            return;
        }
        let remove = self.panel_safe_trim_index(self.rows.len() - MAX_TRANSCRIPT_ROWS);
        self.rows.drain(..remove);
        self.exploring_open = self.trailing_explore_panel_open();
    }

    fn panel_safe_trim_index(&self, min_remove: usize) -> usize {
        let mut remove = min_remove.min(self.rows.len());
        while remove < self.rows.len() && self.row_is_inside_panel(remove) {
            remove = self.panel_end_from(remove);
        }
        remove
    }

    fn row_is_inside_panel(&self, index: usize) -> bool {
        matches!(
            self.rows.get(index).and_then(|row| row.chrome.as_ref()),
            Some(
                ChromeRow::Header { .. }
                    | ChromeRow::Separator
                    | ChromeRow::Body { .. }
                    | ChromeRow::Bottom
            )
        )
    }

    fn trailing_explore_panel_open(&self) -> bool {
        let Some(last) = self.rows.iter().rposition(|row| !is_separator_row(row)) else {
            return false;
        };
        if !matches!(self.rows[last].chrome.as_ref(), Some(ChromeRow::Bottom)) {
            return false;
        }
        for row in self.rows[..=last].iter().rev() {
            match row.chrome.as_ref() {
                Some(ChromeRow::Header { title, .. }) => return *title == "EXPLORE",
                Some(ChromeRow::Top) => return false,
                _ => {}
            }
        }
        false
    }

    /// Commit a submitted prompt into the transcript as plain pane text.
    /// This is display-only; the raw prompt still goes to Nexus unchanged
    /// through the loop.
    fn commit_user(&mut self, text: &str) {
        self.push_blank();
        pane::push_user_rows(&mut self.rows, text);
        self.trim_history();
    }

    fn render(&mut self, width: u16) -> Vec<Line<'static>> {
        let width = usize::from(width);
        self.last_width = width
            .saturating_sub(TEXT_COLUMN_X_PADDING.saturating_mul(2))
            .max(1);
        let mut rows = Vec::new();
        for row in &self.rows {
            row.render(width, &mut rows);
        }
        if let Some(text) = &self.streaming {
            pane::render_streaming_assistant(width, text, &mut rows);
        }
        rows
    }
}

fn streaming_markdown_preview(text: &str) -> String {
    if text.len() <= MAX_STREAMING_MARKDOWN_BYTES {
        return text.to_string();
    }
    let start = text.ceil_char_boundary(text.len() - MAX_STREAMING_MARKDOWN_BYTES);
    format!(
        "… streaming preview truncated; showing latest content …\n{}",
        &text[start..]
    )
}

fn tool_output_line(prefix: &'static str, line: &str) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix, dim_style())];
    spans.extend(ansi_spans(line, dim_style()));
    Line::from(spans)
}

fn approval_line(call: &ToolCall, scope: &str) -> Line<'static> {
    let mut spans = vec![
        Span::styled("✔", ok_style()),
        Span::raw(" You approved iris to run "),
    ];
    spans.extend(ansi_spans(&run_target(call), Style::default()));
    spans.push(Span::raw(format!(" {scope}")));
    Line::from(spans)
}

fn approval_status_line(hint: &ApprovalHint) -> Line<'static> {
    let mut spans = vec![Span::raw("approve ")];
    spans.extend(ansi_spans(&hint.target, Style::default()));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(hint.options, dim_style()));
    Line::from(spans)
}

fn approval_status_lines(hint: &ApprovalHint, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let full = approval_status_line(hint);
    if display_width(&line_text(&full)) <= width {
        return vec![full];
    }

    let mut target = vec![Span::raw("approve ")];
    target.extend(ansi_spans(&hint.target, Style::default()));
    let mut lines = Vec::new();
    push_wrapped_line(&Line::from(target), width, Some("  "), &mut lines);
    push_wrapped_line(
        &Line::from(Span::styled(hint.options, dim_style())),
        width,
        Some("  "),
        &mut lines,
    );
    lines
}

fn ansi_spans(text: &str, default_style: Style) -> Vec<Span<'static>> {
    if let Ok(parsed_text) = text.into_text() {
        // Flatten any parsed sub-lines (a stray \r can split one input line)
        // so no styled content is dropped.
        let mut spans = Vec::new();
        for parsed in parsed_text.lines {
            for mut span in parsed.spans {
                let content = strip_ansi_for_text(span.content.as_ref());
                if content.is_empty() {
                    continue;
                }
                span.content = content.into();
                if matches!(span.style.fg, None | Some(Color::Reset))
                    && let Some(fg) = default_style.fg
                {
                    span.style = span.style.fg(fg);
                }
                spans.push(span);
            }
        }
        if !spans.is_empty() {
            return spans;
        }
    }
    vec![Span::styled(strip_ansi_for_text(text), default_style)]
}

fn strip_ansi_for_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => consume_csi(&mut chars),
                Some(']' | 'P' | '^' | '_' | 'X') => consume_string_control(&mut chars),
                Some(_) | None => {}
            },
            '\u{009b}' => consume_csi(&mut chars),
            '\u{009d}' | '\u{0090}' | '\u{009e}' | '\u{009f}' | '\u{0098}' => {
                consume_string_control(&mut chars);
            }
            _ if ch.is_control() => {}
            _ => out.push(ch),
        }
    }
    out
}

fn consume_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for ch in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&ch) {
            break;
        }
    }
}

fn consume_string_control(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(ch) = chars.next() {
        if ch == '\u{7}' {
            break;
        }
        if ch == '\x1b' && matches!(chars.peek(), Some('\\')) {
            chars.next();
            break;
        }
    }
}

/// Animated turn-progress spinner. Advances only while `active`, so an idle
/// session redraws nothing on a tick (no flicker, no busy CPU). `started`
/// timestamps the turn so the status row can show elapsed time and the turn-end
/// rule can report "Worked for ...".
#[derive(Default)]
struct Spinner {
    active: bool,
    frame: usize,
    started: Option<Instant>,
}

struct ApprovalHint {
    target: String,
    options: &'static str,
}

impl Spinner {
    fn start(&mut self) {
        self.active = true;
        self.frame = 0;
        self.started = Some(Instant::now());
    }

    fn stop(&mut self) {
        self.active = false;
    }

    /// Wall-clock time since the turn began, or `None` before the first turn.
    fn elapsed(&self) -> Option<Duration> {
        self.started.map(|start| start.elapsed())
    }

    /// Advance one frame; a no-op when idle so ticks cause no redraw at rest.
    fn tick(&mut self) -> bool {
        if self.active {
            self.frame = (self.frame + 1) % SPINNER_FRAMES.len();
        }
        self.active
    }

    fn glyph(&self) -> &'static str {
        SPINNER_FRAMES[self.frame % SPINNER_FRAMES.len()]
    }
}

/// Session rail metadata.
struct Footer {
    /// Model display token.
    model: String,
    /// Reasoning effort display token, when configured.
    effort: Option<String>,
    /// Working directory, home-relativized to `~` where possible.
    cwd: String,
    /// Latest provider-reported usage, if the provider surfaced it.
    usage: Option<ProviderUsage>,
}

fn content_width(width: usize) -> usize {
    width
        .saturating_sub(TEXT_COLUMN_X_PADDING.saturating_mul(2))
        .max(1)
}

#[cfg(test)]
fn pad_content_lines(lines: &mut [Line<'static>]) {
    for line in lines {
        if !line_text(line).is_empty() {
            pad_line_left(line, TEXT_COLUMN_X_PADDING);
        }
    }
}

#[cfg(test)]
fn footer_lines(footer: &Footer, width: usize) -> Vec<Line<'static>> {
    let width = content_width(width);
    let model = truncate_to_width(&footer.model, width);
    let model_width = display_width(&model);
    let usage = footer.usage.as_ref().map(footer_usage_text);
    let usage = usage.as_deref().unwrap_or_default();
    let usage_max = width.saturating_sub(model_width).saturating_sub(1);
    let usage = if usage_max > 0 {
        truncate_to_width(usage, usage_max)
    } else {
        String::new()
    };
    let usage_width = display_width(&usage);
    let pad = width
        .saturating_sub(usage_width)
        .saturating_sub(model_width);

    let mut second = Vec::new();
    if !usage.is_empty() {
        second.push(Span::styled(usage, dim_style()));
    }
    second.push(Span::raw(" ".repeat(pad)));
    second.push(Span::styled(model, Style::default().fg(Color::Cyan)));

    vec![
        Line::from(Span::styled(
            truncate_to_width(&footer.cwd, width),
            Style::default().fg(Color::Green),
        )),
        Line::from(second),
    ]
}

#[cfg(test)]
fn footer_usage_text(usage: &ProviderUsage) -> String {
    let mut text = format!("{} tokens", compact_count(usage.total_tokens));
    if usage.cache_read_input_tokens > 0 {
        text.push_str(&format!(
            " ({} cached)",
            compact_count(usage.cache_read_input_tokens)
        ));
    }
    text
}

fn compact_count(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}m", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn working_lines(
    glyph: &str,
    elapsed: Option<Duration>,
    footer: Option<&Footer>,
    width: usize,
) -> Vec<Line<'static>> {
    let secs = elapsed.unwrap_or_default().as_secs();
    let mut details = vec![format_elapsed_compact(secs), "esc to interrupt".to_string()];
    if let Some(usage) = footer.and_then(|footer| footer.usage.as_ref()) {
        details.push(format!("↓ {} tokens", compact_count(usage.total_tokens)));
    }
    if let Some(effort) = footer.and_then(|footer| footer.effort.as_ref()) {
        details.push(format!("thinking with {effort} effort"));
    }
    let mut line = Line::from(vec![
        Span::styled(format!("{glyph} "), prompt_style()),
        Span::styled("Working…", dim_style()),
        Span::styled(format!(" ({})", details.join(" · ")), dim_style()),
    ]);
    truncate_line(&mut line, content_width(width));
    pad_line_left(
        &mut line,
        TEXT_COLUMN_X_PADDING.min(width.saturating_sub(1)),
    );
    truncate_line(&mut line, width.max(1));
    vec![Line::default(), line, Line::default()]
}

/// Build a styled, empty editor for the bordered composer panel: dim
/// placeholder and a reversed block cursor the widget draws itself (no hardware
/// cursor needed). The surrounding border and hint row are painted by
/// `render_editor_chrome`.
fn fresh_editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_wrap_mode(WrapMode::WordOrGlyph);
    editor.set_cursor_line_style(Style::default());
    editor.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    editor.set_placeholder_style(dim_style());
    editor.set_placeholder_text("Give iris a task...");
    editor
}

fn editor_visual_rows(editor: &TextArea<'_>, width: u16) -> u16 {
    let inner_width = usize::from(
        width
            .saturating_sub(BOX_X_PADDING_U16.saturating_mul(2))
            .saturating_sub(TEXT_X_PADDING_U16.saturating_mul(2))
            .max(1),
    );
    editor
        .lines()
        .iter()
        .map(|line| u16::try_from(wrap_to_width(line, inner_width).len()).unwrap_or(u16::MAX))
        .sum::<u16>()
        .clamp(1, MAX_EDITOR_ROWS)
}

/// UI state plus its rendering. Holds no terminal handle and no channels, so its
/// behavior and rendered logical document are unit-testable without a TTY.
pub(crate) struct Screen {
    transcript: Transcript,
    /// Multiline editor buffer (undo/redo, kill-ring, word-nav) owned by
    /// `ratatui-textarea`; the loop drives it from Iris's own keymap.
    pub(crate) editor: TextArea<'static>,
    /// Slash-command palette selection state, synced after every edit.
    pub(crate) palette: Palette,
    spinner: Spinner,
    /// Short status-row hint while a gated tool awaits the user's decision.
    approval_hint: Option<ApprovalHint>,
    /// Sourced global status chrome (model / effort / cwd). The loop refreshes
    /// it from the live model selection; `None` falls back to the composer hint
    /// (e.g. before a provider is selected).
    footer: Option<Footer>,
    /// The active picker/dialog, when one is open. While present it renders
    /// above the editor and the loop routes keys to it instead of the editor.
    pub(crate) modal: Option<Modal>,
}

impl Screen {
    pub(crate) fn new() -> Self {
        Self {
            transcript: Transcript::default(),
            editor: fresh_editor(),
            palette: Palette::default(),
            spinner: Spinner::default(),
            approval_hint: None,
            footer: None,
            modal: None,
        }
    }

    // --- modal/picker ---

    /// Open a picker/dialog above the editor until it closes.
    pub(crate) fn open_modal(&mut self, modal: Modal) {
        self.modal = Some(modal);
    }

    /// Close the active picker and restore the editor.
    pub(crate) fn close_modal(&mut self) {
        self.modal = None;
    }

    /// Whether a picker/dialog is currently open.
    pub(crate) fn modal_open(&self) -> bool {
        self.modal.is_some()
    }

    // --- transcript ---

    /// Apply one semantic event to the transcript. The loop calls this for every
    /// Nexus event; history remains in Iris state so the terminal surface can
    /// replay/reflow it after a resize.
    pub(crate) fn apply_event(&mut self, event: UiEvent) {
        self.apply(event);
    }

    /// Apply one semantic event to the transcript.
    pub(crate) fn apply(&mut self, event: UiEvent) {
        if let UiEvent::ProviderTurnCompleted {
            usage: Some(usage), ..
        } = &event
            && let Some(footer) = &mut self.footer
        {
            footer.usage = Some(usage.clone());
        }
        self.transcript.apply(event);
    }

    /// Commit a submitted prompt into the transcript as a user line.
    pub(crate) fn commit_user(&mut self, text: &str) {
        self.transcript.commit_user(text);
    }

    /// Render all transcript rows plus any in-flight stream, wrapped to `width`.
    /// Finalized history is intentionally retained here; the terminal surface
    /// owns append/diff/full-replay decisions instead of draining UI state.
    fn wrapped_lines(&mut self, width: u16) -> Vec<Line<'static>> {
        self.transcript.render(width)
    }

    // --- editor ---

    /// Whole editor text with logical newlines.
    pub(crate) fn editor_text(&self) -> String {
        self.editor.lines().join("\n")
    }

    /// True when the editor holds nothing (one empty line).
    pub(crate) fn editor_is_empty(&self) -> bool {
        let lines = self.editor.lines();
        lines.len() == 1 && lines[0].is_empty()
    }

    /// Re-sync the palette open-state/selection after the editor changed.
    pub(crate) fn sync_palette(&mut self) {
        let text = self.editor_text();
        self.palette.sync(&text);
    }

    /// Take the current editor text and reset to a fresh empty editor.
    pub(crate) fn submit(&mut self) -> String {
        let text = self.editor_text();
        self.editor = fresh_editor();
        self.palette.sync("");
        text
    }

    /// Clear the editor without submitting (Ctrl-C on non-empty input).
    pub(crate) fn clear_editor(&mut self) {
        self.editor = fresh_editor();
        self.palette.sync("");
    }

    /// Replace the editor contents with `text` (palette command completion).
    pub(crate) fn set_editor(&mut self, text: &str) {
        let mut editor = fresh_editor();
        editor.insert_str(text);
        self.editor = editor;
        self.sync_palette();
    }

    // --- spinner / turn state ---

    /// Set (or refresh) the idle footer from the live model selection. The loop
    /// calls this whenever the model/effort changes; `cwd` is home-relativized.
    pub(crate) fn set_footer(&mut self, model: String, effort: Option<String>, cwd: String) {
        let usage = self.footer.as_ref().and_then(|footer| footer.usage.clone());
        self.footer = Some(Footer {
            model,
            effort,
            cwd,
            usage,
        });
    }

    pub(crate) fn start_turn(&mut self) {
        self.spinner.start();
        self.approval_hint = None;
    }

    pub(crate) fn end_turn(&mut self) {
        let elapsed = self.spinner.elapsed();
        self.spinner.stop();
        self.approval_hint = None;
        let _ = elapsed;
    }

    /// Advance the spinner one frame. Returns whether anything animated (so the
    /// loop only redraws on a tick while a turn is running). While an approval is
    /// shown the spinner is hidden behind the hint, so a tick changes nothing and
    /// requests no redraw -- the loop stays CPU-idle waiting on the decision.
    pub(crate) fn tick(&mut self) -> bool {
        if self.approval_hint.is_some() {
            return false;
        }
        self.spinner.tick()
    }

    // --- approval ---

    /// Show a gated tool's approval prompt in the status row. The transcript
    /// records the final approval/denial outcome, not the transient prompt.
    pub(crate) fn show_approval(&mut self, call: &ToolCall, allow_always: bool) {
        let options = if allow_always {
            "[y] once  [a] always  [N] deny"
        } else {
            "[y] once  [N] deny"
        };
        self.approval_hint = Some(ApprovalHint {
            target: run_target(call),
            options,
        });
    }

    pub(crate) fn record_approval(&mut self, call: &ToolCall, decision: ApprovalDecision) {
        self.transcript.record_approval(call, decision);
    }

    pub(crate) fn clear_approval(&mut self) {
        self.approval_hint = None;
    }

    /// Status row content: approval hint > footer > idle hint.
    #[cfg(test)]
    fn status_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = if let Some(hint) = &self.approval_hint {
            approval_status_lines(hint, content_width(usize::from(width)))
        } else if let Some(footer) = &self.footer {
            footer_lines(footer, usize::from(width))
        } else {
            vec![Line::from(Span::styled(
                truncate_to_width(COMPOSER_HINT, content_width(usize::from(width))),
                dim_style(),
            ))]
        };
        pad_content_lines(&mut lines);
        lines
    }

    fn working_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.spinner.active && self.approval_hint.is_none() {
            working_lines(
                self.spinner.glyph(),
                self.spinner.elapsed(),
                self.footer.as_ref(),
                usize::from(width),
            )
        } else {
            Vec::new()
        }
    }
}

/// Render a unified diff as the edit-tool table from the visual spec:
/// old/new line columns, a marker column, then code. File headers and hunk
/// headers are structural data, not visible rows.
fn diff_table_rows(diff: &str) -> Vec<TranscriptRow> {
    let mut out = Vec::new();
    let mut old_line = 0usize;
    let mut new_line = 0usize;
    for line in diff.lines() {
        if line.starts_with("--- ") || line.starts_with("+++ ") {
            continue;
        }
        if let Some((old_start, new_start)) = parse_hunk_header(line) {
            old_line = old_start;
            new_line = new_start;
            continue;
        }
        let Some(marker) = line.chars().next() else {
            continue;
        };
        let code = line.get(marker.len_utf8()..).unwrap_or_default();
        let (old, new, marker_text, style, bg) = match marker {
            '-' => {
                let row = (Some(old_line), None, "-", err_style(), Some(DIFF_DEL_BG));
                old_line += 1;
                row
            }
            '+' => {
                let row = (None, Some(new_line), "+", ok_style(), Some(DIFF_ADD_BG));
                new_line += 1;
                row
            }
            ' ' => {
                let row = (Some(old_line), Some(new_line), " ", panel_style(), None);
                old_line += 1;
                new_line += 1;
                row
            }
            _ => continue,
        };
        let row = format_diff_table_row(old, new, marker_text, code);
        out.push(TranscriptRow::chrome_with_text(
            ChromeRow::Body {
                line: Line::from(Span::styled(row.clone(), style)),
                bg,
            },
            row,
            style,
        ));
    }
    out
}

fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_part, rest) = rest.split_once(" +")?;
    let (new_part, _) = rest.split_once(" @@")?;
    Some((parse_hunk_start(old_part), parse_hunk_start(new_part)))
}

fn parse_hunk_start(part: &str) -> usize {
    part.split(',')
        .next()
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(0)
}

fn format_diff_table_row(
    old: Option<usize>,
    new: Option<usize>,
    marker: &str,
    code: &str,
) -> String {
    let old = old.map_or_else(String::new, |line| line.to_string());
    let new = new.map_or_else(String::new, |line| line.to_string());
    format!("{old:>4}   {new:>7}  {marker}  |  {code}")
}

/// Greedy word-wrap `text` to `width` display columns, breaking at spaces. A
/// word that fits is moved whole onto its own row rather than split mid-token,
/// so a URL/path that fits within the width stays selectable as one unit; a
/// single word longer than the width still hard-breaks, because the row-exact
/// rendering model (one logical row = one physical row, see [`TuiUi::draw`])
/// cannot emit an over-wide row without the terminal clipping its tail.
/// Returns at least one row (possibly empty) so a blank logical line still
/// occupies a row.
pub(crate) fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 || display_width(text) <= width {
        return vec![text.to_string()];
    }
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
    for (i, word) in text.split(' ').enumerate() {
        let word_w = display_width(word);
        if i > 0 && !cur.is_empty() && cur_w + 1 + word_w <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + word_w;
            continue;
        }
        if !cur.is_empty() {
            rows.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if word_w <= width {
            cur.push_str(word);
            cur_w = word_w;
        } else {
            for ch in word.chars() {
                let cw = char_width(ch);
                // Guard `!cur.is_empty()` so a single glyph wider than the
                // whole width never emits a phantom blank row before itself.
                if cur_w + cw > width && !cur.is_empty() {
                    rows.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push(ch);
                cur_w += cw;
            }
        }
    }
    rows.push(cur);
    rows
}

/// Render the slash popup into an offscreen Ratatui buffer: plain above-composer
/// rows with the selected row accented by foreground color only.
fn render_palette(buf: &mut Buffer, area: Rect, matches: &[&SlashCommand], selected: usize) {
    let inner = Rect {
        x: area.x + u16::try_from(TEXT_COLUMN_X_PADDING).unwrap_or(u16::MAX),
        y: area.y + u16::from(area.height > 1),
        width: area
            .width
            .saturating_sub(
                u16::try_from(TEXT_COLUMN_X_PADDING.saturating_mul(2)).unwrap_or(u16::MAX),
            )
            .max(1),
        height: area.height.saturating_sub(2).max(1),
    };
    let mut rows = Vec::new();
    let command_width = matches
        .iter()
        .map(|cmd| display_width(cmd.name))
        .max()
        .unwrap_or(0);
    for (i, cmd) in matches.iter().enumerate() {
        let selected_row = i == selected;
        let name_style = if selected_row {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };
        let description_style = if selected_row {
            Style::default().fg(Color::Cyan)
        } else {
            dim_style()
        };
        let gap = command_width
            .saturating_sub(display_width(cmd.name))
            .saturating_add(2);
        rows.push(Line::from(vec![
            Span::styled(cmd.name.to_string(), name_style),
            Span::raw(" ".repeat(gap)),
            Span::styled(cmd.description, description_style),
        ]));
    }
    Paragraph::new(Text::from(rows)).render(inner, buf);
}

fn render_plain_menu_lines(buf: &mut Buffer, area: Rect, lines: Vec<Line<'static>>) {
    let inner = Rect {
        x: area.x + u16::try_from(TEXT_COLUMN_X_PADDING).unwrap_or(u16::MAX),
        y: area.y + u16::from(area.height > 1),
        width: area
            .width
            .saturating_sub(
                u16::try_from(TEXT_COLUMN_X_PADDING.saturating_mul(2)).unwrap_or(u16::MAX),
            )
            .max(1),
        height: area.height.saturating_sub(2).max(1),
    };
    Paragraph::new(Text::from(lines)).render(inner, buf);
}

/// Render the full logical document for the current terminal size: all
/// transcript rows retained in Iris state, plus bottom-pinned
/// menu/status/editor chrome. The terminal surface decides how much of this
/// document can be patched and when it must be fully replayed.
#[cfg(test)]
fn render_document(screen: &mut Screen, size: Size) -> Vec<Line<'static>> {
    render_document_with_chrome_tail(screen, size).0
}

fn render_document_with_chrome_tail(
    screen: &mut Screen,
    size: Size,
) -> (Vec<Line<'static>>, usize) {
    if size.height == 0 || size.width < 1 {
        return (Vec::new(), 0);
    }
    let width = size.width.max(1);
    let height = size.height.max(1);
    let mut transcript = screen.wrapped_lines(width);
    let chrome = render_editor_chrome(screen, width, height);
    let chrome_len = chrome.len();
    let target_rows = height.min(MIN_INLINE_DOCUMENT_ROWS);
    let min_transcript_rows = usize::from(target_rows).saturating_sub(chrome.len());
    if transcript.len() < min_transcript_rows {
        let mut padded = Vec::with_capacity(min_transcript_rows + chrome.len());
        padded.extend(
            std::iter::repeat_with(Line::default).take(min_transcript_rows - transcript.len()),
        );
        padded.extend(transcript);
        transcript = padded;
    }
    transcript.extend(chrome);
    (transcript, chrome_len)
}

fn global_status_line(screen: &Screen, width: u16) -> Option<Line<'static>> {
    let footer = screen.footer.as_ref()?;
    let (cwd, branch) = split_cwd_branch(&footer.cwd);
    let mut spans = vec![
        Span::raw(" ".repeat(BOX_X_PADDING)),
        Span::styled("● ", prompt_style()),
        Span::styled("MODE ", panel_style()),
        Span::styled("code", dim_style()),
        rail_sep(),
        Span::styled("MODEL ", panel_style()),
        Span::styled(strip_ansi_for_text(&footer.model), dim_style()),
    ];
    if !screen.spinner.active
        && let Some(effort) = &footer.effort
    {
        spans.push(rail_sep());
        spans.push(Span::styled("EFFORT ", panel_style()));
        spans.push(Span::styled(strip_ansi_for_text(effort), dim_style()));
    }
    spans.push(rail_sep());
    spans.push(Span::styled("CWD ", panel_style()));
    spans.push(Span::styled(strip_ansi_for_text(&cwd), dim_style()));
    if let Some(branch) = branch {
        spans.push(rail_sep());
        spans.push(Span::styled("BRANCH ", panel_style()));
        spans.push(Span::styled(strip_ansi_for_text(&branch), dim_style()));
    }
    let mut line = Line::from(std::mem::take(&mut spans));
    truncate_line(&mut line, usize::from(width).max(1));
    Some(line)
}

fn rail_sep() -> Span<'static> {
    Span::styled("  ┊  ", dim_style())
}

fn split_cwd_branch(cwd: &str) -> (String, Option<String>) {
    if let Some((left, right)) = cwd.rsplit_once(" (")
        && let Some(branch) = right.strip_suffix(')')
    {
        return (left.to_string(), Some(branch.to_string()));
    }
    (cwd.to_string(), None)
}

#[derive(Clone, Copy)]
struct ChromeHeights {
    menu: u16,
    working: u16,
    editor: u16,
}

fn chrome_heights(
    height: u16,
    menu_wanted: u16,
    desired_global_status_h: u16,
    desired_working_h: u16,
    editor_rows: u16,
) -> ChromeHeights {
    let global_status = desired_global_status_h.min(GLOBAL_STATUS_H);
    let working = if desired_working_h > 0
        && height
            >= MIN_EDITOR_H
                .saturating_add(global_status)
                .saturating_add(desired_working_h)
    {
        desired_working_h
    } else {
        0
    };
    let menu = menu_wanted.min(
        height
            .saturating_sub(MIN_EDITOR_H)
            .saturating_sub(global_status)
            .saturating_sub(working),
    );
    let max_editor_h = height
        .saturating_sub(menu)
        .saturating_sub(global_status)
        .saturating_sub(working)
        .max(1);
    let wanted_editor_h = editor_rows.saturating_add(4);
    let editor = if max_editor_h >= MIN_EDITOR_H {
        wanted_editor_h.clamp(MIN_EDITOR_H, max_editor_h)
    } else {
        max_editor_h.max(1)
    };
    ChromeHeights {
        menu,
        working,
        editor,
    }
}

fn render_editor_chrome(screen: &mut Screen, width: u16, height: u16) -> Vec<Line<'static>> {
    let area = Rect::new(0, 0, width, height);

    let editor_rows = screen.approval_hint.as_ref().map_or_else(
        || editor_visual_rows(&screen.editor, area.width),
        |hint| {
            let inner_width = area
                .width
                .saturating_sub(BOX_X_PADDING_U16.saturating_mul(2))
                .saturating_sub(TEXT_X_PADDING_U16.saturating_mul(2))
                .max(1);
            u16::try_from(approval_status_lines(hint, usize::from(inner_width)).len())
                .unwrap_or(u16::MAX)
                .clamp(1, MAX_EDITOR_ROWS)
        },
    );
    let input_text = screen.editor_text();
    let modal_lines = screen.modal.as_ref().map(|modal| {
        modal.render(u16::try_from(content_width(usize::from(area.width))).unwrap_or(u16::MAX))
    });
    let palette_active = modal_lines.is_none() && screen.palette.is_active(&input_text);
    let palette_matches: Vec<&SlashCommand> = if palette_active {
        slash::matches(&input_text)
    } else {
        Vec::new()
    };
    let menu_wanted = if let Some(lines) = &modal_lines {
        u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(MAX_MENU_ROWS)
    } else if palette_active {
        (palette_matches.len() as u16 + 2).min(MAX_MENU_ROWS)
    } else {
        0
    };

    // Bottom-anchored, clamped to the fixed viewport. The editor owns its hint
    // row inside the border; there is no bottom status bar in the spec.
    // Explicit global chrome, when sourced, is outside transcript pane rows,
    // tool panels, and the composer body. There is no bottom telemetry bar.
    let global_status_lines: Vec<Line<'static>> =
        global_status_line(screen, area.width).into_iter().collect();
    let global_status_h = u16::try_from(global_status_lines.len()).unwrap_or(GLOBAL_STATUS_H);
    let working_lines = screen.working_lines(area.width);
    let heights = chrome_heights(
        area.height,
        menu_wanted,
        global_status_h,
        u16::try_from(working_lines.len()).unwrap_or(u16::MAX),
        editor_rows,
    );
    let chrome_h = heights
        .menu
        .saturating_add(global_status_h)
        .saturating_add(heights.working)
        .saturating_add(heights.editor);
    let chrome_area = Rect::new(0, 0, width, chrome_h.max(1));
    let chunks = Layout::vertical([
        Constraint::Length(heights.menu),
        Constraint::Length(global_status_h),
        Constraint::Length(heights.working),
        Constraint::Length(heights.editor),
    ])
    .split(chrome_area);
    let menu_area = chunks[0];
    let rail_area = chunks[1];
    let working_area = chunks[2];
    let editor_area = chunks[3];

    let mut buf = Buffer::empty(chrome_area);

    if heights.menu > 0 {
        if let Some(lines) = modal_lines {
            render_plain_menu_lines(&mut buf, menu_area, lines);
        } else {
            render_palette(
                &mut buf,
                menu_area,
                &palette_matches,
                screen.palette.selected(),
            );
        }
    }
    if global_status_h > 0 {
        Paragraph::new(Text::from(global_status_lines)).render(rail_area, &mut buf);
    }
    if heights.working > 0 {
        Paragraph::new(Text::from(working_lines)).render(working_area, &mut buf);
    }

    let box_area = Rect {
        x: editor_area.x + BOX_X_PADDING_U16.min(editor_area.width.saturating_sub(1)),
        y: editor_area.y,
        width: editor_area
            .width
            .saturating_sub(BOX_X_PADDING_U16 * 2)
            .max(1),
        height: editor_area.height,
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(border_style())
        .render(box_area, &mut buf);
    let text_area = Rect {
        x: box_area.x + 2.min(box_area.width.saturating_sub(1)),
        y: editor_area.y + 1,
        width: box_area.width.saturating_sub(TEXT_X_PADDING_U16 * 2).max(1),
        height: editor_area.height.saturating_sub(3).max(1),
    };
    if let Some(hint) = &screen.approval_hint {
        let approval_lines = approval_status_lines(hint, usize::from(text_area.width));
        Paragraph::new(Text::from(approval_lines)).render(text_area, &mut buf);
    } else {
        (&screen.editor).render(text_area, &mut buf);
    }
    let hint = if screen.approval_hint.is_some() {
        Line::default()
    } else {
        Line::from(Span::styled(COMPOSER_HINT, dim_style()))
    };
    let hint_area = Rect {
        x: box_area.x + 2.min(box_area.width.saturating_sub(1)),
        y: box_area.y.saturating_add(box_area.height.saturating_sub(2)),
        width: box_area.width.saturating_sub(4).max(1),
        height: 1,
    };
    Paragraph::new(Text::from(vec![hint])).render(hint_area, &mut buf);
    buffer_to_lines(&buf)
}

fn buffer_to_lines(buf: &Buffer) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for y in 0..buf.area.height {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            let style = cell.style();
            if let Some(last) = spans.last_mut()
                && last.style == style
            {
                last.content.to_mut().push_str(cell.symbol());
                continue;
            }
            spans.push(Span::styled(cell.symbol().to_string(), style));
        }
        out.push(Line::from(spans));
    }
    out
}

/// Terminal driver: owns raw mode, paste/key flags, cursor visibility, terminal
/// size reads, and the Iris terminal surface for the whole interactive session.
/// It does NOT enter the alternate screen and does not use Ratatui `Terminal`:
/// [`crate::ui::tui_loop`] feeds it events and calls [`TuiUi::draw`].
pub(crate) struct TuiUi {
    surface: TerminalSurface<Stdout>,
    pub(crate) screen: Screen,
    active: bool,
}

impl TuiUi {
    /// Enter raw mode ONCE, enable bracketed paste + modified-key reporting,
    /// hide the hardware cursor, and create the Iris terminal surface. Mouse
    /// capture is deliberately NOT enabled so the terminal owns scroll/select/
    /// copy over the normal screen scrollback. Restored on `drop`/`shutdown`,
    /// and by the signal handler's emergency escape on a force-quit.
    pub(crate) fn new() -> Result<Self> {
        // Capture cooked-mode termios before raw mode so the force-quit signal
        // handler can restore the tty even though Drop will not run then.
        crate::signals::save_termios_for_force_quit();
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnableBracketedPaste,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            Hide,
        ) {
            let _ = execute!(
                stdout,
                DisableBracketedPaste,
                PopKeyboardEnhancementFlags,
                Show,
            );
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        crate::signals::enable_terminal_restore_on_force_quit();
        crate::telemetry::set_tui_active(true);
        Ok(Self {
            surface: TerminalSurface::new(stdout),
            screen: Screen::new(),
            active: true,
        })
    }

    pub(crate) fn draw(&mut self) -> Result<()> {
        let (width, height) = terminal_size()?;
        let size = Size::new(width.max(1), height.max(1));
        let (document, chrome_tail) = render_document_with_chrome_tail(&mut self.screen, size);
        self.surface
            .render_with_volatile_tail(size, &document, chrome_tail)?;
        Ok(())
    }

    fn restore(&mut self) {
        if self.active {
            // Replace the interactive chrome with transcript-only content so
            // the shell prompt resumes below conversation history, not below a
            // stale editor box.
            if let Ok((width, height)) = terminal_size() {
                let size = Size::new(width.max(1), height.max(1));
                let transcript = self.screen.wrapped_lines(size.width);
                let _ = self.surface.render(size, &transcript);
            }
            let _ = self.surface.finish();
            let _ = execute!(
                self.surface.writer_mut(),
                DisableBracketedPaste,
                PopKeyboardEnhancementFlags,
                Show,
            );
            let _ = disable_raw_mode();
            crate::signals::disable_terminal_restore_on_force_quit();
            crate::telemetry::set_tui_active(false);
            self.active = false;
        }
    }

    pub(crate) fn shutdown(&mut self) {
        self.restore();
    }
}

impl Drop for TuiUi {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::terminal_surface::{RenderKind, TerminalSurface};
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        call_args(name, json!({ "path": "note.txt", "content": "hi" }))
    }

    fn call_args(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
            name: name.to_string(),
            arguments,
        }
    }

    fn row_text(row: &TranscriptRow) -> String {
        row.text.clone()
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn line_signature(lines: &[Line<'static>]) -> Vec<Vec<(String, Option<Color>, Modifier)>> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| {
                        (
                            span.content.to_string(),
                            span.style.fg,
                            span.style.add_modifier,
                        )
                    })
                    .collect()
            })
            .collect()
    }

    fn line_matching<'a>(
        lines: &'a [Line<'static>],
        predicate: impl Fn(&Line<'static>) -> bool,
    ) -> &'a Line<'static> {
        lines.iter().find(|line| predicate(line)).expect("line")
    }

    fn span_matching<'a>(
        line: &'a Line<'static>,
        predicate: impl Fn(&Span<'static>) -> bool,
    ) -> &'a Span<'static> {
        line.spans
            .iter()
            .find(|span| predicate(span))
            .expect("span")
    }

    fn rendered_lines(screen: &mut Screen, width: u16, height: u16) -> Vec<Line<'static>> {
        render_document(screen, Size::new(width, height))
    }

    fn rendered_text(screen: &mut Screen, width: u16, height: u16) -> String {
        rendered_lines(screen, width, height)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
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
    fn streaming_deltas_commit_once_without_duplication() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextDelta("Hel".to_string()));
        screen.apply(UiEvent::AssistantTextDelta("lo".to_string()));
        assert_eq!(screen.transcript.rows.len(), 0);
        assert_eq!(screen.wrapped_lines(80).len(), 1);
        screen.apply(UiEvent::AssistantTextEnd("Hello".to_string()));
        let texts: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["Hello".to_string(), String::new()]);
    }

    #[test]
    fn empty_assistant_text_end_commits_accumulated_deltas() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextDelta("Hel".to_string()));
        screen.apply(UiEvent::AssistantTextDelta("lo".to_string()));

        screen.apply(UiEvent::AssistantTextEnd(String::new()));

        let texts: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert_eq!(texts, vec!["Hello".to_string(), String::new()]);
        assert!(screen.transcript.streaming.is_none());
    }

    #[test]
    fn wrap_breaks_long_line_at_spaces_and_hard_breaks_long_words() {
        assert_eq!(
            wrap_to_width("alpha beta gamma", 11),
            vec!["alpha beta".to_string(), "gamma".to_string()]
        );
        assert_eq!(
            wrap_to_width("abcdefgh", 3),
            vec!["abc".to_string(), "def".to_string(), "gh".to_string()]
        );
        assert_eq!(wrap_to_width("short", 80), vec!["short".to_string()]);
    }

    #[test]
    fn assistant_text_renders_with_marker_without_role_label() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText(
            "# Title\n\nuse `cargo test` and:\n- one\n- two".to_string(),
        ));
        let lines = screen.wrapped_lines(80);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>();
        let joined = rendered.join("\n");

        assert!(!joined.contains("AGENT"), "{joined}");
        assert!(!joined.contains("USER"), "{joined}");
        assert!(
            rendered.iter().any(|line| line.starts_with("    ● Title")),
            "{rendered:?}"
        );
        let title = line_matching(&lines, |line| line_text(line).contains("Title"));
        assert!(!line_text(title).contains('#'));
        assert!(
            title
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD)),
            "heading lost bold style: {title:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.contains("use `cargo test`"))
        );
        let code = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref().contains("cargo test"))
            .expect("inline code span");
        assert_eq!(code.style.fg, Some(Color::Cyan));
        assert!(rendered.iter().any(|line| line.trim_start() == "- one"));
        assert!(rendered.iter().any(|line| line.trim_start() == "- two"));
    }

    #[test]
    fn streaming_agent_text_renders_like_finalized_without_committing_early() {
        let markdown = "# Title\n\nuse `cargo test`\n\n- one";
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextDelta(markdown.to_string()));

        let live = screen.wrapped_lines(80);
        assert!(screen.transcript.rows.is_empty());
        let live_document = render_document(&mut screen, Size::new(80, 12))
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(live_document.contains("● Title"), "{live_document}");
        assert!(!live_document.contains("AGENT"), "{live_document}");
        assert!(live.iter().any(|l| line_text(l).contains("Title")));
        assert!(!live.iter().any(|l| line_text(l).contains("# Title")));
        assert!(live.iter().any(|l| line_text(l).contains("cargo test")));
        assert!(live.iter().any(|l| line_text(l).trim_start() == "- one"));

        screen.apply(UiEvent::AssistantTextEnd(markdown.to_string()));
        let finalized = screen.wrapped_lines(80);
        assert_eq!(
            line_signature(&live),
            line_signature(&finalized[..live.len()])
        );
        assert_eq!(
            line_signature(&finalized),
            line_signature(&screen.wrapped_lines(80))
        );
    }

    #[test]
    fn partial_streaming_markdown_renders_without_panic() {
        for markdown in ["```rust\nlet x = **", "half **bold"] {
            let mut screen = Screen::new();
            screen.apply(UiEvent::AssistantTextDelta(markdown.to_string()));
            let lines = screen.wrapped_lines(80);
            assert!(!lines.is_empty(), "partial markdown vanished: {markdown:?}");
            assert!(screen.transcript.rows.is_empty());
        }
    }

    #[test]
    fn url_that_fits_moves_to_its_own_row_unbroken() {
        // A URL that fits within the width is flushed whole onto its own row
        // (not split mid-token), so it stays selectable as one unit.
        let url = "https://ex.com/p";
        let rows = wrap_to_width(&format!("see {url} now"), 16);
        assert!(
            rows.iter().any(|r| r == url),
            "url not kept whole: {rows:?}"
        );
        assert!(rows.iter().any(|r| r == "see"));
        assert!(rows.iter().any(|r| r == "now"));
    }

    #[test]
    fn over_long_url_hard_breaks_without_losing_characters() {
        // A URL longer than the width must hard-break: the row-exact model
        // cannot emit an over-wide row without the terminal clipping its tail,
        // so visibility wins over keeping it on one row. Every character
        // survives -- concatenating the fragments rebuilds the URL exactly.
        let url = "https://example.com/very/long/path/that/exceeds/the/width";
        let rows = wrap_to_width(url, 20);
        assert!(rows.len() > 1, "expected a hard-break: {rows:?}");
        assert_eq!(
            rows.concat(),
            url,
            "characters lost while wrapping: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| display_width(r) <= 20),
            "a row exceeded the width (would clip on render): {rows:?}"
        );
    }

    #[test]
    fn long_plain_word_still_hard_breaks() {
        // A non-URL/path long token still hard-breaks (no overflow).
        assert_eq!(
            wrap_to_width("abcdefgh", 3),
            vec!["abc".to_string(), "def".to_string(), "gh".to_string()]
        );
    }

    #[test]
    fn long_transcript_line_wraps_to_multiple_rows() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText("alpha beta gamma delta".to_string()));
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|row| row.text == "alpha beta gamma delta")
        );
        assert!(screen.wrapped_lines(12).len() >= 2);
    }

    #[test]
    fn assistant_reply_gets_marker_text_padding_and_blank_rows() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText("alpha beta".to_string()));
        let lines = screen.wrapped_lines(16);

        assert_eq!(
            lines.iter().map(line_text).collect::<Vec<_>>(),
            vec![
                "    ● alpha".to_string(),
                "      beta".to_string(),
                String::new()
            ]
        );
        assert!(lines.iter().all(|line| line.style.bg.is_none()));
    }

    #[test]
    fn adjacent_user_and_assistant_turns_are_plain_with_one_separator() {
        let mut screen = Screen::new();
        screen.commit_user("HI");
        screen.apply(UiEvent::AssistantText(
            "Hi! What are you working on?".to_string(),
        ));
        let lines = screen.wrapped_lines(80);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>();
        let joined = rendered.join("\n");

        assert!(!joined.contains("USER"), "{joined}");
        assert!(!joined.contains("AGENT"), "{joined}");
        assert!(rendered.iter().any(|line| line == "    HI"), "{rendered:?}");
        let reply_idx = rendered
            .iter()
            .position(|line| line.contains("Hi! What"))
            .expect("assistant reply");
        assert_eq!(rendered[reply_idx - 1], "");
        assert!(
            rendered[reply_idx].starts_with("    ● Hi! What"),
            "{rendered:?}"
        );
        assert_eq!(lines[reply_idx].style.bg, None);
    }

    #[test]
    fn tool_output_preserves_ansi_color_spans() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "printf color" })),
            content: "\x1b[31mred\x1b[0m plain".to_string(),
            exit_code: None,
            duration: None,
        });
        let lines = screen.wrapped_lines(80);
        let output = line_matching(&lines, |line| line_text(line).contains("red plain"));
        assert!(line_text(output).contains("red plain"), "{output:?}");
        let red = span_matching(output, |span| span.content.as_ref() == "red");
        assert_eq!(red.style, Style::default().fg(Color::Red));
        let plain = span_matching(output, |span| span.content.as_ref() == " plain");
        assert_eq!(plain.style.fg, Some(MUTED));
    }

    #[test]
    fn panel_headers_and_plain_body_rows_strip_terminal_controls() {
        let mut screen = Screen::new();
        let command = "echo \u{1b}]0;owned\u{7}safe\u{1b}[31m red\u{1b}[0m\rboom";
        let file = "src/\u{1b}]0;owned\u{7}safe.rs";

        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": command })),
            content: "ok".to_string(),
            exit_code: None,
            duration: None,
        });
        screen.apply(UiEvent::ToolResult {
            call: call_args("edit", json!({ "file_path": file })),
            content: "patched".to_string(),
            exit_code: None,
            duration: None,
        });

        let rendered = screen
            .wrapped_lines(120)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(!rendered.contains('\u{7}'), "{rendered:?}");
        assert!(!rendered.contains('\r'), "{rendered:?}");
        assert!(!rendered.contains("owned"), "{rendered:?}");
        assert!(rendered.contains("echo safe redboom"), "{rendered:?}");
        assert!(rendered.contains("src/safe.rs"), "{rendered:?}");
    }

    #[test]
    fn ansi_tool_output_metadata_is_per_visible_line() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "printf lines" })),
            content: "\u{1b}[31mfirst\u{1b}[0m\n\u{1b}[32msecond\u{1b}[0m".to_string(),
            exit_code: None,
            duration: None,
        });

        let body_texts: Vec<&str> = screen
            .transcript
            .rows
            .iter()
            .filter(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::Body { .. })))
            .map(|row| row.text.as_str())
            .collect();

        assert!(
            body_texts.iter().any(|text| text.contains("first")),
            "{body_texts:?}"
        );
        assert!(
            body_texts.iter().any(|text| text.contains("second")),
            "{body_texts:?}"
        );
        assert!(
            body_texts
                .iter()
                .all(|text| !(text.contains("first") && text.contains("second"))),
            "each output row should carry only its own visible text: {body_texts:?}"
        );
    }

    #[test]
    fn single_over_budget_line_stays_within_row_cap() {
        // A single very long line must not blow past the physical-row cap: it is
        // clamped (with an ellipsis) to its slice budget instead of wrapping to
        // dozens of rows. Checked at narrow and normal widths.
        for width in [20u16, 80u16] {
            let mut screen = Screen::new();
            let _ = screen.wrapped_lines(width);
            screen.apply(UiEvent::ToolResult {
                call: call_args("bash", json!({ "command": "blob" })),
                content: "x".repeat(2000),
                exit_code: None,
                duration: None,
            });
            let texts: Vec<String> = screen.wrapped_lines(width).iter().map(line_text).collect();
            let output_rows = texts.iter().filter(|t| t.contains('x')).count();
            assert!(
                (1..=MAX_TOOL_OUTPUT_ROWS).contains(&output_rows),
                "width {width}: {output_rows} rows out of 1..={MAX_TOOL_OUTPUT_ROWS}: {texts:?}"
            );
            assert!(
                !texts.iter().any(|t| t.contains("+0 lines")),
                "width {width}: spurious +0 marker: {texts:?}"
            );
        }
    }

    #[test]
    fn live_single_over_budget_line_stays_within_row_cap() {
        // The live streaming cell must also clamp one very long line to the cap.
        let mut screen = Screen::new();
        screen.start_turn();
        let _ = screen.wrapped_lines(20);
        let call = call_args("bash", json!({ "command": "blob" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "y".repeat(2000),
        });
        let texts: Vec<String> = screen.wrapped_lines(20).iter().map(line_text).collect();
        let rows = texts.iter().filter(|t| t.contains('y')).count();
        assert!(
            (1..=MAX_TOOL_OUTPUT_ROWS).contains(&rows),
            "{rows} rows out of 1..={MAX_TOOL_OUTPUT_ROWS}: {texts:?}"
        );
    }

    #[test]
    fn ansi_tool_output_hard_wraps_without_dropping_chars() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "printf color" })),
            content: "\x1b[31mabcdefghijklmnopqrstuvwxyz\x1b[0m".to_string(),
            exit_code: None,
            duration: None,
        });
        // Narrow width forces the styled row across multiple physical lines.
        let lines = screen.wrapped_lines(10);
        let red: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.style.fg == Some(Color::Red))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(red, "abcdefghijklmnopqrstuvwxyz");
        let wrapped_rows = lines
            .iter()
            .filter(|line| line.spans.iter().any(|s| s.style.fg == Some(Color::Red)))
            .count();
        assert!(
            wrapped_rows > 1,
            "expected the row to wrap, got {wrapped_rows}"
        );
    }

    #[test]
    fn tool_output_caps_by_physical_rows_even_under_logical_line_limit() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80); // prime last_width
        // 8 logical lines, each ~400 columns => ~6 wrapped rows each => ~48
        // physical rows if uncapped. Each line alone exceeds the head/tail
        // budgets, but the head always keeps at least the first line, so one
        // line survives and the rest (7) are reported as omitted.
        let long = "x".repeat(400);
        let content = std::iter::repeat_n(long, 8).collect::<Vec<_>>().join("\n");
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "big" })),
            content,
            exit_code: None,
            duration: None,
        });
        let lines = screen.wrapped_lines(80);
        let output_rows = lines.iter().filter(|l| line_text(l).contains('x')).count();
        assert!(
            output_rows <= MAX_TOOL_OUTPUT_ROWS,
            "output not row-capped: {output_rows} physical rows"
        );
        // The visibility guarantee: even when the first line alone exceeds the
        // head budget, it is still shown (never collapsed to only a marker).
        assert!(
            output_rows >= 1,
            "first line must always survive: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| line_text(l).contains("… +7 lines")),
            "expected an accurate '… +7 lines' omitted-line indicator: {lines:?}",
        );
    }

    #[test]
    fn tool_output_keeps_head_and_tail_with_middle_elided() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80); // prime last_width
        // 20 short lines exceed the compact row budget, so a head slice and a
        // tail slice survive with a `… +N lines` marker between (Codex parity:
        // the final/summary line stays visible).
        let content = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "seq" })),
            content,
            exit_code: None,
            duration: None,
        });
        let texts: Vec<String> = screen.wrapped_lines(80).iter().map(line_text).collect();
        // First output line shown under the head gutter; last line shown in tail.
        assert!(texts.iter().any(|t| t.contains("line 0")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("line 19")), "{texts:?}");
        // The middle is elided with an accurate count, and the block stays
        // within the physical-row budget (+ marker).
        assert!(
            texts
                .iter()
                .any(|t| t.contains("… +") && t.contains("lines")),
            "{texts:?}"
        );
        // Truncated: far fewer than the 20 input lines survive (the cap is 8).
        let shown = texts.iter().filter(|t| t.contains("line ")).count();
        assert!(shown <= MAX_TOOL_OUTPUT_ROWS, "{texts:?}");
    }

    #[test]
    fn approval_hint_names_tool_target() {
        let mut screen = Screen::new();
        screen.show_approval(&call_args("bash", json!({ "command": "echo hi" })), false);
        let lines = screen.status_lines(80);
        assert!(line_text(&lines[0]).contains("approve echo hi"));
        assert!(line_text(&lines[0]).contains("[y] once"));
        assert!(line_text(&lines[0]).contains("[N] deny"));
    }

    #[test]
    fn approval_and_working_lines_stay_bounded_at_tiny_widths() {
        let hint = ApprovalHint {
            target: "run an extremely long command".to_string(),
            options: "[y] once  [N] deny",
        };
        for width in 1..=4 {
            for line in approval_status_lines(&hint, width) {
                assert!(
                    display_width(&line_text(&line)) <= width,
                    "width {width}: {line:?}"
                );
            }
            for line in working_lines(SPINNER_FRAMES[0], Some(Duration::from_secs(1)), None, width)
            {
                assert!(
                    display_width(&line_text(&line)) <= width,
                    "width {width}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn approval_prompt_renders_inside_editor_panel_and_wraps() {
        let mut screen = Screen::new();
        screen.show_approval(
            &call_args(
                "bash",
                json!({
                    "command": "printf 'global:\\n'; find \"$HOME/.iris/fragments\" -maxdepth 1 -type f -name '*.md' -print 2>/dev/null",
                    "timeout": 120
                }),
            ),
            false,
        );
        let status_lines = screen.status_lines(48);
        assert!(
            status_lines
                .iter()
                .all(|line| display_width(&line_text(line)) <= 44),
            "{status_lines:?}"
        );
        let rendered = rendered_text(&mut screen, 48, 12);
        assert!(rendered.contains("approve printf 'global:"));
        assert!(rendered.contains("120s)"), "{rendered}");
        assert!(rendered.contains("[N] deny"), "{rendered}");
        assert!(!rendered.contains(COMPOSER_HINT), "{rendered}");
        assert!(
            !rendered.contains("Ask the agent anything..."),
            "{rendered}"
        );
    }

    #[test]
    fn editor_visual_rows_use_actual_inner_text_width() {
        let mut editor = fresh_editor();
        editor.insert_str("abcdefghijklmnopqrst");

        assert_eq!(editor_visual_rows(&editor, 18), 2);
    }

    #[test]
    fn approval_record_renders_as_approval_panel_with_green_marker() {
        let mut screen = Screen::new();
        screen.record_approval(
            &call_args("bash", json!({ "command": "echo hi" })),
            ApprovalDecision::Allow,
        );
        assert!(screen.transcript.rows.iter().any(|row| matches!(
            row.chrome.as_ref(),
            Some(ChromeRow::Header {
                title: "APPROVAL",
                ..
            })
        )));
        let rendered = rendered_text(&mut screen, 80, 12);
        assert!(rendered.contains("APPROVAL"), "{rendered}");
        assert!(
            rendered.contains("You approved iris to run echo hi this time"),
            "{rendered}"
        );
        assert!(rendered.contains("┌"), "{rendered}");
        assert!(rendered.contains("└"), "{rendered}");

        let lines = screen.wrapped_lines(80);
        let line = line_matching(&lines, |line| line_text(line).contains("You approved"));
        let marker = span_matching(line, |span| span.content.as_ref() == "✔");
        assert_eq!(marker.style, ok_style());
    }

    #[test]
    fn tool_denial_renders_as_approval_panel_with_red_marker() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolDenied(call_args(
            "bash",
            json!({ "command": "echo hi" }),
        )));

        let rendered = rendered_text(&mut screen, 80, 12);
        assert!(rendered.contains("APPROVAL"), "{rendered}");
        assert!(rendered.contains("DENIED"), "{rendered}");
        assert!(rendered.contains("✗ Denied echo hi"), "{rendered}");
        let lines = screen.wrapped_lines(80);
        let line = line_matching(&lines, |line| line_text(line).contains("Denied echo hi"));
        let marker = span_matching(line, |span| span.content.as_ref() == "✗");
        assert_eq!(marker.style, err_style());
    }

    #[test]
    fn approval_record_preserves_ansi_target_style() {
        let mut screen = Screen::new();
        screen.record_approval(
            &call_args("bash", json!({ "command": "\u{1b}[31mred\u{1b}[0m" })),
            ApprovalDecision::Allow,
        );
        let lines = screen.wrapped_lines(80);
        let line = line_matching(&lines, |line| line_text(line).contains("red"));
        let red = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "red")
            .expect("red span");
        assert_eq!(red.style, Style::default().fg(Color::Red));
    }

    #[test]
    fn consecutive_blocks_get_one_blank_separator() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText("hi".to_string()));
        screen.apply(UiEvent::Notice("note".to_string()));
        let texts: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert_eq!(
            texts,
            vec!["hi".to_string(), String::new(), "note: note".to_string()]
        );
    }

    #[test]
    fn diff_preview_drops_file_headers_and_colors_changes() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::DiffPreview {
            call: call("edit"),
            diff: "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
        });
        let texts: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert!(!texts.iter().any(|t| t.contains("--- a/note.txt")));
        assert!(!texts.iter().any(|t| t.contains("@@ -1 +1 @@")));
        assert!(texts.iter().any(|t| t.contains("-  |  old")));
        assert!(texts.iter().any(|t| t.contains("+  |  new")));
        let add = screen
            .transcript
            .rows
            .iter()
            .find(|row| row.text.contains("+  |  new"))
            .expect("addition row");
        let remove = screen
            .transcript
            .rows
            .iter()
            .find(|row| row.text.contains("-  |  old"))
            .expect("removal row");
        assert_eq!(add.style, ok_style());
        assert_eq!(remove.style, err_style());
        assert!(matches!(
            add.chrome.as_ref(),
            Some(ChromeRow::Body {
                bg: Some(DIFF_ADD_BG),
                ..
            })
        ));
        assert!(matches!(
            remove.chrome.as_ref(),
            Some(ChromeRow::Body {
                bg: Some(DIFF_DEL_BG),
                ..
            })
        ));
    }

    #[test]
    fn two_file_diff_drops_every_header_pair_not_just_the_first() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::DiffPreview {
            call: call("edit"),
            diff: concat!(
                "--- a/one.txt\n+++ b/one.txt\n@@ -1 +1 @@\n-old1\n+new1\n",
                "--- a/two.txt\n+++ b/two.txt\n@@ -1 +1 @@\n-old2\n+new2\n"
            )
            .to_string(),
        });
        let texts: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        // No file header survives, for either file.
        assert!(!texts.iter().any(|t| t.starts_with("--- ")));
        assert!(!texts.iter().any(|t| t.starts_with("+++ ")));
        // Both files' real changes remain.
        assert!(texts.iter().any(|t| t.contains("+  |  new1")));
        assert!(texts.iter().any(|t| t.contains("+  |  new2")));
        assert!(texts.iter().any(|t| t.contains("-  |  old2")));
        // The second file's removal is red, not styled as plain context.
        let remove2 = screen
            .transcript
            .rows
            .iter()
            .find(|row| row.text.contains("-  |  old2"))
            .expect("second removal row");
        assert_eq!(remove2.style, err_style());
    }

    #[test]
    fn transcript_history_stays_in_state_for_replay_after_turn_end() {
        let mut screen = Screen::new();
        screen.start_turn();
        screen.apply(UiEvent::AssistantText("first answer".to_string()));
        screen.apply(UiEvent::Notice("a note".to_string()));
        screen.end_turn();

        let rendered = rendered_text(&mut screen, 80, 12);
        assert!(rendered.contains("first answer"), "{rendered:?}");
        assert!(rendered.contains("a note"), "{rendered:?}");
        assert!(
            rendered.contains("Give iris a task"),
            "composer missing: {rendered:?}"
        );
        assert!(!rendered.contains("AGENT"), "{rendered:?}");
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|row| row.text.contains("first answer")),
            "finalized history must remain in Iris state"
        );
    }

    #[test]
    fn surface_draw_path_replays_history_from_state() -> std::io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let mut screen = Screen::new();

        screen.commit_user("hello there");
        screen.start_turn();
        surface.render(Size::new(40, 14), &rendered_lines(&mut screen, 40, 14))?;
        screen.apply(UiEvent::AssistantText("# Done\n\nall good".to_string()));
        surface.render(Size::new(40, 14), &rendered_lines(&mut screen, 40, 14))?;
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "echo hi" })),
            content: "hi".to_string(),
            exit_code: None,
            duration: None,
        });
        screen.end_turn();
        surface.render(Size::new(40, 14), &rendered_lines(&mut screen, 40, 14))?;

        let replay = strip_ansi(&surface.state().previous_lines.join("\n"));
        assert!(replay.contains("hello there"), "{replay:?}");
        assert!(replay.contains("● Done"), "{replay:?}");
        assert!(replay.contains("SHELL"), "{replay:?}");
        assert!(replay.contains("$ echo hi"), "{replay:?}");
        assert!(replay.contains("Give iris a task"), "{replay:?}");
        assert!(!replay.contains("USER"), "{replay:?}");
        assert!(!replay.contains("AGENT"), "{replay:?}");
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|row| row.text.contains("hello")),
            "draw must not drain transcript state"
        );
        Ok(())
    }

    #[test]
    fn width_resize_reflows_transcript_from_state() -> std::io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText(
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda".to_string(),
        ));

        surface.render(Size::new(30, 5), &rendered_lines(&mut screen, 30, 5))?;
        let wide_rows = surface.state().previous_lines.len();
        surface.writer_mut().clear();
        let stats = surface.render(Size::new(12, 5), &rendered_lines(&mut screen, 12, 5))?;

        assert_eq!(stats.kind, RenderKind::FullRedraw);
        assert!(
            surface.state().previous_lines.len() > wide_rows,
            "narrow width should wrap/reflow the replayed transcript"
        );
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|row| row.text.contains("alpha beta")),
            "source transcript must remain intact after resize"
        );
        Ok(())
    }

    #[test]
    fn global_status_rail_and_composer_match_pane_spec() {
        let mut screen = Screen::new();
        screen.set_footer(
            "sonnet 3.5".to_string(),
            Some("high".to_string()),
            "~/workspace/user-auth (feat/rate-limit)".to_string(),
        );
        let rendered = rendered_text(&mut screen, 180, 12);

        assert!(rendered.contains("● MODE code  ┊  MODEL sonnet 3.5"));
        assert!(rendered.contains("MODEL sonnet 3.5"));
        assert!(rendered.contains("EFFORT high"));
        assert!(rendered.contains("CWD ~/workspace/user-auth"));
        assert!(rendered.contains("BRANCH feat/rate-limit"));
        assert!(!rendered.contains("CONTEXT 128k"));
        assert!(!rendered.contains("APPROVAL auto"));
        assert!(rendered.contains("┌"));
        assert!(rendered.contains("Give iris a task..."));
        assert!(!rendered.contains("Ask the agent anything..."));
        assert!(rendered.contains("↵ to send  •  shift+↵ for new line  •  / for commands"));
    }

    #[test]
    fn global_status_rail_is_pinned_with_composer_chrome_not_scrollback() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            "~/repo (feat/pin-rail)".to_string(),
        );
        for i in 0..40 {
            screen.apply(UiEvent::AssistantText(format!("line {i}")));
        }

        let lines = rendered_lines(&mut screen, 180, 12);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let rail_idx = texts
            .iter()
            .position(|line| line.contains("● MODE code"))
            .expect("status rail remains visible");
        let editor_idx = texts
            .iter()
            .position(|line| line.contains("Give iris a task"))
            .expect("composer remains visible");
        assert!(rail_idx < editor_idx, "{texts:?}");
        assert!(texts[rail_idx].contains("BRANCH feat/pin-rail"));
        assert!(
            !texts
                .last()
                .is_some_and(|line| line.contains("MODEL") || line.contains("tokens")),
            "{texts:?}"
        );
    }

    #[test]
    fn inline_working_indicator_keeps_spinner_usage_interrupt_hint_and_effort() {
        let mut screen = Screen::new();
        screen.set_footer(
            "opus-4.8".to_string(),
            Some("high".to_string()),
            "~/repo (branch)".to_string(),
        );
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: Some("resp_1".to_string()),
            usage: Some(ProviderUsage {
                provider: "anthropic".to_string(),
                model: "opus-4.8".to_string(),
                input_tokens: 300,
                output_tokens: 104,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 30,
                total_tokens: 404,
                cache_creation: None,
            }),
        });
        screen.start_turn();

        let before = rendered_text(&mut screen, 100, 16);
        assert!(!before.contains("WORKING"), "{before}");
        assert!(before.contains(SPINNER_FRAMES[0]), "{before}");
        assert!(before.contains("Working…"), "{before}");
        assert!(before.contains("esc to interrupt"), "{before}");
        assert!(before.contains("↓ 404 tokens"), "{before}");
        assert!(before.contains("thinking with high effort"), "{before}");

        assert!(screen.tick());
        let after = rendered_text(&mut screen, 100, 16);
        assert!(after.contains(SPINNER_FRAMES[1]), "{after}");
        let working = screen
            .working_lines(100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !working.contains('┌'),
            "working indicator must not be framed: {working}"
        );
    }

    #[test]
    fn active_turn_effort_is_only_in_working_indicator() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            "~/repo".to_string(),
        );
        screen.start_turn();

        let rail = global_status_line(&screen, 100)
            .map(|line| line_text(&line))
            .unwrap_or_default();
        let working = screen
            .working_lines(100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!rail.contains("EFFORT"), "{rail}");
        assert!(working.contains("thinking with high effort"), "{working}");
    }

    #[test]
    fn non_bash_tools_show_live_running_panel_and_finalize_in_place() {
        let mut screen = Screen::new();
        let call = call_args("edit", json!({ "file_path": "src/main.rs" }));

        screen.apply(UiEvent::ToolStarted(call.clone()));
        let running = rendered_text(&mut screen, 100, 12);
        assert!(running.contains("EDIT"), "{running}");
        assert!(running.contains("● RUNNING"), "{running}");
        assert!(running.contains("running…"), "{running}");

        screen.apply(UiEvent::ToolResult {
            call,
            content: "Successfully replaced 1 occurrence.".to_string(),
            exit_code: None,
            duration: Some(Duration::from_millis(3)),
        });
        let done = rendered_text(&mut screen, 100, 12);
        assert!(done.contains("● DONE"), "{done}");
        assert!(
            done.contains("Successfully replaced 1 occurrence."),
            "{done}"
        );
        assert!(!done.contains("running…"), "{done}");
    }

    #[test]
    fn completed_panel_headers_use_success_dot_not_running_accent() {
        let mut transcript = Transcript::default();
        transcript.push_shell_header(false, false, Some(Duration::from_secs(1)), None, "echo hi");
        let dot_style = transcript
            .rows
            .iter()
            .find_map(|row| match row.chrome.as_ref() {
                Some(ChromeRow::Header {
                    title: "SHELL",
                    right,
                    ..
                }) => Some(right[0].1),
                _ => None,
            })
            .expect("shell header dot style");

        assert_eq!(dot_style.fg, ok_style().fg);
    }

    #[test]
    fn non_bash_tool_finalization_preserves_interleaved_rows() {
        let mut screen = Screen::new();
        let call = call_args("edit", json!({ "file_path": "src/main.rs" }));

        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::Notice("interleaved note".to_string()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "Successfully replaced 1 occurrence.".to_string(),
            exit_code: None,
            duration: Some(Duration::from_millis(3)),
        });

        let rendered = rendered_text(&mut screen, 100, 16);
        assert!(rendered.contains("● DONE"), "{rendered}");
        assert!(
            rendered.contains("Successfully replaced 1 occurrence."),
            "{rendered}"
        );
        assert!(rendered.contains("note: interleaved note"), "{rendered}");
        assert!(!rendered.contains("running…"), "{rendered}");
    }

    #[test]
    fn active_shell_delta_and_finalize_preserve_interleaved_rows() {
        let mut screen = Screen::new();
        let call = call_args("bash", json!({ "command": "echo hi" }));

        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::Notice("interleaved note".to_string()));
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "hi\n".to_string(),
        });
        screen.apply(UiEvent::ToolResult {
            call,
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(3)),
        });

        let rendered = rendered_text(&mut screen, 100, 18);
        assert!(rendered.contains("SHELL"), "{rendered}");
        assert!(rendered.contains("● DONE"), "{rendered}");
        assert!(rendered.contains("$ echo hi"), "{rendered}");
        assert!(rendered.contains("hi"), "{rendered}");
        assert!(rendered.contains("note: interleaved note"), "{rendered}");
        assert!(!rendered.contains("RUNNING"), "{rendered}");
    }

    #[test]
    fn exploration_tool_error_stays_inside_explore_panel() {
        let mut screen = Screen::new();
        let call = call_args("read", json!({ "path": "src/missing.rs" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolError {
            call,
            message: "not found".to_string(),
        });

        let rows = &screen.transcript.rows;
        let header = rows
            .iter()
            .position(|row| {
                matches!(
                    row.chrome.as_ref(),
                    Some(ChromeRow::Header {
                        title: "EXPLORE",
                        ..
                    })
                )
            })
            .expect("explore header");
        let error = rows
            .iter()
            .position(|row| row.text.contains("error: not found"))
            .expect("error body");
        let bottom = rows
            .iter()
            .position(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::Bottom)))
            .expect("bottom border");
        assert!(
            header < error && error < bottom,
            "error must stay inside panel"
        );
    }

    #[test]
    fn concurrent_explorations_share_one_header_with_aggregate_state() {
        let mut screen = Screen::new();
        let read = call_args("read", json!({ "path": "src/missing.rs" }));
        let mut grep = call_args("grep", json!({ "pattern": "needle", "path": "src" }));
        grep.id = "call_2".to_string();

        screen.apply(UiEvent::ToolStarted(read.clone()));
        screen.apply(UiEvent::ToolStarted(grep.clone()));
        screen.apply(UiEvent::ToolError {
            call: read,
            message: "not found".to_string(),
        });
        let running = rendered_text(&mut screen, 100, 16);
        assert!(running.contains("EXPLORE"), "{running}");
        assert!(running.contains("RUNNING"), "{running}");
        assert!(!running.contains("ERROR       00:00:00s\n├"), "{running}");

        screen.apply(UiEvent::ToolResult {
            call: grep,
            content: "src/main.rs:needle".to_string(),
            exit_code: None,
            duration: None,
        });

        let rows = &screen.transcript.rows;
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::Top)))
                .count(),
            1,
            "started explorations should share one panel"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(
                    row.chrome.as_ref(),
                    Some(ChromeRow::Header {
                        title: "EXPLORE",
                        ..
                    })
                ))
                .count(),
            1,
            "started explorations should share one header"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::Separator)))
                .count(),
            1,
            "started explorations should share one separator"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::Bottom)))
                .count(),
            1,
            "started explorations should share one bottom border"
        );
        let state = rows
            .iter()
            .find_map(|row| match row.chrome.as_ref() {
                Some(ChromeRow::Header { title, right, .. }) if *title == "EXPLORE" => Some(
                    right
                        .iter()
                        .map(|(text, _)| text.as_str())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .expect("explore header state");
        assert!(state.contains("ERROR"), "{state:?}");
        assert!(!state.contains("RUNNING"), "{state:?}");
        let body_texts: Vec<&str> = rows
            .iter()
            .filter(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::Body { .. })))
            .map(|row| row.text.as_str())
            .collect();
        assert_eq!(body_texts.len(), 2, "{body_texts:?}");
        assert!(body_texts.contains(&"error: not found"), "{body_texts:?}");
        assert!(
            body_texts.iter().any(|text| text.contains("Search needle")),
            "{body_texts:?}"
        );
    }

    #[test]
    fn explore_header_uses_reported_result_duration() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args("read", json!({ "path": "src/a.rs" })),
            content: "ignored".to_string(),
            exit_code: None,
            duration: Some(Duration::from_secs(4)),
        });

        let rendered = rendered_text(&mut screen, 100, 12);
        assert!(rendered.contains("EXPLORE"), "{rendered}");
        assert!(rendered.contains("00:00:04s"), "{rendered}");
        assert!(!rendered.contains("00:00:00s"), "{rendered}");
    }

    #[test]
    fn explore_panel_keeps_bottom_border_when_grouping_results() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args("read", json!({ "path": "src/a.rs" })),
            content: "ignored".to_string(),
            exit_code: None,
            duration: None,
        });
        screen.apply(UiEvent::ToolResult {
            call: call_args("grep", json!({ "pattern": "needle", "path": "src" })),
            content: "ignored".to_string(),
            exit_code: None,
            duration: None,
        });

        let rows = &screen.transcript.rows;
        let explore_headers = rows
            .iter()
            .filter(|row| {
                matches!(
                    row.chrome.as_ref(),
                    Some(ChromeRow::Header {
                        title: "EXPLORE",
                        ..
                    })
                )
            })
            .count();
        assert_eq!(explore_headers, 1);
        assert!(matches!(
            rows.last().and_then(|row| row.chrome.as_ref()),
            Some(ChromeRow::Bottom)
        ));
    }

    #[test]
    fn submitted_prompt_renders_as_plain_unboxed_user_text() {
        let mut screen = Screen::new();
        screen.commit_user("Add rate limiting to the login endpoint.");
        let rendered = rendered_text(&mut screen, 96, 14);

        assert!(!rendered.contains("TASK"));
        assert!(!rendered.contains("USER"), "{rendered}");
        assert!(
            rendered.contains("    Add rate limiting to the login endpoint."),
            "{rendered}"
        );
        assert!(!rendered.contains("│  Add rate limiting"));
    }

    #[test]
    fn shell_and_diff_tools_render_as_bordered_instrument_panels() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "pnpm test --filter user.auth" })),
            content: "PASS    test/auth.service.test.ts (12)\n\nTime        1.48s".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(1480)),
        });
        screen.apply(UiEvent::DiffPreview {
            call: call_args(
                "edit",
                json!({ "file_path": "packages/user.auth/src/auth.service.ts" }),
            ),
            diff: "--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
        });
        let rendered = rendered_text(&mut screen, 110, 24);

        assert!(rendered.contains("SHELL"));
        assert!(rendered.contains("bash"));
        assert!(rendered.contains("● DONE"));
        assert!(rendered.contains("$ pnpm test --filter user.auth"));
        assert!(rendered.contains("PASS    test/auth.service.test.ts"));
        assert!(rendered.contains("EDIT"));
        assert!(rendered.contains("PREVIEW"), "{rendered}");
        assert!(!rendered.contains("RUNNING"), "{rendered}");
        assert!(rendered.contains("packages/user.auth/src/auth.service.ts"));
        assert!(rendered.contains("-  |  old"));
        assert!(rendered.contains("+  |  new"));
        assert!(!rendered.contains("--- a/file"));
        assert!(!rendered.contains("@@ -1 +1 @@"));
    }

    #[test]
    fn diff_preview_denial_leaves_no_stale_running_panel() {
        let mut screen = Screen::new();
        let call = call_args("edit", json!({ "file_path": "src/main.rs" }));
        screen.apply(UiEvent::DiffPreview {
            call: call.clone(),
            diff: "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
        });
        screen.apply(UiEvent::ToolDenied(call));

        let rendered = rendered_text(&mut screen, 100, 16);
        assert!(rendered.contains("PREVIEW"), "{rendered}");
        assert!(rendered.contains("DENIED"), "{rendered}");
        assert!(!rendered.contains("RUNNING"), "{rendered}");
    }

    #[test]
    fn unsourced_global_status_fields_are_not_rendered() {
        let mut screen = Screen::new();
        let rendered = rendered_text(&mut screen, 80, 10);

        assert!(!rendered.contains("MODEL model"), "{rendered}");
        assert!(!rendered.contains("EFFORT -"), "{rendered}");
        assert!(!rendered.contains("CWD ~"), "{rendered}");
        assert!(!rendered.contains("BRANCH -"), "{rendered}");
    }

    #[test]
    fn sourced_global_status_omits_unknown_effort_and_branch() {
        let mut screen = Screen::new();
        screen.set_footer("gpt-5.5".to_string(), None, "~/repo".to_string());
        let rendered = rendered_text(&mut screen, 100, 10);

        assert!(rendered.contains("MODEL gpt-5.5"), "{rendered}");
        assert!(rendered.contains("CWD ~/repo"), "{rendered}");
        assert!(!rendered.contains("EFFORT"), "{rendered}");
        assert!(!rendered.contains("BRANCH"), "{rendered}");
    }

    #[test]
    fn sourced_global_status_separates_model_and_effort() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            "~/repo".to_string(),
        );
        let rendered = rendered_text(&mut screen, 100, 10);

        assert!(rendered.contains("MODEL gpt-5.5"), "{rendered}");
        assert!(rendered.contains("EFFORT high"), "{rendered}");
        assert!(!rendered.contains("MODEL gpt-5.5 high"), "{rendered}");
    }

    #[test]
    fn tiny_panel_rows_are_width_safe_with_visible_border_glyphs() {
        for width in 1..=5 {
            let rows = vec![
                panel_rule_line(width, '┌', '┐'),
                panel_header_line(
                    width,
                    true,
                    "SHELL",
                    "bash",
                    &[
                        ("●".to_string(), prompt_style()),
                        (" DONE".to_string(), panel_style()),
                    ],
                ),
                panel_body_line(
                    width,
                    Line::from(Span::styled("body".to_string(), panel_style())),
                    None,
                ),
            ];
            for row in rows {
                let text = line_text(&row);
                assert!(display_width(&text) <= width, "width {width}: {row:?}");
                assert!(
                    text.contains('┌')
                        || text.contains('┐')
                        || text.contains('│')
                        || text.contains('└')
                        || text.contains('┘'),
                    "width {width}: a tiny panel row should show the clearest possible border glyph: {text:?}"
                );
            }
        }
    }

    #[test]
    fn trim_history_never_leaves_orphan_panel_rows() {
        let mut transcript = Transcript::default();
        let call = call_args("bash", json!({ "command": "echo hi" }));
        transcript.push_shell_panel(&call, "hi", false, false, None, None);
        for i in 0..MAX_TRANSCRIPT_ROWS.saturating_sub(2) {
            transcript
                .rows
                .push(TranscriptRow::new(format!("plain {i}"), panel_style()));
        }
        assert!(transcript.rows.len() > MAX_TRANSCRIPT_ROWS);

        transcript.trim_history();

        assert!(transcript.rows.len() <= MAX_TRANSCRIPT_ROWS);
        assert!(
            !matches!(
                transcript.rows.first().and_then(|row| row.chrome.as_ref()),
                Some(
                    ChromeRow::Header { .. }
                        | ChromeRow::Separator
                        | ChromeRow::Body { .. }
                        | ChromeRow::Bottom
                )
            ),
            "trim left an orphan panel row at the start"
        );
        let mut in_panel = false;
        for row in &transcript.rows {
            match row.chrome.as_ref() {
                Some(ChromeRow::Top) => {
                    assert!(!in_panel, "nested panel start");
                    in_panel = true;
                }
                Some(ChromeRow::Header { .. } | ChromeRow::Separator | ChromeRow::Body { .. }) => {
                    assert!(in_panel, "orphan panel interior: {:?}", row.text);
                }
                Some(ChromeRow::Bottom) => {
                    assert!(in_panel, "orphan panel bottom");
                    in_panel = false;
                }
                None => assert!(!in_panel, "plain row inside panel: {:?}", row.text),
            }
        }
        assert!(!in_panel, "trim left an unterminated panel");
    }

    #[test]
    fn bordered_panel_rows_are_equal_width_and_narrow_width_safe() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args(
                "bash",
                json!({ "command": "printf very-long-command-name-that-wraps" }),
            ),
            content: "line one\nline two".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_secs(71)),
        });
        screen.apply(UiEvent::ToolResult {
            call: call_args("read", json!({ "path": "src/very/long/path/name.rs" })),
            content: "ignored".to_string(),
            exit_code: None,
            duration: None,
        });

        for width in [34u16, 96] {
            let lines = screen.wrapped_lines(width);
            let texts: Vec<String> = lines.iter().map(line_text).collect();
            for text in texts.iter().filter(|text| {
                text.contains('┌') || text.contains('│') || text.contains('├') || text.contains('└')
            }) {
                assert_eq!(
                    display_width(text),
                    usize::from(width),
                    "width {width}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn exploration_tools_render_as_grouped_explore_panel() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args("read", json!({ "path": "src/tool_display.rs" })),
            content: "ignored file body".to_string(),
            exit_code: None,
            duration: None,
        });
        screen.apply(UiEvent::ToolResult {
            call: call_args(
                "grep",
                json!({ "pattern": "DiffPreview", "path": "src/ui", "glob": "*.rs" }),
            ),
            content: "ignored grep body".to_string(),
            exit_code: None,
            duration: None,
        });
        let rendered = rendered_text(&mut screen, 100, 22);

        assert!(rendered.contains("┌"));
        assert!(rendered.contains("EXPLORE"));
        assert!(!rendered.contains("READ"), "{rendered}");
        assert!(!rendered.contains("GREP"), "{rendered}");
        assert!(rendered.contains("src/tool_display.rs"));
        assert!(rendered.contains("Read src/tool_display.rs"));
        assert!(rendered.contains("Search DiffPreview in src/ui (*.rs)"));
        assert!(rendered.contains("src/ui"));
        assert!(rendered.contains("└"));
    }

    #[test]
    fn mutating_non_bash_tools_render_as_edit_panels_not_shell() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args("write", json!({ "path": "/tmp/demo.txt" })),
            content: "Wrote /tmp/demo.txt.".to_string(),
            exit_code: None,
            duration: Some(Duration::from_millis(3)),
        });
        let rendered = rendered_text(&mut screen, 100, 12);

        assert!(rendered.contains("EDIT"), "{rendered}");
        assert!(!rendered.contains("WRITE"), "{rendered}");
        assert!(rendered.contains("/tmp/demo.txt"), "{rendered}");
        assert!(rendered.contains("Wrote /tmp/demo.txt"));
        assert!(!rendered.contains("SHELL"), "{rendered}");
        assert!(!rendered.contains("$ write"), "{rendered}");
    }

    #[test]
    fn pasted_terminal_frames_inside_user_prompt_wrap_as_plain_text() {
        let mut screen = Screen::new();
        screen.commit_user(
            "┌────────────────────────────────────────────────────────────────────────────┐\n\
             │ ▾  SHELL    bash                                     ● DONE        0ms   ▣│\n\
             ├────────────────────────────────────────────────────────────────────────────┤\n\
             │  $ edit /tmp/demo.txt                                                     │\n\
             └────────────────────────────────────────────────────────────────────────────┘",
        );
        let lines: Vec<String> = screen.wrapped_lines(80).iter().map(line_text).collect();
        let joined = lines.join("\n");

        assert!(!joined.contains("USER"), "{joined}");
        assert!(
            lines.first().is_some_and(|line| line.starts_with("    ┌")),
            "{lines:?}"
        );
        for line in &lines {
            assert!(
                display_width(line) <= 80,
                "user prompt row exceeds width: {line:?}"
            );
            if !line.is_empty() {
                assert!(line.starts_with("    "), "{line:?}");
            }
        }
    }

    #[test]
    fn repeated_resize_does_not_duplicate_composer_placeholder() -> std::io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let mut screen = Screen::new();
        screen.apply(UiEvent::SessionStarted);

        for (width, height) in [(50, 14), (32, 10), (60, 16), (32, 10)] {
            surface.render(
                Size::new(width, height),
                &rendered_lines(&mut screen, width, height),
            )?;
        }

        let replay = strip_ansi(&surface.state().previous_lines.join("\n"));
        assert_eq!(replay.matches("Give iris a task").count(), 1, "{replay:?}");
        assert!(!replay.contains("Ask the agent anything"), "{replay:?}");
        Ok(())
    }

    #[test]
    fn shrinking_palette_and_modal_content_clears_old_rows() -> std::io::Result<()> {
        use crate::mimir::model_catalog::CatalogModel;
        use crate::mimir::selection::ProviderId;
        use crate::ui::modal::{Modal, ModelPicker};

        let mut surface = TerminalSurface::new(Vec::new());
        let mut screen = Screen::new();
        screen.open_modal(Modal::Model(ModelPicker::new(
            vec![
                CatalogModel {
                    provider: ProviderId::OpenAiCodex,
                    id: "gpt-5.5".to_string(),
                },
                CatalogModel {
                    provider: ProviderId::Anthropic,
                    id: "claude-sonnet-4-6".to_string(),
                },
            ],
            "openai-codex/gpt-5.5",
            "openai-codex/gpt-5.5",
            crate::mimir::selection::ReasoningEffort::Medium,
        )));
        surface.render(Size::new(60, 14), &rendered_lines(&mut screen, 60, 14))?;
        assert!(
            surface
                .state()
                .previous_lines
                .join("\n")
                .contains("GPT 5.5")
        );

        screen.close_modal();
        let stats = surface.render(Size::new(60, 14), &rendered_lines(&mut screen, 60, 14))?;
        let replay = strip_ansi(&surface.state().previous_lines.join("\n"));
        assert_ne!(stats.kind, RenderKind::Unchanged);
        assert!(!replay.contains("GPT 5.5"), "{replay:?}");
        assert!(replay.contains("Give iris a task"), "{replay:?}");
        Ok(())
    }

    #[test]
    fn editor_submit_clears_and_reports_text() {
        let mut screen = Screen::new();
        assert!(screen.editor_is_empty());
        screen.editor.insert_str("hello");
        assert_eq!(screen.editor_text(), "hello");
        assert!(!screen.editor_is_empty());
        let text = screen.submit();
        assert_eq!(text, "hello");
        assert!(screen.editor_is_empty());
    }

    #[test]
    fn editor_multiline_undo_and_kill_via_textarea() {
        let mut screen = Screen::new();
        screen.editor.insert_str("alpha");
        screen.editor.insert_newline();
        screen.editor.insert_str("beta");
        assert_eq!(screen.editor_text(), "alpha\nbeta");
        // Kill-word removes the last word.
        screen.editor.delete_word();
        assert_eq!(screen.editor_text(), "alpha\n");
        // Yank restores it from the kill-ring.
        screen.editor.paste();
        assert_eq!(screen.editor_text(), "alpha\nbeta");
        // Undo walks back the yank then the kill.
        screen.editor.undo();
        assert_eq!(screen.editor_text(), "alpha\n");
        screen.editor.undo();
        assert_eq!(screen.editor_text(), "alpha\nbeta");
        // Redo replays forward.
        screen.editor.redo();
        assert_eq!(screen.editor_text(), "alpha\n");
    }

    #[test]
    fn modal_render_survives_a_tiny_terminal() {
        use crate::mimir::model_catalog::CatalogModel;
        use crate::mimir::selection::ProviderId;
        use crate::ui::modal::{Modal, ModelPicker};

        for height in [2u16, 3, 4] {
            let mut screen = Screen::new();
            screen.open_modal(Modal::Model(ModelPicker::new(
                vec![CatalogModel {
                    provider: ProviderId::OpenAiCodex,
                    id: "gpt-5.5".to_string(),
                }],
                "openai-codex/gpt-5.5",
                "openai-codex/gpt-5.5",
                crate::mimir::selection::ReasoningEffort::Medium,
            )));
            let _ = rendered_lines(&mut screen, 40, height);
        }
    }

    #[test]
    fn open_modal_renders_plain_picker_above_composer() {
        use crate::mimir::model_catalog::CatalogModel;
        use crate::mimir::selection::ProviderId;
        use crate::ui::modal::{Modal, ModelPicker};

        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText("prior reply".to_string()));
        let models = vec![
            CatalogModel {
                provider: ProviderId::OpenAiCodex,
                id: "gpt-5.5".to_string(),
            },
            CatalogModel {
                provider: ProviderId::Anthropic,
                id: "claude-sonnet-4-6".to_string(),
            },
        ];
        screen.open_modal(Modal::Model(ModelPicker::new(
            models,
            "openai-codex/gpt-5.5",
            "openai-codex/gpt-5.5",
            crate::mimir::selection::ReasoningEffort::Medium,
        )));

        let rendered = rendered_text(&mut screen, 60, 14);
        assert!(rendered.contains("prior reply"), "{rendered}");
        assert!(rendered.contains("GPT 5.5"), "{rendered}");
        assert!(rendered.contains("Sonnet 4.6"), "{rendered}");
        assert!(rendered.contains("Give iris a task"), "{rendered}");
        let model_idx = rendered.find("GPT 5.5").expect("model row");
        let editor_idx = rendered.find("Give iris a task").expect("composer row");
        assert!(model_idx < editor_idx, "{rendered}");
        assert!(!rendered.contains("Select model"), "{rendered}");
    }

    #[test]
    fn open_modal_has_room_for_model_picker_footer() {
        use crate::mimir::model_catalog;
        use crate::ui::modal::{Modal, ModelPicker};

        let mut screen = Screen::new();
        screen.open_modal(Modal::Model(ModelPicker::new(
            model_catalog::all(),
            "anthropic/claude-opus-4-8",
            "anthropic/claude-opus-4-8",
            crate::mimir::selection::ReasoningEffort::XHigh,
        )));

        let rendered = rendered_text(&mut screen, 80, 16);
        assert!(rendered.contains("Haiku 4.5"), "{rendered}");
        assert!(rendered.contains("xhigh effort"), "{rendered}");
        assert!(rendered.contains("Reasoning"), "{rendered}");
        assert!(rendered.contains("Give iris a task"), "{rendered}");
    }

    #[test]
    fn long_composer_line_wraps_instead_of_scrolling_right() {
        let mut screen = Screen::new();
        screen.editor.insert_str("abcdefghijklmnopqrst");

        let rendered = rendered_text(&mut screen, 18, 8);
        assert!(rendered.contains("abcdefghij"), "{rendered}");
        assert!(rendered.contains("klmnopqrst"), "{rendered}");
        for line in rendered.lines() {
            assert!(display_width(line) <= 18, "{line:?}");
        }
    }

    #[test]
    fn footer_shows_real_provider_usage_when_reported() {
        let mut screen = Screen::new();
        screen.set_footer(
            "opus-4.8 xhigh".to_string(),
            Some("xhigh".to_string()),
            "~/repo (branch)".to_string(),
        );
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: Some("resp_1".to_string()),
            usage: Some(ProviderUsage {
                provider: "anthropic".to_string(),
                model: "opus-4.8".to_string(),
                input_tokens: 100,
                output_tokens: 20,
                cache_read_input_tokens: 64,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 5,
                total_tokens: 120,
                cache_creation: None,
            }),
        });

        let second = line_text(&screen.status_lines(80)[1]);
        assert!(second.starts_with("    "), "{second}");
        assert!(second.contains("120 tokens (64 cached)"), "{second}");
        assert!(second.contains("opus-4.8 xhigh"), "{second}");

        screen.set_footer(
            "opus-4.8 high".to_string(),
            Some("high".to_string()),
            "~/repo (branch)".to_string(),
        );
        let refreshed = line_text(&screen.status_lines(80)[1]);
        assert!(refreshed.contains("120 tokens (64 cached)"), "{refreshed}");
        assert!(refreshed.contains("opus-4.8 high"), "{refreshed}");
    }

    #[test]
    fn working_indicator_shows_elapsed_from_the_first_second() {
        let early = working_lines("\u{280b}", Some(Duration::from_secs(5)), None, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!early.contains("WORKING"), "{early}");
        assert!(!early.contains("00:00:05s"), "{early}");
        assert!(early.contains("5s"), "{early}");
        assert!(early.contains("esc to interrupt"), "{early}");

        let zero = working_lines("\u{280b}", None, None, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(zero.contains("0s"), "{zero}");

        let over = working_lines("\u{280b}", Some(Duration::from_secs(71)), None, 80)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(over.contains("1m 11s"), "{over}");
    }

    #[test]
    fn conversational_turn_emits_no_turn_rule() {
        // Codex parity: a turn that ran no tool (text-only answer) shows no
        // empty divider.
        let mut screen = Screen::new();
        screen.start_turn();
        screen.apply(UiEvent::AssistantText("done".to_string()));
        screen.end_turn();
        let lines = screen.wrapped_lines(20);
        assert!(
            !lines.iter().any(|l| line_text(l).starts_with('\u{2500}')),
            "no turn rule expected for a conversational turn: {lines:?}"
        );
    }

    #[test]
    fn elapsed_format_and_labelled_rule() {
        assert_eq!(format_elapsed_compact(45), "45s");
        assert_eq!(format_elapsed_compact(71), "1m 11s");
        assert_eq!(format_elapsed_compact(132), "2m 12s");
        assert_eq!(format_elapsed_compact(3669), "1h 01m 09s");
        // A labelled rule embeds the text and fills to width with dashes.
        let line = hrule_line("Worked for 2m 12s", 40);
        let text = line_text(&line);
        assert!(
            text.starts_with("\u{2500} Worked for 2m 12s \u{2500}"),
            "{text}"
        );
        assert_eq!(display_width(&text), 40);
        assert_eq!(line.spans[0].style, dim_style());
    }

    #[test]
    fn frame_shows_slash_palette_when_typing_command() {
        let mut screen = Screen::new();
        screen.editor.insert_str("/");
        screen.sync_palette();
        let lines = rendered_lines(&mut screen, 80, 12);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("/exit"));
        let exit = line_matching(&lines, |line| line_text(line).contains("/exit"));
        assert!(line_text(exit).starts_with("    /exit"), "{exit:?}");
        assert!(
            exit.spans
                .iter()
                .all(|span| !matches!(span.style.bg, Some(Color::Cyan))),
            "selected slash row must not use a background highlight: {exit:?}"
        );
        assert!(
            exit.spans
                .iter()
                .all(|span| !span.style.add_modifier.contains(Modifier::BOLD)),
            "selected slash row uses foreground color only: {exit:?}"
        );
        assert!(exit.spans.iter().any(|span| {
            span.content.as_ref().contains("End the session") && span.style.fg == Some(Color::Cyan)
        }));
        let model = line_matching(&lines, |line| line_text(line).contains("/model"));
        assert_eq!(
            line_text(exit).find("End the session"),
            line_text(model).find("Show or switch provider/model")
        );
        assert_ne!(model.spans[0].style.fg, Some(Color::Cyan));
        assert!(model.spans.iter().any(|span| {
            span.content.as_ref().contains("Show") && span.style.fg != Some(Color::Cyan)
        }));
    }

    #[test]
    fn tool_started_opens_running_shell_panel_in_replay_state() {
        let mut screen = Screen::new();
        screen.start_turn();
        let call = call_args("bash", json!({ "command": "echo hi" }));
        screen.apply(UiEvent::ToolStarted(call));
        let live: Vec<String> = screen.wrapped_lines(80).iter().map(line_text).collect();
        assert!(live.iter().any(|line| line.contains("SHELL")), "{live:?}");
        assert!(live.iter().any(|line| line.contains("RUNNING")), "{live:?}");
        assert!(
            live.iter().any(|line| line.contains("$ echo hi")),
            "{live:?}"
        );
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|row| row.text.contains("Running echo hi") || row.text.contains("$ echo hi")),
            "running panel must remain in Iris replay state"
        );
    }

    #[test]
    fn tool_output_deltas_stream_inside_shell_panel_and_are_flood_capped() {
        let mut screen = Screen::new();
        screen.start_turn();
        let _ = screen.wrapped_lines(80); // prime last_width
        let call = call_args("bash", json!({ "command": "flood" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        let long = "x".repeat(400);
        for _ in 0..50 {
            screen.apply(UiEvent::ToolOutputDelta {
                call_id: call.id.clone(),
                chunk: format!("{long}\n"),
            });
        }
        let lines = screen.wrapped_lines(80);
        let output_rows = lines.iter().filter(|l| line_text(l).contains('x')).count();
        assert!(
            output_rows <= MAX_TOOL_OUTPUT_ROWS,
            "streamed output not flood-capped: {output_rows} rows"
        );
        assert!(lines.iter().any(|l| line_text(l).contains("SHELL")));
        assert!(lines.iter().any(|l| line_text(l).contains("RUNNING")));
        assert!(lines.iter().any(|l| line_text(l).contains("$ flood")));
    }

    #[test]
    fn live_cell_shows_newest_streamed_lines_not_frozen_head() {
        let mut screen = Screen::new();
        screen.start_turn();
        let _ = screen.wrapped_lines(80); // prime last_width
        let call = call_args("bash", json!({ "command": "seq" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        // Stream more short lines than the row budget; the live tail must scroll
        // to the newest output rather than freezing on the earliest lines.
        for i in 0..100 {
            screen.apply(UiEvent::ToolOutputDelta {
                call_id: call.id.clone(),
                chunk: format!("line {i}\n"),
            });
        }
        let lines: Vec<String> = screen.wrapped_lines(80).iter().map(line_text).collect();
        assert!(
            lines.iter().any(|l| l.contains("line 99")),
            "newest line not shown: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|l| l.contains("line 0\u{0}") || l.ends_with("line 0")),
            "earliest line should have scrolled off: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("earlier lines")),
            "missing dropped-earlier-lines indicator: {lines:?}"
        );
    }

    #[test]
    fn shell_nonzero_exit_renders_error_status() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "false" })),
            content: "boom".to_string(),
            exit_code: Some(1),
            duration: Some(Duration::from_millis(50)),
        });

        let rendered = rendered_text(&mut screen, 80, 12);
        assert!(rendered.contains("SHELL"), "{rendered}");
        assert!(rendered.contains("ERROR"), "{rendered}");
        assert!(!rendered.contains("DONE"), "{rendered}");
        assert!(rendered.contains("boom"), "{rendered}");
    }

    #[test]
    fn finalized_headers_use_started_elapsed_when_duration_is_missing() {
        let mut transcript = Transcript::default();
        let started = Instant::now() - Duration::from_secs(2);
        transcript.push_shell_header(false, false, None, Some(started), "echo hi");
        let rendered = transcript
            .render(100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("SHELL"), "{rendered}");
        assert!(!rendered.contains("00:00:00s"), "{rendered}");
        assert!(rendered.contains("00:00:02s"), "{rendered}");
    }

    #[test]
    fn non_bash_tool_error_renders_error_status() {
        let mut screen = Screen::new();
        let call = call_args("edit", json!({ "file_path": "src/main.rs" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolError {
            call,
            message: "patch failed".to_string(),
        });

        let rendered = rendered_text(&mut screen, 80, 12);
        assert!(rendered.contains("EDIT"), "{rendered}");
        assert!(rendered.contains("ERROR"), "{rendered}");
        assert!(!rendered.contains("DONE"), "{rendered}");
        assert!(rendered.contains("error: patch failed"), "{rendered}");
    }

    #[test]
    fn fallback_tool_error_renders_message_once() {
        let mut screen = Screen::new();
        let call = call_args("bash", json!({ "command": "exit 2" }));

        screen.apply(UiEvent::ToolError {
            call,
            message: "cancelled".to_string(),
        });

        let rendered = rendered_text(&mut screen, 80, 12);
        assert!(rendered.contains("SHELL"), "{rendered}");
        assert_eq!(rendered.matches("cancelled").count(), 1, "{rendered}");
        assert!(rendered.contains("error: cancelled"), "{rendered}");
    }

    #[test]
    fn shell_panel_error_keeps_streamed_output() {
        let mut screen = Screen::new();
        screen.start_turn();
        let call = call_args("bash", json!({ "command": "sleep 9" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "partial line\n".to_string(),
        });
        screen.apply(UiEvent::ToolError {
            call: call.clone(),
            message: "cancelled".to_string(),
        });
        let rendered = rendered_text(&mut screen, 80, 14);
        assert!(rendered.contains("SHELL"), "{rendered}");
        assert!(rendered.contains("ERROR"), "{rendered}");
        assert!(!rendered.contains("DONE"), "{rendered}");
        assert!(rendered.contains("$ sleep 9"), "{rendered}");
        assert!(rendered.contains("partial line"), "{rendered}");
        assert!(rendered.contains("error: cancelled"), "{rendered}");
    }

    #[test]
    fn streamed_shell_panel_replays_from_state_after_finalize() -> std::io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let mut screen = Screen::new();
        screen.commit_user("run it");
        screen.start_turn();
        surface.render(Size::new(40, 14), &rendered_lines(&mut screen, 40, 14))?;
        let call = call_args("bash", json!({ "command": "echo hi" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        surface.render(Size::new(40, 14), &rendered_lines(&mut screen, 40, 14))?;
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "hi\n".to_string(),
        });
        surface.render(Size::new(40, 14), &rendered_lines(&mut screen, 40, 14))?;
        screen.apply(UiEvent::ToolResult {
            call: call.clone(),
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: Some(std::time::Duration::from_millis(10)),
        });
        screen.end_turn();
        surface.render(Size::new(40, 14), &rendered_lines(&mut screen, 40, 14))?;

        let everything = strip_ansi(&surface.state().previous_lines.join("\n"));
        assert!(everything.contains("SHELL"), "{everything:?}");
        assert!(everything.contains("DONE"), "{everything:?}");
        assert!(everything.contains("$ echo hi"), "{everything:?}");
        assert!(everything.contains("hi"), "{everything:?}");
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|row| row.text.contains("$ echo hi")),
            "exec rows must remain replayable from Iris state"
        );
        Ok(())
    }
}
