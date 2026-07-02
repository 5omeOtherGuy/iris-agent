//! Replayable screen state, composer chrome, status rail, and working indicator rendering.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget};
use ratatui_textarea::{TextArea, WrapMode};

#[cfg(test)]
use crate::mimir::model_catalog;
use crate::nexus::{ApprovalDecision, ProviderUsage, ToolCall};
use crate::tool_display::run_target;
use crate::ui::UiEvent;
use crate::ui::modal::Modal;
use crate::ui::slash::Palette;
use crate::ui::terminal_surface::CURSOR_MARKER;

use super::component::{Component, Container, take_cursor_position};
use super::overlay::{FocusTarget, PaletteView, render_menu_lines};
use super::text::{ansi_spans, strip_ansi_for_text};
use super::transcript::{Transcript, TranscriptRender};
use super::wrap::{
    display_width, line_text, pad_line_left, push_wrapped_line, spans_width, truncate_line,
    truncate_to_width, wrap_to_width,
};
use super::{
    BOX_X_PADDING_U16, EDITOR_BOTTOM_PADDING_ROWS, EDITOR_VERTICAL_CHROME_ROWS, MAX_EDITOR_ROWS,
    MAX_MENU_ROWS, MIN_EDITOR_H, MIN_INLINE_DOCUMENT_ROWS, TEXT_COLUMN_X_PADDING, WORKING_FRAMES,
    border_style, dim_style, format_elapsed_compact, panel_style, prompt_style,
};

/// Animated turn-progress spinner. Advances only while `active`, so an idle
/// session redraws nothing on a tick (no flicker, no busy CPU). `started`
/// timestamps the turn so the status row can show elapsed time and the turn-end
/// rule can report "Worked for ...".
#[derive(Default)]
struct Spinner {
    active: bool,
    frame: usize,
    started: Option<Instant>,
    /// When set (`IRIS_REDUCED_MOTION`), the LED chase holds frame 0 instead of
    /// animating, so the working indicator is static while the turn runs.
    reduced_motion: bool,
}

struct ApprovalHint {
    target: String,
    options: &'static str,
    /// Whether the gated action is a shell command, so the action reads with a
    /// `$ ` prompt (per the `ApprovalOutput` design-system component).
    shell: bool,
    /// Whether this platform ships no kernel sandbox backend for the shell at
    /// all (non-Linux). Surfaces an honest `unsandboxed` posture at the
    /// decision point, rather than only a startup notice that scrolls away.
    ///
    /// This reflects platform capability, not per-run confinement. Its ABSENCE
    /// is NOT a guarantee that the run is confined: on Linux the marker is
    /// always false even when runtime enforcement is off (Landlock unavailable
    /// or approvals not opted in). Do not treat a missing marker as sandboxed.
    sandbox_unavailable: bool,
}

#[derive(Default)]
struct TurnDivider {
    had_work: bool,
    elapsed: Option<Duration>,
    usage: Option<ProviderUsage>,
}

impl TurnDivider {
    fn observe(&mut self, event: &UiEvent) {
        if matches!(
            event,
            UiEvent::ToolStarted(_)
                | UiEvent::ToolAutoApproved(_)
                | UiEvent::DiffPreview { .. }
                | UiEvent::ToolDenied(_)
                | UiEvent::ToolResult { .. }
                | UiEvent::ToolOutputDelta { .. }
                | UiEvent::ToolError { .. }
                | UiEvent::ToolCancelled(_)
                | UiEvent::ProviderTurnError { .. }
                | UiEvent::Notice(_)
                | UiEvent::TurnError { .. }
        ) {
            self.had_work = true;
        }
        if let UiEvent::ProviderTurnCompleted {
            usage: Some(usage), ..
        } = event
        {
            self.usage = Some(usage.clone());
        }
    }
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

    /// Advance one frame; idle ticks are a no-op, and under reduced motion the
    /// frame is held so the indicator stays static. Still reports `active` so the
    /// elapsed/telemetry readout keeps refreshing.
    fn tick(&mut self) -> bool {
        if self.active && !self.reduced_motion {
            self.frame = (self.frame + 1) % WORKING_FRAMES.len();
        }
        self.active
    }

    fn frame(&self) -> &'static str {
        WORKING_FRAMES[self.frame % WORKING_FRAMES.len()]
    }
}

/// Whether the working-indicator animation should be frozen
/// (`IRIS_REDUCED_MOTION`); read once when the screen is constructed.
fn reduced_motion() -> bool {
    crate::config::iris_flag_enabled("IRIS_REDUCED_MOTION")
}

/// Session rail metadata.
struct Footer {
    /// Model display token.
    model: String,
    /// Reasoning effort display token, when configured.
    effort: Option<String>,
    /// Context-window label sourced from the model catalog, when known.
    context: Option<String>,
    /// Working directory, home-relativized to `~` where possible.
    cwd: String,
    /// Latest provider-reported usage, if the provider surfaced it. Cleared at
    /// turn start so the working indicator's per-turn token readout resets.
    usage: Option<ProviderUsage>,
    /// Most recent total context tokens, used to drive the top-frame context
    /// meter. Unlike `usage` this persists across turns (so the meter does not
    /// drop to empty at every turn start) and is cleared only when the model or
    /// context window changes.
    context_used_tokens: Option<u64>,
}

fn content_width(width: usize) -> usize {
    width
        .saturating_sub(TEXT_COLUMN_X_PADDING.saturating_mul(2))
        .max(1)
}

pub(super) fn compact_count(value: u64) -> String {
    fn trim_decimal(text: String) -> String {
        if let Some(stripped) = text.strip_suffix(".0") {
            stripped.to_string()
        } else {
            text
        }
    }

    if value >= 1_000_000 {
        trim_decimal(format!("{:.1}", value as f64 / 1_000_000.0)) + "m"
    } else if value >= 100_000 {
        format!("{}k", value / 1_000)
    } else if value >= 1_000 {
        trim_decimal(format!("{:.1}", value as f64 / 1_000.0)) + "k"
    } else {
        value.to_string()
    }
}

fn led_frame_spans(frame: &str) -> Vec<Span<'static>> {
    frame
        .chars()
        .map(|ch| {
            let style = if ch == '●' {
                prompt_style()
            } else {
                dim_style()
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

fn working_sep() -> Span<'static> {
    Span::styled(" ┊ ", dim_style())
}

pub(super) fn working_indicator_line(
    frame: &str,
    elapsed: Duration,
    can_interrupt: bool,
    usage: Option<&ProviderUsage>,
    queued: usize,
    width: usize,
) -> Line<'static> {
    let mut spans = led_frame_spans(frame);
    spans.push(Span::raw(" "));
    spans.push(Span::styled(format_elapsed_compact(elapsed), panel_style()));
    if can_interrupt {
        spans.push(working_sep());
        spans.push(Span::styled("ESC", panel_style()));
    }
    // Surface queued steering/follow-up the user typed during the turn but the
    // loop has not injected yet, so submitted input visibly registers.
    if queued > 0 {
        spans.push(working_sep());
        let label = if queued == 1 {
            "1 queued".to_string()
        } else {
            format!("{queued} queued")
        };
        spans.push(Span::styled(label, dim_style()));
    }
    if let Some(usage) = usage {
        spans.push(working_sep());
        spans.push(Span::styled(
            format!(
                "↑{} ↓{}",
                compact_count(usage.input_tokens),
                compact_count(usage.output_tokens)
            ),
            dim_style(),
        ));
    }
    let mut line = Line::from(spans);
    truncate_line(&mut line, content_width(width));
    pad_line_left(
        &mut line,
        TEXT_COLUMN_X_PADDING.min(width.saturating_sub(1)),
    );
    truncate_line(&mut line, width.max(1));
    line
}

fn working_lines(
    frame: &str,
    elapsed: Option<Duration>,
    footer: Option<&Footer>,
    queued: usize,
    width: usize,
) -> Vec<Line<'static>> {
    vec![working_indicator_line(
        frame,
        elapsed.unwrap_or_default(),
        true,
        footer.and_then(|footer| footer.usage.as_ref()),
        queued,
        width,
    )]
}

/// The `▲ REVIEW` lead and action for a gated tool call: the review glyph (orange
/// accent) + label, then the action — prefixed with a `$ ` prompt for shell
/// commands — matching the `ApprovalOutput` design-system component. State is
/// carried by symbol + label, never color alone.
fn approval_lead_spans(hint: &ApprovalHint) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::styled(format!("{} ", crate::ui::symbols::REVIEW), prompt_style()),
        Span::styled(
            "REVIEW  ".to_string(),
            prompt_style().add_modifier(Modifier::BOLD),
        ),
    ];
    if hint.shell {
        spans.push(Span::styled("$ ", dim_style()));
    }
    spans.extend(ansi_spans(&hint.target, Style::default()));
    if hint.sandbox_unavailable {
        spans.push(Span::styled(
            format!("  {} unsandboxed", crate::ui::symbols::SEP),
            dim_style(),
        ));
    }
    spans
}

fn approval_status_line(hint: &ApprovalHint) -> Line<'static> {
    let mut spans = approval_lead_spans(hint);
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

    let mut lines = Vec::new();
    push_wrapped_line(
        &Line::from(approval_lead_spans(hint)),
        width,
        Some("  "),
        &mut lines,
    );
    push_wrapped_line(
        &Line::from(Span::styled(hint.options, dim_style())),
        width,
        Some("  "),
        &mut lines,
    );
    lines
}

/// Build a styled, empty editor for the bordered composer panel: dim
/// placeholder and a reversed block cursor the widget draws itself (no hardware
/// cursor needed). The surrounding border and hint row are painted by
/// `render_editor_chrome`.
pub(super) fn fresh_editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_wrap_mode(WrapMode::WordOrGlyph);
    editor.set_cursor_line_style(Style::default());
    editor.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    editor.set_placeholder_style(dim_style());
    editor.set_placeholder_text("Give Iris a task...");
    editor
}

pub(super) fn editor_visual_rows(editor: &TextArea<'_>, width: u16) -> u16 {
    let box_width = width
        .saturating_sub(BOX_X_PADDING_U16.saturating_mul(2))
        .max(1);
    let inner_width = usize::from(
        box_width
            .saturating_sub(composer_text_x_offset(box_width))
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
    pub(super) transcript: Transcript,
    /// Multiline editor buffer (undo/redo, kill-ring, word-nav) owned by
    /// `ratatui-textarea`; the loop drives it from Iris's own keymap.
    pub(crate) editor: TextArea<'static>,
    /// Slash-command palette selection state, synced after every edit.
    pub(crate) palette: Palette,
    spinner: Spinner,
    turn_divider: TurnDivider,
    /// Short status-row hint while a gated tool awaits the user's decision.
    approval_hint: Option<ApprovalHint>,
    /// Sourced global status chrome (model / effort / cwd). The loop refreshes
    /// it from the live model selection; `None` falls back to the composer hint
    /// (e.g. before a provider is selected).
    footer: Option<Footer>,
    /// The active picker/dialog, when one is open. While present it renders
    /// above the editor and the loop routes keys to it instead of the editor.
    pub(crate) modal: Option<Modal>,
    /// Count of mid-run messages the user has queued (steering + follow-up) that
    /// the loop has not yet injected. Surfaced on the working indicator so the
    /// user sees their queued input register before it is injected. Reset at
    /// each turn boundary.
    queued: usize,
    /// Whether the last rendered document padded blank rows above a short
    /// transcript (`MIN_INLINE_DOCUMENT_ROWS`). While that padding is on the
    /// surface, its retained lines do not start with transcript content, so the
    /// first un-padded frame must not reuse a stable prefix computed from
    /// transcript rows alone (see [`render_document_inner`]).
    padded_frame: bool,
    /// Whether the terminal (pane) reports itself focused. Terminals without
    /// focus reporting never send focus events, so this stays true. While
    /// unfocused the spinner holds its frame and requests no tick redraws, so N
    /// backgrounded Iris panes in a tmux session do not each animate at 10Hz;
    /// event-driven redraws (streaming, tool output) continue as normal.
    terminal_focused: bool,
}

impl Screen {
    pub(crate) fn new() -> Self {
        Self {
            transcript: Transcript::default(),
            editor: fresh_editor(),
            palette: Palette::default(),
            spinner: Spinner {
                reduced_motion: reduced_motion(),
                ..Spinner::default()
            },
            turn_divider: TurnDivider::default(),
            approval_hint: None,
            footer: None,
            modal: None,
            queued: 0,
            padded_frame: false,
            terminal_focused: true,
        }
    }

    /// Record the terminal's focus state (crossterm `FocusGained`/`FocusLost`).
    /// Returns whether the state changed, so the loop redraws once on regain
    /// (catching the animation up) and never redraws on repeated reports.
    pub(crate) fn set_terminal_focused(&mut self, focused: bool) -> bool {
        let changed = self.terminal_focused != focused;
        self.terminal_focused = focused;
        changed
    }

    /// Set the count of queued (not-yet-injected) steering/follow-up messages
    /// shown on the working indicator. The loop refreshes it from the live queue
    /// whenever input is enqueued or a queued message is injected.
    pub(crate) fn set_queued(&mut self, queued: usize) {
        self.queued = queued;
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

    /// Which layer currently owns keyboard input. Single source of truth for
    /// input routing (`tui_loop.rs`) and docked-overlay selection
    /// (`render_editor_chrome`); precedence is Editor < Palette < Modal,
    /// mirroring pi-mono's overlay focus stack.
    pub(crate) fn focus(&self) -> FocusTarget {
        self.focus_for(&self.editor_text())
    }

    /// [`Screen::focus`] given a precomputed editor snapshot, so hot callers that
    /// already hold the input text do not re-`join` the editor buffer.
    pub(crate) fn focus_for(&self, input: &str) -> FocusTarget {
        if self.modal.is_some() {
            FocusTarget::Modal
        } else if self.palette.is_active(input) {
            FocusTarget::Palette
        } else {
            FocusTarget::Editor
        }
    }

    /// Whether the composer editor currently owns input focus, i.e. the user can
    /// type into it. False while a turn runs, a modal/picker is open, or a tool
    /// is awaiting approval. Drives whether a hardware-cursor (IME) marker is
    /// emitted at the editor cursor.
    fn composer_focused(&self) -> bool {
        !self.spinner.active && self.modal.is_none() && self.approval_hint.is_none()
    }

    // --- transcript ---

    /// Apply one semantic event to the transcript.
    pub(crate) fn apply(&mut self, event: UiEvent) {
        if self.spinner.active {
            self.turn_divider.observe(&event);
        }
        if let UiEvent::ProviderTurnCompleted {
            usage: Some(usage), ..
        } = &event
            && let Some(footer) = &mut self.footer
        {
            // `total_tokens` (prompt + completion) is the full conversation size
            // after this turn, which matches what the harness measures for
            // auto-compaction (`context_tokens` = sum of all message estimates).
            // `input_tokens` alone would omit the latest response and under-report
            // fullness relative to the compaction trigger, so the meter uses the
            // total.
            footer.context_used_tokens = Some(usage.total_tokens);
            footer.usage = Some(usage.clone());
        }
        // `UiEvent::UserMessage` (a mid-run injected steering/follow-up message)
        // is committed as a user row inside `transcript.apply`, so order matches
        // provider context; the initial prompt is committed by the session
        // driver via `commit_user`.
        self.transcript.apply(event);
    }

    /// Commit a submitted prompt into the transcript as a user line.
    pub(crate) fn commit_user(&mut self, text: &str) {
        self.transcript.commit_user(text);
    }

    /// Render all transcript rows plus any in-flight stream, wrapped to `width`.
    /// Finalized history is intentionally retained here; the terminal surface
    /// owns append/diff/full-replay decisions instead of draining UI state.
    pub(super) fn wrapped_lines(&mut self, width: u16) -> TranscriptRender {
        self.transcript.render(width)
    }

    pub(super) fn wrapped_lines_incremental(&mut self, width: u16) -> TranscriptRender {
        self.transcript.render_incremental(width)
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
    #[cfg(test)]
    pub(crate) fn set_footer(&mut self, model: String, effort: Option<String>, cwd: String) {
        let (display_model, lookup_model) = model
            .split_once('/')
            .map(|(_, bare)| (bare.to_string(), model.clone()))
            .unwrap_or_else(|| {
                (
                    model.clone(),
                    format!(
                        "{}/{}",
                        crate::mimir::selection::ProviderId::DEFAULT.as_str(),
                        model
                    ),
                )
            });
        let context = model_catalog::ctx_label(&lookup_model).map(str::to_string);
        self.set_footer_with_context(display_model, effort, context, cwd);
    }

    pub(crate) fn set_footer_with_context(
        &mut self,
        model: String,
        effort: Option<String>,
        context: Option<String>,
        cwd: String,
    ) {
        let prev = self.footer.as_ref();
        // Model ids and catalog context labels are ASCII; compare case-
        // insensitively so a differently-cased model id (e.g. from a future
        // caller) does not needlessly reset the persisted context meter.
        let same_context = prev.is_some_and(|footer| {
            footer.model.eq_ignore_ascii_case(&model)
                && label_eq_ignore_case(footer.context.as_deref(), context.as_deref())
        });
        // Carry usage and the meter value across an unchanged model/context;
        // reset both when the model or context window changes so a prior model's
        // usage cannot be shown against a new context window.
        let usage = same_context
            .then(|| prev.and_then(|footer| footer.usage.clone()))
            .flatten();
        let context_used_tokens = same_context
            .then(|| prev.and_then(|footer| footer.context_used_tokens))
            .flatten();
        self.footer = Some(Footer {
            model,
            effort,
            context,
            cwd,
            usage,
            context_used_tokens,
        });
    }

    pub(crate) fn start_turn(&mut self) {
        self.spinner.start();
        self.turn_divider = TurnDivider::default();
        self.approval_hint = None;
        self.queued = 0;
        if let Some(footer) = &mut self.footer {
            footer.usage = None;
        }
    }

    pub(crate) fn end_turn(&mut self) {
        self.queued = 0;
        self.turn_divider.elapsed = self.spinner.elapsed();
        self.transcript.push_turn_divider(
            self.turn_divider.had_work,
            self.turn_divider.elapsed,
            self.turn_divider.usage.as_ref(),
        );
        self.spinner.stop();
        self.approval_hint = None;
    }

    /// Advance the spinner one frame. Returns whether anything animated (so the
    /// loop only redraws on a tick while a turn is running). While an approval is
    /// shown the spinner is hidden behind the hint, so a tick changes nothing and
    /// requests no redraw -- the loop stays CPU-idle waiting on the decision.
    /// An unfocused terminal likewise holds the frame: pure animation is not
    /// worth per-tick redraws in a pane the user is not looking at.
    pub(crate) fn tick(&mut self) -> bool {
        if self.approval_hint.is_some() || !self.terminal_focused {
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
        let shell = call.name == "bash";
        self.approval_hint = Some(ApprovalHint {
            target: run_target(call),
            options,
            shell,
            sandbox_unavailable: shell && !crate::tools::platform_can_sandbox(),
        });
    }

    pub(crate) fn record_approval(&mut self, call: &ToolCall, decision: ApprovalDecision) {
        self.transcript.record_approval(call, decision);
    }

    pub(crate) fn clear_approval(&mut self) {
        self.approval_hint = None;
    }

    pub(crate) fn toggle_latest_panel(&mut self) -> bool {
        self.transcript.toggle_latest_panel()
    }

    #[cfg(test)]
    pub(crate) fn latest_panel_collapsed(&self) -> bool {
        self.transcript.latest_panel_collapsed()
    }

    pub(super) fn working_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.spinner.active && self.approval_hint.is_none() {
            working_lines(
                self.spinner.frame(),
                self.spinner.elapsed(),
                self.footer.as_ref(),
                self.queued,
                usize::from(width),
            )
        } else {
            Vec::new()
        }
    }
}

/// A composition-root section wrapping already-materialized lines as a
/// [`Component`], so the root assembles the bottom tail through [`Container`]
/// like pi-mono's `TUI extends Container`. `render` clones the section's lines,
/// so it is used only for the viewport-bounded tail (working indicator +
/// composer chrome); the large transcript is moved into the document, never
/// wrapped here.
struct LinesSection(Vec<Line<'static>>);

impl Component for LinesSection {
    fn render(&self, _width: usize) -> Vec<Line<'static>> {
        self.0.clone()
    }

    fn render_into(&self, _width: usize, out: &mut Vec<Line<'static>>) {
        out.extend(self.0.iter().cloned());
    }
}

/// Render the full logical document for the current terminal size: all
/// transcript rows retained in Iris state, plus bottom-pinned
/// menu/status/editor chrome. The terminal surface decides how much of this
/// document can be patched and when it must be fully replayed.
pub(super) struct RenderedDocument {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) chrome_tail: usize,
    pub(super) stable_prefix: usize,
}

#[cfg(test)]
pub(super) fn render_document(screen: &mut Screen, size: Size) -> Vec<Line<'static>> {
    render_document_inner(screen, size, false).lines
}

#[cfg(test)]
pub(super) fn render_document_with_chrome_tail(
    screen: &mut Screen,
    size: Size,
) -> (Vec<Line<'static>>, usize) {
    let rendered = render_document_inner(screen, size, false);
    (rendered.lines, rendered.chrome_tail)
}

pub(super) fn render_document_with_hints(screen: &mut Screen, size: Size) -> RenderedDocument {
    render_document_inner(screen, size, true)
}

fn render_document_inner(screen: &mut Screen, size: Size, incremental: bool) -> RenderedDocument {
    if size.height == 0 || size.width < 1 {
        return RenderedDocument {
            lines: Vec::new(),
            chrome_tail: 0,
            stable_prefix: 0,
        };
    }
    let width = size.width.max(1);
    let height = size.height.max(1);
    let mut transcript = if incremental {
        screen.wrapped_lines_incremental(width)
    } else {
        screen.wrapped_lines(width)
    };
    let working = screen.working_lines(width);
    let working_block = if working.is_empty() {
        Vec::new()
    } else {
        let mut block = Vec::with_capacity(working.len() + 2);
        block.push(Line::default());
        block.extend(working);
        block.push(Line::default());
        block
    };
    let chrome = render_editor_chrome(screen, width, height);
    let chrome_len = chrome.len();
    let volatile_tail = chrome_len + working_block.len();
    let target_rows = height.min(MIN_INLINE_DOCUMENT_ROWS);
    let min_transcript_rows =
        usize::from(target_rows).saturating_sub(chrome.len() + working_block.len());
    let pad_frame = transcript.total_lines < min_transcript_rows;
    if pad_frame {
        transcript = screen.wrapped_lines(width);
        let mut padded = Vec::with_capacity(min_transcript_rows + chrome.len());
        padded.extend(
            std::iter::repeat_with(Line::default)
                .take(min_transcript_rows - transcript.lines.len()),
        );
        padded.extend(transcript.lines);
        transcript.lines = padded;
        transcript.stable_prefix = 0;
    } else if screen.padded_frame && transcript.stable_prefix > 0 {
        // The previous frame padded blank rows above the transcript, so the
        // surface's retained document does not start with transcript content
        // and the stable-prefix hint would splice padding into this frame.
        // Re-render the whole transcript once without the hint; the next frame
        // resumes incremental rendering against the clean surface state.
        transcript = screen.wrapped_lines(width);
        transcript.stable_prefix = 0;
    }
    screen.padded_frame = pad_frame;
    // The transcript is the scrolling base, moved into the document and never
    // cloned. The bottom-pinned tail -- working indicator then composer chrome
    // (which carries the docked overlays) -- is composited through the root
    // Container, mirroring pi-mono's `TUI extends Container` (`tui.ts#L265`).
    // Both tail sections are bounded by the viewport height, not the transcript
    // length, so the container's only per-frame copy is small and constant.
    let mut tail = Container::new();
    tail.add_child(Box::new(LinesSection(working_block)));
    tail.add_child(Box::new(LinesSection(chrome)));
    let stable_prefix = transcript.stable_prefix;
    let mut document = transcript.lines;
    tail.render_into(usize::from(width), &mut document);
    // Locate-and-strip any focus cursor marker before the document reaches the
    // terminal surface. The cursor only ever lives in the composer chrome, so
    // the scan is bounded to the volatile tail instead of the whole (possibly
    // long) document. No shipped component emits a marker yet (the editor draws
    // its own block cursor), so this is a no-op strip today and the real seam the
    // deferred hardware-cursor work plugs into; a real consumer would offset the
    // returned row by `tail_start`.
    let tail_start = document.len().saturating_sub(volatile_tail);
    let _ = take_cursor_position(&mut document[tail_start..]);
    RenderedDocument {
        lines: document,
        chrome_tail: volatile_tail,
        stable_prefix,
    }
}

/// Number of dots in the top-frame context meter; each dot is ~10% usage.
const CONTEXT_METER_DOTS: u64 = 10;

/// Parse a catalog context-window label (`"300k"`, `"200k"`, `"1M"`) into a
/// token count. Returns `None` for labels that are not a number with an optional
/// `k`/`m` suffix.
fn parse_context_window(label: &str) -> Option<u64> {
    let trimmed = label.trim();
    let (digits, multiplier) = match trimmed.chars().last() {
        Some('k' | 'K') => (&trimmed[..trimmed.len() - 1], 1_000.0),
        Some('m' | 'M') => (&trimmed[..trimmed.len() - 1], 1_000_000.0),
        _ => (trimmed, 1.0),
    };
    let value: f64 = digits.trim().parse().ok()?;
    if value < 0.0 {
        return None;
    }
    Some((value * multiplier) as u64)
}

/// Number of lit dots for `used`/`window` tokens: each dot is ~10% usage, the
/// last lit dot is the current edge. `0` means no usage (all dots empty).
fn context_meter_filled(used: u64, window: u64) -> u64 {
    if used == 0 || window == 0 {
        return 0;
    }
    used.min(window)
        .saturating_mul(CONTEXT_METER_DOTS)
        .div_ceil(window)
        .min(CONTEXT_METER_DOTS)
}

/// Muted filled dot for already-consumed context (before the current edge).
fn meter_used_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Render the 10-dot context meter as styled spans: muted filled dots, an orange
/// edge dot at the current usage boundary, and dim empty dots for the remainder.
fn context_meter_spans(filled: u64) -> Vec<Span<'static>> {
    (1..=CONTEXT_METER_DOTS)
        .map(|dot| {
            if filled == 0 || dot > filled {
                Span::styled(crate::ui::symbols::EMPTY.to_string(), dim_style())
            } else if dot == filled {
                Span::styled(crate::ui::symbols::RUNNING.to_string(), prompt_style())
            } else {
                Span::styled(crate::ui::symbols::RUNNING.to_string(), meter_used_style())
            }
        })
        .collect()
}

/// Build the composer statusline — the composer's top content line, under the
/// full-width hairline (`composer_hairline`):
/// `◉ CODE ─ GPT-5.5 XHIGH ─ CTX 300K ●●●○○○○○○○    ~/iris-agent ┊ git main`.
/// The mode glyph is the orange accent; `CODE` is bold; the model name is the
/// underlined model-picker button; effort and the CTX label are muted; the
/// 10-dot meter follows; the workspace `cwd ┊ git branch` right-aligns when it
/// fits. Returns `None` when there is no footer yet or even the minimum
/// content cannot fit.
pub(super) fn composer_statusline(screen: &Screen, box_width: u16) -> Option<Line<'static>> {
    let footer = screen.footer.as_ref()?;
    let width = usize::from(box_width);
    if width < 6 {
        return None;
    }

    let model = strip_ansi_for_text(&footer.model).to_uppercase();
    if model.is_empty() {
        return None;
    }
    let effort = footer
        .effort
        .as_ref()
        .map(|effort| strip_ansi_for_text(effort).to_uppercase())
        .filter(|effort| !effort.is_empty());
    let context = footer
        .context
        .as_ref()
        .map(|context| strip_ansi_for_text(context).to_uppercase())
        .filter(|context| !context.is_empty());
    let meter_filled = context
        .as_deref()
        .and_then(parse_context_window)
        .map(|window| context_meter_filled(footer.context_used_tokens.unwrap_or(0), window));

    let mode_seg = || {
        vec![
            Span::styled(format!("{} ", crate::ui::symbols::ACTIVE), prompt_style()),
            Span::styled(
                "CODE".to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]
    };
    // The model name is the model-picker button: underlined, per the spec.
    let model_span = || {
        Span::styled(
            model.clone(),
            Style::default().add_modifier(Modifier::UNDERLINED),
        )
    };
    let model_with_effort = || match &effort {
        Some(effort) => vec![
            model_span(),
            Span::styled(format!(" {effort}"), dim_style()),
        ],
        None => vec![model_span()],
    };
    let model_only = || vec![model_span()];
    let ctx_meter = |with_meter: bool| {
        context.as_ref().map(|context| {
            let mut spans = vec![Span::styled(format!("CTX {context}"), dim_style())];
            if let (true, Some(filled)) = (with_meter, meter_filled) {
                spans.push(Span::raw(" "));
                spans.extend(context_meter_spans(filled));
            }
            spans
        })
    };

    // Candidates from fullest to minimum. The drop order is monotonic and
    // matches the spec: drop effort, then the meter, then the CTX label, leaving
    // the minimum `◉ CODE ─ MODEL`. Effort never reappears once dropped.
    let mut candidates: Vec<Vec<Vec<Span<'static>>>> = Vec::new();
    match (ctx_meter(true), ctx_meter(false)) {
        (Some(with_meter), Some(without_meter)) => {
            candidates.push(vec![mode_seg(), model_with_effort(), with_meter.clone()]);
            candidates.push(vec![mode_seg(), model_only(), with_meter]);
            candidates.push(vec![mode_seg(), model_only(), without_meter]);
        }
        _ => {
            // No known context window: the fullest form is mode + model + effort.
            candidates.push(vec![mode_seg(), model_with_effort()]);
        }
    }
    candidates.push(vec![mode_seg(), model_only()]);

    let left = candidates
        .into_iter()
        .find_map(|segments| statusline_left(width, segments))?;
    let left_w = spans_width(&left);
    let mut spans = left;
    // Right-aligned quiet workspace label: `~/iris-agent ┊ git main`.
    if let Some(ws) = workspace_spans(footer, width.saturating_sub(left_w).saturating_sub(2)) {
        let ws_w = spans_width(&ws);
        let gap = width.saturating_sub(left_w).saturating_sub(ws_w);
        if gap >= 2 {
            spans.push(Span::raw(" ".repeat(gap)));
            spans.extend(ws);
        }
    }
    let mut line = Line::from(spans);
    truncate_line(&mut line, width.max(1));
    Some(line)
}

/// Assemble one statusline candidate at `width`, or `None` if its segments do
/// not fit (segments joined by dim ` ─ ` separators).
fn statusline_left(width: usize, segments: Vec<Vec<Span<'static>>>) -> Option<Vec<Span<'static>>> {
    let mut joined: Vec<Span<'static>> = Vec::new();
    for (idx, segment) in segments.into_iter().enumerate() {
        if idx > 0 {
            joined.push(Span::styled(" ─ ".to_string(), dim_style()));
        }
        joined.extend(segment);
    }
    (spans_width(&joined) <= width).then_some(joined)
}

/// The dim `cwd ┊ git branch` workspace spans, middle-truncating the cwd to
/// `max` columns. `None` when there is no cwd or no room at all.
fn workspace_spans(footer: &Footer, max: usize) -> Option<Vec<Span<'static>>> {
    let (cwd, branch) = split_cwd_branch(&strip_ansi_for_text(&footer.cwd));
    if cwd.is_empty() || max == 0 {
        return None;
    }
    let suffix = branch
        .as_ref()
        .map(|branch| format!(" ┊ git {branch}"))
        .unwrap_or_default();
    let avail = max.saturating_sub(display_width(&suffix)).max(1);
    let cwd = truncate_cwd_middle(&cwd, avail);
    if cwd.is_empty() {
        return None;
    }
    let mut spans = vec![Span::styled(cwd, dim_style())];
    if !suffix.is_empty() {
        spans.push(Span::styled(suffix, dim_style()));
    }
    Some(spans)
}

/// The composer's top edge: a full-width hairline in the border role — the one
/// rule separating the composer from the transcript (the composer has no box).
fn composer_hairline(width: usize) -> Line<'static> {
    Line::from(Span::styled("─".repeat(width.max(1)), border_style()))
}

/// Middle-ellipsis truncation that preserves the final path segment (the
/// repo/project name). Falls back to a left-ellipsized project name when even
/// `…/<project>` does not fit.
fn truncate_cwd_middle(cwd: &str, max: usize) -> String {
    if display_width(cwd) <= max {
        return cwd.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let last = cwd.rsplit('/').next().unwrap_or("");
    let tail = format!("…/{last}");
    if display_width(&tail) <= max {
        let head_budget = max - display_width(&tail);
        let head = truncate_to_width(cwd, head_budget);
        format!("{head}{tail}")
    } else {
        format!("…{}", take_last_display(last, max.saturating_sub(1)))
    }
}

/// Longest suffix of `text` whose display width is `<= max`. (`wrap` only exposes
/// a prefix variant; the project-name fallback needs the trailing characters.)
fn take_last_display(text: &str, max: usize) -> String {
    let mut tail = String::new();
    let mut used = 0usize;
    for ch in text.chars().rev() {
        let width = display_width(ch.encode_utf8(&mut [0u8; 4]));
        if used + width > max {
            break;
        }
        tail.insert(0, ch);
        used += width;
    }
    tail
}

/// Case-insensitive equality for optional ASCII labels (catalog context labels).
fn label_eq_ignore_case(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        (None, None) => true,
        _ => false,
    }
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
    editor: u16,
}

/// Allocate chrome rows. The composer is protected first: the menu yields to
/// `MIN_EDITOR_H` (hairline + statusline + spacer + one input row) before anything else
/// is squeezed. The bottom padding is preferred, not protected, so overlays can
/// reclaim it in tight viewports.
fn chrome_heights(
    height: u16,
    menu_wanted: u16,
    editor_rows: u16,
    bottom_padding_rows: u16,
) -> ChromeHeights {
    let menu = menu_wanted.min(height.saturating_sub(MIN_EDITOR_H));
    let max_editor_h = height.saturating_sub(menu).max(1);
    let wanted_editor_h = editor_rows
        .saturating_add(EDITOR_VERTICAL_CHROME_ROWS)
        .saturating_add(bottom_padding_rows);
    let editor = if max_editor_h >= MIN_EDITOR_H {
        wanted_editor_h.clamp(MIN_EDITOR_H, max_editor_h)
    } else {
        max_editor_h.max(1)
    };
    ChromeHeights { menu, editor }
}

fn composer_text_x_offset(box_width: u16) -> u16 {
    // `ratatui-textarea` paints the empty-editor cursor one cell before the
    // placeholder, so anchor the widget one cell left of the transcript text
    // column; the visible `Give Iris...` indicator then starts with messages.
    u16::try_from(TEXT_COLUMN_X_PADDING.saturating_sub(1))
        .unwrap_or(u16::MAX)
        .min(box_width.saturating_sub(1))
}

fn render_editor_chrome(screen: &mut Screen, width: u16, height: u16) -> Vec<Line<'static>> {
    let area = Rect::new(0, 0, width, height);

    let editor_rows = screen.approval_hint.as_ref().map_or_else(
        || editor_visual_rows(&screen.editor, area.width),
        |hint| {
            let box_width = area
                .width
                .saturating_sub(BOX_X_PADDING_U16.saturating_mul(2))
                .max(1);
            let inner_width = box_width
                .saturating_sub(composer_text_x_offset(box_width))
                .max(1);
            u16::try_from(approval_status_lines(hint, usize::from(inner_width)).len())
                .unwrap_or(u16::MAX)
                .clamp(1, MAX_EDITOR_ROWS)
        },
    );
    let input_text = screen.editor_text();
    // The docked menu region shows whichever overlay currently has focus, each
    // rendered through the `Component` contract. The inner render width equals
    // the inset width `render_menu_lines` paints into, so output is unchanged.
    let menu_inner_width = content_width(usize::from(area.width));
    let menu_lines: Option<Vec<Line<'static>>> = match screen.focus_for(&input_text) {
        FocusTarget::Modal => screen
            .modal
            .as_ref()
            .map(|modal| Component::render(modal, menu_inner_width)),
        FocusTarget::Palette => {
            Some(PaletteView::for_palette(&screen.palette, &input_text).render(menu_inner_width))
        }
        FocusTarget::Editor => None,
    };
    let menu_wanted = menu_lines
        .as_ref()
        .map(|lines| {
            u16::try_from(lines.len())
                .unwrap_or(u16::MAX)
                .saturating_add(2)
                .min(MAX_MENU_ROWS)
        })
        .unwrap_or(0);

    // Bottom-anchored, clamped to the fixed viewport. The composer tail is a
    // full hairline top edge, the statusline, a blank spacer, then the input
    // rows. No box, no hint row, no separate workspace label (the workspace
    // lives right-aligned in the statusline).
    // Keep one soft row under the normal composer, but do not spend an extra
    // blank row while a docked overlay or approval prompt already occupies the
    // lower viewport.
    let bottom_padding_rows = if menu_wanted == 0 && screen.approval_hint.is_none() {
        EDITOR_BOTTOM_PADDING_ROWS
    } else {
        0
    };
    let heights = chrome_heights(area.height, menu_wanted, editor_rows, bottom_padding_rows);
    let chrome_h = heights.menu.saturating_add(heights.editor);
    let chrome_area = Rect::new(0, 0, width, chrome_h.max(1));
    let chunks = Layout::vertical([
        Constraint::Length(heights.menu),
        Constraint::Length(heights.editor),
    ])
    .split(chrome_area);
    let menu_area = chunks[0];
    let editor_area = chunks[1];

    let mut buf = Buffer::empty(chrome_area);

    if heights.menu > 0
        && let Some(lines) = menu_lines
    {
        render_menu_lines(&mut buf, menu_area, lines);
    }
    // The composer column: inset two cells from the pane edge, sharing the
    // tool-panel measure.
    let box_area = Rect {
        x: editor_area.x + BOX_X_PADDING_U16.min(editor_area.width.saturating_sub(1)),
        y: editor_area.y,
        width: editor_area
            .width
            .saturating_sub(BOX_X_PADDING_U16 * 2)
            .max(1),
        height: editor_area.height,
    };
    let text_x_offset = composer_text_x_offset(box_area.width);
    let text_area = Rect {
        x: box_area.x + text_x_offset,
        y: editor_area.y + EDITOR_VERTICAL_CHROME_ROWS.min(editor_area.height.saturating_sub(1)),
        width: box_area.width.saturating_sub(text_x_offset).max(1),
        height: editor_area
            .height
            .saturating_sub(EDITOR_VERTICAL_CHROME_ROWS)
            .saturating_sub(bottom_padding_rows)
            .max(1),
    };
    // Cell of the editor's hardware-cursor (IME) marker, in buffer coordinates.
    // Only emitted when the composer owns input focus (no turn/modal/approval),
    // located by the reversed block cursor `ratatui-textarea` draws for us.
    let mut cursor_cell: Option<(u16, u16)> = None;
    if let Some(hint) = &screen.approval_hint {
        let approval_lines = approval_status_lines(hint, usize::from(text_area.width));
        Paragraph::new(Text::from(approval_lines)).render(text_area, &mut buf);
    } else {
        (&screen.editor).render(text_area, &mut buf);
        if screen.composer_focused() {
            cursor_cell = find_reversed_cell(&buf, text_area);
        }
    }
    // The composer's chrome rows: the full-width hairline top edge, then the
    // statusline, then a blank spacer before the input. Painted last so they
    // are never overwritten by the textarea/approval body at very small heights.
    if heights.editor > 0 {
        let hairline = composer_hairline(usize::from(box_area.width));
        buf.set_line(box_area.x, box_area.y, &hairline, box_area.width);
    }
    if heights.editor > 1
        && let Some(statusline) = composer_statusline(screen, box_area.width)
    {
        buf.set_line(box_area.x, box_area.y + 1, &statusline, box_area.width);
    }
    buffer_to_lines(&buf, cursor_cell)
}

/// Find the reversed block cursor `ratatui-textarea` draws, scanning only the
/// editor's text area. Returns its buffer cell `(x, y)`, used to place the
/// zero-width hardware-cursor (IME) marker.
fn find_reversed_cell(buf: &Buffer, area: Rect) -> Option<(u16, u16)> {
    for y in area.top()..area.bottom().min(buf.area.bottom()) {
        for x in area.left()..area.right().min(buf.area.right()) {
            if buf[(x, y)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
            {
                return Some((x, y));
            }
        }
    }
    None
}

fn buffer_to_lines(buf: &Buffer, cursor_cell: Option<(u16, u16)>) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for y in 0..buf.area.height {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut x = 0;
        while x < buf.area.width {
            // Inject the zero-width cursor marker as its own span immediately
            // before the cursor cell so the terminal surface can recover the
            // cursor column (it strips the marker before any terminal write).
            if cursor_cell == Some((x, y)) {
                spans.push(Span::raw(CURSOR_MARKER));
            }
            let cell = &buf[(x, y)];
            let style = cell.style();
            let symbol = cell.symbol();
            if let Some(last) = spans.last_mut()
                && last.style == style
                && last.content.as_ref() != CURSOR_MARKER
            {
                last.content.to_mut().push_str(symbol);
                x = x.saturating_add(display_width(symbol).max(1) as u16);
                continue;
            }
            spans.push(Span::styled(symbol.to_string(), style));
            x = x.saturating_add(display_width(symbol).max(1) as u16);
        }
        out.push(Line::from(spans));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        ApprovalHint, CONTEXT_METER_DOTS, Screen, Spinner, approval_status_line,
        approval_status_lines, context_meter_filled, display_width, line_text,
        parse_context_window, truncate_cwd_middle, working_lines,
    };
    use crate::ui::tui::WORKING_FRAMES;

    #[test]
    fn reduced_motion_freezes_the_working_indicator() {
        let mut animated = Spinner::default();
        animated.start();
        animated.tick();
        assert_ne!(
            animated.frame(),
            WORKING_FRAMES[0],
            "the LED chase advances by default"
        );

        let mut frozen = Spinner {
            reduced_motion: true,
            ..Spinner::default()
        };
        frozen.start();
        for _ in 0..WORKING_FRAMES.len() + 2 {
            assert!(frozen.tick(), "tick still reports the turn as active");
            assert_eq!(
                frozen.frame(),
                WORKING_FRAMES[0],
                "reduced motion holds the indicator at frame 0"
            );
        }
    }

    #[test]
    fn unfocused_terminal_holds_tick_redraws_until_refocus() {
        let mut screen = Screen::new();
        screen.start_turn();
        assert!(screen.tick(), "a focused running turn animates");

        // Losing focus (tmux pane switched away) pauses tick-driven redraws;
        // the change itself is reported once so the loop can react.
        assert!(screen.set_terminal_focused(false));
        assert!(!screen.tick(), "no animation redraws while unfocused");
        assert!(
            !screen.set_terminal_focused(false),
            "repeated focus reports are not a state change"
        );

        // Refocus reports the change (the loop redraws once to catch up) and
        // the animation resumes.
        assert!(screen.set_terminal_focused(true));
        assert!(screen.tick(), "animation resumes when refocused");
    }

    #[test]
    fn parse_context_window_handles_k_m_and_plain() {
        assert_eq!(parse_context_window("300k"), Some(300_000));
        assert_eq!(parse_context_window("300K"), Some(300_000));
        assert_eq!(parse_context_window("200k"), Some(200_000));
        assert_eq!(parse_context_window("1M"), Some(1_000_000));
        assert_eq!(parse_context_window("1m"), Some(1_000_000));
        assert_eq!(parse_context_window("4096"), Some(4_096));
        assert_eq!(parse_context_window("unknown"), None);
        assert_eq!(parse_context_window(""), None);
    }

    #[test]
    fn context_meter_filled_is_one_dot_per_ten_percent() {
        let window = 300_000;
        assert_eq!(context_meter_filled(0, window), 0);
        // Any nonzero usage lights at least one dot.
        assert_eq!(context_meter_filled(1, window), 1);
        assert_eq!(context_meter_filled(30_000, window), 1);
        assert_eq!(context_meter_filled(30_001, window), 2);
        assert_eq!(context_meter_filled(90_000, window), 3);
        assert_eq!(context_meter_filled(window, window), CONTEXT_METER_DOTS);
        // Over budget clamps to a full strip, never beyond.
        assert_eq!(context_meter_filled(window * 2, window), CONTEXT_METER_DOTS);
        // A zero/unknown window never divides by zero.
        assert_eq!(context_meter_filled(100, 0), 0);
    }

    #[test]
    fn truncate_cwd_middle_preserves_project_name() {
        let cwd = "~/projects/very/deep/nested/iris-agent";
        let out = truncate_cwd_middle(cwd, 20);
        assert!(display_width(&out) <= 20, "{out:?}");
        assert!(out.ends_with("iris-agent"), "{out:?}");
        assert!(out.contains('…'), "{out:?}");
        // Fits untouched when there is room.
        assert_eq!(truncate_cwd_middle("~/repo", 40), "~/repo");
    }

    #[test]
    fn focused_composer_emits_cursor_marker_and_running_turn_does_not() {
        use super::{Screen, render_document_with_chrome_tail};
        use crate::ui::terminal_surface::CURSOR_MARKER;
        use ratatui::layout::Size;

        let has_marker = |lines: &[ratatui::text::Line<'static>]| {
            lines.iter().any(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content.as_ref() == CURSOR_MARKER)
            })
        };

        let mut screen = Screen::new();
        let (focused, _) = render_document_with_chrome_tail(&mut screen, Size::new(80, 10));
        assert!(
            has_marker(&focused),
            "focused composer must emit the IME marker"
        );

        // While a turn runs the composer is frozen: no marker (cursor hidden).
        screen.start_turn();
        let (running, _) = render_document_with_chrome_tail(&mut screen, Size::new(80, 10));
        assert!(
            !has_marker(&running),
            "a running turn must not emit the composer cursor marker"
        );
    }

    #[test]
    fn composer_wide_glyphs_never_render_over_terminal_width() {
        use super::{Screen, render_document_with_chrome_tail};
        use crate::ui::terminal_surface::CURSOR_MARKER;
        use ratatui::layout::Size;

        for width in [12_u16, 44, 90, 120] {
            let mut screen = Screen::new();
            screen.set_editor("中🙂 wide glyphs");
            let (lines, _) = render_document_with_chrome_tail(&mut screen, Size::new(width, 14));

            for (index, line) in lines.iter().enumerate() {
                let visible = line
                    .spans
                    .iter()
                    .filter(|span| span.content.as_ref() != CURSOR_MARKER)
                    .map(|span| display_width(span.content.as_ref()))
                    .sum::<usize>();
                assert!(
                    visible <= usize::from(width),
                    "width {width}, line {index} exceeded terminal width: {visible} > {width}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn approval_and_working_lines_stay_bounded_at_tiny_widths() {
        let hint = ApprovalHint {
            target: "run an extremely long command".to_string(),
            options: "[y] once  [N] deny",
            shell: true,
            sandbox_unavailable: true,
        };
        for width in 1..=4 {
            for line in approval_status_lines(&hint, width) {
                assert!(
                    display_width(&line_text(&line)) <= width,
                    "width {width}: {line:?}"
                );
            }
            for line in working_lines(
                WORKING_FRAMES[0],
                Some(Duration::from_secs(1)),
                None,
                0,
                width,
            ) {
                assert!(
                    display_width(&line_text(&line)) <= width,
                    "width {width}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn approval_review_line_leads_with_review_and_shell_prompt() {
        // A gated shell action reads `▲ REVIEW  $ <command>  <keys>`; the `$ `
        // prompt is shell-only (ApprovalOutput design-system component).
        let shell = ApprovalHint {
            target: "echo hi".to_string(),
            options: "[y] once  [N] deny",
            shell: true,
            sandbox_unavailable: false,
        };
        let non_shell = ApprovalHint {
            target: "Write src/x.rs".to_string(),
            options: "[y] once  [N] deny",
            shell: false,
            sandbox_unavailable: false,
        };
        let shell_text = line_text(&approval_status_line(&shell));
        let non_shell_text = line_text(&approval_status_line(&non_shell));
        assert!(shell_text.contains("\u{25b2} REVIEW"), "{shell_text}");
        assert!(shell_text.contains("$ echo hi"), "{shell_text}");
        assert!(
            non_shell_text.contains("\u{25b2} REVIEW"),
            "{non_shell_text}"
        );
        assert!(
            non_shell_text.contains("Write src/x.rs"),
            "{non_shell_text}"
        );
        assert!(!non_shell_text.contains("$ "), "{non_shell_text}");
    }

    #[test]
    fn approval_review_line_marks_platform_without_sandbox() {
        // On a platform with no kernel sandbox backend the shell runs
        // unconfined; the approval prompt states that posture at the decision
        // point, in the calm dim aside, rather than only in a startup notice.
        // The marker reflects platform capability, not per-run confinement.
        let unavailable = ApprovalHint {
            target: "echo hi".to_string(),
            options: "[y] once  [N] deny",
            shell: true,
            sandbox_unavailable: true,
        };
        let has_backend = ApprovalHint {
            target: "echo hi".to_string(),
            options: "[y] once  [N] deny",
            shell: true,
            sandbox_unavailable: false,
        };
        let unavailable_text = line_text(&approval_status_line(&unavailable));
        let has_backend_text = line_text(&approval_status_line(&has_backend));
        assert!(
            unavailable_text.contains("unsandboxed"),
            "{unavailable_text}"
        );
        assert!(
            unavailable_text.contains(crate::ui::symbols::SEP),
            "{unavailable_text}"
        );
        // The posture aside is shell-only. A missing marker is not a
        // confinement guarantee, so this only asserts the text is not rendered.
        assert!(
            !has_backend_text.contains("unsandboxed"),
            "{has_backend_text}"
        );
    }
}
