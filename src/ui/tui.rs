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

use crate::nexus::{ApprovalDecision, ToolCall};
use crate::tool_display::{
    exploration_active_summary, exploration_summary, is_exploration_tool, run_target, summarize,
};
use crate::ui::modal::Modal;
use crate::ui::slash::{self, Palette, SlashCommand};
use crate::ui::terminal_surface::TerminalSurface;
use crate::ui::{TurnErrorKind, UiEvent};

const BANNER_LINES: &[&str] = &["iris", "terminal-first coding agent", "Type /exit to quit."];

/// Idle status-row hint: discoverability without a help screen.
const IDLE_HINT: &str =
    "enter send \u{b7} shift+enter newline \u{b7} / commands \u{b7} ctrl-c quit";

/// Editor box grows with content up to this many text rows, then scrolls
/// internally (keeps the transcript from being squeezed by a huge paste).
const MAX_EDITOR_ROWS: u16 = 10;

/// Slash popup height cap (including its border).
const MAX_PALETTE_ROWS: u16 = 8;

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

/// Background fill for a committed user-message block, Codex's shaded prompt
/// cell. A fixed subtle lift above a dark terminal background (Codex computes
/// this from a live terminal-bg probe; Iris keeps a constant to avoid that
/// machinery). Presentation-only.
const USER_BG: Color = Color::Rgb(50, 50, 56);

/// Prompt glyph that opens a user-message block (Codex parity: `›`).
const USER_PREFIX: &str = "\u{203a} ";

/// A turn must run at least this long before the elapsed clause appears in the
/// active-turn status and the turn-end rule is labelled `Worked for ...` (Codex
/// parity: quick turns stay quiet).
const ELAPSED_DISPLAY_THRESHOLD_SECS: u64 = 60;

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
    Style::default().fg(Color::Green)
}
fn err_style() -> Style {
    Style::default().fg(Color::Red)
}
fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}
fn prompt_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}
fn tool_header_style() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

/// Bullet glyph + style for a finalized exec cell: a bold green bullet on success
/// (`Some(0)` or no reported status) and a bold red bullet on a non-zero exit,
/// matching Codex's exec marker (`•`.green().bold() / `•`.red().bold()). A true
/// tool error or cancellation uses `✗` instead (see `push_tool_error` /
/// `finalize_active_error`).
fn exec_status(exit_code: Option<i32>) -> (&'static str, Style) {
    match exit_code {
        Some(0) | None => ("\u{2022}", ok_style().add_modifier(Modifier::BOLD)),
        Some(_) => ("\u{2022}", err_style().add_modifier(Modifier::BOLD)),
    }
}

/// Render a tool's wall-clock duration compactly: `340ms` under a second, else
/// `1.2s`.
fn format_duration(duration: std::time::Duration) -> String {
    let ms = duration.as_millis();
    if ms >= 1000 {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        format!("{ms}ms")
    }
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
fn banner_style() -> Style {
    Style::default().fg(Color::Magenta)
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
/// rows at `width` (accounting for the 4-column gutter), appending an ellipsis
/// when content is dropped. This keeps the head/tail fold a HARD physical-row cap
/// even when a single line (e.g. a minified blob) would otherwise wrap to far
/// more rows than its slice budget.
fn clamp_output_line(raw: &str, width: usize, max_rows: usize) -> String {
    let line = truncate_chars(raw, MAX_TOOL_OUTPUT_LINE_CHARS);
    let usable = width.saturating_sub(4).max(1);
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
/// `width`, accounting for the 4-column gutter prefix. At least one row. ANSI
/// escapes are counted as visible width (a conservative over-estimate that only
/// makes the flood cap trip slightly earlier), which is fine for a guard.
fn wrapped_row_estimate(line: &str, width: usize) -> usize {
    let usable = width.saturating_sub(4).max(1);
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
    /// wraps to (Codex's shaded user-message cell). `None` for ordinary rows.
    background: Option<Color>,
    /// A horizontal-rule row (Codex's turn separator). When set, `text` is the
    /// optional centered label and the row renders as `─ label ─────` to width.
    hrule: bool,
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
        }
    }

    fn with_line(line: Line<'static>, continuation_prefix: Option<&'static str>) -> Self {
        Self {
            text: line_text(&line),
            style: Style::default(),
            continuation_prefix,
            line: Some(line),
            word_wrap: false,
            background: None,
            hrule: false,
        }
    }

    /// A styled Markdown prose line, wrapped word-aware (and URL-safe).
    fn markdown(line: Line<'static>) -> Self {
        Self {
            text: line_text(&line),
            style: Style::default(),
            continuation_prefix: None,
            line: Some(line),
            word_wrap: true,
            background: None,
            hrule: false,
        }
    }

    /// A full-width horizontal-rule row with an optional centered label.
    fn rule(label: String) -> Self {
        Self {
            text: label,
            style: dim_style(),
            continuation_prefix: None,
            line: None,
            word_wrap: false,
            background: None,
            hrule: true,
        }
    }

    /// Paint this row's wrapped lines with a full-width background fill.
    fn with_bg(mut self, color: Color) -> Self {
        self.background = Some(color);
        self
    }

    fn render(&self, width: usize, out: &mut Vec<Line<'static>>) {
        if self.hrule {
            out.push(hrule_line(&self.text, width));
            return;
        }
        let start = out.len();
        match &self.line {
            Some(line) if self.word_wrap => push_wrapped_line_wordwise(line, width, out),
            Some(line) => push_wrapped_line(line, width, self.continuation_prefix, out),
            None => push_wrapped_row(&self.text, self.style, width, self.continuation_prefix, out),
        }
        if let Some(bg) = self.background {
            for physical in &mut out[start..] {
                apply_full_width_bg(physical, bg, width);
            }
        }
    }
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
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

/// Apply a full-width background fill to one already-wrapped physical line: set
/// the line's base style so every span inherits the bg, then pad to `width` with
/// a trailing space span (ratatui only colours the cells a span occupies).
fn apply_full_width_bg(line: &mut Line<'static>, bg: Color, width: usize) {
    line.style = line.style.bg(bg);
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
#[cfg(test)]
fn is_separator_row(row: &TranscriptRow) -> bool {
    !row.hrule && row.text.is_empty() && row.line.is_none()
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
                    spans.push(Span::styled(prefix, dim_style()));
                    cur_w = display_width(prefix);
                }
            }
            push_span_char(&mut spans, ch, span.style);
            cur_w += cw;
        }
    }

    out.push(Line::from(spans));
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
    // The only chars wrap_to_width removes are space separators, so matching
    // each physical row's chars back to the cell stream in order recovers the
    // style for every glyph (collapsed spaces are skipped over).
    let mut ci = 0;
    for physical in wrap_to_width(&text, width.max(1)) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for rc in physical.chars() {
            while ci < cells.len() && cells[ci].0 != rc {
                ci += 1;
            }
            let style = cells.get(ci).map_or(Style::default(), |(_, st)| *st);
            push_span_char(&mut spans, rc, style);
            ci += 1;
        }
        out.push(Line::from(spans));
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
}

struct ActiveExploration {
    call_id: String,
    row: usize,
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
    exploring_open: bool,
    /// Last width the transcript was rendered/flushed at, so width-aware
    /// shaping in the width-agnostic `apply` path (the tool-output flood cap)
    /// uses a realistic column count. Zero until the first render.
    last_width: usize,
}

impl Transcript {
    /// Append a blank separator row before a new top-level block, unless the
    /// transcript is empty or already ends in a blank row.
    fn push_blank(&mut self) {
        self.exploring_open = false;
        match self.rows.last() {
            None => {}
            Some(last) if last.text.is_empty() => {}
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

    /// Commit assistant `text` as rendered Markdown (headings/emphasis/code/
    /// lists/quotes). Tool output and diffs are NOT Markdown and use [`push`].
    fn push_markdown(&mut self, text: &str) {
        for line in crate::ui::markdown::render_markdown(text) {
            self.rows.push(TranscriptRow::markdown(line));
        }
    }

    fn push_continued(&mut self, text: &str, style: Style, continuation_prefix: &'static str) {
        for line in text.split('\n') {
            self.rows.push(TranscriptRow::with_line(
                Line::from(Span::styled(line.to_string(), style)),
                Some(continuation_prefix),
            ));
        }
    }

    /// Commit any in-flight streamed assistant text into the transcript.
    fn finish_stream(&mut self) {
        if let Some(text) = self.streaming.take()
            && !text.is_empty()
        {
            self.push_markdown(&text);
        }
    }

    fn record_approval(&mut self, call: &ToolCall, decision: ApprovalDecision) {
        let scope = match decision {
            ApprovalDecision::Allow => "this time",
            ApprovalDecision::AllowAlways => "this session",
            ApprovalDecision::Deny => return,
        };
        self.begin_block();
        self.rows.push(TranscriptRow::with_line(
            approval_line(call, scope),
            Some("  │ "),
        ));
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
            self.push_explored_result(call);
            return;
        }
        self.begin_block();
        let (bullet, style) = exec_status(exit_code);
        self.push_exec_header(
            bullet,
            style,
            &format!(" Ran {}", run_target(call)),
            duration,
        );
        self.push_tool_output(content);
    }

    fn push_tool_error(&mut self, call: &ToolCall, message: &str) {
        self.begin_block();
        self.push_exec_header(
            "\u{2717}",
            err_style(),
            &format!(" Ran {}", run_target(call)),
            None,
        );
        self.push_continued(&format!("  └ error: {message}"), err_style(), "    ");
    }

    /// Push an exec header row (`• Running`/`• Ran`/`✗ Ran`) with a
    /// status-colored bullet, the verb+target in the tool-header style, and an
    /// optional duration suffix. Wraps with the `  │ ` gutter like a tool result.
    fn push_exec_header(
        &mut self,
        bullet: &'static str,
        bullet_style: Style,
        rest: &str,
        duration: Option<std::time::Duration>,
    ) {
        let mut spans = vec![
            Span::styled(bullet, bullet_style),
            Span::styled(rest.to_string(), tool_header_style()),
        ];
        if let Some(duration) = duration {
            spans.push(Span::styled(
                format!(" ({})", format_duration(duration)),
                dim_style(),
            ));
        }
        self.rows
            .push(TranscriptRow::with_line(Line::from(spans), Some("  │ ")));
    }

    /// Open a live exec block: a `• Running {target}` header under a fresh
    /// separator, tracked as the active cell so deltas and the final result
    /// finalize it in place.
    fn begin_exec(&mut self, call: ToolCall) {
        self.begin_block();
        let body_start = self.rows.len();
        self.push_exec_header(
            "\u{2022}",
            tool_header_style(),
            &format!(" Running {}", run_target(&call)),
            None,
        );
        self.active_exec = Some(ActiveExec {
            call,
            output: String::new(),
            body_start,
        });
    }

    /// Re-render the open exec block in place from its bounded output buffer: the
    /// `Running` header followed by the flood-capped live tail.
    fn relayout_active_running(&mut self) {
        let Some(active) = self.active_exec.take() else {
            return;
        };
        self.rows.truncate(active.body_start);
        self.push_exec_header(
            "\u{2022}",
            tool_header_style(),
            &format!(" Running {}", run_target(&active.call)),
            None,
        );
        self.push_tool_output_tail(&active.output);
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
        self.rows.truncate(active.body_start);
        let (bullet, style) = exec_status(exit_code);
        self.push_exec_header(
            bullet,
            style,
            &format!(" Ran {}", run_target(call)),
            duration,
        );
        self.push_tool_output(content);
    }

    /// Finalize the open exec block as an error/cancellation in place: a red
    /// `✗ Ran` header, whatever streamed so far (so a cancelled command keeps
    /// its partial output), then the error line.
    fn finalize_active_error(&mut self, call: &ToolCall, message: &str) {
        let Some(active) = self.active_exec.take() else {
            return;
        };
        self.rows.truncate(active.body_start);
        self.push_exec_header(
            "\u{2717}",
            err_style(),
            &format!(" Ran {}", run_target(call)),
            None,
        );
        if !active.output.is_empty() {
            self.push_tool_output_tail(&active.output);
        }
        self.push_continued(&format!("  └ error: {message}"), err_style(), "    ");
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
        let prefix = if first { "  └ " } else { "    " };
        if line.contains("\x1b[") {
            self.rows.push(TranscriptRow::with_line(
                tool_output_line(prefix, &line),
                Some("    "),
            ));
        } else {
            // Char-hard-wrap (via a styled line) rather than word-wrap so leading
            // indentation and aligned columns in tool output are preserved and
            // the physical-row estimate stays exact.
            self.rows.push(TranscriptRow::with_line(
                Line::from(Span::styled(format!("{prefix}{line}"), dim_style())),
                Some("    "),
            ));
        }
    }

    fn push_tool_output(&mut self, content: &str) {
        if content.is_empty() {
            self.push("  └ (no output)", dim_style());
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
            self.push(&format!("    … +{hidden} lines"), dim_style());
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
            self.push("  └ (no output)", dim_style());
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
            self.push(&format!("  └ … +{start} earlier lines"), dim_style());
        }
        for (offset, raw) in lines[start..].iter().enumerate() {
            // Use the head gutter for the very first visible row only when no
            // earlier-lines note took it. Clamp each shown line so one very long
            // line cannot blow past the physical-row cap.
            let clamped = clamp_output_line(raw, width, MAX_TOOL_OUTPUT_ROWS);
            self.push_output_line(&clamped, start == 0 && offset == 0);
        }
    }

    fn push_explored_result(&mut self, call: &ToolCall) {
        self.finish_stream();
        if self.finish_exploration(call, false) {
            return;
        }
        if !self.exploring_open {
            self.push_blank();
            self.push("• Explored", tool_header_style());
            self.exploring_open = true;
        }
        self.push_continued(
            &format!("  └ {}", exploration_summary(call)),
            dim_style(),
            "    ",
        );
    }

    fn push_explored_start(&mut self, call: &ToolCall) {
        self.finish_stream();
        if !self.exploring_open {
            self.push_blank();
            self.push("• Explored", tool_header_style());
            self.exploring_open = true;
        }
        let row = self.rows.len();
        self.push_continued(
            &format!("  └ {}", exploration_active_summary(call)),
            dim_style(),
            "    ",
        );
        self.active_explorations.push(ActiveExploration {
            call_id: call.id.clone(),
            row,
        });
    }

    fn finish_exploration(&mut self, call: &ToolCall, failed: bool) -> bool {
        let Some(pos) = self
            .active_explorations
            .iter()
            .position(|active| active.call_id == call.id)
        else {
            return false;
        };
        let active = self.active_explorations.remove(pos);
        let marker = if failed { "\u{2717} " } else { "" };
        if let Some(row) = self.rows.get_mut(active.row) {
            *row = TranscriptRow::with_line(
                Line::from(Span::styled(
                    format!("  └ {marker}{}", exploration_summary(call)),
                    if failed { err_style() } else { dim_style() },
                )),
                Some("    "),
            );
        }
        true
    }

    fn push_explored_error(&mut self, call: &ToolCall, message: &str) -> bool {
        self.finish_stream();
        if !self.finish_exploration(call, true) {
            return false;
        }
        self.push_continued(&format!("    error: {message}"), err_style(), "    ");
        true
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
                // The accumulated stream and `text` are the same content; drop
                // the accumulator and commit the authoritative text exactly once.
                self.streaming = None;
                if !text.is_empty() {
                    self.push_blank();
                    self.push_markdown(&text);
                }
            }
            UiEvent::AssistantText(text) => {
                self.finish_stream();
                if !text.is_empty() {
                    self.push_blank();
                    self.push_markdown(&text);
                }
            }
            UiEvent::SessionStarted => {
                self.finish_stream();
                for line in BANNER_LINES {
                    self.push(line, banner_style());
                }
            }
            UiEvent::ToolProposed(_) => {
                // Non-gated tools show only their result row; nothing to render.
                self.finish_stream();
            }
            UiEvent::ToolStarted(call) => {
                if is_exploration_tool(&call) {
                    self.push_explored_start(&call);
                } else {
                    self.begin_exec(call);
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
                self.begin_block();
                self.push(&format!("diff - {}", summarize(&call)), dim_style());
                self.rows.extend(diff_rows(&diff));
            }
            UiEvent::ToolDenied(call) => {
                self.begin_block();
                self.push_continued(
                    &format!("✗ Denied {}", run_target(&call)),
                    err_style(),
                    "  │ ",
                );
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
                } else {
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
            || !self.active_explorations.is_empty()
        {
            return;
        }
        let remove = self.rows.len() - MAX_TRANSCRIPT_ROWS;
        self.rows.drain(..remove);
        self.exploring_open = self.rows.iter().any(|row| row.text == "• Explored");
    }

    /// Commit a submitted prompt into the transcript as a shaded user block
    /// (Codex parity): a `›` prompt glyph opens the first line, continuations are
    /// indented two columns, and every wrapped row gets a full-width background.
    fn commit_user(&mut self, text: &str) {
        self.push_blank();
        for (i, line) in text.split('\n').enumerate() {
            let prefix = if i == 0 { USER_PREFIX } else { "  " };
            let spans = vec![
                Span::styled(prefix, dim_style().add_modifier(Modifier::BOLD)),
                Span::raw(line.to_string()),
            ];
            self.rows
                .push(TranscriptRow::with_line(Line::from(spans), Some("  ")).with_bg(USER_BG));
        }
        self.trim_history();
    }

    /// Append Codex's turn-end separator: a dim full-width rule, labelled
    /// `Worked for <elapsed>` only when the turn ran longer than a minute.
    fn push_turn_rule(&mut self, elapsed: Option<Duration>) {
        self.finish_stream();
        self.push_blank();
        let label = match elapsed {
            Some(d) if d.as_secs() >= ELAPSED_DISPLAY_THRESHOLD_SECS => {
                format!("Worked for {}", format_elapsed_compact(d.as_secs()))
            }
            _ => String::new(),
        };
        self.rows.push(TranscriptRow::rule(label));
        self.trim_history();
    }

    fn render(&mut self, width: u16) -> Vec<Line<'static>> {
        let width = usize::from(width);
        self.last_width = width;
        let mut rows = Vec::new();
        for row in &self.rows {
            row.render(width, &mut rows);
        }
        if let Some(text) = &self.streaming {
            let text = streaming_markdown_preview(text);
            for line in crate::ui::markdown::render_markdown(&text) {
                TranscriptRow::markdown(line).render(width, &mut rows);
            }
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
    push_wrapped_line(&Line::from(target), width, Some("  │ "), &mut lines);
    lines.push(Line::from(vec![
        Span::styled("  │ ", dim_style()),
        Span::styled(hint.options, dim_style()),
    ]));
    lines
}

fn ansi_spans(text: &str, default_style: Style) -> Vec<Span<'static>> {
    if let Ok(parsed_text) = text.into_text() {
        // Flatten any parsed sub-lines (a stray \r can split one input line)
        // so no styled content is dropped.
        let mut spans = Vec::new();
        for mut parsed in parsed_text.lines {
            for span in &mut parsed.spans {
                if matches!(span.style.fg, None | Some(Color::Reset))
                    && let Some(fg) = default_style.fg
                {
                    span.style = span.style.fg(fg);
                }
            }
            spans.append(&mut parsed.spans);
        }
        if !spans.is_empty() {
            return spans;
        }
    }
    vec![Span::styled(text.to_string(), default_style)]
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

/// Idle status footer (Codex's bottom bar): `model effort · cwd`.
struct Footer {
    /// Model + effort, already joined (e.g. `gpt-5.5 xhigh`).
    model: String,
    /// Working directory, home-relativized to `~` where possible.
    cwd: String,
}

/// Render the idle footer: model+effort accented, cwd in green, dim `·` between.
fn footer_line(footer: &Footer) -> Line<'static> {
    Line::from(vec![
        Span::styled(footer.model.clone(), Style::default().fg(Color::Cyan)),
        Span::styled(" \u{b7} ", dim_style()),
        Span::styled(footer.cwd.clone(), Style::default().fg(Color::Green)),
    ])
}

/// Active-turn status spans: `{spinner} Working ({elapsed} · esc to interrupt)`.
/// Codex parity: the elapsed clause is shown from the first second (`0s`), and
/// the interrupt hint is always present.
fn working_spans(glyph: &str, elapsed: Option<Duration>) -> Vec<Span<'static>> {
    let secs = elapsed.map_or(0, |d| d.as_secs());
    let suffix = format!(
        " ({} \u{b7} esc to interrupt)",
        format_elapsed_compact(secs)
    );
    vec![
        Span::styled(format!("{glyph} "), prompt_style()),
        Span::styled("Working", dim_style()),
        Span::styled(suffix, dim_style()),
    ]
}

/// Whether an event represents concrete turn work (a tool ran). Gates the
/// Codex turn-end separator so purely conversational turns get no empty divider.
fn is_turn_work(event: &UiEvent) -> bool {
    matches!(
        event,
        UiEvent::ToolProposed(_)
            | UiEvent::ToolStarted(_)
            | UiEvent::ToolAutoApproved(_)
            | UiEvent::DiffPreview { .. }
            | UiEvent::ToolDenied(_)
            | UiEvent::ToolResult { .. }
            | UiEvent::ToolOutputDelta { .. }
            | UiEvent::ToolError { .. }
    )
}

/// Build a styled, empty editor: bordered box, dim placeholder, a reversed
/// block cursor the widget draws itself (no hardware cursor needed).
fn fresh_editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_wrap_mode(WrapMode::WordOrGlyph);
    editor.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(dim_style())
            .title(Span::styled(" message ", dim_style())),
    );
    editor.set_cursor_line_style(Style::default());
    editor.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    editor.set_placeholder_text("Type a message, / for commands, Enter to send");
    editor
}

fn editor_visual_rows(editor: &TextArea<'_>, width: u16) -> u16 {
    let inner_width = usize::from(width.saturating_sub(2).max(1)); // editor borders
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
    /// Idle status-row footer (model / effort / cwd), Codex's bottom bar. The
    /// loop refreshes it from the live model selection; `None` falls back to the
    /// keybind hint (e.g. before a provider is selected).
    footer: Option<Footer>,
    /// The active picker/dialog, when one is open. While present it replaces the
    /// editor area and the loop routes keys to it instead of the editor.
    pub(crate) modal: Option<Modal>,
    /// Whether the active turn ran any tool (Codex's "concrete work" gate). The
    /// turn-end separator is emitted only when this is set, so a purely
    /// conversational turn shows no empty divider. Reset at `start_turn`.
    turn_did_work: bool,
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
            turn_did_work: false,
        }
    }

    // --- modal/picker ---

    /// Open a picker/dialog, replacing the editor until it closes.
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
        if is_turn_work(&event) {
            self.turn_did_work = true;
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
    pub(crate) fn set_footer(&mut self, model: String, cwd: String) {
        self.footer = Some(Footer { model, cwd });
    }

    pub(crate) fn start_turn(&mut self) {
        self.spinner.start();
        self.approval_hint = None;
        self.turn_did_work = false;
    }

    pub(crate) fn end_turn(&mut self) {
        let elapsed = self.spinner.elapsed();
        self.spinner.stop();
        self.approval_hint = None;
        // Codex parity: only a turn that did concrete work (ran a tool) gets the
        // turn-end separator; a purely conversational turn shows no empty rule.
        // The rule is labelled with the elapsed time only once a turn ran longer
        // than a minute.
        if self.turn_did_work {
            self.transcript.push_turn_rule(elapsed);
        }
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

    /// Status row content: approval hint > active spinner > footer > idle hint.
    fn status_lines(&self, width: u16) -> Vec<Line<'static>> {
        if let Some(hint) = &self.approval_hint {
            approval_status_lines(hint, usize::from(width))
        } else if self.spinner.active {
            vec![Line::from(working_spans(
                self.spinner.glyph(),
                self.spinner.elapsed(),
            ))]
        } else if let Some(footer) = &self.footer {
            vec![footer_line(footer)]
        } else {
            vec![Line::from(Span::styled(IDLE_HINT, dim_style()))]
        }
    }

    fn status_height(&self, width: u16) -> u16 {
        u16::try_from(self.status_lines(width).len())
            .unwrap_or(u16::MAX)
            .max(1)
    }
}

/// Colorize a unified diff into styled transcript rows. Every file-header pair
/// (`--- ` immediately followed by `+++ ` and a `@@` hunk) is dropped -- not
/// just the first -- so a multi-file diff never renders later files' headers as
/// red/green change lines. Hunk headers cyan, additions green, removals red,
/// context dimmed.
fn diff_rows(diff: &str) -> Vec<TranscriptRow> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if crate::ui::is_diff_file_header(&lines, i) {
            i += 2; // skip the `--- `/`+++ ` pair; the `@@` renders next pass
            continue;
        }
        let style = if line.starts_with("@@") {
            Style::default().fg(Color::Cyan)
        } else {
            match line.chars().next() {
                Some('+') => Style::default().fg(Color::Green),
                Some('-') => Style::default().fg(Color::Red),
                _ => dim_style(),
            }
        };
        out.push(TranscriptRow::new(line, style));
        i += 1;
    }
    out
}

/// Greedy word-wrap `text` to `width` display columns, breaking at spaces. A
/// word that fits is moved whole onto its own row rather than split mid-token,
/// so a URL/path that fits within the width stays selectable as one unit; a
/// single word longer than the width still hard-breaks, because the row-exact
/// rendering model (one logical row = one physical row, see [`TuiUi::draw`])
/// cannot emit an over-wide row without the terminal clipping its tail.
/// Returns at least one row (possibly empty) so a blank logical line still
/// occupies a row.
fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
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

/// Render the slash popup into an offscreen Ratatui buffer: a bordered list with
/// the selected row highlighted.
fn render_palette(buf: &mut Buffer, area: Rect, matches: &[&SlashCommand], selected: usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(dim_style())
        .title(Span::styled(" commands ", dim_style()));
    let inner = block.inner(area);
    block.render(area, buf);
    let mut rows = Vec::new();
    for (i, cmd) in matches.iter().enumerate() {
        let name_style = if i == selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan)
        };
        rows.push(Line::from(vec![
            Span::styled(format!(" {} ", cmd.name), name_style),
            Span::raw(" "),
            Span::styled(cmd.description, dim_style()),
        ]));
    }
    Paragraph::new(Text::from(rows)).render(inner, buf);
}

/// Render the full logical document for the current terminal size: all
/// transcript rows retained in Iris state, plus bottom-pinned modal or
/// status/palette/editor chrome. The terminal surface decides how much of this
/// document can be patched and when it must be fully replayed.
fn render_document(screen: &mut Screen, size: Size) -> Vec<Line<'static>> {
    if size.height == 0 || size.width < 1 {
        return Vec::new();
    }
    let width = size.width.max(1);
    let height = size.height.max(1);
    let mut transcript = screen.wrapped_lines(width);
    let chrome = if screen.modal.is_some() {
        render_modal_chrome(screen, width, height)
    } else {
        render_editor_chrome(screen, width, height)
    };
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
    transcript
}

fn render_editor_chrome(screen: &mut Screen, width: u16, height: u16) -> Vec<Line<'static>> {
    let area = Rect::new(0, 0, width, height);

    let editor_rows = editor_visual_rows(&screen.editor, area.width);
    let input_text = screen.editor_text();
    let palette_active = screen.palette.is_active(&input_text);
    let palette_matches: Vec<&SlashCommand> = if palette_active {
        slash::matches(&input_text)
    } else {
        Vec::new()
    };
    let palette_wanted = if palette_active {
        (palette_matches.len() as u16 + 2).min(MAX_PALETTE_ROWS)
    } else {
        0
    };

    // Bottom-anchored, clamped to the fixed viewport. The editor and a status
    // row are reserved FIRST so a tall slash palette can never starve the
    // editor out of the layout; the palette then takes only what remains, and
    // the active-block region absorbs any slack at the top.
    const MIN_EDITOR_H: u16 = 3; // one text row + top/bottom border
    let palette_h = palette_wanted.min(area.height.saturating_sub(MIN_EDITOR_H).saturating_sub(1));
    let max_editor_h = area
        .height
        .saturating_sub(palette_h)
        .saturating_sub(1)
        .max(1);
    let editor_h = (editor_rows + 2).min(max_editor_h).max(1);
    let desired_status_h = screen.status_height(area.width);
    let max_status_h = area
        .height
        .saturating_sub(editor_h)
        .saturating_sub(palette_h)
        .max(1);
    let status_h = desired_status_h.min(max_status_h);
    let status_lines = screen.status_lines(area.width);

    let chrome_h = status_h.saturating_add(palette_h).saturating_add(editor_h);
    let chrome_area = Rect::new(0, 0, width, chrome_h.max(1));
    let chunks = Layout::vertical([
        Constraint::Length(status_h),
        Constraint::Length(palette_h),
        Constraint::Length(editor_h),
    ])
    .split(chrome_area);
    let status_area = chunks[0];
    let palette_area = chunks[1];
    let editor_area = chunks[2];

    let mut buf = Buffer::empty(chrome_area);
    Paragraph::new(Text::from(status_lines)).render(status_area, &mut buf);

    if palette_h > 0 {
        render_palette(
            &mut buf,
            palette_area,
            &palette_matches,
            screen.palette.selected(),
        );
    }

    // The TextArea draws its own border (set in `fresh_editor`) and cursor.
    (&screen.editor).render(editor_area, &mut buf);
    buffer_to_lines(&buf)
}

/// Render the open modal in a bordered box at the document bottom, in place of
/// the editor/palette/status rows. The transcript remains outside this chrome
/// and is replayable from [`Screen`] state.
fn render_modal_chrome(screen: &mut Screen, width: u16, height: u16) -> Vec<Line<'static>> {
    let Some(modal) = &screen.modal else {
        return Vec::new();
    };
    let lines = modal.render(width.saturating_sub(2));
    let body_rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    // content rows + top/bottom border, clamped to the visible terminal height.
    let max_modal_h = height.max(1);
    // Prefer at least 3 rows (border + one line), but never exceed the available
    // height: on a tiny terminal `max_modal_h` can be 1-2, so cap last. Using
    // `clamp(3, max_modal_h)` here would panic when max < min.
    let modal_h = body_rows.saturating_add(2).max(3).min(max_modal_h);
    let modal_area = Rect::new(0, 0, width, modal_h);
    let mut buf = Buffer::empty(modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(prompt_style())
        .title(Span::styled(format!(" {} ", modal.title()), prompt_style()));
    let inner = block.inner(modal_area);
    block.render(modal_area, &mut buf);
    Paragraph::new(Text::from(lines)).render(inner, &mut buf);
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
        let document = render_document(&mut self.screen, size);
        self.surface.render(size, &document)?;
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
        assert_eq!(texts, vec!["Hello".to_string()]);
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
    fn assistant_text_renders_as_markdown() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText(
            "# Title\n\nuse `cargo test` and:\n- one\n- two".to_string(),
        ));
        let lines = screen.wrapped_lines(80);
        // Heading is bold.
        let title = line_matching(&lines, |l| line_text(l) == "Title");
        assert!(
            title
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
        // Inline code keeps backticks with a distinct color.
        let code_line = line_matching(&lines, |l| line_text(l).contains("cargo test"));
        assert!(
            code_line
                .spans
                .iter()
                .any(|s| s.content.as_ref() == "`cargo test`" && s.style.fg == Some(Color::Cyan))
        );
        // Bullets render with dash markers.
        assert!(lines.iter().any(|l| line_text(l) == "- one"));
        assert!(lines.iter().any(|l| line_text(l) == "- two"));
    }

    #[test]
    fn streaming_markdown_renders_like_finalized_markdown_without_committing_early() {
        let markdown = "# Title\n\nuse `cargo test`\n\n- one";
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextDelta(markdown.to_string()));

        let live = screen.wrapped_lines(80);
        assert!(screen.transcript.rows.is_empty());
        assert!(
            render_document(&mut screen, Size::new(80, 12))
                .iter()
                .any(|line| line_text(line) == "Title")
        );

        let title = line_matching(&live, |l| line_text(l) == "Title");
        assert!(
            title
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
        let code_line = line_matching(&live, |l| line_text(l).contains("cargo test"));
        assert!(
            code_line
                .spans
                .iter()
                .any(|s| s.content.as_ref() == "`cargo test`" && s.style.fg == Some(Color::Cyan))
        );
        assert!(live.iter().any(|l| line_text(l) == "- one"));

        screen.apply(UiEvent::AssistantTextEnd(markdown.to_string()));
        let finalized = screen.wrapped_lines(80);
        assert_eq!(line_signature(&live), line_signature(&finalized));
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
        assert_eq!(screen.transcript.rows.len(), 1);
        assert!(screen.wrapped_lines(12).len() >= 2);
    }

    #[test]
    fn bash_tool_result_uses_codex_style_gutters() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "echo hi" })),
            content: "a\nb".to_string(),
            exit_code: None,
            duration: None,
        });
        let texts: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert!(texts.iter().any(|t| t == "• Ran echo hi"));
        assert!(texts.iter().any(|t| t == "  └ a"));
        assert!(texts.iter().any(|t| t == "    b"));
        assert!(!texts.iter().any(|t| t.starts_with("[ok]")));
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
        assert_eq!(line_text(output), "  └ red plain");
        assert_eq!(output.spans[0].content.as_ref(), "  └ ");
        assert_eq!(output.spans[0].style, dim_style());
        assert_eq!(output.spans[1].content.as_ref(), "red");
        assert_eq!(output.spans[1].style, Style::default().fg(Color::Red));
        assert_eq!(output.spans[2].content.as_ref(), " plain");
        assert_eq!(output.spans[2].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn committed_user_block_carries_full_width_background() {
        // The shaded user block must keep its full-width background in the
        // replayable transcript document, not only while being edited live.
        let mut screen = Screen::new();
        screen.commit_user("hello");
        let lines = screen.wrapped_lines(20);
        let row = lines
            .iter()
            .find(|l| line_text(l).contains("hello"))
            .expect("user row rendered from state");
        assert_eq!(row.style.bg, Some(USER_BG));
        assert_eq!(display_width(&line_text(row)), 20);
        assert!(
            row.spans
                .iter()
                .all(|s| s.style.bg.is_none() || s.style.bg == Some(USER_BG)),
            "every span inherits or sets the shaded background: {row:?}"
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
        assert!(texts.iter().any(|t| t == "  └ line 0"), "{texts:?}");
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
    fn empty_tool_output_shows_no_output_line() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "true" })),
            content: String::new(),
            exit_code: None,
            duration: None,
        });
        let texts: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert!(texts.iter().any(|t| t == "• Ran true"));
        assert!(texts.iter().any(|t| t == "  └ (no output)"));
    }

    #[test]
    fn approval_hint_names_tool_target() {
        let mut screen = Screen::new();
        screen.show_approval(&call_args("bash", json!({ "command": "echo hi" })), false);
        let lines = screen.status_lines(80);
        assert!(line_text(&lines[0]).contains("approve echo hi"));
    }

    #[test]
    fn approval_hint_wraps_in_narrow_frame() {
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
        let rendered = rendered_text(&mut screen, 48, 10);
        assert!(rendered.contains("approve printf 'global:"));
        assert!(rendered.contains("  │ "));
        assert!(rendered.contains("(timeout 120s)"));
        assert!(rendered.contains("[N] deny"), "{rendered}");
    }

    #[test]
    fn approval_record_styles_only_marker_green() {
        let mut screen = Screen::new();
        screen.record_approval(
            &call_args("bash", json!({ "command": "echo hi" })),
            ApprovalDecision::Allow,
        );
        let lines = screen.wrapped_lines(80);
        let line = line_matching(&lines, |line| line_text(line).contains("You approved"));
        assert_eq!(line.spans[0].content.as_ref(), "✔");
        assert_eq!(line.spans[0].style, ok_style());
        assert_eq!(
            line.spans[1].content.as_ref(),
            " You approved iris to run echo hi this time"
        );
        assert_eq!(line.spans[1].style, Style::default());
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
    fn read_only_tool_results_group_as_explored() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call("read"),
            content: "ignored file body".to_string(),
            exit_code: None,
            duration: None,
        });
        screen.apply(UiEvent::ToolResult {
            call: call_args("grep", json!({ "pattern": "needle", "path": "src" })),
            content: "ignored grep body".to_string(),
            exit_code: None,
            duration: None,
        });
        let texts: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert_eq!(
            texts,
            vec![
                "• Explored".to_string(),
                "  └ Read note.txt".to_string(),
                "  └ Search needle in src".to_string(),
            ]
        );
    }

    #[test]
    fn exploration_tool_start_names_target_live_and_finalizes_in_place() {
        let mut screen = Screen::new();
        screen.start_turn();
        let read = call_args("read", json!({ "path": "src/ui/tui.rs" }));

        screen.apply(UiEvent::ToolStarted(read.clone()));
        let live: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert_eq!(
            live,
            vec![
                "• Explored".to_string(),
                "  └ Reading src/ui/tui.rs".to_string(),
            ]
        );
        assert!(
            screen
                .wrapped_lines(80)
                .iter()
                .any(|line| line_text(line).contains("Reading src/ui/tui.rs"))
        );

        screen.apply(UiEvent::ToolResult {
            call: read,
            content: "ignored file body".to_string(),
            exit_code: None,
            duration: None,
        });
        let done: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert_eq!(
            done,
            vec![
                "• Explored".to_string(),
                "  └ Read src/ui/tui.rs".to_string(),
            ]
        );
    }

    #[test]
    fn long_exploration_rows_keep_gutter_when_wrapped() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args(
                "grep",
                json!({
                    "pattern": "bordered editor|editor box|editor borders|borderless|shaded editor",
                    "path": "src/ui",
                    "glob": "tui.rs",
                }),
            ),
            content: "ignored grep body".to_string(),
            exit_code: None,
            duration: None,
        });

        let lines: Vec<String> = screen.wrapped_lines(36).iter().map(line_text).collect();
        let first = lines
            .iter()
            .find(|line| line.contains("Search"))
            .expect("search row");
        assert!(first.starts_with("  └ "), "{lines:?}");
        assert!(
            lines.iter().filter(|line| line.starts_with("    ")).count() > 0,
            "wrapped search row lost continuation gutter: {lines:?}"
        );
    }

    #[test]
    fn long_run_header_wraps_with_vertical_gutter() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args(
                "bash",
                json!({ "command": "node -e 'const fs=require(\"fs\"); const p=\"CHANGELOG.md\"; console.log(p)'" }),
            ),
            content: String::new(),
            exit_code: None,
            duration: None,
        });
        let lines: Vec<String> = screen.wrapped_lines(34).iter().map(line_text).collect();
        assert!(lines.iter().any(|line| line.starts_with("• Ran node -e")));
        assert!(lines.iter().any(|line| line.starts_with("  │ ")));
        assert!(lines.iter().any(|line| line == "  └ (no output)"));
    }

    #[test]
    fn long_run_header_gutter_does_not_inherit_bold() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args(
                "bash",
                json!({ "command": "rm -rf tmp && mkdir -p tmp && printf 'Created tmp directory\\n' && pwd && ls -ld tmp", "timeout": 120 }),
            ),
            content: "Created tmp directory\n/home/someotherguy/projects/iris-tool-output\ndrwxrwxr-x 2 someotherguy someotherguy 4096 Jun 17 23:59 tmp".to_string(),
            exit_code: None,
            duration: None,
        });
        let lines = screen.wrapped_lines(80);
        let continuation = line_matching(&lines, |line| line_text(line).starts_with("  │ "));
        assert_eq!(continuation.spans.len(), 2);
        assert_eq!(continuation.spans[0].content.as_ref(), "  │ ");
        assert_eq!(continuation.spans[0].style, dim_style());
        assert_eq!(continuation.spans[1].style, tool_header_style());
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
        assert!(texts.iter().any(|t| t == "+new"));
        assert!(texts.iter().any(|t| t == "-old"));
        let add = screen
            .transcript
            .rows
            .iter()
            .find(|row| row.text == "+new")
            .expect("addition row");
        let remove = screen
            .transcript
            .rows
            .iter()
            .find(|row| row.text == "-old")
            .expect("removal row");
        assert_eq!(add.style, Style::default().fg(Color::Green));
        assert_eq!(remove.style, Style::default().fg(Color::Red));
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
        assert!(texts.iter().any(|t| t == "+new1"));
        assert!(texts.iter().any(|t| t == "+new2"));
        assert!(texts.iter().any(|t| t == "-old2"));
        // The second file's removal is red, not styled as plain context.
        let remove2 = screen
            .transcript
            .rows
            .iter()
            .find(|row| row.text == "-old2")
            .expect("second removal row");
        assert_eq!(remove2.style, Style::default().fg(Color::Red));
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
        assert!(rendered.contains("message"), "editor missing: {rendered:?}");
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

        let replay = surface.state().previous_lines.join("\n");
        assert!(replay.contains("hello there"), "{replay:?}");
        assert!(replay.contains("Done"), "{replay:?}");
        assert!(replay.contains("Ran echo hi"), "{replay:?}");
        assert!(replay.contains("message"), "{replay:?}");
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
    fn repeated_resize_does_not_duplicate_editor_shadow() -> std::io::Result<()> {
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
        assert_eq!(replay.matches(" message ").count(), 1, "{replay:?}");
        assert_eq!(replay.matches("Type a message").count(), 1, "{replay:?}");
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
                .contains("Select model")
        );

        screen.close_modal();
        let stats = surface.render(Size::new(60, 14), &rendered_lines(&mut screen, 60, 14))?;
        let replay = strip_ansi(&surface.state().previous_lines.join("\n"));
        assert_ne!(stats.kind, RenderKind::Unchanged);
        assert!(!replay.contains("Select model"), "{replay:?}");
        assert!(replay.contains("message"), "{replay:?}");
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

        // A 3-row (and 2-row) terminal must not panic on the modal height clamp.
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
    fn open_modal_renders_picker_frame_in_place_of_editor() {
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
        // Transcript still on top; the modal frame replaces the editor below.
        assert!(rendered.contains("prior reply"), "{rendered}");
        assert!(rendered.contains("Select model"), "{rendered}");
        assert!(rendered.contains("GPT 5.5"), "{rendered}");
        assert!(rendered.contains("Sonnet 4.6"), "{rendered}");
        // The editor placeholder is hidden while the modal is open.
        assert!(!rendered.contains("Type a message"), "{rendered}");
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
        assert!(rendered.contains("xHigh effort"), "{rendered}");
        assert!(rendered.contains("Enter to set as default"), "{rendered}");
    }

    #[test]
    fn frame_pins_editor_box_below_transcript() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText("hello world".to_string()));
        screen.editor.insert_str("hi");

        let rendered = rendered_text(&mut screen, 40, 8);
        assert!(rendered.contains("hello world"));
        // The editor text sits inside the bordered box near the bottom.
        assert!(rendered.contains("hi"));
        // Idle status hint is shown when no turn runs (the long hint is
        // truncated at this narrow test width, so assert its leading words).
        assert!(rendered.contains("enter send"));
    }

    #[test]
    fn long_editor_line_wraps_instead_of_scrolling_right() {
        let mut screen = Screen::new();
        screen.editor.insert_str("abcdefghijklmnopqrst");

        let rendered = rendered_text(&mut screen, 18, 8);
        // The editor inner width is 16 cells (18-wide frame minus borders), so
        // a 20-cell word should use two visible rows instead of horizontally
        // scrolling to the tail of the line.
        assert!(rendered.contains("abcdefghijklmnop"));
        assert!(rendered.contains("qrst"));
    }

    #[test]
    fn frame_shows_spinner_while_turn_active() {
        let mut screen = Screen::new();
        screen.start_turn();
        let before = rendered_text(&mut screen, 40, 8);
        assert!(before.contains("Working"));
        assert!(before.contains("esc to interrupt"));

        // A tick advances the spinner glyph (animation), idle does not.
        let glyph0 = SPINNER_FRAMES[0];
        assert!(before.contains(glyph0));
        assert!(screen.tick());
        let after = rendered_text(&mut screen, 40, 8);
        assert!(after.contains(SPINNER_FRAMES[1]));

        screen.end_turn();
        assert!(!screen.tick());
        let idle = rendered_text(&mut screen, 40, 8);
        assert!(
            idle.contains("enter send"),
            "idle hint replaces the spinner"
        );
        assert!(!idle.contains("Working"), "spinner cleared on turn end");
    }

    #[test]
    fn footer_replaces_idle_hint_when_set() {
        let mut screen = Screen::new();
        // No footer wired: the keybind hint shows.
        assert!(line_text(&screen.status_lines(80)[0]).contains("enter send"));
        screen.set_footer("gpt-5.5 xhigh".to_string(), "~".to_string());
        let text = line_text(&screen.status_lines(80)[0]);
        assert!(text.contains("gpt-5.5 xhigh"), "{text}");
        assert!(text.contains('~'), "{text}");
        assert!(!text.contains("enter send"), "{text}");
    }

    #[test]
    fn working_status_shows_elapsed_from_the_first_second() {
        // Codex parity: elapsed is shown immediately (`0s`), not gated on a
        // minute, and the interrupt hint is always present.
        let early: String = working_spans("\u{280b}", Some(Duration::from_secs(5)))
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(early.contains("Working"), "{early}");
        assert!(early.contains("5s \u{b7} esc to interrupt"), "{early}");
        let zero: String = working_spans("\u{280b}", None)
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(zero.contains("0s \u{b7} esc to interrupt"), "{zero}");
        let over: String = working_spans("\u{280b}", Some(Duration::from_secs(71)))
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(over.contains("1m 11s \u{b7} esc to interrupt"), "{over}");
    }

    #[test]
    fn user_message_renders_shaded_block_with_prompt_glyph() {
        let mut screen = Screen::new();
        screen.commit_user("hello\nworld");
        let lines = screen.wrapped_lines(20);
        let first = line_matching(&lines, |l| line_text(l).contains("hello"));
        // The prompt glyph opens the block; the whole row is shaded and padded
        // to the full width.
        assert_eq!(first.spans[0].content.as_ref(), USER_PREFIX);
        assert_eq!(first.style.bg, Some(USER_BG));
        assert_eq!(display_width(&line_text(first)), 20);
        // Continuation input line indents two columns and stays shaded.
        let second = line_matching(&lines, |l| line_text(l).contains("world"));
        assert_eq!(second.spans[0].content.as_ref(), "  ");
        assert_eq!(second.style.bg, Some(USER_BG));
    }

    #[test]
    fn end_turn_appends_dim_turn_rule_after_tool_work() {
        let mut screen = Screen::new();
        screen.start_turn();
        // A tool ran, so this is "concrete work" and earns the turn rule.
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "echo hi" })),
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: None,
        });
        screen.end_turn();
        // A short turn (<60s) closes with a plain full-width rule (no label).
        let lines = screen.wrapped_lines(20);
        let rule = line_matching(&lines, |l| line_text(l).starts_with('\u{2500}'));
        assert_eq!(line_text(rule), "\u{2500}".repeat(20));
        assert_eq!(rule.spans[0].style, dim_style());
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
        screen.editor.insert_str("/e");
        screen.sync_palette();
        let rendered = rendered_text(&mut screen, 40, 10);
        assert!(rendered.contains("/exit"));
        assert!(!rendered.contains("/quit"), "filtered to /exit only");
    }

    #[test]
    fn tool_started_opens_running_cell_in_replay_state() {
        let mut screen = Screen::new();
        screen.start_turn();
        let call = call_args("bash", json!({ "command": "echo hi" }));
        screen.apply(UiEvent::ToolStarted(call));
        // The live viewport shows the Running header.
        let live: Vec<String> = screen.wrapped_lines(80).iter().map(line_text).collect();
        assert!(
            live.iter().any(|l| l.contains("\u{2022} Running echo hi")),
            "running header not live: {live:?}"
        );
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|row| row.text.contains("Running echo hi")),
            "running cell must remain in Iris replay state"
        );
    }

    #[test]
    fn tool_output_deltas_append_under_gutter_and_are_flood_capped() {
        let mut screen = Screen::new();
        screen.start_turn();
        let _ = screen.wrapped_lines(80); // prime last_width
        let call = call_args("bash", json!({ "command": "flood" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        // Stream a flood of long lines; the growing tail must respect the cap.
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
        // Still streaming: the Running header is present, not yet finalized.
        assert!(lines.iter().any(|l| line_text(l).contains("Running flood")));
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
    fn exec_cell_finalizes_in_place_with_green_bullet_and_duration() {
        let mut screen = Screen::new();
        screen.start_turn();
        let call = call_args("bash", json!({ "command": "echo hi" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "hi\n".to_string(),
        });
        let sep_before = screen
            .transcript
            .rows
            .iter()
            .filter(|r| is_separator_row(r))
            .count();
        screen.apply(UiEvent::ToolResult {
            call: call.clone(),
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: Some(std::time::Duration::from_millis(1200)),
        });
        // Finalize must not open a new block (same separator count).
        let sep_after = screen
            .transcript
            .rows
            .iter()
            .filter(|r| is_separator_row(r))
            .count();
        assert_eq!(sep_before, sep_after, "finalize opened a new block");
        let lines = screen.wrapped_lines(80);
        assert!(
            !lines.iter().any(|l| line_text(l).contains("Running")),
            "Running header not replaced in place"
        );
        let header = line_matching(&lines, |l| line_text(l).contains("Ran echo hi"));
        assert_eq!(header.spans[0].content.as_ref(), "\u{2022}");
        assert_eq!(
            header.spans[0].style,
            ok_style().add_modifier(Modifier::BOLD)
        );
        assert!(
            line_text(header).contains("(1.2s)"),
            "duration suffix missing: {}",
            line_text(header)
        );
    }

    #[test]
    fn exec_cell_nonzero_exit_shows_red_bold_bullet() {
        let mut screen = Screen::new();
        screen.start_turn();
        let call = call_args("bash", json!({ "command": "false" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call: call.clone(),
            content: "boom".to_string(),
            exit_code: Some(1),
            duration: Some(std::time::Duration::from_millis(50)),
        });
        let lines = screen.wrapped_lines(80);
        let header = line_matching(&lines, |l| line_text(l).contains("Ran false"));
        // Codex parity: a failed command (non-zero exit) keeps the bullet glyph,
        // colored red and bold; `✗` is reserved for true tool errors.
        assert_eq!(header.spans[0].content.as_ref(), "\u{2022}");
        assert_eq!(
            header.spans[0].style,
            err_style().add_modifier(Modifier::BOLD)
        );
        assert!(line_text(header).contains("(50ms)"));
    }

    #[test]
    fn exec_cell_error_keeps_streamed_output() {
        let mut screen = Screen::new();
        screen.start_turn();
        let call = call_args("bash", json!({ "command": "sleep 9" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "partial line\n".to_string(),
        });
        // Cancellation finalizes as a ToolError but keeps whatever streamed.
        screen.apply(UiEvent::ToolError {
            call: call.clone(),
            message: "cancelled".to_string(),
        });
        let lines: Vec<String> = screen.wrapped_lines(80).iter().map(line_text).collect();
        assert!(lines.iter().any(|l| l.contains("\u{2717} Ran sleep 9")));
        assert!(
            lines.iter().any(|l| l.contains("partial line")),
            "streamed output lost on error finalize: {lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("error: cancelled")));
    }

    #[test]
    fn streamed_exec_cell_replays_from_state_after_finalize() -> std::io::Result<()> {
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

        let everything = surface.state().previous_lines.join("\n");
        assert!(
            everything.contains("Ran echo hi"),
            "finalized exec cell missing from terminal replay: {everything:?}"
        );
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|row| row.text.contains("Ran echo hi")),
            "exec rows must remain replayable from Iris state"
        );
        Ok(())
    }
}
