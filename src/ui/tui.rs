//! Full-screen terminal front-end (Tier 3) built on ratatui.
//!
//! Layering (see the renderer design notes): ratatui owns the cell buffer, the
//! frame diff, and width-aware layout; it re-exports crossterm (raw mode,
//! alternate screen, key/resize/paste events) which provides the raw terminal
//! plumbing. This module is the thin layer on top: [`Screen`] holds the UI state
//! and renders it into a frame, and [`TuiUi`] drives the terminal lifecycle and
//! the input loop, adapting both onto the [`Ui`] seam.
//!
//! Concurrency / cancellation: the session loop is blocking and single-threaded.
//! Raw mode is enabled only while reading input ([`next_prompt`] /
//! [`request_approval`]) via [`RawGuard`]; during the turn's compute the terminal
//! is in cooked mode, so a Ctrl-C still raises SIGINT and the existing per-turn
//! watcher cancels the token. While raw mode is active, Ctrl-C is delivered as a
//! key event instead, so the read loop calls [`crate::signals::interrupt_from_terminal`].

use std::io::{self, IsTerminal, Stdout};

#[cfg(unix)]
use std::mem::MaybeUninit;

use anyhow::Result;
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::approval::parse_decision;
use crate::nexus::{ApprovalDecision, ToolCall};
use crate::tool_display::{fold, summarize};
use crate::ui::text::TextUi;
use crate::ui::{TurnErrorKind, Ui, UiEvent};

/// Editor prompt label. Empty for now: the bottom input row is enough chrome.
const PROMPT: &str = "";

const BANNER_LINES: &[&str] = &["iris", "terminal-first coding agent", "Type /exit to quit."];

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

/// Display width of a string as the terminal renders it, reused for word-wrap
/// and editor-cursor math. Control chars count as zero (they are not emitted).
fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Display width of a single char, clamped to at least 1 so a zero-width or
/// control char still advances the wrap and never loops forever.
fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0).max(1)
}

/// One styled logical line of transcript. Kept as plain text + style so it can
/// be wrapped to the current width at render time (one logical line may render
/// as several physical rows).
#[derive(Clone)]
struct Row {
    text: String,
    style: Style,
}

/// UI state plus its rendering. Holds no terminal handle, so its behavior is
/// unit-testable without a TTY and its frame output is testable via ratatui's
/// `TestBackend`.
pub(crate) struct Screen {
    transcript: Vec<Row>,
    input: String,
    /// Byte offset of the caret into `input`; always on a char boundary.
    cursor: usize,
    /// Live assistant text being streamed; shown below the transcript and
    /// committed into it once the stream ends.
    streaming: Option<String>,
}

impl Screen {
    pub(crate) fn new() -> Self {
        Self {
            transcript: Vec::new(),
            input: String::new(),
            cursor: 0,
            streaming: None,
        }
    }

    /// Append a blank separator row before a new top-level block, unless the
    /// transcript is empty or already ends in a blank row. Keeps consecutive
    /// event blocks visually distinct without stacking multiple blank lines.
    fn push_blank(&mut self) {
        match self.transcript.last() {
            None => {}
            Some(last) if last.text.is_empty() => {}
            _ => self.transcript.push(Row {
                text: String::new(),
                style: Style::default(),
            }),
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
            self.transcript.push(Row {
                text: line.to_string(),
                style,
            });
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

    /// Apply one semantic event to the transcript.
    pub(crate) fn apply(&mut self, event: UiEvent) {
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
                for (text, style) in diff_rows(&diff) {
                    self.transcript.push(Row { text, style });
                }
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

    /// Transcript plus any live streamed assistant text, wrapped to `width` so
    /// each returned `Line` is exactly one physical terminal row. Rendering with
    /// one row per line keeps the scroll-to-bottom math exact (no divergence
    /// from a word-wrapping widget).
    fn wrapped_lines(&self, width: u16) -> Vec<Line<'static>> {
        let width = usize::from(width);
        let mut rows = Vec::new();
        let wrap_into = |text: &str, style: Style, rows: &mut Vec<Line<'static>>| {
            for physical in wrap_to_width(text, width) {
                rows.push(Line::from(Span::styled(physical, style)));
            }
        };
        for row in &self.transcript {
            wrap_into(&row.text, row.style, &mut rows);
        }
        if let Some(text) = &self.streaming {
            for line in text.split('\n') {
                wrap_into(line, assistant_style(), &mut rows);
            }
        }
        rows
    }

    // --- editor operations (char-boundary safe) ---

    fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn insert_str(&mut self, s: &str) {
        self.input.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    fn backspace(&mut self) {
        if let Some(prev) = self.input[..self.cursor].chars().next_back() {
            let len = prev.len_utf8();
            self.cursor -= len;
            self.input.remove(self.cursor);
        }
    }

    fn move_left(&mut self) {
        if let Some(prev) = self.input[..self.cursor].chars().next_back() {
            self.cursor -= prev.len_utf8();
        }
    }

    fn move_right(&mut self) {
        if let Some(next) = self.input[self.cursor..].chars().next() {
            self.cursor += next.len_utf8();
        }
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.input.len();
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
    }

    /// Take the current input, clearing the editor.
    fn submit(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.input)
    }

    /// Commit a submitted prompt into the transcript as a user line.
    fn commit_user(&mut self, text: &str) {
        self.push_blank();
        for line in text.split('\n') {
            self.push(&format!("> {line}"), user_style());
        }
    }

    /// Visible editor input plus caret column. The editor is deliberately one
    /// terminal row for the first cut; when it overflows, keep the caret visible
    /// by showing the tail before the cursor rather than doing fragile wrapping.
    fn editor_view(&self, width: u16) -> (String, u16) {
        let width = usize::from(width.max(1));
        let prompt_cols = display_width(PROMPT).min(width.saturating_sub(1));
        let available = width.saturating_sub(prompt_cols).max(1);
        let before_cursor = &self.input[..self.cursor];
        let before_width = display_width(before_cursor);
        if before_width < available {
            return (
                self.input.clone(),
                (prompt_cols + before_width).min(width - 1) as u16,
            );
        }

        let mut start = self.cursor;
        let mut cols = 0;
        for (idx, ch) in before_cursor.char_indices().rev() {
            let w = display_width(&ch.to_string()).max(1);
            if cols + w >= available {
                break;
            }
            start = idx;
            cols += w;
        }
        (
            self.input[start..].to_string(),
            (prompt_cols + cols).min(width - 1) as u16,
        )
    }
}

/// Colorize a unified diff into styled transcript rows. Mirrors the text UI:
/// the two file headers before the first hunk are dropped, hunk headers cyan,
/// additions green, removals red, context dimmed.
fn diff_rows(diff: &str) -> Vec<(String, Style)> {
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
        out.push((line.to_string(), style));
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
        // Keep the word on the current row with its leading space if it fits.
        if i > 0 && !cur.is_empty() && cur_w + 1 + word_w <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + word_w;
            continue;
        }
        // Otherwise wrap: the trailing space stays off the wrapped row.
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

/// Render the whole UI: scrollback area on top, a rule, then the bottom editor.
/// Free function so it can be exercised with any ratatui backend in tests.
fn render(frame: &mut Frame, screen: &Screen, show_cursor: bool) {
    let area = frame.area();
    if area.height < 2 || area.width < 1 {
        return;
    }
    let width = area.width;
    let editor_h = 2; // separator + one-line editor

    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(editor_h)]).split(area);
    let transcript_area = chunks[0];
    let editor_area = chunks[1];

    // Scrollback: lines are pre-wrapped to exactly one physical row each, so the
    // scroll-to-bottom offset is exact and never diverges from the renderer.
    let lines = screen.wrapped_lines(transcript_area.width);
    let scroll = (lines.len() as u16).saturating_sub(transcript_area.height);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).scroll((scroll, 0)),
        transcript_area,
    );

    // Separator rule.
    let rule = Line::from("-".repeat(editor_area.width as usize)).style(dim_style());
    frame.render_widget(
        Paragraph::new(rule),
        Rect::new(editor_area.x, editor_area.y, editor_area.width, 1),
    );

    // Editor input line(s).
    let input_rect = Rect::new(editor_area.x, editor_area.y + 1, editor_area.width, 1);
    let (visible_input, cursor_col) = screen.editor_view(width);
    let editor_line = Line::from(vec![
        Span::styled(
            PROMPT,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(visible_input),
    ]);
    frame.render_widget(Paragraph::new(editor_line), input_rect);

    if show_cursor {
        frame.set_cursor_position((input_rect.x + cursor_col, input_rect.y));
    }
}

/// Restores the terminal's original line discipline after the full-screen UI
/// exits. While the alternate screen is active we keep cooked mode's SIGINT
/// behavior, but disable ECHO so type-ahead during model/tool work cannot smear
/// the ratatui frame.
#[cfg(unix)]
struct EchoRestore {
    original: Option<libc::termios>,
}

#[cfg(unix)]
impl EchoRestore {
    fn capture_and_disable() -> Result<Self> {
        let mut original = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `tcgetattr` initializes `original` on success.
        let rc = unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) };
        if rc == -1 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: success above initialized the termios struct.
        let original = unsafe { original.assume_init() };
        let mut muted = original;
        muted.c_lflag &= !(libc::ECHO | libc::ECHONL);
        // SAFETY: `muted` is a valid termios captured from this fd.
        let rc = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &muted) };
        if rc == -1 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(Self {
            original: Some(original),
        })
    }

    fn restore(&mut self) {
        if let Some(original) = self.original.take() {
            // SAFETY: `original` was captured from this fd at startup.
            let _ = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
        }
    }
}

#[cfg(unix)]
impl Drop for EchoRestore {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(not(unix))]
struct EchoRestore;

#[cfg(not(unix))]
impl EchoRestore {
    fn capture_and_disable() -> Result<Self> {
        Ok(Self)
    }

    fn restore(&mut self) {}
}

/// Enables raw mode + bracketed paste for the lifetime of an input read, and
/// restores cooked mode on drop (including on panic / early return).
struct RawGuard;

impl RawGuard {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
}

/// Terminal driver: owns the ratatui terminal and the alternate-screen
/// lifecycle, and runs the blocking input loop.
pub(crate) struct TuiUi {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    screen: Screen,
    echo: EchoRestore,
    active: bool,
}

impl TuiUi {
    pub(crate) fn new() -> Result<Self> {
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let echo = EchoRestore::capture_and_disable()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(Self {
            terminal,
            screen: Screen::new(),
            echo,
            active: true,
        })
    }

    fn draw(&mut self, show_cursor: bool) -> Result<()> {
        let screen = &self.screen;
        self.terminal
            .draw(|frame| render(frame, screen, show_cursor))?;
        Ok(())
    }

    fn restore(&mut self) {
        if self.active {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), DisableBracketedPaste);
            let _ = self.terminal.show_cursor();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            self.echo.restore();
            self.active = false;
        }
    }
}

impl Drop for TuiUi {
    fn drop(&mut self) {
        self.restore();
    }
}

impl Ui for TuiUi {
    fn next_prompt(&mut self) -> Result<Option<String>> {
        let _guard = RawGuard::new()?;
        loop {
            self.draw(true)?;
            let key = match event::read()? {
                Event::Key(key) => key,
                Event::Paste(text) => {
                    let flattened: String = text
                        .chars()
                        .map(|c| if matches!(c, '\r' | '\n') { ' ' } else { c })
                        .collect();
                    self.screen.insert_str(&flattened);
                    continue;
                }
                _ => continue, // resize repaints on the next loop; others ignored
            };
            if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('c') if ctrl => {
                    if self.screen.input.is_empty() {
                        return Ok(None);
                    }
                    self.screen.clear_input();
                }
                KeyCode::Char('d') if ctrl => {
                    if self.screen.input.is_empty() {
                        return Ok(None);
                    }
                }
                KeyCode::Char('u') if ctrl => self.screen.clear_input(),
                KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                    self.screen.insert_char(c);
                }
                KeyCode::Backspace => self.screen.backspace(),
                KeyCode::Left => self.screen.move_left(),
                KeyCode::Right => self.screen.move_right(),
                KeyCode::Home => self.screen.home(),
                KeyCode::End => self.screen.end(),
                KeyCode::Enter => {
                    let text = self.screen.submit();
                    if text.trim().is_empty() {
                        continue;
                    }
                    self.screen.commit_user(&text);
                    return Ok(Some(text));
                }
                _ => {}
            }
        }
    }

    fn emit(&mut self, event: UiEvent) -> Result<()> {
        self.screen.apply(event);
        self.draw(false)
    }

    fn request_approval(
        &mut self,
        call: &ToolCall,
        allow_always: bool,
    ) -> Result<ApprovalDecision> {
        self.screen.begin_block();
        let options = if allow_always {
            "[y] once  [a] always  [N] deny"
        } else {
            "[y] once  [N] deny"
        };
        self.screen.push(
            &format!("approve {}?  {options}", summarize(call)),
            prompt_style(),
        );
        let _guard = RawGuard::new()?;
        loop {
            self.draw(false)?;
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            if ctrl && matches!(key.code, KeyCode::Char('c')) {
                // Raw mode swallows SIGINT; signal the watcher to cancel the turn.
                crate::signals::interrupt_from_terminal();
                return Ok(ApprovalDecision::Deny);
            }
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(ApprovalDecision::Allow),
                KeyCode::Char('a') | KeyCode::Char('A') if allow_always => {
                    return Ok(ApprovalDecision::AllowAlways);
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Enter | KeyCode::Esc => {
                    return Ok(parse_decision("n"));
                }
                KeyCode::Char('d') if ctrl => return Ok(ApprovalDecision::Deny),
                _ => {}
            }
        }
    }

    fn shutdown(&mut self) -> Result<()> {
        self.restore();
        Ok(())
    }
}

/// Build the interactive front-end: the ratatui full-screen UI when stdin and
/// stdout are both a terminal, otherwise the plain text UI (pipes, CI, tests).
pub(crate) fn stdio() -> Box<dyn Ui> {
    if io::stdout().is_terminal() && io::stdin().is_terminal() {
        match TuiUi::new() {
            Ok(ui) => return Box::new(ui),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "TUI unavailable; using text UI");
            }
        }
    }
    Box::new(TextUi::stdio())
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

    fn row_text(row: &Row) -> String {
        row.text.clone()
    }

    #[test]
    fn streaming_deltas_commit_once_without_duplication() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextDelta("Hel".to_string()));
        screen.apply(UiEvent::AssistantTextDelta("lo".to_string()));
        // Mid-stream the live text shows but is not yet in the transcript.
        assert_eq!(screen.transcript.len(), 0);
        assert_eq!(screen.wrapped_lines(80).len(), 1);
        screen.apply(UiEvent::AssistantTextEnd("Hello".to_string()));
        let texts: Vec<String> = screen.transcript.iter().map(row_text).collect();
        assert_eq!(texts, vec!["Hello".to_string()]);
    }

    #[test]
    fn wrap_breaks_long_line_at_spaces_and_hard_breaks_long_words() {
        // Word wrap at spaces.
        assert_eq!(
            wrap_to_width("alpha beta gamma", 11),
            vec!["alpha beta".to_string(), "gamma".to_string()]
        );
        // A word longer than the width is hard-broken.
        assert_eq!(
            wrap_to_width("abcdefgh", 3),
            vec!["abc".to_string(), "def".to_string(), "gh".to_string()]
        );
        // Fits in one row -> unchanged.
        assert_eq!(wrap_to_width("short", 80), vec!["short".to_string()]);
    }

    #[test]
    fn long_transcript_line_wraps_to_multiple_rows() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText("alpha beta gamma delta".to_string()));
        // One logical line, but several physical rows at a narrow width.
        assert_eq!(screen.transcript.len(), 1);
        assert!(screen.wrapped_lines(12).len() >= 2);
    }

    #[test]
    fn tool_result_renders_check_summary_and_folded_preview() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call("read"),
            content: "a\nb\n".to_string(),
        });
        let texts: Vec<String> = screen.transcript.iter().map(row_text).collect();
        assert!(texts.iter().any(|t| t == "[ok] read note.txt - 2 lines"));
        assert!(texts.iter().any(|t| t == "  a"));
        assert!(texts.iter().any(|t| t == "  b"));
    }

    #[test]
    fn consecutive_blocks_get_one_blank_separator() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText("hi".to_string()));
        screen.apply(UiEvent::Notice("note".to_string()));
        let texts: Vec<String> = screen.transcript.iter().map(row_text).collect();
        // A single blank row separates the two blocks; no leading or double blank.
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
        let texts: Vec<String> = screen.transcript.iter().map(row_text).collect();
        assert!(!texts.iter().any(|t| t.contains("--- a/note.txt")));
        assert!(texts.iter().any(|t| t == "+new"));
        assert!(texts.iter().any(|t| t == "-old"));
    }

    #[test]
    fn editor_insert_and_view_tracks_cursor() {
        let mut screen = Screen::new();
        screen.insert_char('h');
        screen.insert_char('i');
        assert_eq!(screen.input, "hi");
        let (_, col) = screen.editor_view(20);
        assert_eq!(col, 2);
        screen.move_left();
        let (_, col) = screen.editor_view(20);
        assert_eq!(col, 1);
        screen.backspace();
        assert_eq!(screen.input, "i");
    }

    #[test]
    fn submit_clears_input_and_commits_user_line() {
        let mut screen = Screen::new();
        screen.insert_str("hello");
        let text = screen.submit();
        assert_eq!(text, "hello");
        assert_eq!(screen.input, "");
        assert_eq!(screen.cursor, 0);
        screen.commit_user(&text);
        let last = row_text(screen.transcript.last().unwrap());
        assert_eq!(last, "> hello");
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
    fn frame_pins_editor_at_bottom_below_transcript() -> Result<()> {
        let mut terminal = Terminal::new(TestBackend::new(24, 6))?;
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText("hello world".to_string()));
        screen.insert_str("hi");
        terminal.draw(|f| render(f, &screen, true))?;

        let rendered = buffer_text(&terminal);
        let rows: Vec<&str> = rendered.lines().collect();
        // Editor text is on the last row; transcript text is above it.
        assert!(!rows.last().unwrap().contains("iris>"));
        assert!(rows.last().unwrap().contains("hi"));
        assert!(rendered.contains("hello world"));
        Ok(())
    }
}
