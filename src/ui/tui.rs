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

use anyhow::Result;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui_textarea::{TextArea, WrapMode};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::nexus::ToolCall;
use crate::tool_display::{fold, summarize};
use crate::ui::slash::{self, Palette, SlashCommand};
use crate::ui::{TurnErrorKind, UiEvent};

const BANNER_LINES: &[&str] = &["iris", "terminal-first coding agent", "Type /exit to quit."];

/// Idle status-row hint: discoverability without a help screen.
const IDLE_HINT: &str = "enter send \u{b7} alt+enter newline \u{b7} / commands \u{b7} ctrl-c quit";

/// Editor box grows with content up to this many text rows, then scrolls
/// internally (keeps the transcript from being squeezed by a huge paste).
const MAX_EDITOR_ROWS: u16 = 10;

/// Slash popup height cap (including its border).
const MAX_PALETTE_ROWS: u16 = 8;

/// Braille spinner frames; cycled by the render tick while a turn computes.
/// Not emojis: single-cell Unicode glyphs that render on any UTF-8 terminal.
const SPINNER_FRAMES: &[&str] = &[
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];

fn assistant_style() -> Style {
    Style::default()
}
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

/// One styled logical transcript row. Kept as plain text + style so it can be
/// rendered from the current width into physical terminal rows.
#[derive(Clone)]
struct TranscriptRow {
    text: String,
    style: Style,
}

impl TranscriptRow {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }

    fn render(&self, width: usize, out: &mut Vec<Line<'static>>) {
        push_wrapped_row(&self.text, self.style, width, out);
    }
}

fn push_wrapped_row(text: &str, style: Style, width: usize, out: &mut Vec<Line<'static>>) {
    for physical in wrap_to_width(text, width) {
        out.push(Line::from(Span::styled(physical, style)));
    }
}

/// Transcript state and width-aware rendering, separate from editor/spinner UI.
#[derive(Default)]
struct Transcript {
    rows: Vec<TranscriptRow>,
    /// Live assistant text being streamed; rendered after committed rows and
    /// committed exactly once on `AssistantTextEnd`.
    streaming: Option<String>,
}

impl Transcript {
    /// Append a blank separator row before a new top-level block, unless the
    /// transcript is empty or already ends in a blank row.
    fn push_blank(&mut self) {
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

    /// Commit any in-flight streamed assistant text into the transcript.
    fn finish_stream(&mut self) {
        if let Some(text) = self.streaming.take()
            && !text.is_empty()
        {
            self.push(&text, assistant_style());
        }
    }

    /// Apply one semantic event to the transcript rows.
    fn apply(&mut self, event: UiEvent) {
        match event {
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
                    self.push(&text, assistant_style());
                }
            }
            UiEvent::AssistantText(text) => {
                self.finish_stream();
                if !text.is_empty() {
                    self.push_blank();
                    self.push(&text, assistant_style());
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
            UiEvent::ToolAutoApproved(call) => {
                self.begin_block();
                self.push(
                    &format!("auto-approved - {} - session", summarize(&call)),
                    dim_style(),
                );
            }
            UiEvent::DiffPreview { call, diff } => {
                self.begin_block();
                self.push(&format!("diff - {}", summarize(&call)), dim_style());
                self.rows.extend(diff_rows(&diff));
            }
            UiEvent::ToolDenied(call) => {
                self.begin_block();
                self.push(
                    &format!("[error] denied - {}", summarize(&call)),
                    err_style(),
                );
            }
            UiEvent::ToolResult { call, content } => {
                self.begin_block();
                let summary = summarize(&call);
                let head = if content.is_empty() {
                    summary
                } else {
                    let lines = content.lines().count();
                    let plural = if lines == 1 { "" } else { "s" };
                    format!("{summary} - {lines} line{plural}")
                };
                self.push(&format!("[ok] {head}"), ok_style());
                let folded = fold(&content);
                for line in folded.preview.lines() {
                    self.push(&format!("  {line}"), dim_style());
                }
                if folded.hidden_lines > 0 {
                    self.push(
                        &format!("  ... (+{} more lines)", folded.hidden_lines),
                        dim_style(),
                    );
                }
            }
            UiEvent::ToolError { call, message } => {
                self.begin_block();
                self.push(&format!("[error] {}", summarize(&call)), err_style());
                self.push(&format!("  error: {message}"), err_style());
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

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let width = usize::from(width);
        let mut rows = Vec::new();
        for row in &self.rows {
            row.render(width, &mut rows);
        }
        if let Some(text) = &self.streaming {
            for line in text.split('\n') {
                push_wrapped_row(line, assistant_style(), width, &mut rows);
            }
        }
        rows
    }
}

/// Animated turn-progress spinner. Advances only while `active`, so an idle
/// session redraws nothing on a tick (no flicker, no busy CPU).
#[derive(Default)]
struct Spinner {
    active: bool,
    frame: usize,
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
    approval_hint: Option<String>,
    /// Physical rows above the bottom of the transcript to keep in view. Zero
    /// means follow the latest output.
    scrollback: u16,
}

impl Screen {
    pub(crate) fn new() -> Self {
        Self {
            transcript: Transcript::default(),
            editor: fresh_editor(),
            palette: Palette::default(),
            spinner: Spinner::default(),
            approval_hint: None,
            scrollback: 0,
        }
    }

    // --- transcript ---

    /// Apply one semantic event, preserving follow-to-bottom when the user has
    /// not scrolled away. The loop calls this for every Nexus event.
    pub(crate) fn apply_event(&mut self, event: UiEvent) {
        let was_following = self.scrollback == 0;
        self.apply(event);
        if was_following {
            self.follow_bottom();
        }
    }

    /// Apply one semantic event to the transcript.
    pub(crate) fn apply(&mut self, event: UiEvent) {
        self.transcript.apply(event);
    }

    /// Commit a submitted prompt into the transcript as a user line.
    pub(crate) fn commit_user(&mut self, text: &str) {
        self.transcript.commit_user(text);
    }

    // --- scrollback ---

    pub(crate) fn follow_bottom(&mut self) {
        self.scrollback = 0;
    }

    pub(crate) fn scroll_up(&mut self, rows: u16) {
        self.scrollback = self.scrollback.saturating_add(rows);
    }

    pub(crate) fn scroll_down(&mut self, rows: u16) {
        self.scrollback = self.scrollback.saturating_sub(rows);
    }

    fn wrapped_lines(&self, width: u16) -> Vec<Line<'static>> {
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

    /// Clear the editor without submitting (Ctrl-U / Ctrl-C on non-empty input).
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

    /// Show a gated tool's approval prompt: a transcript line for history plus a
    /// short status-row hint. The loop captures the decision from a keypress.
    pub(crate) fn show_approval(&mut self, call: &ToolCall, allow_always: bool) {
        self.transcript.begin_block();
        let options = if allow_always {
            "[y] once  [a] always  [N] deny"
        } else {
            "[y] once  [N] deny"
        };
        self.transcript.push(
            &format!("approve {}?  {options}", summarize(call)),
            prompt_style(),
        );
        self.approval_hint = Some(format!("awaiting approval  {options}"));
        self.follow_bottom();
    }

    pub(crate) fn clear_approval(&mut self) {
        self.approval_hint = None;
    }

    /// Status row content: approval hint > spinner > idle hint.
    fn status_line(&self) -> Line<'static> {
        if let Some(hint) = &self.approval_hint {
            Line::from(Span::styled(hint.clone(), prompt_style()))
        } else if self.spinner.active {
            Line::from(vec![
                Span::styled(format!("{} ", self.spinner.glyph()), prompt_style()),
                Span::styled("working", dim_style()),
            ])
        } else {
            Line::from(Span::styled(IDLE_HINT, dim_style()))
        }
    }
}

/// Colorize a unified diff into styled transcript rows. The two file headers
/// before the first hunk are dropped, hunk headers cyan, additions green,
/// removals red, context dimmed.
fn diff_rows(diff: &str) -> Vec<TranscriptRow> {
    let mut seen_hunk = false;
    let mut out = Vec::new();
    for line in diff.lines() {
        if !seen_hunk && (line.starts_with("--- ") || line.starts_with("+++ ")) {
            continue;
        }
        let style = if line.starts_with("@@") {
            seen_hunk = true;
            Style::default().fg(Color::Cyan)
        } else {
            match line.chars().next() {
                Some('+') => Style::default().fg(Color::Green),
                Some('-') => Style::default().fg(Color::Red),
                _ => dim_style(),
            }
        };
        out.push(TranscriptRow::new(line, style));
    }
    out
}

/// Greedy word-wrap `text` to `width` display columns, breaking at spaces and
/// hard-breaking any single word longer than the line. Returns at least one row
/// (possibly empty) so a blank logical line still occupies a row.
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
                if cur_w + cw > width {
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

/// Render the whole UI: transcript on top, a status/spinner row, an optional
/// slash popup, then the bordered editor at the bottom. Takes `&mut Screen` so
/// scrollback can be clamped against the real transcript height computed here.
fn render(frame: &mut Frame, screen: &mut Screen) {
    let area = frame.area();
    if area.height < 4 || area.width < 1 {
        return;
    }

    let editor_rows = editor_visual_rows(&screen.editor, area.width);
    let editor_h = editor_rows + 2; // top + bottom border
    let input_text = screen.editor_text();
    let palette_active = screen.palette.is_active(&input_text);
    let palette_matches: Vec<&SlashCommand> = if palette_active {
        slash::matches(&input_text)
    } else {
        Vec::new()
    };
    let palette_h = if palette_active {
        (palette_matches.len() as u16 + 2).min(MAX_PALETTE_ROWS)
    } else {
        0
    };

    let chunks = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(palette_h),
        Constraint::Length(editor_h),
    ])
    .split(area);
    let transcript_area = chunks[0];
    let status_area = chunks[1];
    let palette_area = chunks[2];
    let editor_area = chunks[3];

    // Transcript: lines pre-wrapped to exactly one physical row each, so the
    // scroll-to-bottom offset is exact and never diverges from the renderer.
    // Wrap once per frame, then clamp scrollback against that same count.
    let lines = screen.wrapped_lines(transcript_area.width);
    let total_rows = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let max_scroll = total_rows.saturating_sub(transcript_area.height);
    screen.scrollback = screen.scrollback.min(max_scroll);
    let scroll = max_scroll.saturating_sub(screen.scrollback);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).scroll((scroll, 0)),
        transcript_area,
    );

    frame.render_widget(Paragraph::new(screen.status_line()), status_area);

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

/// Terminal driver: owns the ratatui terminal and the persistent
/// alternate-screen + raw-mode lifecycle for the whole interactive session.
/// Reads no input itself; [`crate::ui::tui_loop`] feeds it events and calls
/// [`TuiUi::draw`].
pub(crate) struct TuiUi {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    pub(crate) screen: Screen,
    active: bool,
}

impl TuiUi {
    /// Enter raw mode + the alternate screen ONCE and enable bracketed paste and
    /// scroll-wheel reporting for the session. Restored on `drop`/`shutdown`,
    /// and by the signal handler's emergency escape on a force-quit.
    pub(crate) fn new() -> Result<Self> {
        // Capture cooked-mode termios before raw mode so the force-quit signal
        // handler can restore the tty even though Drop will not run then.
        crate::signals::save_termios_for_force_quit();
        enable_raw_mode()?;
        let terminal = match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                return Err(error.into());
            }
        };
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        ) {
            // The combined `execute!` may have partially applied before failing,
            // so best-effort undo every mode rather than only raw mode.
            let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        crate::signals::enable_terminal_restore_on_force_quit();
        Ok(Self {
            terminal,
            screen: Screen::new(),
            active: true,
        })
    }

    pub(crate) fn draw(&mut self) -> Result<()> {
        let screen = &mut self.screen;
        self.terminal.draw(|frame| render(frame, screen))?;
        Ok(())
    }

    fn restore(&mut self) {
        if self.active {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
            let _ = self.terminal.show_cursor();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            crate::signals::disable_terminal_restore_on_force_quit();
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
    use ratatui::backend::TestBackend;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments: json!({ "path": "note.txt", "content": "hi" }),
        }
    }

    fn row_text(row: &TranscriptRow) -> String {
        row.text.clone()
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
    fn long_transcript_line_wraps_to_multiple_rows() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText("alpha beta gamma delta".to_string()));
        assert_eq!(screen.transcript.rows.len(), 1);
        assert!(screen.wrapped_lines(12).len() >= 2);
    }

    #[test]
    fn tool_result_renders_check_summary_and_folded_preview() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call("read"),
            content: "a\nb\n".to_string(),
        });
        let texts: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert!(texts.iter().any(|t| t == "[ok] read note.txt - 2 lines"));
        assert!(texts.iter().any(|t| t == "  a"));
        assert!(texts.iter().any(|t| t == "  b"));
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
    fn scroll_up_clamps_to_real_top_on_render() -> Result<()> {
        let mut terminal = Terminal::new(TestBackend::new(20, 8))?;
        let mut screen = Screen::new();
        for i in 0..20 {
            screen
                .transcript
                .push(&format!("line {i}"), assistant_style());
        }
        // Scroll past the top; render clamps to the real maximum. Layout: 8 rows
        // minus status(1) and the editor box(1 + 2 border) leaves a 4-row
        // transcript, so max scrollback is 20 - 4 = 16.
        screen.scroll_up(u16::MAX);
        terminal.draw(|f| render(f, &mut screen))?;
        assert_eq!(screen.scrollback, 16);
        screen.scroll_down(10);
        assert_eq!(screen.scrollback, 6);
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
}
