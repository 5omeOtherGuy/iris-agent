//! Full-screen terminal front-end state and rendering (Tier 3) built on ratatui.
//!
//! Layering: ratatui owns the cell buffer, the frame diff, and width-aware
//! layout; `ratatui-textarea` owns the editor buffer (multiline + undo/redo +
//! kill-ring + word-nav) over that same buffer; this module is the thin layer on
//! top. [`Screen`] holds the UI state (transcript, editor, spinner, slash
//! palette) and renders it into a frame; [`TuiUi`] owns the terminal lifecycle
//! (one persistent alternate-screen + raw-mode session). The async input/render
//! loop that drives them lives in [`crate::ui::tui_loop`].
//!
//! Concurrency / cancellation: raw mode is entered ONCE for the whole session,
//! so Ctrl-C arrives as a key event, never SIGINT; the loop (not this module)
//! reads keys and cancels the turn token. This module performs no terminal
//! reads and holds no channels, so its state transitions and frame output are
//! unit-testable via ratatui's `TestBackend` without a TTY.

use std::io::{self, Stdout};

use ansi_to_tui::IntoText;
use anyhow::Result;
use ratatui::backend::{Backend, ClearType, CrosstermBackend};
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::layout::{Constraint, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use ratatui_textarea::{TextArea, WrapMode};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::nexus::{ApprovalDecision, ToolCall};
use crate::tool_display::{exploration_summary, is_exploration_tool, run_target, summarize};
use crate::ui::modal::Modal;
use crate::ui::slash::{self, Palette, SlashCommand};
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

/// Height of the persistent inline live viewport (the small region that
/// repaints every frame: active in-flight block tail + status + palette +
/// editor). Everything finalized is committed above it into the terminal's
/// native scrollback via `insert_before`. ratatui's inline viewport height is
/// fixed at construction, so this reserves enough room for the `/model` modal
/// mockup plus the current catalog while keeping normal editor slack bounded.
const LIVE_VIEWPORT_ROWS: u16 = 16;

/// Cap on rows committed in a single `insert_before` call, so one finalized
/// block never allocates an unbounded scratch buffer; larger blocks are
/// committed in successive chunks (order preserved).
const MAX_INSERT_ROWS: usize = 500;

/// Flood guard: cap a tool result at this many physical (wrapped) rows in the
/// transcript so a few very long lines cannot flood the viewport/scrollback.
/// The model still receives the full output; only the terminal preview is
/// bounded, and the omitted logical-line count is reported.
const MAX_TOOL_OUTPUT_ROWS: usize = 24;

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

fn user_style() -> Style {
    Style::default().fg(Color::Cyan)
}
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

/// Bullet glyph + style for a finalized exec cell: a green bullet on success
/// (`Some(0)` or no reported status) and a red cross on a non-zero exit.
fn exec_status(exit_code: Option<i32>) -> (&'static str, Style) {
    match exit_code {
        Some(0) | None => ("\u{2022}", ok_style()),
        Some(_) => ("\u{2717}", err_style()),
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
}

impl TranscriptRow {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            continuation_prefix: None,
            line: None,
            word_wrap: false,
        }
    }

    fn with_continuation(
        text: impl Into<String>,
        style: Style,
        continuation_prefix: &'static str,
    ) -> Self {
        Self {
            text: text.into(),
            style,
            continuation_prefix: Some(continuation_prefix),
            line: None,
            word_wrap: false,
        }
    }

    fn with_line(line: Line<'static>, continuation_prefix: Option<&'static str>) -> Self {
        Self {
            text: line_text(&line),
            style: Style::default(),
            continuation_prefix,
            line: Some(line),
            word_wrap: false,
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
        }
    }

    fn render(&self, width: usize, out: &mut Vec<Line<'static>>) {
        match &self.line {
            Some(line) if self.word_wrap => push_wrapped_line_wordwise(line, width, out),
            Some(line) => push_wrapped_line(line, width, self.continuation_prefix, out),
            None => push_wrapped_row(&self.text, self.style, width, self.continuation_prefix, out),
        }
    }
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// A block-separator row: the empty plain row `push_blank` inserts between
/// top-level blocks. Distinguished from a Markdown-internal blank line (which
/// carries a styled `line`) so only true block boundaries split scrollback
/// commits.
fn is_separator_row(row: &TranscriptRow) -> bool {
    row.text.is_empty() && row.line.is_none()
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
/// block body (its `Running`/`Ran` header); `take_scrollback` keeps it valid
/// across scrollback drains. `output` is the bounded live tail re-rendered (and
/// flood-capped) under the gutter on each delta.
struct ActiveExec {
    call: ToolCall,
    output: String,
    body_start: usize,
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
            self.rows.push(TranscriptRow::with_continuation(
                line,
                style,
                continuation_prefix,
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

    /// Index of the last block separator (a blank `push_blank` row), or 0 if
    /// none. Rows before it form complete blocks safe to commit to scrollback;
    /// the rows from it onward are the current, still-growable block (so an
    /// `Explored` group or a streamed message can keep appending to it).
    fn last_separator_index(&self) -> usize {
        self.rows.iter().rposition(is_separator_row).unwrap_or(0)
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
        // Flood-safe: wrap each output line to the transcript width FIRST, then
        // cap the total *physical* rows so a handful of very long lines cannot
        // flood the viewport/scrollback. The omitted count is logical lines.
        // This is the HEAD-capped rendering used for a finalized result (matches
        // the established `• Ran` look); the live cell uses the tail variant.
        let width = self.wrap_width();
        let total_logical = content.lines().count();
        let mut physical = 0usize;
        let mut shown = 0usize;
        for raw in content.lines() {
            let rows =
                wrapped_row_estimate(&truncate_chars(raw, MAX_TOOL_OUTPUT_LINE_CHARS), width);
            // Always show at least the first line (head); otherwise stop once
            // the next line would exceed the physical-row budget.
            if shown > 0 && physical + rows > MAX_TOOL_OUTPUT_ROWS {
                break;
            }
            self.push_output_line(raw, shown == 0);
            physical += rows;
            shown += 1;
        }
        let hidden = total_logical.saturating_sub(shown);
        if hidden > 0 {
            self.push(&format!("    … +{hidden} lines"), dim_style());
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
            // earlier-lines note took it.
            self.push_output_line(raw, start == 0 && offset == 0);
        }
    }

    fn push_explored_result(&mut self, call: &ToolCall) {
        self.finish_stream();
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
                // Exploration tools (read/grep/find/ls) stay grouped under
                // `Explored` and render nothing until their result; only a
                // non-exploration tool opens a live `Running` exec cell.
                if is_exploration_tool(&call) {
                    self.finish_stream();
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
                        // Bound the re-rendered buffer to its tail; only ~24 rows
                        // ever show and the full output arrives with the result.
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
                } else {
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
    }

    /// Commit a submitted prompt into the transcript as a user line.
    fn commit_user(&mut self, text: &str) {
        self.push_blank();
        for line in text.split('\n') {
            self.push(&format!("> {line}"), user_style());
        }
    }

    fn render(&mut self, width: u16) -> Vec<Line<'static>> {
        let width = usize::from(width);
        self.last_width = width;
        let mut rows = Vec::new();
        for row in &self.rows {
            row.render(width, &mut rows);
        }
        if let Some(text) = &self.streaming {
            for line in crate::ui::markdown::render_markdown(text) {
                TranscriptRow::markdown(line).render(width, &mut rows);
            }
        }
        rows
    }
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
/// session redraws nothing on a tick (no flicker, no busy CPU).
#[derive(Default)]
struct Spinner {
    active: bool,
    frame: usize,
}

struct ApprovalHint {
    target: String,
    options: &'static str,
}

impl Spinner {
    fn start(&mut self) {
        self.active = true;
        self.frame = 0;
    }

    fn stop(&mut self) {
        self.active = false;
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
/// behavior is unit-testable without a TTY and its frame output is testable via
/// ratatui's `TestBackend`.
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
    /// The active picker/dialog, when one is open. While present it replaces the
    /// editor area and the loop routes keys to it instead of the editor.
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
            modal: None,
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
    /// Nexus event; committed history is moved to native scrollback on draw.
    pub(crate) fn apply_event(&mut self, event: UiEvent) {
        self.apply(event);
    }

    /// Apply one semantic event to the transcript.
    pub(crate) fn apply(&mut self, event: UiEvent) {
        self.transcript.apply(event);
    }

    /// Commit a submitted prompt into the transcript as a user line.
    pub(crate) fn commit_user(&mut self, text: &str) {
        self.transcript.commit_user(text);
    }

    // --- scrollback commit ---

    /// Drain the finalized transcript rows, wrapped to `width`, for committing
    /// into the terminal's native scrollback. While a turn is active the
    /// current (still-growing) block stays live in the viewport; between turns
    /// everything is finalized so the viewport holds only the editor/status.
    pub(crate) fn take_scrollback(&mut self, width: u16) -> Vec<Line<'static>> {
        let w = usize::from(width.max(1));
        self.transcript.last_width = w;
        let commit_point = if self.spinner.active {
            self.transcript.last_separator_index()
        } else {
            self.transcript.rows.len()
        };
        let drained: Vec<TranscriptRow> = self.transcript.rows.drain(..commit_point).collect();
        // The drain shifts every surviving row down by `commit_point`; keep the
        // open exec cell's body anchor pointing at its (now-relocated) header so
        // in-place finalize/relayout still target the right rows. `commit_point`
        // never exceeds the active block's start, so this stays non-negative.
        if let Some(active) = self.transcript.active_exec.as_mut() {
            active.body_start = active.body_start.saturating_sub(commit_point);
        }
        let mut out = Vec::new();
        for row in &drained {
            row.render(w, &mut out);
        }
        out
    }

    /// Render the live (not-yet-finalized) transcript rows plus any in-flight
    /// stream, wrapped to `width`. This is the bounded active-block content
    /// shown in the viewport above the editor.
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

    pub(crate) fn start_turn(&mut self) {
        self.spinner.start();
        self.approval_hint = None;
    }

    pub(crate) fn end_turn(&mut self) {
        self.spinner.stop();
        self.approval_hint = None;
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

    /// Status row content: approval hint > spinner > idle hint.
    fn status_lines(&self, width: u16) -> Vec<Line<'static>> {
        if let Some(hint) = &self.approval_hint {
            approval_status_lines(hint, usize::from(width))
        } else if self.spinner.active {
            vec![Line::from(vec![
                Span::styled(format!("{} ", self.spinner.glyph()), prompt_style()),
                Span::styled("working", dim_style()),
            ])]
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

/// Render the slash popup: a bordered list with the selected row highlighted.
fn render_palette(frame: &mut Frame, area: Rect, matches: &[&SlashCommand], selected: usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(dim_style())
        .title(Span::styled(" commands ", dim_style()));
    let inner = block.inner(area);
    frame.render_widget(block, area);
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
    frame.render_widget(Paragraph::new(Text::from(rows)), inner);
}

/// Render the small live viewport: the active in-flight block (its tail) on
/// top, a status/spinner row, an optional slash popup, then the bordered editor
/// pinned at the bottom. Finalized history is NOT drawn here -- it already lives
/// in the terminal's native scrollback above this viewport (see
/// [`TuiUi::draw`]). The editor/palette are clamped to the fixed viewport
/// height; the editor scrolls internally beyond its visible rows.
fn render(frame: &mut Frame, screen: &mut Screen) {
    let area = frame.area();
    if area.height == 0 || area.width < 1 {
        return;
    }

    // A picker/dialog replaces the editor area entirely: the active transcript
    // stays on top, the modal occupies the bottom, framed in its own border.
    if screen.modal.is_some() {
        render_with_modal(frame, screen, area);
        return;
    }

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

    let chunks = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(status_h),
        Constraint::Length(palette_h),
        Constraint::Length(editor_h),
    ])
    .split(area);
    let active_area = chunks[0];
    let status_area = chunks[1];
    let palette_area = chunks[2];
    let editor_area = chunks[3];

    // Active in-flight block: show its tail so the newest live output (a
    // streaming reply or the most recent tool block) stays visible until it is
    // finalized into scrollback.
    if active_area.height > 0 {
        let lines = screen.wrapped_lines(active_area.width);
        let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let scroll = total.saturating_sub(active_area.height);
        frame.render_widget(
            Paragraph::new(Text::from(lines)).scroll((scroll, 0)),
            active_area,
        );
    }

    frame.render_widget(Paragraph::new(Text::from(status_lines)), status_area);

    if palette_h > 0 {
        render_palette(
            frame,
            palette_area,
            &palette_matches,
            screen.palette.selected(),
        );
    }

    // The TextArea draws its own border (set in `fresh_editor`) and cursor.
    frame.render_widget(&screen.editor, editor_area);
}

/// Render the active transcript on top and the open modal in a bordered box at
/// the bottom, in place of the editor/palette/status rows. The modal box is
/// sized to its content and may use the whole live viewport when needed.
fn render_with_modal(frame: &mut Frame, screen: &mut Screen, area: Rect) {
    let Some(modal) = &screen.modal else {
        return;
    };
    let lines = modal.render(area.width.saturating_sub(2));
    let body_rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    // content rows + top/bottom border, clamped to the live viewport.
    let max_modal_h = area.height.max(1);
    // Prefer at least 3 rows (border + one line), but never exceed the available
    // height: on a tiny terminal `max_modal_h` can be 1-2, so cap last. Using
    // `clamp(3, max_modal_h)` here would panic when max < min.
    let modal_h = body_rows.saturating_add(2).max(3).min(max_modal_h);
    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(modal_h)]).split(area);
    let active_area = chunks[0];
    let modal_area = chunks[1];

    if active_area.height > 0 {
        let active_lines = screen.transcript.render(active_area.width);
        let total = u16::try_from(active_lines.len()).unwrap_or(u16::MAX);
        let scroll = total.saturating_sub(active_area.height);
        frame.render_widget(
            Paragraph::new(Text::from(active_lines)).scroll((scroll, 0)),
            active_area,
        );
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(prompt_style())
        .title(Span::styled(format!(" {} ", modal.title()), prompt_style()));
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

/// Commit finalized transcript `lines` into the terminal's native scrollback,
/// above the inline live viewport, in row-bounded chunks (order preserved).
/// Generic over the backend so it is exercisable with `TestBackend`.
fn insert_lines_before<B: Backend>(
    terminal: &mut Terminal<B>,
    lines: Vec<Line<'static>>,
) -> Result<(), B::Error> {
    for chunk in lines.chunks(MAX_INSERT_ROWS) {
        let height = u16::try_from(chunk.len()).unwrap_or(u16::MAX);
        let text = Text::from(chunk.to_vec());
        terminal.insert_before(height, move |buf| {
            Paragraph::new(text).render(buf.area, buf);
        })?;
    }
    Ok(())
}

fn render_without_autoresize<B: Backend>(
    terminal: &mut Terminal<B>,
    screen: &mut Screen,
) -> Result<(), B::Error> {
    {
        let mut frame = terminal.get_frame();
        render(&mut frame, screen);
    }
    terminal.apply_buffer_with_cursor(None)?;
    Ok(())
}

/// Terminal driver: owns the ratatui terminal and the persistent raw-mode +
/// inline-viewport lifecycle for the whole interactive session. It does NOT
/// enter the alternate screen: finalized history is committed into the
/// terminal's native scrollback (selectable, copyable, scrolled by the real
/// terminal) above a small inline live viewport. Reads no input itself;
/// [`crate::ui::tui_loop`] feeds it events and calls [`TuiUi::draw`].
pub(crate) struct TuiUi {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    pub(crate) screen: Screen,
    native_scrollback: bool,
    allow_resize_probe: bool,
    last_size: Size,
    active: bool,
}

impl TuiUi {
    /// Enter raw mode ONCE and create an inline viewport for the session, plus
    /// bracketed paste and modified-key reporting. Mouse capture is deliberately NOT enabled so the
    /// terminal's own scroll/select/copy works over the native scrollback the
    /// inline viewport writes into. Restored on `drop`/`shutdown`, and by the
    /// signal handler's emergency escape on a force-quit.
    pub(crate) fn new() -> Result<Self> {
        // Capture cooked-mode termios before raw mode so the force-quit signal
        // handler can restore the tty even though Drop will not run then.
        crate::signals::save_termios_for_force_quit();
        enable_raw_mode()?;
        let backend = CrosstermBackend::new(io::stdout());
        let options = TerminalOptions {
            viewport: Viewport::Inline(LIVE_VIEWPORT_ROWS),
        };
        let (terminal, native_scrollback) = match Terminal::with_options(backend, options) {
            Ok(terminal) => (terminal, true),
            Err(error) if is_cursor_position_timeout(&error) => {
                // ponytail: full-screen TUI fallback. Native scrollback needs
                // an inline cursor probe; if this terminal cannot answer it,
                // keep the TUI alive and let the in-frame transcript hold
                // history. Upgrade path: switch input to an interruptible
                // event stream so inline resize probes can be serialized.
                match Terminal::new(CrosstermBackend::new(io::stdout())) {
                    Ok(terminal) => (terminal, false),
                    Err(error) => {
                        let _ = disable_raw_mode();
                        return Err(error.into());
                    }
                }
            }
            Err(error) => {
                let _ = disable_raw_mode();
                return Err(error.into());
            }
        };
        let last_size = terminal.size()?;
        if let Err(error) = execute!(
            io::stdout(),
            EnableBracketedPaste,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        ) {
            let _ = execute!(
                io::stdout(),
                DisableBracketedPaste,
                PopKeyboardEnhancementFlags,
            );
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        crate::signals::enable_terminal_restore_on_force_quit();
        crate::telemetry::set_tui_active(true);
        Ok(Self {
            terminal,
            screen: Screen::new(),
            native_scrollback,
            allow_resize_probe: true,
            last_size,
            active: true,
        })
    }

    fn fallback_to_fullscreen(&mut self) -> Result<()> {
        // ponytail: once inline would need a post-startup cursor probe (resize),
        // stop using native scrollback and keep the Ratatui UI alive in-frame.
        // Upgrade path: serialize terminal probes with input reads.
        self.terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        self.native_scrollback = false;
        self.allow_resize_probe = false;
        self.last_size = self.terminal.size()?;
        Ok(())
    }

    pub(crate) fn draw(&mut self) -> Result<()> {
        // Ratatui's inline autoresize recomputes the viewport from a terminal
        // cursor-position probe. That is safe before the input reader starts,
        // but after startup `event::read()` can steal the CPR response. Avoid
        // `Terminal::draw()` here because it autoresizes internally every pass.
        let size = self.terminal.size()?;
        if self.native_scrollback && !self.allow_resize_probe && size != self.last_size {
            self.fallback_to_fullscreen()?;
        }
        if !self.native_scrollback || self.allow_resize_probe {
            self.terminal.autoresize()?;
            self.last_size = self.terminal.size()?;
        }
        self.allow_resize_probe = false;
        let width = self.terminal.size()?.width;
        if self.native_scrollback {
            let committed = self.screen.take_scrollback(width);
            insert_lines_before(&mut self.terminal, committed)?;
        }
        render_without_autoresize(&mut self.terminal, &mut self.screen)?;
        Ok(())
    }

    fn restore(&mut self) {
        if self.active {
            // Clear the live viewport and drop the cursor onto a fresh line so
            // the shell prompt resumes below, not over the editor box. No
            // alternate screen was entered, so committed scrollback stays put.
            let area = self.terminal.get_frame().area();
            let backend = self.terminal.backend_mut();
            let _ = backend.set_cursor_position(area.as_position());
            let _ = backend.clear_region(ClearType::AfterCursor);
            let _ = backend.set_cursor_position((0, area.y + area.height.saturating_sub(1)));
            let _ = disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                DisableBracketedPaste,
                PopKeyboardEnhancementFlags,
            );
            let _ = backend.show_cursor();
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

fn is_cursor_position_timeout(error: &io::Error) -> bool {
    error
        .to_string()
        .contains("cursor position could not be read")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
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
        assert!(screen.take_scrollback(80).is_empty());

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

        let committed = screen.take_scrollback(80);
        assert_eq!(line_signature(&finalized), line_signature(&committed));
        assert!(screen.take_scrollback(80).is_empty());
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
        // 8 logical lines (under the old 12-logical-line fold cap), each ~400
        // columns => ~6 wrapped rows each => ~48 physical rows if uncapped.
        // The physical-row cap must bound it and report the omitted lines.
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
        // 8 logical lines, each estimated at 6 physical rows; the 24-row cap
        // admits exactly 4 of them (4*6 = 24) and reports the other 4 omitted.
        assert!(
            output_rows <= MAX_TOOL_OUTPUT_ROWS,
            "output not row-capped: {output_rows} physical rows"
        );
        assert!(
            lines.iter().any(|l| line_text(l).contains("… +4 lines")),
            "expected an accurate '… +4 lines' omitted-line indicator: {lines:?}",
        );
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
    fn approval_hint_wraps_in_narrow_frame() -> Result<()> {
        let mut terminal = Terminal::new(TestBackend::new(48, 10))?;
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
        terminal.draw(|f| render(f, &mut screen))?;
        let rendered = buffer_text(&terminal);
        assert!(rendered.contains("approve printf 'global:"));
        assert!(rendered.contains("  │ "));
        assert!(rendered.contains("(timeout 120s)"));
        assert!(rendered.contains("[N] deny"), "{rendered}");
        Ok(())
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
    fn take_scrollback_commits_finalized_blocks_and_keeps_current_block_live() {
        let mut screen = Screen::new();
        // Active turn: the most recent block stays live for grouping/streaming.
        screen.start_turn();
        screen.apply(UiEvent::AssistantText("first answer".to_string()));
        screen.apply(UiEvent::Notice("a note".to_string()));
        // Two blocks exist; committing keeps the last (current) block live.
        let committed: Vec<String> = screen.take_scrollback(80).iter().map(line_text).collect();
        assert!(committed.iter().any(|l| l == "first answer"));
        assert!(
            !committed.iter().any(|l| l.contains("a note")),
            "current block must stay live during a turn: {committed:?}"
        );
        // The live viewport still shows the current block.
        assert!(
            screen
                .wrapped_lines(80)
                .iter()
                .any(|l| line_text(l).contains("a note"))
        );
        // Ending the turn finalizes everything; nothing remains live.
        screen.end_turn();
        let rest: Vec<String> = screen.take_scrollback(80).iter().map(line_text).collect();
        assert!(rest.iter().any(|l| l.contains("a note")));
        assert!(
            screen.wrapped_lines(80).is_empty(),
            "viewport empty at idle"
        );
    }

    /// Replicate `TuiUi::draw` against a generic backend so the full
    /// commit-then-render path is exercisable with `TestBackend`.
    fn draw_step<B: Backend>(terminal: &mut Terminal<B>, screen: &mut Screen) -> Result<()>
    where
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        draw_step_with_scrollback(terminal, screen, true)
    }

    fn draw_step_with_scrollback<B>(
        terminal: &mut Terminal<B>,
        screen: &mut Screen,
        native_scrollback: bool,
    ) -> Result<()>
    where
        B: Backend,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let width = terminal.size()?.width;
        if native_scrollback {
            let committed = screen.take_scrollback(width);
            insert_lines_before(terminal, committed)?;
        }
        render_without_autoresize(terminal, screen)?;
        Ok(())
    }

    struct CursorProbeFailBackend {
        inner: TestBackend,
        fail_cursor_probe: bool,
        size_override: Option<ratatui::layout::Size>,
    }

    impl CursorProbeFailBackend {
        fn new(width: u16, height: u16) -> Self {
            Self {
                inner: TestBackend::new(width, height),
                fail_cursor_probe: false,
                size_override: None,
            }
        }
    }

    impl Backend for CursorProbeFailBackend {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, content: I) -> std::result::Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            self.inner.draw(content).map_err(|err| match err {})
        }

        fn append_lines(&mut self, n: u16) -> std::result::Result<(), Self::Error> {
            self.inner.append_lines(n).map_err(|err| match err {})
        }

        fn hide_cursor(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.hide_cursor().map_err(|err| match err {})
        }

        fn show_cursor(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.show_cursor().map_err(|err| match err {})
        }

        fn get_cursor_position(
            &mut self,
        ) -> std::result::Result<ratatui::layout::Position, Self::Error> {
            if self.fail_cursor_probe {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "cursor position could not be read",
                ));
            }
            self.inner.get_cursor_position().map_err(|err| match err {})
        }

        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            position: P,
        ) -> std::result::Result<(), Self::Error> {
            self.inner
                .set_cursor_position(position)
                .map_err(|err| match err {})
        }

        fn clear(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.clear().map_err(|err| match err {})
        }

        fn clear_region(&mut self, clear_type: ClearType) -> std::result::Result<(), Self::Error> {
            self.inner
                .clear_region(clear_type)
                .map_err(|err| match err {})
        }

        fn size(&self) -> std::result::Result<ratatui::layout::Size, Self::Error> {
            Ok(self
                .size_override
                .unwrap_or_else(|| self.inner.size().expect("test backend size")))
        }

        fn window_size(
            &mut self,
        ) -> std::result::Result<ratatui::backend::WindowSize, Self::Error> {
            Ok(ratatui::backend::WindowSize {
                columns_rows: self.size()?,
                pixels: ratatui::layout::Size::ZERO,
            })
        }

        fn flush(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.flush().map_err(|err| match err {})
        }

        fn scroll_region_up(
            &mut self,
            region: std::ops::Range<u16>,
            line_count: u16,
        ) -> std::result::Result<(), Self::Error> {
            self.inner
                .scroll_region_up(region, line_count)
                .map_err(|err| match err {})
        }

        fn scroll_region_down(
            &mut self,
            region: std::ops::Range<u16>,
            line_count: u16,
        ) -> std::result::Result<(), Self::Error> {
            self.inner
                .scroll_region_down(region, line_count)
                .map_err(|err| match err {})
        }
    }

    #[test]
    fn draw_after_startup_does_not_probe_cursor_even_if_size_changes() -> Result<()> {
        let mut backend = CursorProbeFailBackend::new(40, 14);
        backend.set_cursor_position(ratatui::layout::Position::new(0, 13))?;
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(LIVE_VIEWPORT_ROWS),
            },
        )?;
        terminal.backend_mut().fail_cursor_probe = true;
        terminal.backend_mut().size_override = Some(ratatui::layout::Size::new(41, 14));

        let mut screen = Screen::new();
        screen.apply(UiEvent::SessionStarted);
        screen.commit_user("after startup");
        screen.start_turn();
        screen.apply(UiEvent::AssistantText("still rendering".to_string()));

        draw_step(&mut terminal, &mut screen)?;
        Ok(())
    }

    fn visible_and_scrollback_text(terminal: &Terminal<TestBackend>) -> String {
        let backend = terminal.backend();
        let mut out = String::new();
        for cell in backend.scrollback().content.iter() {
            out.push_str(cell.symbol());
        }
        out.push('\n');
        out.push_str(&buffer_text(terminal));
        out
    }

    #[test]
    fn full_draw_path_commits_history_and_keeps_live_viewport() -> Result<()> {
        let mut backend = TestBackend::new(40, 14);
        backend.set_cursor_position(ratatui::layout::Position::new(0, 13))?;
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(LIVE_VIEWPORT_ROWS),
            },
        )?;
        let mut screen = Screen::new();

        // A turn: user prompt, a streamed/markdown assistant answer, a tool
        // result. Draw after each event like the loop does.
        screen.commit_user("hello there");
        screen.start_turn();
        draw_step(&mut terminal, &mut screen)?;
        screen.apply(UiEvent::AssistantText("# Done\n\nall good".to_string()));
        draw_step(&mut terminal, &mut screen)?;
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "echo hi" })),
            content: "hi".to_string(),
            exit_code: None,
            duration: None,
        });
        draw_step(&mut terminal, &mut screen)?;
        screen.end_turn();
        draw_step(&mut terminal, &mut screen)?;

        // Finalized history reached native scrollback (committed above the
        // viewport), and the live viewport now shows only the editor box.
        let everything = visible_and_scrollback_text(&terminal);
        assert!(
            everything.contains("hello there"),
            "user line missing: {everything:?}"
        );
        assert!(everything.contains("Done"), "assistant heading missing");
        assert!(everything.contains("Ran echo hi"), "tool block missing");
        let viewport = buffer_text(&terminal);
        assert!(
            viewport.contains("message"),
            "editor box not in live viewport: {viewport:?}"
        );
        // Finalized history must have LEFT the live model (committed to
        // scrollback, not merely duplicated): at idle the transcript holds no
        // live rows and the active region renders nothing. (`viewport` above is
        // the whole backend buffer, which legitimately still shows committed
        // history above the inline viewport, so it is not the right surface for
        // a negative check.)
        assert!(
            screen.transcript.rows.is_empty(),
            "finalized rows were not drained out of the live viewport"
        );
        assert!(
            screen.wrapped_lines(40).is_empty(),
            "live active region still renders finalized content"
        );

        Ok(())
    }

    #[test]
    fn fullscreen_tui_fallback_keeps_history_in_frame() -> Result<()> {
        let mut terminal = Terminal::new(TestBackend::new(40, 14))?;
        let mut screen = Screen::new();

        screen.apply(UiEvent::SessionStarted);
        draw_step_with_scrollback(&mut terminal, &mut screen, false)?;
        assert!(buffer_text(&terminal).contains("iris"));
        assert!(
            !screen.transcript.rows.is_empty(),
            "full-screen fallback must not drain history into native scrollback"
        );

        screen.commit_user("hello there");
        screen.start_turn();
        screen.apply(UiEvent::AssistantText("ok".to_string()));
        screen.end_turn();
        draw_step_with_scrollback(&mut terminal, &mut screen, false)?;

        let frame = buffer_text(&terminal);
        assert!(
            frame.contains("hello there"),
            "user line missing: {frame:?}"
        );
        assert!(frame.contains("ok"), "assistant text missing: {frame:?}");
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|row| row.text.contains("hello")),
            "history should remain live in full-screen fallback"
        );
        Ok(())
    }

    #[test]
    fn finalized_lines_reach_native_scrollback_via_insert_before() -> Result<()> {
        // Inline viewport at the bottom of a short screen; committing more lines
        // than fit above it pushes the overflow into native scrollback.
        let mut backend = TestBackend::new(16, 6);
        backend.set_cursor_position(ratatui::layout::Position::new(0, 5))?;
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(2),
            },
        )?;
        let lines: Vec<Line<'static>> = (0..6).map(|i| Line::from(format!("history{i}"))).collect();
        insert_lines_before(&mut terminal, lines)?;
        let scrollback = terminal.backend().scrollback();
        let text: String = scrollback
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(
            text.contains("history0"),
            "first line not in scrollback: {text:?}"
        );
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

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let area = buf.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn modal_render_survives_a_tiny_terminal() -> Result<()> {
        use crate::mimir::model_catalog::CatalogModel;
        use crate::mimir::selection::ProviderId;
        use crate::ui::modal::{Modal, ModelPicker};

        // A 3-row (and 2-row) terminal must not panic on the modal height clamp.
        for height in [2u16, 3, 4] {
            let mut terminal = Terminal::new(TestBackend::new(40, height))?;
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
            terminal.draw(|f| render(f, &mut screen))?;
        }
        Ok(())
    }

    #[test]
    fn open_modal_renders_picker_frame_in_place_of_editor() -> Result<()> {
        use crate::mimir::model_catalog::CatalogModel;
        use crate::mimir::selection::ProviderId;
        use crate::ui::modal::{Modal, ModelPicker};

        let mut terminal = Terminal::new(TestBackend::new(60, 14))?;
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
        terminal.draw(|f| render(f, &mut screen))?;

        let rendered = buffer_text(&terminal);
        // Transcript still on top; the modal frame replaces the editor below.
        assert!(rendered.contains("prior reply"), "{rendered}");
        assert!(rendered.contains("Select model"), "{rendered}");
        assert!(rendered.contains("GPT 5.5"), "{rendered}");
        assert!(rendered.contains("Sonnet 4.6"), "{rendered}");
        // The editor placeholder is hidden while the modal is open.
        assert!(!rendered.contains("Type a message"), "{rendered}");
        Ok(())
    }

    #[test]
    fn open_modal_has_room_for_model_picker_footer() -> Result<()> {
        use crate::mimir::model_catalog;
        use crate::ui::modal::{Modal, ModelPicker};

        let mut terminal = Terminal::new(TestBackend::new(80, LIVE_VIEWPORT_ROWS))?;
        let mut screen = Screen::new();
        screen.open_modal(Modal::Model(ModelPicker::new(
            model_catalog::all(),
            "anthropic/claude-opus-4-8",
            "anthropic/claude-opus-4-8",
            crate::mimir::selection::ReasoningEffort::XHigh,
        )));
        terminal.draw(|f| render(f, &mut screen))?;

        let rendered = buffer_text(&terminal);
        assert!(rendered.contains("Haiku 4.5"), "{rendered}");
        assert!(rendered.contains("xHigh effort"), "{rendered}");
        assert!(rendered.contains("Enter to set as default"), "{rendered}");
        Ok(())
    }

    #[test]
    fn frame_pins_editor_box_below_transcript() -> Result<()> {
        let mut terminal = Terminal::new(TestBackend::new(40, 8))?;
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText("hello world".to_string()));
        screen.editor.insert_str("hi");
        terminal.draw(|f| render(f, &mut screen))?;

        let rendered = buffer_text(&terminal);
        assert!(rendered.contains("hello world"));
        // The editor text sits inside the bordered box near the bottom.
        assert!(rendered.contains("hi"));
        // Idle status hint is shown when no turn runs (the long hint is
        // truncated at this narrow test width, so assert its leading words).
        assert!(rendered.contains("enter send"));
        Ok(())
    }

    #[test]
    fn long_editor_line_wraps_instead_of_scrolling_right() -> Result<()> {
        let mut terminal = Terminal::new(TestBackend::new(18, 8))?;
        let mut screen = Screen::new();
        screen.editor.insert_str("abcdefghijklmnopqrst");
        terminal.draw(|f| render(f, &mut screen))?;

        let rendered = buffer_text(&terminal);
        // The editor inner width is 16 cells (18-wide frame minus borders), so
        // a 20-cell word should use two visible rows instead of horizontally
        // scrolling to the tail of the line.
        assert!(rendered.contains("abcdefghijklmnop"));
        assert!(rendered.contains("qrst"));
        Ok(())
    }

    #[test]
    fn frame_shows_spinner_while_turn_active() -> Result<()> {
        let mut terminal = Terminal::new(TestBackend::new(40, 8))?;
        let mut screen = Screen::new();
        screen.start_turn();
        terminal.draw(|f| render(f, &mut screen))?;
        let before = buffer_text(&terminal);
        assert!(before.contains("working"));

        // A tick advances the spinner glyph (animation), idle does not.
        let glyph0 = SPINNER_FRAMES[0];
        assert!(before.contains(glyph0));
        assert!(screen.tick());
        terminal.draw(|f| render(f, &mut screen))?;
        let after = buffer_text(&terminal);
        assert!(after.contains(SPINNER_FRAMES[1]));

        screen.end_turn();
        assert!(!screen.tick());
        terminal.draw(|f| render(f, &mut screen))?;
        let idle = buffer_text(&terminal);
        assert!(
            idle.contains("enter send"),
            "idle hint replaces the spinner"
        );
        assert!(!idle.contains("working"), "spinner cleared on turn end");
        Ok(())
    }

    #[test]
    fn frame_shows_slash_palette_when_typing_command() -> Result<()> {
        let mut terminal = Terminal::new(TestBackend::new(40, 10))?;
        let mut screen = Screen::new();
        screen.editor.insert_str("/e");
        screen.sync_palette();
        terminal.draw(|f| render(f, &mut screen))?;
        let rendered = buffer_text(&terminal);
        assert!(rendered.contains("/exit"));
        assert!(!rendered.contains("/quit"), "filtered to /exit only");
        Ok(())
    }

    #[test]
    fn tool_started_opens_running_cell_live_not_in_scrollback() {
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
        // While the turn runs, the Running block stays live (not committed).
        let committed: Vec<String> = screen.take_scrollback(80).iter().map(line_text).collect();
        assert!(
            !committed.iter().any(|l| l.contains("Running")),
            "running cell committed to scrollback too early: {committed:?}"
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
        assert_eq!(header.spans[0].style, ok_style());
        assert!(
            line_text(header).contains("(1.2s)"),
            "duration suffix missing: {}",
            line_text(header)
        );
    }

    #[test]
    fn exec_cell_nonzero_exit_shows_red_cross_bullet() {
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
        assert_eq!(header.spans[0].content.as_ref(), "\u{2717}");
        assert_eq!(header.spans[0].style, err_style());
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
    fn streamed_exec_cell_lands_in_scrollback_after_finalize() -> Result<()> {
        let mut backend = TestBackend::new(40, 14);
        backend.set_cursor_position(ratatui::layout::Position::new(0, 13))?;
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(LIVE_VIEWPORT_ROWS),
            },
        )?;
        let mut screen = Screen::new();
        screen.commit_user("run it");
        screen.start_turn();
        draw_step(&mut terminal, &mut screen)?;
        let call = call_args("bash", json!({ "command": "echo hi" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        draw_step(&mut terminal, &mut screen)?;
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "hi\n".to_string(),
        });
        draw_step(&mut terminal, &mut screen)?;
        screen.apply(UiEvent::ToolResult {
            call: call.clone(),
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: Some(std::time::Duration::from_millis(10)),
        });
        draw_step(&mut terminal, &mut screen)?;
        screen.end_turn();
        draw_step(&mut terminal, &mut screen)?;
        // The finalized exec cell reached native scrollback and left the live model.
        let everything = visible_and_scrollback_text(&terminal);
        assert!(
            everything.contains("Ran echo hi"),
            "finalized exec cell missing from scrollback: {everything:?}"
        );
        assert!(
            screen.transcript.rows.is_empty(),
            "exec rows not drained out of the live viewport"
        );
        Ok(())
    }
}
