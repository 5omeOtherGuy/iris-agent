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
use std::time::{Duration, Instant};

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
use ratatui::style::Style;
#[cfg(test)]
use ratatui::text::{Line, Span};

#[cfg(test)]
use crate::nexus::ProviderUsage;
use crate::ui::screen_mode::ScreenMode;
use crate::ui::terminal_surface::TerminalSurface;
use frame_stats::FrameStats;
use pager::PagerSurface;

mod activity;
mod component;
mod frame_stats;
mod overlay;
mod pager;
mod pane;
mod panel;
mod rows;
mod screen;
mod session_menu;
mod shell_command;
mod startup;
mod streaming;
mod text;
mod tool_render;
mod transcript;
mod wrap;

pub(crate) use component::Component;
pub(crate) use overlay::{FocusTarget, overlay_menu};
#[cfg(test)]
use panel::PanelState;
#[cfg(test)]
use rows::{ChromeRow, TranscriptRow, hrule_line};
pub(crate) use screen::compact_count;
use screen::render_document_with_hints;
pub(crate) use screen::{
    ApprovalPolicy, ContextAccounting, Screen, SessionMeter, SwitchCacheStatus, SwitchStatus,
};
pub(crate) use screen::{BarSegment, session_bar_hit};
#[cfg(test)]
use screen::{
    composer_statusline, editor_visual_rows, fresh_editor, render_document,
    render_document_with_chrome_tail, session_bar, working_indicator_line,
};
pub(crate) use session_menu::{
    GitMenu, JjMenu, MenuAction, MenuKey, MenuOutcome, SessionMenu, TreeMenu,
};
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

/// Flood guard: cap a tool result at this many physical (wrapped) rows in the
/// transcript so a few very long lines cannot flood the viewport/scrollback.
/// Tuned to Codex's compact exec cell: a finalized result keeps a head and a
/// tail slice with a `… +N lines` marker between (see [`Transcript::push_tool_output`]).
/// The model still receives the full output; only the terminal preview is
/// bounded, and the omitted logical-line count is reported. This is now the
/// FLOOR of the viewport-aware preview budget ([`preview_row_budget`]); a pane
/// ≤ 40 rows still previews exactly this many rows, so nothing regresses on
/// small terminals.
const MAX_TOOL_OUTPUT_ROWS: usize = 8;
/// Ceiling of the viewport-aware preview budget: a tool-output preview never
/// claims more than this many physical rows no matter how tall the pane is.
const PREVIEW_ROWS_CEILING: usize = 24;

/// Viewport-aware tool-output preview budget: `clamp(pane_height / 5, 8, 24)`
/// rows (reactive-density spec §2). A print-time decision — `pane_height` is the
/// last-known terminal height at the moment the block's rows are built, and the
/// result is immutable in scrollback (a block keeps the budget it was printed
/// at; see docs/TUI_DESIGN_LANGUAGE.md §8.1). Divisor rationale: at height/5 the
/// preview claims at most a fifth of the viewport, so a tool block never
/// dominates the pane — the conversation keeps the floor. The floor is the
/// historical fixed [`MAX_TOOL_OUTPUT_ROWS`] (8), so a pane ≤ 40 rows is
/// byte-identical to before; taller panes let the preview breathe to the
/// ceiling. `pane_height == 0` (before the first frame) yields the floor.
fn preview_row_budget(pane_height: usize) -> usize {
    (pane_height / 5).clamp(MAX_TOOL_OUTPUT_ROWS, PREVIEW_ROWS_CEILING)
}
/// Frameless body hang: the block body hangs one 2-cell step under the header
/// label — the same step the reasoning rail's `┊` body and a user turn's `›`
/// body take, so every block's body lands on ONE shared text column. (Was a
/// `2.5ch`→3 snap that left the tool body one cell shy of that column.)
const PANEL_BODY_INDENT: usize = 4;
const PANEL_BODY_CHROME_WIDTH: usize = PANEL_BODY_INDENT;
/// Footer hang: the hairline rule and state-label row sit one cell left of the
/// body (the spec's `2.5ch` hang rounded DOWN), while their right edge stays on
/// the block's right rail.
const PANEL_FOOTER_INDENT: usize = 2;

// Color roles live in `crate::ui::palette` (the single source of truth). The
// themed accessors are imported here so the whole `tui` module tree resolves
// them as `border()`, `orange()`, … (and its child modules as
// `super::border()`), reading the active theme at render time (ADR-0042).
use crate::ui::palette::{border, diff_add_bg, diff_del_bg, green, muted, orange, red, stdout};

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
    Style::default().fg(green())
}
fn err_style() -> Style {
    Style::default().fg(red())
}
/// The recessive-text role (`muted`): metadata, hints, markers, elisions, and
/// the `┊`/`─` separators. A real themed color (see `palette::muted`) rather
/// than the `Modifier::DIM` attribute it replaced — so muted survives DIM-blind
/// terminals and adopts each named theme's own grey (§2.1). Name retained to
/// keep the change a single body edit across its ~140 call sites.
fn dim_style() -> Style {
    Style::default().fg(muted())
}
/// SHELL program output (`stdout` role): lighter than `muted`, recessive to the
/// bright command line above it.
fn stdout_style() -> Style {
    Style::default().fg(stdout())
}
fn prompt_style() -> Style {
    Style::default().fg(orange())
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
/// The divider's telemetry label: task wall time, summed task token flows,
/// and — when generation time was measured — the task's mean output rate
/// over provider time (never inflated by tool execution between rounds).
fn turn_divider_label(
    elapsed: Option<Duration>,
    flows: &crate::metrics::TokenFlows,
    timing: &crate::metrics::TimingStats,
) -> String {
    let Some(elapsed) = elapsed else {
        return String::new();
    };
    let elapsed = format_elapsed_compact(elapsed);
    let sep = crate::ui::symbols::SEP;
    if flows.is_empty() {
        return elapsed;
    }
    let mut label = format!(
        "{elapsed} {sep} ↑{} ↓{}",
        compact_count(flows.input_tokens),
        compact_count(flows.output_tokens)
    );
    if let Some(rate) = crate::metrics::tokens_per_second(flows.output_tokens, timing.generation)
        && flows.output_tokens > 0
    {
        label.push_str(&format!(" {sep} {} tok/s", rate.round() as u64));
    }
    label
}

#[cfg(test)]
fn turn_divider_line(
    elapsed: Option<Duration>,
    flows: &crate::metrics::TokenFlows,
    timing: &crate::metrics::TimingStats,
    width: usize,
) -> Line<'static> {
    hrule_line(&turn_divider_label(elapsed, flows, timing), width)
}

fn border_style() -> Style {
    Style::default().fg(border())
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
    /// Per-frame compose/flush timings, surfaced through `/debug`. Recording is
    /// O(1) per frame; the ring is bounded, so this is always-on (no flag).
    frame_stats: FrameStats,
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
        // Resolve palette capability before any widget is built. Named themes
        // stay truecolor where supported, quantize to xterm indices at 256,
        // and fall back to semantic ANSI roles at 16 colors.
        crate::ui::palette::configure_terminal_color_depth();
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
        // Focus reporting is tracked so duplicate focus changes can be ignored
        // and a regained pane can redraw once. It must not pause rendering:
        // inactive tmux panes can remain visible beside the active pane.
        if let Err(error) = execute!(stdout, EnableBracketedPaste, EnableFocusChange, Hide) {
            let _ = execute!(stdout, DisableBracketedPaste, DisableFocusChange, Show);
            let _ = disable_raw_mode();
            crate::signals::disable_terminal_restore_on_force_quit();
            return Err(error.into());
        }
        // Best-effort: a failure to negotiate the protocol must not abort startup.
        let keyboard_enhanced =
            enable_keyboard_enhancement(&mut stdout, supports_enhancement).unwrap_or(false);
        // One-time ZWJ-shaping probe (issue #351): measure a family emoji on a
        // scratch line while still on the normal screen -- after raw mode, before
        // the first frame and before any alt-screen entry -- so the glyph never
        // leaks into scrollback (inline) or the pager frame. Rich-TTY path only:
        // `TuiUi::new` is unreachable on the --plain/non-TTY path. The verdict is
        // recorded in `textengine` for transcript substitution and the doctor.
        crate::ui::zwj_probe::run_startup_probe();
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
            frame_stats: FrameStats::new(),
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
            // Split compose (frame build) from flush (terminal write): the
            // closure builds the frame, `render_with` writes it under `?2026`
            // with ratatui diffing, so the remainder of the call is flush.
            let mut compose = Duration::ZERO;
            let frame_start = Instant::now();
            pager.render_with(|frame_size| {
                let started = Instant::now();
                let frame = pager::compose_frame(screen, frame_size);
                compose = started.elapsed();
                frame
            })?;
            let flush = frame_start.elapsed().saturating_sub(compose);
            self.frame_stats.record(compose, flush);
            return Ok(());
        }
        let compose_start = Instant::now();
        let document = render_document_with_hints(&mut self.screen, size);
        let compose = compose_start.elapsed();
        let flush_start = Instant::now();
        self.surface.render_with_hints(
            size,
            &document.lines,
            document.chrome_tail,
            document.stable_prefix,
        )?;
        self.frame_stats.record(compose, flush_start.elapsed());
        Ok(())
    }

    /// Frame-timing lines for the `/debug` snapshot; empty until the first frame
    /// is drawn. Percentiles are computed here, on demand, never per frame.
    pub(crate) fn frame_stats_lines(&self) -> Vec<String> {
        self.frame_stats
            .summary()
            .map(|summary| summary.lines())
            .unwrap_or_default()
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
                    // OSC 8 link markers are zero-width structured metadata, not
                    // visible text: exclude them from the width dump too.
                    .filter(|content| !crate::ui::hyperlink::is_marker(content))
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
        // The run meter survives a session swap: the exit receipt's scope is
        // the process run, so `/new` must not restart its clock or counters.
        let meter = self.screen.take_session_meter();
        self.screen = Screen::new();
        self.screen.pager_active = pager_active;
        self.screen.restore_session_meter(meter);
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
    use crate::nexus::{ApprovalDecision, CompactionLifecycleState, ToolCall};
    use crate::ui::UiEvent;
    use crate::ui::delegation_dashboard::{
        DelegationPayload, DelegationResponse, DelegationSnapshot,
    };
    use crate::ui::terminal_surface::{RenderKind, TerminalSurface};
    use iris_subagent_runtime::{WorkerEvent, WorkerId, WorkerSnapshot};
    use ratatui::style::{Color, Modifier};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    fn call(name: &str) -> ToolCall {
        call_args(name, json!({ "path": "note.txt", "content": "hi" }))
    }

    fn call_args(name: &str, arguments: serde_json::Value) -> ToolCall {
        call_args_id("call_1", name, arguments)
    }

    fn call_args_id(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
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

    fn lane_worker(
        id: &WorkerId,
        status: &str,
        description: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> WorkerSnapshot {
        serde_json::from_value(json!({
            "request": {
                "schema_version": 1,
                "kind": {"type": "general"},
                "prompt": description,
                "system_prompt": "delegated worker",
                "description": description,
                "priority": "normal",
                "policy": {
                    "tools": null,
                    "isolation": "none",
                    "cwd": null,
                    "allow_outside_workspace": false,
                    "nesting_depth": 0,
                    "max_nesting_depth": 2
                },
                "budgets": {},
                "recovery": "adoptable",
                "parent_worker_id": null,
                "session_id": "session-1",
                "route_id": null,
                "profile_id": null,
                "resume_from": null,
                "host": {"schema_version": 1, "kind": "none", "value": null}
            },
            "worker_id": id,
            "status": status,
            "group_id": null,
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "provider_rounds": 1,
                "tool_rounds": 1
            },
            "result": null,
            "last_event_sequence": 3
        }))
        .expect("worker fixture")
    }

    fn lane_events(id: &WorkerId, activity: &str) -> Vec<WorkerEvent> {
        serde_json::from_value(json!([
            {
                "schema_version": 1,
                "worker_id": id,
                "sequence": 1,
                "timestamp_ms": 1_000,
                "kind": {"type": "status", "data": "queued"}
            },
            {
                "schema_version": 1,
                "worker_id": id,
                "sequence": 2,
                "timestamp_ms": 2_000,
                "kind": {"type": "progress", "data": {"message": activity}}
            }
        ]))
        .expect("worker events fixture")
    }

    fn terminal_lane_worker(id: &WorkerId, status: &str, changed_paths: &[&str]) -> WorkerSnapshot {
        let mut worker = serde_json::to_value(lane_worker(id, status, "Terminal worker", 20, 4))
            .expect("serialize worker fixture");
        worker["result"] = json!({
            "schema_version": 1,
            "worker_id": id,
            "status": status,
            "summary": "finished",
            "inline_output": null,
            "artifacts": [],
            "usage": {
                "input_tokens": 20,
                "output_tokens": 4,
                "provider_rounds": 1,
                "tool_rounds": 1
            },
            "changed_paths": changed_paths,
            "worktree": null,
            "apply_plan_id": null,
            "host": {"schema_version": 1, "kind": "none", "value": null},
            "message": null
        });
        serde_json::from_value(worker).expect("terminal worker fixture")
    }

    fn arm_background_workers(screen: &mut Screen, ids: &[WorkerId], background: bool) {
        for id in ids {
            screen.apply(UiEvent::ToolResult {
                call: call_args(
                    "spawn_subagent",
                    json!({"background": background, "description": "worker"}),
                ),
                content: json!({"worker_id": id, "status": "queued"}).to_string(),
                exit_code: Some(0),
                duration: Some(Duration::from_millis(1)),
            });
        }
    }

    fn apply_lane_snapshot(
        screen: &mut Screen,
        now: Instant,
        now_ms: u64,
        workers: Vec<WorkerSnapshot>,
        events: BTreeMap<WorkerId, Vec<WorkerEvent>>,
    ) {
        let request = screen
            .request_worker_refresh(now)
            .expect("worker refresh request");
        assert!(screen.apply_worker_snapshot_response(
            &DelegationResponse {
                request_id: request.request_id,
                result: Ok(DelegationPayload::Snapshot(DelegationSnapshot {
                    workers,
                    worktrees: None,
                    events,
                })),
            },
            now_ms,
        ));
    }

    #[test]
    fn provider_transport_recovery_does_not_add_a_transcript_row() {
        let mut screen = Screen::new();
        let rows = screen.transcript.rows.len();

        screen.apply(UiEvent::ProviderTransportRecovery);

        assert_eq!(screen.transcript.rows.len(), rows);
    }

    #[test]
    fn spawn_subagent_renders_a_delegate_card_not_an_edit_panel() {
        let mut screen = Screen::new();
        screen.set_footer("gpt-5.5".to_string(), None, "~/repo".to_string());
        let call = call_args(
            "spawn_subagent",
            json!({
                "subagent_type": "review",
                "model": "sonnet-4-5",
                "effort": "high",
                "description": "smoke review",
                "task": "run the smoke suite"
            }),
        );
        screen.apply(UiEvent::ToolStarted(call.clone()));
        let id = WorkerId::new();
        screen.apply(UiEvent::ToolResult {
            call,
            content: json!({
                "worker_id": id,
                "status": "queued"
            })
            .to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(1)),
        });
        let rendered = rendered_text(&mut screen, 120, 30);
        assert!(rendered.contains("DELEGATE"), "{rendered}");
        assert!(
            rendered.contains("review · sonnet-4-5 · high effort — smoke review"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("EDIT"),
            "dispatch must not render as an EDIT panel: {rendered}"
        );
        // The card body echoes the worker-lane row grammar: state glyph and
        // bold short ID for the single dispatched worker.
        let short = format!("wrk_{}", &id.as_str()[4..12]);
        assert!(rendered.contains(&short), "{rendered}");
    }

    #[test]
    fn ambient_worker_lane_matches_inline_and_pager_top_chrome() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            "~/repo".to_string(),
        );
        let worker_id = WorkerId::new();
        arm_background_workers(&mut screen, std::slice::from_ref(&worker_id), true);
        let worker = lane_worker(&worker_id, "running", "Run focused tests", 1_200, 34);
        let activity = "running bash: cargo test --locked lane";
        let events = BTreeMap::from([(worker_id.clone(), lane_events(&worker_id, activity))]);
        apply_lane_snapshot(&mut screen, Instant::now(), 3_500, vec![worker], events);

        let short = format!("wrk_{}", &worker_id.as_str()[4..12]);
        let expected = format!(
            "  {} {short}  Run focused tests  {activity}  ↑1.2k ↓34  2.5s",
            crate::ui::symbols::RUNNING
        );
        // The card renders BELOW the session bar block (bar, rule) on both
        // surfaces, then closes with its own rule.
        let inline = rendered_lines(&mut screen, 100, 24);
        assert_eq!(line_text(&inline[2]), expected);

        let pager = pager::compose_frame(&mut screen, Size::new(100, 24));
        assert_eq!(line_text(&pager.lines[2]), expected);
    }

    #[test]
    fn ambient_worker_lane_caps_rows_and_degrades_at_narrow_width() {
        let mut screen = Screen::new();
        screen.set_footer("gpt-5.5".to_string(), None, "~/repo".to_string());
        let ids = (0..9).map(|_| WorkerId::new()).collect::<Vec<_>>();
        arm_background_workers(&mut screen, &ids, true);
        let workers = ids
            .iter()
            .enumerate()
            .map(|(index, id)| lane_worker(id, "running", &format!("Worker {index}"), 10, 2))
            .collect::<Vec<_>>();
        let events = ids
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    lane_events(id, "running bash: cargo test --locked worker_lane"),
                )
            })
            .collect();
        apply_lane_snapshot(&mut screen, Instant::now(), 2_000, workers, events);

        let wide = screen::ambient_worker_lane_block(&screen, 100);
        assert_eq!(wide.len(), 10, "eight worker rows + overflow + rule");
        assert_eq!(line_text(&wide[8]), "  … 1 more");

        let narrow = screen::ambient_worker_lane_block(&screen, 48);
        assert_eq!(narrow.len(), 10, "the card height is width-independent");
        assert!(
            line_text(&narrow[0]).contains("running bash"),
            "{}",
            line_text(&narrow[0])
        );
        assert!(
            narrow
                .iter()
                .all(|line| crate::ui::textengine::display_width(&line_text(line)) <= 48)
        );
    }

    #[test]
    fn ambient_worker_lane_keeps_finished_and_pending_rows_until_the_group_retires() {
        let mut screen = Screen::new();
        screen.set_footer("gpt-5.5".to_string(), None, "~/repo".to_string());
        let ids = (0..3).map(|_| WorkerId::new()).collect::<Vec<_>>();
        arm_background_workers(&mut screen, &ids, true);

        // Before the first snapshot the card is already at its final height:
        // one starting placeholder per armed worker.
        let block = screen::ambient_worker_lane_block(&screen, 100);
        assert_eq!(block.len(), 4, "three placeholders + rule");
        assert!(
            block
                .iter()
                .take(3)
                .all(|line| line_text(line).contains("starting…"))
        );

        // One finished, one live, one still absent from the snapshot: the
        // height must not change and the finished row must stay visible.
        let workers = vec![
            terminal_lane_worker(&ids[0], "completed", &[]),
            lane_worker(&ids[1], "running", "Live worker", 10, 2),
        ];
        apply_lane_snapshot(&mut screen, Instant::now(), 2_000, workers, BTreeMap::new());
        let block = screen::ambient_worker_lane_block(&screen, 100);
        assert_eq!(block.len(), 4, "card height is stable while the group runs");
        let text = block.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(
            text.contains("Terminal worker"),
            "finished row stays visible until the whole group retires: {text}"
        );
        assert!(text.contains("Live worker"), "{text}");
        assert!(text.contains("starting…"), "{text}");
        assert!(
            !screen.tick(),
            "a partially finished group must not start the linger countdown"
        );
    }

    #[test]
    fn pinned_prompt_band_stays_above_the_ambient_worker_card() {
        let mut screen = Screen::new();
        screen.set_footer("gpt-5.5".to_string(), None, "~/repo".to_string());
        screen.pager_active = true;
        screen.commit_user("governing question");
        for i in 0..60 {
            screen.apply(UiEvent::Notice(format!("answer detail {i}")));
        }
        let worker_id = WorkerId::new();
        arm_background_workers(&mut screen, std::slice::from_ref(&worker_id), true);
        let worker = lane_worker(&worker_id, "running", "Lane worker", 10, 2);
        apply_lane_snapshot(
            &mut screen,
            Instant::now(),
            2_000,
            vec![worker],
            BTreeMap::new(),
        );

        let frame = pager::compose_frame(&mut screen, Size::new(80, 24)).lines;
        let texts = frame.iter().map(line_text).collect::<Vec<_>>();
        let band = texts
            .iter()
            .position(|text| text.contains("governing question"))
            .expect("pinned prompt band");
        let lane = texts
            .iter()
            .position(|text| text.contains("Lane worker"))
            .expect("worker card row");
        assert_eq!(
            band, 2,
            "the governing prompt pins directly under the session bar: {texts:?}"
        );
        assert!(
            band < lane,
            "the worker card renders below the pinned prompt, never above it: \
             band {band} lane {lane}"
        );
    }

    #[test]
    fn ambient_worker_lane_polls_only_for_known_live_workers_and_lingers_five_ticks() {
        let now = Instant::now();
        let mut idle = Screen::new();
        assert!(idle.request_worker_refresh(now).is_none());

        let blocking_id = WorkerId::new();
        arm_background_workers(&mut idle, std::slice::from_ref(&blocking_id), false);
        assert!(
            idle.request_worker_refresh(now).is_none(),
            "blocking spawn must not arm ambient polling"
        );

        let mut screen = Screen::new();
        screen.set_footer("gpt-5.5".to_string(), None, "~/repo".to_string());
        let worker_id = WorkerId::new();
        arm_background_workers(&mut screen, std::slice::from_ref(&worker_id), true);
        let worker = lane_worker(&worker_id, "completed", "Finished worker", 20, 4);
        let events = BTreeMap::from([(
            worker_id.clone(),
            lane_events(&worker_id, "running read: src/lib.rs"),
        )]);
        apply_lane_snapshot(&mut screen, now, 2_000, vec![worker], events);

        assert!(
            screen
                .request_worker_refresh(now + Duration::from_secs(1))
                .is_none()
        );
        assert!(rendered_text(&mut screen, 80, 24).contains("Finished worker"));
        for _ in 0..4 {
            assert!(screen.tick());
        }
        assert!(rendered_text(&mut screen, 80, 24).contains("Finished worker"));
        assert!(screen.tick());
        assert!(!rendered_text(&mut screen, 80, 24).contains("Finished worker"));
    }

    #[test]
    fn ambient_worker_lane_retries_after_its_phase_abandons_a_snapshot_request() {
        let now = Instant::now();
        let mut screen = Screen::new();
        let worker_id = WorkerId::new();
        arm_background_workers(&mut screen, std::slice::from_ref(&worker_id), true);

        let abandoned = screen
            .request_worker_refresh(now)
            .expect("initial worker refresh request");
        assert!(
            screen
                .request_worker_refresh(now + Duration::from_millis(100))
                .is_none(),
            "an in-flight request must not be duplicated"
        );

        screen.abandon_worker_refresh();
        let retry = screen
            .request_worker_refresh(now + Duration::from_millis(300))
            .expect("the next phase must be able to refresh");
        assert_ne!(abandoned.request_id, retry.request_id);

        assert!(
            !screen.apply_worker_snapshot_response(
                &DelegationResponse {
                    request_id: abandoned.request_id,
                    result: Ok(DelegationPayload::Snapshot(DelegationSnapshot {
                        workers: Vec::new(),
                        worktrees: None,
                        events: BTreeMap::new(),
                    })),
                },
                0,
            ),
            "a late response must not clear the retry's request ID"
        );
        assert!(
            screen
                .request_worker_refresh(now + Duration::from_secs(1))
                .is_none(),
            "the retry must remain in flight after a stale response"
        );
    }

    #[test]
    fn background_worker_terminal_states_append_durable_quiet_notices() {
        let cases = [
            ("completed", vec!["src/lib.rs", "src/ui.rs", "tests/ui.rs"]),
            ("failed", Vec::new()),
            ("cancelled", Vec::new()),
        ];
        for (status, changed_paths) in cases {
            let mut screen = Screen::new();
            screen.set_footer("gpt-5.5".to_string(), None, "~/repo".to_string());
            let worker_id = WorkerId::new();
            arm_background_workers(&mut screen, std::slice::from_ref(&worker_id), true);
            let worker = terminal_lane_worker(&worker_id, status, &changed_paths);
            let events = BTreeMap::from([(
                worker_id.clone(),
                lane_events(&worker_id, "running read: src/lib.rs"),
            )]);
            apply_lane_snapshot(&mut screen, Instant::now(), 2_000, vec![worker], events);

            let short = format!("wrk_{}", &worker_id.as_str()[4..12]);
            let changed = if changed_paths.is_empty() {
                String::new()
            } else {
                format!(" — {} files changed", changed_paths.len())
            };
            let notice = format!("┊ subagent {short} {status}{changed}");
            assert!(
                rendered_text(&mut screen, 100, 24).contains(&notice),
                "{notice}"
            );
            for _ in 0..5 {
                screen.tick();
            }
            assert!(
                rendered_text(&mut screen, 100, 24).contains(&notice),
                "terminal notice must outlive the lane: {notice}"
            );
        }
    }

    #[test]
    fn blocking_worker_completion_does_not_append_a_lifecycle_notice() {
        let mut screen = Screen::new();
        let worker_id = WorkerId::new();
        arm_background_workers(&mut screen, std::slice::from_ref(&worker_id), false);
        assert!(screen.request_worker_refresh(Instant::now()).is_none());
        assert!(
            !rendered_text(&mut screen, 100, 24).contains("┊ subagent"),
            "blocking spawn results already have a tool block"
        );
    }

    /// Drive stream beats on the tick grid until the escapement has released all
    /// held text into the visible tail and the paced backlog has settled. The
    /// escapement (issue: the escapement spec) advances the tail by word-quanta
    /// per tick, so a test inspecting the mid-stream tail must first let those
    /// beats run — this reproduces the loop's cadence without a live loop.
    fn settle_stream(screen: &mut Screen) {
        let mut now = std::time::Instant::now();
        for _ in 0..256 {
            now += std::time::Duration::from_millis(100);
            if !screen.commit_stream_tick(now) {
                break;
            }
        }
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
        // The tail advances on the tick beat, not on the delta: let the
        // escapement release "Hello" into the tail, then it renders as one line.
        settle_stream(&mut screen);
        assert_eq!(screen.wrapped_lines(80).len(), 1);
        assert_eq!(screen.transcript.rows.len(), 0, "tail still uncommitted");
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
        assert!(!screen.transcript.stream.is_active());
    }

    #[test]
    fn assistant_text_renders_unmarked_without_role_label() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText(
            "# Title\n\nuse `cargo test` and:\n- one\n- two".to_string(),
        ));
        let lines = screen.wrapped_lines(80);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>();
        let joined = rendered.join("\n");

        assert!(!joined.contains("AGENT"), "{joined}");
        assert!(!joined.contains("USER"), "{joined}");
        // The agent speaks unmarked on the shared text column — no `›` (that
        // marks the user's turn now), no role label.
        assert!(
            !joined.contains('\u{203a}'),
            "agent stays unmarked: {joined}"
        );
        assert!(
            rendered.iter().any(|line| line.starts_with("      Title")),
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

        // Let the escapement release the whole delta and pace the closed blocks;
        // the still-open list item stays in the mutable tail (never committed
        // early), and the whole thing renders live like the finalized version.
        settle_stream(&mut screen);
        let live = screen.wrapped_lines(80);
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .all(|r| !row_text(r).contains("- one")),
            "the open list item is held in the tail, not committed early"
        );
        let live_document = render_document(&mut screen, Size::new(80, 12))
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(live_document.contains("Title"), "{live_document}");
        assert!(!live_document.contains('\u{203a}'), "{live_document}");
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
            // Release the held delta into the tail; the open block never commits.
            settle_stream(&mut screen);
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
        settle_stream(&mut screen);
        let live_a = screen.wrapped_lines(80);
        assert!(live_a.iter().any(|l| line_text(l).contains("alpha")));
        screen.apply(UiEvent::AssistantTextEnd(String::new()));

        screen.apply(UiEvent::AssistantTextDelta("gamma".to_string()));
        settle_stream(&mut screen);
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
        settle_stream(&mut screen);
        // Unchanged stream renders identically across repeated frames (the
        // memo-hit path must be byte-stable with the fresh render).
        let first = screen.wrapped_lines(80);
        let second = screen.wrapped_lines(80);
        assert_eq!(line_signature(&first), line_signature(&second));

        // A delta grows the buffer: once its beat releases, the tail shows it.
        screen.apply(UiEvent::AssistantTextDelta(" four".to_string()));
        settle_stream(&mut screen);
        let grown = screen.wrapped_lines(80);
        assert!(grown.iter().any(|l| line_text(l).contains("four")));

        // A width change must re-wrap the memoized stream, not reuse it.
        let narrow = screen.wrapped_lines(12);
        assert!(
            narrow.iter().all(|l| display_width(&line_text(l)) <= 12),
            "memoized streaming lines not re-wrapped for the narrower width"
        );
    }

    // --- Slice 1: assistant-message stream controller (issue #87) ---

    #[test]
    fn streamed_block_commits_to_scrollback_before_end() {
        // DoD: a completed streamed line enters transcript scrollback BEFORE
        // `AssistantTextEnd`, and the in-progress tail is not committed until
        // finalize.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantTextDelta(
            "First paragraph.\n\n".to_string(),
        ));
        // Pacing happens on the commit tick, not on the delta.
        assert!(
            screen.transcript.rows.is_empty(),
            "nothing commits before a commit tick"
        );
        let now = std::time::Instant::now();
        assert!(
            screen.commit_stream_tick(now),
            "a completed block commits on the tick"
        );
        let committed: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert!(
            committed.iter().any(|t| t.contains("First paragraph.")),
            "first block in scrollback before End: {committed:?}"
        );
        assert!(screen.transcript.stream.is_active(), "stream still active");

        // An in-progress second block stays in the mutable tail, not scrollback,
        // but is visible in the rendered frame.
        screen.apply(UiEvent::AssistantTextDelta(
            "Second in progress".to_string(),
        ));
        // Let the escapement release the second block into the tail.
        settle_stream(&mut screen);
        let committed2: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert!(
            !committed2.iter().any(|t| t.contains("Second in progress")),
            "open tail not committed: {committed2:?}"
        );
        let frame = rendered_text(&mut screen, 80, 16);
        assert!(
            frame.contains("Second in progress"),
            "tail visible: {frame}"
        );

        // Finalize commits the remainder exactly once.
        screen.apply(UiEvent::AssistantTextEnd(String::new()));
        let full = screen
            .transcript
            .rows
            .iter()
            .map(row_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(full.matches("Second in progress").count(), 1);
        assert!(!screen.transcript.stream.is_active());
    }

    #[test]
    fn stream_never_commits_before_newline_utf8_safe() {
        // DoD: no commit before a newline; a multibyte grapheme is never torn.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantTextDelta("汉".to_string()));
        screen.commit_stream_tick(std::time::Instant::now());
        assert!(
            screen.transcript.rows.is_empty(),
            "no commit before a newline"
        );
        let frame = rendered_text(&mut screen, 80, 8);
        assert!(
            frame.contains('汉'),
            "partial multibyte visible in tail: {frame}"
        );
        screen.apply(UiEvent::AssistantTextEnd(String::new()));
        let committed: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert_eq!(
            committed.iter().filter(|t| t.contains('汉')).count(),
            1,
            "final partial line committed exactly once: {committed:?}"
        );
    }

    #[test]
    fn streamed_markdown_table_does_not_snap_on_finalize() {
        // DoD / issue #87: a streamed markdown table must not reflow ("snap")
        // when the stream finalizes. The whole table is held in the mutable
        // tail (never committed row-by-row) so it renders exactly once.
        let deltas = [
            "| Col A | Col B |\n",
            "| --- | --- |\n",
            "| 1 | 2 |\n",
            "| 33333 | 4 |\n",
        ];
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        let base = std::time::Instant::now();
        for (i, delta) in deltas.iter().enumerate() {
            screen.apply(UiEvent::AssistantTextDelta((*delta).to_string()));
            let _ =
                screen.commit_stream_tick(base + std::time::Duration::from_millis(i as u64 * 100));
        }
        // No table row is committed while the table is still open.
        let committed_mid: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert!(
            committed_mid.iter().all(|t| !t.contains("Col A")),
            "table committed incrementally would snap: {committed_mid:?}"
        );
        // Let the escapement finish beating the table into the tail: this test
        // pins finalize-reflow identity, not pacing.
        settle_stream(&mut screen);
        // The live (tail) render already shows the complete table.
        let before: Vec<_> = screen
            .wrapped_lines(80)
            .iter()
            .filter(|l| !line_text(l).trim().is_empty())
            .cloned()
            .collect();
        assert!(
            before.iter().any(|l| line_text(l).contains("Col A")),
            "table visible in the live tail before finalize"
        );
        // Finalize: the committed render must be byte-identical to the live one.
        screen.apply(UiEvent::AssistantTextEnd(String::new()));
        let after: Vec<_> = screen
            .wrapped_lines(80)
            .iter()
            .filter(|l| !line_text(l).trim().is_empty())
            .cloned()
            .collect();
        assert_eq!(
            line_signature(&before),
            line_signature(&after),
            "streamed table reflowed on finalize (issue #87)"
        );
    }

    #[test]
    fn commit_tick_catches_up_on_backlog() {
        // DoD: the adaptive drain enters CatchUp under queue pressure and drains
        // the backlog (Smooth=1/tick is proven by the chunking unit tests).
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        let mut src = String::new();
        for i in 0..200 {
            src.push_str(&format!("Para {i}.\n\n"));
        }
        screen.apply(UiEvent::AssistantTextDelta(src));
        // A single tick with a firehose-deep backlog: the escapement fast-
        // forwards (half the buffer in one beat), the collector queue goes
        // deep, and the drain enters CatchUp — rather than committing one line.
        assert!(screen.commit_stream_tick(std::time::Instant::now()));
        let committed = screen
            .transcript
            .rows
            .iter()
            .map(row_text)
            .collect::<Vec<_>>();
        let paras = committed.iter().filter(|t| t.contains("Para ")).count();
        assert!(
            paras >= 10,
            "catch-up should drain the backlog in one tick, got {paras}: {committed:?}"
        );
    }

    #[test]
    fn incremental_stream_matches_full_replay() {
        // DoD: incremental render output == full replay of the same deltas.
        let deltas = [
            "# Title\n\n",
            "A paragraph with `code` and ",
            "**bold**.\n\n",
            "- item one\n- item two\n\n",
            "| a | b |\n| --- | --- |\n| 1 | 2 |\n\n",
            "Final line.\n",
        ];
        // Incremental path: stream deltas with ticks, then finalize.
        let mut inc = Screen::new();
        let _ = inc.wrapped_lines(80);
        let base = std::time::Instant::now();
        for (i, d) in deltas.iter().enumerate() {
            inc.apply(UiEvent::AssistantTextDelta((*d).to_string()));
            inc.commit_stream_tick(base + std::time::Duration::from_millis(i as u64 * 100));
        }
        inc.apply(UiEvent::AssistantTextEnd(String::new()));

        // Full replay: one non-streamed assistant message with the whole text.
        let full_text: String = deltas.concat();
        let mut full = Screen::new();
        let _ = full.wrapped_lines(80);
        full.apply(UiEvent::AssistantText(full_text));

        let inc_rows: Vec<String> = inc.transcript.rows.iter().map(row_text).collect();
        let full_rows: Vec<String> = full.transcript.rows.iter().map(row_text).collect();
        assert_eq!(
            inc_rows, full_rows,
            "committed rows differ from full replay"
        );
        assert_eq!(
            line_signature(&inc.wrapped_lines(80)),
            line_signature(&full.wrapped_lines(80)),
            "rendered signature differs from full replay"
        );
    }

    #[test]
    fn reasoning_splices_above_already_committed_answer() {
        // Production ordering: answer text streams and is paced into scrollback
        // during the turn; reasoning arrives only at completion. The thinking
        // block must still render ABOVE the already-committed answer.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantTextDelta(
            "The answer paragraph.\n\n".to_string(),
        ));
        settle_stream(&mut screen);
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|r| r.text.contains("The answer paragraph.")),
            "answer committed before reasoning arrives"
        );
        screen.apply(UiEvent::AssistantReasoning {
            text: "deliberating".to_string(),
            redacted: false,
        });
        screen.apply(UiEvent::AssistantTextEnd(String::new()));
        let out = rendered_text(&mut screen, 80, 20);
        let thinking_at = out.find("THINKING").expect("thinking label");
        let answer_at = out.find("The answer paragraph.").expect("answer");
        assert!(
            thinking_at < answer_at,
            "reasoning must render above the committed answer: {out}"
        );
        assert_eq!(
            out.matches("The answer paragraph.").count(),
            1,
            "answer rendered exactly once: {out}"
        );
    }

    // --- Slice 5: ordering, cancellation, pager hardening ---

    #[test]
    fn a_tool_event_commits_streamed_lines_before_the_tool_renders() {
        // DoD: a tool event never renders before the preceding streamed
        // assistant lines. Each tool arm finishes the stream first, so every
        // committed answer line precedes the tool panel in the frame (FIFO).
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(100);
        // Two complete blocks are streamed but NOT yet paced into scrollback.
        screen.apply(UiEvent::AssistantTextDelta(
            "Alpha answer line.\n\nBeta answer line.\n\n".to_string(),
        ));
        assert!(
            screen.transcript.rows.is_empty(),
            "nothing is committed before a tick or a tool event"
        );
        // A tool starts while the streamed answer is still un-committed.
        let tool = call_args("bash", json!({ "command": "echo SENTINEL_TOOL" }));
        screen.apply(UiEvent::ToolStarted(tool));
        assert!(
            !screen.transcript.stream.is_active(),
            "the tool event flushed the stream"
        );
        let frame = rendered_text(&mut screen, 100, 40);
        let alpha = frame.find("Alpha answer line.").expect("alpha committed");
        let beta = frame.find("Beta answer line.").expect("beta committed");
        let tool_at = frame.find("SENTINEL_TOOL").expect("tool rendered");
        assert!(
            alpha < beta && beta < tool_at,
            "streamed answer lines must precede the tool, in order: {frame}"
        );
    }

    #[test]
    fn cancellation_commits_partial_text_once_and_clears_the_tail() {
        // DoD: cancellation commits the partial assistant text EXACTLY once and
        // clears the tail/queues. On cancel Nexus emits `AssistantTextEnd(partial)`
        // (deltas were seen) then `ProviderTurnCancelled`; the End finalizes the
        // accumulated stream once and resets the controller.
        let partial = "Committed answer.\n\nOpen tail still typing";
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantTextDelta(partial.to_string()));
        // Pace the first complete block into scrollback; the last line stays in
        // the mutable tail.
        screen.commit_stream_tick(std::time::Instant::now());
        assert!(screen.transcript.stream.is_active(), "stream is mid-flight");

        screen.apply(UiEvent::AssistantTextEnd(partial.to_string()));
        screen.apply(UiEvent::ProviderTurnCancelled {
            turn_id: "t1".to_string(),
        });

        let full = screen
            .transcript
            .rows
            .iter()
            .map(row_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            full.matches("Open tail still typing").count(),
            1,
            "the partial tail is committed exactly once: {full}"
        );
        assert_eq!(
            full.matches("Committed answer.").count(),
            1,
            "the already-paced block is not duplicated on finalize: {full}"
        );
        assert!(
            !screen.transcript.stream.is_active() && !screen.has_stream_work(),
            "the tail and paced queue are cleared after cancellation"
        );
    }

    #[test]
    fn pager_visible_total_counts_the_active_stream_tail() {
        // DoD: the pager visible total includes the active tail, so a scrollback
        // that is entirely in-flight is still reachable/visible.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        // Three complete blocks. Before any beat they are held in the escapement
        // (nothing anywhere yet); the beat then releases them.
        screen.apply(UiEvent::AssistantTextDelta(
            "Tail A.\n\nTail B.\n\nTail C.\n\n".to_string(),
        ));
        assert!(
            screen.transcript.rows.is_empty(),
            "nothing is committed or shown before the first beat"
        );
        settle_stream(&mut screen);
        // The pager sizes its scrollable range from `transcript_visible_total`
        // (committed rows + streaming preview), not `render().total_lines`. The
        // paced tail (committed prefix + un-emitted tail) must all be counted, so
        // a regression that dropped the tail from the pager total would read 0.
        let visible_total = screen.transcript_visible_total(80);
        assert!(
            visible_total >= 3,
            "the uncommitted tail is counted in the pager visible total: {visible_total}"
        );
        let frame: String = screen
            .transcript
            .render(80)
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            frame.contains("Tail A.") && frame.contains("Tail C."),
            "the tail rows are part of the rendered document: {frame}"
        );
    }

    #[test]
    fn history_trim_is_held_while_a_stream_or_tool_is_active() {
        // DoD: history trim does not run while a stream (or tool) is active, so
        // an in-flight tail/queue is never dropped by a mid-turn trim.
        let mut screen = Screen::new();
        for i in 0..(MAX_TRANSCRIPT_ROWS + 32) {
            screen.transcript.rows.push(TranscriptRow::new(
                format!("history row {i}"),
                Style::default(),
            ));
        }
        let over_cap = screen.transcript.rows.len();
        assert!(over_cap > MAX_TRANSCRIPT_ROWS);

        // A stream in flight holds the trim.
        screen.apply(UiEvent::AssistantTextDelta("streaming tail...".to_string()));
        assert!(screen.transcript.stream.is_active());
        screen.transcript.trim_history();
        assert!(
            screen.transcript.rows.len() > MAX_TRANSCRIPT_ROWS,
            "no trim while a stream is active"
        );

        // Finalizing the stream releases the trim on the next event.
        screen.apply(UiEvent::AssistantTextEnd(String::new()));
        assert!(!screen.transcript.stream.is_active(), "stream finalized");
        assert!(
            screen.transcript.rows.len() <= MAX_TRANSCRIPT_ROWS,
            "trim runs once the stream is done: {}",
            screen.transcript.rows.len()
        );

        // A running tool likewise holds the trim.
        for i in 0..(MAX_TRANSCRIPT_ROWS + 32) {
            screen.transcript.rows.push(TranscriptRow::new(
                format!("more row {i}"),
                Style::default(),
            ));
        }
        assert!(screen.transcript.rows.len() > MAX_TRANSCRIPT_ROWS);
        screen.apply(UiEvent::ToolStarted(call_args(
            "bash",
            json!({ "command": "sleep 1" }),
        )));
        screen.transcript.trim_history();
        assert!(
            screen.transcript.rows.len() > MAX_TRANSCRIPT_ROWS,
            "no trim while a tool is running"
        );
    }

    #[test]
    fn full_replay_parity_across_stream_then_tool_then_cancellation() {
        // DoD: after a stream + tool + cancellation sequence, the incrementally
        // paced transcript matches a full replay of the same logical events
        // (no duplication, no lost partial, identical ordering).
        let one = "Answer part one.\n\n";
        let two = "Answer part two, interrupted";
        let tool = call_args("bash", json!({ "command": "echo hi" }));

        // Incremental: deltas with paced ticks, a tool between, then a cancel
        // (End carries the accumulated partial, as Nexus emits it).
        let mut inc = Screen::new();
        let _ = inc.wrapped_lines(80);
        let base = std::time::Instant::now();
        inc.apply(UiEvent::AssistantTextDelta(one.to_string()));
        inc.commit_stream_tick(base);
        inc.apply(UiEvent::ToolStarted(tool.clone()));
        inc.apply(UiEvent::ToolResult {
            call: tool.clone(),
            content: "hi".to_string(),
            exit_code: Some(0),
            // Deterministic elapsed so the finalized tool header cannot derive a
            // wall-clock duration from `Instant::now()` (avoids a 0.0s/0.1s flake
            // between the two sides of the parity comparison).
            duration: Some(std::time::Duration::ZERO),
        });
        inc.apply(UiEvent::AssistantTextDelta(two.to_string()));
        inc.apply(UiEvent::AssistantTextEnd(two.to_string()));
        inc.apply(UiEvent::ProviderTurnCancelled {
            turn_id: "t1".to_string(),
        });

        // Full replay: the same logical events without streaming/pacing. The
        // cancelled partial commits as a whole non-streamed assistant message,
        // mirroring Nexus's no-delta commit path.
        let mut full = Screen::new();
        let _ = full.wrapped_lines(80);
        full.apply(UiEvent::AssistantText(one.to_string()));
        full.apply(UiEvent::ToolStarted(tool.clone()));
        full.apply(UiEvent::ToolResult {
            call: tool,
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: Some(std::time::Duration::ZERO),
        });
        full.apply(UiEvent::AssistantText(two.to_string()));
        full.apply(UiEvent::ProviderTurnCancelled {
            turn_id: "t1".to_string(),
        });

        let inc_rows: Vec<String> = inc.transcript.rows.iter().map(row_text).collect();
        let full_rows: Vec<String> = full.transcript.rows.iter().map(row_text).collect();
        assert_eq!(
            inc_rows, full_rows,
            "paced stream+tool+cancel must match a full replay of the same events"
        );
        // And the rendered documents agree line-for-line.
        assert_eq!(
            line_signature(&inc.wrapped_lines(80)),
            line_signature(&full.wrapped_lines(80)),
            "rendered signature differs from full replay"
        );
    }

    // --- The escapement: even beats for the live stream ---

    /// Count occurrences of `needle` in the rendered document (committed rows +
    /// the transient stream/reasoning tail).
    fn rendered_needle_count(screen: &mut Screen, needle: &str) -> usize {
        rendered_text(screen, 90, 60).matches(needle).count()
    }

    #[test]
    fn streamed_tail_advances_by_the_beat_quantum_not_the_whole_burst() {
        // Criterion 2: feed a large burst in one delta — the visible tail grows
        // only by the beat quantum per tick, never the whole burst at once.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        let burst = "word ".repeat(100); // 500 bytes, one open paragraph
        screen.apply(UiEvent::AssistantTextDelta(burst));
        // Nothing is shown before the first beat (held in the escapement).
        assert_eq!(
            rendered_needle_count(&mut screen, "word"),
            0,
            "no tail before the first beat"
        );
        // One beat releases a bounded quantum — some words, but not all 100.
        let base = std::time::Instant::now();
        screen.commit_stream_tick(base);
        let after_one = rendered_needle_count(&mut screen, "word");
        assert!(
            after_one > 0 && after_one < 100,
            "the tail grew by a quantum, not the whole burst: {after_one}"
        );
        // Each further beat advances it monotonically until fully drained.
        screen.commit_stream_tick(base + std::time::Duration::from_millis(100));
        let after_two = rendered_needle_count(&mut screen, "word");
        assert!(
            after_two > after_one,
            "the tail keeps advancing per beat: {after_one} -> {after_two}"
        );
        settle_stream(&mut screen);
        assert_eq!(
            rendered_needle_count(&mut screen, "word"),
            100,
            "the whole burst drains across beats"
        );
        // The committed pipeline is untouched: an open paragraph never commits.
        assert!(
            screen.transcript.rows.is_empty(),
            "the open tail is never committed by the escapement"
        );
    }

    #[test]
    fn pacing_changes_when_a_line_shows_never_what_the_message_is() {
        // Criterion 3: the finalized message is byte-identical with the
        // escapement pacing on vs bypassed (reduced motion) — pacing changes
        // WHEN a line shows, never WHAT the finished message says.
        let deltas = [
            "# Heading\n\n",
            "A paragraph with `code` and **bold**.\n\n",
            "- item one\n- item two\n\n",
            "| a | b |\n| --- | --- |\n| 1 | 2 |\n\n",
            "Final tail line.\n",
        ];
        // Paced: escapement on, streamed with per-delta ticks, then finalize.
        let mut paced = Screen::new();
        let _ = paced.wrapped_lines(80);
        let base = std::time::Instant::now();
        for (i, d) in deltas.iter().enumerate() {
            paced.apply(UiEvent::AssistantTextDelta((*d).to_string()));
            paced.commit_stream_tick(base + std::time::Duration::from_millis(i as u64 * 100));
        }
        paced.apply(UiEvent::AssistantTextEnd(String::new()));

        // Bypassed: reduced motion, same deltas, no ticks.
        let mut bypass = Screen::new();
        bypass.set_reduced_motion(true);
        let _ = bypass.wrapped_lines(80);
        for d in deltas {
            bypass.apply(UiEvent::AssistantTextDelta(d.to_string()));
        }
        bypass.apply(UiEvent::AssistantTextEnd(String::new()));

        let paced_rows: Vec<String> = paced.transcript.rows.iter().map(row_text).collect();
        let bypass_rows: Vec<String> = bypass.transcript.rows.iter().map(row_text).collect();
        assert_eq!(
            paced_rows, bypass_rows,
            "the finished message is byte-identical regardless of pacing"
        );
        assert_eq!(
            line_signature(&paced.wrapped_lines(80)),
            line_signature(&bypass.wrapped_lines(80)),
            "rendered finished message differs between paced and bypassed"
        );
    }

    #[test]
    fn reduced_motion_shows_arrival_in_the_same_frame() {
        // Criterion 6: reduced motion is pass-through — arrival == display in the
        // same frame, with no beat needed.
        let mut screen = Screen::new();
        screen.set_reduced_motion(true);
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantTextDelta(
            "instant answer tail".to_string(),
        ));
        assert!(
            rendered_text(&mut screen, 80, 12).contains("instant answer tail"),
            "reduced-motion answer renders on arrival, no beat"
        );

        let mut screen2 = Screen::new();
        screen2.set_reduced_motion(true);
        let _ = screen2.wrapped_lines(80);
        screen2.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen2.apply(UiEvent::AssistantReasoningDelta(
            "instant reasoning trace".to_string(),
        ));
        assert!(
            rendered_text(&mut screen2, 80, 12).contains("instant reasoning trace"),
            "reduced-motion reasoning renders on arrival, no beat"
        );
    }

    #[test]
    fn entering_reduced_motion_flushes_held_stream_text() {
        // Criterion 4 (§2.2): entering reduced motion flushes any escapement-held
        // text immediately.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantTextDelta(
            "held mid-stream text".to_string(),
        ));
        assert!(
            !rendered_text(&mut screen, 80, 12).contains("held mid-stream text"),
            "held before entering reduced motion"
        );
        screen.set_reduced_motion(true);
        assert!(
            rendered_text(&mut screen, 80, 12).contains("held mid-stream text"),
            "entering reduced motion flushed the held tail"
        );
    }

    #[test]
    fn only_the_commit_tick_beats_the_escapement_no_second_timer() {
        // Criterion 7: the drain is driven by the existing tick/commit cadence
        // ONLY. The animation tick (`screen.tick`) must not advance the tail —
        // there is no second timer.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantTextDelta(
            "held until the commit tick".to_string(),
        ));
        for _ in 0..5 {
            let _ = screen.tick();
        }
        assert!(
            !rendered_text(&mut screen, 80, 12).contains("held until"),
            "screen.tick (animation) must not beat the escapement"
        );
        // Only commit_stream_tick — the same cadence that paces scrollback —
        // advances the tail.
        assert!(screen.commit_stream_tick(std::time::Instant::now()));
        assert!(
            rendered_text(&mut screen, 80, 12).contains("held until"),
            "commit_stream_tick is the single beat driver"
        );
    }

    #[test]
    fn approval_gate_flushes_pending_text_before_the_review_block() {
        // Criterion 4 (§2.2): an approval gate opening flushes — the user must
        // review against complete context, so the pending assistant text is
        // visible above the REVIEW block.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(100);
        screen.apply(UiEvent::AssistantTextDelta(
            "Rationale for the proposed change.\n\n".to_string(),
        ));
        // The gated tool opens its REVIEW block; the block's own begin_block
        // finalizes (flushes) the stream so the text lands above it.
        let call = call_args("bash", json!({ "command": "rm -rf build" }));
        screen.apply(UiEvent::ToolReview {
            call,
            allow_always: true,
            allow_project: false,
            dirty_gate: false,
            reason: None,
        });
        let out = rendered_text(&mut screen, 100, 30);
        let text_at = out
            .find("Rationale for the proposed change.")
            .expect("pending text visible before review");
        let review_at = out.find("REVIEW").expect("review block rendered");
        assert!(
            text_at < review_at,
            "pending assistant text renders above the REVIEW block: {out}"
        );
    }

    #[test]
    fn show_approval_flushes_the_pending_tail() {
        // Criterion 4 (§2.2): the show_approval entry point itself flushes the
        // escapement into the tail, independent of the block-rendering path.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantTextDelta(
            "pending answer tail".to_string(),
        ));
        assert!(
            !rendered_text(&mut screen, 80, 12).contains("pending answer tail"),
            "held before the gate opens"
        );
        screen.show_approval(true, false, false);
        assert!(
            rendered_text(&mut screen, 80, 12).contains("pending answer tail"),
            "the approval gate released the held tail"
        );
    }

    #[test]
    fn session_reset_flushes_the_pending_stream() {
        // Criterion 4 (§2.2): a session reset (`/new`) flushes — the stream is
        // finalized, its text committed, and no escapement backlog is stranded.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantTextDelta(
            "answer before the reset.\n\n".to_string(),
        ));
        screen.apply(UiEvent::SessionStarted);
        assert!(
            !screen.transcript.stream.is_active() && !screen.has_stream_work(),
            "the session reset flushed and finalized the stream"
        );
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|r| row_text(r).contains("answer before the reset.")),
            "the held text was flushed, not stranded"
        );
    }

    #[test]
    fn cancel_flushes_the_held_answer_tail_without_an_end() {
        // Criterion 4 (§2.2): a terminal provider event (cancel) flushes the
        // answer stream even when no AssistantTextEnd precedes it.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantTextDelta(
            "partial answer held in the escapement.\n\n".to_string(),
        ));
        screen.apply(UiEvent::ProviderTurnCancelled {
            turn_id: "t1".to_string(),
        });
        assert!(
            !screen.transcript.stream.is_active() && !screen.has_stream_work(),
            "cancel finalized the stream"
        );
        let committed = screen
            .transcript
            .rows
            .iter()
            .map(row_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            committed
                .matches("partial answer held in the escapement.")
                .count(),
            1,
            "the held tail is committed exactly once on cancel: {committed}"
        );
    }

    #[test]
    fn completion_flushes_the_held_answer_tail_without_an_end() {
        // Criterion 4 (§2.2): provider turn completion flushes the answer
        // stream even when no AssistantTextEnd precedes it — same defensive
        // guard as cancel/error, not a reliance on Nexus event ordering.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantTextDelta(
            "partial answer held in the escapement.\n\n".to_string(),
        ));
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "t1".to_string(),
            response_id: None,
            usage: None,
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        assert!(
            !screen.transcript.stream.is_active() && !screen.has_stream_work(),
            "completion finalized the stream"
        );
        let committed = screen
            .transcript
            .rows
            .iter()
            .map(row_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            committed
                .matches("partial answer held in the escapement.")
                .count(),
            1,
            "the held tail is committed exactly once on completion: {committed}"
        );
    }

    #[test]
    fn reasoning_burst_paces_across_beats_and_flushes_on_end() {
        // Criterion 5: a reasoning delta burst renders across beats (not all at
        // once), and reasoning end flushes the trace.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        let burst = "reasoning ".repeat(30); // 300 bytes
        screen.apply(UiEvent::AssistantReasoningDelta(burst));
        // Held until the beat: nothing shown yet.
        assert_eq!(
            rendered_needle_count(&mut screen, "reasoning"),
            0,
            "reasoning is held until the beat"
        );
        // One beat releases a bounded quantum of the trace, not all of it.
        screen.commit_stream_tick(std::time::Instant::now());
        let after_one = rendered_needle_count(&mut screen, "reasoning");
        assert!(
            after_one > 0 && after_one < 30,
            "reasoning renders across beats, not all at once: {after_one}"
        );
        settle_stream(&mut screen);
        assert_eq!(
            rendered_needle_count(&mut screen, "reasoning"),
            30,
            "the whole reasoning burst drains across beats"
        );
        // Still transient — not committed to scrollback until the trace ends.
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .all(|r| !row_text(r).contains("reasoning")),
            "reasoning preview stays transient before its end"
        );
        // Reasoning end (a non-reasoning event) flushes + commits the trace.
        screen.apply(UiEvent::TurnComplete);
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|r| row_text(r).contains("reasoning")),
            "reasoning flushed and committed on end"
        );
    }

    #[test]
    fn reasoning_flushes_and_commits_on_provider_error() {
        // Criterion 4 (§2.2): a provider error flushes the reasoning trace.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(
            "partial thought held".to_string(),
        ));
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .all(|r| !row_text(r).contains("partial thought held")),
            "held before the error"
        );
        screen.apply(UiEvent::ProviderTurnError {
            turn_id: "t1".to_string(),
            message: "boom".to_string(),
        });
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|r| row_text(r).contains("partial thought held")),
            "reasoning flushed and committed on error"
        );
    }

    // --- Slice 3: provider-neutral live reasoning deltas ---

    #[test]
    fn live_reasoning_previews_then_commits_above_the_answer() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        // Reasoning summary streams first, as a transient preview: visible in
        // the frame but NOT yet committed to scrollback.
        screen.apply(UiEvent::AssistantReasoningDelta("Weighing ".to_string()));
        screen.apply(UiEvent::AssistantReasoningDelta("the options.".to_string()));
        // The reasoning caret steps on the tick beat: let the escapement release
        // the held reasoning into the preview.
        settle_stream(&mut screen);
        let preview = rendered_text(&mut screen, 80, 16);
        assert!(
            preview.contains("THINKING"),
            "live thinking rail: {preview}"
        );
        assert!(preview.contains("Weighing the options."), "{preview}");
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .all(|r| !row_text(r).contains("Weighing")),
            "reasoning preview is transient, not committed yet"
        );
        // The answer starts: the reasoning trace commits to scrollback above it.
        screen.apply(UiEvent::AssistantTextDelta(
            "Here is the answer.".to_string(),
        ));
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|r| row_text(r).contains("Weighing the options.")),
            "reasoning committed on first answer delta"
        );
        // Release the held answer into the tail so it renders below the trace.
        settle_stream(&mut screen);
        let out = rendered_text(&mut screen, 80, 16);
        let thinking_at = out.find("THINKING").expect("thinking");
        let answer_at = out.find("Here is the answer.").expect("answer");
        assert!(thinking_at < answer_at, "reasoning above the answer: {out}");
        // Exactly one thinking block (no duplicate from a late canonical event,
        // which Nexus suppresses when the summary streamed).
        assert_eq!(
            out.matches("THINKING").count(),
            1,
            "one thinking block: {out}"
        );
    }

    #[test]
    fn live_thinking_keeps_the_same_leading_separator_as_settled_thinking() {
        // Screenshot regression: while reasoning streamed after a tool footer,
        // THINKING mounted directly against DONE. Finalization then inserted a
        // blank above the committed rail and shifted the visible pane by one
        // row. The block boundary must exist from its first live frame.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "cat /tmp/findings.md" })),
            content: "finding".to_string(),
            exit_code: Some(0),
            duration: None,
        });
        screen.set_reduced_motion(true);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(
            "Checking the revealed rows.".to_string(),
        ));

        let live: Vec<String> = rendered_lines(&mut screen, 80, 24)
            .iter()
            .map(line_text)
            .collect();
        let live_header = live
            .iter()
            .position(|line| line.contains("THINKING"))
            .expect("live THINKING header");
        assert!(
            live_header > 0 && live[live_header - 1].trim().is_empty(),
            "live rail needs its leading separator from frame one: {live:?}"
        );

        screen.apply(UiEvent::AssistantTextDelta("Done.".to_string()));
        let settled: Vec<String> = rendered_lines(&mut screen, 80, 24)
            .iter()
            .map(line_text)
            .collect();
        let settled_header = settled
            .iter()
            .position(|line| line.contains("THINKING"))
            .expect("settled THINKING header");
        assert!(
            settled_header > 0 && settled[settled_header - 1].trim().is_empty(),
            "settled rail keeps the same boundary: {settled:?}"
        );
    }

    #[test]
    fn settled_thinking_owns_exactly_one_separator_before_the_next_tool() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.set_reduced_motion(true);
        screen.apply(UiEvent::AssistantReasoningDelta(
            "Inspecting the result.".to_string(),
        ));
        screen.apply(UiEvent::ToolResult {
            call: call_args_id(
                "call-after-thinking",
                "bash",
                json!({ "command": "echo next" }),
            ),
            content: "next".to_string(),
            exit_code: Some(0),
            duration: None,
        });
        let lines: Vec<String> = rendered_lines(&mut screen, 80, 24)
            .iter()
            .map(line_text)
            .collect();
        let thinking = lines
            .iter()
            .position(|line| line.contains("THINKING"))
            .expect("THINKING header");
        let shell = lines
            .iter()
            .enumerate()
            .skip(thinking + 1)
            .find_map(|(index, line)| line.contains("SHELL").then_some(index))
            .expect("following SHELL header");
        let gaps = lines[thinking + 1..shell]
            .iter()
            .filter(|line| line.trim().is_empty())
            .count();
        assert_eq!(
            gaps, 1,
            "RailEnd already supplies the one inter-block gap: {lines:?}"
        );
    }

    #[test]
    fn late_thinking_owns_exactly_one_separator_before_the_existing_answer() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.set_reduced_motion(true);
        screen.apply(UiEvent::AssistantTextDelta("Existing answer.".to_string()));
        screen.apply(UiEvent::AssistantReasoning {
            text: "Late trace.".to_string(),
            redacted: false,
        });
        screen.apply(UiEvent::AssistantTextEnd("Existing answer.".to_string()));
        let lines: Vec<String> = rendered_lines(&mut screen, 80, 24)
            .iter()
            .map(line_text)
            .collect();
        let thinking = lines
            .iter()
            .position(|line| line.contains("THINKING"))
            .expect("THINKING header");
        let answer = lines
            .iter()
            .enumerate()
            .skip(thinking + 1)
            .find_map(|(index, line)| line.contains("Existing answer.").then_some(index))
            .expect("existing answer");
        let gaps = lines[thinking + 1..answer]
            .iter()
            .filter(|line| line.trim().is_empty())
            .count();
        assert_eq!(
            gaps, 1,
            "RailEnd separates the late rail from the answer once: {lines:?}"
        );
    }

    #[test]
    fn live_summary_collapses_and_raw_expands() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(
            "Checking parser precedence.".to_string(),
        ));
        screen.apply(UiEvent::AssistantRawReasoningDelta(
            "Need inspect completed_response before item-level placeholder wins.".to_string(),
        ));
        screen.apply(UiEvent::AssistantTextDelta("Answer.".to_string()));

        let collapsed = rendered_text(&mut screen, 80, 18);
        assert!(collapsed.contains("THINKING"), "{collapsed}");
        assert!(collapsed.contains("▸"), "{collapsed}");
        assert!(
            collapsed.contains("Checking parser precedence."),
            "collapsed thinking shows summary: {collapsed}"
        );
        assert!(
            !collapsed.contains("Need inspect completed_response"),
            "collapsed thinking hides raw reasoning: {collapsed}"
        );

        assert!(screen.toggle_latest_panel());
        let expanded = rendered_text(&mut screen, 80, 18);
        assert!(expanded.contains("▾"), "{expanded}");
        assert!(
            expanded.contains("Need inspect completed_response"),
            "expanded thinking reveals raw reasoning: {expanded}"
        );
        assert!(
            !expanded.contains("Checking parser precedence."),
            "expanded thinking swaps summary for raw body: {expanded}"
        );
    }

    #[test]
    fn find_matches_collapsed_thinking_summary() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(
            "Checking parser precedence.".to_string(),
        ));
        screen.apply(UiEvent::AssistantRawReasoningDelta(
            "Inspect completed response before item placeholder wins.".to_string(),
        ));
        screen.apply(UiEvent::AssistantTextDelta("Answer.".to_string()));

        let collapsed = rendered_text(&mut screen, 80, 18);
        assert!(
            collapsed.contains("Checking parser precedence."),
            "{collapsed}"
        );
        assert!(
            !collapsed.contains("Inspect completed response"),
            "{collapsed}"
        );

        let matches = screen.transcript.search_matches("parser precedence");
        assert!(
            !matches.is_empty(),
            "collapsed thinking summary is searchable"
        );
    }

    #[test]
    fn thinking_toggle_updates_incremental_terminal_surface() -> std::io::Result<()> {
        let mut screen = Screen::new();
        let mut surface = TerminalSurface::new(Vec::new());
        let size = Size::new(80, 18);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(
            "Checking parser precedence.".to_string(),
        ));
        screen.apply(UiEvent::AssistantRawReasoningDelta(
            "Inspect completed response before item placeholder wins.".to_string(),
        ));
        screen.apply(UiEvent::AssistantTextDelta("Answer.".to_string()));

        let first = render_perf_cycle(&mut screen, &mut surface, size)?;
        let collapsed = strip_ansi(&surface.state().previous_lines.join("\n"));
        assert_eq!(first, RenderKind::First);
        assert!(
            collapsed.contains("Checking parser precedence."),
            "{collapsed}"
        );
        assert!(
            !collapsed.contains("Inspect completed response"),
            "{collapsed}"
        );

        assert!(screen.toggle_all_panels());
        let toggled = render_perf_cycle(&mut screen, &mut surface, size)?;
        let expanded = strip_ansi(&surface.state().previous_lines.join("\n"));
        assert_ne!(toggled, RenderKind::Unchanged);
        assert!(
            expanded.contains("Inspect completed response"),
            "{expanded}"
        );
        assert!(
            !expanded.contains("Checking parser precedence."),
            "{expanded}"
        );
        Ok(())
    }

    #[test]
    fn live_reasoning_commits_on_reasoning_only_completion() {
        // A turn that streams reasoning and then ends with no answer text (e.g.
        // straight into a tool or turn end) must still commit the trace.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(
            "Planning quietly.".to_string(),
        ));
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .all(|r| !row_text(r).contains("Planning")),
            "still transient before completion"
        );
        screen.apply(UiEvent::TurnComplete);
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|r| row_text(r).contains("Planning quietly.")),
            "reasoning-only trace committed at turn end"
        );
    }

    #[test]
    fn live_reasoning_commits_on_cancellation() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(
            "Half a thought".to_string(),
        ));
        screen.apply(UiEvent::ProviderTurnCancelled {
            turn_id: "t1".to_string(),
        });
        assert!(
            screen
                .transcript
                .rows
                .iter()
                .any(|r| row_text(r).contains("Half a thought")),
            "a partial reasoning trace is committed, not lost, on cancel"
        );
    }

    #[test]
    fn live_reasoning_section_break_separates_paragraphs() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta("First part.".to_string()));
        screen.apply(UiEvent::AssistantReasoningSectionBreak);
        screen.apply(UiEvent::AssistantReasoningDelta("Second part.".to_string()));
        // A leading section break (before any text) is a no-op; the two parts
        // both render in the live preview once their beats release.
        settle_stream(&mut screen);
        let preview = rendered_text(&mut screen, 80, 16);
        assert!(preview.contains("First part."), "{preview}");
        assert!(preview.contains("Second part."), "{preview}");
        // On finalize the multi-paragraph trace commits as a foldable block.
        screen.apply(UiEvent::TurnComplete);
        let committed: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert!(
            committed.iter().any(|t| t.contains("First part.")),
            "{committed:?}"
        );
    }

    #[test]
    fn live_reasoning_telemetry_patches_committed_header() {
        // The thinking header committed from a live trace still receives the
        // reasoning-token telemetry at ProviderTurnCompleted.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(100);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(
            "Deliberating.".to_string(),
        ));
        screen.apply(UiEvent::AssistantTextDelta("Answer.".to_string()));
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "t1".to_string(),
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
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        let header = rendered_lines(&mut screen, 100, 18)
            .iter()
            .map(line_text)
            .find(|t| t.contains("THINKING"))
            .expect("thinking header");
        assert!(
            header.contains("↓2.4k"),
            "telemetry on committed header: {header}"
        );
    }

    // --- The living thought: the streaming thinking block (living-thought spec) ---

    /// Build a reasoning trace of `n` fixed-width tokens that each wrap to exactly
    /// one rail row at width 30 (content 22, rail text area 20): an 18-cell token
    /// fits alone, two do not — so `n` tokens render as `n` wrapped body rows.
    fn reasoning_rows(n: usize) -> String {
        (0..n)
            .map(|i| format!("ROW{i:02}xxxxxxxxxxxxx"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The THINKING header line of the current frame at width 30.
    fn thinking_header(screen: &mut Screen) -> String {
        rendered_lines(screen, 30, 30)
            .iter()
            .map(line_text)
            .find(|t| t.contains("THINKING"))
            .expect("a THINKING header row")
    }

    #[test]
    fn live_thinking_header_lights_the_lamp_and_elapsed_then_drops_on_commit() {
        // Criterion 1: streaming shows `▾ THINKING ●` + live elapsed; commit drops
        // the lamp and patches `↓tokens elapsed`.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(30);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(reasoning_rows(40)));
        settle_stream(&mut screen);
        let header = thinking_header(&mut screen);
        assert!(header.contains('\u{25be}'), "foldable arrow ▾: {header}");
        assert!(header.contains('\u{25cf}'), "lit lamp ●: {header}");
        assert!(
            header.trim_end().ends_with('s') && header.chars().any(|c| c.is_ascii_digit()),
            "live elapsed on the right rail: {header}"
        );
        assert!(
            !header.contains('\u{2193}'),
            "no fabricated ↓tokens live: {header}"
        );
        // Commit: the lamp drops, the settled telemetry (`↓tokens elapsed`) lands.
        screen.apply(UiEvent::AssistantTextDelta("Answer.".to_string()));
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "t1".to_string(),
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
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        settle_stream(&mut screen);
        let settled = thinking_header(&mut screen);
        assert!(
            !settled.contains('\u{25cf}'),
            "lamp is dark once settled: {settled}"
        );
        assert!(
            settled.contains("\u{2193}2.4k"),
            "settled telemetry: {settled}"
        );
    }

    #[test]
    fn live_thinking_body_is_a_bounded_tail_window_with_honest_elision() {
        // A 40-row live stream renders 4 tail rows + `┊ … +36 rows`. The lit
        // header lamp carries liveness; generated text does not grow a channel-
        // specific caret.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(30);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(reasoning_rows(40)));
        settle_stream(&mut screen);
        let lines: Vec<String> = rendered_lines(&mut screen, 30, 30)
            .iter()
            .map(line_text)
            .collect();
        // Exactly four rail body rows (the `┊` rail, minus the elision) render.
        let rail_rows: Vec<&String> = lines
            .iter()
            .filter(|t| t.contains('\u{250a}') && !t.contains('\u{2026}'))
            .collect();
        assert_eq!(rail_rows.len(), 4, "four tail rows: {lines:?}");
        // The honest elision names the hidden count, and it is a rail readout.
        assert!(
            lines
                .iter()
                .any(|t| t.contains('\u{250a}') && t.contains("\u{2026} +36 rows")),
            "honest elision row: {lines:?}"
        );
        // The tail is the LAST four rows; earlier rows are hidden.
        assert!(
            lines.iter().any(|t| t.contains("ROW39")),
            "tail edge: {lines:?}"
        );
        assert!(
            lines.iter().any(|t| t.contains("ROW36")),
            "tail window: {lines:?}"
        );
        assert!(
            !lines.iter().any(|t| t.contains("ROW35")),
            "earlier rows are hidden: {lines:?}"
        );
        assert!(
            lines.iter().all(|line| !line.contains('\u{258b}')),
            "model output must not use an asymmetric caret: {lines:?}"
        );
    }

    #[test]
    fn thinking_and_answer_streams_share_the_no_caret_output_grammar() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(30);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(reasoning_rows(6)));
        settle_stream(&mut screen);
        let thinking = rendered_text(&mut screen, 30, 30);
        assert!(
            !thinking.contains('\u{258b}'),
            "thinking used a model-output caret: {thinking}"
        );
        screen.apply(UiEvent::AssistantTextDelta(
            "Answer arriving now.".to_string(),
        ));
        settle_stream(&mut screen);
        let answer = rendered_text(&mut screen, 30, 30);
        assert!(
            !answer.contains('\u{258b}'),
            "assistant answer used a model-output caret: {answer}"
        );
    }

    #[test]
    fn short_live_thinking_shows_whole_and_is_not_foldable() {
        // A 3-row live stream renders whole, no elision, no disclosure arrow,
        // and it does not participate in
        // ctrl+o (no no-op toggle).
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(30);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(reasoning_rows(3)));
        settle_stream(&mut screen);
        let lines: Vec<String> = rendered_lines(&mut screen, 30, 30)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            lines.iter().any(|t| t.contains("ROW00")),
            "whole trace: {lines:?}"
        );
        assert!(
            lines.iter().any(|t| t.contains("ROW02")),
            "whole trace: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|t| t.contains("rows") && t.contains('\u{2026}')),
            "no elision for a short trace: {lines:?}"
        );
        let header = thinking_header(&mut screen);
        assert!(
            header.contains('\u{25cf}'),
            "lamp still lit on a short trace: {header}"
        );
        assert!(
            !header.contains('\u{25be}') && !header.contains('\u{25b8}'),
            "a ≤4-row live trace has no disclosure arrow: {header}"
        );
        // Nothing hidden ⇒ ctrl+o offers no no-op toggle.
        assert!(
            !screen.toggle_all_panels(),
            "short live trace is not foldable"
        );
    }

    #[test]
    fn ctrl_o_toggles_live_thinking_between_tail_window_and_full_stream() {
        // Criterion 3: ctrl+o during streaming toggles tail window ⇄ full stream.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(30);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(reasoning_rows(40)));
        settle_stream(&mut screen);
        let windowed: Vec<String> = rendered_lines(&mut screen, 30, 60)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            !windowed.iter().any(|t| t.contains("ROW00")),
            "windowed: earliest row hidden: {windowed:?}"
        );
        // ctrl+o opens the full live stream: every row shows, no elision.
        assert!(screen.toggle_all_panels(), "ctrl+o toggles the live block");
        let full: Vec<String> = rendered_lines(&mut screen, 30, 60)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            full.iter().any(|t| t.contains("ROW00")),
            "full stream: {full:?}"
        );
        assert!(
            full.iter().any(|t| t.contains("ROW39")),
            "full stream: {full:?}"
        );
        assert!(
            !full
                .iter()
                .any(|t| t.contains("\u{2026} +") && t.contains("rows")),
            "no elision in the full stream: {full:?}"
        );
        assert!(
            full.iter().all(|t| !t.contains('\u{258b}')),
            "no channel-specific output caret: {full:?}"
        );
        // ctrl+o again returns to the bounded tail window.
        assert!(screen.toggle_all_panels(), "ctrl+o toggles back");
        let rewindowed: Vec<String> = rendered_lines(&mut screen, 30, 60)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            !rewindowed.iter().any(|t| t.contains("ROW00")),
            "back to the tail window: {rewindowed:?}"
        );
        assert!(
            rewindowed.iter().any(|t| t.contains("\u{2026} +36 rows")),
            "elision returns: {rewindowed:?}"
        );
    }

    #[test]
    fn reduced_motion_live_thinking_renders_the_same_bounded_window() {
        // Criterion 4: reduced motion changes no behavior here — the lamp, caret,
        // window, and elision render identically; elapsed still updates (it is
        // data). Under reduced motion arrival renders immediately (no beats).
        let mut screen = Screen::new();
        screen.set_reduced_motion(true);
        let _ = screen.wrapped_lines(30);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta(reasoning_rows(40)));
        let lines: Vec<String> = rendered_lines(&mut screen, 30, 30)
            .iter()
            .map(line_text)
            .collect();
        let rail_rows = lines
            .iter()
            .filter(|t| t.contains('\u{250a}') && !t.contains('\u{2026}'))
            .count();
        assert_eq!(
            rail_rows, 4,
            "bounded tail window under reduced motion: {lines:?}"
        );
        assert!(
            lines.iter().any(|t| t.contains("\u{2026} +36 rows")),
            "elision under reduced motion: {lines:?}"
        );
        assert!(
            lines.iter().any(|t| t.contains("ROW39"))
                && lines.iter().all(|t| !t.contains('\u{258b}')),
            "same no-caret output grammar under reduced motion: {lines:?}"
        );
        let header = thinking_header(&mut screen);
        assert!(
            header.contains('\u{25cf}'),
            "lamp under reduced motion: {header}"
        );
        assert!(
            header.trim_end().ends_with('s') && header.chars().any(|c| c.is_ascii_digit()),
            "elapsed still updates under reduced motion: {header}"
        );
    }

    #[test]
    fn redacted_reasoning_has_no_live_body_caret_or_elision() {
        // Criterion 6: redacted reasoning never streams a body — no live rows
        // beyond the placeholder, no caret, no elision, no lamp.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(30);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoning {
            text: String::new(),
            redacted: true,
        });
        let lines: Vec<String> = rendered_lines(&mut screen, 30, 20)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            lines.iter().any(|t| t.contains("withheld")),
            "placeholder: {lines:?}"
        );
        assert!(
            !lines.iter().any(|t| t.contains('\u{258b}')),
            "no live caret for redacted reasoning: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|t| t.contains('\u{2026}') && t.contains("rows")),
            "no elision for redacted reasoning: {lines:?}"
        );
        let header = lines
            .iter()
            .find(|t| t.contains("THINKING"))
            .expect("a THINKING header");
        assert!(
            !header.contains('\u{25cf}'),
            "no lamp for redacted reasoning: {header}"
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

        // Streaming frames: transient rows after committed history. Beat the
        // escapement so the paced tail is actually present to diff.
        screen.apply(UiEvent::AssistantTextDelta(
            "streaming **tail** ".to_string(),
        ));
        settle_stream(&mut screen);
        check(&mut screen, &mut incremental);
        screen.apply(UiEvent::AssistantTextDelta("grows".to_string()));
        settle_stream(&mut screen);
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
    fn assistant_reply_gets_text_column_and_blank_rows() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText("alpha beta".to_string()));
        let lines = screen.wrapped_lines(16);

        // Unmarked, on the shared text column (col 6); wrapped rows align there.
        assert_eq!(
            lines.iter().map(line_text).collect::<Vec<_>>(),
            vec![
                "      alpha".to_string(),
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
        // The user turn is the marked one (`›`); the agent replies unmarked.
        // Neither is boxed, and both bodies share one text column.
        assert!(
            rendered.iter().any(|line| line == "    \u{203a} HI"),
            "{rendered:?}"
        );
        let user_idx = rendered
            .iter()
            .position(|line| line.trim_start() == "\u{203a} HI")
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
            rendered[reply_idx].starts_with("      Hi! What"),
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
        // Program-coloured text is preserved verbatim; text the program left
        // uncoloured falls to the recessive `stdout` role, not default ink.
        let plain = span_matching(output, |span| span.content.as_ref() == " plain");
        assert_eq!(plain.style.fg, Some(stdout()));
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
            call_id: call.id,
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
    fn shell_review_renders_in_block_with_indicator() {
        // A gated SHELL call renders its review INSIDE its own tool block: the
        // `REVIEW` state, the `$ command` body, and a dim awaiting-decision
        // note on the block's footer — never a separate approval panel or
        // docked box, and never the decision keymap (that renders once, at the
        // composer, §8.5).
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolReview {
            call: call_args("bash", json!({ "command": "echo hi" })),
            allow_always: false,
            allow_project: false,
            dirty_gate: false,
            reason: None,
        });
        let rendered = rendered_text(&mut screen, 80, 14);
        assert!(rendered.contains("REVIEW"), "{rendered}");
        assert!(rendered.contains("SHELL"), "{rendered}");
        assert!(rendered.contains("$ echo hi"), "{rendered}");
        assert!(rendered.contains("awaiting decision"), "{rendered}");
        assert!(!rendered.contains("y approve"), "{rendered}");
        assert!(!rendered.contains("n deny"), "{rendered}");
        // The approval lives in the tool block: no separate APPROVAL panel.
        assert!(!rendered.contains("APPROVAL"), "{rendered}");
    }

    #[test]
    fn review_keymap_lives_only_in_the_composer_echo() {
        // The de-duplication contract: with a review pending AND the loop's
        // approval posture raised, the offered keymap renders exactly once —
        // in the composer placeholder — while the block carries the dim
        // awaiting-decision note.
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolReview {
            call: call_args("bash", json!({ "command": "echo hi" })),
            allow_always: false,
            allow_project: false,
            dirty_gate: false,
            reason: None,
        });
        screen.show_approval(false, false, false);
        let rendered = rendered_text(&mut screen, 80, 14);
        assert_eq!(
            rendered.matches("y approve").count(),
            1,
            "keymap once, at the composer: {rendered}"
        );
        assert!(rendered.contains("awaiting decision"), "{rendered}");
    }

    #[test]
    fn review_block_renders_above_composer_and_keeps_editor_visible() {
        // The review block sits in the transcript ABOVE the composer; the
        // composer body (placeholder) stays visible below it.
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolReview {
            call: call_args("bash", json!({ "command": "echo hi" })),
            allow_always: false,
            allow_project: false,
            dirty_gate: false,
            reason: None,
        });
        let lines = rendered_lines(&mut screen, 80, 16);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let review_row = texts
            .iter()
            .position(|t| t.contains("REVIEW"))
            .expect("REVIEW footer row present");
        let placeholder_row = texts
            .iter()
            .position(|t| t.contains("Give Iris a task..."))
            .expect("composer placeholder still visible");
        assert!(
            review_row < placeholder_row,
            "the review block must render above the composer: {texts:?}"
        );
    }

    #[test]
    fn review_block_wraps_at_narrow_width() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolReview {
            call: call_args(
                "bash",
                json!({
                    "command": "printf 'global:\\n'; find \"$HOME/.iris/fragments\" -maxdepth 1 -type f -name '*.md' -print 2>/dev/null",
                    "timeout": 120
                }),
            ),
            allow_always: false,
            allow_project: false,
            dirty_gate: false,
            reason: None,
        });
        let lines = rendered_lines(&mut screen, 48, 16);
        assert!(
            lines
                .iter()
                .all(|line| display_width(&line_text(line)) <= 48),
            "{lines:?}"
        );
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("$ printf 'global:"), "{rendered}");
        // Timeout is right-bound invocation metadata in the SHELL body.
        assert!(rendered.contains("timeout 120s"), "{rendered}");
        assert!(rendered.contains("awaiting decision"), "{rendered}");
    }

    #[test]
    fn empty_composer_keeps_blank_line_below_placeholder() {
        let mut screen = Screen::new();
        let lines = rendered_lines(&mut screen, 80, 13)
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
    fn approved_shell_call_folds_note_into_its_own_block_footer() {
        // A manually-approved SHELL call stays ONE block through its whole
        // lifecycle: the `approved this time` note folds into that block's own
        // footer (a muted aside), never a separate APPROVAL panel.
        let mut screen = Screen::new();
        let call = call_args("bash", json!({ "command": "echo hi" }));
        screen.apply(UiEvent::ToolReview {
            call: call.clone(),
            allow_always: false,
            allow_project: false,
            dirty_gate: false,
            reason: None,
        });
        screen.note_approval(&call, ApprovalDecision::Allow);
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(0)),
        });

        // Compact by default: expand to inspect the settled block.
        screen.toggle_all_panels();
        let rendered = rendered_text(&mut screen, 80, 16);
        assert!(rendered.contains("DONE"), "{rendered}");
        assert!(rendered.contains("$ echo hi"), "{rendered}");
        assert!(rendered.contains("approved this time"), "{rendered}");
        // One tool block, no separate APPROVAL panel, no enclosing frame. The
        // lone `└` is the command-to-output connector, not a box corner.
        assert_eq!(rendered.matches("SHELL").count(), 1, "{rendered}");
        assert!(!rendered.contains("APPROVAL"), "{rendered}");
        assert_eq!(rendered.matches('└').count(), 1, "{rendered}");
        for frame in ['┌', '┐', '┘', '│'] {
            assert!(!rendered.contains(frame), "{rendered}");
        }

        // The note is a muted footer aside, not the decision-carrying label.
        let lines = screen.wrapped_lines(80);
        let line = line_matching(&lines, |line| {
            line_text(line).contains("approved this time")
        });
        let marker = span_matching(line, |span| span.content.as_ref().contains("approved"));
        assert_eq!(marker.style, dim_style());
    }

    #[test]
    fn denied_shell_call_flips_its_block_to_denied() {
        // A refused SHELL call flips its own review block to `DENIED` in place:
        // the honest record of what was proposed and declined, one block, no
        // separate APPROVAL panel.
        let mut screen = Screen::new();
        let call = call_args("bash", json!({ "command": "echo hi" }));
        screen.apply(UiEvent::ToolReview {
            call: call.clone(),
            allow_always: false,
            allow_project: false,
            dirty_gate: false,
            reason: None,
        });
        screen.apply(UiEvent::ToolDenied(call));

        let rendered = rendered_text(&mut screen, 80, 12);
        assert!(rendered.contains("DENIED"), "{rendered}");
        assert!(rendered.contains("$ echo hi"), "{rendered}");
        assert_eq!(rendered.matches("SHELL").count(), 1, "{rendered}");
        assert!(!rendered.contains("APPROVAL"), "{rendered}");
        assert!(!rendered.contains("RUNNING"), "{rendered}");

        // The DENIED label carries the danger role (shared with ERROR).
        let lines = screen.wrapped_lines(80);
        let line = line_matching(&lines, |line| line_text(line).contains("DENIED"));
        let marker = span_matching(line, |span| span.content.as_ref().contains("DENIED"));
        assert_eq!(marker.style.fg, err_style().fg);
    }

    #[test]
    fn reviewed_command_sanitizes_ansi_in_its_block() {
        // The SHELL review body strips ANSI from the command it echoes, so an
        // escape embedded in the proposed command cannot colour or corrupt the
        // review surface — `red` renders as plain command text on the `$` row,
        // never a styled red span.
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolReview {
            call: call_args("bash", json!({ "command": "\u{1b}[31mred\u{1b}[0m" })),
            allow_always: false,
            allow_project: false,
            dirty_gate: false,
            reason: None,
        });
        let lines = rendered_lines(&mut screen, 80, 14);
        let cmd_row = line_matching(&lines, |line| line_text(line).contains("$ red"));
        assert!(
            !cmd_row
                .spans
                .iter()
                .any(|span| span.style.fg == Some(Color::Red)),
            "ANSI colour must be stripped from the command: {cmd_row:?}"
        );
        assert!(
            line_text(cmd_row).contains("red"),
            "command text preserved: {}",
            line_text(cmd_row)
        );
    }

    #[test]
    fn approved_review_adopted_by_toolstarted_drops_affordance_without_a_note() {
        // ToolStarted adopts a pending review block in place: `REVIEW` becomes
        // the running block and the decision affordance is gone. With no manual
        // approval recorded, no `approved …` note appears — the running block
        // alone is the record.
        let mut screen = Screen::new();
        let call = call_args("bash", json!({ "command": "echo hi" }));
        screen.apply(UiEvent::ToolReview {
            call: call.clone(),
            allow_always: false,
            allow_project: false,
            dirty_gate: false,
            reason: None,
        });
        screen.apply(UiEvent::ToolStarted(call));
        let rendered = rendered_text(&mut screen, 80, 14);
        assert!(rendered.contains("RUNNING"), "{rendered}");
        assert!(rendered.contains("$ echo hi"), "{rendered}");
        assert!(
            !rendered.contains("awaiting decision"),
            "indicator gone: {rendered}"
        );
        assert!(!rendered.contains("REVIEW"), "no stale REVIEW: {rendered}");
        assert!(!rendered.contains("approved this"), "no note: {rendered}");
        assert_eq!(rendered.matches("SHELL").count(), 1, "{rendered}");
    }

    #[test]
    fn edit_preview_flips_to_review_in_place() {
        // A mutation whose diff already arrived (DiffPreview) keeps that block:
        // `PREVIEW` flips to `REVIEW` in place — the diff IS the review body,
        // never a second block.
        let mut screen = Screen::new();
        let call = call_args("edit", json!({ "file_path": "src/main.rs" }));
        screen.apply(UiEvent::DiffPreview {
            call: call.clone(),
            diff: "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
        });
        screen.apply(UiEvent::ToolReview {
            call,
            allow_always: false,
            allow_project: false,
            dirty_gate: false,
            reason: None,
        });
        let rendered = rendered_text(&mut screen, 100, 20);
        assert!(rendered.contains("REVIEW"), "{rendered}");
        assert!(rendered.contains("awaiting decision"), "{rendered}");
        // The review arrives expanded: the diff body IS the review surface.
        assert!(rendered.contains("new"), "diff body kept: {rendered}");
        assert_eq!(
            rendered.matches("EDIT").count(),
            1,
            "one EDIT block: {rendered}"
        );
        assert!(
            !rendered.contains("PREVIEW"),
            "flipped in place: {rendered}"
        );
    }

    #[test]
    fn review_reason_shows_danger_toned_caution() {
        // A danger-toned caution (destructive / dirty paths /
        // unsandboxed) rides the review footer in the danger role, ahead of the
        // awaiting-decision note, so the safety fact survives the decision point.
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolReview {
            call: call_args("bash", json!({ "command": "rm -rf build" })),
            allow_always: false,
            allow_project: false,
            dirty_gate: false,
            reason: Some("destructive".to_string()),
        });
        let lines = screen.wrapped_lines(80);
        let line = line_matching(&lines, |line| line_text(line).contains("destructive"));
        let marker = span_matching(line, |span| span.content.as_ref().contains("destructive"));
        assert_eq!(marker.style.fg, err_style().fg, "danger-toned reason");
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("awaiting decision"), "{rendered}");
    }

    #[test]
    fn dirty_review_renders_paths_and_all_dirty_scope() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::ToolReview {
            call: call_args("write", json!({ "file_path": "src/main.rs" })),
            allow_always: true,
            allow_project: false,
            dirty_gate: true,
            reason: Some("Touches uncommitted user changes: src/main.rs.".to_string()),
        });
        let rendered = rendered_text(&mut screen, 140, 14);
        assert!(
            rendered.contains("Touches uncommitted user changes: src/main.rs"),
            "{rendered}"
        );
        // The dirty-scoped `always` label renders at the composer echo (the
        // keymap's one home), not on the block footer.
        assert!(
            !rendered.contains("a all dirty files (this task)"),
            "{rendered}"
        );
        screen.show_approval(true, false, true);
        let rendered = rendered_text(&mut screen, 140, 14);
        assert!(
            rendered.contains("a all dirty files (this task)"),
            "{rendered}"
        );
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
    fn a_run_of_notices_shares_one_rail_without_interior_blanks() {
        // Several notices firing back-to-back (e.g. a compaction's runtime event
        // plus the `/compact` command's own lines) coalesce onto one `┊` rail:
        // one blank above the run, one below, and NO blank between siblings.
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText("hi".to_string()));
        screen.apply(UiEvent::Notice("first".to_string()));
        screen.apply(UiEvent::Notice("second".to_string()));
        screen.apply(UiEvent::Notice("third".to_string()));
        let texts: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert_eq!(
            texts,
            vec![
                "hi".to_string(),
                String::new(),
                "┊ first".to_string(),
                "┊ second".to_string(),
                "┊ third".to_string(),
                String::new(),
            ]
        );
    }

    #[test]
    fn a_wrapped_notice_carries_the_rail_onto_continuation_rows() {
        // A long info notice wraps (instead of truncating) and re-emits the `┊`
        // rail at col 4 on every physical row, exactly like the reasoning trace.
        let mut screen = Screen::new();
        screen.apply(UiEvent::Notice(
            "Context compacted and folded a great many spent tool results across \
             the whole session history to reclaim room"
                .to_string(),
        ));
        let rail_rows: Vec<String> = screen
            .wrapped_lines(48)
            .iter()
            .map(line_text)
            .filter(|t| t.trim_start().starts_with('\u{250a}'))
            .collect();
        assert!(rail_rows.len() >= 2, "notice should wrap: {rail_rows:?}");
        for row in &rail_rows {
            assert!(row.starts_with("    \u{250a} "), "rail at col 4: {row:?}");
        }
    }

    #[test]
    fn diff_preview_keeps_hunk_location_drops_duplicate_path_headers_and_colors_changes() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::DiffPreview {
            call: call("edit"),
            diff: "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
        });
        let texts: Vec<String> = screen.transcript.rows.iter().map(row_text).collect();
        assert!(!texts.iter().any(|t| t.contains("--- a/note.txt")));
        assert!(
            texts.iter().any(|t| t.contains("@@ -1 +1 @@")),
            "the path lives in the EDIT header, but the hunk location stays visible"
        );
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
        assert_ne!(add.style.fg, Some(diff_add_bg()));
        assert_ne!(remove.style.fg, Some(diff_del_bg()));
        assert!(matches!(
            add.chrome.as_ref(),
            Some(ChromeRow::Body { bg, .. }) if *bg == Some(diff_add_bg())
        ));
        assert!(matches!(
            remove.chrome.as_ref(),
            Some(ChromeRow::Body { bg, .. }) if *bg == Some(diff_del_bg())
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
            "raw git header dropped"
        );
        assert!(
            texts.iter().any(|t| t.contains("FILE  note.txt")),
            "task diff names the file section"
        );
        assert!(
            texts.iter().any(|t| t.contains("@@ -1 +1 @@")),
            "task diff keeps the hunk location"
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
    fn multi_file_task_diff_names_every_change_section() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::TaskDiff {
            summary: vec!["2 files changed, +2/-2".to_string()],
            diff: "--- a/src/a.rs\n+++ b/src/a.rs\n@@ -10 +10 @@ fn a()\n-old_a\n+new_a\n--- a/src/b.rs\n+++ b/src/b.rs\n@@ -20 +20 @@ fn b()\n-old_b\n+new_b\n"
                .to_string(),
        });
        let rendered = rendered_text(&mut screen, 90, 30);

        let a_file = rendered.find("FILE  src/a.rs").expect("first file lane");
        let a_hunk = rendered.find("@@ -10 +10 @@ fn a()").expect("first hunk");
        let a_change = rendered.find("new_a").expect("first change");
        let b_file = rendered.find("FILE  src/b.rs").expect("second file lane");
        let b_hunk = rendered.find("@@ -20 +20 @@ fn b()").expect("second hunk");
        let b_change = rendered.find("new_b").expect("second change");
        assert!(a_file < a_hunk && a_hunk < a_change, "{rendered}");
        assert!(
            a_change < b_file && b_file < b_hunk && b_hunk < b_change,
            "{rendered}"
        );
        assert!(
            !rendered.contains("--- a/"),
            "raw headers stay out: {rendered}"
        );
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
            footer.text.starts_with("\u{25c7} PREVIEW  +1 \u{2212}1"),
            "state glyph + label then counts field: {}",
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

        let rendered = rendered_text(&mut screen, 80, 13);
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
        assert!(replay.contains("Done"), "{replay:?}");
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
        let rendered = rendered_text(&mut screen, 180, 13);

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
    fn pane_chrome_shows_the_review_posture_while_awaiting_approval() {
        // Golden frame (review-posture spec, criterion 7): composer + statusline
        // in the waiting state — the `▲ REVIEW` swap and the decision-echo
        // placeholder, all keyed on `awaiting_approval`.
        let mut screen = Screen::new();
        screen.set_footer(
            "sonnet 3.5".to_string(),
            Some("high".to_string()),
            "~/workspace/user-auth".to_string(),
        );
        screen.show_approval(true, true, false);
        let rendered = rendered_text(&mut screen, 100, 12);

        // The leading segment swaps to `▲ REVIEW`; the policy segment still
        // reads `▲ on-request` (now dimmed) and `◉ CODE` is gone.
        assert!(
            rendered.contains("▲ REVIEW ─ SONNET 3.5 HIGH ─ ▲ on-request"),
            "{rendered}"
        );
        assert!(!rendered.contains("◉ CODE"), "{rendered}");

        // The empty composer echoes the offered decision set as a dim
        // placeholder, in place of the product prompt.
        assert!(
            rendered.contains("review waiting ┊ y approve ┊ n deny ┊ a always ┊ p project"),
            "{rendered}"
        );
        assert!(!rendered.contains("Give Iris a task..."), "{rendered}");
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
        let lines = rendered_lines(&mut screen, 80, 13);
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
    fn short_session_composer_chrome_is_compact_after_transcript() {
        // Inline (ADR-0006) is scrollback-append with a COMPACT tail: a short
        // session renders a content-height document (transcript then composer
        // chrome), NOT a viewport-height frame with a blank body (issue #353).
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextEnd("Short answer.".to_string()));

        let height = 24;
        let lines = rendered_lines(&mut screen, 100, height);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let input_idx = texts
            .iter()
            .position(|line| line.contains("Give Iris a task..."))
            .expect("composer input");

        // Content-height, not padded to the viewport.
        assert!(
            lines.len() < usize::from(height),
            "inline document must be compact, not viewport-height: {texts:?}"
        );
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
    fn short_transcript_is_top_anchored_with_compact_tail_no_filler() {
        // The conversation reads top-down from the first pane row and the
        // composer chrome follows immediately -- no blank filler body padding
        // the transcript out to the bottom of the pane (issue #353).
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextEnd("Short answer.".to_string()));

        let height = 24u16;
        let lines = rendered_lines(&mut screen, 100, height);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        assert!(
            lines.len() < usize::from(height),
            "compact document must be shorter than the viewport: {texts:?}"
        );
        assert!(
            texts[0].contains("Short answer."),
            "transcript must start on the first pane row: {texts:?}"
        );
        let hairline = texts
            .iter()
            .position(|line| line.trim().chars().all(|ch| ch == '─') && line.contains('─'))
            .expect("composer hairline");
        // The transcript block ends with its own single blank row; the composer
        // hairline follows right after it -- no run of filler blanks.
        assert!(
            hairline <= 2,
            "composer chrome must sit right after the transcript, no filler body: {texts:?}"
        );
    }

    #[test]
    fn empty_launch_document_is_compact_not_full_viewport() {
        // At launch (empty transcript, no start page) the inline document is the
        // compact composer chrome only -- it does NOT span the whole pane with a
        // blank filler body (issue #353). The centered launcher full-pane layout
        // is reserved for the start page (see
        // `start_page_shows_centered_launcher_inside_the_shared_chrome`).
        let mut screen = Screen::new();
        screen.apply(UiEvent::SessionStarted);

        let height = 24u16;
        let lines = rendered_lines(&mut screen, 80, height);
        let texts: Vec<String> = lines.iter().map(line_text).collect();

        assert!(
            lines.len() < usize::from(height),
            "launch document must be compact, not viewport-height: {texts:?}"
        );
        let input_idx = texts
            .iter()
            .position(|line| line.contains("Give Iris a task..."))
            .expect("composer input");
        assert_eq!(input_idx + 4, texts.len(), "{texts:?}");
        let hairline = texts
            .iter()
            .position(|line| line.trim().chars().all(|ch| ch == '─') && line.contains('─'))
            .expect("composer hairline");
        // The composer hairline is the first row (no filler above it).
        assert_eq!(
            hairline, 0,
            "no filler body above the launch composer: {texts:?}"
        );
    }

    #[test]
    fn inline_after_session_start_renders_one_bar_no_menu_no_blank_body() {
        // Regression for issue #353. Drive the incremental terminal surface from
        // cold exactly like production (TuiUi owns one surface for the whole
        // session): start page -> dismiss -> `/session`-style notices. The
        // reconstructed surface document must be compact -- exactly one session
        // bar, no leftover launcher menu, and no full-height blank body between
        // the transcript and the composer -- and no second bar may be duplicated
        // into the surface (the prefix-doubling that scrolled a stale bar into
        // native scrollback).
        let size = Size::new(80, 40);
        let mut screen = Screen::new();
        let mut surface = TerminalSurface::new(Vec::new());
        screen.set_footer(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            "~/repo".to_string(),
        );
        screen.apply(UiEvent::SessionStarted);
        screen.show_start_page(0, true);
        render_perf_cycle(&mut screen, &mut surface, size).expect("start-page frame");
        // New session selected / first prompt submitted -> start page dismissed.
        screen.leave_start_page();
        render_perf_cycle(&mut screen, &mut surface, size).expect("dismiss frame");
        for i in 0..3 {
            screen.apply(UiEvent::Notice(format!("session notice {i}")));
            render_perf_cycle(&mut screen, &mut surface, size).expect("notice frame");
        }

        // The surface document is the reconstructed full frame (reused stable
        // prefix + rendered suffix): this is what actually reaches the terminal.
        let texts: Vec<String> = surface
            .state()
            .previous_lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect();

        // No launcher menu rows survive session start.
        for label in ["New session", "Resume session", "Settings", "Quit"] {
            assert!(
                !texts.iter().any(|line| line.contains(label)),
                "start-page menu leaked after session start ({label}): {texts:?}"
            );
        }
        // Exactly one session-bar row (cwd + context meter) -- no duplicate.
        let bar_rows = texts
            .iter()
            .filter(|line| line.contains("~/repo") && line.contains("CTX"))
            .count();
        assert_eq!(bar_rows, 1, "exactly one session bar: {texts:?}");
        // Compact: shorter than the viewport, not padded to full height.
        assert!(
            texts.len() < usize::from(size.height),
            "inline document must be compact after session start: {texts:?}"
        );
        // No run of more than two consecutive blank rows between the transcript
        // and the composer input (i.e. no full-height blank filler body).
        let input_idx = texts
            .iter()
            .position(|line| line.contains("Give Iris a task..."))
            .expect("composer input");
        let mut consecutive_blank = 0;
        let mut max_blank = 0;
        for line in &texts[..input_idx] {
            if line.trim().is_empty() {
                consecutive_blank += 1;
                max_blank = max_blank.max(consecutive_blank);
            } else {
                consecutive_blank = 0;
            }
        }
        assert!(
            max_blank <= 2,
            "no full-height blank body between transcript and composer: {texts:?}"
        );
    }

    #[test]
    fn bar_only_change_keeps_retained_transcript_rows() {
        // Regression for the review finding on issue #353: when ONLY the
        // session bar changes (context meter tick, branch switch) between
        // incremental renders, `stable_prefix` resets to 0 and the surface
        // replays nothing -- so the document must carry the FULL transcript
        // again, not just the incremental suffix, or retained rows silently
        // vanish from the frame.
        let size = Size::new(80, 40);
        let mut screen = Screen::new();
        let mut surface = TerminalSurface::new(Vec::new());
        screen.set_footer(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            "~/repo".to_string(),
        );
        screen.apply(UiEvent::SessionStarted);
        for i in 0..4 {
            screen.apply(UiEvent::Notice(format!("retained notice {i}")));
            render_perf_cycle(&mut screen, &mut surface, size).expect("notice frame");
        }
        // Bar-only change: the context meter moves, no transcript change.
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
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        render_perf_cycle(&mut screen, &mut surface, size).expect("bar-change frame");

        let texts: Vec<String> = surface
            .state()
            .previous_lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect();
        // Every retained transcript row is still in the reconstructed frame.
        for i in 0..4 {
            let needle = format!("retained notice {i}");
            assert!(
                texts.iter().any(|line| line.contains(&needle)),
                "transcript row dropped on bar-only change ({needle}): {texts:?}"
            );
        }
        // Still exactly one (updated) session bar.
        let bar_rows = texts
            .iter()
            .filter(|line| line.contains("~/repo") && line.contains("CTX"))
            .count();
        assert_eq!(bar_rows, 1, "exactly one session bar: {texts:?}");
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
        screen.show_start_page(0, true);
        screen.start_page.as_mut().expect("start page").skip_boot();

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
        // The IrisMark LED strip sits above the launcher menu, with the
        // silkscreen identity row (wordmark + rev) printed directly beneath it.
        let mark_idx = texts
            .iter()
            .position(|line| line.contains('●') && line.contains('○') && !line.contains("CTX"))
            .expect("IrisMark strip");
        let menu_idx = texts
            .iter()
            .position(|line| line.contains("New session"))
            .expect("launcher menu");
        assert!(mark_idx < menu_idx, "{texts:?}");
        assert!(
            texts[mark_idx + 1].contains("I R I S")
                && texts[mark_idx + 1].contains(env!("CARGO_PKG_VERSION")),
            "silkscreen under the strip: {texts:?}"
        );
        // All five rows, in order, with their key hints and the house idiom:
        // ◉ marker on the selected row, dotted leaders, no hairline dividers.
        assert!(texts[menu_idx].contains("◉ New session"), "{texts:?}");
        assert!(texts[menu_idx].trim_end().ends_with("ctrl-n"), "{texts:?}");
        assert!(texts[menu_idx + 1].contains("Resume session"), "{texts:?}");
        assert!(
            texts[menu_idx + 1].trim_end().ends_with("ctrl-r"),
            "{texts:?}"
        );
        assert!(texts[menu_idx + 2].contains("Tasks"), "{texts:?}");
        assert!(
            texts[menu_idx + 2].trim_end().ends_with("ctrl-t"),
            "{texts:?}"
        );
        assert!(
            texts[menu_idx + 3].trim_end().ends_with("ctrl-,"),
            "{texts:?}"
        );
        assert!(
            texts[menu_idx + 4].trim_end().ends_with("ctrl-q"),
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
    fn transcript_growth_keeps_a_compact_content_height_document() -> std::io::Result<()> {
        // Inline is scrollback-append (ADR-0006): the document is content-height
        // and GROWS as the transcript grows -- it is never padded out to the
        // viewport with a blank filler body (issue #353). An empty launch is
        // just the compact composer chrome, well short of the viewport.
        let size = Size::new(60, 16);
        let mut screen = Screen::new();
        let mut surface = TerminalSurface::new(Vec::new());
        render_perf_cycle(&mut screen, &mut surface, size)?;
        let empty_len = surface.state().previous_lines.len();
        assert!(
            empty_len < 16,
            "empty launch must be compact, not viewport-height: {empty_len}"
        );

        for i in 0..2 {
            screen.commit_user(&format!("prompt {i}"));
            screen.apply(UiEvent::AssistantText(format!("answer {i}")));
            render_perf_cycle(&mut screen, &mut surface, size)?;
        }

        assert!(
            surface.state().previous_lines.len() > empty_len,
            "the compact document must grow with the transcript"
        );
        // No blank filler body between the transcript and the composer: the
        // composer hairline follows the transcript with at most the single
        // blank row each transcript block ends with (no viewport padding run).
        let replay = strip_ansi(&surface.state().previous_lines.join("\n"));
        let rows: Vec<&str> = replay.lines().collect();
        let hairline = rows
            .iter()
            .position(|line| line.trim().chars().all(|ch| ch == '─') && line.contains('─'))
            .expect("composer hairline");
        let mut consecutive_blank = 0;
        let mut max_blank = 0;
        for line in &rows[..hairline] {
            if line.trim().is_empty() {
                consecutive_blank += 1;
                max_blank = max_blank.max(consecutive_blank);
            } else {
                consecutive_blank = 0;
            }
        }
        assert!(
            max_blank <= 2,
            "no full-height blank filler body between transcript and composer: {replay:?}"
        );
        let first = rows.first().copied().unwrap_or_default();
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
        let lines = rendered_lines(&mut screen, 120, 13);
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
    fn compaction_status_chip_lives_only_while_the_worker_runs() {
        let mut screen = Screen::new();
        screen.set_footer(
            "gpt-5.4".to_string(),
            None,
            "~/projects/iris-agent".to_string(),
        );
        let lifecycle = |state| UiEvent::CompactionLifecycle {
            job_id: "compaction_00000003".to_string(),
            state,
            covered_messages: 118,
            original_tokens_estimate: 48_200,
            message: None,
        };

        screen.apply(lifecycle(CompactionLifecycleState::Running));
        let running = line_text(&composer_statusline(&screen, 100).unwrap());
        assert!(running.contains("compacting…"), "{running}");

        screen.apply(lifecycle(CompactionLifecycleState::Ready));
        let ready = line_text(&composer_statusline(&screen, 100).unwrap());
        assert!(!ready.contains("compacting"), "{ready}");

        screen.apply(lifecycle(CompactionLifecycleState::Applied));
        let applied = line_text(&composer_statusline(&screen, 100).unwrap());
        assert!(!applied.contains("compacting"), "{applied}");
    }

    #[test]
    fn compaction_inspection_is_a_foldable_pager_panel() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::CompactionInspection {
            title: "compaction generation 3 (entry 0000000a)".to_string(),
            detail: vec![
                "origin             subagent".to_string(),
                "covered            1..9 (8 message(s))".to_string(),
            ],
            summary: "Goal: preserve NEEDLE.\nNext steps: continue.".to_string(),
        });

        let expanded = rendered_text(&mut screen, 100, 24);
        assert!(expanded.contains("COMPACTION"), "{expanded}");
        assert!(expanded.contains("Goal: preserve NEEDLE."), "{expanded}");

        screen.toggle_scrollback_focus();
        assert!(screen.toggle_selected_entry());
        let collapsed = rendered_text(&mut screen, 100, 24);
        assert!(collapsed.contains("COMPACTION"), "{collapsed}");
        assert!(!collapsed.contains("Goal: preserve NEEDLE."), "{collapsed}");
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

        let lines = rendered_lines(&mut screen, 180, 13);
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
            timing: crate::nexus::ProviderTurnTiming::sample(),
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
            timing: crate::nexus::ProviderTurnTiming::sample(),
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
            Some(300_000),
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
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        let before = session_bar(&screen, 110)
            .map(|l| line_text(&l))
            .expect("session bar");
        assert!(before.contains("CTX 150k/300k ●●●●●○○○○○"), "{before:?}");

        // A refresh with a differently-cased same model id must NOT reset the meter.
        screen.set_footer_with_context(
            "GPT-5.5".to_string(),
            None,
            Some(300_000),
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
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });

        let lines = rendered_lines(&mut screen, 100, 16);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        let working_idx = texts
            .iter()
            .position(|line| line.contains("●···") && line.contains("┊······┊"))
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
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });

        let before = rendered_text(&mut screen, 100, 16);
        assert!(!before.contains("WORKING"), "{before}");
        assert!(!before.contains("Working…"), "{before}");
        assert!(before.contains("●···"), "{before}");
        assert!(!before.contains("┊ ESC ┊"), "{before}");
        assert!(before.contains("┊······┊"), "{before}");
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
    fn user_turn_is_marked_and_agent_paragraphs_are_not() {
        // The transcript marks exactly one voice: the user's. The agent — the
        // default, dominant voice — speaks unmarked, so the `›` stays a
        // scannable "what did I ask?" anchor rather than decorating every line.
        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantTextEnd(
            "First agent paragraph.\n\nSecond agent paragraph.".to_string(),
        ));
        screen.apply(UiEvent::UserMessage("Next user message.".to_string()));

        let lines = rendered_lines(&mut screen, 100, 24);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        for needle in ["First agent paragraph.", "Second agent paragraph."] {
            let line = lines
                .iter()
                .map(line_text)
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("missing agent paragraph {needle:?}: {rendered}"));
            assert!(
                !line.trim_start().starts_with("\u{203a} "),
                "agent paragraphs stay unmarked: {rendered}"
            );
        }

        let next_user = lines
            .iter()
            .map(line_text)
            .find(|line| line.contains("Next user message."))
            .expect("next user message");
        assert!(
            next_user.trim_start().starts_with("\u{203a} "),
            "the user turn carries the `›` marker: {rendered}"
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
                    &crate::metrics::TokenFlows::default(),
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
                "●··· 1:27",
                "·●·· 1:27",
                "··●· 1:27",
                "···● 1:27",
                "··●· 1:27",
                "·●·· 1:27",
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
            &crate::metrics::TokenFlows::default(),
            0,
            80,
        ))
        .trim()
        .to_string();
        let without_interrupt = line_text(&working_indicator_line(
            WORKING_FRAMES[0],
            Duration::from_secs(87),
            false,
            &crate::metrics::TokenFlows::from(&usage),
            0,
            80,
        ))
        .trim()
        .to_string();
        let elapsed_only = line_text(&working_indicator_line(
            WORKING_FRAMES[0],
            Duration::from_secs(87),
            false,
            &crate::metrics::TokenFlows::default(),
            0,
            80,
        ))
        .trim()
        .to_string();

        assert_eq!(without_telemetry, "●··· 1:27");
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
    fn settled_explore_collapses_when_the_group_closes() {
        let mut screen = Screen::new();
        let call = call_args("read", json!({ "path": "src/lib.rs" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "l1\nl2\nl3".to_string(),
            exit_code: None,
            duration: Some(Duration::from_millis(5)),
        });
        // While the group is open the live block stays expanded.
        let rendered = rendered_text(&mut screen, 100, 16);
        assert!(
            rendered.contains("Read"),
            "open group shows its ops: {rendered}"
        );

        // The next top-level block closes the group: compact by default, the
        // settled explore collapses to header + footer.
        screen.apply(UiEvent::AssistantText("done".to_string()));
        let rendered = rendered_text(&mut screen, 100, 16);
        assert!(rendered.contains("▸ EXPLORE"), "collapsed: {rendered}");
        assert!(
            !rendered.contains("Read  "),
            "op rows unmounted when collapsed: {rendered}"
        );
    }

    #[test]
    fn user_expanded_explore_survives_the_group_close() {
        let mut screen = Screen::new();
        let call = call_args("read", json!({ "path": "src/lib.rs" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "l1".to_string(),
            exit_code: None,
            duration: Some(Duration::from_millis(5)),
        });
        // The user explicitly re-affirms the open block's expansion…
        let header = screen
            .transcript
            .rows
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
        screen.transcript.set_panel_expanded_at(header, true);
        // …so the group close honors that intent instead of collapsing.
        screen.apply(UiEvent::AssistantText("done".to_string()));
        let rendered = rendered_text(&mut screen, 100, 16);
        assert!(rendered.contains("▾ EXPLORE"), "{rendered}");
        assert!(rendered.contains("Read"), "{rendered}");
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
        // Body rows ride the block spine: a dim `┊` rail at the label column
        // (col 4), content on the shared text column (col 6).
        assert!(read_line.starts_with("    \u{250a} Read"), "{read_line:?}");
        assert!(grep_line.starts_with("    \u{250a} Grep"), "{grep_line:?}");
        let rail = display_width(header_line.trim_end());
        assert_eq!(display_width(read_line.trim_end()), rail, "{read_line:?}");
        assert_eq!(display_width(grep_line.trim_end()), rail, "{grep_line:?}");
    }

    #[test]
    fn shell_block_reproduces_the_frameless_mockup() {
        // SHELL — success: a compact history header carries the command; the
        // open posture moves that command to one bright invocation row, then a
        // `└` result connector, hairline, and measured footer. Exact rows/rails.
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
        let collapsed = rendered_text(&mut screen, 90, 12);
        assert!(
            collapsed.contains("▸ SHELL  cargo test -p context"),
            "folded history names what ran: {collapsed}"
        );

        // Expanded: the full command appears once in the body, never cramped
        // into the header as a duplicate.
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
        assert!(header.starts_with("  ▾ SHELL"), "{header:?}");
        assert!(
            !header.contains("cargo test"),
            "open header duplicated command: {header:?}"
        );
        assert!(header.trim_end().ends_with("3.2s"), "{header:?}");
        assert_eq!(display_width(header.trim_end()), rail, "{header:?}");
        // Body rides the block spine: a dim `┊` rail at the label column (col
        // 4), content on the shared text column (col 6) — one continuous left
        // edge from the header label down to the footer rule.
        assert_eq!(texts[1], "    ┊ $ cargo test -p context");
        assert_eq!(texts[2], "    ┊ └    Compiling context v0.4.1");
        assert_eq!(texts[3], "    ┊   test result: ok. 142 passed; 0 failed");
        assert_eq!(
            texts
                .iter()
                .filter(|line| line.contains("cargo test -p context"))
                .count(),
            1,
            "command renders exactly once: {texts:?}"
        );
        // Hairline rule from the footer indent to the block's right edge.
        assert!(texts[4].starts_with("    ─"), "{:?}", texts[4]);
        assert_eq!(display_width(&texts[4]), rail, "{:?}", texts[4]);
        // Footer: state glyph + label, EXIT + meta fields, diagnostics
        // right-bound at the shared rail.
        let footer = &texts[5];
        assert!(
            footer.starts_with("    \u{25c6} DONE  EXIT 0 ┊ 142 passed · 0 failed"),
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
            diag_call: None,
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
            timing: crate::nexus::ProviderTurnTiming::sample(),
        }
    }

    #[test]
    fn shell_footer_carries_measured_turn_diagnostics() {
        // Forward attribution, end-to-end: the proposing turn reports ↓output
        // and proposes a bash tool; the FOLLOWING turn that ingests the tool
        // result supplies the ↑/cache. No context cap => no ctx field.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        // Proposing turn: output 164 => ↓164.
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(turn_usage(4_000, 164, 0));
        let call = call_args("bash", json!({ "command": "cargo test" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "ok".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(120)),
        });
        // Following turn ingests the result: input 19_300 with 17_200 cache
        // reads => 2_100 fresh input processed (↑2.1k), cache 17.2k.
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t2".to_string(),
        });
        screen.apply(turn_usage(19_300, 200, 17_200));
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
        // When the following turn's whole prompt is served from cache
        // (input == cache_read), the fresh input it processed is zero: render
        // an honest `↑0`.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(turn_usage(4_000, 90, 0));
        let call = call_args("bash", json!({ "command": "cargo test" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "ok".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(120)),
        });
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t2".to_string(),
        });
        screen.apply(turn_usage(18_200, 120, 18_200));
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
            timing: crate::nexus::ProviderTurnTiming::sample(),
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
        // cache_read = 0 on the following turn is noise, not signal: no `cache`
        // field, no dangling ┊.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(turn_usage(500, 40, 0));
        let call = call_args("bash", json!({ "command": "true" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: String::new(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(5)),
        });
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t2".to_string(),
        });
        screen.apply(turn_usage(800, 60, 0));
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
        // ctx is measured on the FOLLOWING turn: the signed input-token growth
        // from the proposing turn to the ingesting turn, as a percentage of the
        // known context cap. Proposing input 87_000, following input 90_000 =>
        // 3_000 of a 300k cap => +1.0%.
        let mut screen = Screen::new();
        screen.set_footer_with_context(
            "gpt-5.5".to_string(),
            None,
            Some(300_000),
            "~/repo".to_string(),
        );
        let _ = screen.wrapped_lines(90);
        // Proposing turn.
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(turn_usage(87_000, 120, 0));
        let call = call_args("bash", json!({ "command": "ls" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: String::new(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(5)),
        });
        // Following turn ingests the result and grows the context.
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t2".to_string(),
        });
        screen.apply(turn_usage(90_000, 140, 0));
        let lines = screen.wrapped_lines(90);
        let footer = line_text(line_matching(&lines, |line| {
            line_text(line).contains("EXIT 0")
        }));
        assert!(footer.contains("ctx +1.0%"), "{footer:?}");
    }

    #[test]
    fn explore_footer_carries_measured_turn_diagnostics() {
        // The EXPLORE in-place footer rewrite must not drop the ↓ stamped by
        // the proposing turn, and the following turn must patch ↑/cache onto it.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(turn_usage(3_000, 38, 0));
        let call = call_args("read", json!({ "path": "src/context/engine.rs" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "line\nline\nline".to_string(),
            exit_code: None,
            duration: Some(Duration::from_millis(10)),
        });
        // Following turn ingests the read: input 18_200 with 16_800 cache reads
        // => 1_400 fresh input processed.
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t2".to_string(),
        });
        screen.apply(turn_usage(18_200, 120, 16_800));
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
    fn shell_footer_shows_only_received_before_following_turn() {
        // Forward attribution: immediately after the tool result (and with no
        // following provider turn yet), the footer carries only ↓ from the
        // proposing turn's output — no ↑/cache/ctx are invented.
        let mut screen = Screen::new();
        screen.set_footer_with_context(
            "gpt-5.5".to_string(),
            None,
            Some(300_000),
            "~/repo".to_string(),
        );
        let _ = screen.wrapped_lines(90);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(turn_usage(9_000, 164, 1_000));
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
        // ↓ present, from the PROPOSING turn's output; no input-side fields yet.
        assert!(footer.trim_end().ends_with("↓164"), "{footer:?}");
        assert!(!footer.contains('↑'), "{footer:?}");
        assert!(!footer.contains("cache"), "{footer:?}");
        assert!(!footer.contains("ctx"), "{footer:?}");
    }

    #[test]
    fn shell_footer_following_turn_patches_input_side_without_touching_received() {
        // The following turn patches ↑/cache/ctx onto the already-rendered
        // footer; the proposing turn's ↓ is preserved (the following turn's own
        // output must NOT overwrite it).
        let mut screen = Screen::new();
        screen.set_footer_with_context(
            "gpt-5.5".to_string(),
            None,
            Some(300_000),
            "~/repo".to_string(),
        );
        let _ = screen.wrapped_lines(90);
        // Proposing turn: input 87_000, output 164 => ↓164.
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(turn_usage(87_000, 164, 0));
        let call = call_args("bash", json!({ "command": "cargo test" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolResult {
            call,
            content: "ok".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(120)),
        });
        // Following turn: input 90_000 with 16_800 cache reads, output 999.
        // ↑ = 90_000 - 16_800 = 73_200 => 73.2k; cache 16.8k;
        // ctx = (90_000 - 87_000) / 300_000 => +1.0%. ↓ stays 164 (not 999).
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t2".to_string(),
        });
        screen.apply(turn_usage(90_000, 999, 16_800));
        let lines = screen.wrapped_lines(90);
        let footer = line_text(line_matching(&lines, |line| {
            line_text(line).contains("EXIT 0")
        }));
        assert!(
            footer
                .trim_end()
                .ends_with("↑73.2k ↓164 ┊ cache 16.8k ┊ ctx +1.0%"),
            "{footer:?}"
        );
        assert!(!footer.contains("999"), "{footer:?}");
    }

    #[test]
    fn parallel_tools_share_following_turn_input_side_numbers() {
        // Two tool calls proposed by the same turn share one following turn's
        // ↑/cache numbers; no per-call split is invented.
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(90);
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(turn_usage(1_000, 50, 0));
        let call_a = call_args_id("call_a", "bash", json!({ "command": "echo a" }));
        let call_b = call_args_id("call_b", "bash", json!({ "command": "echo b" }));
        screen.apply(UiEvent::ToolStarted(call_a.clone()));
        screen.apply(UiEvent::ToolResult {
            call: call_a,
            content: "a".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(5)),
        });
        screen.apply(UiEvent::ToolStarted(call_b.clone()));
        screen.apply(UiEvent::ToolResult {
            call: call_b,
            content: "b".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(5)),
        });
        // Following turn: input 2_000 with 500 cache reads => ↑1.5k, cache 500.
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t2".to_string(),
        });
        screen.apply(turn_usage(2_000, 77, 500));
        let lines = screen.wrapped_lines(90);
        let footers: Vec<String> = lines
            .iter()
            .map(line_text)
            .filter(|text| text.contains("EXIT 0"))
            .collect();
        assert_eq!(footers.len(), 2, "{footers:?}");
        for footer in &footers {
            assert!(
                footer.trim_end().ends_with("↑1.5k ↓50 ┊ cache 500"),
                "{footer:?}"
            );
        }
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
        // The exit status is a footer field after the state token (the green
        // `◆ DONE` glyph + label); `┊` only between sibling fields.
        assert!(
            rendered.contains("\u{25c6} DONE  EXIT 0 ┊ 142 passed · 0 failed"),
            "{rendered}"
        );
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
        // Applied: the same single EDIT block remains open, DONE, diff + footer
        // counts. Consequential evidence does not vanish on finalization.
        assert!(!screen.latest_panel_collapsed());
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
        // and a refused (DENIED) block all start the state token — the colored
        // glyph then the label — at the shared footer indent. The glyph lives in
        // the footer, never the header.
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
        // DENIED (in-block review lifecycle: a refused call flips to DENIED).
        let denied = call_args_id("call_deny", "bash", json!({ "command": "ls" }));
        screen.apply(UiEvent::ToolReview {
            call: denied.clone(),
            allow_always: false,
            allow_project: false,
            dirty_gate: false,
            reason: None,
        });
        screen.apply(UiEvent::ToolDenied(denied));

        let lines = screen.wrapped_lines(99);
        // Each family's footer opens with its state token: the colored glyph
        // then the label (proportional-prominence footer). The header never
        // carries a glyph — that is asserted in the header-only tests.
        let tokens = [("DONE", '◆'), ("PREVIEW", '◇'), ("DENIED", '■')];
        let mut columns = Vec::new();
        for line in lines.iter() {
            let text = line_text(line);
            for (label, glyph) in tokens {
                let token = format!("{glyph} {label}");
                if let Some(idx) = text.find(&token) {
                    assert!(
                        text[..idx].trim().is_empty(),
                        "the state token starts the footer content: {text:?}"
                    );
                    columns.push(display_width(&text[..idx]));
                }
            }
        }
        assert!(columns.len() >= 4, "expected a footer per family");
        assert!(
            columns.iter().all(|col| *col == columns[0]),
            "footer state tokens share one column: {columns:?}"
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
            origin: crate::nexus::CompactionOrigin::Subagent,
        });
        let rendered = rendered_text(&mut screen, 100, 12);
        assert!(
            rendered.contains("┊ Context compacted — 128k → 41k tokens via subagent"),
            "{rendered}"
        );
        // No undo keybind exists, so no undo hint is asserted into the UI.
        assert!(!rendered.contains("ctrl+r"), "{rendered}");
    }

    /// Audit F11c/F20: the route must name itself for every origin, not just
    /// one -- covers a second origin (`provider`) alongside the `subagent`
    /// case above so the transcript line is not accidentally hardcoded.
    #[test]
    fn compaction_event_names_provider_origin() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::CompactionApplied {
            compaction_id: "c2".to_string(),
            covered_from: "m1".to_string(),
            covered_to: "m5".to_string(),
            covered_messages: 5,
            original_tokens_estimate: 3_400,
            summary_tokens_estimate: 442,
            budget: 80_000,
            origin: crate::nexus::CompactionOrigin::Provider,
        });
        let rendered = rendered_text(&mut screen, 100, 12);
        assert!(
            rendered.contains("┊ Context compacted — 3.4k → 442 tokens via provider"),
            "{rendered}"
        );
    }

    /// The provider-native route reads `provider-native` in this PROSE
    /// transcript line (via `CompactionOrigin::display_label`), never the
    /// camelCase `providerNative` the machine-facing `/compaction` inspector and
    /// session log keep verbatim.
    #[test]
    fn compaction_event_names_provider_native_origin_hyphenated() {
        let mut screen = Screen::new();
        screen.apply(UiEvent::CompactionApplied {
            compaction_id: "c3".to_string(),
            covered_from: "m1".to_string(),
            covered_to: "m9".to_string(),
            covered_messages: 9,
            original_tokens_estimate: 3_400,
            summary_tokens_estimate: 442,
            budget: 80_000,
            origin: crate::nexus::CompactionOrigin::ProviderNative,
        });
        let rendered = rendered_text(&mut screen, 100, 12);
        assert!(
            rendered.contains("┊ Context compacted — 3.4k → 442 tokens via provider-native"),
            "{rendered}"
        );
        assert!(
            !rendered.contains("providerNative"),
            "prose must not leak the camelCase machine label: {rendered}"
        );
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
            timing: crate::nexus::ProviderTurnTiming::sample(),
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
        // Marked with `›` in the gutter, unboxed — the marker is the whole
        // treatment (no border, no role card).
        assert!(
            rendered.contains("    \u{203a} Add rate limiting to the login endpoint."),
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
        assert!(rendered.contains("@@ -1 +1 @@"));
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

        // The preview block flips to `DENIED` in place: one EDIT block, the
        // honest record of the refused mutation — no stale PREVIEW or RUNNING
        // panel left behind, and no duplicate block.
        let rendered = rendered_text(&mut screen, 100, 16);
        assert!(rendered.contains("DENIED"), "{rendered}");
        assert!(!rendered.contains("RUNNING"), "{rendered}");
        assert!(
            !rendered.contains("PREVIEW"),
            "flipped in place: {rendered}"
        );
        assert_eq!(
            rendered.matches("EDIT").count(),
            1,
            "one EDIT block, no duplicate: {rendered}"
        );
    }

    #[test]
    fn unsourced_composer_chrome_has_no_status_or_workspace_label() {
        let mut screen = Screen::new();
        let rendered = rendered_text(&mut screen, 80, 13);

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
        let rendered = rendered_text(&mut screen, 100, 13);

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
        let rendered = rendered_text(&mut screen, 100, 13);

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
            // Streaming path: beat the escapement so the table is in the tail.
            let mut screen = Screen::new();
            let _ = screen.wrapped_lines(width);
            screen.apply(UiEvent::AssistantTextDelta(md.to_string()));
            settle_stream(&mut screen);
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
        // Summary-only thinking is a real disclosure: closed history is one
        // header; opening it reveals the complete trace.
        assert!(collapsed.contains("THINKING"), "{collapsed}");
        assert!(collapsed.contains("▸"), "{collapsed}");
        assert!(!collapsed.contains("▾"), "{collapsed}");
        assert!(
            !collapsed.contains("First I check") && !collapsed.contains("Then the cache"),
            "closed thinking leaked its body: {collapsed}"
        );

        assert!(screen.toggle_latest_panel());
        let expanded = rendered_text(&mut screen, 80, 18);
        assert!(expanded.contains("▾"), "{expanded}");
        assert!(
            expanded.contains("First I check") && expanded.contains("Then the cache"),
            "expanded thinking omitted its trace: {expanded}"
        );
    }

    #[test]
    fn short_reasoning_without_raw_is_still_foldable() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantReasoning {
            text: "One short thought.".to_string(),
            redacted: false,
        });
        let collapsed = rendered_text(&mut screen, 80, 14);
        assert!(collapsed.contains("THINKING"), "{collapsed}");
        assert!(collapsed.contains("▸"), "{collapsed}");
        assert!(!collapsed.contains("▾"), "{collapsed}");
        assert!(
            !collapsed.contains("One short thought."),
            "closed short reasoning leaked: {collapsed}"
        );
        assert!(screen.toggle_latest_panel());
        let expanded = rendered_text(&mut screen, 80, 14);
        assert!(expanded.contains("One short thought."), "{expanded}");
    }

    #[test]
    fn summary_only_thinking_offers_a_real_expand() {
        let mut screen = Screen::new();
        let _ = screen.wrapped_lines(80);
        screen.apply(UiEvent::AssistantReasoning {
            text: "Inspect the config.\n\nThen inspect the cache.".to_string(),
            redacted: false,
        });
        let rendered = rendered_text(&mut screen, 80, 14);
        assert!(rendered.contains("THINKING"), "{rendered}");
        assert!(
            !rendered.contains("Then inspect the cache."),
            "closed summary body leaked: {rendered}"
        );
        assert!(rendered.contains("▸"), "{rendered}");
        assert!(!rendered.contains("▾"), "{rendered}");
        assert!(screen.toggle_latest_panel());
        let expanded = rendered_text(&mut screen, 80, 14);
        assert!(expanded.contains("Then inspect the cache."), "{expanded}");
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
        // The header renders as a muted disclosure (label, no box). The closed
        // state is header-only; opening it mounts body rows on the rail.
        let collapsed_lines: Vec<String> = rendered_lines(&mut screen, 80, 14)
            .into_iter()
            .map(|line| line_text(&line))
            .collect();
        let header = collapsed_lines
            .iter()
            .find(|t| t.contains("THINKING"))
            .expect("THINKING rail header");
        assert!(
            header.contains('\u{25b8}'),
            "summary-only thinking needs a disclosure arrow: {header}"
        );
        assert!(!header.contains('\u{2502}'), "no box side │: {header}");
        assert!(
            !collapsed_lines
                .iter()
                .any(|t| t.contains("Weigh the options.")),
            "closed thinking leaked body: {collapsed_lines:?}"
        );
        assert!(screen.toggle_latest_panel());
        let lines: Vec<String> = rendered_lines(&mut screen, 80, 14)
            .into_iter()
            .map(|line| line_text(&line))
            .collect();
        // The expanded summary body hangs on the muted `┊` rail, never box chrome.
        let body = lines
            .iter()
            .find(|t| t.contains("Weigh the options."))
            .expect("summary body row");
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
        // Redacted reasoning has no alternate expanded body, so it shows the
        // placeholder without a no-op disclosure affordance.
        let collapsed = rendered_text(&mut screen, 80, 14);
        assert!(collapsed.contains("THINKING"), "{collapsed}");
        assert!(!collapsed.contains("▸"), "{collapsed}");
        assert!(!collapsed.contains("▾"), "{collapsed}");
        assert!(
            collapsed.contains("withheld"),
            "redacted placeholder is visible: {collapsed}"
        );
        assert!(!screen.toggle_latest_panel());
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
    fn edit_diff_stays_expanded_after_it_is_applied() {
        // The same diff remains visible across preview -> running -> applied;
        // the mutation becoming real must not hide its evidence.
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
            !screen.latest_panel_collapsed(),
            "applied edit diff must stay expanded"
        );
        let applied = rendered_text(&mut screen, 100, 20);
        assert!(
            applied.contains("old") && applied.contains("new"),
            "{applied}"
        );
        assert!(screen.toggle_latest_panel(), "operator can still fold it");
        assert!(screen.latest_panel_collapsed());
    }

    #[test]
    fn ctrl_o_toggle_all_expands_then_collapses() {
        // Tool blocks and summary-only thinking rails share the same real fold
        // contract, so toggle-all operates on all three.
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
        assert_eq!(headers.len(), 3, "three block headers");
        assert!(
            headers
                .iter()
                .all(|&h| screen.transcript.panel_expanded_at(h) == Some(false)),
            "settled blocks arrive collapsed"
        );
        // Mixed state: expand one so not all are collapsed.
        screen.transcript.set_panel_expanded_at(headers[0], true);

        // First toggle-all: any collapsed -> expand all foldable blocks.
        assert!(screen.toggle_all_panels());
        assert!(
            screen
                .transcript
                .panel_header_rows()
                .iter()
                .all(|&h| screen.transcript.panel_expanded_at(h) == Some(true)),
            "first press expands all"
        );
        // Second toggle-all: none collapsed -> collapse all foldable blocks.
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
            timing: crate::nexus::ProviderTurnTiming::sample(),
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
        // The paste is the user's turn: marked with `›`, then wrapped as plain
        // text — never re-interpreted into a real SHELL panel.
        assert!(
            lines
                .first()
                .is_some_and(|line| line.starts_with("    \u{203a}")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.starts_with("      ┌")),
            "{lines:?}"
        );
        for line in &lines {
            assert!(
                display_width(line) <= 80,
                "user prompt row exceeds width: {line:?}"
            );
            if !line.is_empty() {
                assert!(
                    line.starts_with("      ") || line.starts_with("    \u{203a}"),
                    "{line:?}"
                );
            }
        }
    }

    #[test]
    fn repeated_resize_does_not_duplicate_composer_placeholder() -> std::io::Result<()> {
        let mut surface = TerminalSurface::new(Vec::new());
        let mut screen = Screen::new();
        screen.apply(UiEvent::SessionStarted);

        for (width, height) in [(50, 14), (32, 10), (60, 16), (32, 14)] {
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
        use crate::ui::modal::Modal;
        use crate::ui::settings_menu::{HatchTarget, SettingsPanel};

        let mut surface = TerminalSurface::new(Vec::new());
        let mut screen = Screen::new();
        screen.open_modal(Modal::Settings(Box::new(SettingsPanel::with_expanded(
            faceplate_snapshot(),
            HatchTarget::Model,
        ))));
        surface.render(Size::new(60, 22), &rendered_lines(&mut screen, 60, 22))?;
        assert!(
            surface
                .state()
                .previous_lines
                .join("\n")
                .contains("GPT 5.5")
        );

        screen.close_modal();
        let stats = surface.render(Size::new(60, 22), &rendered_lines(&mut screen, 60, 22))?;
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
        use crate::ui::modal::Modal;
        use crate::ui::settings_menu::{HatchTarget, SettingsPanel};

        // Every hatch, at every degenerate width/height: rendering must never
        // panic (the adversarial narrow-and-short pass, §6).
        for target in [
            HatchTarget::Model,
            HatchTarget::Scope,
            HatchTarget::Permissions,
            HatchTarget::Login,
        ] {
            for width in [10u16, 16, 24, 40] {
                for height in [2u16, 3, 4, 20] {
                    let mut screen = Screen::new();
                    screen.open_modal(Modal::Settings(Box::new(SettingsPanel::with_expanded(
                        faceplate_snapshot(),
                        target,
                    ))));
                    let _ = rendered_lines(&mut screen, width, height);
                }
            }
        }
    }

    #[test]
    fn model_hatch_renders_above_the_composer_on_a_tall_pane() {
        use crate::ui::modal::Modal;
        use crate::ui::settings_menu::{HatchTarget, SettingsPanel};

        let mut screen = Screen::new();
        screen.apply(UiEvent::AssistantText("prior reply".to_string()));
        screen.open_modal(Modal::Settings(Box::new(SettingsPanel::with_expanded(
            faceplate_snapshot(),
            HatchTarget::Model,
        ))));

        // Golden (a): the model hatch open on a tall pane — the ▾ marker, the
        // candidate rows, and the reasoning track, all above the composer.
        let rendered = rendered_text(&mut screen, 80, 30);
        assert!(rendered.contains("prior reply"), "{rendered}");
        assert!(rendered.contains("SETTINGS"), "masthead:\n{rendered}");
        assert!(
            rendered.contains(crate::ui::symbols::EXPANDED),
            "▾:\n{rendered}"
        );
        assert!(rendered.contains("GPT 5.5"), "{rendered}");
        assert!(rendered.contains("Sonnet 4.6"), "{rendered}");
        assert!(rendered.contains("Give Iris a task"), "{rendered}");
        let model_idx = rendered.find("GPT 5.5").expect("model row");
        let editor_idx = rendered.find("Give Iris a task").expect("composer row");
        assert!(model_idx < editor_idx, "{rendered}");
        // The old modal titles are gone.
        assert!(!rendered.contains("MODEL & REASONING"), "{rendered}");
        assert!(!rendered.contains("Model & reasoning"), "{rendered}");
    }

    #[test]
    fn scope_hatch_windows_on_a_short_pane() {
        use crate::ui::modal::Modal;
        use crate::ui::settings_menu::{HatchTarget, SettingsPanel};

        // Golden (b): the scope hatch windowed on a short pane — the masthead is
        // pinned, the house (n/N) position row prints, the composer survives.
        let mut screen = Screen::new();
        screen.open_modal(Modal::Settings(Box::new(SettingsPanel::with_expanded(
            faceplate_snapshot(),
            HatchTarget::Scope,
        ))));
        let rendered = rendered_text(&mut screen, 80, 19);
        assert!(
            rendered.contains("SETTINGS"),
            "masthead pinned:\n{rendered}"
        );
        assert!(rendered.contains("ENGINE"), "{rendered}");
        assert!(
            rendered.contains(crate::ui::symbols::EXPANDED),
            "▾:\n{rendered}"
        );
        assert!(rendered.contains('('), "position row:\n{rendered}");
        assert!(rendered.contains("Give Iris a task"), "{rendered}");
    }

    #[test]
    fn permissions_hatch_renders_a_bash_grant() {
        use crate::ui::modal::Modal;
        use crate::ui::settings_menu::{HatchTarget, SettingsPanel};

        // Golden (c): the permissions hatch with a bash grant — the per-tool
        // switches, the revoke-only bash row, and the read-only sandbox line.
        let mut snap = faceplate_snapshot();
        snap.policy.bash_exact = vec!["cargo test".to_string()];
        snap.policy.sandbox = Some("workspace-write".to_string());
        let mut screen = Screen::new();
        screen.open_modal(Modal::Settings(Box::new(SettingsPanel::with_expanded(
            snap,
            HatchTarget::Permissions,
        ))));
        let rendered = rendered_text(&mut screen, 90, 32);
        assert!(
            rendered.contains(crate::ui::symbols::EXPANDED),
            "▾:\n{rendered}"
        );
        assert!(
            rendered.contains("ask") && rendered.contains("always"),
            "{rendered}"
        );
        assert!(rendered.contains("bash: cargo test"), "{rendered}");
        assert!(rendered.contains("revoke"), "{rendered}");
        assert!(rendered.contains("workspace-write"), "sandbox:\n{rendered}");
        assert!(rendered.contains("Give Iris a task"), "{rendered}");
    }

    fn faceplate_model_choice(
        provider: crate::mimir::selection::ProviderId,
        model_id: &str,
        is_current: bool,
        is_default: bool,
    ) -> crate::ui::settings_menu::ModelChoice {
        let qualified = format!("{}/{}", provider.as_str(), model_id);
        crate::ui::settings_menu::ModelChoice {
            display: crate::mimir::model_catalog::display_name(&qualified),
            provider_label: provider.display_name().to_string(),
            levels: crate::mimir::model_capabilities::level_options(provider, model_id)
                .iter()
                .map(|option| (option.level, option.label))
                .collect(),
            provider,
            model_id: model_id.to_string(),
            is_current,
            is_default,
            qualified,
        }
    }

    fn faceplate_snapshot() -> crate::ui::settings_menu::Snapshot {
        use crate::mimir::selection::{ProviderId, ReasoningEffort};
        crate::ui::settings_menu::Snapshot {
            default_model: "openai-codex/gpt-5.5".to_string(),
            reasoning_levels: vec![
                (ReasoningEffort::Low, "low"),
                (ReasoningEffort::Medium, "medium"),
                (ReasoningEffort::High, "high"),
            ],
            reasoning: ReasoningEffort::Medium,
            catalog: vec![
                faceplate_model_choice(ProviderId::OpenAiCodex, "gpt-5.5", true, true),
                faceplate_model_choice(ProviderId::Anthropic, "claude-sonnet-4-6", false, false),
            ],
            scope_candidates: vec![
                crate::ui::settings_menu::ScopeChoice {
                    qualified: "openai-codex/gpt-5.5".to_string(),
                    provider_label: "OpenAI Codex".to_string(),
                },
                crate::ui::settings_menu::ScopeChoice {
                    qualified: "anthropic/claude-sonnet-4-6".to_string(),
                    provider_label: "Anthropic".to_string(),
                },
            ],
            scope_enabled: None,
            scope_persisted: None,
            providers: vec![
                crate::ui::settings_menu::ProviderStatus {
                    id: "openai-codex".to_string(),
                    name: "OpenAI Codex".to_string(),
                    badge: "subscription".to_string(),
                    oauth_capable: true,
                    api_key_capable: false,
                    credentialed: true,
                },
                crate::ui::settings_menu::ProviderStatus {
                    id: "anthropic".to_string(),
                    name: "Anthropic".to_string(),
                    badge: "\u{2014}".to_string(),
                    oauth_capable: true,
                    api_key_capable: true,
                    credentialed: false,
                },
            ],
            policy: crate::ui::settings_menu::PolicySnapshot::default(),
            default_approval: "auto".to_string(),
            skip_permissions: false,
            context_token_budget: 232_000,
            compaction_enabled: true,
            compaction_warn_pct: 60,
            compaction_start_pct: 72,
            compaction_hard_pct: 90,
            compaction_keep_recent_tokens: 8_000,
            compaction_hard_wait_ms: 120_000,
            compaction_reactive: true,
            compaction_worker_input: "transcript".to_string(),
            resolved_ladder: None,
            compaction_provider_native: "off".to_string(),
            compaction_summarizer: "subagent".to_string(),
            microcompaction: true,
            microcompaction_watermark: 32_000,
            compaction_aggressiveness: "conservative".to_string(),
            compaction_cache_timing: "cacheAware".to_string(),
            semantic_retain_per_path: 1,
            tool_clearing_keep_recent: 8,
            semantic_dedupe_enabled: true,
            tool_clearing_enabled: false,
            model_context_window: Some(232_000),
            prompt_cache_retention: "short".to_string(),
            web_search_backend: "off".to_string(),
            read_web_page_backend: "off".to_string(),
            searxng_url: None,
            search_timeout_ms: 30_000,
            read_timeout_ms: 30_000,
            max_search_results: 10,
            max_search_response_bytes: 200 * 1024,
            max_read_response_bytes: 200 * 1024,
            max_read_output_bytes: 200 * 1024,
            verify_command: None,
            verify_max_attempts: 3,
            theme: "terminal".to_string(),
            alt_screen: "auto".to_string(),
            scroll_speed: 3,
            reduced_motion: false,
            mutation_safety: true,
            native_jj_available: true,
            native_jj_enabled: false,
            worktree_root: None,
            pending_rows: Vec::new(),
        }
    }

    #[test]
    fn settings_panel_docks_the_whole_faceplate_on_a_tall_viewport() {
        use crate::ui::modal::Modal;
        use crate::ui::settings_menu::SettingsPanel;

        let mut screen = Screen::new();
        screen.open_modal(Modal::Settings(Box::new(SettingsPanel::new(
            faceplate_snapshot(),
        ))));
        let rendered = rendered_text(&mut screen, 100, 80);
        // Masthead silkscreen + every section printed at once.
        assert!(rendered.contains("SETTINGS"), "{rendered}");
        assert!(
            rendered.contains(&format!("iris {}", env!("CARGO_PKG_VERSION"))),
            "{rendered}"
        );
        for section in [
            "ENGINE",
            "SAFETY",
            "COMPACTION",
            "WEB",
            "CHECKS",
            "PANEL",
            "GIT",
        ] {
            assert!(rendered.contains(section), "{section} visible:\n{rendered}");
        }
        // The control archetypes: a printed switch scale, a 10-LED dial with
        // its honest value, and a `▸` port.
        assert!(rendered.contains("○ low  ◉ medium  ○ high"), "{rendered}");
        assert!(rendered.contains("●●●●●●○○○○  232k tokens"), "{rendered}");
        assert!(rendered.contains("▸ gpt-5.5 ┊ openai-codex"), "{rendered}");
        // Nothing windowed: no position row on a tall viewport.
        assert!(!rendered.contains("(1/42)"), "{rendered}");
        // The composer stays protected below the panel.
        assert!(rendered.contains("Give Iris a task"), "{rendered}");
    }

    #[test]
    fn settings_panel_windows_honestly_under_the_session_bar_on_short_viewports() {
        use crate::ui::modal::Modal;
        use crate::ui::settings_menu::SettingsPanel;

        let mut screen = Screen::new();
        screen.open_modal(Modal::Settings(Box::new(SettingsPanel::new(
            faceplate_snapshot(),
        ))));
        let rendered = rendered_text(&mut screen, 80, 20);
        // The masthead is pinned (never scrolled out, never painted under the
        // session bar) and the window scrolls with the house position row.
        assert!(rendered.contains("SETTINGS"), "{rendered}");
        assert!(rendered.contains("ENGINE"), "{rendered}");
        assert!(rendered.contains("(1/43)"), "{rendered}");
        assert!(!rendered.contains("worktree root"), "windowed:\n{rendered}");
        assert!(rendered.contains("Give Iris a task"), "{rendered}");
    }

    #[test]
    fn settings_detent_flash_settles_through_the_screen_tick() {
        use crate::ui::modal::{Modal, ModalKey};
        use crate::ui::settings_menu::SettingsPanel;

        let mut screen = Screen::new();
        screen.open_modal(Modal::Settings(Box::new(SettingsPanel::new(
            faceplate_snapshot(),
        ))));
        // Click the reasoning switch one detent right (row 1 on the panel).
        let modal = screen.modal.as_mut().expect("panel open");
        modal.handle_key(ModalKey::Down);
        modal.handle_key(ModalKey::Right);
        // The flash decays on the shared tick grid, forcing repaints until the
        // element settles — the same cadence as the statusline detents.
        assert!(screen.tick(), "first tick still settling");
        assert!(screen.tick(), "second tick settles");
        assert!(!screen.tick(), "settled: no more repaints");
    }

    #[test]
    fn open_modal_reclaims_composer_bottom_padding() {
        use crate::ui::modal::Modal;
        use crate::ui::settings_menu::{HatchTarget, SettingsPanel};

        let mut screen = Screen::new();
        screen.open_modal(Modal::Settings(Box::new(SettingsPanel::with_expanded(
            faceplate_snapshot(),
            HatchTarget::Model,
        ))));

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
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        let rendered = rendered_text(&mut screen, 120, 13);
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
        let refreshed = rendered_text(&mut screen, 120, 13);
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
            &crate::metrics::TokenFlows::default(),
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
            &crate::metrics::TokenFlows::default(),
            0,
            80,
        ));
        assert!(over_ten.contains("13s"), "{over_ten}");

        let over_minute = line_text(&working_indicator_line(
            WORKING_FRAMES[0],
            Duration::from_secs(87),
            true,
            &crate::metrics::TokenFlows::default(),
            0,
            80,
        ));
        assert!(over_minute.contains("1:27"), "{over_minute}");

        let over_hour = line_text(&working_indicator_line(
            WORKING_FRAMES[0],
            Duration::from_secs(3734),
            true,
            &crate::metrics::TokenFlows::default(),
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
            timing: crate::nexus::ProviderTurnTiming::sample(),
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
    fn turn_divider_sums_token_flows_across_the_tool_loop() {
        let usage = |input: u64, output: u64| ProviderUsage {
            provider: "anthropic".to_string(),
            model: "opus-4.8".to_string(),
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: input + output,
            cache_creation: None,
        };
        let mut screen = Screen::new();
        screen.start_turn();
        let call = call_args("bash", json!({ "command": "echo hi" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        // Provider turn 1 proposes the tool…
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: None,
            usage: Some(usage(6_800, 35)),
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        screen.apply(UiEvent::ToolResult {
            call,
            content: "hi".to_string(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(3)),
        });
        // …provider turn 2 answers. The divider reports the whole task's
        // flows (6.8k+8k sent, 35+40 received), matching its whole-task
        // elapsed — never just the last provider turn.
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_2".to_string(),
            response_id: None,
            usage: Some(usage(8_000, 40)),
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        screen.end_turn();
        let lines: Vec<String> = screen.wrapped_lines(90).iter().map(line_text).collect();
        assert!(
            lines.iter().any(|line| line.contains("↑14.8k ↓75")),
            "divider sums the task's provider turns: {lines:?}"
        );
        // Output rate over provider generation time only: two sampled turns
        // (1200ms - 300ms TTFT each) = 1.8s generating 75 tokens -> 42 tok/s.
        assert!(
            lines.iter().any(|line| line.contains("42 tok/s")),
            "divider reports the task's measured output rate: {lines:?}"
        );
    }

    #[test]
    fn session_receipt_sums_every_provider_turn_and_reports_cache_share() {
        let usage = |input: u64, output: u64, cached: u64| ProviderUsage {
            provider: "anthropic".to_string(),
            model: "opus-4.8".to_string(),
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: cached,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: input + output,
            cache_creation: None,
        };
        let mut screen = Screen::new();
        // Before any turn: no receipt — a receipt for nothing is noise.
        assert_eq!(screen.session_receipt(), None);

        // One task spanning two provider turns (a tool loop), then a second
        // task: the receipt sums ALL provider turns, unlike the per-task
        // divider, and reports the cached share of sent tokens.
        screen.start_turn();
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: None,
            usage: Some(usage(10_000, 500, 8_000)),
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_2".to_string(),
            response_id: None,
            usage: Some(usage(20_000, 1_500, 19_000)),
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        screen.end_turn();
        screen.start_turn();
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_3".to_string(),
            response_id: None,
            usage: Some(usage(30_000, 1_000, 27_000)),
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        screen.end_turn();

        let receipt = screen.session_receipt().expect("receipt after turns");
        assert!(
            receipt.starts_with(&format!("iris {} ┊ ", env!("CARGO_PKG_VERSION"))),
            "{receipt}"
        );
        assert!(receipt.contains(" ┊ 2 turns ┊ "), "{receipt}");
        assert!(receipt.contains(" ┊ ↑60k ↓3k ┊ "), "{receipt}");
        // 54k cached of 60k sent = 90%.
        assert!(receipt.contains(" ┊ cache 90%"), "{receipt}");
        // 3k output over three sampled generation windows (3 x 900ms): the
        // mean output rate over provider generation time, not wall time.
        assert!(receipt.ends_with(" ┊ 1111 tok/s"), "{receipt}");
    }

    #[test]
    fn session_receipt_counts_user_turns_not_background_compactions() {
        let mut screen = Screen::new();
        screen.start_turn();
        screen.apply(UiEvent::Notice("compacted context".to_string()));
        screen.end_background_work();
        assert_eq!(
            screen.session_receipt(),
            None,
            "compaction-only work must not print a user-turn receipt"
        );

        screen.start_turn();
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: None,
            usage: Some(ProviderUsage {
                provider: "openai".to_string(),
                model: "gpt-5.5".to_string(),
                input_tokens: 1_000,
                output_tokens: 20,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 1_020,
                cache_creation: None,
            }),
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        screen.end_turn();

        let receipt = screen.session_receipt().expect("receipt after user turn");
        assert!(receipt.contains(" ┊ 1 turn ┊ "), "{receipt}");
    }

    #[test]
    fn session_receipt_survives_a_session_swap() {
        let mut screen = Screen::new();
        screen.start_turn();
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: None,
            usage: Some(ProviderUsage {
                provider: "openai".to_string(),
                model: "gpt-5.5".to_string(),
                input_tokens: 5_000,
                output_tokens: 100,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 5_100,
                cache_creation: None,
            }),
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        screen.end_turn();

        // `/new` swaps to a fresh screen; the run meter rides across, so the
        // exit receipt still covers the whole process run.
        let meter = screen.take_session_meter();
        let mut fresh = Screen::new();
        fresh.restore_session_meter(meter);
        let receipt = fresh.session_receipt().expect("receipt after swap");
        assert!(receipt.contains("1 turn"), "{receipt}");
        assert!(receipt.contains("↑5k ↓100"), "{receipt}");
    }

    #[test]
    fn session_receipt_omits_unmeasured_fields() {
        let mut screen = Screen::new();
        // A turn that reported no usage at all (e.g. provider error): the
        // receipt still records time + turns but claims nothing unmeasured.
        screen.start_turn();
        screen.apply(UiEvent::ProviderTurnError {
            turn_id: "turn_1".to_string(),
            message: "rate limited".to_string(),
        });
        screen.end_turn();
        let receipt = screen.session_receipt().expect("receipt after a turn");
        assert!(receipt.contains(" ┊ 1 turn"), "{receipt}");
        assert!(!receipt.contains('↑'), "no token claim: {receipt}");
        assert!(!receipt.contains("cache"), "no cache claim: {receipt}");
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
        let line = line_text(&turn_divider_line(
            Some(Duration::from_secs(16)),
            &crate::metrics::TokenFlows::default(),
            &crate::metrics::TimingStats::default(),
            60,
        ));

        assert!(line.contains("── 16s ─"), "{line}");
        assert!(!line.contains('┊'), "{line}");
        assert_eq!(display_width(&line), 60);
    }

    #[test]
    fn turn_divider_elapsed_aligns_with_working_indicator_elapsed() {
        let divider = line_text(&inset_rule_line(
            90,
            &turn_divider_label(
                Some(Duration::from_secs(27)),
                &crate::metrics::TokenFlows::default(),
                &crate::metrics::TimingStats::default(),
            ),
        ));
        let working = line_text(&working_indicator_line(
            WORKING_FRAMES[1],
            Duration::from_millis(700),
            true,
            &crate::metrics::TokenFlows::default(),
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
        let line = line_text(&turn_divider_line(
            None,
            &crate::metrics::TokenFlows::default(),
            &crate::metrics::TimingStats::default(),
            60,
        ));

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
        // The palette is a frameless overlay list (SlashMenu idiom): no
        // box-drawing frame anywhere.
        assert!(
            !rendered.chars().any(|c| "┌┐└┘├┤│".contains(c)),
            "no frame chars: {rendered}"
        );
        let exit = line_matching(&lines, |line| line_text(line).contains("/exit"));
        // The selected row carries the surface fill + a bold name; the
        // description stays muted — never a cyan foreground accent.
        assert!(
            exit.spans
                .iter()
                .any(|span| span.style.bg == Some(crate::ui::palette::surface())),
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
        // Descriptions align in one column across rows (match on the leading
        // words, which survive any right-edge truncation).
        assert_eq!(
            line_text(exit).find("End the session"),
            line_text(model).find("Model & reasoning")
        );
        assert!(
            model
                .spans
                .iter()
                .all(|span| span.style.bg != Some(crate::ui::palette::surface())),
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

    // --- reactive density: preview budget breathes with height (spec §2) ---

    #[test]
    fn preview_row_budget_clamps_height_over_five() {
        // Criterion 1 (the clamp): heights 20/24/40/60/120/200 → 8/8/8/12/24/24.
        // The floor 8 is the historical fixed cap (a pane ≤ 40 rows is
        // byte-identical to before); the ceiling 24 keeps a preview from
        // swallowing an ultra-tall pane.
        assert_eq!(preview_row_budget(20), 8);
        assert_eq!(preview_row_budget(24), 8);
        assert_eq!(preview_row_budget(40), 8);
        assert_eq!(preview_row_budget(60), 12);
        assert_eq!(preview_row_budget(120), 24);
        assert_eq!(preview_row_budget(200), 24);
        // Before the first frame (height 0) the budget is the floor, never zero.
        assert_eq!(preview_row_budget(0), 8);
    }

    /// Stream 30 short lines into a live SHELL tail with the pane height threaded
    /// through the real render path (`render_document` → `note_pane_height`)
    /// BEFORE the deltas, so the tail is built against `height`. Returns the
    /// rendered line texts.
    fn live_shell_tail_lines(height: u16) -> Vec<String> {
        let mut screen = Screen::new();
        screen.start_turn();
        // Prime last_height + last_width via the production compose path.
        let _ = rendered_lines(&mut screen, 80, height);
        let call = call_args("bash", json!({ "command": "seq" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        for i in 0..30 {
            screen.apply(UiEvent::ToolOutputDelta {
                call_id: call.id.clone(),
                chunk: format!("Xrow{i:02}\n"),
            });
        }
        rendered_lines(&mut screen, 80, height)
            .iter()
            .map(line_text)
            .collect()
    }

    #[test]
    fn live_preview_tail_previews_full_budget_on_a_tall_pane() {
        // Criterion 1 (row-level): a 30-line output at height 120 previews 24
        // rows + the earlier-lines elision marker.
        let lines = live_shell_tail_lines(120);
        let shown = lines.iter().filter(|l| l.contains("Xrow")).count();
        assert_eq!(shown, 24, "height 120 should preview 24 rows: {lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("earlier lines hidden")),
            "missing elision marker: {lines:?}"
        );
    }

    #[test]
    fn live_preview_tail_stays_at_the_floor_on_a_small_pane() {
        // Criterion 1 (row-level): at height 24 the same output previews 8 rows —
        // today's exact behavior (the floor is the status quo).
        let lines = live_shell_tail_lines(24);
        let shown = lines.iter().filter(|l| l.contains("Xrow")).count();
        assert_eq!(shown, 8, "height 24 should preview 8 rows: {lines:?}");
    }

    #[test]
    fn printed_preview_keeps_its_size_across_a_resize() {
        // Criterion 5: a block printed at height 24 (8 preview rows) keeps its
        // size when the pane later grows to 120 — rows are immutable in
        // scrollback, so a resize never reflows a printed block. Only the NEXT
        // block built uses the new height (proven by the tall-pane test above).
        let mut screen = Screen::new();
        screen.start_turn();
        let _ = rendered_lines(&mut screen, 80, 24);
        let call = call_args("bash", json!({ "command": "seq" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        for i in 0..30 {
            screen.apply(UiEvent::ToolOutputDelta {
                call_id: call.id.clone(),
                chunk: format!("Xrow{i:02}\n"),
            });
        }
        let before = rendered_lines(&mut screen, 80, 24)
            .iter()
            .filter(|l| line_text(l).contains("Xrow"))
            .count();
        assert_eq!(before, 8, "small pane previews the floor budget");
        // Grow the pane; with no new output the printed tail must not rebuild.
        let after = rendered_lines(&mut screen, 80, 120)
            .iter()
            .filter(|l| line_text(l).contains("Xrow"))
            .count();
        assert_eq!(
            after, 8,
            "a printed block must keep its size across a resize"
        );
    }

    // --- transcript rows use the available pane width ---

    #[test]
    fn transcript_messages_use_the_available_pane_width() {
        let mut screen = Screen::new();
        screen.start_turn();
        let _ = rendered_lines(&mut screen, 200, 40);
        screen.apply(UiEvent::UserMessage("USERWORD ".repeat(30)));
        screen.apply(UiEvent::AssistantText("ASSISTANTWORD ".repeat(24)));
        screen.apply(UiEvent::AssistantReasoning {
            text: "THINKINGWORD ".repeat(24),
            redacted: false,
        });
        assert!(screen.toggle_all_panels(), "thinking block should expand");
        let lines: Vec<String> = rendered_lines(&mut screen, 200, 40)
            .iter()
            .map(line_text)
            .collect();

        for (needle, role) in [
            ("USERWORD", "user"),
            ("ASSISTANTWORD", "assistant"),
            ("THINKINGWORD", "thinking"),
        ] {
            let widest = lines
                .iter()
                .filter(|line| line.contains(needle))
                .map(|line| display_width(line))
                .max()
                .unwrap_or_else(|| panic!("missing {role} message: {lines:?}"));
            assert!(
                widest > 96 + TEXT_COLUMN_X_PADDING,
                "{role} message stayed capped at the old prose measure: {lines:?}"
            );
            assert!(
                widest <= 200,
                "{role} message overflowed the pane: {lines:?}"
            );
        }
    }

    #[test]
    fn message_continuations_align_under_content_at_full_width() {
        let mut screen = Screen::new();
        screen.start_turn();
        let _ = rendered_lines(&mut screen, 200, 40);
        screen.apply(UiEvent::AssistantText("lorem ".repeat(40)));
        let prose: Vec<String> = rendered_lines(&mut screen, 200, 40)
            .iter()
            .map(line_text)
            .filter(|l| l.contains("lorem"))
            .collect();
        assert!(prose.len() >= 2, "paragraph should wrap: {prose:?}");
        let indent = |s: &str| s.len() - s.trim_start().len();
        let first = indent(&prose[0]);
        for row in &prose {
            assert_eq!(indent(row), first, "continuation drifted: {prose:?}");
        }
    }

    #[test]
    fn notice_uses_the_available_pane_width() {
        let mut screen = Screen::new();
        screen.start_turn();
        let _ = rendered_lines(&mut screen, 200, 40);
        screen.apply(UiEvent::Notice("NOTICEWORD ".repeat(30)));
        let notice: Vec<String> = rendered_lines(&mut screen, 200, 40)
            .iter()
            .map(line_text)
            .filter(|l| l.contains("NOTICEWORD"))
            .collect();
        let widest = notice.iter().map(|row| display_width(row)).max().unwrap();
        assert!(
            widest > 100,
            "notice stayed capped at the old measure: {notice:?}"
        );
        assert!(widest <= 200, "notice overflowed the pane: {notice:?}");
    }

    #[test]
    fn tool_output_uses_the_available_pane_width() {
        let full_width = |row: &TranscriptRow| -> bool {
            row.render(200)
                .iter()
                .any(|l| display_width(&line_text(l)) > 100)
        };

        let wide = "S".repeat(150);
        let mut screen = Screen::new();
        let _ = rendered_lines(&mut screen, 200, 40); // prime width
        screen.apply(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "cat wide" })),
            content: wide.clone(),
            exit_code: Some(0),
            duration: Some(Duration::from_millis(5)),
        });
        let shell_row = screen
            .transcript
            .rows
            .iter()
            .find(|r| r.text.contains(&wide))
            .expect("SHELL body row present");
        assert!(full_width(shell_row), "SHELL body did not keep full width");

        let added = format!("+DIFFWIDE{}", "d".repeat(120));
        let diff = format!("--- a/x\n+++ b/x\n@@ -0,0 +1 @@\n{added}\n");
        screen.apply(UiEvent::TaskDiff {
            summary: vec!["1 file changed, +1/-0".to_string()],
            diff,
        });
        let diff_row = screen
            .transcript
            .rows
            .iter()
            .find(|r| r.text.contains("DIFFWIDE"))
            .expect("diff body row present");
        assert!(full_width(diff_row), "diff row did not keep full width");
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

        assert!(
            !screen.latest_panel_collapsed(),
            "failed shell output stays open by default"
        );
        let rendered = rendered_text(&mut screen, 80, 12);
        assert!(rendered.contains("SHELL"), "{rendered}");
        assert!(rendered.contains("ERROR"), "{rendered}");
        assert!(!rendered.contains("DONE"), "{rendered}");
        assert!(rendered.contains("boom"), "{rendered}");
        assert!(rendered.contains("EXIT 1"), "{rendered}");
    }

    #[test]
    fn shell_tool_error_stays_open_and_keeps_cause_when_folded() {
        let mut screen = Screen::new();
        let call = call_args("bash", json!({ "command": "cargo build" }));
        screen.apply(UiEvent::ToolStarted(call.clone()));
        screen.apply(UiEvent::ToolOutputDelta {
            call_id: call.id.clone(),
            chunk: "compiling iris\n".to_string(),
        });
        screen.apply(UiEvent::ToolError {
            call,
            message: "linker unavailable\nfull diagnostic follows".to_string(),
        });

        assert!(
            !screen.latest_panel_collapsed(),
            "errored SHELL must keep diagnostic output open"
        );
        let open = rendered_text(&mut screen, 80, 16);
        assert!(open.contains("compiling iris"), "{open}");
        assert!(open.contains("error: linker unavailable"), "{open}");

        assert!(
            screen.toggle_latest_panel(),
            "error remains manually foldable"
        );
        let folded = rendered_text(&mut screen, 80, 16);
        assert!(folded.contains("ERROR"), "{folded}");
        assert!(folded.contains("linker unavailable"), "{folded}");
        assert!(!folded.contains("compiling iris"), "{folded}");
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
        // Body rides the block spine: a dim `┊` rail at the label column, body
        // text on the shared text column — no frame, just a soft left edge.
        assert!(rendered.contains("\u{250a} $ cargo test"), "{rendered}");
        assert!(rendered.contains("\u{250a} └ test result"), "{rendered}");
        // The exit status closes the block as a footer field, after the green
        // `◆ DONE` state token.
        assert!(
            rendered.contains("\u{25c6} DONE  EXIT 0 ┊ 142 passed · 0 failed"),
            "{rendered}"
        );
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

        let folded = rendered_text(&mut screen, 80, 12);
        assert!(folded.contains("EDIT"), "{folded}");
        assert!(folded.contains("ERROR"), "{folded}");
        assert!(!folded.contains("DONE"), "{folded}");
        assert!(
            folded.contains("patch failed"),
            "folded footer must keep the cause: {folded}"
        );
        assert!(!folded.contains("error: patch failed"), "{folded}");

        screen.toggle_all_panels();
        let expanded = rendered_text(&mut screen, 80, 12);
        assert!(expanded.contains("error: patch failed"), "{expanded}");
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
        screen.apply(UiEvent::ToolCancelled(call));
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
            call,
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
