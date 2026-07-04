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
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
use crate::ui::screen_mode::ScreenMode;
use crate::ui::terminal_surface::TerminalSurface;
use pager::PagerSurface;

mod component;
mod overlay;
mod pager;
mod pane;
mod panel;
mod rows;
mod screen;
mod session_menu;
mod shell_command;
mod startup;
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
pub(crate) use screen::{ApprovalPolicy, Screen};
pub(crate) use screen::{BarSegment, session_bar_hit};
use screen::{compact_count, render_document_with_hints};
#[cfg(test)]
use screen::{
    composer_statusline, editor_visual_rows, fresh_editor, render_document,
    render_document_with_chrome_tail, session_bar, working_indicator_line,
};
pub(crate) use session_menu::{GitMenu, MenuAction, MenuKey, MenuOutcome, SessionMenu, TreeMenu};
pub(crate) use startup::StartAction;
#[cfg(test)]
use transcript::Transcript;
#[cfg(test)]
use wrap::display_width;
pub(crate) use wrap::wrap_to_width;

/// Editor box grows with content up to this many text rows, then scrolls
/// internally (keeps the transcript from being squeezed by a huge paste).
const MAX_EDITOR_ROWS: u16 = 8;

/// Above-editor menu height cap, including the blank row above and below.
const MAX_MENU_ROWS: u16 = 16;
/// Minimum composer height: hairline + one input row + internal rule + statusline.
const MIN_EDITOR_H: u16 = 4;
/// Composer chrome rows around the input: the hairline top edge above, plus
/// the internal rule and bottom statusline below.
const EDITOR_VERTICAL_CHROME_ROWS: u16 = 3;
/// Composer chrome above the input rows: the hairline top edge only (the rule
/// and statusline sit below the input).
const EDITOR_CHROME_ROWS_ABOVE: u16 = 1;
/// Blank row below the composer statusline so it does not sit on the screen edge.
const EDITOR_BOTTOM_PADDING_ROWS: u16 = 1;

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
/// Frameless body hang: the block body indents under the header so it aligns
/// under the TOOL label, past the disclosure glyph (the spec's `2.5ch` hang
/// snapped to the terminal cell grid).
const PANEL_BODY_INDENT: usize = 3;
const PANEL_BODY_CHROME_WIDTH: usize = PANEL_BODY_INDENT;
/// Footer hang: the hairline rule and state-label row sit one cell left of the
/// body (the spec's `2.5ch` hang rounded DOWN), while their right edge stays on
/// the block's right rail.
const PANEL_FOOTER_INDENT: usize = 2;

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
    /// Alt-screen pager lifecycle guard (ADR-0029). `Some` only in pager mode;
    /// inline mode never touches the alternate screen.
    pager: Option<PagerSurface>,
    pub(crate) screen: Screen,
    active: bool,
    /// Whether keyboard-enhancement flags were successfully pushed, so they are
    /// popped exactly once on shutdown/error and never on terminals that did not
    /// negotiate the protocol.
    keyboard_enhanced: bool,
    /// Mouse-capture state actually applied to the terminal (pager mode only).
    /// `draw` syncs it to `Screen::mouse_capture` so the Ctrl+T / `/mouse`
    /// toggle takes effect on the next frame.
    mouse_applied: bool,
}

impl TuiUi {
    /// Enter raw mode ONCE, enable bracketed paste + modified-key reporting,
    /// hide the hardware cursor, and create the Iris terminal surface. Inline
    /// mode deliberately does NOT capture the mouse, so the terminal owns
    /// scroll/select/copy over the normal screen scrollback; pager mode
    /// captures it by default (wheel scrolls the Iris-owned scrollback) with a
    /// Ctrl+T / `/mouse` runtime toggle. Everything is restored on
    /// `drop`/`shutdown`, the panic hook, and the signal handler's emergency
    /// escape on a force-quit.
    pub(crate) fn new(mode: ScreenMode) -> Result<Self> {
        // Capture cooked-mode termios before raw mode so the force-quit signal
        // handler can restore the tty even though Drop will not run then.
        crate::signals::save_termios_for_force_quit();
        enable_raw_mode()?;
        // Arm the force-quit emergency restore BEFORE any terminal-owned state
        // beyond raw mode is entered, so a repeat Ctrl-C in the setup window
        // (paste/focus flags, keyboard protocol, alt screen) still restores the
        // tty. Every error unwind below disarms it again.
        crate::signals::enable_terminal_restore_on_force_quit();
        let mut stdout = io::stdout();
        // Probe Kitty keyboard-protocol support before negotiating so the push is
        // gated and the matching pop is conditional. A probe error is treated as
        // "unsupported" (safe fallback to plain Crossterm key events).
        let supports_enhancement = supports_keyboard_enhancement().unwrap_or(false);
        // Focus reporting drives the unfocused-pane animation pause (tmux with
        // `focus-events on`, most terminals natively). Terminals without it
        // ignore the mode and simply never send focus events, so Iris stays in
        // its default focused state.
        if let Err(error) = execute!(stdout, EnableBracketedPaste, EnableFocusChange, Hide) {
            let _ = execute!(stdout, DisableBracketedPaste, DisableFocusChange, Show);
            let _ = disable_raw_mode();
            crate::signals::disable_terminal_restore_on_force_quit();
            return Err(error.into());
        }
        // Best-effort: a failure to negotiate the protocol must not abort startup.
        let keyboard_enhanced =
            enable_keyboard_enhancement(&mut stdout, supports_enhancement).unwrap_or(false);
        // Pager mode: enter the alternate screen last (after every mode toggle
        // above), with the panic hook installed first so a panic between here
        // and shutdown always restores the normal screen before the message
        // prints. On enter failure, unwind the setup exactly like the paste/
        // focus error path.
        let pager = match mode {
            ScreenMode::Inline => None,
            ScreenMode::Pager => {
                pager::install_panic_hook();
                match PagerSurface::enter() {
                    Ok(pager) => Some(pager),
                    Err(error) => {
                        let _ = disable_keyboard_enhancement(&mut stdout, keyboard_enhanced);
                        let _ = execute!(stdout, DisableBracketedPaste, DisableFocusChange, Show);
                        let _ = disable_raw_mode();
                        crate::signals::disable_terminal_restore_on_force_quit();
                        return Err(error.into());
                    }
                }
            }
        };
        crate::telemetry::set_tui_active(true);
        let mut screen = Screen::new();
        screen.pager_active = pager.is_some();
        // Mouse capture is on by default in pager mode (wheel scrolls the
        // Iris-owned scrollback); best-effort -- a failure leaves it off and
        // the statusline shows the state. Inline mode never captures.
        let mouse_applied =
            pager.is_some() && pager::set_mouse_capture(&mut io::stdout(), true).is_ok();
        screen.mouse_capture = mouse_applied;
        Ok(Self {
            surface: TerminalSurface::new(stdout),
            pager,
            screen,
            active: true,
            keyboard_enhanced,
            mouse_applied,
        })
    }

    pub(crate) fn draw(&mut self) -> Result<()> {
        let (width, height) = terminal_size()?;
        let size = Size::new(width.max(1), height.max(1));
        // Pager mode: full frame from the same logical state, stock ratatui
        // diffing. Inline mode: the Iris-owned scrollback-append surface.
        if let Some(pager) = self.pager.as_mut() {
            // Sync the terminal's mouse capture to the toggled desired state
            // before the frame, so `/mouse` / Ctrl+T take effect immediately.
            if self.screen.mouse_capture != self.mouse_applied
                && pager::set_mouse_capture(self.surface.writer_mut(), self.screen.mouse_capture)
                    .is_ok()
            {
                self.mouse_applied = self.screen.mouse_capture;
            }
            let screen = &mut self.screen;
            pager.render_with(|frame_size| pager::compose_frame(screen, frame_size))?;
            return Ok(());
        }
        let document = render_document_with_hints(&mut self.screen, size);
        self.surface.render_with_hints(
            size,
            &document.lines,
            document.chrome_tail,
            document.stable_prefix,
        )?;
        Ok(())
    }

    /// Snapshot the rendered document for `/debug`: the current terminal size
    /// plus every rendered line as `[idx] (w=NN) "escaped text"`, mirroring
    /// pi-mono's debug dump of all rendered lines with visible widths. The
    /// zero-width hardware-cursor marker is stripped so widths reflect what the
    /// terminal shows.
    pub(crate) fn debug_render_lines(&mut self) -> Result<(Size, Vec<String>)> {
        let (width, height) = terminal_size()?;
        let size = Size::new(width.max(1), height.max(1));
        let document = render_document_with_hints(&mut self.screen, size);
        let lines = document
            .lines
            .iter()
            .enumerate()
            .map(|(idx, line)| {
                let text: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .filter(|content| *content != crate::ui::terminal_surface::CURSOR_MARKER)
                    .collect();
                format!("[{idx}] (w={}) {text:?}", wrap::display_width(&text))
            })
            .collect();
        Ok((size, lines))
    }

    fn restore(&mut self) {
        if self.active {
            match self.pager.take() {
                Some(mut pager) => {
                    // Drop mouse capture before leaving the alt screen so the
                    // shell never receives mouse escapes.
                    if self.mouse_applied {
                        let _ = pager::set_mouse_capture(self.surface.writer_mut(), false);
                        self.mouse_applied = false;
                    }
                    // Pager mode: reset render-toggled terminal modes (autowrap,
                    // synchronized output -- they are terminal-global, not
                    // per-screen) while still inside the alt screen, then leave;
                    // the terminal restores the pre-session normal screen, so
                    // there is no transcript replay (the alt screen owns no
                    // scrollback to hand back).
                    let _ = self.surface.cleanup_modes();
                    let _ = pager.leave();
                }
                None => {
                    // Replace the interactive chrome with transcript-only
                    // content so the shell prompt resumes below conversation
                    // history, not below a stale editor box.
                    if let Ok((width, height)) = terminal_size() {
                        let size = Size::new(width.max(1), height.max(1));
                        let transcript = self.screen.wrapped_lines(size.width);
                        let _ = self.surface.render(size, &transcript.lines);
                    }
                    let _ = self.surface.finish();
                }
            }
            // Restore the keyboard protocol first (pop only if pushed), then the
            // paste mode and cursor, then raw mode. Ordering mirrors setup in
            // reverse so no terminal mode Iris toggled is left enabled.
            let _ = disable_keyboard_enhancement(self.surface.writer_mut(), self.keyboard_enhanced);
            self.keyboard_enhanced = false;
            let _ = execute!(
                self.surface.writer_mut(),
                DisableBracketedPaste,
                DisableFocusChange,
                Show
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

    /// Replace the transcript/composer state with a fresh [`Screen`] for an
    /// in-process session swap (`/new`, `/resume`). The terminal, raw mode, and
    /// keyboard flags stay as-is; only the in-app conversation view is reset, so
    /// the next draw starts the swapped session with an empty transcript. The
    /// caller re-applies the banner and refreshes the footer afterward.
    /// Whether the kitty keyboard protocol was negotiated at startup (the
    /// `/terminal-setup` doctor reports the live state, never a re-probe).
    pub(crate) fn keyboard_enhanced(&self) -> bool {
        self.keyboard_enhanced
    }

    pub(crate) fn reset_screen(&mut self) {
        let pager_active = self.screen.pager_active;
        self.screen = Screen::new();
        self.screen.pager_active = pager_active;
    }
}

impl Drop for TuiUi {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::panel::{footer_rule_line, inset_rule_line, panel_body_line, panel_header_line};
    use super::*;
    use crate::nexus::{ApprovalDecision, ReviewContext, ToolCall};
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

    fn synthetic_render_perf_screen() -> Screen {
        let mut screen = Screen::new();
        screen.set_footer(
            "openai-codex/gpt-5.5".to_string(),
            Some("xhigh".to_string()),
            "~/iris-agent (main)".to_string(),
        );

        let mut i = 0usize;
        while screen.transcript.rows.len() < MAX_TRANSCRIPT_ROWS.saturating_sub(96) {
            match i % 6 {
                0 => screen.commit_user(&format!(
                    "inspect render hot path batch {i} with enough prose to wrap across several \
                     words and preserve user transcript rhythm"
                )),
                1 => screen.apply(UiEvent::AssistantText(format!(
                    "## Render batch {i}\n\nThe renderer keeps markdown prose, `inline_code`, \
                     links, and lists byte-stable while the terminal surface diffs only the \
                     rows that changed.\n\n- fold visibility remains semantic\n- rules stay muted\n\n---"
                ))),
                2 => {
                    let call = call_args(
                        "bash",
                        json!({ "command": format!("printf 'line %04d\\n' {i}") }),
                    );
                    let content = (0..18)
                        .map(|n| format!("shell output batch {i} row {n}: a moderately long line"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    screen.apply(UiEvent::ToolResult {
                        call,
                        content,
                        exit_code: Some(0),
                        duration: Some(Duration::from_millis((i % 97) as u64)),
                    });
                    let _ = screen.toggle_latest_panel();
                }
                3 => {
                    let call = call_args("edit", json!({ "path": format!("src/file_{i}.rs") }));
                    screen.apply(UiEvent::DiffPreview {
                        call: call.clone(),
                        diff: format!(
                            "--- a/src/file_{i}.rs\n+++ b/src/file_{i}.rs\n@@ -1,3 +1,3 @@\n fn sample() {{\n-old_{i}();\n+new_{i}();\n }}\n"
                        ),
                    });
                    screen.apply(UiEvent::ToolResult {
                        call,
                        content: "applied".to_string(),
                        exit_code: Some(0),
                        duration: Some(Duration::from_millis(3)),
                    });
                }
                4 => screen.apply(UiEvent::AssistantReasoning {
                    text: format!(
                        "Candidate {i}: keep cached row wraps unless a fold toggle, trim, or \
                         panel rewrite invalidates the row range.\n\nSecond paragraph is hidden behind \
                         progressive disclosure for long traces."
                    ),
                    redacted: false,
                }),
                _ => screen.apply(UiEvent::Notice(format!(
                    "synthetic notice {i}: resize replay and append diff remain stable"
                ))),
            }
            i += 1;
        }

        screen.transcript.trim_history();
        screen
    }

    fn render_perf_cycle(
        screen: &mut Screen,
        surface: &mut TerminalSurface<Vec<u8>>,
        size: Size,
    ) -> std::io::Result<RenderKind> {
        let document = render_document_with_hints(screen, size);
        surface
            .render_with_hints(
                size,
                &document.lines,
                document.chrome_tail,
                document.stable_prefix,
            )
            .map(|stats| stats.kind)
    }

    #[test]
    #[ignore = "timer benchmark; run explicitly with --ignored --nocapture"]
    fn render_pipeline_near_retention_cap_benchmark() -> std::io::Result<()> {
        let size = Size::new(120, 40);
        let mut screen = synthetic_render_perf_screen();
        eprintln!(
            "render_perf rows={} width={} height={}",
            screen.transcript.rows.len(),
            size.width,
            size.height
        );

        let mut surface = TerminalSurface::new(Vec::new());
        let full_start = Instant::now();
        let full_kind = render_perf_cycle(&mut screen, &mut surface, size)?;
        let full = full_start.elapsed();

        screen.start_turn();
        let spinner_start = Instant::now();
        for _ in 0..100 {
            let _ = screen.tick();
            render_perf_cycle(&mut screen, &mut surface, size)?;
        }
        let spinner = spinner_start.elapsed();

        screen.apply(UiEvent::Notice(
            "synthetic append after spinner churn".to_string(),
        ));
        let append_start = Instant::now();
        let append_kind = render_perf_cycle(&mut screen, &mut surface, size)?;
        let append = append_start.elapsed();

        eprintln!(
            "render_perf full={full:?} kind={full_kind:?}; spinner_100={spinner:?}; \
             append={append:?} kind={append_kind:?}"
        );
        Ok(())
    }

    #[test]
    #[ignore = "timer benchmark; run explicitly with --ignored --nocapture"]
    fn streaming_render_benchmark() -> std::io::Result<()> {
        let size = Size::new(120, 40);
        let mut screen = Screen::new();
        let mut surface = TerminalSurface::new(Vec::new());
        screen.start_turn();
        let chunk =
            "Streaming markdown with `code`, **emphasis**, and prose that wraps across rows. ";

        // ~50KB streamed in 600 deltas. Each delta renders one frame, followed
        // by three no-delta frames (spinner ticks / typing while streaming), the
        // realistic frame mix during a streamed answer.
        let start = Instant::now();
        for _ in 0..600 {
            screen.apply(UiEvent::AssistantTextDelta(chunk.to_string()));
            render_perf_cycle(&mut screen, &mut surface, size)?;
            for _ in 0..3 {
                let _ = screen.tick();
                render_perf_cycle(&mut screen, &mut surface, size)?;
            }
        }
        let streamed = start.elapsed();
        eprintln!("streaming_perf deltas=600 frames=2400 elapsed={streamed:?}");
        Ok(())
    }

    #[derive(Clone, Debug)]
    enum CachedRenderOp {
        User(usize),
        Assistant(usize),
        Shell(usize),
        Diff(usize),
        Reasoning(usize),
        ToggleLatest,
        Width(u16),
        TrimBurst(usize),
    }

    struct TestRng(u64);

    impl TestRng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn pick(&mut self, count: usize) -> usize {
            (self.next() as usize) % count
        }
    }

    fn apply_cached_render_op(screen: &mut Screen, op: &CachedRenderOp) {
        match *op {
            CachedRenderOp::User(i) => screen.commit_user(&format!(
                "cached render user prompt {i} with enough text to wrap on narrow widths"
            )),
            CachedRenderOp::Assistant(i) => screen.apply(UiEvent::AssistantText(format!(
                "Assistant batch {i}\n\n- preserves markdown wrapping\n- keeps `code` styled\n\n---"
            ))),
            CachedRenderOp::Shell(i) => screen.apply(UiEvent::ToolResult {
                call: call_args("bash", json!({ "command": format!("seq {i}") })),
                content: (0..14)
                    .map(|n| format!("foldable shell output {i}.{n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                exit_code: Some(0),
                duration: Some(Duration::from_millis((i % 13) as u64)),
            }),
            CachedRenderOp::Diff(i) => {
                let call = call_args("edit", json!({ "path": format!("src/cache_{i}.rs") }));
                screen.apply(UiEvent::DiffPreview {
                    call: call.clone(),
                    diff: format!(
                        "--- a/src/cache_{i}.rs\n+++ b/src/cache_{i}.rs\n@@ -1 +1 @@\n-old_{i}\n+new_{i}\n"
                    ),
                });
                screen.apply(UiEvent::ToolResult {
                    call,
                    content: "applied".to_string(),
                    exit_code: Some(0),
                    duration: Some(Duration::from_millis(1)),
                });
            }
            CachedRenderOp::Reasoning(i) => screen.apply(UiEvent::AssistantReasoning {
                text: format!(
                    "Reasoning preview {i}.\n\nHidden paragraph {i} exercises fold visibility."
                ),
                redacted: false,
            }),
            CachedRenderOp::ToggleLatest => {
                let _ = screen.toggle_latest_panel();
            }
            CachedRenderOp::Width(_) => {}
            CachedRenderOp::TrimBurst(i) => {
                for n in 0..(MAX_TRANSCRIPT_ROWS + 24) {
                    screen.transcript.rows.push(TranscriptRow::new(
                        format!("trim burst {i} row {n}"),
                        dim_style(),
                    ));
                }
                screen.transcript.trim_history();
            }
        }
    }

    fn cached_render_signature(
        screen: &mut Screen,
        width: u16,
    ) -> Vec<Vec<(String, Option<Color>, Modifier)>> {
        let lines = screen.wrapped_lines(width);
        line_signature(&lines)
    }

    #[test]
    fn cached_transcript_render_matches_fresh_replay_after_mutations() {
        let mut rng = TestRng(0x5eed_1ced_5eed_1ced);
        let mut ops = Vec::new();
        for i in 0..36 {
            let op = match rng.pick(7) {
                0 => CachedRenderOp::User(i),
                1 => CachedRenderOp::Assistant(i),
                2 => CachedRenderOp::Shell(i),
                3 => CachedRenderOp::Diff(i),
                4 => CachedRenderOp::Reasoning(i),
                5 => CachedRenderOp::ToggleLatest,
                _ => CachedRenderOp::Width([44, 72, 100, 132][rng.pick(4)]),
            };
            ops.push(op);
        }
        ops.push(CachedRenderOp::TrimBurst(99));
        ops.push(CachedRenderOp::Width(88));
        ops.push(CachedRenderOp::ToggleLatest);

        let mut cached = Screen::new();
        let mut applied = Vec::new();
        let mut width = 80u16;
        for (step, op) in ops.into_iter().enumerate() {
            if let CachedRenderOp::Width(next) = &op {
                width = *next;
            }
            apply_cached_render_op(&mut cached, &op);
            applied.push(op.clone());

            let cached_sig = cached_render_signature(&mut cached, width);
            let mut fresh = Screen::new();
            let mut fresh_width = 80u16;
            let mut fresh_sig = Vec::new();
            for prior in &applied {
                if let CachedRenderOp::Width(next) = prior {
                    fresh_width = *next;
                }
                apply_cached_render_op(&mut fresh, prior);
                fresh_sig = cached_render_signature(&mut fresh, fresh_width);
            }
            assert_eq!(fresh_width, width);
            assert_eq!(
                cached_sig, fresh_sig,
                "cached render diverged after step {step}: {op:?}"
            );
        }
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
    fn streaming_render_memo_is_dropped_across_stream_boundaries() {
        // Two consecutive streams of identical byte length at the same width:
        // the second stream must never reuse the first stream's memoized
        // render (the `(len, width)` memo key is only sound because the memo
        // is dropped on every stream start/end transition).
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextDelta("alpha".to_string()));
        let live_a = screen.wrapped_lines(80);
        assert!(live_a.iter().any(|l| line_text(l).contains("alpha")));
        screen.apply(UiEvent::AssistantTextEnd(String::new()));

        screen.apply(UiEvent::AssistantTextDelta("gamma".to_string()));
        let live_b = screen.wrapped_lines(80);
        assert!(
            live_b.iter().any(|l| line_text(l).contains("gamma")),
            "second stream reused a stale memoized render"
        );
        let alpha_rows = live_b
            .iter()
            .filter(|l| line_text(l).contains("alpha"))
            .count();
        assert_eq!(alpha_rows, 1, "committed text duplicated by stale memo");
    }

    #[test]
    fn streaming_render_memo_tracks_growth_and_width() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextDelta("one two three".to_string()));
        // Unchanged stream renders identically across repeated frames (the
        // memo-hit path must be byte-stable with the fresh render).
        let first = screen.wrapped_lines(80);
        let second = screen.wrapped_lines(80);
        assert_eq!(line_signature(&first), line_signature(&second));

        // A delta grows the buffer: the next frame must show the new tail.
        screen.apply(UiEvent::AssistantTextDelta(" four".to_string()));
        let grown = screen.wrapped_lines(80);
        assert!(grown.iter().any(|l| line_text(l).contains("four")));

        // A width change must re-wrap the memoized stream, not reuse it.
        let narrow = screen.wrapped_lines(12);
        assert!(
            narrow.iter().all(|l| display_width(&line_text(l)) <= 12),
            "memoized streaming lines not re-wrapped for the narrower width"
        );
    }

    /// Incremental rendering (wrapped-row cache + fold-visibility cache +
    /// stable-prefix hints + surface prefix reuse) must leave the terminal
    /// surface byte-identical to a fresh full replay of the same screen after
    /// every kind of mutation, including fold toggles that dirty mid-transcript
    /// rows and streaming frames that append transient rows.
    #[test]
    fn incremental_surface_matches_full_replay_after_each_mutation() {
        let size = Size::new(60, 18);
        let mut screen = Screen::new();
        let mut incremental = TerminalSurface::new(Vec::new());

        let mut step = 0usize;
        let mut check = |screen: &mut Screen, incremental: &mut TerminalSurface<Vec<u8>>| {
            step += 1;
            render_perf_cycle(screen, incremental, size).expect("incremental render");
            let (lines, chrome_tail) = render_document_with_chrome_tail(screen, size);
            let mut fresh = TerminalSurface::new(Vec::new());
            fresh
                .render_with_volatile_tail(size, &lines, chrome_tail)
                .expect("fresh render");
            assert_eq!(
                incremental.state().previous_lines,
                fresh.state().previous_lines,
                "incremental surface diverged from full replay at step {step}"
            );
        };

        screen.apply(UiEvent::AssistantText(
            "## Session start\n\nplain prose with `code` and a list\n\n- one\n- two".to_string(),
        ));
        check(&mut screen, &mut incremental);

        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "seq 20" })),
            content: (0..20)
                .map(|n| format!("output row {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(12)),
        });
        check(&mut screen, &mut incremental);

        // Fold toggle dirties rows in the middle of the transcript.
        assert!(screen.toggle_latest_panel());
        check(&mut screen, &mut incremental);
        assert!(screen.toggle_latest_panel());
        check(&mut screen, &mut incremental);

        screen.commit_user("a user prompt that wraps across the narrow test width");
        check(&mut screen, &mut incremental);

        // Streaming frames: transient rows after committed history.
        screen.apply(UiEvent::AssistantTextDelta(
            "streaming **tail** ".to_string(),
        ));
        check(&mut screen, &mut incremental);
        screen.apply(UiEvent::AssistantTextDelta("grows".to_string()));
        check(&mut screen, &mut incremental);
        screen.apply(UiEvent::AssistantTextEnd(String::new()));
        check(&mut screen, &mut incremental);

        screen.apply(UiEvent::AssistantReasoning {
            text: "first paragraph.\n\nhidden second paragraph.".to_string(),
            redacted: false,
        });
        check(&mut screen, &mut incremental);
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
                .any(|line| line.starts_with("    › Second paragraph")),
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
        // Compact by default: expand the finalized block to inspect its body.
        screen.toggle_all_panels();
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
        let command = "echo \u{1b}]0;owned\u{7}safe\t\u{1b}[31mred\u{1b}[0m\rboom";
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
        assert!(rendered.contains("echo safe       redboom"), "{rendered:?}");
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
    fn single_over_budget_line_arrives_collapsed_and_reveals_whole() {
        // A single very long line would wrap to dozens of physical rows; the
        // flood guard folds the block on arrival (header + footer only) and
        // ctrl+o reveals the full, unelided output. Checked at narrow and
        // normal widths.
        for width in [20u16, 80u16] {
            let mut screen = Screen::new();
            let _ = screen.wrapped_lines(width);
            screen.apply(UiEvent::ToolResult {
                call: call_args("bash", json!({ "command": "blob" })),
                content: "x".repeat(2000),
                exit_code: None,
                duration: None,
            });
            assert!(screen.latest_panel_collapsed(), "width {width}");
            let texts: Vec<String> = screen.wrapped_lines(width).iter().map(line_text).collect();
            let output_rows = texts.iter().filter(|t| t.contains('x')).count();
            assert_eq!(
                output_rows, 0,
                "width {width}: collapsed body must be unmounted: {texts:?}"
            );
            assert!(screen.toggle_latest_panel(), "width {width}");
            let texts: Vec<String> = screen.wrapped_lines(width).iter().map(line_text).collect();
            let revealed: usize = texts.iter().map(|t| t.matches('x').count()).sum();
            assert_eq!(revealed, 2000, "width {width}: full output on reveal");
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
        // Compact by default: expand the finalized block to inspect its body.
        screen.toggle_all_panels();
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
    fn over_budget_output_arrives_collapsed_by_physical_rows() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80); // prime last_width
        // 8 logical lines, each ~400 columns => ~6 wrapped rows each => far
        // past the physical-row budget even though the logical count is at the
        // limit: the flood guard folds the block on arrival.
        let long = "x".repeat(400);
        let content = std::iter::repeat_n(long, 8).collect::<Vec<_>>().join("\n");
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "big" })),
            content,
            exit_code: None,
            duration: None,
        });
        assert!(screen.latest_panel_collapsed());
        let lines = screen.wrapped_lines(80);
        let output_rows = lines.iter().filter(|l| line_text(l).contains('x')).count();
        assert_eq!(output_rows, 0, "collapsed body must be unmounted");
    }

    #[test]
    fn over_budget_output_reveals_whole_without_elision() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80); // prime last_width
        // 20 short lines exceed the compact row budget: the block arrives
        // collapsed to exactly header + footer, and ctrl+o reveals EVERY line
        // — binary disclosure, no partial preview, no elision row.
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
        assert!(screen.latest_panel_collapsed());
        let texts: Vec<String> = screen.wrapped_lines(80).iter().map(line_text).collect();
        assert!(
            !texts.iter().any(|t| t.contains("line 0")),
            "collapsed body must be unmounted: {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("lines hidden")),
            "no elision affordance in the frameless design: {texts:?}"
        );
        assert!(screen.toggle_latest_panel());
        let texts: Vec<String> = screen.wrapped_lines(80).iter().map(line_text).collect();
        for i in 0..20 {
            assert!(
                texts.iter().any(|t| t.contains(&format!("line {i}"))),
                "revealed body must carry every line: {texts:?}"
            );
        }
    }

    #[test]
    fn approval_hint_names_tool_target() {
        let mut screen = Screen::new();
        screen.show_approval(
            &call_args("bash", json!({ "command": "echo hi" })),
            false,
            false,
            &ReviewContext::default(),
        );
        let rendered = rendered_text(&mut screen, 80, 14);
        assert!(rendered.contains("REVIEW"), "{rendered}");
        assert!(rendered.contains("bash"), "{rendered}");
        assert!(rendered.contains("$ echo hi"), "{rendered}");
        // The explanatory reason line and the new `┊`-separated decision hints.
        assert!(
            rendered.contains("Runs a shell command in the workspace."),
            "{rendered}"
        );
        assert!(rendered.contains("y approve"), "{rendered}");
        assert!(rendered.contains("n deny"), "{rendered}");
    }

    #[test]
    fn approval_panel_renders_above_composer_and_keeps_editor_visible() {
        // The approval docks in the overlay region ABOVE the composer; the
        // composer body (placeholder) stays visible below it, and the approval
        // is not painted into the editor text rows.
        let mut screen = Screen::new();
        screen.show_approval(
            &call_args("bash", json!({ "command": "echo hi" })),
            false,
            false,
            &ReviewContext::default(),
        );
        let lines = rendered_lines(&mut screen, 80, 16);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let review_row = texts
            .iter()
            .position(|t| t.contains("\u{25b2} REVIEW"))
            .expect("REVIEW header row present");
        let placeholder_row = texts
            .iter()
            .position(|t| t.contains("Give Iris a task..."))
            .expect("composer placeholder still visible");
        assert!(
            review_row < placeholder_row,
            "approval must render above the composer: {texts:?}"
        );
    }

    #[test]
    fn approval_prompt_renders_above_composer_and_wraps() {
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
            false,
            &ReviewContext::default(),
        );
        let lines = rendered_lines(&mut screen, 48, 16);
        assert!(
            lines
                .iter()
                .all(|line| display_width(&line_text(line)) <= 48),
            "{lines:?}"
        );
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("$ printf 'global:"));
        assert!(rendered.contains("120s)"), "{rendered}");
        assert!(rendered.contains("n deny"), "{rendered}");
        assert!(!rendered.contains("\u{21b5} to send"), "{rendered}");
        assert!(
            !rendered.contains("Ask the agent anything..."),
            "{rendered}"
        );
    }

    #[test]
    fn empty_composer_keeps_blank_line_below_placeholder() {
        let mut screen = Screen::new();
        let lines = rendered_lines(&mut screen, 80, 8)
            .into_iter()
            .map(|line| line_text(&line))
            .collect::<Vec<_>>();
        let placeholder = lines
            .iter()
            .position(|line| line.contains("Give Iris a task..."))
            .expect("placeholder line");

        let blank_rows_after_placeholder = lines[placeholder + 1..]
            .iter()
            .take_while(|line| line.trim().is_empty())
            .count();
        // Internal-rule and statusline rows (blank before a footer exists) plus
        // the one intentional soft bottom row.
        assert_eq!(blank_rows_after_placeholder, 3, "{lines:?}");
    }

    #[test]
    fn editor_visual_rows_use_actual_inner_text_width() {
        let mut editor = fresh_editor();
        editor.insert_str("abcdefghijkl");

        assert_eq!(editor_visual_rows(&editor, 18), 2);
    }

    #[test]
    fn editor_visual_rows_cap_at_eight_lines() {
        let mut editor = fresh_editor();
        editor.insert_str("abcdefghijk".repeat(12));

        assert_eq!(editor_visual_rows(&editor, 18), MAX_EDITOR_ROWS);
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
        // The decision is the footer state label; no frame anywhere.
        assert!(rendered.contains("APPROVED"), "{rendered}");
        for frame in ['┌', '┐', '└', '┘', '│'] {
            assert!(!rendered.contains(frame), "{rendered}");
        }

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
    fn task_diff_panel_shows_summary_and_colorized_diff() {
        // Issue #264: the /diff panel renders the per-file summary rows plus the
        // unified diff through the shared diff_table_rows colorizer.
        let mut screen = Screen::new();
        screen.apply(UiEvent::TaskDiff {
            summary: vec![
                "1 file changed, +1/-1".to_string(),
                "  +1/-1  note.txt".to_string(),
            ],
            diff: "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
        });
        let texts: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert!(
            texts.iter().any(|t| t.contains("+1/-1  note.txt")),
            "per-file line"
        );
        assert!(
            !texts.iter().any(|t| t.contains("--- a/note.txt")),
            "header dropped"
        );
        assert!(
            texts.iter().any(|t| t.contains("+  new")),
            "colorized add row"
        );
        let add = screen
            .transcript
            .rows
            .iter()
            .find(|row| row.text.contains("+  new"))
            .expect("addition row");
        assert_eq!(add.style, ok_style());
    }

    #[test]
    fn diff_preview_footer_carries_counts_after_state_label() {
        // EDIT's `+1 −1` counts join the block footer as ONE field after the
        // state label, tinted to the diff inks (unicode minus, never ASCII).
        let mut screen = Screen::new();
        screen.apply(UiEvent::DiffPreview {
            call: call("edit"),
            diff: "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
        });
        let footer = screen
            .transcript
            .rows
            .iter()
            .find(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::Footer { .. })))
            .expect("block footer row");
        assert!(
            footer.text.starts_with("PREVIEW  +1 \u{2212}1"),
            "state label then counts field: {}",
            footer.text
        );
        let Some(ChromeRow::Footer { left, .. }) = footer.chrome.as_ref() else {
            unreachable!();
        };
        assert!(
            left.spans
                .iter()
                .any(|s| s.content.contains("+1") && s.style.fg == ok_style().fg),
            "additions tinted to the add ink: {left:?}"
        );
        assert!(
            left.spans
                .iter()
                .any(|s| s.content.contains("\u{2212}1") && s.style.fg == err_style().fg),
            "removals tinted to the del ink: {left:?}"
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
        // Compact by default: expand the finalized block to inspect its body.
        screen.toggle_all_panels();
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
            "~/workspace/user-auth".to_string(),
        );
        screen.set_footer_git(Some(crate::git::status::GitStatus {
            branch: Some("feat/rate-limit".to_string()),
            ..Default::default()
        }));
        let rendered = rendered_text(&mut screen, 180, 12);

        // Runtime status is the composer's bottom statusline, with the
        // approval-policy segment (symbol + label) after the model.
        assert!(
            rendered.contains("◉ CODE ─ SONNET 3.5 HIGH ─ ▲ on-request"),
            "{rendered}"
        );
        // Workspace state lives on the pane-top session bar.
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

        // The input row sits directly under the composer's top edge.
        assert!(texts[top + 1].contains("Give Iris a task..."), "{texts:?}");
        assert!(
            texts[top + 1].starts_with("      Give Iris a task..."),
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
    fn short_session_composer_chrome_pins_to_viewport_bottom() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextEnd("Short answer.".to_string()));

        let height = 24;
        let lines = rendered_lines(&mut screen, 100, height);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let input_idx = texts
            .iter()
            .position(|line| line.contains("Give Iris a task..."))
            .expect("composer input");

        assert_eq!(lines.len(), usize::from(height), "{texts:?}");
        // Below the input: the internal-rule and statusline rows (blank before
        // a footer exists) and the one intentional soft bottom row.
        assert_eq!(
            input_idx + 4,
            texts.len(),
            "only the composer's bottom chrome should sit below the input: {texts:?}"
        );
        assert_eq!(texts[input_idx + 3].trim(), "", "{texts:?}");
    }

    #[test]
    fn short_transcript_is_top_anchored_with_filler_above_bottom_chrome() {
        // Full-pane takeover: the conversation reads top-down from the first
        // pane row, blank filler sits between the transcript and the composer,
        // and the composer occupies the bottom rows of the pane.
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextEnd("Short answer.".to_string()));

        let height = 24u16;
        let lines = rendered_lines(&mut screen, 100, height);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        assert_eq!(lines.len(), usize::from(height), "{texts:?}");
        assert!(
            texts[0].contains("Short answer."),
            "transcript must start on the first pane row: {texts:?}"
        );
        let hairline = texts
            .iter()
            .position(|line| line.trim().chars().all(|ch| ch == '─') && line.contains('─'))
            .expect("composer hairline");
        // Everything between the transcript and the composer hairline is blank
        // filler (the transcript block ends with its own single blank row).
        assert!(
            texts[1..hairline].iter().all(|line| line.trim().is_empty()),
            "filler between transcript and composer must be blank: {texts:?}"
        );
        assert!(hairline > 2, "filler rows expected on a short session");
    }

    #[test]
    fn empty_launch_document_fills_viewport_with_composer_at_bottom() {
        // At launch (empty transcript) the rendered document already spans the
        // whole pane: filler on top, composer chrome pinned to the bottom rows.
        let mut screen = Screen::new();
        screen.apply(UiEvent::SessionStarted);

        let height = 24u16;
        let lines = rendered_lines(&mut screen, 80, height);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        assert_eq!(lines.len(), usize::from(height), "{texts:?}");
        let input_idx = texts
            .iter()
            .position(|line| line.contains("Give Iris a task..."))
            .expect("composer input");
        assert_eq!(input_idx + 4, texts.len(), "{texts:?}");
        let hairline = texts
            .iter()
            .position(|line| line.trim().chars().all(|ch| ch == '─') && line.contains('─'))
            .expect("composer hairline");
        assert!(
            texts[..hairline].iter().all(|line| line.trim().is_empty()),
            "launch filler above the composer must be blank: {texts:?}"
        );
    }

    #[test]
    fn start_page_shows_centered_launcher_inside_the_shared_chrome() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.5".to_string(),
            Some("xhigh".to_string()),
            "~/demo".to_string(),
        );
        screen.set_footer_git(Some(crate::git::status::GitStatus {
            branch: Some("main".to_string()),
            ..Default::default()
        }));
        screen.apply(UiEvent::SessionStarted);
        screen.show_start_page();

        let height = 24u16;
        let lines = rendered_lines(&mut screen, 80, height);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(lines.len(), usize::from(height), "{texts:?}");

        // Session bar on top with the launch cwd and an empty meter.
        assert!(texts[0].contains("~/demo ┊ git main"), "{texts:?}");
        assert!(
            texts[0].trim_end().ends_with("CTX 0/300k ○○○○○○○○○○"),
            "{texts:?}"
        );
        // The IrisMark LED strip sits above the launcher menu.
        let mark_idx = texts
            .iter()
            .position(|line| line.contains('●') && line.contains('○') && !line.contains("CTX"))
            .expect("IrisMark strip");
        let menu_idx = texts
            .iter()
            .position(|line| line.contains("New session"))
            .expect("launcher menu");
        assert!(mark_idx < menu_idx, "{texts:?}");
        // All four rows, in order, with their key hints and the house idiom:
        // ◉ marker on the selected row, dotted leaders, no hairline dividers.
        assert!(texts[menu_idx].contains("◉ New session"), "{texts:?}");
        assert!(texts[menu_idx].trim_end().ends_with("ctrl-n"), "{texts:?}");
        assert!(texts[menu_idx + 1].contains("Resume session"), "{texts:?}");
        assert!(
            texts[menu_idx + 1].trim_end().ends_with("ctrl-r"),
            "{texts:?}"
        );
        assert!(
            texts[menu_idx + 2].trim_end().ends_with("ctrl-,"),
            "{texts:?}"
        );
        assert!(
            texts[menu_idx + 3].trim_end().ends_with("ctrl-q"),
            "{texts:?}"
        );
        // The composer chrome stays live below the launcher.
        assert!(
            texts
                .iter()
                .skip(menu_idx)
                .any(|line| line.contains("Give Iris a task...")),
            "{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|line| line.contains("◉ CODE ─ GPT-5.5 XHIGH ─ ▲ on-request")),
            "{texts:?}"
        );

        // Entering a session replaces the launcher; the chrome is unchanged.
        screen.start_turn();
        let after: Vec<String> = rendered_lines(&mut screen, 80, height)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            !after.iter().any(|line| line.contains("New session")),
            "{after:?}"
        );
        assert!(after[0].contains("~/demo ┊ git main"), "{after:?}");
    }

    #[test]
    fn transcript_growth_keeps_composer_pinned_without_scrolling_filler() -> std::io::Result<()> {
        // While the padded document fits the pane, transcript growth shrinks
        // the filler in place: the document stays exactly viewport-height and
        // no blank filler row is ever pushed into native scrollback.
        let size = Size::new(60, 16);
        let mut screen = Screen::new();
        let mut surface = TerminalSurface::new(Vec::new());
        render_perf_cycle(&mut screen, &mut surface, size)?;
        assert_eq!(surface.state().previous_lines.len(), 16);

        for i in 0..2 {
            screen.commit_user(&format!("prompt {i}"));
            screen.apply(UiEvent::AssistantText(format!("answer {i}")));
            render_perf_cycle(&mut screen, &mut surface, size)?;
        }

        assert_eq!(
            surface.state().previous_lines.len(),
            16,
            "padded document must hold the viewport height while content fits"
        );
        assert!(
            !surface.state().scrolled,
            "filler shrinkage must not scroll rows into native scrollback"
        );
        let replay = strip_ansi(&surface.state().previous_lines.join("\n"));
        let first = replay.lines().next().unwrap_or_default();
        assert!(
            first.contains("prompt 0"),
            "transcript must stay anchored to the first pane row: {replay:?}"
        );
        Ok(())
    }

    #[test]
    fn composer_statusline_shows_status_with_context_meter() {
        let mut screen = Screen::new();
        screen.set_footer(
            "openai-codex/gpt-5.4-mini".to_string(),
            Some("off".to_string()),
            "~/project".to_string(),
        );
        let lines = rendered_lines(&mut screen, 120, 10);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let status_idx = texts
            .iter()
            .position(|line| line.contains("◉ CODE"))
            .expect("statusline");
        let status = &texts[status_idx];

        // Mode/model/effort + approval policy, uppercase runtime tokens.
        assert!(
            status.contains("◉ CODE ─ GPT-5.4-MINI OFF ─ ▲ on-request"),
            "{status:?}"
        );
        // Location and context moved to the pane-top session bar: cwd on the
        // left, right-aligned `CTX used/cap` readout with the 10-dot meter.
        assert!(texts[0].starts_with("  ~/project"), "{texts:?}");
        assert!(
            texts[0].trim_end().ends_with("CTX 0/300k ○○○○○○○○○○"),
            "{texts:?}"
        );
        assert!(!status.contains("CTX"), "{status:?}");
        assert!(!status.contains("~/project"), "{status:?}");
        // A soft hairline sits under the session bar.
        assert!(
            texts[1].trim().chars().all(|ch| ch == '─') && texts[1].contains('─'),
            "{texts:?}"
        );
        // The statusline is the last content row: input, then the lighter
        // internal rule, then the statusline.
        assert!(
            texts[status_idx - 2].starts_with("      Give Iris a task..."),
            "input should align with transcript text: {texts:?}"
        );
        assert!(
            texts[status_idx - 1].contains('╌'),
            "internal rule above the statusline: {texts:?}"
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

        // Narrow widths drop the policy segment first (effort survives)...
        let status = composer_statusline(&screen, 30)
            .map(|line| line_text(&line))
            .expect("statusline");
        assert!(status.contains("◉ CODE ─ GPT-5.4-MINI OFF"), "{status:?}");
        assert!(!status.contains("on-request"), "{status:?}");
        assert!(display_width(&status) <= 30, "{status:?}");

        // ...then effort, leaving the minimum: mode + model only.
        let minimum = composer_statusline(&screen, 22)
            .map(|line| line_text(&line))
            .expect("statusline");
        assert!(minimum.contains("◉ CODE ─ GPT-5.4-MINI"), "{minimum:?}");
        assert!(!minimum.contains("OFF"), "{minimum:?}");
        assert!(!minimum.contains("on-request"), "{minimum:?}");
        assert!(minimum.matches('◉').count() == 1, "{minimum:?}");
        assert!(display_width(&minimum) <= 22, "{minimum:?}");
    }

    #[test]
    fn composer_chrome_is_pinned_not_scrollback() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            "~/repo".to_string(),
        );
        screen.set_footer_git(Some(crate::git::status::GitStatus {
            branch: Some("feat/pin-rail".to_string()),
            ..Default::default()
        }));
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
        // The bottom statusline sits below the input; the workspace label lives
        // on the session bar at the top of the document.
        assert!(editor_idx < status_idx, "{texts:?}");
        assert!(texts[0].contains("~/repo ┊ git feat/pin-rail"), "{texts:?}");
        assert!(!texts[status_idx].contains("~/repo"), "{texts:?}");
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
        let empty = session_bar(&screen, 110)
            .map(|l| line_text(&l))
            .expect("session bar");
        assert!(empty.contains("CTX 0/300k ○○○○○○○○○○"), "{empty:?}");

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
        let filled = session_bar(&screen, 110)
            .map(|l| line_text(&l))
            .expect("session bar");
        assert!(filled.contains("CTX 90k/300k ●●●○○○○○○○"), "{filled:?}");

        // The meter must NOT drop to empty at the start of the next turn.
        screen.start_turn();
        let during = session_bar(&screen, 110)
            .map(|l| line_text(&l))
            .expect("session bar");
        assert!(during.contains("CTX 90k/300k ●●●○○○○○○○"), "{during:?}");
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
        let before = session_bar(&screen, 110)
            .map(|l| line_text(&l))
            .expect("session bar");
        assert!(before.contains("CTX 150k/300k ●●●●●○○○○○"), "{before:?}");

        // Switching model clears the meter (prior usage no longer maps).
        screen.set_footer("gpt-5.4".to_string(), None, "~/repo".to_string());
        let after = session_bar(&screen, 110)
            .map(|l| line_text(&l))
            .expect("session bar");
        assert!(after.contains("CTX 0/300k ○○○○○○○○○○"), "{after:?}");
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
        let before = session_bar(&screen, 110)
            .map(|l| line_text(&l))
            .expect("session bar");
        assert!(before.contains("CTX 150k/300k ●●●●●○○○○○"), "{before:?}");

        // A refresh with a differently-cased same model id must NOT reset the meter.
        screen.set_footer_with_context(
            "GPT-5.5".to_string(),
            None,
            Some("300k".to_string()),
            "~/repo".to_string(),
        );
        let after = session_bar(&screen, 110)
            .map(|l| line_text(&l))
            .expect("session bar");
        assert!(after.contains("CTX 150k/300k ●●●●●○○○○○"), "{after:?}");
    }

    #[test]
    fn statusline_workspace_truncates_cwd_preserving_project_and_branch() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.5".to_string(),
            None,
            "~/projects/very/deeply/nested/path/iris-agent".to_string(),
        );
        screen.set_footer_git(Some(crate::git::status::GitStatus {
            branch: Some("main".to_string()),
            ..Default::default()
        }));
        let label = session_bar(&screen, 50)
            .map(|line| line_text(&line))
            .expect("session bar");
        assert!(display_width(&label) <= 50, "{label:?}");
        assert!(label.contains("iris-agent"), "{label:?}");
        assert!(label.contains('…'), "{label:?}");
        assert!(label.contains("┊ git main"), "{label:?}");
    }

    #[test]
    fn composer_statusline_never_overflows_at_any_width() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            "~/projects/iris".to_string(),
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
        // blank, hairline, input, internal rule, then the bottom statusline.
        assert_eq!(status_idx, working_idx + 5, "{texts:?}");
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
    fn assistant_paragraphs_keep_marker_before_following_user_message() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextEnd(
            "First assistant paragraph.\n\nSecond assistant paragraph.".to_string(),
        ));
        screen.apply(UiEvent::UserMessage("Next user message.".to_string()));

        let lines = rendered_lines(&mut screen, 100, 24);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        let second_assistant = lines
            .iter()
            .map(line_text)
            .find(|line| line.contains("Second assistant paragraph."))
            .expect("second assistant paragraph");
        assert!(
            second_assistant.trim_start().starts_with("› "),
            "assistant paragraphs need the visible marker before user text: {rendered}"
        );

        let next_user = lines
            .iter()
            .map(line_text)
            .find(|line| line.contains("Next user message."))
            .expect("next user message");
        assert!(
            !next_user.trim_start().starts_with("› "),
            "user message must stay unmarked: {rendered}"
        );
    }

    #[test]
    fn assistant_marker_skips_structural_markdown_block_starts() {
        let markdown = "Intro paragraph.\n\n```rust\nlet answer = 42;\n```\n\n> quoted note\n\n| left | right |\n| --- | --- |\n| one | two |\n\nOutro paragraph.\n\n---";
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextEnd(markdown.to_string()));

        let rendered = rendered_lines(&mut screen, 100, 32)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();

        let structural_needles = ["let answer = 42;", "> quoted note", "┌", "---"];
        for needle in structural_needles {
            let line = rendered
                .iter()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("missing structural line {needle:?}: {rendered:?}"));
            assert!(
                !line.trim_start().starts_with("› "),
                "structural markdown line must stay unmarked: {rendered:?}"
            );
        }

        let outro = rendered
            .iter()
            .find(|line| line.contains("Outro paragraph."))
            .expect("outro paragraph");
        assert!(
            outro.trim_start().starts_with("› "),
            "prose after structural markdown still needs assistant marker: {rendered:?}"
        );
    }

    #[test]
    fn assistant_marker_allows_prose_containing_pipe() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextEnd(
            "First paragraph.\n\nUse foo | bar when choosing a mode.".to_string(),
        ));

        let rendered = rendered_lines(&mut screen, 100, 24)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        let pipe_prose = rendered
            .iter()
            .find(|line| line.contains("Use foo | bar"))
            .expect("pipe prose paragraph");

        assert!(
            pipe_prose.trim_start().starts_with("› "),
            "ordinary prose with a pipe should still get the assistant marker: {rendered:?}"
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
        assert!(running.contains("RUNNING"), "{running}");
        assert!(running.contains("running…"), "{running}");

        screen.apply(UiEvent::ToolResult {
            call,
            content: "Successfully replaced 1 occurrence.".to_string(),
            exit_code: None,
            duration: Some(Duration::from_millis(3)),
        });
        // Compact by default: expand the finalized block to inspect its body.
        screen.toggle_all_panels();
        let done = rendered_text(&mut screen, 100, 12);
        assert!(done.contains("DONE"), "{done}");
        assert!(
            done.contains("Successfully replaced 1 occurrence."),
            "{done}"
        );
        assert!(!done.contains("running…"), "{done}");
    }

    #[test]
    fn shell_header_right_edge_carries_only_the_elapsed_time() {
        // The frameless header has no state cluster: the right edge is the
        // elapsed time alone, and the state lives in the footer.
        let mut transcript = Transcript::default();
        transcript.push_shell_header(
            PanelState::Done,
            Some(Duration::from_secs(1)),
            None,
            "echo hi",
        );
        let elapsed = transcript
            .rows
            .iter()
            .find_map(|row| match row.chrome.as_ref() {
                Some(ChromeRow::Header {
                    title: "SHELL",
                    elapsed,
                    ..
                }) => Some(elapsed.clone()),
                _ => None,
            })
            .expect("shell header elapsed");
        assert_eq!(elapsed, "1.0s");
        let rendered = transcript.rows[1].render(80);
        let text = line_text(&rendered[0]);
        assert!(text.trim_end().ends_with("1.0s"), "{text}");
        for state_glyph in ["◆", "■", "◇", "DONE"] {
            assert!(!text.contains(state_glyph), "{text}");
        }
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

        // Compact by default: expand the finalized block to inspect its body.
        screen.toggle_all_panels();
        let rendered = rendered_text(&mut screen, 100, 16);
        assert!(rendered.contains("DONE"), "{rendered}");
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

        // Compact by default: expand the finalized block to inspect its body.
        screen.toggle_all_panels();
        let rendered = rendered_text(&mut screen, 100, 18);
        assert!(rendered.contains("SHELL"), "{rendered}");
        assert!(rendered.contains("DONE"), "{rendered}");
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
        let end = rows
            .iter()
            .position(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::BlockEnd)))
            .expect("block end");
        assert!(
            header < error && error < end,
            "error must stay inside the block"
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
                .filter(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::BlockStart)))
                .count(),
            1,
            "started explorations should share one block"
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
                .filter(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::FooterRule)))
                .count(),
            1,
            "started explorations should share one footer rule"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::BlockEnd)))
                .count(),
            1,
            "started explorations should share one end marker"
        );
        // The aggregate state lives in the footer, label only.
        let state = rows
            .iter()
            .find_map(|row| match row.chrome.as_ref() {
                Some(ChromeRow::Footer { .. }) => Some(row.text.clone()),
                _ => None,
            })
            .expect("explore footer state");
        assert!(state.contains("ERROR"), "{state:?}");
        assert!(!state.contains("RUNNING"), "{state:?}");
        let body_texts: Vec<&str> = rows
            .iter()
            .filter(|row| {
                matches!(
                    row.chrome.as_ref(),
                    Some(ChromeRow::Body { .. } | ChromeRow::BodyRight { .. })
                )
            })
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
        // Verb column + target, with a right-bound real count per op.
        assert!(
            rendered.contains("Read  src/context/engine.rs"),
            "{rendered}"
        );
        assert!(rendered.contains("3 lines"), "{rendered}");
        assert!(
            rendered.contains("Grep  \"fn emit\" in src/context"),
            "{rendered}"
        );
        assert!(rendered.contains("3 matches · 2 files"), "{rendered}");

        // The right rail: op metas and the header's elapsed share one right
        // edge; the ops hang at the body indent under the TOOL label.
        let lines = screen.wrapped_lines(100);
        let header_line = line_text(line_matching(&lines, |line| {
            line_text(line).contains("EXPLORE")
        }));
        let read_line = line_text(line_matching(&lines, |line| {
            line_text(line).contains("Read  src/context/engine.rs")
        }));
        let grep_line = line_text(line_matching(&lines, |line| {
            line_text(line).contains("Grep  \"fn emit\" in src/context")
        }));
        assert!(read_line.starts_with("     Read"), "{read_line:?}");
        assert!(grep_line.starts_with("     Grep"), "{grep_line:?}");
        let rail = display_width(header_line.trim_end());
        assert_eq!(display_width(read_line.trim_end()), rail, "{read_line:?}");
        assert_eq!(display_width(grep_line.trim_end()), rail, "{grep_line:?}");
    }

    #[test]
    fn shell_block_reproduces_the_frameless_mockup() {
        // DESIGN spec §2, SHELL — success: header (▾ SHELL  <command> … elapsed),
        // hanging body, hairline rule, footer `DONE  EXIT 0 ┊ <meta>` with the
        // right-bound diagnostics cluster. Exact rows, exact rails.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        let call = call_args("bash", json!({ "command": "cargo test -p context" }));
        screen.transcript.set_tool_diag(
            &call.id,
            super::panel::ToolDiag {
                sent: Some("612".to_string()),
                received: Some("1.0k".to_string()),
                cache: Some("17.9k".to_string()),
                ctx: Some("+0.5%".to_string()),
            },
        );
        screen.apply(UiEvent::ToolResult {
            call,
            content: "   Compiling context v0.4.1\ntest result: ok. 142 passed; 0 failed"
                .to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(3200)),
        });
        // Compact by default: expand the finalized block to inspect its body.
        screen.toggle_all_panels();
        let texts: Vec<String> = screen
            .wrapped_lines(90)
            .iter()
            .map(line_text)
            .filter(|t| !t.trim().is_empty())
            .collect();
        // Right rail: header elapsed and footer diagnostics end at one column.
        let rail = 88; // width 90 − 2ch outer padding
        let header = &texts[0];
        assert!(
            header.starts_with("  ▾ SHELL  cargo test -p context"),
            "{header:?}"
        );
        assert!(header.trim_end().ends_with("3.2s"), "{header:?}");
        assert_eq!(display_width(header.trim_end()), rail, "{header:?}");
        // Body hangs at 2ch + 3ch, under the TOOL label.
        assert_eq!(texts[1], "     $ cargo test -p context");
        assert_eq!(texts[2], "        Compiling context v0.4.1");
        assert_eq!(texts[3], "     test result: ok. 142 passed; 0 failed");
        // Hairline rule from the footer indent to the block's right edge.
        assert!(texts[4].starts_with("    ─"), "{:?}", texts[4]);
        assert_eq!(display_width(&texts[4]), rail, "{:?}", texts[4]);
        // Footer: state label (no glyph), EXIT + meta fields, diagnostics
        // right-bound at the shared rail.
        let footer = &texts[5];
        assert!(
            footer.starts_with("    DONE  EXIT 0 ┊ 142 passed · 0 failed"),
            "{footer:?}"
        );
        assert!(
            footer
                .trim_end()
                .ends_with("↑612 ↓1.0k ┊ cache 17.9k ┊ ctx +0.5%"),
            "{footer:?}"
        );
        assert_eq!(display_width(footer.trim_end()), rail, "{footer:?}");
        assert_eq!(texts.len(), 6, "{texts:?}");
    }

    #[test]
    fn narrow_footer_keeps_state_label_and_drops_diagnostics() {
        // The state label and extras always win the footer row; the optional
        // diagnostics cluster is dropped when it does not fit.
        let footer = ChromeRow::Footer {
            left: Line::from(Span::styled("DONE  EXIT 0", ok_style())),
            right: "↑612 ↓1.0k ┊ cache 17.9k ┊ ctx +0.5%".to_string(),
        };
        let text = line_text(&footer.render(30));
        assert!(text.contains("DONE  EXIT 0"), "{text:?}");
        assert!(!text.contains("cache"), "{text:?}");
        let wide = line_text(&footer.render(90));
        assert!(wide.contains("DONE  EXIT 0"), "{wide:?}");
        assert!(wide.trim_end().ends_with("ctx +0.5%"), "{wide:?}");
    }

    #[test]
    fn explore_footer_carries_measured_diagnostics() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        let call = call_args("read", json!({ "path": "src/context/engine.rs" }));
        screen.transcript.set_tool_diag(
            &call.id,
            super::panel::ToolDiag {
                sent: Some("1.4k".to_string()),
                received: Some("38".to_string()),
                cache: Some("16.8k".to_string()),
                ctx: Some("+0.9%".to_string()),
            },
        );
        screen.apply(UiEvent::ToolResult {
            call,
            content: "line\nline\nline".to_string(),
            exit_code: None,
            duration: Some(Duration::from_millis(10)),
        });
        let lines = screen.wrapped_lines(90);
        let footer = line_text(line_matching(&lines, |line| {
            line_text(line).contains("DONE")
        }));
        assert!(
            footer
                .trim_end()
                .ends_with("↑1.4k ↓38 ┊ cache 16.8k ┊ ctx +0.9%"),
            "{footer:?}"
        );
    }

    #[test]
    fn footer_omits_diagnostics_cluster_when_no_diag_was_measured() {
        // Numbers are honest: without a measured diag entry the footer right
        // edge is empty — no fabricated tokens, no dangling `┊`.
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "echo hi" })),
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(100)),
        });
        let lines = screen.wrapped_lines(80);
        let footer = line_text(line_matching(&lines, |line| {
            line_text(line).contains("EXIT 0")
        }));
        assert!(!footer.contains('↑'), "{footer}");
        assert!(footer.trim_end().ends_with("EXIT 0"), "{footer}");
    }

    /// A completed provider turn carrying the given token accounting, with no
    /// prior turn (so no ctx delta). Cache reads default to zero.
    fn turn_usage(input: u64, output: u64, cache_read: u64) -> UiEvent {
        UiEvent::ProviderTurnCompleted {
            turn_id: "turn".to_string(),
            response_id: None,
            usage: Some(ProviderUsage {
                provider: "openai".to_string(),
                model: "gpt-5.5".to_string(),
                input_tokens: input,
                output_tokens: output,
                cache_read_input_tokens: cache_read,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: input + output,
                cache_creation: None,
            }),
        }
    }

    #[test]
    fn shell_footer_carries_measured_turn_diagnostics() {
        // End-to-end: a provider turn reports usage, then proposes a bash tool;
        // the tool's footer carries that turn's cost right-bound. No prior turn
        // and no context cap => no ctx field.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "turn".to_string(),
        });
        // input 19_300 with 17_200 cache reads => 2_100 fresh input processed.
        screen.apply(turn_usage(19_300, 164, 17_200));
        let call = call_args("bash", json!({ "command": "cargo test" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "ok".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(120)),
        });
        let lines = screen.wrapped_lines(90);
        let footer = line_text(line_matching(&lines, |line| {
            line_text(line).contains("EXIT 0")
        }));
        assert!(
            footer.trim_end().ends_with("↑2.1k ↓164 ┊ cache 17.2k"),
            "{footer:?}"
        );
        assert!(!footer.contains("ctx"), "{footer:?}");
    }

    #[test]
    fn shell_footer_sent_excludes_cache_reads() {
        // When the whole prompt is served from cache (input == cache_read),
        // the fresh input processed this turn is zero: render an honest `↑0`.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "turn".to_string(),
        });
        screen.apply(turn_usage(18_200, 90, 18_200));
        let call = call_args("bash", json!({ "command": "cargo test" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "ok".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(120)),
        });
        let lines = screen.wrapped_lines(90);
        let footer = line_text(line_matching(&lines, |line| {
            line_text(line).contains("EXIT 0")
        }));
        assert!(
            footer.trim_end().ends_with("↑0 ↓90 ┊ cache 18.2k"),
            "{footer:?}"
        );
    }

    #[test]
    fn shell_footer_has_no_cluster_without_turn_usage() {
        // A turn that reports no usage measures nothing: the footer stays clean.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "turn".to_string(),
        });
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn".to_string(),
            response_id: None,
            usage: None,
        });
        let call = call_args("bash", json!({ "command": "echo hi" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(10)),
        });
        let lines = screen.wrapped_lines(90);
        let footer = line_text(line_matching(&lines, |line| {
            line_text(line).contains("EXIT 0")
        }));
        assert!(!footer.contains('↑'), "{footer:?}");
        assert!(footer.trim_end().ends_with("EXIT 0"), "{footer:?}");
    }

    #[test]
    fn shell_footer_omits_cache_field_when_cache_read_is_zero() {
        // cache_read = 0 is noise, not signal: no `cache` field, no dangling ┊.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "turn".to_string(),
        });
        screen.apply(turn_usage(800, 40, 0));
        let call = call_args("bash", json!({ "command": "true" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: String::new(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(5)),
        });
        let lines = screen.wrapped_lines(90);
        let footer = line_text(line_matching(&lines, |line| {
            line_text(line).contains("EXIT 0")
        }));
        assert!(footer.trim_end().ends_with("↑800 ↓40"), "{footer:?}");
        assert!(!footer.contains("cache"), "{footer:?}");
        assert!(
            !footer.contains('┊') || footer.contains("EXIT"),
            "{footer:?}"
        );
    }

    #[test]
    fn shell_footer_reports_signed_context_growth_against_cap() {
        // Two sequential turns: the second turn's ctx field is the signed
        // input-token growth as a percentage of the known context cap.
        // 90_000 - 87_000 = 3_000 of a 300k cap => +1.0%.
        let mut screen = Screen::new();
        screen.set_footer_with_context(
            "gpt-5.5".to_string(),
            None,
            Some("300k".to_string()),
            "~/repo".to_string(),
        );
        let _ = screen.wrapped_lines(90);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(turn_usage(87_000, 100, 0));
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t2".to_string(),
        });
        screen.apply(turn_usage(90_000, 120, 0));
        let call = call_args("bash", json!({ "command": "ls" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: String::new(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(5)),
        });
        let lines = screen.wrapped_lines(90);
        let footer = line_text(line_matching(&lines, |line| {
            line_text(line).contains("EXIT 0")
        }));
        assert!(footer.contains("ctx +1.0%"), "{footer:?}");
    }

    #[test]
    fn explore_footer_carries_measured_turn_diagnostics() {
        // The EXPLORE in-place footer rewrite must not drop the diag stamped by
        // the proposing turn.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "turn".to_string(),
        });
        // input 18_200 with 16_800 cache reads => 1_400 fresh input processed.
        screen.apply(turn_usage(18_200, 38, 16_800));
        let call = call_args("read", json!({ "path": "src/context/engine.rs" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "line\nline\nline".to_string(),
            exit_code: None,
            duration: Some(Duration::from_millis(10)),
        });
        let lines = screen.wrapped_lines(90);
        let footer = line_text(line_matching(&lines, |line| {
            line_text(line).contains("DONE")
        }));
        assert!(
            footer.trim_end().ends_with("↑1.4k ↓38 ┊ cache 16.8k"),
            "{footer:?}"
        );
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
        // The exit status is a footer field after the state label: no glyph,
        // `┊` only between sibling fields.
        assert!(
            rendered.contains("DONE  EXIT 0 ┊ 142 passed · 0 failed"),
            "{rendered}"
        );
        assert!(!rendered.contains("◆"), "{rendered}");
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
        // Applied: the same single EDIT block, DONE, diff + footer counts.
        // Compact by default: the applied block collapses, so expand to inspect.
        screen.toggle_all_panels();
        let done = rendered_text(&mut screen, 100, 24);
        assert!(done.contains("DONE"), "{done}");
        assert!(done.contains("self.budget.justify(ctx)?;"), "{done}");
        assert!(done.contains("DONE  +1 −1"), "{done}");
        assert_eq!(done.matches("EDIT").count(), 1, "one EDIT panel: {done}");
        assert!(!done.contains("PREVIEW"), "{done}");
        assert!(
            !done.contains("Successfully replaced"),
            "the diff is the canonical EDIT body: {done}"
        );
    }

    #[test]
    fn footer_state_label_shares_one_column_across_families() {
        // The frameless footer is the state's only home: EXPLORE, SHELL, EDIT,
        // and APPROVAL footers all start the label at the shared body indent,
        // with no state glyph anywhere in the block.
        let mut screen = Screen::new();
        // EXPLORE (grouped explore path).
        screen.apply(UiEvent::ToolResult {
            call: call_args("read", json!({ "path": "src/a.rs" })),
            content: "ignored".to_string(),
            exit_code: None,
            duration: Some(Duration::from_millis(0)),
        });
        // SHELL (generic panel path).
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "echo hi" })),
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(0)),
        });
        // EDIT (preview path).
        screen.apply(UiEvent::DiffPreview {
            call: call_args("edit", json!({ "file_path": "src/b.rs" })),
            diff: "--- a/src/b.rs\n+++ b/src/b.rs\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
        });
        // APPROVAL (approval panel path).
        screen.apply(UiEvent::ToolAutoApproved(call_args(
            "bash",
            json!({ "command": "ls" }),
        )));

        let lines = screen.wrapped_lines(99);
        let labels = ["DONE", "PREVIEW", "APPROVED"];
        let mut columns = Vec::new();
        for line in lines.iter() {
            let text = line_text(line);
            for glyph in ['◆', '■', '◇', '▲'] {
                assert!(!text.contains(glyph), "no state glyphs anywhere: {text:?}");
            }
            if let Some(label) = labels
                .iter()
                .find(|label| text.trim_start().starts_with(**label))
            {
                let idx = text.find(label).expect("label index");
                columns.push(display_width(&text[..idx]));
            }
        }
        assert!(columns.len() >= 4, "expected a footer per family");
        assert!(
            columns.iter().all(|col| *col == columns[0]),
            "footer state labels share one column: {columns:?}"
        );
    }

    #[test]
    fn edit_preview_with_empty_diff_renders_placeholder_row() {
        // BUG 2 regression: an EDIT preview whose diff is empty (e.g. the
        // old_string did not match) rendered an empty frame with nothing to
        // review. It must show one honest dim placeholder body row instead.
        let mut screen = Screen::new();
        screen.apply(UiEvent::DiffPreview {
            call: call_args("edit", json!({ "file_path": "src/main.rs" })),
            diff: String::new(),
        });
        let rendered = rendered_text(&mut screen, 100, 12);
        assert!(rendered.contains("PREVIEW"), "{rendered}");
        assert!(rendered.contains("no preview available"), "{rendered}");
    }

    #[test]
    fn edit_preview_with_unavailable_message_renders_its_own_text() {
        // Regression: a non-empty preview that does not parse into diff rows
        // (e.g. "diff unavailable: preview too large") carries actionable text
        // and must be rendered verbatim, not replaced by the generic
        // "no preview available" placeholder.
        let mut screen = Screen::new();
        screen.apply(UiEvent::DiffPreview {
            call: call_args("edit", json!({ "file_path": "src/main.rs" })),
            diff: "diff unavailable: preview too large".to_string(),
        });
        let rendered = rendered_text(&mut screen, 100, 12);
        assert!(rendered.contains("PREVIEW"), "{rendered}");
        assert!(
            rendered.contains("diff unavailable: preview too large"),
            "{rendered}"
        );
        assert!(!rendered.contains("no preview available"), "{rendered}");
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
            Some(ChromeRow::BlockEnd)
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
        // Compact by default: expand the finalized SHELL block to inspect it
        // (the EDIT preview already arrives expanded).
        screen.toggle_all_panels();
        let rendered = rendered_text(&mut screen, 110, 24);

        assert!(rendered.contains("SHELL"));
        assert!(rendered.contains("DONE"));
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

        // No effort token between the model and the policy separator.
        assert!(
            rendered.contains("◉ CODE ─ GPT-5.5 ─ ▲ on-request"),
            "{rendered}"
        );
        // No branch: a bare cwd label with no git suffix on the session bar.
        assert!(rendered.contains("~/repo"), "{rendered}");
        assert!(rendered.contains("CTX 0/300k"), "{rendered}");
        assert!(!rendered.contains("┊ git"), "{rendered}");
    }

    #[test]
    fn sourced_top_border_renders_effort_after_model() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            "~/repo".to_string(),
        );
        screen.set_footer_git(Some(crate::git::status::GitStatus {
            branch: Some("branch".to_string()),
            ..Default::default()
        }));
        let rendered = rendered_text(&mut screen, 100, 10);

        assert!(
            rendered.contains("◉ CODE ─ GPT-5.5 HIGH ─ ▲ on-request"),
            "{rendered}"
        );
        assert!(rendered.contains("~/repo ┊ git branch"), "{rendered}");
        assert!(rendered.contains("CTX 0/300k"), "{rendered}");
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

        // Stream enough lines that the live tail drops earlier output.
        let chunk = std::iter::once("FIRSTLINE".to_string())
            .chain((2..=20).map(|n| format!("line {n}")))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk,
        });

        // The live cell shows the bounded tail with an honest earlier-lines
        // marker; the earliest line is out of the tail. The running block
        // stays expanded.
        let live = transcript_text(&mut screen, 80);
        assert!(live.contains("▾"), "{live}");
        assert!(live.contains("earlier lines hidden"), "{live}");
        assert!(!live.contains("FIRSTLINE"), "{live}");

        // Collapse by hand; the fold must survive a later delta.
        assert!(screen.toggle_latest_panel());
        let collapsed = transcript_text(&mut screen, 80);
        assert!(collapsed.contains("▸"), "{collapsed}");
        assert!(!collapsed.contains("$ seq"), "{collapsed}");

        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "line 21\n".to_string(),
        });
        let after_delta = transcript_text(&mut screen, 80);
        assert!(after_delta.contains("▸"), "{after_delta}");

        // The finalized result replaces the live cell; the explicit collapse
        // is preserved, and expanding reveals the FULL output (the live tail's
        // dropped head is back).
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
        assert!(after_result.contains("▸"), "{after_result}");
        assert!(!after_result.contains("FIRSTLINE"), "{after_result}");
        assert!(screen.toggle_latest_panel());
        let revealed = transcript_text(&mut screen, 80);
        assert!(revealed.contains("FIRSTLINE"), "{revealed}");
        assert!(revealed.contains("line 21"), "{revealed}");
    }

    #[test]
    fn ctrl_o_reveals_and_recollapses_over_budget_tool_output() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80); // prime last_width
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

        // Over-budget: arrives collapsed to header + footer, body unmounted.
        let preview = transcript_text(&mut screen, 80);
        assert!(preview.contains("▸"), "{preview}");
        assert!(!preview.contains("MIDDLELINE"), "{preview}");
        assert!(!preview.contains("ctrl+o"), "{preview}");

        // Expand reveals the whole body — including the middle.
        assert!(screen.toggle_latest_panel());
        let revealed = transcript_text(&mut screen, 80);
        assert!(revealed.contains("▾"), "{revealed}");
        assert!(revealed.contains("MIDDLELINE"), "{revealed}");

        // Collapse again unmounts the body.
        assert!(screen.toggle_latest_panel());
        let recollapsed = transcript_text(&mut screen, 80);
        assert!(recollapsed.contains("▸"), "{recollapsed}");
        assert!(!recollapsed.contains("MIDDLELINE"), "{recollapsed}");
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
                        ChromeRow::BlockStart
                            | ChromeRow::BlockEnd
                            | ChromeRow::FooterRule
                            | ChromeRow::Footer { .. }
                            | ChromeRow::Header { .. }
                            | ChromeRow::Body { .. }
                    )
                ),
                "reasoning must not use tool-block chrome: {:?}",
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
    fn collapsed_block_is_exactly_header_and_footer() {
        // Over-budget output folds on arrival: no body, no elision affordance,
        // no `ctrl+o` chrome — the collapsed block is header + rule + footer
        // and still answers what ran · outcome · cost.
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
        let texts: Vec<String> = screen
            .wrapped_lines(80)
            .iter()
            .map(line_text)
            .filter(|t| !t.trim().is_empty())
            .collect();
        assert_eq!(texts.len(), 3, "header + rule + footer: {texts:?}");
        assert!(
            texts[0].contains("▸") && texts[0].contains("SHELL"),
            "{texts:?}"
        );
        assert!(texts[1].trim().chars().all(|c| c == '─'), "{texts:?}");
        assert!(texts[2].contains("DONE"), "{texts:?}");
        assert!(
            !texts
                .iter()
                .any(|t| t.contains("ctrl+o") || t.contains("hidden")),
            "{texts:?}"
        );
    }

    #[test]
    fn small_finalized_block_arrives_collapsed() {
        // Compact by default: even a body that would fit uncapped arrives
        // collapsed once the block is finalized.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "echo hi" })),
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: None,
        });
        assert!(screen.latest_panel_collapsed());
    }

    #[test]
    fn running_block_is_expanded_and_collapses_on_finalize() {
        // A running block stays expanded so its live tail is watchable, then
        // collapses on finalize (no explicit user expand).
        let mut screen = Screen::new();
        screen.start_turn();
        let _ = screen.wrapped_lines(80);
        let call = call_args("bash", json!({ "command": "echo hi" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "hi\n".to_string(),
        });
        assert!(!screen.latest_panel_collapsed(), "running stays expanded");
        screen.apply(UiEvent::ToolResult {
            call,
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: None,
        });
        assert!(screen.latest_panel_collapsed(), "finalize collapses");
    }

    #[test]
    fn user_expanded_running_block_stays_expanded_after_finalize() {
        // An explicit user expand on a running block is recorded as intent and
        // survives the in-place finalize rebuild (even though it is a no-op on
        // the row, since running already arrives expanded).
        let mut screen = Screen::new();
        screen.start_turn();
        let _ = screen.wrapped_lines(80);
        let call = call_args("bash", json!({ "command": "echo hi" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "hi\n".to_string(),
        });
        let header = *screen
            .transcript
            .panel_header_rows()
            .last()
            .expect("running header");
        screen.transcript.set_panel_expanded_at(header, true);
        screen.apply(UiEvent::ToolResult {
            call,
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: None,
        });
        assert!(
            !screen.latest_panel_collapsed(),
            "user expand survives finalize"
        );
    }

    #[test]
    fn edit_preview_arrives_expanded_and_collapses_when_applied() {
        // EXCEPTION to compact-by-default: a pending EDIT preview arrives
        // expanded for review, then collapses once the edit is applied.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(100);
        let call = call_args("edit", json!({ "file_path": "src/main.rs" }));
        let diff = "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n";
        screen.apply(UiEvent::DiffPreview {
            call: call.clone(),
            diff: diff.to_string(),
        });
        assert!(
            !screen.latest_panel_collapsed(),
            "pending preview arrives expanded"
        );
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "Successfully replaced 1 occurrence.".to_string(),
            exit_code: None,
            duration: None,
        });
        assert!(
            screen.latest_panel_collapsed(),
            "applied edit block collapses"
        );
    }

    #[test]
    fn ctrl_o_toggle_all_expands_then_collapses() {
        // With a mix of collapsed and expanded foldable blocks (tool blocks +
        // a thinking rail), toggle-all expands ALL when any is collapsed, then
        // collapses ALL on the next press.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "one" })),
            content: "first body".to_string(),
            exit_code: Some(0),
            duration: None,
        });
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "two" })),
            content: "second body".to_string(),
            exit_code: Some(0),
            duration: None,
        });
        screen.apply(UiEvent::AssistantReasoning {
            text: "First thought.\n\nSecond thought.\n\nThird thought.".to_string(),
            redacted: false,
        });
        let headers = screen.transcript.panel_header_rows();
        assert_eq!(headers.len(), 3, "three foldable blocks arrive collapsed");
        assert!(
            headers
                .iter()
                .all(|&h| screen.transcript.panel_expanded_at(h) == Some(false)),
            "all arrive collapsed"
        );
        // Mixed state: expand one so not all are collapsed.
        screen.transcript.set_panel_expanded_at(headers[0], true);

        // First toggle-all: any collapsed -> expand all (rail included).
        assert!(screen.toggle_all_panels());
        assert!(
            screen
                .transcript
                .panel_header_rows()
                .iter()
                .all(|&h| screen.transcript.panel_expanded_at(h) == Some(true)),
            "first press expands all"
        );
        // Second toggle-all: none collapsed -> collapse all.
        assert!(screen.toggle_all_panels());
        assert!(
            screen
                .transcript
                .panel_header_rows()
                .iter()
                .all(|&h| screen.transcript.panel_expanded_at(h) == Some(false)),
            "second press collapses all"
        );
    }

    #[test]
    fn header_click_toggles_only_the_clicked_block() {
        // A left-button-down on a block's header row toggles THAT block; a
        // click on a body row is a no-op. The whole header row is the target.
        use super::pager::compose_frame;
        let mut screen = Screen::new();
        screen.pager_active = true;
        screen.mouse_capture = true;
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "echo hi" })),
            content: "body line one\nbody line two".to_string(),
            exit_code: Some(0),
            duration: None,
        });
        let size = Size::new(80, 30);
        // Collapsed on arrival: click the header row to expand.
        let frame = compose_frame(&mut screen, size);
        let header_row = frame
            .lines
            .iter()
            .position(|line| line_text(line).contains("SHELL"))
            .expect("header rendered");
        assert!(screen.latest_panel_collapsed());
        assert!(
            screen.toggle_header_at_screen_row(header_row as u16),
            "header click toggles the block"
        );
        assert!(
            !screen.latest_panel_collapsed(),
            "header click expanded the block"
        );
        // Body is now visible: a click on a body row does nothing.
        let frame = compose_frame(&mut screen, size);
        let body_row = frame
            .lines
            .iter()
            .position(|line| line_text(line).contains("body line one"))
            .expect("body rendered");
        assert!(
            !screen.toggle_header_at_screen_row(body_row as u16),
            "body click is a no-op"
        );
        assert!(
            !screen.latest_panel_collapsed(),
            "body click left the block expanded"
        );
    }

    #[test]
    fn collapsed_shell_block_footer_still_reports_exit_status() {
        // Collapsing hides the body only: the footer (state · EXIT · meta)
        // stays visible, so the outcome is never hidden.
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
        assert!(screen.latest_panel_collapsed());
        let lines = screen.wrapped_lines(80);
        let footer = line_text(line_matching(&lines, |line| {
            line_text(line).contains("EXIT 0")
        }));
        assert!(footer.contains("DONE  EXIT 0"), "{footer}");
        assert!(
            !lines.iter().any(|line| line_text(line).contains("line 5")),
            "collapsed body must be unmounted: {:?}",
            lines.iter().map(line_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn tiny_block_rows_are_width_safe() {
        for width in 1..=5 {
            let rows = vec![
                panel_header_line(width, true, "SHELL", "bash", "1.0s"),
                footer_rule_line(width),
                panel_body_line(
                    width,
                    Line::from(Span::styled("body".to_string(), panel_style())),
                    None,
                ),
            ];
            for row in rows {
                let text = line_text(&row);
                assert!(display_width(&text) <= width, "width {width}: {row:?}");
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
                        | ChromeRow::Body { .. }
                        | ChromeRow::BodyRight { .. }
                        | ChromeRow::BodyRule { .. }
                        | ChromeRow::FooterRule
                        | ChromeRow::Footer { .. }
                        | ChromeRow::BlockEnd
                )
            ),
            "trim left an orphan block row at the start"
        );
        let mut in_panel = false;
        for row in &transcript.rows {
            match row.chrome.as_ref() {
                Some(ChromeRow::BlockStart) => {
                    assert!(!in_panel, "nested block start");
                    in_panel = true;
                }
                Some(
                    ChromeRow::Header { .. }
                    | ChromeRow::Body { .. }
                    | ChromeRow::BodyRight { .. }
                    | ChromeRow::BodyRule { .. }
                    | ChromeRow::FooterRule
                    | ChromeRow::Footer { .. },
                ) => assert!(in_panel, "orphan block interior: {:?}", row.text),
                Some(ChromeRow::BlockEnd) => {
                    assert!(in_panel, "orphan block end");
                    in_panel = false;
                }
                // The reasoning rail is chromeless (not a box panel): its header
                // and end markers never open/close `in_panel`, and its trace rows
                // are plain rows outside any box.
                Some(ChromeRow::RailHeader { .. } | ChromeRow::RailEnd) => {}
                Some(ChromeRow::Notice { .. }) => {
                    assert!(!in_panel, "notice row inside panel: {:?}", row.text);
                }
                None => assert!(!in_panel, "plain row inside panel: {:?}", row.text),
            }
        }
        assert!(!in_panel, "trim left an unterminated panel");
    }

    #[test]
    fn trim_history_keeps_thinking_header_telemetry_index_aligned() {
        let mut transcript = Transcript::default();
        for i in 0..MAX_TRANSCRIPT_ROWS {
            transcript
                .rows
                .push(TranscriptRow::new(format!("old {i}"), panel_style()));
        }

        transcript.apply(UiEvent::ProviderTurnStarted {
            turn_id: "turn_1".to_string(),
        });
        transcript.apply(UiEvent::AssistantReasoning {
            text: "first paragraph\n\nsecond paragraph".to_string(),
            redacted: false,
        });
        transcript.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: None,
            usage: Some(ProviderUsage {
                provider: "openai".to_string(),
                model: "gpt-5.5".to_string(),
                input_tokens: 1,
                output_tokens: 1,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 1_234,
                total_tokens: 2,
                cache_creation: None,
            }),
        });

        let headers: Vec<&String> = transcript
            .rows
            .iter()
            .filter_map(|row| match row.chrome.as_ref() {
                Some(ChromeRow::RailHeader { right, .. }) => Some(right),
                _ => None,
            })
            .collect();
        assert_eq!(headers.len(), 1, "expected one thinking header");
        assert!(headers[0].contains("↓1.2k"), "{:?}", headers[0]);
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

        assert!(rendered.contains("EXPLORE"));
        assert!(!rendered.contains("READ"), "{rendered}");
        assert!(!rendered.contains("GREP"), "{rendered}");
        assert!(rendered.contains("src/tool_display.rs"));
        assert!(rendered.contains("Read  src/tool_display.rs"));
        assert!(rendered.contains("Grep  \"DiffPreview\" in src/ui"));
        assert!(rendered.contains("src/ui"));
        // Frameless: a hairline footer rule, no box-drawing frame.
        assert!(rendered.contains("─"), "{rendered}");
        for frame in ['┌', '┐', '└', '┘', '│'] {
            assert!(!rendered.contains(frame), "{rendered}");
        }
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
        // Compact by default: expand the finalized block to inspect its body.
        screen.toggle_all_panels();
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
                    ctx_label: None,
                },
                CatalogModel {
                    provider: ProviderId::Anthropic,
                    id: "claude-sonnet-4-6".to_string(),
                    ctx_label: None,
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

        for width in [10u16, 16, 24, 40] {
            for height in [2u16, 3, 4] {
                let mut screen = Screen::new();
                screen.open_modal(Modal::Model(ModelPicker::new(
                    vec![CatalogModel {
                        provider: ProviderId::OpenAiCodex,
                        id: "gpt-5.5".to_string(),
                        ctx_label: None,
                    }],
                    "openai-codex/gpt-5.5",
                    "openai-codex/gpt-5.5",
                    crate::mimir::selection::ReasoningEffort::Medium,
                )));
                let _ = rendered_lines(&mut screen, width, height);
            }
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
                ctx_label: None,
            },
            CatalogModel {
                provider: ProviderId::Anthropic,
                id: "claude-sonnet-4-6".to_string(),
                ctx_label: None,
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
    fn open_modal_reclaims_composer_bottom_padding() {
        use crate::mimir::model_catalog;
        use crate::ui::modal::{Modal, ModelPicker};

        let mut screen = Screen::new();
        screen.open_modal(Modal::Model(ModelPicker::new(
            model_catalog::all(),
            "anthropic/claude-opus-4-8",
            "anthropic/claude-opus-4-8",
            crate::mimir::selection::ReasoningEffort::XHigh,
        )));

        let lines = rendered_lines(&mut screen, 80, 17)
            .into_iter()
            .map(|line| line_text(&line))
            .collect::<Vec<_>>();
        let placeholder = lines
            .iter()
            .position(|line| line.contains("Give Iris a task..."))
            .expect("placeholder line");
        let blank_rows_after_placeholder = lines[placeholder + 1..]
            .iter()
            .take_while(|line| line.trim().is_empty())
            .count();

        // The internal-rule and statusline rows are blank before a footer
        // exists; the soft bottom padding row itself is reclaimed by the modal.
        assert_eq!(blank_rows_after_placeholder, 2, "{lines:?}");
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
            "~/repo".to_string(),
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
            "~/repo".to_string(),
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
            "~/repo".to_string(),
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

        // Compact by default: expand the finalized block to inspect its body.
        screen.toggle_all_panels();
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

        // Compact by default: expand the finalized block to inspect its body.
        screen.toggle_all_panels();
        let rendered = rendered_text(&mut screen, 80, 12);
        // Body hangs at the block indent (2ch pane indent + 3ch hang), no frame.
        assert!(rendered.contains("     $ cargo test"), "{rendered}");
        assert!(rendered.contains("     test result"), "{rendered}");
        // The exit status closes the block as a footer field, glyph-free.
        assert!(
            rendered.contains("DONE  EXIT 0 ┊ 142 passed · 0 failed"),
            "{rendered}"
        );
        assert!(!rendered.contains("\u{25c6}"), "{rendered}");
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

        // Compact by default: expand the finalized block to inspect its body.
        screen.toggle_all_panels();
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
        // Compact by default: expand the finalized block to inspect its body.
        screen.toggle_all_panels();
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
        // Compact by default: expand the finalized block to inspect its body.
        screen.toggle_all_panels();
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
