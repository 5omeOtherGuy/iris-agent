//! Replayable screen state, composer chrome, status rail, and working indicator rendering.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui_textarea::{TextArea, WrapMode};

#[cfg(test)]
use crate::mimir::model_catalog;
use crate::nexus::{ApprovalDecision, ProviderUsage, ReviewContext, ToolCall};
use crate::tool_display::{
    APPROVAL_DESTRUCTIVE_NOTE, approval_dirty_note, approval_reason_lead, run_target,
};
use crate::ui::UiEvent;
use crate::ui::modal::Modal;
use crate::ui::slash::Palette;
use crate::ui::terminal_surface::CURSOR_MARKER;

use super::component::{Component, Container, take_cursor_position};
use super::overlay::{FocusTarget, PaletteView, render_menu_lines};
use super::startup::StartPage;
use super::text::{ansi_spans, strip_ansi_for_text};
use super::transcript::{Transcript, TranscriptRender};
use super::wrap::{
    display_width, line_text, pad_line_left, push_wrapped_line_wordwise, spans_width,
    truncate_line, truncate_to_width, wrap_to_width,
};
use super::{
    BOX_X_PADDING_U16, EDITOR_BOTTOM_PADDING_ROWS, EDITOR_CHROME_ROWS_ABOVE,
    EDITOR_VERTICAL_CHROME_ROWS, MAX_EDITOR_ROWS, MAX_MENU_ROWS, MIN_EDITOR_H,
    TEXT_COLUMN_X_PADDING, WORKING_FRAMES, border_style, dim_style, err_style,
    format_elapsed_compact, panel_style, prompt_style,
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
    /// Tool name shown as the header meta (`bash`, `edit`, `write`, ...).
    tool: String,
    /// The action text: the run target (a `$ `-prompted command for shell).
    target: String,
    /// The muted base sentence of the reason line, derived deterministically
    /// from the call in Tier 3 (never sent to the model).
    reason_lead: String,
    /// The call tripped the destructive floor (ADR-0010): the reason line
    /// carries a danger-toned clause.
    destructive: bool,
    /// Workspace-relative display paths of uncommitted user changes the call
    /// touches (ADR-0028). Non-empty relabels `a` to "all dirty files this
    /// task" and appends a muted dirty clause to the reason line.
    dirty_paths: Vec<String>,
    /// Whether an "always" grant is on offer (`a`).
    allow_always: bool,
    /// Whether a per-project grant is on offer (`p`); never for a destructive
    /// or dirty-tree call.
    allow_project: bool,
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

/// Effective approval-policy posture shown on the composer's bottom
/// statusline. State is always symbol + label, never color alone. The mapping
/// follows the runtime's real approval surface: the interactive loop gates
/// every non-allowlisted tool through the approval prompt (`on-request`);
/// `always-approve` mirrors the auto-approving print gate, and `read-only` /
/// `off` are reserved postures for gates that deny or skip approvals entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalPolicy {
    /// Gated tools are auto-approved (the print gate's `--approve` posture).
    /// Not constructed by the interactive loop today; kept so the statusline
    /// vocabulary covers every runtime posture.
    #[allow(dead_code)]
    AlwaysApprove,
    /// Gated tools prompt for a decision — the interactive loop's posture.
    OnRequest,
    /// Gated tools are denied. Reserved posture; not constructed yet.
    #[allow(dead_code)]
    ReadOnly,
    /// Approvals are disabled entirely. Reserved posture; not constructed yet.
    #[allow(dead_code)]
    Off,
}

impl ApprovalPolicy {
    /// State glyph from the symbol vocabulary (`◆`/`▲`/`■`/`○`).
    fn symbol(self) -> &'static str {
        match self {
            Self::AlwaysApprove => crate::ui::symbols::DONE,
            Self::OnRequest => crate::ui::symbols::REVIEW,
            Self::ReadOnly => crate::ui::symbols::ERROR,
            Self::Off => crate::ui::symbols::EMPTY,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::AlwaysApprove => "always-approve",
            Self::OnRequest => "on-request",
            Self::ReadOnly => "read-only",
            Self::Off => "off",
        }
    }

    /// Symbol color role: green done / orange review / red error / dim empty.
    fn symbol_style(self) -> Style {
        match self {
            Self::AlwaysApprove => Style::default().fg(crate::ui::palette::GREEN),
            Self::OnRequest => prompt_style(),
            Self::ReadOnly => Style::default().fg(crate::ui::palette::RED),
            Self::Off => dim_style(),
        }
    }
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

/// Content width inside the docked approval panel's frame: the given panel width
/// less the two border cells and the one-cell padding on each side (`│ … │`).
fn approval_content_width(width: usize) -> usize {
    width.saturating_sub(4).max(1)
}

/// One horizontal rule of the docked approval panel (`┌─┐`/`├─┤`/`└─┘`), spanning
/// the full panel width in the border role. Square corners, hand-drawn — never a
/// `Block` widget (design-language §6, §13.7).
fn approval_rule(width: usize, left: char, right: char) -> Line<'static> {
    let n = width.max(2);
    Line::from(Span::styled(
        format!("{left}{}{right}", "─".repeat(n - 2)),
        border_style(),
    ))
}

/// Frame one already-wrapped physical content line as a panel body row: `│ ` +
/// content padded to the inner width + ` │`, the border cells in the border role.
fn approval_body_row(width: usize, mut content: Line<'static>) -> Line<'static> {
    let inner = approval_content_width(width);
    truncate_line(&mut content, inner);
    let used = display_width(&line_text(&content));
    let mut spans = vec![
        Span::styled("\u{2502}".to_string(), border_style()),
        Span::raw(" "),
    ];
    spans.extend(content.spans);
    if used < inner {
        spans.push(Span::raw(" ".repeat(inner - used)));
    }
    spans.push(Span::raw(" "));
    spans.push(Span::styled("\u{2502}".to_string(), border_style()));
    Line::from(spans)
}

/// Wrap `content` to the panel's inner width and push one framed body row per
/// physical line. Word-aware and token-safe (breaks at spaces, keeps paths and
/// identifiers intact), so continuations align under the content column
/// (design-language §3, §7).
fn approval_body_rows(width: usize, content: Line<'static>, out: &mut Vec<Line<'static>>) {
    let inner = approval_content_width(width);
    let mut wrapped = Vec::new();
    push_wrapped_line_wordwise(&content, inner, &mut wrapped);
    for line in wrapped {
        out.push(approval_body_row(width, line));
    }
}

/// Header spans: `▲ REVIEW` (orange accent glyph + bold label carrying the
/// decision state, per §8.5) with the muted tool name as meta. State is
/// symbol + label + color, never color alone (§13.8).
fn approval_header_spans(hint: &ApprovalHint) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::styled(format!("{} ", crate::ui::symbols::REVIEW), prompt_style()),
        Span::styled(
            "REVIEW".to_string(),
            prompt_style().add_modifier(Modifier::BOLD),
        ),
    ];
    if !hint.tool.is_empty() {
        spans.push(Span::styled(format!("  {}", hint.tool), dim_style()));
    }
    spans
}

/// Action spans: `$ <command>` for shell (dim `$ ` prompt) or the run target for
/// other tools, keeping the honest `┊ unsandboxed` posture marker.
fn approval_action_spans(hint: &ApprovalHint) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
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

/// Reason spans: the muted base sentence, then the danger-toned destructive
/// clause (red per §2.2 — the sentence itself carries the meaning so it survives
/// the monochrome test), then the muted dirty-tree clause. `content_width` caps
/// the dirty list so it degrades with an `…` at narrow widths.
fn approval_reason_spans(hint: &ApprovalHint, content_width: usize) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(hint.reason_lead.clone(), dim_style())];
    if hint.destructive {
        spans.push(Span::styled(
            format!(" {APPROVAL_DESTRUCTIVE_NOTE}"),
            err_style(),
        ));
    }
    if let Some(note) = approval_dirty_note(&hint.dirty_paths, content_width) {
        spans.push(Span::styled(format!(" {note}"), dim_style()));
    }
    spans
}

/// Decision-affordance spans: `┊`-separated `key label` hints (keys in ink,
/// labels muted). Only offered options appear; in the dirty-tree context `a` is
/// relabelled to "all dirty files this task" (that is what an "always" grant
/// means then) and `p` is never offered. `n deny` is always last (the default).
fn approval_hint_spans(hint: &ApprovalHint) -> Vec<Span<'static>> {
    let dirty = !hint.dirty_paths.is_empty();
    let mut items: Vec<(&str, &str)> = vec![("y", "approve")];
    if hint.allow_always {
        items.push((
            "a",
            if dirty {
                "all dirty files this task"
            } else {
                "always"
            },
        ));
    }
    if hint.allow_project {
        items.push(("p", "project"));
    }
    items.push(("n", "deny"));

    let mut spans = Vec::new();
    for (index, (key, label)) in items.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                format!(" {} ", crate::ui::symbols::SEP),
                dim_style(),
            ));
        }
        spans.push(Span::styled(key.to_string(), Style::default()));
        spans.push(Span::styled(format!(" {label}"), dim_style()));
    }
    spans
}

/// The docked APPROVAL panel (§8.5): a hand-drawn box-drawing frame with a
/// `▲ REVIEW` header, the action, the explanatory reason, a hairline-ruled
/// decision affordance, and a bottom border. Rendered in the overlay region
/// above the composer, so the composer body stays visible while input focus is
/// on the decision.
fn approval_panel_lines(hint: &ApprovalHint, width: usize) -> Vec<Line<'static>> {
    let inner = approval_content_width(width);
    let mut out = Vec::new();
    out.push(approval_rule(width, '\u{250c}', '\u{2510}'));
    out.push(approval_body_row(
        width,
        Line::from(approval_header_spans(hint)),
    ));
    approval_body_rows(width, Line::from(approval_action_spans(hint)), &mut out);
    approval_body_rows(
        width,
        Line::from(approval_reason_spans(hint, inner)),
        &mut out,
    );
    // Hairline-ruled decision affordance (§8.5).
    out.push(approval_rule(width, '\u{251c}', '\u{2524}'));
    approval_body_rows(width, Line::from(approval_hint_spans(hint)), &mut out);
    out.push(approval_rule(width, '\u{2514}', '\u{2518}'));
    // Defense in depth for very narrow panels: the frame needs a few cells to
    // hold `│ … │`, so clamp every row to the panel width. In the real render
    // path `render_menu_lines` also clips, but the rows must never claim more
    // width than they were given.
    for line in &mut out {
        truncate_line(line, width.max(1));
    }
    out
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
    /// Whether the terminal (pane) reports itself focused. Terminals without
    /// focus reporting never send focus events, so this stays true. While
    /// unfocused the spinner holds its frame and requests no tick redraws, so N
    /// backgrounded Iris panes in a tmux session do not each animate at 10Hz;
    /// event-driven redraws (streaming, tool output) continue as normal.
    terminal_focused: bool,
    /// Effective approval-policy posture for the bottom statusline.
    approval_policy: ApprovalPolicy,
    /// The start page (IrisMark + launcher), shown before the first session
    /// activity when Iris launched interactively with no task/resume target.
    pub(crate) start_page: Option<StartPage>,
    /// The session bar as last rendered `(width, lines)`, so the document
    /// stable-prefix hint stays accurate: the transcript's stable prefix only
    /// extends below the bar when the bar itself did not change.
    last_session_bar: Option<(u16, Vec<Line<'static>>)>,
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
            terminal_focused: true,
            approval_policy: ApprovalPolicy::OnRequest,
            start_page: None,
            last_session_bar: None,
        }
    }

    /// Show the start page (IrisMark + launcher) until the session begins.
    pub(crate) fn show_start_page(&mut self) {
        self.start_page = Some(StartPage::new(reduced_motion()));
    }

    /// Dismiss the start page: entering a session replaces the launcher with
    /// the normal transcript; the shared chrome stays.
    pub(crate) fn leave_start_page(&mut self) {
        self.start_page = None;
    }

    pub(crate) fn start_page_active(&self) -> bool {
        self.start_page.is_some()
    }

    /// Set the effective approval-policy posture shown on the bottom statusline.
    pub(crate) fn set_approval_policy(&mut self, policy: ApprovalPolicy) {
        self.approval_policy = policy;
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
        // A submitted task enters the session: the launcher gives way to the
        // normal transcript, under the same chrome.
        self.start_page = None;
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
        // The start page's IrisMark reuses the spinner tick machinery: it
        // animates only while the terminal is focused, and holds still under
        // reduced motion (StartPage::tick handles both cadence and freeze).
        if let Some(page) = &mut self.start_page {
            return page.tick();
        }
        self.spinner.tick()
    }

    // --- approval ---

    /// Show a gated tool's approval prompt in the status row. The transcript
    /// records the final approval/denial outcome, not the transient prompt.
    pub(crate) fn show_approval(
        &mut self,
        call: &ToolCall,
        allow_always: bool,
        allow_project: bool,
        ctx: &ReviewContext,
    ) {
        let shell = call.name == "bash";
        self.approval_hint = Some(ApprovalHint {
            tool: call.name.clone(),
            target: run_target(call),
            reason_lead: approval_reason_lead(call),
            destructive: ctx.destructive,
            dirty_paths: ctx.dirty_paths.clone(),
            allow_always,
            allow_project,
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
    let transcript = if incremental {
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
    // The session bar (bar + soft hairline) is reserved ahead of the
    // transcript: it occupies the top pane rows and the transcript flows
    // beneath it. The stable-prefix hint covers the bar only while the bar
    // itself is unchanged at this width; a bar change (context meter movement,
    // branch switch) resets the hint so the diff never reuses a stale bar row.
    let bar = session_bar_lines(screen, width);
    let bar_rows = bar.len();
    let bar_stable = screen
        .last_session_bar
        .as_ref()
        .is_some_and(|(prev_width, prev)| *prev_width == width && *prev == bar);
    if !bar_stable {
        screen.last_session_bar = Some((width, bar.clone()));
    }
    // Full-pane takeover: while the transcript is shorter than the pane, blank
    // filler rows sit BETWEEN the transcript and the bottom-pinned tail, so the
    // conversation reads top-down from the first pane row while the working
    // indicator and composer always occupy the bottom rows (Claude Code-style).
    // The filler lives in the volatile tail: it shrinks as the transcript
    // grows, the document holds exactly the viewport height until content
    // overflows, and no blank row ever scrolls into native scrollback. On the
    // start page the filler carries the centered IrisMark + launcher instead of
    // blanks.
    let tail_rows = chrome.len() + working_block.len();
    let filler_rows = usize::from(height)
        .saturating_sub(tail_rows)
        .saturating_sub(transcript.total_lines)
        .saturating_sub(bar_rows);
    let volatile_tail = tail_rows + filler_rows;
    // The transcript is the scrolling base, moved into the document and never
    // cloned. The bottom-pinned tail -- viewport filler, working indicator,
    // then composer chrome (which carries the docked overlays) -- is composited
    // through the root Container, mirroring pi-mono's `TUI extends Container`
    // (`tui.ts#L265`). Every tail section is bounded by the viewport height,
    // not the transcript length, so the container's only per-frame copy is
    // small and constant.
    let mut tail = Container::new();
    tail.add_child(Box::new(LinesSection(filler_lines(
        screen,
        filler_rows,
        width,
    ))));
    tail.add_child(Box::new(LinesSection(working_block)));
    tail.add_child(Box::new(LinesSection(chrome)));
    // The bar rows shift the whole document down, so the transcript's stable
    // prefix only holds when the bar rows above it are themselves unchanged.
    let stable_prefix = if bar_stable {
        transcript.stable_prefix.saturating_add(bar_rows)
    } else {
        0
    };
    let mut document = bar;
    document.extend(transcript.lines);
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

/// The filler section between the transcript and the bottom-pinned tail:
/// blank rows normally, or the start page's centered IrisMark + launcher block
/// (vertically centered, truncated when the viewport is too short).
fn filler_lines(screen: &Screen, filler_rows: usize, width: u16) -> Vec<Line<'static>> {
    let Some(page) = &screen.start_page else {
        return std::iter::repeat_with(Line::default)
            .take(filler_rows)
            .collect();
    };
    let mut block = Component::render(page, usize::from(width));
    block.truncate(filler_rows);
    let top = filler_rows.saturating_sub(block.len()) / 2;
    let bottom = filler_rows.saturating_sub(block.len()).saturating_sub(top);
    let mut lines = Vec::with_capacity(filler_rows);
    lines.extend(std::iter::repeat_with(Line::default).take(top));
    lines.extend(block);
    lines.extend(std::iter::repeat_with(Line::default).take(bottom));
    lines
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

/// Build the composer's bottom statusline — the composer's last content row,
/// under the input and the lighter internal rule:
/// `◉ CODE ─ GPT-5.5 XHIGH ─ ◆ always-approve`.
/// The mode glyph is the orange accent; `CODE` is bold; the model name is the
/// underlined model-picker button; effort is muted; the approval-policy
/// segment carries its state symbol + label (never color alone). Location and
/// context moved to the pane-top [`session_bar_lines`] and never appear here.
/// Narrow widths drop, in order: policy → effort → minimum `◉ CODE ─ MODEL`.
/// Returns `None` when there is no footer yet or even the minimum cannot fit.
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
    let policy = screen.approval_policy;
    let policy_seg = || {
        vec![
            Span::styled(format!("{} ", policy.symbol()), policy.symbol_style()),
            Span::styled(policy.label().to_string(), dim_style()),
        ]
    };

    // Candidates from fullest to minimum. The drop order is monotonic and
    // matches the spec: drop the policy segment, then effort, leaving the
    // minimum `◉ CODE ─ MODEL`.
    let candidates: Vec<Vec<Vec<Span<'static>>>> = vec![
        vec![mode_seg(), model_with_effort(), policy_seg()],
        vec![mode_seg(), model_with_effort()],
        vec![mode_seg(), model_only()],
    ];

    let spans = candidates
        .into_iter()
        .find_map(|segments| statusline_left(width, segments))?;
    let mut line = Line::from(spans);
    truncate_line(&mut line, width.max(1));
    Some(line)
}

/// Build the session bar — the pane-top "where am I / how full am I" row:
/// `<cwd> ┊ git <branch>` on the left (cwd body ink, separator and branch
/// dim), and the right-aligned context readout `CTX <used>/<cap> <meter>`
/// (`CTX` and `/<cap>` dim, `<used>` body ink, then the 10-dot meter). With an
/// unknown context window the readout is `CTX <used>` with no meter. Narrow
/// widths drop, in order: meter → `/<cap>` → branch → middle-truncate the cwd
/// harder; the minimum form is the cwd alone. Returns `None` when there is no
/// footer yet.
pub(super) fn session_bar(screen: &Screen, width: u16) -> Option<Line<'static>> {
    let footer = screen.footer.as_ref()?;
    let width = usize::from(width).max(1);
    let (cwd, branch) = split_cwd_branch(&strip_ansi_for_text(&footer.cwd));
    if cwd.is_empty() {
        return None;
    }
    let used = footer.context_used_tokens.unwrap_or(0);
    let used_text = compact_count(used);
    let cap = footer
        .context
        .as_ref()
        .map(|context| strip_ansi_for_text(context))
        .filter(|context| !context.is_empty());
    let meter_filled = cap
        .as_deref()
        .and_then(parse_context_window)
        .map(|window| context_meter_filled(used, window));

    // The context readout, fullest form first: used/cap + meter, then used/cap,
    // then used alone, then nothing.
    let ctx_spans = |with_cap: bool, with_meter: bool| -> Vec<Span<'static>> {
        let mut spans = vec![
            Span::styled("CTX ".to_string(), dim_style()),
            Span::styled(used_text.clone(), Style::default()),
        ];
        if with_cap && let Some(cap) = cap.as_deref() {
            spans.push(Span::styled(format!("/{cap}"), dim_style()));
        }
        if with_meter && let Some(filled) = meter_filled {
            spans.push(Span::raw(" "));
            spans.extend(context_meter_spans(filled));
        }
        spans
    };
    let right_candidates: Vec<Vec<Span<'static>>> = vec![
        ctx_spans(true, true),
        ctx_spans(true, false),
        ctx_spans(false, false),
    ];

    let branch_suffix = branch
        .as_ref()
        .map(|branch| format!(" {} git {branch}", crate::ui::symbols::SEP))
        .unwrap_or_default();
    // A middle-truncated cwd keeps at least `…/<project>`-ish room before a
    // lower-priority segment is dropped instead.
    const CWD_MIN: usize = 12;

    // Drop order: meter → `/<cap>` → branch → hard cwd truncation.
    for (right, with_branch) in right_candidates
        .iter()
        .map(|right| (Some(right), true))
        .chain([(right_candidates.last(), false), (None, false)])
    {
        let right_w = right.map(|spans| spans_width(spans)).unwrap_or(0);
        let suffix = if with_branch {
            branch_suffix.as_str()
        } else {
            ""
        };
        let gap = if right_w > 0 { 2 } else { 0 };
        let avail_cwd = width
            .saturating_sub(right_w)
            .saturating_sub(gap)
            .saturating_sub(display_width(suffix));
        if right.is_some() && avail_cwd < CWD_MIN.min(display_width(&cwd)) {
            continue;
        }
        if avail_cwd == 0 {
            continue;
        }
        let shown_cwd = truncate_cwd_middle(&cwd, avail_cwd);
        if shown_cwd.is_empty() {
            continue;
        }
        let mut spans = vec![Span::styled(shown_cwd.clone(), Style::default())];
        if !suffix.is_empty() {
            spans.push(Span::styled(suffix.to_string(), dim_style()));
        }
        if let Some(right) = right {
            let left_w = spans_width(&spans);
            let fill = width.saturating_sub(left_w).saturating_sub(right_w);
            if fill >= 2 {
                spans.push(Span::raw(" ".repeat(fill)));
                spans.extend(right.iter().cloned());
            }
        }
        let mut line = Line::from(spans);
        truncate_line(&mut line, width);
        return Some(line);
    }
    // Minimum form: the cwd alone, truncated to whatever fits.
    let shown = truncate_cwd_middle(&cwd, width);
    (!shown.is_empty()).then(|| Line::from(Span::styled(shown, Style::default())))
}

/// The session bar block: the bar row plus its soft hairline (a dim `─`
/// repeat, visibly lighter than the composer's border-weight top edge),
/// inset to the shared pane measure. Empty when there is no footer yet.
pub(super) fn session_bar_lines(screen: &Screen, width: u16) -> Vec<Line<'static>> {
    let inset = BOX_X_PADDING_U16.min(width.saturating_sub(1));
    let content_width = width.saturating_sub(inset.saturating_mul(2)).max(1);
    let Some(mut bar) = session_bar(screen, content_width) else {
        return Vec::new();
    };
    pad_line_left(&mut bar, usize::from(inset));
    let mut rule = Line::from(Span::styled(
        "─".repeat(usize::from(content_width)),
        dim_style(),
    ));
    pad_line_left(&mut rule, usize::from(inset));
    vec![bar, rule]
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

/// The composer's top edge: a full-width hairline in the border role — the one
/// rule separating the composer from the transcript (the composer has no box).
fn composer_hairline(width: usize) -> Line<'static> {
    Line::from(Span::styled("─".repeat(width.max(1)), border_style()))
}

/// The composer's internal rule between the input rows and the bottom
/// statusline: a lighter hairline (dim `╌` repeat, not border weight).
fn composer_internal_rule(width: usize) -> Line<'static> {
    Line::from(Span::styled("╌".repeat(width.max(1)), dim_style()))
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
/// `MIN_EDITOR_H` (hairline + one input row + internal rule + statusline) before anything else
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

    // The composer editor always renders at its natural height; the approval
    // surface docks in the overlay region above it (below), so the composer body
    // stays visible while input focus is on the decision.
    let editor_rows = editor_visual_rows(&screen.editor, area.width);
    let input_text = screen.editor_text();
    // The docked menu region shows the pending approval, or whichever overlay
    // currently has focus, each rendered through the `Component` contract. The
    // inner render width equals the inset width `render_menu_lines` paints into,
    // so output is unchanged. A pending approval takes the region exclusively:
    // the composer is frozen while it is shown, so no modal/palette can be open.
    let menu_inner_width = content_width(usize::from(area.width));
    let menu_lines: Option<Vec<Line<'static>>> = if let Some(hint) = &screen.approval_hint {
        Some(approval_panel_lines(hint, menu_inner_width))
    } else {
        match screen.focus_for(&input_text) {
            FocusTarget::Modal => screen
                .modal
                .as_ref()
                .map(|modal| Component::render(modal, menu_inner_width)),
            FocusTarget::Palette => Some(
                PaletteView::for_palette(&screen.palette, &input_text).render(menu_inner_width),
            ),
            FocusTarget::Editor => None,
        }
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
    // full hairline top edge, then the input rows, a lighter internal rule,
    // and the bottom statusline. No box, no hint row; location/context live in
    // the pane-top session bar, never here.
    // Keep one soft row under the normal composer, but do not spend an extra
    // blank row while a docked overlay (or the docked approval panel, which now
    // lives in the same region) already occupies the lower viewport.
    let bottom_padding_rows = if menu_wanted == 0 {
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
    // Padding is preferred, not protected: at the minimum composer height the
    // input row wins over the soft bottom row.
    let pad_rows = bottom_padding_rows.min(editor_area.height.saturating_sub(MIN_EDITOR_H));
    let text_area = Rect {
        x: box_area.x + text_x_offset,
        y: editor_area.y + EDITOR_CHROME_ROWS_ABOVE.min(editor_area.height.saturating_sub(1)),
        width: box_area.width.saturating_sub(text_x_offset).max(1),
        height: editor_area
            .height
            .saturating_sub(EDITOR_VERTICAL_CHROME_ROWS)
            .saturating_sub(pad_rows)
            .max(1),
    };
    // Cell of the editor's hardware-cursor (IME) marker, in buffer coordinates.
    // Only emitted when the composer owns input focus (no turn/modal/approval),
    // located by the reversed block cursor `ratatui-textarea` draws for us.
    let mut cursor_cell: Option<(u16, u16)> = None;
    (&screen.editor).render(text_area, &mut buf);
    if screen.composer_focused() {
        cursor_cell = find_reversed_cell(&buf, text_area);
    }
    // The composer's chrome rows: the full-width hairline top edge above the
    // input, then — below the input — the lighter internal rule and the bottom
    // statusline. Painted last so they are never overwritten by the
    // textarea/approval body at very small heights.
    if heights.editor > 0 {
        let hairline = composer_hairline(usize::from(box_area.width));
        buf.set_line(box_area.x, box_area.y, &hairline, box_area.width);
    }
    let status_y = heights.editor.saturating_sub(pad_rows).saturating_sub(1);
    if status_y >= 2
        && let Some(statusline) = composer_statusline(screen, box_area.width)
    {
        buf.set_line(
            box_area.x,
            editor_area.y + status_y,
            &statusline,
            box_area.width,
        );
        // The internal rule sits directly above the statusline, only when a
        // row remains for the input above it (hairline + input + rule + status).
        if status_y >= 3 {
            let rule = composer_internal_rule(usize::from(box_area.width));
            buf.set_line(
                box_area.x,
                editor_area.y + status_y - 1,
                &rule,
                box_area.width,
            );
        }
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

    use ratatui::text::Line;

    use super::{
        ApprovalHint, ApprovalPolicy, CONTEXT_METER_DOTS, Screen, Spinner, approval_hint_spans,
        approval_panel_lines, composer_statusline, context_meter_filled, display_width, line_text,
        parse_context_window, session_bar, truncate_cwd_middle, working_lines,
    };
    use crate::ui::tui::WORKING_FRAMES;

    /// A minimal review hint for the docked-approval rendering tests.
    fn test_hint() -> ApprovalHint {
        ApprovalHint {
            tool: "bash".to_string(),
            target: "echo hi".to_string(),
            reason_lead: "Runs a shell command in the workspace.".to_string(),
            destructive: false,
            dirty_paths: Vec::new(),
            allow_always: false,
            allow_project: false,
            shell: true,
            sandbox_unavailable: false,
        }
    }

    /// The joined plain text of a rendered approval panel.
    fn panel_text(hint: &ApprovalHint, width: usize) -> String {
        approval_panel_lines(hint, width)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn footer_screen(cwd: &str) -> Screen {
        let mut screen = Screen::new();
        screen.set_footer_with_context(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            Some("300k".to_string()),
            cwd.to_string(),
        );
        screen
    }

    #[test]
    fn session_bar_shows_location_left_and_context_right() {
        let mut screen = footer_screen("~/repo (main)");
        screen.apply(crate::ui::UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: None,
            usage: Some(crate::nexus::ProviderUsage {
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
        let bar = session_bar(&screen, 80)
            .map(|l| line_text(&l))
            .expect("bar");
        assert!(bar.starts_with("~/repo ┊ git main"), "{bar:?}");
        assert!(
            bar.trim_end().ends_with("CTX 90k/300k ●●●○○○○○○○"),
            "{bar:?}"
        );
        // Mode/model/policy never appear on the session bar.
        assert!(!bar.contains("CODE"), "{bar:?}");
        assert!(!bar.contains("GPT"), "{bar:?}");
    }

    #[test]
    fn session_bar_drops_meter_then_cap_then_branch_then_truncates() {
        let screen = footer_screen("~/repo (main)");
        // Wide: everything fits.
        let full = session_bar(&screen, 60)
            .map(|l| line_text(&l))
            .expect("bar");
        assert!(full.contains("┊ git main"), "{full:?}");
        assert!(full.contains("CTX 0/300k ○○○○○○○○○○"), "{full:?}");

        // 1) The meter drops first.
        let no_meter = session_bar(&screen, 34)
            .map(|l| line_text(&l))
            .expect("bar");
        assert!(no_meter.contains("CTX 0/300k"), "{no_meter:?}");
        assert!(!no_meter.contains('○'), "{no_meter:?}");
        assert!(no_meter.contains("┊ git main"), "{no_meter:?}");

        // 2) Then the `/<cap>` suffix.
        let no_cap = session_bar(&screen, 25)
            .map(|l| line_text(&l))
            .expect("bar");
        assert!(no_cap.contains("CTX 0"), "{no_cap:?}");
        assert!(!no_cap.contains("/300k"), "{no_cap:?}");
        assert!(no_cap.contains("┊ git main"), "{no_cap:?}");

        // 3) Then the branch.
        let no_branch = session_bar(&screen, 16)
            .map(|l| line_text(&l))
            .expect("bar");
        assert!(no_branch.contains("~/repo"), "{no_branch:?}");
        assert!(!no_branch.contains("git"), "{no_branch:?}");
        assert!(no_branch.contains("CTX 0"), "{no_branch:?}");

        // 4) Minimum form: the cwd alone.
        let minimum = session_bar(&screen, 7).map(|l| line_text(&l)).expect("bar");
        assert!(minimum.contains("~/repo"), "{minimum:?}");
        assert!(!minimum.contains("CTX"), "{minimum:?}");

        // Never overflows at any width.
        for width in 1..=80u16 {
            if let Some(line) = session_bar(&screen, width) {
                assert!(
                    display_width(&line_text(&line)) <= usize::from(width),
                    "width {width}: {:?}",
                    line_text(&line)
                );
            }
        }
    }

    #[test]
    fn session_bar_without_context_window_shows_used_tokens_only() {
        let mut screen = Screen::new();
        screen.set_footer_with_context("custom".to_string(), None, None, "~/repo".to_string());
        let bar = session_bar(&screen, 60)
            .map(|l| line_text(&l))
            .expect("bar");
        assert!(bar.contains("CTX 0"), "{bar:?}");
        assert!(!bar.contains("CTX 0/"), "{bar:?}");
        assert!(
            !bar.contains('○') && !bar.contains('●'),
            "no meter: {bar:?}"
        );
    }

    #[test]
    fn bottom_statusline_policy_segment_carries_symbol_and_label() {
        let mut screen = footer_screen("~/repo");
        for (policy, expected) in [
            (ApprovalPolicy::AlwaysApprove, "◆ always-approve"),
            (ApprovalPolicy::OnRequest, "▲ on-request"),
            (ApprovalPolicy::ReadOnly, "■ read-only"),
            (ApprovalPolicy::Off, "○ off"),
        ] {
            screen.set_approval_policy(policy);
            let status = composer_statusline(&screen, 80)
                .map(|l| line_text(&l))
                .expect("statusline");
            assert!(status.contains(expected), "{policy:?}: {status:?}");
            // Location/context never return to the composer statusline.
            assert!(!status.contains("~/repo"), "{status:?}");
            assert!(!status.contains("CTX"), "{status:?}");
        }
    }

    #[test]
    fn document_stable_prefix_covers_bar_only_while_it_is_unchanged() {
        use super::render_document_with_hints;
        use ratatui::layout::Size;

        let mut screen = footer_screen("~/repo");
        screen.commit_user("hello");
        let size = Size::new(80, 12);
        let _ = render_document_with_hints(&mut screen, size);
        // Unchanged bar: the stable prefix extends past the two bar rows.
        let unchanged = render_document_with_hints(&mut screen, size);
        assert!(
            unchanged.stable_prefix >= 2,
            "stable prefix must cover the unchanged session bar: {}",
            unchanged.stable_prefix
        );
        // A bar change (new branch) resets the hint so no stale bar row is reused.
        screen.set_footer_with_context(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            Some("300k".to_string()),
            "~/repo (feat/x)".to_string(),
        );
        let changed = render_document_with_hints(&mut screen, size);
        assert_eq!(changed.stable_prefix, 0, "bar change must reset the hint");
    }

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
        let mut hint = test_hint();
        hint.target = "run an extremely long command".to_string();
        hint.sandbox_unavailable = true;
        hint.dirty_paths = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        hint.destructive = true;
        for width in 1..=6 {
            for line in approval_panel_lines(&hint, width) {
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
    fn approval_panel_header_action_and_shell_prompt() {
        // The docked panel leads with `▲ REVIEW` + the tool meta, then the
        // action row: `$ <command>` for shell, prose otherwise.
        let shell = test_hint();
        let mut non_shell = test_hint();
        non_shell.tool = "edit".to_string();
        non_shell.target = "edit src/x.rs".to_string();
        non_shell.reason_lead = "Modifies src/x.rs.".to_string();
        non_shell.shell = false;

        let shell_text = panel_text(&shell, 60);
        let non_shell_text = panel_text(&non_shell, 60);
        assert!(shell_text.contains("\u{25b2} REVIEW"), "{shell_text}");
        assert!(shell_text.contains("bash"), "{shell_text}");
        assert!(shell_text.contains("$ echo hi"), "{shell_text}");
        // The explanatory reason line is present.
        assert!(
            shell_text.contains("Runs a shell command in the workspace."),
            "{shell_text}"
        );
        assert!(
            non_shell_text.contains("\u{25b2} REVIEW"),
            "{non_shell_text}"
        );
        assert!(non_shell_text.contains("edit src/x.rs"), "{non_shell_text}");
        assert!(
            non_shell_text.contains("Modifies src/x.rs."),
            "{non_shell_text}"
        );
        // The `$ ` prompt is shell-only (the action row has no `$ `).
        assert!(!non_shell_text.contains("$ "), "{non_shell_text}");
    }

    #[test]
    fn approval_panel_hints_are_sep_separated_and_offer_only_options() {
        // Decision affordance: `┊`-separated `key label` hints, only offered
        // options, `n deny` last.
        let hint = ApprovalHint {
            allow_always: true,
            allow_project: true,
            ..test_hint()
        };
        let hints = line_text(&Line::from(approval_hint_spans(&hint)));
        assert_eq!(
            hints,
            "y approve \u{250a} a always \u{250a} p project \u{250a} n deny"
        );

        // y/N-only when nothing else is offered.
        let plain = line_text(&Line::from(approval_hint_spans(&test_hint())));
        assert_eq!(plain, "y approve \u{250a} n deny");
    }

    #[test]
    fn approval_panel_destructive_and_dirty_reason() {
        // Destructive appends the danger clause; a non-empty dirty set appends
        // the dirty clause and relabels `a`; `p` is never offered.
        let hint = ApprovalHint {
            destructive: true,
            dirty_paths: vec!["src/main.rs".to_string()],
            allow_always: true,
            allow_project: false,
            ..test_hint()
        };
        // The reason wraps across body rows, so assert the individual clauses
        // are present (a wrap boundary may fall between words).
        let text = panel_text(&hint, 110);
        assert!(text.contains("Flagged destructive"), "{text}");
        assert!(text.contains("uncommitted user changes"), "{text}");
        assert!(text.contains("src/main.rs"), "{text}");
        assert!(text.contains("a all dirty files this task"), "{text}");
        assert!(!text.contains("p project"), "{text}");
    }

    #[test]
    fn approval_panel_marks_platform_without_sandbox() {
        // On a platform with no kernel sandbox backend the shell runs
        // unconfined; the action row states that posture at the decision point.
        // The marker reflects platform capability, not per-run confinement.
        let unavailable = ApprovalHint {
            sandbox_unavailable: true,
            ..test_hint()
        };
        let has_backend = test_hint();
        let unavailable_text = panel_text(&unavailable, 60);
        let has_backend_text = panel_text(&has_backend, 60);
        assert!(
            unavailable_text.contains("unsandboxed"),
            "{unavailable_text}"
        );
        assert!(
            unavailable_text.contains(crate::ui::symbols::SEP),
            "{unavailable_text}"
        );
        assert!(
            !has_backend_text.contains("unsandboxed"),
            "{has_backend_text}"
        );
    }
}
