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

use std::io::{self, Stdout, Write};
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

use anyhow::Result;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size as terminal_size, supports_keyboard_enhancement,
};
use ratatui::crossterm::{execute, queue};
use ratatui::layout::Size;
use ratatui::style::{Modifier, Style};
#[cfg(test)]
use ratatui::text::{Line, Span};

use crate::nexus::ProviderUsage;
use crate::ui::terminal_surface::TerminalSurface;

mod component;
mod overlay;
mod pane;
mod panel;
mod rows;
mod screen;
mod shell_command;
mod text;
mod tool_render;
mod transcript;
mod wrap;

pub(crate) use component::Component;
pub(crate) use overlay::{FocusTarget, overlay_box};
#[cfg(test)]
use panel::PanelState;
#[cfg(test)]
use rows::{ChromeRow, TranscriptRow, hrule_line};
pub(crate) use screen::Screen;
use screen::{compact_count, render_document_with_chrome_tail};
#[cfg(test)]
use screen::{
    composer_statusline, editor_visual_rows, fresh_editor, render_document, working_indicator_line,
};
#[cfg(test)]
use transcript::Transcript;
#[cfg(test)]
use wrap::display_width;
pub(crate) use wrap::wrap_to_width;

/// Editor box grows with content up to this many text rows, then scrolls
/// internally (keeps the transcript from being squeezed by a huge paste).
const MAX_EDITOR_ROWS: u16 = 10;

/// Above-editor menu height cap, including the blank row above and below.
const MAX_MENU_ROWS: u16 = 16;
/// Minimum composer height: hairline + statusline + blank spacer + one input row.
const MIN_EDITOR_H: u16 = 4;
/// Composer chrome above the input rows: the hairline top edge, statusline, and spacer.
const EDITOR_VERTICAL_CHROME_ROWS: u16 = 3;
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
const PANEL_BODY_LEFT_PADDING: usize = 4;
const PANEL_BODY_RIGHT_PADDING: usize = 2;
const PANEL_BODY_BORDER_WIDTH: usize = 2;
const PANEL_BODY_CHROME_WIDTH: usize =
    PANEL_BODY_BORDER_WIDTH + PANEL_BODY_LEFT_PADDING + PANEL_BODY_RIGHT_PADDING;

// Color roles live in `crate::ui::palette` (the single source of truth). They
// are imported here under their long-standing names so the whole `tui` module
// tree keeps referencing them as `BORDER`, `ORANGE`, … (and its child modules
// as `super::BORDER`).
use crate::ui::palette::{BORDER, DIFF_ADD_BG, DIFF_DEL_BG, GREEN, ORANGE, RED};

const X_PADDING: usize = 2;
const BOX_X_PADDING: usize = X_PADDING;
const TEXT_X_PADDING: usize = X_PADDING;
const TEXT_COLUMN_X_PADDING: usize = BOX_X_PADDING + TEXT_X_PADDING;
const BOX_X_PADDING_U16: u16 = X_PADDING as u16;
const TEXT_COLUMN_X_PADDING_U16: u16 = TEXT_COLUMN_X_PADDING as u16;

/// Secondary guard: truncate any single output line to this many characters
/// before wrapping, so one pathological line cannot dominate the row budget.
const MAX_TOOL_OUTPUT_LINE_CHARS: usize = 2000;

/// Cap on the live exec stream buffer re-rendered under the gutter on each
/// delta. Only the tail (flood-capped to `MAX_TOOL_OUTPUT_ROWS`) is shown and
/// the authoritative full output arrives with the final `ToolResult`, so
/// trimming the head here only bounds the per-delta re-render cost; it never
/// reaches the model.
const MAX_EXEC_STREAM_BYTES: usize = 64 * 1024;

/// LED-chase frames for the active turn indicator. The ping-pong sequence avoids
/// a hard visual wrap from the rightmost LED back to the leftmost LED.
const WORKING_FRAMES: &[&str] = &["●···", "·●··", "··●·", "···●", "··●·", "·●··"];

#[cfg(test)]
const BRAILLE_SPINNER_FRAMES: &[&str] = &[
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
    Style::default().add_modifier(Modifier::DIM)
}
fn prompt_style() -> Style {
    Style::default().fg(ORANGE)
}
fn tool_header_style() -> Style {
    Style::default()
}

/// Format an elapsed turn duration compactly for the working indicator:
/// `<10s` gets tenths, seconds stay terse until one minute, then clock-like only
/// at minute/hour granularity.
fn format_elapsed_compact(duration: Duration) -> String {
    let secs = duration.as_secs();
    if duration < Duration::from_secs(10) {
        format!("{:.1}s", duration.as_secs_f64())
    } else if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}:{:02}", secs / 60, secs % 60)
    } else {
        format!("{}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
    }
}
fn turn_divider_label(elapsed: Option<Duration>, usage: Option<&ProviderUsage>) -> String {
    let Some(elapsed) = elapsed else {
        return String::new();
    };
    let elapsed = format_elapsed_compact(elapsed);
    let sep = crate::ui::symbols::SEP;
    match usage {
        Some(usage) => format!(
            "{elapsed} {sep} ↑{} ↓{}",
            compact_count(usage.input_tokens),
            compact_count(usage.output_tokens)
        ),
        None => elapsed,
    }
}

#[cfg(test)]
fn turn_divider_line(
    elapsed: Option<Duration>,
    usage: Option<&ProviderUsage>,
    width: usize,
) -> Line<'static> {
    hrule_line(&turn_divider_label(elapsed, usage), width)
}

fn border_style() -> Style {
    Style::default().fg(BORDER)
}

fn panel_style() -> Style {
    Style::default()
}

/// Keyboard-enhancement (Kitty keyboard protocol) flags Iris requests when the
/// terminal advertises support. Beyond `DISAMBIGUATE_ESCAPE_CODES` (an
/// unambiguous Esc and reliably distinct modified keys) Iris also asks for event
/// types and alternate keys so the enhanced layout is reported where available.
/// Iris ignores key-release events (every key handler gates on Press/Repeat), so
/// requesting event types is safe. Mirrors pi-mono's requested flag set (7).
fn keyboard_enhancement_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
}

/// Push the keyboard-enhancement flags only when the terminal supports them.
/// Returns whether they were pushed, so shutdown/error paths pop exactly once
/// and never emit a stray pop on terminals that never negotiated the protocol.
///
/// Safe fallback: when the terminal does not support the protocol Iris simply
/// does not push (Crossterm still delivers usable key events). Iris does not
/// emit pi-mono's raw `modifyOtherKeys` (`CSI >4;2m`) fallback because Crossterm
/// does not model or parse it; that is a deliberate non-parity choice.
fn enable_keyboard_enhancement<W: Write>(writer: &mut W, supported: bool) -> io::Result<bool> {
    if !supported {
        return Ok(false);
    }
    queue!(
        writer,
        PushKeyboardEnhancementFlags(keyboard_enhancement_flags())
    )?;
    writer.flush()?;
    Ok(true)
}

/// Restore the keyboard protocol: pop the pushed flags exactly when they were
/// pushed. A no-op otherwise, so it is safe to call on every shutdown/error path.
fn disable_keyboard_enhancement<W: Write>(writer: &mut W, enabled: bool) -> io::Result<()> {
    if enabled {
        queue!(writer, PopKeyboardEnhancementFlags)?;
        writer.flush()?;
    }
    Ok(())
}

/// Terminal driver: owns raw mode, paste/key flags, cursor visibility, terminal
/// size reads, and the Iris terminal surface for the whole interactive session.
/// It does NOT enter the alternate screen and does not use Ratatui `Terminal`:
/// [`crate::ui::tui_loop`] feeds it events and calls [`TuiUi::draw`].
pub(crate) struct TuiUi {
    surface: TerminalSurface<Stdout>,
    pub(crate) screen: Screen,
    active: bool,
    /// Whether keyboard-enhancement flags were successfully pushed, so they are
    /// popped exactly once on shutdown/error and never on terminals that did not
    /// negotiate the protocol.
    keyboard_enhanced: bool,
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
        // Probe Kitty keyboard-protocol support before negotiating so the push is
        // gated and the matching pop is conditional. A probe error is treated as
        // "unsupported" (safe fallback to plain Crossterm key events).
        let supports_enhancement = supports_keyboard_enhancement().unwrap_or(false);
        if let Err(error) = execute!(stdout, EnableBracketedPaste, Hide) {
            let _ = execute!(stdout, DisableBracketedPaste, Show);
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        // Best-effort: a failure to negotiate the protocol must not abort startup.
        let keyboard_enhanced =
            enable_keyboard_enhancement(&mut stdout, supports_enhancement).unwrap_or(false);
        crate::signals::enable_terminal_restore_on_force_quit();
        crate::telemetry::set_tui_active(true);
        Ok(Self {
            surface: TerminalSurface::new(stdout),
            screen: Screen::new(),
            active: true,
            keyboard_enhanced,
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
            // Restore the keyboard protocol first (pop only if pushed), then the
            // paste mode and cursor, then raw mode. Ordering mirrors setup in
            // reverse so no terminal mode Iris toggled is left enabled.
            let _ = disable_keyboard_enhancement(self.surface.writer_mut(), self.keyboard_enhanced);
            self.keyboard_enhanced = false;
            let _ = execute!(self.surface.writer_mut(), DisableBracketedPaste, Show);
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
    use super::panel::{inset_rule_line, panel_body_line, panel_header_line, panel_rule_line};
    use super::*;
    use crate::nexus::{ApprovalDecision, ToolCall};
    use crate::ui::UiEvent;
    use crate::ui::terminal_surface::{RenderKind, TerminalSurface};
    use ratatui::style::Color;
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
        // Skip the zero-width hardware-cursor (IME) marker: it is an internal
        // artifact the terminal surface strips, never visible text.
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .filter(|content| *content != crate::ui::terminal_surface::CURSOR_MARKER)
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
            rendered.iter().any(|line| line.starts_with("    › Title")),
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
        assert!(live_document.contains("› Title"), "{live_document}");
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
                "    › alpha".to_string(),
                "      beta".to_string(),
                String::new()
            ]
        );
        assert!(lines.iter().all(|line| line.style.bg.is_none()));
    }

    #[test]
    fn assistant_paragraph_starts_align_with_wrapped_text() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText(
            "First paragraph has enough words to wrap onto another display row.\n\nSecond paragraph also has enough words to wrap onto another display row."
                .to_string(),
        ));
        let lines = screen.wrapped_lines(48);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>();

        assert!(
            rendered
                .iter()
                .any(|line| line.starts_with("      Second paragraph")),
            "paragraph start lost assistant text-column alignment: {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.starts_with("      to wrap onto another display row.")),
            "wrapped paragraph line lost assistant text-column alignment: {rendered:?}"
        );
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
        assert!(
            rendered.iter().any(|line| line == "      HI"),
            "{rendered:?}"
        );
        let user_idx = rendered
            .iter()
            .position(|line| line.trim_start() == "HI")
            .expect("user prompt");
        let reply_idx = rendered
            .iter()
            .position(|line| line.contains("Hi! What"))
            .expect("assistant reply");
        assert_eq!(rendered[reply_idx - 1], "");
        let user_col = rendered[user_idx]
            .find("HI")
            .map(|idx| display_width(&rendered[user_idx][..idx]));
        let reply_col = rendered[reply_idx]
            .find("Hi!")
            .map(|idx| display_width(&rendered[reply_idx][..idx]));
        assert_eq!(
            user_col, reply_col,
            "user text and assistant text should share a column: {rendered:?}"
        );
        assert!(
            rendered[reply_idx].starts_with("    › Hi! What"),
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
        assert_eq!(plain.style.fg, Some(Color::Reset));
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
            lines
                .iter()
                .any(|l| line_text(l).contains("7 lines hidden")),
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
                .any(|t| t.contains("…") && t.contains("lines hidden")),
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
        let rendered = rendered_text(&mut screen, 80, 12);
        assert!(rendered.contains("REVIEW"), "{rendered}");
        assert!(rendered.contains("$ echo hi"), "{rendered}");
        assert!(rendered.contains("[y] once"), "{rendered}");
        assert!(rendered.contains("[N] deny"), "{rendered}");
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
        let lines = rendered_lines(&mut screen, 48, 12);
        assert!(
            lines
                .iter()
                .all(|line| display_width(&line_text(line)) <= 48),
            "{lines:?}"
        );
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("$ printf 'global:"));
        assert!(rendered.contains("120s)"), "{rendered}");
        assert!(rendered.contains("[N] deny"), "{rendered}");
        assert!(!rendered.contains("\u{21b5} to send"), "{rendered}");
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
        assert!(rendered.contains("$ echo hi"), "{rendered}");
        assert!(rendered.contains("┊ approved this time"), "{rendered}");
        assert!(rendered.contains("┌"), "{rendered}");
        assert!(rendered.contains("└"), "{rendered}");

        let lines = screen.wrapped_lines(80);
        let line = line_matching(&lines, |line| {
            line_text(line).contains("approved this time")
        });
        // The reason is a muted aside; the decision itself lives in the header.
        let marker = span_matching(line, |span| span.content.as_ref().contains("approved"));
        assert_eq!(marker.style, dim_style());
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
        assert!(rendered.contains("$ echo hi"), "{rendered}");
        assert!(rendered.contains("┊ denied"), "{rendered}");
        let lines = screen.wrapped_lines(80);
        let line = line_matching(&lines, |line| line_text(line).contains("┊ denied"));
        let marker = span_matching(line, |span| span.content.as_ref().contains("denied"));
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
            vec![
                "hi".to_string(),
                String::new(),
                "┊ note".to_string(),
                String::new(),
            ]
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
        assert!(texts.iter().any(|t| t.contains("\u{2212}  old")));
        assert!(texts.iter().any(|t| t.contains("+  new")));
        let add = screen
            .transcript
            .rows
            .iter()
            .find(|row| row.text.contains("+  new"))
            .expect("addition row");
        let remove = screen
            .transcript
            .rows
            .iter()
            .find(|row| row.text.contains("\u{2212}  old"))
            .expect("removal row");
        assert_eq!(add.style, ok_style());
        assert_eq!(remove.style, err_style());
        assert_ne!(add.style.fg, Some(DIFF_ADD_BG));
        assert_ne!(remove.style.fg, Some(DIFF_DEL_BG));
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
    fn diff_preview_appends_added_removed_footer_tinted_to_diff_inks() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::DiffPreview {
            call: call("edit"),
            diff: "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
        });
        let footer = screen
            .transcript
            .rows
            .iter()
            .find(|row| row.text.contains("+1") && row.text.contains("\u{2212}1"))
            .expect("added/removed footer row");
        let Some(ChromeRow::Body { line, .. }) = footer.chrome.as_ref() else {
            panic!("footer is a body row");
        };
        assert!(
            line.spans
                .iter()
                .any(|s| s.content.contains("+1") && s.style.fg == ok_style().fg),
            "additions tinted to the add ink: {line:?}"
        );
        assert!(
            line.spans
                .iter()
                .any(|s| s.content.contains("\u{2212}1") && s.style.fg == err_style().fg),
            "removals tinted to the del ink: {line:?}"
        );
    }

    #[test]
    fn diff_preview_new_file_footer_notes_new_file() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::DiffPreview {
            call: call("write"),
            diff: "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+alpha\n+beta\n".to_string(),
        });
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|row| row.text.contains("new file")),
            "new-file preview footer carries a `new file` note"
        );
    }

    #[test]
    fn single_line_modification_highlights_changed_token() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::DiffPreview {
            call: call("edit"),
            diff: "--- a/n.txt\n+++ b/n.txt\n@@ -1 +1 @@\n-foo bar baz\n+foo qux baz\n".to_string(),
        });
        let added = screen
            .transcript
            .rows
            .iter()
            .find(|row| row.text.contains("+  foo qux baz"))
            .expect("addition row");
        let Some(ChromeRow::Body { line, .. }) = added.chrome.as_ref() else {
            panic!("expected body row");
        };
        let reversed = ratatui::style::Modifier::REVERSED;
        let changed: Vec<&str> = line
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(reversed))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(changed, vec!["qux"], "only the changed token is emphasised");
        // The unchanged tokens must not be emphasised.
        assert!(
            line.spans
                .iter()
                .any(|s| s.content.contains("baz") && !s.style.add_modifier.contains(reversed))
        );
    }

    #[test]
    fn multi_line_modification_skips_intra_line_highlight() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::DiffPreview {
            call: call("edit"),
            diff: "--- a/n.txt\n+++ b/n.txt\n@@ -1,2 +1,2 @@\n-aa\n-bb\n+cc\n+dd\n".to_string(),
        });
        let reversed = ratatui::style::Modifier::REVERSED;
        let any_reversed = screen
            .transcript
            .rows
            .iter()
            .any(|row| match row.chrome.as_ref() {
                Some(ChromeRow::Body { line, .. }) => line
                    .spans
                    .iter()
                    .any(|s| s.style.add_modifier.contains(reversed)),
                _ => false,
            });
        assert!(!any_reversed, "multi-line edits should not token-highlight");
    }

    #[test]
    fn indentation_only_change_is_not_token_highlighted() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::DiffPreview {
            call: call("edit"),
            diff: "--- a/n.txt\n+++ b/n.txt\n@@ -1 +1 @@\n-foo\n+  foo\n".to_string(),
        });
        let reversed = ratatui::style::Modifier::REVERSED;
        let any_reversed = screen
            .transcript
            .rows
            .iter()
            .any(|row| match row.chrome.as_ref() {
                Some(ChromeRow::Body { line, .. }) => line
                    .spans
                    .iter()
                    .any(|s| s.style.add_modifier.contains(reversed)),
                _ => false,
            });
        assert!(
            !any_reversed,
            "pure indentation changes must stay quiet (no reversed tokens)"
        );
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
        assert!(texts.iter().any(|t| t.contains("+  new1")));
        assert!(texts.iter().any(|t| t.contains("+  new2")));
        assert!(texts.iter().any(|t| t.contains("\u{2212}  old2")));
        // The second file's removal is red, not styled as plain context.
        let remove2 = screen
            .transcript
            .rows
            .iter()
            .find(|row| row.text.contains("\u{2212}  old2"))
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
            rendered.contains("Give Iris a task"),
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
        assert!(replay.contains("› Done"), "{replay:?}");
        assert!(replay.contains("SHELL"), "{replay:?}");
        assert!(replay.contains("$ echo hi"), "{replay:?}");
        assert!(replay.contains("Give Iris a task"), "{replay:?}");
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
    fn pane_chrome_renders_composer_statusline() {
        let mut screen = Screen::new();
        screen.set_footer(
            "sonnet 3.5".to_string(),
            Some("high".to_string()),
            "~/workspace/user-auth (feat/rate-limit)".to_string(),
        );
        let rendered = rendered_text(&mut screen, 180, 12);

        // Runtime status is the composer statusline under the hairline edge.
        assert!(rendered.contains("◉ CODE ─ SONNET 3.5 HIGH"), "{rendered}");
        // Workspace state right-aligns on the statusline itself.
        assert!(
            rendered.contains("~/workspace/user-auth ┊ git feat/rate-limit"),
            "{rendered}"
        );
        assert!(!rendered.contains("MODE code"), "{rendered}");
        assert!(!rendered.contains("CWD"), "{rendered}");
        assert!(!rendered.contains("BRANCH"), "{rendered}");
        assert!(!rendered.contains("APPROVAL auto"), "{rendered}");
        assert!(rendered.contains("Give Iris a task..."));
        assert!(!rendered.contains("Ask the agent anything..."));
        // The composer has no hint row and no box: statusline + input only.
        assert!(!rendered.contains("↵ to send"), "{rendered}");
    }

    #[test]
    fn keyboard_enhancement_pushed_only_when_supported() -> io::Result<()> {
        // Unsupported terminal: never push (safe fallback to plain key events).
        let mut out: Vec<u8> = Vec::new();
        assert!(!enable_keyboard_enhancement(&mut out, false)?);
        assert!(out.is_empty(), "{out:?}");

        // Supported terminal: push the requested flags (DISAMBIGUATE | EVENT_TYPES
        // | ALTERNATE_KEYS = 7) as CSI > 7 u.
        let mut out: Vec<u8> = Vec::new();
        assert!(enable_keyboard_enhancement(&mut out, true)?);
        let seq = String::from_utf8(out).expect("utf8");
        assert!(seq.starts_with("\x1b["), "{seq:?}");
        assert!(seq.contains(">7u"), "{seq:?}");
        Ok(())
    }

    #[test]
    fn keyboard_enhancement_popped_only_when_enabled() -> io::Result<()> {
        // Never negotiated: popping is a no-op, so no stray sequence leaks.
        let mut out: Vec<u8> = Vec::new();
        disable_keyboard_enhancement(&mut out, false)?;
        assert!(out.is_empty(), "{out:?}");

        // Negotiated: restore by popping (CSI < u).
        let mut out: Vec<u8> = Vec::new();
        disable_keyboard_enhancement(&mut out, true)?;
        let seq = String::from_utf8(out).expect("utf8");
        assert!(seq.starts_with("\x1b[<") && seq.ends_with('u'), "{seq:?}");
        Ok(())
    }

    #[test]
    fn composer_editor_uses_canonical_multiline_shape() {
        let mut screen = Screen::new();
        let lines = rendered_lines(&mut screen, 80, 8);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        // The composer top edge is a plain full hairline — no box corners.
        let top = texts
            .iter()
            .position(|line| line.trim().chars().all(|ch| ch == '─') && line.contains('─'))
            .expect("hairline top edge");

        // Statusline row (blank before a footer exists), spacer row, then the input row.
        assert!(!texts[top + 1].contains("Give Iris"), "{texts:?}");
        assert_eq!(texts[top + 2].trim(), "", "{texts:?}");
        assert!(texts[top + 3].contains("Give Iris a task..."), "{texts:?}");
        assert!(
            texts[top + 3].starts_with("      Give Iris a task..."),
            "input should align with transcript text: {texts:?}"
        );
        // No box: no side borders, no bottom border, no hint row.
        let composer = texts[top..].join("\n");
        assert!(!composer.contains('│'), "{composer:?}");
        assert!(!composer.contains('┌'), "{composer:?}");
        assert!(!composer.contains('└'), "{composer:?}");
        assert!(!composer.contains("↵ to send"), "{composer:?}");
        assert!(!texts.join("\n").contains("Give iris a task"));
    }

    #[test]
    fn composer_statusline_shows_status_with_context_meter() {
        let mut screen = Screen::new();
        screen.set_footer(
            "openai-codex/gpt-5.4-mini".to_string(),
            Some("off".to_string()),
            "~/project".to_string(),
        );
        let lines = rendered_lines(&mut screen, 120, 8);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let status = texts
            .iter()
            .find(|line| line.contains("◉ CODE"))
            .expect("statusline");

        // Mode/model/effort/context + 10-dot meter, all uppercase.
        assert!(
            status.contains("◉ CODE ─ GPT-5.4-MINI OFF ─ CTX 300K ○○○○○○○○○○"),
            "{status:?}"
        );
        // The workspace right-aligns on the same line.
        assert!(status.trim_end().ends_with("~/project"), "{status:?}");
        // The statusline is followed by a blank spacer, then the aligned input.
        let status_idx = texts
            .iter()
            .position(|line| line.contains("◉ CODE"))
            .expect("statusline");
        assert_eq!(texts[status_idx + 1].trim(), "", "{texts:?}");
        assert!(
            texts[status_idx + 2].starts_with("      Give Iris a task..."),
            "input should align with transcript text: {texts:?}"
        );
        // No box corners anywhere in the composer chrome.
        assert!(!status.contains('┌'), "{status:?}");
        // Nothing overflows the terminal width.
        for line in &texts {
            assert!(display_width(line) <= 120, "{line:?}");
        }
    }

    #[test]
    fn composer_statusline_drops_lower_priority_fields_when_narrow() {
        let mut screen = Screen::new();
        screen.set_footer(
            "openai-codex/gpt-5.4-mini".to_string(),
            Some("off".to_string()),
            "~/projects/iris (feat/composer-statusline)".to_string(),
        );

        // At a constrained width the statusline falls back to the minimum:
        // mode + model only (effort, meter, and CTX dropped, in that order).
        let status = composer_statusline(&screen, 30)
            .map(|line| line_text(&line))
            .expect("statusline");

        assert!(status.contains("◉ CODE ─ GPT-5.4-MINI"), "{status:?}");
        assert!(!status.contains("OFF"), "{status:?}");
        assert!(!status.contains("CTX"), "{status:?}");
        assert!(!status.contains('○'), "{status:?}");
        assert!(status.matches('◉').count() == 1, "{status:?}");
        assert!(display_width(&status) <= 30, "{status:?}");
    }

    #[test]
    fn composer_chrome_is_pinned_not_scrollback() {
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
        let status_idx = texts
            .iter()
            .position(|line| line.contains("◉ CODE"))
            .expect("statusline remains visible");
        let editor_idx = texts
            .iter()
            .position(|line| line.contains("Give Iris a task"))
            .expect("composer remains visible");
        // The statusline stays in the composer chrome above the spacer/input; the workspace label
        // right-aligns on the statusline itself.
        assert!(status_idx < editor_idx, "{texts:?}");
        assert!(
            texts[status_idx].contains("~/repo ┊ git feat/pin-rail"),
            "{texts:?}"
        );
    }

    #[test]
    fn context_meter_reflects_usage_and_persists_across_turn_start() {
        let mut screen = Screen::new();
        // gpt-5.5 has a 300k catalog window.
        screen.set_footer(
            "gpt-5.5".to_string(),
            Some("low".to_string()),
            "~/repo".to_string(),
        );
        // No usage yet: meter is all empty.
        let empty = composer_statusline(&screen, 110)
            .map(|l| line_text(&l))
            .expect("top");
        assert!(empty.contains("CTX 300K ○○○○○○○○○○"), "{empty:?}");

        screen.start_turn();
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: None,
            usage: Some(ProviderUsage {
                provider: "openai".to_string(),
                model: "gpt-5.5".to_string(),
                input_tokens: 90_000,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 90_000,
                cache_creation: None,
            }),
        });
        screen.end_turn();
        // 90k/300k => 30% => 3 lit dots (last is the orange edge).
        let filled = composer_statusline(&screen, 110)
            .map(|l| line_text(&l))
            .expect("top");
        assert!(filled.contains("CTX 300K ●●●○○○○○○○"), "{filled:?}");

        // The meter must NOT drop to empty at the start of the next turn.
        screen.start_turn();
        let during = composer_statusline(&screen, 110)
            .map(|l| line_text(&l))
            .expect("top");
        assert!(during.contains("CTX 300K ●●●○○○○○○○"), "{during:?}");
    }

    #[test]
    fn context_meter_resets_when_model_changes() {
        let mut screen = Screen::new();
        screen.set_footer("gpt-5.5".to_string(), None, "~/repo".to_string());
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: None,
            usage: Some(ProviderUsage {
                provider: "openai".to_string(),
                model: "gpt-5.5".to_string(),
                input_tokens: 150_000,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 150_000,
                cache_creation: None,
            }),
        });
        let before = composer_statusline(&screen, 110)
            .map(|l| line_text(&l))
            .expect("top");
        assert!(before.contains("CTX 300K ●●●●●○○○○○"), "{before:?}");

        // Switching model clears the meter (prior usage no longer maps).
        screen.set_footer("gpt-5.4".to_string(), None, "~/repo".to_string());
        let after = composer_statusline(&screen, 110)
            .map(|l| line_text(&l))
            .expect("top");
        assert!(after.contains("CTX 300K ○○○○○○○○○○"), "{after:?}");
    }

    #[test]
    fn context_meter_persists_across_case_insensitive_model_refresh() {
        let mut screen = Screen::new();
        // Use set_footer_with_context directly so the (case-sensitive) catalog
        // lookup does not change the context label between refreshes.
        screen.set_footer_with_context(
            "gpt-5.5".to_string(),
            None,
            Some("300k".to_string()),
            "~/repo".to_string(),
        );
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: None,
            usage: Some(ProviderUsage {
                provider: "openai".to_string(),
                model: "gpt-5.5".to_string(),
                input_tokens: 150_000,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 150_000,
                cache_creation: None,
            }),
        });
        let before = composer_statusline(&screen, 110)
            .map(|l| line_text(&l))
            .expect("top");
        assert!(before.contains("CTX 300K ●●●●●○○○○○"), "{before:?}");

        // A refresh with a differently-cased same model id must NOT reset the meter.
        screen.set_footer_with_context(
            "GPT-5.5".to_string(),
            None,
            Some("300k".to_string()),
            "~/repo".to_string(),
        );
        let after = composer_statusline(&screen, 110)
            .map(|l| line_text(&l))
            .expect("top");
        assert!(after.contains("CTX 300K ●●●●●○○○○○"), "{after:?}");
    }

    #[test]
    fn statusline_workspace_truncates_cwd_preserving_project_and_branch() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.5".to_string(),
            None,
            "~/projects/very/deeply/nested/path/iris-agent (main)".to_string(),
        );
        let label = composer_statusline(&screen, 80)
            .map(|line| line_text(&line))
            .expect("statusline");
        assert!(display_width(&label) <= 80, "{label:?}");
        assert!(label.contains("iris-agent"), "{label:?}");
        assert!(label.contains('…'), "{label:?}");
        assert!(label.trim_end().ends_with("┊ git main"), "{label:?}");
    }

    #[test]
    fn composer_statusline_never_overflows_at_any_width() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            "~/projects/iris (main)".to_string(),
        );
        for box_width in 6u16..=200 {
            let Some(line) = composer_statusline(&screen, box_width) else {
                continue;
            };
            let text = line_text(&line);
            assert!(
                display_width(&text) <= usize::from(box_width),
                "width {box_width}: {text:?}"
            );
            assert!(text.starts_with('◉'), "width {box_width}: {text:?}");
        }
    }

    #[test]
    fn assistant_message_working_indicator_and_statusline_have_vertical_separation() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.4".to_string(),
            None,
            "~/projects/iris-agent".to_string(),
        );
        screen.apply(UiEvent::AssistantText(
            "assistant message...\nwrapped assistant message".to_string(),
        ));
        screen.start_turn();
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: Some("resp_1".to_string()),
            usage: Some(ProviderUsage {
                provider: "openai".to_string(),
                model: "gpt-5.4".to_string(),
                input_tokens: 5_400,
                output_tokens: 137,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 5_537,
                cache_creation: None,
            }),
        });

        let lines = rendered_lines(&mut screen, 100, 16);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let working_idx = texts
            .iter()
            .position(|line| line.contains("●···") && line.contains("┊ ESC ┊"))
            .expect("working indicator");
        let status_idx = texts
            .iter()
            .position(|line| line.contains("◉ CODE"))
            .expect("composer statusline");

        assert!(
            texts[..working_idx]
                .iter()
                .any(|line| line.contains("assistant message")),
            "{texts:?}"
        );
        assert_eq!(texts[working_idx - 1].trim(), "", "{texts:?}");
        assert_eq!(texts[working_idx + 1].trim(), "", "{texts:?}");
        // blank, then the composer hairline, then the statusline.
        assert_eq!(status_idx, working_idx + 3, "{texts:?}");
        assert!(texts[working_idx].contains("↑5.4k ↓137"), "{texts:?}");
    }

    #[test]
    fn inline_working_indicator_uses_led_chase_interrupt_and_token_telemetry() {
        let mut screen = Screen::new();
        screen.set_footer(
            "opus-4.8".to_string(),
            Some("high".to_string()),
            "~/repo".to_string(),
        );
        screen.start_turn();
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: Some("resp_1".to_string()),
            usage: Some(ProviderUsage {
                provider: "anthropic".to_string(),
                model: "opus-4.8".to_string(),
                input_tokens: 177_000,
                output_tokens: 5_700,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 30,
                total_tokens: 182_700,
                cache_creation: None,
            }),
        });

        let before = rendered_text(&mut screen, 100, 16);
        assert!(!before.contains("WORKING"), "{before}");
        assert!(!before.contains("Working…"), "{before}");
        assert!(before.contains("●···"), "{before}");
        assert!(before.contains("┊ ESC ┊"), "{before}");
        assert!(before.contains("↑177k ↓5.7k"), "{before}");
        assert!(!before.contains('|'), "{before}");
        assert!(!before.contains("T+"), "{before}");
        for frame in BRAILLE_SPINNER_FRAMES {
            assert!(
                !before.contains(frame),
                "braille spinner frame {frame} leaked: {before}"
            );
        }

        assert!(screen.tick());
        let after = rendered_text(&mut screen, 100, 16);
        assert!(after.contains("·●··"), "{after}");
        let working_lines = screen.working_lines(100);
        assert_eq!(
            working_lines.len(),
            1,
            "working indicator is one line: {working_lines:?}"
        );
        let working = working_lines
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
    fn working_indicator_shows_queued_steering_count() {
        let mut screen = Screen::new();
        screen.start_turn();
        // No queued input: the indicator omits the segment.
        let none = screen
            .working_lines(100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!none.contains("queued"), "{none}");

        screen.set_queued(2);
        let two = screen
            .working_lines(100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(two.contains("2 queued"), "{two}");

        // A turn boundary clears the indicator.
        screen.end_turn();
        screen.start_turn();
        let reset = screen
            .working_lines(100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!reset.contains("queued"), "{reset}");
    }

    #[test]
    fn injected_user_message_renders_as_a_user_row() {
        let mut screen = Screen::new();
        screen.commit_user("first prompt");
        screen.start_turn();
        screen.apply(UiEvent::AssistantText("on it".to_string()));
        // A mid-run injected steering/follow-up message renders in transcript
        // order, after the assistant text that preceded it.
        screen.apply(UiEvent::UserMessage("also do this".to_string()));
        let rendered = rendered_text(&mut screen, 100, 24);
        assert!(rendered.contains("also do this"), "{rendered}");
        let prompt_idx = rendered.find("on it").expect("assistant text");
        let injected_idx = rendered.find("also do this").expect("injected row");
        assert!(
            prompt_idx < injected_idx,
            "injected row must follow: {rendered}"
        );
    }

    #[test]
    fn working_indicator_renders_all_ping_pong_led_frames() {
        let frames: Vec<String> = (0..WORKING_FRAMES.len())
            .map(|frame| {
                line_text(&working_indicator_line(
                    WORKING_FRAMES[frame],
                    Duration::from_secs(87),
                    true,
                    None,
                    0,
                    80,
                ))
                .trim()
                .to_string()
            })
            .collect();
        assert_eq!(
            frames,
            vec![
                "●··· 1:27 ┊ ESC",
                "·●·· 1:27 ┊ ESC",
                "··●· 1:27 ┊ ESC",
                "···● 1:27 ┊ ESC",
                "··●· 1:27 ┊ ESC",
                "·●·· 1:27 ┊ ESC",
            ]
        );
    }

    #[test]
    fn working_indicator_omits_unavailable_optional_fields_without_empty_separators() {
        let usage = ProviderUsage {
            provider: "openai".to_string(),
            model: "gpt-5.5".to_string(),
            input_tokens: 12_000,
            output_tokens: 5_700,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: 12_400,
            cache_creation: None,
        };
        let without_telemetry = line_text(&working_indicator_line(
            WORKING_FRAMES[0],
            Duration::from_secs(87),
            true,
            None,
            0,
            80,
        ))
        .trim()
        .to_string();
        let without_interrupt = line_text(&working_indicator_line(
            WORKING_FRAMES[0],
            Duration::from_secs(87),
            false,
            Some(&usage),
            0,
            80,
        ))
        .trim()
        .to_string();
        let elapsed_only = line_text(&working_indicator_line(
            WORKING_FRAMES[0],
            Duration::from_secs(87),
            false,
            None,
            0,
            80,
        ))
        .trim()
        .to_string();

        assert_eq!(without_telemetry, "●··· 1:27 ┊ ESC");
        assert_eq!(without_interrupt, "●··· 1:27 ┊ ↑12k ↓5.7k");
        assert_eq!(elapsed_only, "●··· 1:27");
        assert!(!without_telemetry.contains("┊ ┊"));
        assert!(!without_interrupt.contains("┊ ┊"));
        assert!(!elapsed_only.contains('┊'));
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
        assert!(done.contains("◆ DONE"), "{done}");
        assert!(
            done.contains("Successfully replaced 1 occurrence."),
            "{done}"
        );
        assert!(!done.contains("running…"), "{done}");
    }

    #[test]
    fn completed_panel_headers_use_success_dot_not_running_accent() {
        let mut transcript = Transcript::default();
        transcript.push_shell_header(
            PanelState::Done,
            Some(Duration::from_secs(1)),
            None,
            "echo hi",
        );
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
        assert!(rendered.contains("◆ DONE"), "{rendered}");
        assert!(
            rendered.contains("Successfully replaced 1 occurrence."),
            "{rendered}"
        );
        assert!(rendered.contains("┊ interleaved note"), "{rendered}");
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
        assert!(rendered.contains("◆ DONE"), "{rendered}");
        assert!(rendered.contains("$ echo hi"), "{rendered}");
        assert!(rendered.contains("hi"), "{rendered}");
        assert!(rendered.contains("┊ interleaved note"), "{rendered}");
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
    fn cancelled_exploration_tool_updates_shared_explore_panel() {
        let mut screen = Screen::new();
        let call = call_args("read", json!({ "path": "src/cancelled.rs" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolCancelled(call));

        let rendered = rendered_text(&mut screen, 100, 14);
        assert!(rendered.contains("EXPLORE"), "{rendered}");
        assert!(rendered.contains("CANCELLED"), "{rendered}");
        assert!(!rendered.contains("RUNNING"), "{rendered}");
        assert_eq!(rendered.matches("EXPLORE").count(), 1, "{rendered}");
        assert_eq!(rendered.matches("CANCELLED").count(), 1, "{rendered}");
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
        // Aggregate EXPLORE state must stay RUNNING (uppercase ERROR is the
        // state label; the errored read still streams a lowercase `error:` body).
        assert!(!running.contains("ERROR"), "{running}");

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
            body_texts
                .iter()
                .any(|text| text.contains("Grep") && text.contains("\"needle\" in src")),
            "{body_texts:?}"
        );
    }

    #[test]
    fn explore_rows_carry_verb_column_and_honest_counts() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(100);
        screen.apply(UiEvent::ToolResult {
            call: call_args("read", json!({ "path": "src/context/engine.rs" })),
            content: "  1→fn a() {}\n  2→fn b() {}\n  3→fn c() {}".to_string(),
            exit_code: None,
            duration: None,
        });
        screen.apply(UiEvent::ToolResult {
            call: call_args(
                "grep",
                json!({ "pattern": "fn emit", "path": "src/context" }),
            ),
            content: "3 matches in 2 files\nsrc/a.rs\n> 1│ fn emit".to_string(),
            exit_code: None,
            duration: None,
        });
        let rendered = rendered_text(&mut screen, 100, 22);
        // Verb column + target, with a right-aligned real count per op.
        assert!(
            rendered.contains("Read   src/context/engine.rs"),
            "{rendered}"
        );
        assert!(rendered.contains("3 lines"), "{rendered}");
        assert!(
            rendered.contains("Grep   \"fn emit\" in src/context"),
            "{rendered}"
        );
        assert!(
            rendered.contains("│    Read   src/context/engine.rs"),
            "EXPLORE body should use the framed-output inset: {rendered}"
        );
        assert!(
            rendered.contains("│    Grep   \"fn emit\" in src/context"),
            "EXPLORE body should use the framed-output inset: {rendered}"
        );
        assert!(rendered.contains("3 matches · 2 files"), "{rendered}");

        let lines = screen.wrapped_lines(100);
        let header_line = line_text(line_matching(&lines, |line| {
            let text = line_text(line);
            text.contains("EXPLORE") && text.contains("DONE")
        }));
        let done_col = header_line
            .find("DONE")
            .map(|idx| display_width(&header_line[..idx]))
            .expect("DONE header label");
        let read_line = line_text(line_matching(&lines, |line| {
            line_text(line).contains("Read   src/context/engine.rs")
        }));
        let grep_line = line_text(line_matching(&lines, |line| {
            line_text(line).contains("Grep   \"fn emit\" in src/context")
        }));
        let lines_col = read_line
            .find("lines")
            .map(|idx| display_width(&read_line[..idx]))
            .expect("lines unit");
        let files_col = grep_line
            .find("files")
            .map(|idx| display_width(&grep_line[..idx]))
            .expect("files unit");
        assert_eq!(lines_col, done_col, "{read_line:?} vs {header_line:?}");
        assert_eq!(files_col, done_col, "{grep_line:?} vs {header_line:?}");
    }

    #[test]
    fn shell_exit_row_summarizes_test_results() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "cargo test context::emit" })),
            content: "running 142 tests\ntest result: ok. 142 passed; 0 failed; 0 ignored"
                .to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(4100)),
        });
        let rendered = rendered_text(&mut screen, 90, 14);
        assert!(rendered.contains("◆ EXIT 0"), "{rendered}");
        assert!(rendered.contains("┊ 142 passed · 0 failed"), "{rendered}");
    }

    #[test]
    fn edit_panel_keeps_diff_body_through_the_whole_lifecycle() {
        let mut screen = Screen::new();
        let call = call_args("edit", json!({ "file_path": "src/context/emit.rs" }));
        let diff = "--- a/src/context/emit.rs\n+++ b/src/context/emit.rs\n@@ -40,3 +40,3 @@\n fn emit(&self, ctx: &Context) -> Prompt {\n-    let body = dump_everything(ctx);\n+    let body = self.budget.justify(ctx)?;\n";
        screen.apply(UiEvent::DiffPreview {
            call: call.clone(),
            diff: diff.to_string(),
        });
        // Pending: ◇ PREVIEW with the diff and no elapsed time.
        let preview = rendered_text(&mut screen, 100, 20);
        assert!(preview.contains("EDIT"), "{preview}");
        assert!(preview.contains("PREVIEW"), "{preview}");
        assert!(preview.contains("dump_everything"), "{preview}");
        assert!(!preview.contains("0.0s"), "{preview}");

        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "Successfully replaced 1 occurrence.".to_string(),
            exit_code: None,
            duration: Some(Duration::from_millis(400)),
        });
        // Applied: the same single EDIT panel, ◆ DONE, diff + footer.
        let done = rendered_text(&mut screen, 100, 24);
        assert!(done.contains("◆ DONE"), "{done}");
        assert!(done.contains("self.budget.justify(ctx)?;"), "{done}");
        assert!(done.contains("+1  −1"), "{done}");
        assert_eq!(done.matches("EDIT").count(), 1, "one EDIT panel: {done}");
        assert!(!done.contains("PREVIEW"), "{done}");
        assert!(
            !done.contains("Successfully replaced"),
            "the diff is the canonical EDIT body: {done}"
        );
    }

    #[test]
    fn compaction_event_renders_quiet_info_notice() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::CompactionApplied {
            compaction_id: "c1".to_string(),
            covered_from: "m1".to_string(),
            covered_to: "m9".to_string(),
            covered_messages: 12,
            original_tokens_estimate: 128_000,
            summary_tokens_estimate: 41_000,
            budget: 300_000,
        });
        let rendered = rendered_text(&mut screen, 100, 12);
        assert!(
            rendered.contains("┊ Context compacted — 128k → 41k tokens"),
            "{rendered}"
        );
        // No undo keybind exists, so no undo hint is asserted into the UI.
        assert!(!rendered.contains("ctrl+r"), "{rendered}");
    }

    #[test]
    fn thinking_header_gains_token_telemetry_when_usage_arrives() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(100);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "turn_1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoning {
            text: "Weigh the plan.\n\nThen check the emit path.".to_string(),
            redacted: false,
        });
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: None,
            usage: Some(ProviderUsage {
                provider: "openai".to_string(),
                model: "gpt-5.5".to_string(),
                input_tokens: 10_000,
                output_tokens: 3_000,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 2_400,
                total_tokens: 13_000,
                cache_creation: None,
            }),
        });
        let lines = rendered_lines(&mut screen, 100, 18);
        let header = lines
            .iter()
            .map(line_text)
            .find(|t| t.contains("THINKING"))
            .expect("thinking header");
        assert!(header.contains("↓2.4k"), "{header}");
    }

    #[test]
    fn statusline_model_is_the_underlined_picker_button() {
        let mut screen = Screen::new();
        screen.set_footer("gpt-5.5".to_string(), None, "~/repo".to_string());
        let line = composer_statusline(&screen, 100).expect("statusline");
        let model = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "GPT-5.5")
            .expect("model span");
        assert!(
            model.style.add_modifier.contains(Modifier::UNDERLINED),
            "{model:?}"
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
        assert!(rendered.contains("4.0s"), "{rendered}");
        assert!(!rendered.contains("0.0s"), "{rendered}");
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
            rendered.contains("      Add rate limiting to the login endpoint."),
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
        assert!(rendered.contains("◆ DONE"));
        assert!(rendered.contains("$ pnpm test --filter user.auth"));
        assert!(rendered.contains("PASS    test/auth.service.test.ts"));
        assert!(rendered.contains("EDIT"));
        assert!(rendered.contains("PREVIEW"), "{rendered}");
        assert!(!rendered.contains("RUNNING"), "{rendered}");
        assert!(rendered.contains("packages/user.auth/src/auth.service.ts"));
        assert!(rendered.contains("\u{2212}  old"));
        assert!(rendered.contains("+  new"));
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
    fn unsourced_composer_chrome_has_no_status_or_workspace_label() {
        let mut screen = Screen::new();
        let rendered = rendered_text(&mut screen, 80, 10);

        // No footer yet: hairline + blank statusline + input, no status text.
        assert!(!rendered.contains("◉ CODE"), "{rendered}");
        assert!(!rendered.contains("┊ git"), "{rendered}");
        assert!(rendered.contains('─'), "{rendered}");
        assert!(rendered.contains("Give Iris a task"), "{rendered}");
    }

    #[test]
    fn sourced_top_border_omits_unknown_effort_and_workspace_omits_branch() {
        let mut screen = Screen::new();
        screen.set_footer("gpt-5.5".to_string(), None, "~/repo".to_string());
        let rendered = rendered_text(&mut screen, 100, 10);

        // No effort token between the model and the CTX separator.
        assert!(
            rendered.contains("◉ CODE ─ GPT-5.5 ─ CTX 300K"),
            "{rendered}"
        );
        // No branch: a bare cwd label with no git suffix.
        assert!(rendered.contains("~/repo"), "{rendered}");
        assert!(!rendered.contains("┊ git"), "{rendered}");
    }

    #[test]
    fn sourced_top_border_renders_effort_after_model() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            "~/repo (branch)".to_string(),
        );
        let rendered = rendered_text(&mut screen, 100, 10);

        assert!(
            rendered.contains("◉ CODE ─ GPT-5.5 HIGH ─ CTX 300K"),
            "{rendered}"
        );
        assert!(rendered.contains("~/repo ┊ git branch"), "{rendered}");
    }

    fn transcript_text(screen: &mut Screen, width: u16) -> String {
        screen
            .wrapped_lines(width)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn revealed_shell_panel_stays_revealed_across_updates_and_finalize() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80); // prime last_width so hints fit the body
        let call = call_args("bash", json!({ "command": "seq" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));

        // Stream enough lines that the live tail caps and becomes foldable.
        let chunk = std::iter::once("FIRSTLINE".to_string())
            .chain((2..=20).map(|n| format!("line {n}")))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk,
        });

        // Default preview: collapsed marker, expand hint, earliest line hidden.
        let preview = transcript_text(&mut screen, 80);
        assert!(preview.contains("▸"), "{preview}");
        assert!(preview.contains("ctrl+o to expand"), "{preview}");
        assert!(!preview.contains("FIRSTLINE"), "{preview}");

        // Reveal; expansion must survive a later delta and the final result.
        assert!(screen.toggle_latest_panel());
        let revealed = transcript_text(&mut screen, 80);
        assert!(revealed.contains("▾"), "{revealed}");
        assert!(revealed.contains("ctrl+o to collapse"), "{revealed}");
        assert!(revealed.contains("FIRSTLINE"), "{revealed}");

        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "line 21\n".to_string(),
        });
        let after_delta = transcript_text(&mut screen, 80);
        assert!(after_delta.contains("▾"), "{after_delta}");
        assert!(after_delta.contains("FIRSTLINE"), "{after_delta}");

        screen.apply(UiEvent::ToolResult {
            call,
            content: std::iter::once("FIRSTLINE".to_string())
                .chain((2..=21).map(|n| format!("line {n}")))
                .collect::<Vec<_>>()
                .join("\n"),
            exit_code: None,
            duration: None,
        });
        let after_result = transcript_text(&mut screen, 80);
        assert!(after_result.contains("▾"), "{after_result}");
        assert!(
            after_result.contains("ctrl+o to collapse"),
            "{after_result}"
        );
        assert!(after_result.contains("FIRSTLINE"), "{after_result}");
    }

    #[test]
    fn ctrl_o_reveals_and_recollapses_capped_tool_output() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80); // prime last_width so hints fit the body
        // HEAD/TAIL capping hides the middle, so the unique marker sits there.
        let content = (1..=20)
            .map(|n| {
                if n == 10 {
                    "MIDDLELINE".to_string()
                } else {
                    format!("line {n}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "seq" })),
            content,
            exit_code: None,
            duration: None,
        });

        // Default preview: collapsed marker, expand hint, middle line hidden.
        let preview = transcript_text(&mut screen, 80);
        assert!(preview.contains("▸"), "{preview}");
        assert!(preview.contains("ctrl+o to expand"), "{preview}");
        assert!(!preview.contains("MIDDLELINE"), "{preview}");

        // Expand reveals the hidden line and switches the hint.
        assert!(screen.toggle_latest_panel());
        let revealed = transcript_text(&mut screen, 80);
        assert!(revealed.contains("▾"), "{revealed}");
        assert!(revealed.contains("MIDDLELINE"), "{revealed}");
        assert!(revealed.contains("ctrl+o to collapse"), "{revealed}");
        assert!(!revealed.contains("ctrl+o to expand"), "{revealed}");

        // Collapse again restores the capped preview.
        assert!(screen.toggle_latest_panel());
        let recollapsed = transcript_text(&mut screen, 80);
        assert!(recollapsed.contains("▸"), "{recollapsed}");
        assert!(!recollapsed.contains("MIDDLELINE"), "{recollapsed}");
        assert!(recollapsed.contains("ctrl+o to expand"), "{recollapsed}");
    }

    #[test]
    fn assistant_table_never_exceeds_frame_width() {
        let md = "| Column one heading here | Column two heading here | Three |\n| - | - | - |\n| a fairly long cell value goes here | another long value also here | x |";
        for width in [40u16, 60, 80] {
            // Committed path.
            let mut screen = Screen::new();
            let _ = screen.wrapped_lines(width);
            screen.apply(UiEvent::AssistantText(md.to_string()));
            for line in rendered_lines(&mut screen, width, 24) {
                let w = super::wrap::display_width(&line_text(&line));
                assert!(
                    w <= width as usize,
                    "committed table line exceeds frame {width}: {:?}",
                    line_text(&line)
                );
            }
            // Streaming path.
            let mut screen = Screen::new();
            let _ = screen.wrapped_lines(width);
            screen.apply(UiEvent::AssistantTextDelta(md.to_string()));
            for line in rendered_lines(&mut screen, width, 24) {
                let w = super::wrap::display_width(&line_text(&line));
                assert!(
                    w <= width as usize,
                    "streaming table line exceeds frame {width}: {:?}",
                    line_text(&line)
                );
            }
        }
    }

    #[test]
    fn reasoning_renders_collapsed_thinking_block_by_default() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantReasoning {
            text: "First I check the **config**.\n\nThen the cache.\n\nThen I stop.".to_string(),
            redacted: false,
        });
        let collapsed = rendered_text(&mut screen, 80, 18);
        // Collapsed: label + collapsed arrow, the first-paragraph preview, and
        // the paragraph-count fold affordance; later paragraphs hidden.
        assert!(collapsed.contains("THINKING"), "{collapsed}");
        assert!(collapsed.contains("▸"), "{collapsed}");
        assert!(collapsed.contains("First I check"), "{collapsed}");
        assert!(collapsed.contains("… 2 more paragraphs"), "{collapsed}");
        assert!(collapsed.contains("ctrl+o to expand"), "{collapsed}");
        assert!(
            !collapsed.contains("Then the cache"),
            "later paragraphs should be hidden while collapsed: {collapsed}"
        );
    }

    #[test]
    fn short_reasoning_is_shown_whole_and_not_foldable() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantReasoning {
            text: "One short thought.".to_string(),
            redacted: false,
        });
        let rendered = rendered_text(&mut screen, 80, 14);
        assert!(rendered.contains("THINKING"), "{rendered}");
        assert!(rendered.contains("One short thought."), "{rendered}");
        assert!(!rendered.contains("more paragraph"), "{rendered}");
        // Nothing hidden: ctrl+o has nothing to toggle.
        assert!(!screen.toggle_latest_panel());
    }

    #[test]
    fn reasoning_thinking_block_expands_to_show_trace() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantReasoning {
            text: "Inspect the config.\n\nThen inspect the cache.".to_string(),
            redacted: false,
        });
        // Thinking panel is the latest panel for a reasoning-only turn.
        assert!(screen.toggle_latest_panel());
        let expanded = rendered_text(&mut screen, 80, 14);
        assert!(expanded.contains("▾"), "{expanded}");
        assert!(
            expanded.contains("Then inspect the cache."),
            "expanded trace missing: {expanded}"
        );
        assert!(!expanded.contains("more paragraph"), "{expanded}");
    }

    #[test]
    fn reasoning_block_is_a_chromeless_left_rail() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantReasoning {
            text: "Weigh the options.\n\nPick one.".to_string(),
            redacted: false,
        });
        // Reasoning is recessive: it never gets box chrome (Top/Bottom/Separator/
        // Header/Body) — only the rail markers.
        for row in &screen.transcript.rows {
            assert!(
                !matches!(
                    row.chrome.as_ref(),
                    Some(
                        ChromeRow::Top
                            | ChromeRow::Bottom
                            | ChromeRow::Separator
                            | ChromeRow::Header { .. }
                            | ChromeRow::Body { .. }
                    )
                ),
                "reasoning must not use box chrome: {:?}",
                row.text
            );
        }
        assert!(
            screen.transcript.rows.iter().any(|r| matches!(
                r.chrome.as_ref(),
                Some(ChromeRow::RailHeader {
                    expanded: false,
                    ..
                })
            )),
            "collapsed rail header missing"
        );
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|r| matches!(r.chrome.as_ref(), Some(ChromeRow::RailEnd))),
            "rail end marker missing"
        );
        // The header renders as a muted `▸ THINKING` line (arrow + label, no
        // box); the rail glyph lives on the body rows.
        let lines: Vec<String> = rendered_lines(&mut screen, 80, 14)
            .into_iter()
            .map(|line| line_text(&line))
            .collect();
        let header = lines
            .iter()
            .find(|t| t.contains("THINKING"))
            .expect("THINKING rail header");
        assert!(header.contains('\u{25b8}'), "collapsed arrow ▸: {header}");
        assert!(!header.contains('\u{2502}'), "no box side │: {header}");
        let body = lines
            .iter()
            .find(|t| t.contains("Weigh the options."))
            .expect("preview body row");
        assert!(body.contains('\u{250a}'), "rail glyph ┊ on body: {body}");
    }

    #[test]
    fn redacted_reasoning_never_renders_trace_text() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantReasoning {
            // A redacted block carries no text downstream; this guards against a
            // future regression that would render recovered text.
            text: String::new(),
            redacted: true,
        });
        // A redacted block is a single placeholder paragraph: shown whole,
        // nothing foldable.
        assert!(!screen.toggle_latest_panel());
        let rendered = rendered_text(&mut screen, 80, 14);
        assert!(rendered.contains("THINKING"), "{rendered}");
        assert!(
            rendered.contains("withheld"),
            "redacted placeholder missing: {rendered}"
        );
    }

    #[test]
    fn reasoning_renders_above_streamed_answer_without_duplication() {
        // Real provider path: answer text streams as deltas, then reasoning and
        // the terminal text event arrive at completion. The thinking block must
        // land above the committed answer, and the answer must appear once.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantTextDelta("The ".to_string()));
        screen.apply(UiEvent::AssistantTextDelta("answer.".to_string()));
        screen.apply(UiEvent::AssistantReasoning {
            text: "deliberating".to_string(),
            redacted: false,
        });
        screen.apply(UiEvent::AssistantTextEnd("The answer.".to_string()));
        let out = rendered_text(&mut screen, 80, 16);
        let thinking_at = out.find("THINKING").expect("thinking label");
        let answer_at = out.find("The answer.").expect("answer");
        assert!(
            thinking_at < answer_at,
            "thinking block should precede the streamed answer: {out}"
        );
        assert_eq!(
            out.matches("The answer.").count(),
            1,
            "streamed answer must be committed exactly once: {out}"
        );
    }

    #[test]
    fn reasoning_renders_before_assistant_text() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantReasoning {
            text: "planning".to_string(),
            redacted: false,
        });
        screen.apply(UiEvent::AssistantText("Here is the answer.".to_string()));
        let out = rendered_text(&mut screen, 80, 16);
        let thinking_at = out.find("THINKING").expect("thinking label");
        let answer_at = out.find("Here is the answer.").expect("answer");
        assert!(
            thinking_at < answer_at,
            "thinking block should precede the answer: {out}"
        );
    }

    #[test]
    fn hidden_output_affordance_includes_count_and_ctrl_o_hint() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
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
        let lines = screen.wrapped_lines(80);
        let fold = line_matching(&lines, |line| line_text(line).contains("hidden"));
        let text = line_text(fold);
        assert!(text.contains("…"), "{text}");
        assert!(text.contains("lines hidden"), "{text}");
        assert!(text.contains("ctrl+o"), "{text}");
        assert!(display_width(&text) <= 80, "{text}");
    }

    #[test]
    fn hidden_shell_output_moves_expand_hint_to_exit_row_when_finished() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        let content = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "seq" })),
            content,
            exit_code: Some(0),
            duration: None,
        });
        let lines = screen.wrapped_lines(80);
        let hidden = line_text(line_matching(&lines, |line| {
            line_text(line).contains("lines hidden")
        }));
        let exit = line_text(line_matching(&lines, |line| {
            line_text(line).contains("EXIT 0")
        }));

        assert!(!hidden.contains("ctrl+o"), "{hidden}");
        assert!(exit.contains("ctrl+o to expand"), "{exit}");
        assert!(display_width(&exit) <= 80, "{exit}");
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
                        ("◆".to_string(), prompt_style()),
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
                // The reasoning rail is chromeless (not a box panel): its header
                // and end markers never open/close `in_panel`, and its trace rows
                // are plain rows outside any box.
                Some(ChromeRow::RailHeader { .. } | ChromeRow::RailEnd) => {}
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
        assert!(rendered.contains("Read   src/tool_display.rs"));
        assert!(rendered.contains("Grep   \"DiffPreview\" in src/ui"));
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
             │ ▾  SHELL    bash                                     ◆ DONE        0ms   ▣│\n\
             ├────────────────────────────────────────────────────────────────────────────┤\n\
             │  $ edit /tmp/demo.txt                                                     │\n\
             └────────────────────────────────────────────────────────────────────────────┘",
        );
        let lines: Vec<String> = screen.wrapped_lines(80).iter().map(line_text).collect();
        let joined = lines.join("\n");

        assert!(!joined.contains("USER"), "{joined}");
        assert!(
            lines
                .first()
                .is_some_and(|line| line.starts_with("      ┌")),
            "{lines:?}"
        );
        for line in &lines {
            assert!(
                display_width(line) <= 80,
                "user prompt row exceeds width: {line:?}"
            );
            if !line.is_empty() {
                assert!(line.starts_with("      "), "{line:?}");
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
        assert_eq!(replay.matches("Give Iris a task").count(), 1, "{replay:?}");
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
        assert!(replay.contains("Give Iris a task"), "{replay:?}");
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
        assert!(rendered.contains("Give Iris a task"), "{rendered}");
        let model_idx = rendered.find("GPT 5.5").expect("model row");
        let editor_idx = rendered.find("Give Iris a task").expect("composer row");
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

        let rendered = rendered_text(&mut screen, 80, 17);
        assert!(rendered.contains("Sonnet 5"), "{rendered}");
        assert!(rendered.contains("effort (xhigh)"), "{rendered}");
        assert!(rendered.contains("SELECT MODEL"), "{rendered}");
        assert!(rendered.contains("Give Iris a task"), "{rendered}");
    }

    #[test]
    fn long_composer_line_wraps_instead_of_scrolling_right() {
        let mut screen = Screen::new();
        screen.editor.insert_str("abcdefghijklmnopqrst");

        let rendered = rendered_text(&mut screen, 18, 8);
        assert!(rendered.contains("abcdefghijk"), "{rendered}");
        assert!(rendered.contains("lmnopqrst"), "{rendered}");
        for line in rendered.lines() {
            assert!(display_width(line) <= 18, "{line:?}");
        }
    }

    #[test]
    fn footer_shows_real_provider_usage_when_reported() {
        let mut screen = Screen::new();
        screen.set_footer(
            "opus-4.8".to_string(),
            Some("xhigh".to_string()),
            "~/repo (branch)".to_string(),
        );
        screen.start_turn();
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
        let rendered = rendered_text(&mut screen, 120, 12);
        assert!(rendered.contains("◉ CODE ─ OPUS-4.8 XHIGH"), "{rendered}");
        assert!(rendered.contains("↑100 ↓20"), "{rendered}");
        assert!(
            !rendered.contains("thinking with xhigh effort"),
            "{rendered}"
        );

        screen.set_footer(
            "opus-4.8".to_string(),
            Some("high".to_string()),
            "~/repo (branch)".to_string(),
        );
        let refreshed = rendered_text(&mut screen, 120, 12);
        assert!(refreshed.contains("◉ CODE ─ OPUS-4.8 HIGH"), "{refreshed}");
        assert!(refreshed.contains("↑100 ↓20"), "{refreshed}");
        assert!(
            !refreshed.contains("thinking with high effort"),
            "{refreshed}"
        );
    }

    #[test]
    fn working_indicator_formats_elapsed_duration_compactly() {
        let under_ten = line_text(&working_indicator_line(
            WORKING_FRAMES[0],
            Duration::from_millis(500),
            true,
            None,
            0,
            80,
        ));
        assert!(under_ten.contains("0.5s"), "{under_ten}");
        assert!(!under_ten.contains("T+"), "{under_ten}");
        assert!(!under_ten.contains("00:00:00s"), "{under_ten}");

        let over_ten = line_text(&working_indicator_line(
            WORKING_FRAMES[0],
            Duration::from_secs(13),
            true,
            None,
            0,
            80,
        ));
        assert!(over_ten.contains("13s"), "{over_ten}");

        let over_minute = line_text(&working_indicator_line(
            WORKING_FRAMES[0],
            Duration::from_secs(87),
            true,
            None,
            0,
            80,
        ));
        assert!(over_minute.contains("1:27"), "{over_minute}");

        let over_hour = line_text(&working_indicator_line(
            WORKING_FRAMES[0],
            Duration::from_secs(3734),
            true,
            None,
            0,
            80,
        ));
        assert!(over_hour.contains("1:02:14"), "{over_hour}");
    }

    #[test]
    fn conversational_turn_emits_no_turn_rule() {
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
    fn tool_backed_turn_appends_quiet_divider_with_elapsed_and_telemetry() {
        let mut screen = Screen::new();
        screen.set_footer(
            "opus-4.8".to_string(),
            Some("high".to_string()),
            "~/repo (branch)".to_string(),
        );
        screen.start_turn();
        let call = call_args("bash", json!({ "command": "echo hi" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(12)),
        });
        screen.apply(UiEvent::AssistantText("done".to_string()));
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: None,
            usage: Some(ProviderUsage {
                provider: "anthropic".to_string(),
                model: "opus-4.8".to_string(),
                input_tokens: 18_200,
                output_tokens: 846,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 19_046,
                cache_creation: None,
            }),
        });
        screen.end_turn();

        let lines: Vec<String> = screen.wrapped_lines(90).iter().map(line_text).collect();
        let divider = lines
            .iter()
            .find(|line| line.contains("↑18.2k ↓846"))
            .expect("turn divider with telemetry");
        assert!(divider.trim_start().starts_with("────── "), "{divider}");
        assert!(divider.contains(" ┊ ↑18.2k ↓846 "), "{divider}");
        assert!(!divider.contains("Worked for"), "{divider}");
        assert!(!divider.contains("T+"), "{divider}");
        assert_eq!(display_width(divider), 90);
        let idx = lines.iter().position(|line| line == divider).unwrap();
        assert_eq!(lines[idx - 1].trim(), "", "{lines:?}");
        assert_eq!(lines[idx + 1].trim(), "", "{lines:?}");
    }

    #[test]
    fn provider_turn_error_counts_as_runtime_work_for_divider() {
        let mut screen = Screen::new();
        screen.start_turn();
        screen.apply(UiEvent::ProviderTurnError {
            turn_id: "turn_1".to_string(),
            message: "rate limited".to_string(),
        });
        screen.end_turn();

        let lines: Vec<String> = screen.wrapped_lines(60).iter().map(line_text).collect();
        assert!(
            lines
                .iter()
                .any(|line| line.trim_start().starts_with("────── ")),
            "runtime-error divider missing: {lines:?}"
        );
    }

    #[test]
    fn turn_divider_label_omits_telemetry_when_usage_is_unavailable() {
        let line = line_text(&turn_divider_line(Some(Duration::from_secs(16)), None, 60));

        assert!(line.contains("── 16s ─"), "{line}");
        assert!(!line.contains('┊'), "{line}");
        assert_eq!(display_width(&line), 60);
    }

    #[test]
    fn turn_divider_elapsed_aligns_with_working_indicator_elapsed() {
        let divider = line_text(&inset_rule_line(
            90,
            &turn_divider_label(Some(Duration::from_secs(27)), None),
        ));
        let working = line_text(&working_indicator_line(
            WORKING_FRAMES[1],
            Duration::from_millis(700),
            true,
            None,
            0,
            90,
        ));

        let divider_at = divider
            .find("27s")
            .map(|idx| display_width(&divider[..idx]));
        let working_at = working
            .find("0.7s")
            .map(|idx| display_width(&working[..idx]));
        assert_eq!(divider_at, working_at);
    }

    #[test]
    fn turn_divider_unlabelled_when_elapsed_is_unavailable() {
        let line = line_text(&turn_divider_line(None, None, 60));

        assert_eq!(line, "─".repeat(60));
    }

    #[test]
    fn elapsed_format_and_labelled_rule() {
        assert_eq!(format_elapsed_compact(Duration::from_millis(500)), "0.5s");
        assert_eq!(format_elapsed_compact(Duration::from_millis(9900)), "9.9s");
        // Threshold boundaries: tenths < 10s, bare seconds < 60s, M:SS < 60min,
        // then H:MM:SS.
        assert_eq!(format_elapsed_compact(Duration::from_secs(10)), "10s");
        assert_eq!(format_elapsed_compact(Duration::from_secs(45)), "45s");
        assert_eq!(format_elapsed_compact(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed_compact(Duration::from_secs(60)), "1:00");
        assert_eq!(format_elapsed_compact(Duration::from_secs(71)), "1:11");
        assert_eq!(format_elapsed_compact(Duration::from_secs(132)), "2:12");
        assert_eq!(format_elapsed_compact(Duration::from_secs(3599)), "59:59");
        assert_eq!(format_elapsed_compact(Duration::from_secs(3600)), "1:00:00");
        assert_eq!(format_elapsed_compact(Duration::from_secs(3669)), "1:01:09");
    }

    #[test]
    fn frame_shows_slash_palette_when_typing_command() {
        let mut screen = Screen::new();
        screen.editor.insert_str("/");
        screen.sync_palette();
        let lines = rendered_lines(&mut screen, 80, 18);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("/exit"));
        // The palette is a bordered overlay box (SlashMenu idiom).
        assert!(rendered.contains('┌'), "{rendered}");
        assert!(rendered.contains('└'), "{rendered}");
        let exit = line_matching(&lines, |line| line_text(line).contains("/exit"));
        // The selected row carries the surface fill + a bold name; the
        // description stays muted — never a cyan foreground accent.
        assert!(
            exit.spans
                .iter()
                .any(|span| span.style.bg == Some(crate::ui::palette::SURFACE)),
            "selected slash row should use the surface fill: {exit:?}"
        );
        assert!(
            exit.spans.iter().any(|span| {
                span.content.as_ref().contains("/exit")
                    && span.style.add_modifier.contains(Modifier::BOLD)
            }),
            "selected command name should be bold: {exit:?}"
        );
        assert!(
            exit.spans
                .iter()
                .all(|span| span.style.fg != Some(Color::Cyan)),
            "no cyan selection accent: {exit:?}"
        );
        let model = line_matching(&lines, |line| line_text(line).contains("/model"));
        // Descriptions align in one column across rows.
        assert_eq!(
            line_text(exit).find("End the session"),
            line_text(model).find("Show or switch provider/model")
        );
        assert!(
            model
                .spans
                .iter()
                .all(|span| span.style.bg != Some(crate::ui::palette::SURFACE)),
            "unselected rows are unfilled: {model:?}"
        );
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
        assert!(rendered.contains("EXIT 1"), "{rendered}");
    }

    #[test]
    fn shell_panel_closes_with_exit_status_result_row_end_to_end() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolStarted(call_args(
            "bash",
            json!({ "command": "cargo test" }),
        )));
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "cargo test" })),
            content: "test result: ok. 142 passed; 0 failed".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(120)),
        });

        let rendered = rendered_text(&mut screen, 80, 12);
        assert!(rendered.contains("│    $ cargo test"), "{rendered}");
        assert!(rendered.contains("│    test result"), "{rendered}");
        assert!(rendered.contains("\u{25c6} EXIT 0"), "{rendered}");
    }

    #[test]
    fn finalized_headers_use_started_elapsed_when_duration_is_missing() {
        let mut transcript = Transcript::default();
        let started = Instant::now() - Duration::from_secs(2);
        transcript.push_shell_header(PanelState::Done, None, Some(started), "echo hi");
        let rendered = transcript
            .render(100)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("SHELL"), "{rendered}");
        assert!(!rendered.contains("0.0s"), "{rendered}");
        assert!(rendered.contains("2.0s"), "{rendered}");
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
    fn fallback_tool_cancelled_renders_cancelled_without_error_body() {
        let mut screen = Screen::new();
        let call = call_args("bash", json!({ "command": "exit 2" }));

        screen.apply(UiEvent::ToolCancelled(call));

        let rendered = rendered_text(&mut screen, 80, 12);
        assert!(rendered.contains("SHELL"), "{rendered}");
        assert!(rendered.contains("CANCELLED"), "{rendered}");
        assert!(!rendered.contains("ERROR"), "{rendered}");
        assert!(!rendered.contains("error: cancelled"), "{rendered}");
    }

    #[test]
    fn cancelled_shell_panel_keeps_streamed_output() {
        let mut screen = Screen::new();
        screen.start_turn();
        let call = call_args("bash", json!({ "command": "sleep 9" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "partial line\n".to_string(),
        });
        screen.apply(UiEvent::ToolCancelled(call.clone()));
        let rendered = rendered_text(&mut screen, 80, 14);
        assert!(rendered.contains("SHELL"), "{rendered}");
        assert!(rendered.contains("CANCELLED"), "{rendered}");
        assert!(!rendered.contains("ERROR"), "{rendered}");
        assert!(!rendered.contains("DONE"), "{rendered}");
        assert!(rendered.contains("$ sleep 9"), "{rendered}");
        assert!(rendered.contains("partial line"), "{rendered}");
        assert!(!rendered.contains("error: cancelled"), "{rendered}");
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
