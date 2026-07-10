//! Replayable screen state, composer chrome, status rail, and working indicator rendering.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect, Size};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui_textarea::{TextArea, WrapMode};

use crate::git::status::{GitStatus, JjStatus, VcsStatus};
#[cfg(test)]
use crate::mimir::model_catalog;
use crate::nexus::{ApprovalDecision, ContextPressureTier, ProviderUsage, ToolCall};
use crate::ui::UiEvent;
use crate::ui::modal::Modal;
use crate::ui::slash::Palette;
use crate::ui::terminal_surface::CURSOR_MARKER;
use crate::ui::tui::activity::WorkPhase;

use super::component::{Component, Container, take_cursor_position};
use super::overlay::{FocusTarget, PaletteView, render_menu_lines};
use super::session_menu::{MAX_DROPDOWN_ROWS, SessionMenu};
use super::startup::StartPage;
use super::text::strip_ansi_for_text;
use super::transcript::{Transcript, TranscriptRender};
use super::wrap::{
    display_width, line_text, pad_line_left, spans_width, truncate_line, truncate_to_width,
    wrap_to_width,
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
                | UiEvent::CompactionLifecycle { .. }
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

/// Whether the working-indicator animation should be frozen at construction:
/// the `IRIS_REDUCED_MOTION` env flag only, so a pure UI unit test never depends
/// on the machine's persisted config. The persisted `tui.reducedMotion`
/// preference is applied post-construction via [`Screen::set_reduced_motion`]
/// (env still wins), the same way `scroll_speed`/alt-screen are threaded.
fn reduced_motion() -> bool {
    crate::config::iris_flag_enabled("IRIS_REDUCED_MOTION")
}

/// Effective approval-policy posture shown on the composer's bottom
/// statusline. State is always symbol + label, never color alone. The mapping
/// follows the runtime's real approval surface: the interactive loop gates
/// every non-allowlisted tool through the approval prompt (`on-request`) unless
/// `--dangerously-skip-permissions` bypasses the gate; `read-only` / `off` are
/// reserved postures for gates that deny or skip approvals entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalPolicy {
    /// Gated tools are auto-approved (`--dangerously-skip-permissions`).
    /// Distinct from `Auto`: this bypasses every approval prompt, floors
    /// included, and is activated only by the explicit CLI flag.
    SkipPermissions,
    /// `on-request` (strict): gated tools prompt for a decision — the default
    /// interactive posture. Maps to [`nexus::ApprovalMode::Strict`].
    OnRequest,
    /// `auto` preset (ADR-0032): Nexus auto-runs calls it can prove safe and
    /// prompts for the rest. NOT the same as `always-approve`. Maps to
    /// [`nexus::ApprovalMode::Auto`].
    Auto,
    /// `never-ask` preset (ADR-0032): gated tools never prompt; an unresolved
    /// call is denied. Maps to [`nexus::ApprovalMode::NeverAsk`].
    NeverAsk,
    /// Gated tools are denied. Reserved posture; not constructed yet.
    #[allow(dead_code)]
    ReadOnly,
    /// Approvals are disabled entirely. Reserved posture; not constructed yet.
    #[allow(dead_code)]
    Off,
}

impl From<crate::nexus::ApprovalMode> for ApprovalPolicy {
    fn from(mode: crate::nexus::ApprovalMode) -> Self {
        match mode {
            crate::nexus::ApprovalMode::Strict => Self::OnRequest,
            crate::nexus::ApprovalMode::Auto => Self::Auto,
            crate::nexus::ApprovalMode::NeverAsk => Self::NeverAsk,
        }
    }
}

impl ApprovalPolicy {
    /// State glyph from the symbol vocabulary (`◆`/`▲`/`■`/`○`).
    fn symbol(self) -> &'static str {
        match self {
            Self::SkipPermissions => crate::ui::symbols::ERROR,
            Self::Auto => crate::ui::symbols::ACTIVE,
            Self::OnRequest => crate::ui::symbols::REVIEW,
            Self::NeverAsk => crate::ui::symbols::CANCELLED,
            Self::ReadOnly => crate::ui::symbols::ERROR,
            Self::Off => crate::ui::symbols::EMPTY,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::SkipPermissions => "dangerously skip permissions",
            Self::Auto => "auto",
            Self::OnRequest => "on-request",
            Self::NeverAsk => "never-ask",
            Self::ReadOnly => "read-only",
            Self::Off => "off",
        }
    }

    /// Symbol color role: green done / orange review / red error / dim empty.
    fn symbol_style(self) -> Style {
        match self {
            Self::SkipPermissions | Self::ReadOnly => {
                Style::default().fg(crate::ui::palette::red())
            }
            Self::Auto => Style::default().fg(crate::ui::palette::green()),
            Self::OnRequest => prompt_style(),
            Self::NeverAsk => dim_style(),
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
    /// Last-known VCS status snapshot for the session bar's VCS segment and
    /// dropdown (`None` = not a VCS repo / not yet captured). Painted
    /// last-known; the loop refreshes it from the async [`crate::git::status`]
    /// cache.
    vcs: Option<VcsStatus>,
    /// Latest provider-reported usage, if the provider surfaced it. Cleared at
    /// turn start so the working indicator's per-turn token readout resets.
    usage: Option<ProviderUsage>,
    /// Most recent total context tokens, used to drive the top-frame context
    /// meter. Unlike `usage` this persists across turns (so the meter does not
    /// drop to empty at every turn start) and is cleared only when the model or
    /// context window changes.
    context_used_tokens: Option<u64>,
    context_pressure: ContextPressureTier,
}

/// Predicted prompt-cache posture for a just-applied model/reasoning switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwitchCacheStatus {
    /// No provider/model/reasoning bytes changed.
    Unchanged,
    /// Reasoning changed but the stable prompt prefix should remain warm.
    Warm,
    /// Provider or model changed; the next request starts a cold prompt-cache lane.
    Cold,
}

/// Composer-adjacent switch analytics: a predicted handoff line until the next
/// provider turn reports usage, then the realized token/cache/reduction line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SwitchStatus {
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
    pub(crate) context_tokens: u64,
    pub(crate) cache: SwitchCacheStatus,
    pub(crate) compact_recommended: bool,
    pending: bool,
    folded_tokens: u64,
    compaction_original_tokens: u64,
    compaction_summary_tokens: u64,
    realized_usage: Option<ProviderUsage>,
}

impl SwitchStatus {
    pub(crate) fn new(
        model: String,
        effort: Option<String>,
        context_tokens: u64,
        cache: SwitchCacheStatus,
        compact_recommended: bool,
    ) -> Self {
        Self {
            model,
            effort,
            context_tokens,
            cache,
            compact_recommended,
            pending: true,
            folded_tokens: 0,
            compaction_original_tokens: 0,
            compaction_summary_tokens: 0,
            realized_usage: None,
        }
    }

    fn pending(&self) -> bool {
        self.pending
    }

    fn observe(&mut self, event: &UiEvent) {
        if !self.pending {
            return;
        }
        match event {
            UiEvent::FoldApplied {
                reclaimed_tokens_estimate,
                ..
            } => {
                self.folded_tokens = self
                    .folded_tokens
                    .saturating_add(*reclaimed_tokens_estimate);
            }
            UiEvent::CompactionApplied {
                original_tokens_estimate,
                summary_tokens_estimate,
                ..
            } => {
                self.compaction_original_tokens = self
                    .compaction_original_tokens
                    .saturating_add(*original_tokens_estimate);
                self.compaction_summary_tokens = self
                    .compaction_summary_tokens
                    .saturating_add(*summary_tokens_estimate);
            }
            UiEvent::ProviderTurnCompleted { usage, .. } => {
                self.realized_usage = usage.clone();
                self.pending = false;
            }
            UiEvent::ProviderTurnCancelled { .. }
            | UiEvent::ProviderTurnError { .. }
            | UiEvent::TurnError { .. } => {
                self.realized_usage = None;
                self.pending = false;
            }
            _ => {}
        }
    }

    fn spans(&self) -> Vec<Span<'static>> {
        if self.pending {
            self.predicted_spans()
        } else {
            self.realized_spans()
        }
    }

    fn model_label(&self) -> String {
        let mut label = strip_ansi_for_text(&self.model).to_uppercase();
        if let Some(effort) = self
            .effort
            .as_ref()
            .map(|effort| strip_ansi_for_text(effort).to_uppercase())
            .filter(|effort| !effort.is_empty())
        {
            if !label.is_empty() {
                label.push(' ');
            }
            label.push_str(&effort);
        }
        label
    }

    fn push_sep(spans: &mut Vec<Span<'static>>) {
        spans.push(Span::styled(
            format!(" {} ", crate::ui::symbols::SEP),
            dim_style(),
        ));
    }

    fn predicted_spans(&self) -> Vec<Span<'static>> {
        let mut spans = vec![Span::styled(
            self.model_label(),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        Self::push_sep(&mut spans);
        spans.push(Span::styled(
            format!("~{} ctx", compact_count(self.context_tokens)),
            dim_style(),
        ));
        Self::push_sep(&mut spans);
        spans.push(Span::styled(
            match self.cache {
                SwitchCacheStatus::Unchanged => "cache unchanged",
                SwitchCacheStatus::Warm => "cache prefix warm",
                SwitchCacheStatus::Cold => "cache cold next request",
            }
            .to_string(),
            dim_style(),
        ));
        if self.compact_recommended {
            Self::push_sep(&mut spans);
            spans.push(Span::styled(
                format!("{} ", crate::ui::symbols::REVIEW),
                prompt_style(),
            ));
            spans.push(Span::styled("compact recommended".to_string(), dim_style()));
        }
        spans
    }

    fn realized_spans(&self) -> Vec<Span<'static>> {
        let mut spans = vec![Span::styled(
            self.model_label(),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        Self::push_sep(&mut spans);
        if let Some(usage) = &self.realized_usage {
            spans.push(Span::styled(
                format!(
                    "↑{} ↓{}",
                    compact_count(usage.input_tokens),
                    compact_count(usage.output_tokens)
                ),
                dim_style(),
            ));
            Self::push_sep(&mut spans);
            spans.push(Span::styled(
                format!("cache read {}%", cache_read_percent(usage)),
                dim_style(),
            ));
        } else {
            spans.push(Span::styled("usage unavailable".to_string(), dim_style()));
        }
        Self::push_sep(&mut spans);
        spans.push(Span::styled(
            if self.folded_tokens > 0 {
                format!("folded ~{}", compact_count(self.folded_tokens))
            } else {
                "folded none".to_string()
            },
            dim_style(),
        ));
        Self::push_sep(&mut spans);
        spans.push(Span::styled(
            if self.compaction_original_tokens > 0 {
                format!(
                    "compacted ~{}→~{}",
                    compact_count(self.compaction_original_tokens),
                    compact_count(self.compaction_summary_tokens)
                )
            } else {
                "compacted none".to_string()
            },
            dim_style(),
        ));
        spans
    }
}

fn cache_read_percent(usage: &ProviderUsage) -> u64 {
    if usage.input_tokens == 0 {
        return 0;
    }
    (usage
        .cache_read_input_tokens
        .saturating_mul(100)
        .saturating_add(usage.input_tokens / 2)
        / usage.input_tokens)
        .min(100)
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
    let running = crate::ui::symbols::RUNNING.chars().next();
    frame
        .chars()
        .map(|ch| {
            let style = if Some(ch) == running {
                prompt_style()
            } else {
                dim_style()
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

fn working_sep() -> Span<'static> {
    Span::styled(format!(" {} ", crate::ui::symbols::SEP), dim_style())
}

#[cfg(test)]
pub(super) fn working_indicator_line(
    frame: &str,
    elapsed: Duration,
    can_interrupt: bool,
    usage: Option<&ProviderUsage>,
    queued: usize,
    width: usize,
) -> Line<'static> {
    working_indicator_line_with_activity(frame, elapsed, can_interrupt, None, usage, queued, width)
}

fn working_indicator_line_with_activity(
    frame: &str,
    elapsed: Duration,
    can_interrupt: bool,
    activity: Option<&str>,
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
    if let Some(activity) = activity {
        spans.push(working_sep());
        spans.push(Span::styled(activity.to_string(), dim_style()));
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
    activity: Option<&str>,
    queued: usize,
    width: usize,
) -> Vec<Line<'static>> {
    vec![working_indicator_line_with_activity(
        frame,
        elapsed.unwrap_or_default(),
        true,
        activity,
        footer.and_then(|footer| footer.usage.as_ref()),
        queued,
        width,
    )]
}

/// Build a styled, empty editor for the bordered composer panel: dim
/// placeholder and a reversed block cursor the widget draws itself (no hardware
/// cursor needed). The surrounding border and hint row are painted by
/// `render_editor_chrome`.
pub(super) fn fresh_editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_wrap_mode(WrapMode::WordOrGlyph);
    editor.set_cursor_line_style(Style::default());
    // The caret is the orange accent (§9.4): REVERSED swaps fg↔bg, so an orange
    // fg paints a solid orange block. REVERSED is retained because
    // `find_reversed_cell` locates the caret cell by that modifier.
    editor.set_cursor_style(
        Style::default()
            .fg(crate::ui::palette::orange())
            .add_modifier(Modifier::REVERSED),
    );
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
    /// True while a gated tool awaits the user's decision. The review renders in
    /// the tool block (`▲ REVIEW`); this flag only gates the composer freeze, the
    /// working-indicator suppression, and the IME-cursor hide.
    awaiting_approval: bool,
    /// Sourced global status chrome (model / effort / cwd). The loop refreshes
    /// it from the live model selection; `None` falls back to the composer hint
    /// (e.g. before a provider is selected).
    footer: Option<Footer>,
    /// Composer-adjacent model-switch analytics. A switch first shows predicted
    /// context/cache impact; the next provider turn replaces it with realized
    /// usage/cache/fold/compaction numbers without appending transcript notices.
    switch_status: Option<SwitchStatus>,
    /// The active picker/dialog, when one is open. While present it renders
    /// above the editor and the loop routes keys to it instead of the editor.
    pub(crate) modal: Option<Modal>,
    /// Count of mid-run messages the user has queued (steering + follow-up) that
    /// the loop has not yet injected. Surfaced on the working indicator so the
    /// user sees their queued input register before it is injected. Reset at
    /// each turn boundary.
    queued: usize,
    /// Coarse, provider-neutral phase of the running task, surfaced as the
    /// always-visible working-header label so the status rail is never blank
    /// while a task runs. Driven by display events (`WorkPhase::on_event`) and
    /// the approval lifecycle; only meaningful while the spinner is active.
    phase: WorkPhase,
    /// Whether the terminal (pane) reports itself focused. Terminals without
    /// focus reporting never send focus events, so this stays true. We track the
    /// state only to coalesce focus-change redraws; animation and streaming stay
    /// live in inactive panes so adjacent terminal panes do not look frozen.
    terminal_focused: bool,
    /// Effective approval-policy posture for the bottom statusline.
    approval_policy: ApprovalPolicy,
    /// The start page (IrisMark + launcher), shown before the first session
    /// activity when Iris launched interactively with no task/resume target.
    pub(crate) start_page: Option<StartPage>,
    /// The open SessionBar dropdown (directory tree or git console), if any.
    /// One shared slot: opening one closes the other; a docked modal or
    /// approval closes it. Renders between the session-bar row and its soft
    /// hairline, pushing the transcript down.
    pub(crate) session_menu: Option<SessionMenu>,
    /// The session bar as last rendered `(width, lines)`, so the document
    /// stable-prefix hint stays accurate: the transcript's stable prefix only
    /// extends below the bar when the bar itself did not change.
    last_session_bar: Option<(u16, Vec<Line<'static>>)>,
    /// Pager-mode scroll offset + follow state (ADR-0029). Unused (and never
    /// mutated) in inline mode; the only pager-only state besides the mode
    /// flag, per the ADR's "no pager-only transcript state beyond
    /// scroll/focus" rule.
    pub(crate) scroll: super::pager::ScrollState,
    /// Whether the alt-screen pager renders this screen. Gates the pager-only
    /// scroll keys so inline-mode input routing is untouched.
    pub(crate) pager_active: bool,
    /// Desired mouse-capture state in pager mode (Ctrl+T / `/mouse` toggle it;
    /// `TuiUi::draw` syncs the terminal to it). Off restores terminal-native
    /// select/copy; the composer statusline shows `○ mouse off` while off.
    pub(crate) mouse_capture: bool,
    /// Wheel scroll step in lines (pager mode; `tui.scrollSpeed`, default 3).
    pub(crate) scroll_speed: u16,
    /// Freeze animations for accessibility (`IRIS_REDUCED_MOTION` /
    /// `tui.reducedMotion`). Seeded from the env flag at construction and
    /// refined from the persisted preference via [`Screen::set_reduced_motion`]
    /// (env still wins), so pure UI unit tests stay isolated from machine config.
    reduced_motion: bool,
    /// Pager focus: `true` while the scrollback pane has keyboard focus (Tab
    /// toggles; typing a printable character always returns to the prompt;
    /// Esc is never a focus key -- ADR-0029).
    pub(crate) scrollback_focus: bool,
    /// Selected scrollback entry (a panel-header transcript row index) while
    /// the scrollback pane is focused.
    pub(crate) selected_entry: Option<usize>,
    /// Active transcript search (`/find`), pager mode only.
    pub(crate) search: Option<SearchState>,
    /// One-shot scroll target consumed by the next pager compose (search
    /// jumps): reveal without pinning the view.
    pub(crate) reveal_line: Option<usize>,
    /// Clickable OSC 8 link regions for the last composed pager frame, keyed by
    /// frame `(row, column)`. Rebuilt every compose from the frame's link
    /// markers (which are stripped before the cells reach the ratatui `Buffer`,
    /// since it cannot carry OSC 8), so a mouse click resolves to a target via
    /// [`Screen::pager_link_at`].
    pub(crate) pager_links: Vec<crate::ui::hyperlink::LinkRegion>,
    /// Previously submitted prompts for shell-style Up/Down recall (newest at end).
    prompt_history: Vec<String>,
    /// Current prompt-history cursor while browsing; `None` means editing a fresh draft.
    prompt_history_cursor: Option<usize>,
    /// Pager sticky prompt disclosure state. A newly pinned prompt starts
    /// collapsed so it names the governing turn without taking over the pane.
    pub(crate) sticky_prompt_expanded: bool,
    /// Session-scoped fold/compaction accounting for the `/context` breakdown
    /// (issue #400, design §5.1): accumulated from the display-event stream in
    /// [`Screen::apply`]. Covers THIS process's events only -- reductions from
    /// a prior process are visible structurally (stubs/summaries in context)
    /// but their reclaimed mass is not re-derived.
    pub(crate) context_accounting: ContextAccounting,
}

/// Session-scoped reduction totals for the `/context` breakdown (issue #400):
/// every fold batch with its trigger tag, and every compaction's before/after
/// estimates, as reported by the runtime events (never fabricated).
#[derive(Debug, Default)]
pub(crate) struct ContextAccounting {
    /// One entry per fold flush: `(trigger code, folds, reclaimed tokens)`.
    pub(crate) fold_batches: Vec<(&'static str, usize, u64)>,
    /// One entry per compaction: `(original tokens, summary tokens)`.
    pub(crate) compactions: Vec<(u64, u64)>,
}

impl ContextAccounting {
    /// Total tokens reclaimed by fold flushes this session.
    pub(crate) fn folded_reclaimed(&self) -> u64 {
        self.fold_batches
            .iter()
            .map(|(_, _, reclaimed)| *reclaimed)
            .fold(0, u64::saturating_add)
    }
}

/// `/find` state: the query plus the current match position. Match lines are
/// recomputed on every jump (the transcript moves under the search), so only
/// the current position is retained.
#[derive(Debug)]
pub(crate) struct SearchState {
    pub(crate) query: String,
    /// 1-based position of the current match, for the `k/N` indicator.
    pub(crate) position: usize,
    pub(crate) total: usize,
    /// Visible-line index of the current match at the last jump, resolved
    /// after any fold reveal so the highlight lands on a rendered row.
    pub(crate) line: Option<usize>,
    /// Identity of the current match as a `(row, sub-line)` pair, so `n`/`N`
    /// re-anchor on the same match across transcript mutations and fold
    /// reveals (which renumber visible lines).
    anchor: Option<(usize, usize)>,
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
            awaiting_approval: false,
            footer: None,
            switch_status: None,
            modal: None,
            queued: 0,
            phase: WorkPhase::default(),
            terminal_focused: true,
            approval_policy: ApprovalPolicy::OnRequest,
            start_page: None,
            session_menu: None,
            last_session_bar: None,
            scroll: super::pager::ScrollState::default(),
            pager_active: false,
            mouse_capture: true,
            scroll_speed: 3,
            reduced_motion: reduced_motion(),
            scrollback_focus: false,
            selected_entry: None,
            search: None,
            reveal_line: None,
            pager_links: Vec::new(),
            prompt_history: Vec::new(),
            prompt_history_cursor: None,
            sticky_prompt_expanded: false,
            context_accounting: ContextAccounting::default(),
        }
    }

    /// Resolve a pager-frame click `(row, column)` to an OSC 8 link target, if
    /// any. Pure lookup over the regions the last compose recorded; a click
    /// outside every link region returns `None`.
    pub(crate) fn pager_link_at(&self, row: u16, column: u16) -> Option<&str> {
        crate::ui::hyperlink::region_at(&self.pager_links, usize::from(row), usize::from(column))
            .map(|region| region.uri.as_str())
    }

    /// Start (or clear, with an empty query) a transcript search. Jumps to the
    /// newest match and focuses the scrollback so `n`/`N` navigate. Returns
    /// the match count, or `None` when the search was cleared.
    pub(crate) fn start_search(&mut self, query: &str) -> Option<usize> {
        let query = query.trim();
        if query.is_empty() {
            self.search = None;
            return None;
        }
        self.search = Some(SearchState {
            query: query.to_string(),
            position: 0,
            total: 0,
            line: None,
            anchor: None,
        });
        self.scrollback_focus = true;
        self.search_step(0);
        Some(self.search.as_ref().map_or(0, |state| state.total))
    }

    /// Move the search cursor: `-1` = older (up), `+1` = newer (down), `0` =
    /// (re)select the newest match. Matches are recomputed against the current
    /// transcript; the jump target is queued for the next pager compose.
    pub(crate) fn search_step(&mut self, direction: isize) -> bool {
        let Some(state) = self.search.as_ref() else {
            return false;
        };
        let query = state.query.clone();
        let prev = state.anchor;
        let matches = self.transcript.search_matches(&query);
        let total = matches.len();
        if matches.is_empty() {
            let state = self.search.as_mut().expect("search active");
            state.total = 0;
            state.position = 0;
            state.line = None;
            state.anchor = None;
            return false;
        }
        // Re-anchor on the previous match where possible, else the newest.
        // Matches are sorted ascending by (row, sub), so this is stable across
        // appends and fold reveals that renumber visible lines.
        let anchor = prev
            .and_then(|key| matches.iter().position(|m| (m.row, m.sub) >= key))
            .unwrap_or(matches.len() - 1);
        let index = match direction {
            0 => matches.len() - 1,
            d if d < 0 => anchor.saturating_sub(1),
            _ => (anchor + 1).min(matches.len() - 1),
        };
        let (row, sub) = (matches[index].row, matches[index].sub);
        // Reveal the fold if the match is hidden, then resolve its visible line
        // against the refreshed layout so the jump lands on a rendered row.
        let line = self.transcript.reveal_and_locate(row, sub);
        let state = self.search.as_mut().expect("search active");
        state.total = total;
        state.position = index + 1;
        state.anchor = Some((row, sub));
        state.line = line;
        self.reveal_line = line;
        true
    }

    /// Tab: toggle prompt <-> scrollback focus (pager only). Entering the
    /// scrollback selects the newest entry when none is selected yet.
    pub(crate) fn toggle_scrollback_focus(&mut self) -> bool {
        self.scrollback_focus = !self.scrollback_focus;
        if self.scrollback_focus && self.selected_entry.is_none() {
            self.selected_entry = self.transcript.panel_header_rows().last().copied();
        }
        self.scrollback_focus
    }

    /// Return focus to the prompt (typing always wins).
    pub(crate) fn focus_prompt(&mut self) {
        self.scrollback_focus = false;
    }

    /// Move the entry selection up (`-1`) or down (`+1`). With no selectable
    /// entries the keys fall back to one-line scrolling so the pane still
    /// responds. Selection drift after history trimming snaps to the nearest
    /// entry.
    pub(crate) fn move_selection(&mut self, delta: isize) {
        let headers = self.transcript.panel_header_rows();
        if headers.is_empty() {
            if delta < 0 {
                self.scroll.scroll_up(1);
            } else {
                self.scroll.scroll_down(1);
            }
            return;
        }
        let current = self
            .selected_entry
            .and_then(|row| headers.iter().position(|&h| h >= row))
            .unwrap_or(headers.len().saturating_sub(1));
        let next = current.saturating_add_signed(delta).min(headers.len() - 1);
        self.selected_entry = Some(headers[next]);
    }

    /// Validate the selected entry against the CURRENT panel headers, snapping
    /// a stale index (history trim, panel rebuild) to the nearest header (or
    /// clearing it when none exist). The single normalization path used by
    /// every reveal/highlight/fold action.
    pub(crate) fn normalized_selection(&mut self) -> Option<usize> {
        let row = self.selected_entry?;
        if self.transcript.panel_expanded_at(row).is_some() {
            return Some(row);
        }
        let headers = self.transcript.panel_header_rows();
        let snapped = headers
            .iter()
            .rev()
            .find(|&&header| header <= row)
            .or_else(|| headers.first())
            .copied();
        self.selected_entry = snapped;
        snapped
    }

    /// Fold (`false`) or reveal (`true`) the selected entry's panel.
    pub(crate) fn set_selected_expanded(&mut self, expand: bool) -> bool {
        let Some(row) = self.normalized_selection() else {
            return false;
        };
        self.transcript.set_panel_expanded_at(row, expand)
    }

    /// Toggle the selected entry's fold (Enter while scrollback-focused).
    pub(crate) fn toggle_selected_entry(&mut self) -> bool {
        let Some(row) = self.normalized_selection() else {
            return false;
        };
        match self.transcript.panel_expanded_at(row) {
            Some(expanded) => self.transcript.set_panel_expanded_at(row, !expanded),
            None => false,
        }
    }

    /// Visible line index of a transcript row under the warm wrap cache
    /// (pager selection reveal/highlight).
    pub(crate) fn transcript_line_of_row(&self, row: usize) -> Option<usize> {
        self.transcript.visible_line_of_row(row)
    }

    /// Flip the desired pager mouse-capture state; returns the new state.
    pub(crate) fn toggle_mouse(&mut self) -> bool {
        self.mouse_capture = !self.mouse_capture;
        self.mouse_capture
    }

    /// Show the start page (IrisMark + launcher) until the session begins.
    /// `recoverable` is the count of recoverable Iris tasks in this workspace at
    /// launch, surfaced as a dim badge on the `Tasks` row (ADR-0031) instead of
    /// popping a picker over the home menu.
    pub(crate) fn show_start_page(&mut self, recoverable: usize) {
        self.start_page = Some(StartPage::new(self.reduced_motion, recoverable));
    }

    /// Apply the resolved reduced-motion posture (env flag OR persisted
    /// `tui.reducedMotion`) after construction, mirroring how `scroll_speed`
    /// and the alt-screen policy are threaded from `tui_settings`. Updates the
    /// live spinner too so an already-built screen honors the preference.
    pub(crate) fn set_reduced_motion(&mut self, reduced_motion: bool) {
        self.reduced_motion = reduced_motion;
        self.spinner.reduced_motion = reduced_motion;
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

    /// Replace the transient model-switch status chip. The next provider turn
    /// will convert it from predicted cache/context impact to realized usage.
    pub(crate) fn set_switch_status(&mut self, status: SwitchStatus) {
        self.switch_status = Some(status);
    }

    // --- modal/picker ---

    /// Open a picker/dialog above the editor until it closes. A docked modal
    /// takes precedence over a SessionBar dropdown: opening one closes it.
    pub(crate) fn open_modal(&mut self, modal: Modal) {
        self.session_menu = None;
        self.modal = Some(modal);
    }

    // --- session-bar dropdowns ---

    /// Open a SessionBar dropdown. Exclusive slot: an already-open dropdown
    /// (either kind) is replaced.
    pub(crate) fn open_session_menu(&mut self, menu: SessionMenu) {
        self.session_menu = Some(menu);
    }

    pub(crate) fn close_session_menu(&mut self) {
        self.session_menu = None;
    }

    /// Whether a turn is running, i.e. an open dropdown is a readout.
    pub(crate) fn menu_readonly(&self) -> bool {
        self.spinner.active
    }

    /// Update the last-known VCS snapshot (and an open VCS dropdown's copy).
    pub(crate) fn set_footer_vcs(&mut self, vcs: Option<VcsStatus>) {
        let Some(vcs) = vcs else {
            return;
        };
        if let Some(SessionMenu::Git(menu)) = &mut self.session_menu {
            match &vcs {
                VcsStatus::Git(status) => menu.set_status(status.clone()),
                _ => self.session_menu = None,
            }
        } else if let Some(SessionMenu::Jj(menu)) = &mut self.session_menu {
            match &vcs {
                VcsStatus::Jj(status) => menu.set_status(status.clone()),
                _ => self.session_menu = None,
            }
        }
        if let Some(footer) = &mut self.footer {
            footer.vcs = Some(vcs);
        }
    }

    /// Update the last-known git snapshot (and an open git dropdown's copy).
    #[cfg(test)]
    pub(crate) fn set_footer_git(&mut self, git: Option<GitStatus>) {
        self.set_footer_vcs(git.map(VcsStatus::Git));
    }

    #[cfg(test)]
    pub(crate) fn set_footer_jj(&mut self, jj: Option<JjStatus>) {
        self.set_footer_vcs(jj.map(VcsStatus::Jj));
    }

    /// The last-known git snapshot, if any.
    pub(crate) fn footer_git(&self) -> Option<&GitStatus> {
        self.footer
            .as_ref()
            .and_then(|footer| footer.vcs.as_ref())
            .and_then(VcsStatus::as_git)
    }

    pub(crate) fn footer_vcs(&self) -> Option<&VcsStatus> {
        self.footer.as_ref().and_then(|footer| footer.vcs.as_ref())
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
        } else if self.session_menu.is_some() {
            FocusTarget::SessionMenu
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
        !self.spinner.active
            && self.modal.is_none()
            && !self.awaiting_approval
            && self.session_menu.is_none()
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
        if let UiEvent::ContextPressure { tier, measured, .. } = &event
            && let Some(footer) = &mut self.footer
        {
            footer.context_used_tokens = Some(*measured);
            footer.context_pressure = *tier;
        }
        // Accumulate the session-scoped reduction accounting for `/context`
        // (issue #400): fold batches with their trigger tags, and compaction
        // before/after estimates, straight from the runtime events.
        match &event {
            UiEvent::FoldApplied {
                folds,
                reclaimed_tokens_estimate,
                trigger,
                ..
            } => self.context_accounting.fold_batches.push((
                trigger.code(),
                *folds,
                *reclaimed_tokens_estimate,
            )),
            UiEvent::CompactionApplied {
                original_tokens_estimate,
                summary_tokens_estimate,
                ..
            } => self
                .context_accounting
                .compactions
                .push((*original_tokens_estimate, *summary_tokens_estimate)),
            _ => {}
        }
        if let Some(status) = &mut self.switch_status {
            status.observe(&event);
        }
        // Advance the always-visible work phase from the display-event stream.
        // Approval transitions are owned by `show_approval`/`clear_approval`, so
        // they are not derived here; every other event that implies a phase
        // updates the label. `None` keeps the current phase (e.g. a running
        // tool's output deltas do not change the RunningTool label).
        if let Some(phase) = WorkPhase::on_event(&event) {
            self.phase = phase;
        }
        if matches!(event, UiEvent::UserMessage(_)) {
            self.sticky_prompt_expanded = false;
        }
        // `UiEvent::UserMessage` (a mid-run injected steering/follow-up message)
        // is committed as a user row inside `transcript.apply`, so order matches
        // provider context; the initial prompt is committed by the session
        // driver via `commit_user`.
        self.transcript.apply(event);
    }

    /// Commit a submitted prompt into the transcript as a user line.
    pub(crate) fn commit_user(&mut self, text: &str) {
        self.sticky_prompt_expanded = false;
        self.transcript.commit_user(text);
    }

    /// Toggle the pager's sticky prompt between its one-line collapsed header and
    /// expanded body. Returns false when no sticky prompt is currently visible.
    pub(crate) fn toggle_sticky_prompt(&mut self) -> bool {
        let top = self.scroll.top();
        if top == 0 || self.transcript.sticky_prompt_text(top).is_none() {
            return false;
        }
        self.sticky_prompt_expanded = !self.sticky_prompt_expanded;
        true
    }

    /// Pager-mode sticky-prompt disclosure click: the pinned prompt starts on the
    /// row immediately below the session bar, so a click there toggles the same
    /// state as Ctrl+O. Other rows fall through to transcript header/link/wheel
    /// handling.
    pub(crate) fn toggle_sticky_prompt_at_screen_row(&mut self, screen_row: u16) -> bool {
        let (width, height) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
        let bar_rows = session_bar_lines(self, width, height).len();
        if usize::from(screen_row) != bar_rows {
            return false;
        }
        self.toggle_sticky_prompt()
    }

    /// Render all transcript rows plus any in-flight stream, wrapped to `width`.
    /// Finalized history is intentionally retained here; the terminal surface
    /// owns append/diff/full-replay decisions instead of draining UI state.
    /// Total visible transcript lines at `width` (pager layout math).
    pub(super) fn transcript_visible_total(&mut self, width: u16) -> usize {
        self.transcript.visible_total(width)
    }

    /// Clone the visible transcript window `[top .. top+rows)` (pager render).
    pub(super) fn transcript_window(
        &mut self,
        width: u16,
        top: usize,
        rows: usize,
    ) -> Vec<Line<'static>> {
        self.transcript.render_window(width, top, rows)
    }

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

    /// Take the current editor text, record it for recall, and reset to a fresh empty editor.
    pub(crate) fn submit(&mut self) -> String {
        let text = self.editor_text();
        if !text.trim().is_empty() && self.prompt_history.last() != Some(&text) {
            self.prompt_history.push(text.clone());
        }
        self.prompt_history_cursor = None;
        self.editor = fresh_editor();
        self.palette.sync("");
        text
    }

    /// Clear the editor without submitting (Ctrl-C on non-empty input).
    pub(crate) fn clear_editor(&mut self) {
        self.prompt_history_cursor = None;
        self.editor = fresh_editor();
        self.palette.sync("");
    }

    /// Replace the editor contents with `text` (palette command completion).
    pub(crate) fn set_editor(&mut self, text: &str) {
        self.prompt_history_cursor = None;
        let mut editor = fresh_editor();
        editor.insert_str(text);
        self.editor = editor;
        self.sync_palette();
    }

    pub(crate) fn browsing_prompt_history(&self) -> bool {
        self.prompt_history_cursor.is_some()
    }

    pub(crate) fn reset_prompt_history_cursor(&mut self) {
        self.prompt_history_cursor = None;
    }

    pub(crate) fn prompt_history_previous(&mut self) -> bool {
        let Some(next) = self
            .prompt_history_cursor
            .map(|cursor| cursor.saturating_sub(1))
            .or_else(|| self.prompt_history.len().checked_sub(1))
        else {
            return false;
        };
        if self.prompt_history_cursor == Some(0) {
            return false;
        }
        self.prompt_history_cursor = Some(next);
        let text = self.prompt_history[next].clone();
        self.replace_editor_from_history(&text);
        true
    }

    pub(crate) fn prompt_history_next(&mut self) -> bool {
        let Some(cursor) = self.prompt_history_cursor else {
            return false;
        };
        if cursor + 1 >= self.prompt_history.len() {
            self.prompt_history_cursor = None;
            self.replace_editor_from_history("");
            return true;
        }
        let next = cursor + 1;
        self.prompt_history_cursor = Some(next);
        let text = self.prompt_history[next].clone();
        self.replace_editor_from_history(&text);
        true
    }

    fn replace_editor_from_history(&mut self, text: &str) {
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
        // The VCS snapshot is orthogonal to the model/context identity: always
        // carried across a footer rebuild (the loop refreshes it separately).
        let vcs = self.footer.as_mut().and_then(|footer| footer.vcs.take());
        // Mirror the meter's context cap into the transcript so tool-footer
        // diagnostics can scale their `ctx` growth delta against it.
        self.transcript
            .set_context_cap(context.as_deref().and_then(parse_context_window));
        self.footer = Some(Footer {
            model,
            effort,
            context,
            cwd,
            vcs,
            usage,
            context_used_tokens,
            context_pressure: ContextPressureTier::Normal,
        });
    }

    pub(crate) fn start_turn(&mut self) {
        // A submitted task enters the session: the launcher gives way to the
        // normal transcript, under the same chrome.
        self.start_page = None;
        // A realized switch chip is useful while idle after the handoff; clear it
        // when the user starts another turn. A still-pending chip survives so the
        // next provider request can realize it.
        if self
            .switch_status
            .as_ref()
            .is_some_and(|status| !status.pending())
        {
            self.switch_status = None;
        }
        // Pager: a submitted prompt snaps the view back to the live tail.
        self.scroll.follow_latest();
        self.spinner.start();
        self.phase = WorkPhase::Starting;
        self.turn_divider = TurnDivider::default();
        self.awaiting_approval = false;
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
        self.awaiting_approval = false;
    }

    /// Advance the spinner one frame. Returns whether anything animated (so the
    /// loop only redraws on a tick while a turn is running). While an approval is
    /// shown the spinner is hidden behind the hint, so a tick changes nothing and
    /// requests no redraw -- the loop stays CPU-idle waiting on the decision.
    pub(crate) fn tick(&mut self) -> bool {
        if self.awaiting_approval {
            return false;
        }
        // The start page's IrisMark reuses the spinner tick machinery and holds
        // still under reduced motion (StartPage::tick handles both cadence and
        // freeze). It must keep ticking even in an inactive terminal pane: tmux
        // users can see adjacent panes, and a frozen Iris pane reads as stalled.
        if let Some(page) = &mut self.start_page {
            return page.tick();
        }
        self.spinner.tick()
    }

    /// Drive one paced assistant-stream commit tick: migrate newly-stable
    /// streamed lines into scrollback. Returns `true` when rows were committed
    /// (a redraw is due). Called from the render loop's tick while a turn runs.
    pub(crate) fn commit_stream_tick(&mut self, now: std::time::Instant) -> bool {
        self.transcript.commit_stream_tick(now)
    }

    /// Whether the assistant stream still has content to pace into scrollback.
    pub(crate) fn has_stream_work(&self) -> bool {
        self.transcript.has_stream_work()
    }

    // --- approval ---

    /// Show a gated tool's approval prompt in the status row. The transcript
    /// records the final approval/denial outcome, not the transient prompt.
    /// Enter the awaiting-approval state. The review itself renders inside the
    /// gated tool block (the `▲ REVIEW` state, via the `ToolReview` event); this
    /// only claims the input surface and marks the phase so the composer freezes
    /// and the working indicator steps aside while the user decides.
    pub(crate) fn show_approval(&mut self) {
        // The review takes the input surface: close any dropdown.
        self.session_menu = None;
        self.phase = WorkPhase::AwaitingApproval;
        self.awaiting_approval = true;
    }

    /// Fold a manual approval decision into the gated tool block's own footer
    /// (the muted `approved …` note) — approvals never render as a separate
    /// panel. Denials flow through the `ToolDenied` event.
    pub(crate) fn note_approval(&mut self, call: &ToolCall, decision: ApprovalDecision) {
        self.transcript.note_approval(call, decision);
    }

    /// Clear the docked approval prompt. `approved` selects the phase to resume:
    /// an approved call is about to run (`PreparingTool`, refined to
    /// `RunningTool` by the next `ToolStarted`), while a denial, cancellation,
    /// or error is winding the turn down (`Finishing`). The change is guarded on
    /// the phase still being `AwaitingApproval`, so a terminal event applied
    /// just before clearing (the turn-error cleanup path applies its event
    /// first) is never overwritten.
    pub(crate) fn clear_approval(&mut self, approved: bool) {
        self.awaiting_approval = false;
        if matches!(self.phase, WorkPhase::AwaitingApproval) {
            self.phase = if approved {
                WorkPhase::PreparingTool
            } else {
                WorkPhase::Finishing
            };
        }
    }

    /// ctrl+o: expand every foldable panel if any is collapsed, else collapse
    /// them all. Returns whether anything changed.
    pub(crate) fn toggle_all_panels(&mut self) -> bool {
        self.transcript.toggle_all_panels()
    }

    #[cfg(test)]
    pub(crate) fn toggle_latest_panel(&mut self) -> bool {
        self.transcript.toggle_latest_panel()
    }

    /// Map a pager-mode screen row to the foldable header under it and toggle
    /// that one block. Returns whether a header toggled (false for clicks
    /// outside any header row). Frame layout: session-bar rows, then the
    /// transcript window at the current scroll top, so the clicked visible line
    /// is `scroll.top() + (row - bar_rows)`.
    pub(crate) fn toggle_header_at_screen_row(&mut self, screen_row: u16) -> bool {
        let (width, height) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
        let bar_rows = session_bar_lines(self, width, height).len();
        let row = usize::from(screen_row);
        if row < bar_rows {
            return false;
        }
        let line = self.scroll.top() + (row - bar_rows);
        let Some(header) = self.transcript.header_row_at_visible_line(line) else {
            return false;
        };
        let expanded = self.transcript.panel_expanded_at(header).unwrap_or(false);
        self.transcript.set_panel_expanded_at(header, !expanded)
    }

    #[cfg(test)]
    pub(crate) fn latest_panel_collapsed(&self) -> bool {
        self.transcript.latest_panel_collapsed()
    }

    pub(super) fn working_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.spinner.active && !self.awaiting_approval {
            working_lines(
                self.spinner.frame(),
                self.spinner.elapsed(),
                self.footer.as_ref(),
                Some(self.phase.label()),
                self.queued,
                usize::from(width),
            )
        } else {
            Vec::new()
        }
    }

    /// The current work-phase label (test-only): lets phase-transition tests
    /// assert the phase even when the working header is suppressed (approval).
    #[cfg(test)]
    pub(crate) fn work_phase_label(&self) -> &str {
        self.phase.label()
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
    // The session bar (bar + soft hairline) is reserved ahead of the
    // transcript, so its stability must be decided BEFORE choosing the
    // transcript render mode: when the bar changed (context meter movement,
    // branch switch), `stable_prefix` resets to 0 and the surface will not
    // replay any reused rows -- an incremental transcript suffix would then
    // silently drop the unchanged transcript prefix from the frame. A bar-only
    // change therefore forces a full transcript render (same cache, so the
    // next frame's incremental baseline stays correct).
    let bar = session_bar_lines(screen, width, height);
    let bar_rows = bar.len();
    let bar_stable = screen
        .last_session_bar
        .as_ref()
        .is_some_and(|(prev_width, prev)| *prev_width == width && *prev == bar);
    if !bar_stable {
        screen.last_session_bar = Some((width, bar.clone()));
    }
    let transcript = if incremental && bar_stable {
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
    // Inline (ADR-0006) is a scrollback-append surface: the transcript flows
    // into the terminal's own scrollback and only a COMPACT volatile tail
    // (bar + working indicator + composer) is repainted. The document is
    // therefore content-height, not viewport-height -- there is no blank body
    // padding the transcript out to the bottom of the pane, and no pager-shaped
    // full-height frame is ever appended into native scrollback (issue #353;
    // ADR-0029 keeps inline as the honest fallback).
    //
    // The one exception is the start page: with an empty transcript the
    // launcher is centered in the pane, so its filler DOES span the viewport
    // (the IrisMark + menu block, vertically centered). This is pre-session
    // chrome only; entering a session (`start_turn`, `leave_start_page`,
    // resume adoption) clears `start_page`, collapsing the tail to compact.
    let tail_rows = chrome.len() + working_block.len();
    let filler_rows = if screen.start_page.is_some() {
        usize::from(height)
            .saturating_sub(tail_rows)
            .saturating_sub(transcript.total_lines)
            .saturating_sub(bar_rows)
    } else {
        0
    };
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
    // Reused leading rows are EXCLUDED from the emitted document and replayed
    // from the previous frame via `stable_prefix` -- exactly how the transcript
    // omits its own stable prefix (`transcript.lines` is already the changed
    // suffix). The session bar is the other stable leading block: when it is
    // unchanged AND the incremental surface will honor the hint, emitting the
    // bar here too would append it a SECOND time on top of the reused copy,
    // duplicating the bar in the surface document and scrolling a stale bar into
    // native scrollback (issue #353). The non-incremental render keeps the full
    // bar because that path does not honor `stable_prefix`.
    let reuse_bar = incremental && bar_stable;
    let mut document = if reuse_bar { Vec::new() } else { bar };
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
pub(super) fn filler_lines(screen: &Screen, filler_rows: usize, width: u16) -> Vec<Line<'static>> {
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
/// Uses the themed `muted` role so the meter follows named themes too (it hard-
/// coded `DarkGray` before, which ignored the active theme's grey); the hollow
/// `○`/solid `●` glyphs still carry filled-vs-empty independent of color.
fn meter_used_style() -> Style {
    Style::default().fg(crate::ui::palette::muted())
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
    // Pager-only state hint while mouse reporting is toggled off (Ctrl+T):
    // terminal-native selection is active. Symbol + label, never color alone.
    let mouse_off = screen.pager_active && !screen.mouse_capture;
    let mouse_seg = || {
        vec![
            Span::styled(format!("{} ", crate::ui::symbols::EMPTY), dim_style()),
            Span::styled("mouse off".to_string(), dim_style()),
        ]
    };

    // Candidates from fullest to minimum. The drop order is monotonic and
    // matches the spec: drop the mouse hint, then the policy segment, then
    // effort, leaving the minimum `◉ CODE ─ MODEL`.
    let mut candidates: Vec<Vec<Vec<Span<'static>>>> = Vec::new();
    if mouse_off {
        candidates.push(vec![
            mode_seg(),
            model_with_effort(),
            policy_seg(),
            mouse_seg(),
        ]);
    }
    candidates.extend([
        vec![mode_seg(), model_with_effort(), policy_seg()],
        vec![mode_seg(), model_with_effort()],
        vec![mode_seg(), model_only()],
    ]);

    let spans = candidates
        .into_iter()
        .find_map(|segments| statusline_left(width, segments))?;
    let mut line = Line::from(spans);
    truncate_line(&mut line, width.max(1));
    Some(line)
}

/// One-line, composer-adjacent status for runtime model/reasoning switches. It
/// is volatile chrome, not transcript history: routine switch confirmations live
/// here while durable analytics remain in the session log and `/context`.
fn switch_status_line(screen: &Screen, width: u16) -> Option<Line<'static>> {
    let status = screen.switch_status.as_ref()?;
    if width < 6 {
        return None;
    }
    let inset = BOX_X_PADDING_U16.min(width.saturating_sub(1));
    let box_width = width.saturating_sub(inset.saturating_mul(2)).max(1);
    let mut line = Line::from(status.spans());
    truncate_line(&mut line, usize::from(box_width));
    pad_line_left(&mut line, usize::from(inset));
    truncate_line(&mut line, usize::from(width));
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
    let cwd = strip_ansi_for_text(&footer.cwd);
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
        let pressure = !matches!(footer.context_pressure, ContextPressureTier::Normal);
        let mut spans = vec![
            Span::styled(
                if pressure { "CTX! " } else { "CTX " }.to_string(),
                if pressure {
                    border_style()
                } else {
                    dim_style()
                },
            ),
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

    let git_open = matches!(screen.session_menu, Some(SessionMenu::Git(_)));
    let jj_open = matches!(screen.session_menu, Some(SessionMenu::Jj(_)));
    let tree_open = matches!(screen.session_menu, Some(SessionMenu::Tree(_)));
    // VCS segment candidates, fullest first. Git keeps its existing
    // degradation levels; jj drops description, then counts, then base.
    let vcs_levels: Vec<Vec<Span<'static>>> = footer
        .vcs
        .as_ref()
        .map(|status| {
            (0..5u8)
                .map(|level| match status {
                    VcsStatus::Git(git) => git_segment_spans(git, level, git_open),
                    VcsStatus::Jj(jj) => jj_segment_spans(jj, level, jj_open),
                })
                .collect()
        })
        .unwrap_or_default();

    // A middle-truncated cwd keeps at least `…/<project>`-ish room before a
    // lower-priority segment is dropped instead.
    const CWD_MIN: usize = 12;

    // Drop order: meter → `/<cap>` → VCS counts/details → whole VCS segment →
    // hard cwd truncation. Minimum form: cwd alone.
    let mut candidates: Vec<(Option<usize>, Option<usize>)> = Vec::new();
    if vcs_levels.is_empty() {
        candidates.extend([(Some(0), None), (Some(1), None), (Some(2), None)]);
    } else {
        // Git level 0 = the explicit task badge; it is held while the right side
        // collapses, then degrades to the authoritative count form (level 1)
        // and follows the spec drop order (counts -> no counts -> base).
        candidates.extend([
            (Some(0), Some(0)),
            (Some(1), Some(0)),
            (Some(2), Some(0)),
            (Some(2), Some(1)),
            (Some(2), Some(2)),
            (Some(2), Some(3)),
            (Some(2), Some(4)),
            (Some(2), None),
        ]);
    }
    candidates.push((None, None));
    let tree_prefix =
        tree_open.then(|| Span::styled(format!("{} ", crate::ui::symbols::EXPANDED), dim_style()));
    let prefix_w = if tree_open { 2 } else { 0 };
    for (right_idx, git_idx) in candidates {
        let right = right_idx.map(|index| &right_candidates[index]);
        let vcs_spans: Vec<Span<'static>> = git_idx
            .map(|index| vcs_levels[index].clone())
            .unwrap_or_default();
        let right_w = right.map(|spans| spans_width(spans)).unwrap_or(0);
        let gap = if right_w > 0 { 2 } else { 0 };
        let avail_cwd = width
            .saturating_sub(right_w)
            .saturating_sub(gap)
            .saturating_sub(spans_width(&vcs_spans))
            .saturating_sub(prefix_w);
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
        let mut spans = Vec::new();
        if let Some(prefix) = tree_prefix.clone() {
            spans.push(prefix);
        }
        spans.push(Span::styled(shown_cwd.clone(), Style::default()));
        spans.extend(vcs_spans);
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

/// The session-bar git segment (` ┊ git <branch> [state cluster]`) at a drop
/// level: 0 = the explicit task badge (`±N ◇M task <id8>`), 1 = the
/// authoritative `±N ◇M` count form, 2 = counts reduced to the `±` half,
/// 3 = no counts, 4 = base only. Mutually exclusive base states in precedence
/// order: unmerged `▲N` (overrides everything) → task-partitioned `±N ◇M`
/// (either half omitted at zero; the fullest form appends `task <id8>` as an
/// explicit first-class badge, ADR-0031) → plain dirty `±N` → clean (no glyph
/// — silence is the signal). `▾ ` prefixes the segment only while the git
/// dropdown is open. The badge is an additive fuller tier above the
/// design-language `±N ◇M` cluster: it degrades to that exact form (level 1)
/// before the spec's own drop order applies, so narrow widths never overflow.
fn git_segment_spans(status: &GitStatus, level: u8, open: bool) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        format!(" {} ", crate::ui::symbols::SEP),
        dim_style(),
    )];
    if open {
        spans.push(Span::styled(
            format!("{} ", crate::ui::symbols::EXPANDED),
            dim_style(),
        ));
    }
    spans.push(Span::styled("git ".to_string(), dim_style()));
    match (&status.branch, &status.detached_at) {
        (Some(branch), _) => spans.push(Span::styled(branch.clone(), dim_style())),
        (None, at) => {
            let sha = at
                .as_deref()
                .and_then(|a| a.split_whitespace().next())
                .unwrap_or("?")
                .to_string();
            spans.push(Span::styled(
                format!("{} ", crate::ui::symbols::ERROR),
                err_style(),
            ));
            spans.push(Span::styled(format!("detached @ {sha}"), dim_style()));
        }
    }
    if level <= 2 {
        if status.unmerged > 0 {
            spans.push(Span::styled(
                format!(" {}{}", crate::ui::symbols::REVIEW, status.unmerged),
                prompt_style(),
            ));
        } else if let Some(task) = status.task.as_ref() {
            if status.user_dirty > 0 {
                spans.push(Span::styled(
                    format!(" {}{}", crate::ui::symbols::DIRTY, status.user_dirty),
                    prompt_style(),
                ));
            }
            // Iris-task half: the explicit badge (count + short id) at the
            // fullest level, the authoritative count at level 1, dropped at
            // level 2. The preview glyph always leads, so the unsettled-task
            // signal is present even when no ledger file currently matches tip.
            match level {
                0 => {
                    let short: String = task.task_id.chars().take(8).collect();
                    let count = if status.iris_unsettled > 0 {
                        status.iris_unsettled.to_string()
                    } else {
                        String::new()
                    };
                    spans.push(Span::styled(
                        format!(" {}{} task {}", crate::ui::symbols::PREVIEW, count, short),
                        dim_style(),
                    ));
                }
                1 if status.iris_unsettled > 0 => {
                    spans.push(Span::styled(
                        format!(" {}{}", crate::ui::symbols::PREVIEW, status.iris_unsettled),
                        dim_style(),
                    ));
                }
                _ => {}
            }
        } else if status.total_uncommitted > 0 {
            spans.push(Span::styled(
                format!(" {}{}", crate::ui::symbols::DIRTY, status.total_uncommitted),
                prompt_style(),
            ));
        }
    }
    if level <= 3 && status.is_linked_worktree {
        spans.push(Span::styled(" [WT]".to_string(), dim_style()));
    }
    spans
}

fn jj_segment_spans(status: &JjStatus, level: u8, open: bool) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        format!(" {} ", crate::ui::symbols::SEP),
        dim_style(),
    )];
    if open {
        spans.push(Span::styled(
            format!("{} ", crate::ui::symbols::EXPANDED),
            dim_style(),
        ));
    }
    spans.push(Span::styled("jj ".to_string(), dim_style()));
    spans.push(Span::styled(status.change_id.clone(), dim_style()));
    if level == 0 && !status.description.is_empty() {
        spans.push(Span::styled(" \"".to_string(), dim_style()));
        spans.push(Span::styled(status.description.clone(), dim_style()));
        spans.push(Span::styled("\"".to_string(), dim_style()));
    }
    if level <= 1 {
        if status.conflicted > 0 {
            spans.push(Span::styled(
                format!(" {}{}", crate::ui::symbols::REVIEW, status.conflicted),
                prompt_style(),
            ));
        } else if status.total_changed > 0 {
            spans.push(Span::styled(
                format!(" {}{}", crate::ui::symbols::DIRTY, status.total_changed),
                prompt_style(),
            ));
        }
    }
    spans
}

/// Which half of the session bar a click at display column `x` hits: the cwd
/// (tree dropdown target) or the git segment (git dropdown target). `None`
/// for the right-side context readout / empty fill.
pub(crate) fn session_bar_hit(screen: &Screen, width: u16, x: u16) -> Option<BarSegment> {
    let inset = BOX_X_PADDING_U16.min(width.saturating_sub(1));
    let content_width = width.saturating_sub(inset.saturating_mul(2)).max(1);
    let bar = session_bar(screen, content_width)?;
    let x = usize::from(x.checked_sub(inset)?);
    let text = line_text(&bar);
    let git_sep = format!(" {} git", crate::ui::symbols::SEP);
    let jj_sep = format!(" {} jj", crate::ui::symbols::SEP);
    let vcs_at = text
        .find(&git_sep)
        .or_else(|| text.find(&jj_sep))
        .map(|at| display_width(&text[..at]));
    let left_end = match vcs_at {
        Some(at) => at,
        None => display_width(text.trim_end()),
    };
    if x < left_end {
        return Some(BarSegment::Cwd);
    }
    if let Some(at) = vcs_at {
        // The git segment runs to the start of the right-side fill (two or
        // more spaces) or the end of the text.
        let seg_start = text
            .find(&git_sep)
            .or_else(|| text.find(&jj_sep))
            .unwrap_or(0);
        let seg_text = &text[seg_start..];
        let seg_len = seg_text
            .find("  ")
            .map_or(display_width(seg_text.trim_end()), |end| {
                display_width(&seg_text[..end])
            });
        if x < at + seg_len {
            return Some(BarSegment::Git);
        }
    }
    None
}

/// A session-bar mouse target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BarSegment {
    Cwd,
    Git,
}

/// The session bar block: the bar row, an open dropdown's rows (the tree or
/// git console renders BETWEEN the bar and the hairline, pushing the
/// transcript down), and the soft hairline (a dim `─` repeat, visibly lighter
/// than the composer's border-weight top edge) which becomes the dropdown's
/// closing rule. Inset to the shared pane measure. Empty when there is no
/// footer yet. `height` caps the dropdown at [`MAX_DROPDOWN_ROWS`] or ⅓ of
/// the pane, whichever is smaller.
pub(super) fn session_bar_lines(screen: &Screen, width: u16, height: u16) -> Vec<Line<'static>> {
    let inset = BOX_X_PADDING_U16.min(width.saturating_sub(1));
    let content_width = width.saturating_sub(inset.saturating_mul(2)).max(1);
    let Some(mut bar) = session_bar(screen, content_width) else {
        return Vec::new();
    };
    pad_line_left(&mut bar, usize::from(inset));
    let mut lines = vec![bar];
    if let Some(menu) = &screen.session_menu {
        let max_rows = MAX_DROPDOWN_ROWS.min(usize::from(height) / 3).max(3);
        let referenced = referenced_paths(&screen.editor_text());
        for mut line in menu.render_lines(
            usize::from(content_width),
            max_rows,
            screen.menu_readonly(),
            screen.footer_git(),
            &referenced,
        ) {
            pad_line_left(&mut line, usize::from(inset));
            lines.push(line);
        }
    }
    let mut rule = Line::from(Span::styled(
        "─".repeat(usize::from(content_width)),
        dim_style(),
    ));
    pad_line_left(&mut rule, usize::from(inset));
    lines.push(rule);
    lines
}

/// `@path` tokens in the composer text: the tree dropdown's `◉ open` markers.
fn referenced_paths(input: &str) -> Vec<String> {
    input
        .split_whitespace()
        .filter_map(|token| token.strip_prefix('@'))
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect()
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

pub(super) fn render_editor_chrome(
    screen: &mut Screen,
    width: u16,
    height: u16,
) -> Vec<Line<'static>> {
    let switch_status = if height > MIN_EDITOR_H {
        switch_status_line(screen, width)
    } else {
        None
    };
    let status_rows = u16::from(switch_status.is_some());
    let area = Rect::new(0, 0, width, height.saturating_sub(status_rows));

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
    // Approvals no longer dock here — the review renders inside the gated tool
    // block (`▲ REVIEW`). This region is the modal/palette overlay only.
    let menu_lines: Option<Vec<Line<'static>>> = match screen.focus_for(&input_text) {
        FocusTarget::Modal => screen
            .modal
            .as_ref()
            .map(|modal| Component::render(modal, menu_inner_width)),
        FocusTarget::Palette => {
            Some(PaletteView::for_palette(&screen.palette, &input_text).render(menu_inner_width))
        }
        // A SessionMenu renders at the pane top (session bar), never in
        // the docked menu region above the composer.
        FocusTarget::Editor | FocusTarget::SessionMenu => None,
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
    let mut lines = buffer_to_lines(&buf, cursor_cell);
    if let Some(status) = switch_status {
        lines.insert(0, status);
    }
    lines
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
        ApprovalPolicy, CONTEXT_METER_DOTS, Screen, Spinner, SwitchCacheStatus, SwitchStatus,
        composer_statusline, context_meter_filled, display_width, line_text, parse_context_window,
        session_bar, session_bar_lines, switch_status_line, truncate_cwd_middle,
    };
    use crate::nexus::{ContextPressureTier, ToolCall};
    use crate::ui::UiEvent;
    use crate::ui::tui::WORKING_FRAMES;

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

    /// A snapshot on branch `main`, optionally dirty/partitioned.
    fn git_status(branch: &str) -> crate::git::status::GitStatus {
        crate::git::status::GitStatus {
            branch: Some(branch.to_string()),
            ..Default::default()
        }
    }

    fn git_screen(cwd: &str, status: crate::git::status::GitStatus) -> Screen {
        let mut screen = footer_screen(cwd);
        screen.set_footer_git(Some(status));
        screen
    }

    fn jj_status(change: &str) -> crate::git::status::JjStatus {
        crate::git::status::JjStatus {
            change_id: change.to_string(),
            description: "draft status work".to_string(),
            total_changed: 3,
            log: vec![crate::git::status::JjLogEntry {
                change_id: change.to_string(),
                description: "draft status work".to_string(),
            }],
            ..Default::default()
        }
    }

    fn jj_screen(cwd: &str, status: crate::git::status::JjStatus) -> Screen {
        let mut screen = footer_screen(cwd);
        screen.set_footer_jj(Some(status));
        screen
    }

    fn bar_text(screen: &Screen, width: u16) -> String {
        session_bar(screen, width)
            .map(|l| line_text(&l))
            .expect("bar")
    }

    #[test]
    fn sticky_prompt_click_row_toggles_like_ctrl_o() {
        let mut screen = footer_screen("~/repo (main)");
        screen.pager_active = true;
        screen.commit_user("question that has scrolled away");
        for i in 0..20 {
            screen.apply(UiEvent::Notice(format!("detail {i}")));
        }
        let (width, height) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
        let total = screen.transcript_visible_total(width);
        screen.scroll.sync(total, 5);
        assert!(screen.scroll.top() > 0, "prompt must be pinned");

        let sticky_row = session_bar_lines(&screen, width, height).len() as u16;
        assert!(!screen.toggle_sticky_prompt_at_screen_row(sticky_row + 1));
        assert!(!screen.sticky_prompt_expanded);
        assert!(screen.toggle_sticky_prompt_at_screen_row(sticky_row));
        assert!(screen.sticky_prompt_expanded);
    }

    #[test]
    fn session_bar_shows_location_left_and_context_right() {
        let mut screen = git_screen("~/repo", git_status("main"));
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
    fn session_bar_marks_context_pressure_until_it_returns_to_normal() {
        let mut screen = footer_screen("~/repo");
        screen.apply(UiEvent::ContextPressure {
            tier: ContextPressureTier::Warn,
            measured: 90_000,
            effective_window: 120_000,
            source: crate::nexus::ContextMeasurementSource::Estimated,
        });
        assert!(bar_text(&screen, 80).contains("CTX! 90k"));

        screen.apply(UiEvent::ContextPressure {
            tier: ContextPressureTier::Normal,
            measured: 20_000,
            effective_window: 120_000,
            source: crate::nexus::ContextMeasurementSource::Estimated,
        });
        let bar = bar_text(&screen, 80);
        assert!(bar.contains("CTX 20k"), "{bar:?}");
        assert!(!bar.contains("CTX!"), "{bar:?}");
    }

    #[test]
    fn session_bar_task_badge_degrades_to_count_then_follows_drop_order() {
        // ADR-0031 Unit 2: an unsettled Iris task shows an explicit badge
        // (`±N ◇M task <id8>`) at the fullest width; it degrades to the
        // authoritative `±N ◇M` cluster before any count drops, then follows the
        // spec drop order, never overflowing.
        let screen = git_screen(
            "~/repo",
            crate::git::status::GitStatus {
                user_dirty: 2,
                iris_unsettled: 3,
                total_uncommitted: 5,
                is_linked_worktree: true,
                task: Some(crate::git::status::TaskSummary {
                    task_id: "abcd1234ef99".to_string(),
                    age: Duration::from_secs(60),
                }),
                ..git_status("main")
            },
        );
        // Widest: the explicit task badge (short id) is shown, with WT + meter.
        let full = bar_text(&screen, 100);
        assert!(full.contains("┊ git main ±2 ◇3 task abcd1234"), "{full:?}");
        assert!(full.contains("[WT]"), "{full:?}");
        assert!(full.contains("CTX 0/300k ○○○○○○○○○○"), "{full:?}");

        // Sweep every width: never overflow, and observe the ordered forms --
        // the badge (id shown), then the authoritative `±2 ◇3` count form (no
        // id), then `±2` alone after the iris count drops.
        let mut saw_badge = false;
        let mut saw_count_form = false;
        let mut saw_pm_only = false;
        for width in 1..=100u16 {
            let Some(line) = session_bar(&screen, width) else {
                continue;
            };
            let text = line_text(&line);
            assert!(
                display_width(&text) <= usize::from(width),
                "width {width}: {text:?}"
            );
            let has_id = text.contains("task abcd1234");
            let has_iris = text.contains('◇');
            let has_pm = text.contains("±2");
            if has_id {
                saw_badge = true;
                assert!(has_iris, "the badge keeps the ◇ signal: {text:?}");
            }
            if has_iris && !has_id {
                saw_count_form = true;
            }
            if has_pm && !has_iris {
                saw_pm_only = true;
            }
        }
        assert!(saw_badge, "the explicit id badge appears at some width");
        assert!(
            saw_count_form,
            "the badge degrades to the authoritative `±2 ◇3` cluster (no id)"
        );
        assert!(saw_pm_only, "then `±2` alone after the iris count drops");

        // Narrowest useful form: the cwd alone.
        let minimum = bar_text(&screen, 7);
        assert!(minimum.contains("~/repo"), "{minimum:?}");
        assert!(!minimum.contains("CTX"), "{minimum:?}");
    }

    #[test]
    fn session_bar_unmerged_overrides_task_badge() {
        // Unmerged conflicts override everything until resolved (design language
        // §9.1): even with an unsettled task, the bar shows `▲N`, never the
        // task badge or the ◇ count.
        let screen = git_screen(
            "~/repo",
            crate::git::status::GitStatus {
                unmerged: 2,
                user_dirty: 4,
                iris_unsettled: 1,
                total_uncommitted: 5,
                task: Some(crate::git::status::TaskSummary {
                    task_id: "abcd1234ef99".to_string(),
                    age: Duration::from_secs(30),
                }),
                ..git_status("main")
            },
        );
        let bar = bar_text(&screen, 100);
        assert!(bar.contains("git main ▲2"), "{bar:?}");
        assert!(!bar.contains('◇'), "no task badge while unmerged: {bar:?}");
        assert!(
            !bar.contains("task abcd1234"),
            "no id badge while unmerged: {bar:?}"
        );
    }

    #[test]
    fn git_segment_state_cluster_per_state() {
        // Clean: no glyph — silence is the signal.
        let clean = git_screen("~/repo", git_status("main"));
        let bar = bar_text(&clean, 80);
        assert!(bar.contains("┊ git main"), "{bar:?}");
        assert!(
            !bar.contains('±') && !bar.contains('◇') && !bar.contains('▲'),
            "{bar:?}"
        );

        // Dirty, no task: one number.
        let dirty = git_screen(
            "~/repo",
            crate::git::status::GitStatus {
                total_uncommitted: 5,
                user_dirty: 5,
                ..git_status("main")
            },
        );
        assert!(
            bar_text(&dirty, 80).contains("git main ±5"),
            "{:?}",
            bar_text(&dirty, 80)
        );

        // Unmerged overrides ±/◇ until resolved.
        let conflicted = git_screen(
            "~/repo",
            crate::git::status::GitStatus {
                unmerged: 2,
                total_uncommitted: 4,
                user_dirty: 4,
                ..git_status("main")
            },
        );
        let bar = bar_text(&conflicted, 80);
        assert!(bar.contains("git main ▲2"), "{bar:?}");
        assert!(!bar.contains('±'), "{bar:?}");

        // Detached: `■ detached @ <short-sha>`, dirty count still appends.
        let detached = git_screen(
            "~/repo",
            crate::git::status::GitStatus {
                branch: None,
                detached_at: Some("46b104 fix: pulse".to_string()),
                total_uncommitted: 1,
                user_dirty: 1,
                ..Default::default()
            },
        );
        let bar = bar_text(&detached, 80);
        assert!(bar.contains("git ■ detached @ 46b104 ±1"), "{bar:?}");

        // Non-repo: no git segment at all.
        let plain = footer_screen("~/repo");
        assert!(!bar_text(&plain, 80).contains("git"));
    }

    #[test]
    fn session_bar_renders_jj_status_and_degrades_without_overflow() {
        let screen = jj_screen("~/repo", jj_status("abcdefgh"));
        let full = bar_text(&screen, 100);
        assert!(
            full.contains("┊ jj abcdefgh \"draft status work\" ±3"),
            "{full:?}"
        );
        assert!(full.contains("CTX 0/300k ○○○○○○○○○○"), "{full:?}");

        let mut saw_description = false;
        let mut saw_count = false;
        let mut saw_base = false;
        for width in 1..=100u16 {
            let Some(line) = session_bar(&screen, width) else {
                continue;
            };
            let text = line_text(&line);
            assert!(
                display_width(&text) <= usize::from(width),
                "width {width}: {text:?}"
            );
            if text.contains("\"draft status work\"") {
                saw_description = true;
            }
            if text.contains("±3") && !text.contains("\"draft status work\"") {
                saw_count = true;
            }
            if text.contains("jj abcdefgh") && !text.contains('±') {
                saw_base = true;
            }
        }
        assert!(saw_description);
        assert!(saw_count);
        assert!(saw_base);
    }

    #[test]
    fn session_bar_renders_jj_conflict_before_dirty_count() {
        let screen = jj_screen(
            "~/repo",
            crate::git::status::JjStatus {
                conflicted: 2,
                total_changed: 4,
                ..jj_status("abcdefgh")
            },
        );
        let bar = bar_text(&screen, 100);
        assert!(
            bar.contains("jj abcdefgh \"draft status work\" ▲2"),
            "{bar:?}"
        );
        assert!(!bar.contains("±4"), "{bar:?}");
    }

    #[test]
    fn session_bar_marks_open_dropdown_with_disclosure_prefix() {
        use crate::ui::tui::session_menu::{GitMenu, SessionMenu, TreeMenu};
        let mut screen = git_screen("~/repo", git_status("main"));
        // Git dropdown open: `▾ ` prefixes the git segment only.
        screen.open_session_menu(SessionMenu::Git(Box::new(GitMenu::new(
            git_status("main"),
            std::path::PathBuf::from("/wt"),
        ))));
        let bar = bar_text(&screen, 80);
        assert!(bar.contains("┊ ▾ git main"), "{bar:?}");
        assert!(!bar.starts_with("▾"), "{bar:?}");
        // Tree dropdown open (exclusive slot: replaces the git dropdown): the
        // cwd gets the prefix instead.
        screen.open_session_menu(SessionMenu::Tree(TreeMenu::new(
            std::env::temp_dir(),
            false,
        )));
        let bar = bar_text(&screen, 80);
        assert!(bar.starts_with("▾ ~/repo"), "{bar:?}");
        assert!(!bar.contains("▾ git"), "{bar:?}");
    }

    #[test]
    fn session_bar_marks_open_jj_dropdown_with_disclosure_prefix() {
        use crate::ui::tui::session_menu::{JjMenu, SessionMenu};
        let status = jj_status("abcdefgh");
        let mut screen = jj_screen("~/repo", status.clone());
        screen.open_session_menu(SessionMenu::Jj(JjMenu::new(status)));
        let bar = bar_text(&screen, 80);
        assert!(bar.contains("┊ ▾ jj abcdefgh"), "{bar:?}");
        assert!(!bar.starts_with("▾"), "{bar:?}");
    }

    #[test]
    fn dropdown_renders_between_bar_and_hairline_and_resets_stable_prefix() {
        use super::{render_document_with_hints, session_bar_lines};
        use crate::ui::tui::session_menu::{GitMenu, SessionMenu};
        use ratatui::layout::Size;

        let mut screen = git_screen("~/repo", git_status("main"));
        screen.commit_user("hello");
        let size = Size::new(80, 24);
        let _ = render_document_with_hints(&mut screen, size);
        let closed_rows = session_bar_lines(&screen, 80, 24).len();
        assert_eq!(closed_rows, 2, "bar + hairline when closed");

        screen.open_session_menu(SessionMenu::Git(Box::new(GitMenu::new(
            git_status("main"),
            std::path::PathBuf::from("/wt"),
        ))));
        let lines = session_bar_lines(&screen, 80, 24);
        assert!(lines.len() > 2, "dropdown rows inserted");
        // The soft hairline stays the closing rule (last row).
        let last = line_text(lines.last().unwrap());
        assert!(last.trim_start().starts_with('─'), "{last:?}");
        // Height cap: ⅓ of the pane or 16 rows.
        assert!(lines.len() <= 2 + 8, "{}", lines.len());
        // Opening the dropdown changes the bar block → stable prefix resets.
        let rendered = render_document_with_hints(&mut screen, size);
        assert_eq!(rendered.stable_prefix, 0);
    }

    #[test]
    fn jj_dropdown_renders_between_bar_and_hairline() {
        use super::{render_document_with_hints, session_bar_lines};
        use crate::ui::tui::session_menu::{JjMenu, SessionMenu};
        use ratatui::layout::Size;

        let status = jj_status("abcdefgh");
        let mut screen = jj_screen("~/repo", status.clone());
        screen.commit_user("hello");
        screen.open_session_menu(SessionMenu::Jj(JjMenu::new(status)));
        let lines = session_bar_lines(&screen, 80, 24);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("RECENT"), "{text}");
        assert!(text.contains("read-only"), "{text}");
        assert!(
            line_text(lines.last().unwrap())
                .trim_start()
                .starts_with('─')
        );
        assert_eq!(
            render_document_with_hints(&mut screen, Size::new(80, 24)).stable_prefix,
            0
        );
    }

    #[test]
    fn transient_missing_vcs_snapshot_keeps_open_jj_dropdown() {
        use crate::ui::tui::session_menu::{JjMenu, SessionMenu};
        let status = jj_status("abcdefgh");
        let mut screen = jj_screen("~/repo", status.clone());
        screen.open_session_menu(SessionMenu::Jj(JjMenu::new(status)));

        screen.set_footer_vcs(None);

        assert!(matches!(screen.session_menu, Some(SessionMenu::Jj(_))));
        let bar = bar_text(&screen, 80);
        assert!(bar.contains("┊ ▾ jj abcdefgh"), "{bar:?}");
    }

    #[test]
    fn modal_and_approval_close_the_dropdown_and_focus_ranks_it() {
        use crate::ui::tui::session_menu::{GitMenu, SessionMenu};
        let mut screen = git_screen("~/repo", git_status("main"));
        screen.open_session_menu(SessionMenu::Git(Box::new(GitMenu::new(
            git_status("main"),
            std::path::PathBuf::from("/wt"),
        ))));
        assert_eq!(screen.focus(), crate::ui::tui::FocusTarget::SessionMenu);
        // SessionMenu outranks the palette…
        screen.set_editor("/mo");
        assert_eq!(screen.focus(), crate::ui::tui::FocusTarget::SessionMenu);
        screen.set_editor("");
        // …and a modal outranks (and closes) the dropdown.
        screen.open_modal(crate::ui::modal::Modal::LoginDialog(
            crate::ui::modal::LoginDialog::new("p", false),
        ));
        assert!(screen.session_menu.is_none());
        assert_eq!(screen.focus(), crate::ui::tui::FocusTarget::Modal);
    }

    #[test]
    fn session_bar_hit_maps_cwd_and_git_segments() {
        use super::{BarSegment, session_bar_hit};
        let screen = git_screen("~/repo", git_status("main"));
        // Column inside the cwd (past the 2-cell inset).
        assert_eq!(session_bar_hit(&screen, 80, 4), Some(BarSegment::Cwd));
        // Column inside `git main`.
        let bar = bar_text(&screen, 76);
        let git_col = bar.find("git").map(|at| display_width(&bar[..at])).unwrap();
        assert_eq!(
            session_bar_hit(&screen, 80, git_col as u16 + 3),
            Some(BarSegment::Git)
        );
        // The right-side context readout is neither target.
        assert_eq!(session_bar_hit(&screen, 80, 74), None);
    }

    #[test]
    fn session_bar_hit_maps_jj_segment() {
        use super::{BarSegment, session_bar_hit};
        let screen = jj_screen("~/repo", jj_status("abcdefgh"));
        assert_eq!(session_bar_hit(&screen, 80, 4), Some(BarSegment::Cwd));
        let bar = bar_text(&screen, 76);
        let jj_col = bar.find("jj").map(|at| display_width(&bar[..at])).unwrap();
        assert_eq!(
            session_bar_hit(&screen, 80, jj_col as u16 + 3),
            Some(BarSegment::Git)
        );
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
            (
                ApprovalPolicy::SkipPermissions,
                "■ dangerously skip permissions",
            ),
            (ApprovalPolicy::Auto, "◉ auto"),
            (ApprovalPolicy::OnRequest, "▲ on-request"),
            (ApprovalPolicy::NeverAsk, "□ never-ask"),
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
    fn auto_policy_label_is_distinct_from_skip_permissions() {
        // ADR-0032/0049: the `auto` preset must never be shown as the
        // dangerous skip-permissions bypass.
        // Different label AND different glyph so neither color nor text confuses
        // a floor-guarded auto policy with blanket approval.
        assert_ne!(
            ApprovalPolicy::Auto.label(),
            ApprovalPolicy::SkipPermissions.label()
        );
        assert_eq!(ApprovalPolicy::Auto.label(), "auto");
        assert_ne!(
            ApprovalPolicy::Auto.symbol(),
            ApprovalPolicy::SkipPermissions.symbol()
        );
        // The nexus preset maps onto the distinct statusline posture.
        assert_eq!(
            ApprovalPolicy::from(crate::nexus::ApprovalMode::Auto),
            ApprovalPolicy::Auto
        );
        assert_eq!(
            ApprovalPolicy::from(crate::nexus::ApprovalMode::NeverAsk),
            ApprovalPolicy::NeverAsk
        );
        assert_eq!(
            ApprovalPolicy::from(crate::nexus::ApprovalMode::Strict),
            ApprovalPolicy::OnRequest
        );
    }

    #[test]
    fn bottom_statusline_shows_mouse_off_hint_only_in_pager_mode() {
        let mut screen = footer_screen("~/repo");
        screen.set_approval_policy(ApprovalPolicy::OnRequest);
        // Inline mode, capture on/off: never a hint.
        screen.mouse_capture = false;
        let status = composer_statusline(&screen, 80)
            .map(|l| line_text(&l))
            .expect("statusline");
        assert!(!status.contains("mouse off"), "{status:?}");

        // Pager mode with capture off: dim `○ mouse off` segment appears.
        screen.pager_active = true;
        let status = composer_statusline(&screen, 80)
            .map(|l| line_text(&l))
            .expect("statusline");
        assert!(status.contains("\u{25cb} mouse off"), "{status:?}");

        // Capture back on: hint disappears.
        assert!(screen.toggle_mouse());
        let status = composer_statusline(&screen, 80)
            .map(|l| line_text(&l))
            .expect("statusline");
        assert!(!status.contains("mouse off"), "{status:?}");
    }

    #[test]
    fn switch_status_predicts_then_realizes_tokens_cache_and_reductions() {
        let mut screen = footer_screen("~/repo");
        screen.set_switch_status(SwitchStatus::new(
            "gpt-5.5".to_string(),
            Some("high".to_string()),
            42_000,
            SwitchCacheStatus::Cold,
            true,
        ));

        let predicted = switch_status_line(&screen, 100)
            .map(|line| line_text(&line))
            .expect("switch status");
        assert!(predicted.contains("GPT-5.5 HIGH"), "{predicted:?}");
        assert!(predicted.contains("~42k ctx"), "{predicted:?}");
        assert!(
            predicted.contains("cache cold next request"),
            "{predicted:?}"
        );
        assert!(predicted.contains("▲ compact recommended"), "{predicted:?}");

        screen.apply(UiEvent::FoldApplied {
            folds: 2,
            semantic_dedupe_folds: 2,
            tool_clearing_folds: 0,
            reclaimed_tokens_estimate: 8_000,
            trigger: crate::nexus::FoldTrigger::SelectionSwitch,
        });
        screen.apply(UiEvent::CompactionApplied {
            compaction_id: "c1".into(),
            covered_from: "1".into(),
            covered_to: "5".into(),
            covered_messages: 5,
            original_tokens_estimate: 40_000,
            summary_tokens_estimate: 4_000,
            budget: 80_000,
        });
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: None,
            usage: Some(crate::nexus::ProviderUsage {
                provider: "openai-codex".to_string(),
                model: "gpt-5.5".to_string(),
                input_tokens: 48_000,
                output_tokens: 846,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 48_846,
                cache_creation: None,
            }),
        });

        let realized = switch_status_line(&screen, 100)
            .map(|line| line_text(&line))
            .expect("switch status");
        assert!(realized.contains("↑48k ↓846"), "{realized:?}");
        assert!(realized.contains("cache read 0%"), "{realized:?}");
        assert!(realized.contains("folded ~8k"), "{realized:?}");
        assert!(realized.contains("compacted ~40k→~4k"), "{realized:?}");

        screen.start_turn();
        assert!(
            switch_status_line(&screen, 100).is_none(),
            "realized switch status clears on the next user turn"
        );
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
        // A bar change (branch switch) resets the hint so no stale bar row is
        // reused.
        screen.set_footer_git(Some(git_status("feat/x")));
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
    fn inactive_terminal_panes_keep_tick_redraws_live() {
        let mut screen = Screen::new();
        screen.start_turn();
        assert!(screen.tick(), "a focused running turn animates");

        // tmux and terminal tabs can keep inactive panes visible. Focus changes
        // should be tracked for coalescing, but they must not pause rendering.
        assert!(screen.set_terminal_focused(false));
        assert!(
            screen.tick(),
            "animation continues while the pane is inactive"
        );
        assert!(
            !screen.set_terminal_focused(false),
            "repeated focus reports are not a state change"
        );

        assert!(screen.set_terminal_focused(true));
        assert!(screen.tick(), "animation remains live when refocused");
    }

    #[test]
    fn working_indicator_names_provider_waits() {
        let mut screen = Screen::new();
        screen.start_turn();
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "turn_0".to_string(),
        });

        let text = line_text(&screen.working_lines(80).remove(0));
        assert!(text.contains("model"), "provider wait is visible: {text:?}");

        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "turn_0".to_string(),
            response_id: None,
            usage: None,
        });
        let text = line_text(&screen.working_lines(80).remove(0));
        assert!(
            !text.contains("model"),
            "provider wait label clears after completion: {text:?}"
        );
    }

    // --- Slice 2: always-visible work-phase state machine ---

    fn bash_call(command: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
            name: "bash".to_string(),
            arguments: serde_json::json!({ "command": command }),
        }
    }

    #[test]
    fn work_header_is_non_empty_within_one_frame_of_turn_start() {
        // DoD: the status header must be meaningful the instant a task starts,
        // before any provider event arrives -- never a blank/dead moment.
        let mut screen = Screen::new();
        screen.start_turn();
        let lines = screen.working_lines(80);
        assert!(!lines.is_empty(), "a running turn always shows a header");
        let text = line_text(&lines[0]);
        assert!(
            text.contains("Starting"),
            "header names the starting phase immediately: {text:?}"
        );
    }

    #[test]
    fn work_phase_walks_waiting_thinking_answering_running_approval_done() {
        // DoD: the phase machine covers the whole task lifecycle with
        // provider-neutral labels, including a named+targeted running tool and a
        // distinct approval phase.
        let mut screen = Screen::new();
        screen.start_turn();
        assert_eq!(screen.work_phase_label(), "Starting");

        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        assert_eq!(screen.work_phase_label(), "Waiting for model");

        screen.apply(UiEvent::AssistantReasoningDelta("Planning".to_string()));
        assert_eq!(screen.work_phase_label(), "Thinking");

        screen.apply(UiEvent::AssistantTextDelta("Here".to_string()));
        assert_eq!(screen.work_phase_label(), "Responding");

        screen.apply(UiEvent::ToolStarted(bash_call("ls -la")));
        let running = screen.work_phase_label().to_string();
        assert!(running.contains("bash"), "names the tool: {running:?}");
        assert!(running.contains("ls -la"), "names the target: {running:?}");

        // Approval is its own phase and, while shown, suppresses the working
        // animation so it never competes with the decision (the approval panel
        // is the primary surface).
        screen.show_approval();
        assert_eq!(screen.work_phase_label(), "Awaiting approval");
        assert!(
            screen.working_lines(80).is_empty(),
            "no working header competes with the approval prompt"
        );

        // Decision in (approved): the header resumes preparing the tool, then
        // the turn winds down.
        screen.clear_approval(true);
        assert_eq!(screen.work_phase_label(), "Preparing tool");
        screen.apply(UiEvent::ProviderTurnCompleted {
            turn_id: "t1".to_string(),
            response_id: None,
            usage: None,
        });
        assert_eq!(screen.work_phase_label(), "Finishing");
    }

    #[test]
    fn cancel_and_denied_approval_wind_down_without_a_stale_label() {
        // A cancelled turn must not leave the header stuck on the phase it was
        // in (the old provider_waiting bool cleared on cancel); it winds down.
        let mut screen = Screen::new();
        screen.start_turn();
        screen.apply(UiEvent::ProviderTurnStarted {
            turn_id: "t1".to_string(),
        });
        screen.apply(UiEvent::AssistantReasoningDelta("Planning".to_string()));
        assert_eq!(screen.work_phase_label(), "Thinking");
        screen.apply(UiEvent::ProviderTurnCancelled {
            turn_id: "t1".to_string(),
        });
        assert_eq!(
            screen.work_phase_label(),
            "Finishing",
            "a cancelled turn winds down, not a stale Thinking"
        );

        // A DENIED approval winds the turn down too -- it must not resume the
        // misleading "Preparing tool" label, since no tool is about to run.
        let mut screen = Screen::new();
        screen.start_turn();
        screen.show_approval();
        assert_eq!(screen.work_phase_label(), "Awaiting approval");
        screen.clear_approval(false);
        assert_eq!(
            screen.work_phase_label(),
            "Finishing",
            "a denied/cancelled approval does not claim a tool is being prepared"
        );
    }

    #[test]
    fn work_phase_labels_are_provider_neutral() {
        // DoD: no provider/model-specific strings in the status labels. Labels
        // live in `activity.rs` and describe the activity, never the provider or
        // model, so a new provider needs no label change.
        use crate::ui::tui::activity::WorkPhase;
        let labels = [
            WorkPhase::Starting.label().to_string(),
            WorkPhase::WaitingProvider.label().to_string(),
            WorkPhase::Thinking.label().to_string(),
            WorkPhase::Answering.label().to_string(),
            WorkPhase::PreparingTool.label().to_string(),
            WorkPhase::AwaitingApproval.label().to_string(),
            WorkPhase::running_tool(&bash_call("ls"))
                .label()
                .to_string(),
            WorkPhase::Finishing.label().to_string(),
        ];
        // Provider/model identity tokens that must never appear in a label.
        let banned = [
            "openai",
            "gpt",
            "codex",
            "claude",
            "anthropic",
            "gemini",
            "o1",
            "o3",
        ];
        for label in labels {
            let lower = label.to_lowercase();
            for token in banned {
                assert!(
                    !lower.contains(token),
                    "label {label:?} must not name provider/model {token:?}"
                );
            }
        }
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
}
