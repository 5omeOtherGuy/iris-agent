//! Replayable screen state, composer chrome, status rail, and working indicator rendering.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui_textarea::{TextArea, WrapMode};

#[cfg(test)]
use crate::mimir::model_catalog;
use crate::nexus::{ApprovalDecision, ProviderUsage, ToolCall};
use crate::tool_display::run_target;
use crate::ui::UiEvent;
use crate::ui::modal::Modal;
use crate::ui::slash::Palette;

use super::component::{Component, Container, take_cursor_position};
use super::overlay::{FocusTarget, PaletteView, render_menu_lines};
use super::text::{ansi_spans, strip_ansi_for_text};
use super::transcript::Transcript;
use super::wrap::{
    display_width, line_text, pad_line_left, push_wrapped_line, spans_width, truncate_line,
    truncate_to_width, wrap_to_width,
};
use super::{
    BOX_X_PADDING_U16, COMPOSER_HINT, EDITOR_VERTICAL_CHROME_ROWS, MAX_EDITOR_ROWS, MAX_MENU_ROWS,
    MIN_EDITOR_H, MIN_INLINE_DOCUMENT_ROWS, TEXT_COLUMN_X_PADDING, TEXT_X_PADDING_U16,
    WORKING_FRAMES, WORKSPACE_LABEL_H, border_style, dim_style, format_elapsed_compact,
    panel_style, prompt_style,
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
}

struct ApprovalHint {
    target: String,
    options: &'static str,
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

    /// Advance one frame; a no-op when idle so ticks cause no redraw at rest.
    fn tick(&mut self) -> bool {
        if self.active {
            self.frame = (self.frame + 1) % WORKING_FRAMES.len();
        }
        self.active
    }

    fn frame(&self) -> &'static str {
        WORKING_FRAMES[self.frame % WORKING_FRAMES.len()]
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
    width: usize,
) -> Line<'static> {
    let mut spans = led_frame_spans(frame);
    spans.push(Span::raw(" "));
    spans.push(Span::styled(format_elapsed_compact(elapsed), panel_style()));
    if can_interrupt {
        spans.push(working_sep());
        spans.push(Span::styled("ESC", panel_style()));
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
    width: usize,
) -> Vec<Line<'static>> {
    vec![working_indicator_line(
        frame,
        elapsed.unwrap_or_default(),
        true,
        footer.and_then(|footer| footer.usage.as_ref()),
        width,
    )]
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
    push_wrapped_line(&Line::from(target), width, Some("  "), &mut lines);
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
    let inner_width = usize::from(
        width
            .saturating_sub(BOX_X_PADDING_U16.saturating_mul(2))
            .saturating_sub(TEXT_X_PADDING_U16.saturating_mul(2))
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
}

impl Screen {
    pub(crate) fn new() -> Self {
        Self {
            transcript: Transcript::default(),
            editor: fresh_editor(),
            palette: Palette::default(),
            spinner: Spinner::default(),
            turn_divider: TurnDivider::default(),
            approval_hint: None,
            footer: None,
            modal: None,
        }
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
        self.transcript.apply(event);
    }

    /// Commit a submitted prompt into the transcript as a user line.
    pub(crate) fn commit_user(&mut self, text: &str) {
        self.transcript.commit_user(text);
    }

    /// Render all transcript rows plus any in-flight stream, wrapped to `width`.
    /// Finalized history is intentionally retained here; the terminal surface
    /// owns append/diff/full-replay decisions instead of draining UI state.
    pub(super) fn wrapped_lines(&mut self, width: u16) -> Vec<Line<'static>> {
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
        if let Some(footer) = &mut self.footer {
            footer.usage = None;
        }
    }

    pub(crate) fn end_turn(&mut self) {
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
#[cfg(test)]
pub(super) fn render_document(screen: &mut Screen, size: Size) -> Vec<Line<'static>> {
    render_document_with_chrome_tail(screen, size).0
}

pub(super) fn render_document_with_chrome_tail(
    screen: &mut Screen,
    size: Size,
) -> (Vec<Line<'static>>, usize) {
    if size.height == 0 || size.width < 1 {
        return (Vec::new(), 0);
    }
    let width = size.width.max(1);
    let height = size.height.max(1);
    let mut transcript = screen.wrapped_lines(width);
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
    if transcript.len() < min_transcript_rows {
        let mut padded = Vec::with_capacity(min_transcript_rows + chrome.len());
        padded.extend(
            std::iter::repeat_with(Line::default).take(min_transcript_rows - transcript.len()),
        );
        padded.extend(transcript);
        transcript = padded;
    }
    // The transcript is the scrolling base, moved into the document and never
    // cloned. The bottom-pinned tail -- working indicator then composer chrome
    // (which carries the docked overlays) -- is composited through the root
    // Container, mirroring pi-mono's `TUI extends Container` (`tui.ts#L265`).
    // Both tail sections are bounded by the viewport height, not the transcript
    // length, so the container's only per-frame copy is small and constant.
    let mut tail = Container::new();
    tail.add_child(Box::new(LinesSection(working_block)));
    tail.add_child(Box::new(LinesSection(chrome)));
    let mut document = transcript;
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
    (document, volatile_tail)
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
                Span::styled("○".to_string(), dim_style())
            } else if dot == filled {
                Span::styled("●".to_string(), prompt_style())
            } else {
                Span::styled("●".to_string(), meter_used_style())
            }
        })
        .collect()
}

/// Build the editor's top border, which doubles as the primary statusline:
/// `┌─ ● CODE ─ GPT-5.4 LOW ─ CTX 300K ●●●○○○○○○○ ───┐`. Returns `None` when
/// there is no footer yet or even the minimum content cannot fit, in which case
/// the caller leaves the plain `Block` border in place so the frame never breaks.
pub(super) fn composer_top_border(screen: &Screen, box_width: u16) -> Option<Line<'static>> {
    let footer = screen.footer.as_ref()?;
    let width = usize::from(box_width);
    // `┌` + `─ ` + content + ` ` + fill + `┐` needs at least the corners plus a
    // one-character field.
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
            Span::styled("● ".to_string(), prompt_style()),
            Span::raw("CODE"),
        ]
    };
    let model_with_effort = || match &effort {
        Some(effort) => vec![Span::raw(format!("{model} {effort}"))],
        None => vec![Span::raw(model.clone())],
    };
    let model_only = || vec![Span::raw(model.clone())];
    let ctx_meter = |with_meter: bool| {
        context.as_ref().map(|context| {
            let mut spans = vec![Span::raw(format!("CTX {context}"))];
            if let (true, Some(filled)) = (with_meter, meter_filled) {
                spans.push(Span::raw(" "));
                spans.extend(context_meter_spans(filled));
            }
            spans
        })
    };

    // Candidates from fullest to minimum. The drop order is monotonic and
    // matches the spec: drop effort, then the meter, then the CTX label, leaving
    // the minimum `● CODE ─ MODEL`. Effort never reappears once dropped.
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

    candidates
        .into_iter()
        .find_map(|segments| top_border_line(width, segments))
}

/// Assemble one top-border candidate at `width`, or `None` if its segments do
/// not fit (`┌─ ` + segments joined by ` ─ ` + ` ` + `─` fill + `┐`).
fn top_border_line(width: usize, segments: Vec<Vec<Span<'static>>>) -> Option<Line<'static>> {
    let mut joined: Vec<Span<'static>> = Vec::new();
    for (idx, segment) in segments.into_iter().enumerate() {
        if idx > 0 {
            joined.push(Span::styled(" ─ ".to_string(), border_style()));
        }
        joined.extend(segment);
    }
    let joined_w = spans_width(&joined);
    // Fixed cells: `┌`, `─ ` (2), trailing ` ` (1), `┐` => 5.
    let fill = width.checked_sub(joined_w + 5)?;
    let mut spans = vec![Span::styled("┌─ ".to_string(), border_style())];
    spans.extend(joined);
    spans.push(Span::styled(" ".to_string(), border_style()));
    if fill > 0 {
        spans.push(Span::styled("─".repeat(fill), border_style()));
    }
    spans.push(Span::styled("┐".to_string(), border_style()));
    Some(Line::from(spans))
}

/// Quiet unboxed workspace label rendered below the editor:
/// `~/projects/iris ┊ git main`. Returns `None` when there is no footer/cwd.
pub(super) fn workspace_label_line(screen: &Screen, width: u16) -> Option<Line<'static>> {
    let footer = screen.footer.as_ref()?;
    let width = usize::from(width);
    let (cwd, branch) = split_cwd_branch(&strip_ansi_for_text(&footer.cwd));
    if cwd.is_empty() {
        return None;
    }
    let indent = TEXT_COLUMN_X_PADDING.min(width.saturating_sub(1));
    let suffix = branch
        .as_ref()
        .map(|branch| format!(" ┊ git {branch}"))
        .unwrap_or_default();
    let avail = width
        .saturating_sub(indent)
        .saturating_sub(display_width(&suffix))
        .max(1);
    let cwd = truncate_cwd_middle(&cwd, avail);
    let mut spans = vec![
        Span::raw(" ".repeat(indent)),
        Span::styled(cwd, dim_style()),
    ];
    if !suffix.is_empty() {
        spans.push(Span::styled(suffix, dim_style()));
    }
    let mut line = Line::from(spans);
    truncate_line(&mut line, width.max(1));
    Some(line)
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
    workspace: u16,
}

/// Allocate chrome rows. The editor frame is protected first: the menu yields to
/// `MIN_EDITOR_H`, then the workspace label yields to both, so a constrained
/// height drops the workspace label before squeezing the editor frame.
fn chrome_heights(
    height: u16,
    menu_wanted: u16,
    workspace_wanted: u16,
    editor_rows: u16,
) -> ChromeHeights {
    let workspace_wanted = workspace_wanted.min(WORKSPACE_LABEL_H);
    let menu = menu_wanted.min(height.saturating_sub(MIN_EDITOR_H));
    let workspace = workspace_wanted.min(height.saturating_sub(menu).saturating_sub(MIN_EDITOR_H));
    let max_editor_h = height.saturating_sub(menu).saturating_sub(workspace).max(1);
    let wanted_editor_h = editor_rows.saturating_add(EDITOR_VERTICAL_CHROME_ROWS);
    let editor = if max_editor_h >= MIN_EDITOR_H {
        wanted_editor_h.clamp(MIN_EDITOR_H, max_editor_h)
    } else {
        max_editor_h.max(1)
    };
    ChromeHeights {
        menu,
        editor,
        workspace,
    }
}

fn render_editor_chrome(screen: &mut Screen, width: u16, height: u16) -> Vec<Line<'static>> {
    let area = Rect::new(0, 0, width, height);

    let editor_rows = screen.approval_hint.as_ref().map_or_else(
        || editor_visual_rows(&screen.editor, area.width),
        |hint| {
            let inner_width = area
                .width
                .saturating_sub(BOX_X_PADDING_U16.saturating_mul(2))
                .saturating_sub(TEXT_X_PADDING_U16.saturating_mul(2))
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

    // Bottom-anchored, clamped to the fixed viewport. The runtime statusline is
    // printed into the editor's top border; the workspace label sits below the
    // editor as a quiet unboxed line. There is no separate rail and no bottom
    // telemetry bar.
    let workspace_line = workspace_label_line(screen, area.width);
    let workspace_wanted = u16::from(workspace_line.is_some());
    let heights = chrome_heights(area.height, menu_wanted, workspace_wanted, editor_rows);
    let chrome_h = heights
        .menu
        .saturating_add(heights.editor)
        .saturating_add(heights.workspace);
    let chrome_area = Rect::new(0, 0, width, chrome_h.max(1));
    let chunks = Layout::vertical([
        Constraint::Length(heights.menu),
        Constraint::Length(heights.editor),
        Constraint::Length(heights.workspace),
    ])
    .split(chrome_area);
    let menu_area = chunks[0];
    let editor_area = chunks[1];
    let workspace_area = chunks[2];

    let mut buf = Buffer::empty(chrome_area);

    if heights.menu > 0
        && let Some(lines) = menu_lines
    {
        render_menu_lines(&mut buf, menu_area, lines);
    }
    let box_area = Rect {
        x: editor_area.x + BOX_X_PADDING_U16.min(editor_area.width.saturating_sub(1)),
        y: editor_area.y,
        width: editor_area
            .width
            .saturating_sub(BOX_X_PADDING_U16 * 2)
            .max(1),
        height: editor_area.height,
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(border_style())
        .render(box_area, &mut buf);
    let text_area = Rect {
        x: box_area.x + 2.min(box_area.width.saturating_sub(1)),
        y: editor_area.y + 2.min(editor_area.height.saturating_sub(1)),
        width: box_area.width.saturating_sub(TEXT_X_PADDING_U16 * 2).max(1),
        height: editor_area
            .height
            .saturating_sub(EDITOR_VERTICAL_CHROME_ROWS)
            .max(1),
    };
    if let Some(hint) = &screen.approval_hint {
        let approval_lines = approval_status_lines(hint, usize::from(text_area.width));
        Paragraph::new(Text::from(approval_lines)).render(text_area, &mut buf);
    } else {
        (&screen.editor).render(text_area, &mut buf);
    }
    let hint = if screen.approval_hint.is_some() || editor_area.height < MIN_EDITOR_H {
        Line::default()
    } else {
        Line::from(Span::styled(COMPOSER_HINT, dim_style()))
    };
    let hint_area = Rect {
        x: box_area.x + 2.min(box_area.width.saturating_sub(1)),
        y: box_area.y.saturating_add(box_area.height.saturating_sub(2)),
        width: box_area.width.saturating_sub(4).max(1),
        height: 1,
    };
    if !hint.spans.is_empty() {
        Paragraph::new(Text::from(vec![hint])).render(hint_area, &mut buf);
    }
    // Print the statusline into the editor's top border last so it is never
    // overwritten by the textarea/approval body at very small heights.
    if heights.editor > 0
        && let Some(top_border) = composer_top_border(screen, box_area.width)
    {
        buf.set_line(box_area.x, box_area.y, &top_border, box_area.width);
    }
    if heights.workspace > 0
        && let Some(line) = workspace_line
    {
        Paragraph::new(Text::from(vec![line])).render(workspace_area, &mut buf);
    }
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        ApprovalHint, CONTEXT_METER_DOTS, approval_status_lines, context_meter_filled,
        display_width, line_text, parse_context_window, truncate_cwd_middle, working_lines,
    };
    use crate::ui::tui::WORKING_FRAMES;

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
    fn approval_and_working_lines_stay_bounded_at_tiny_widths() {
        let hint = ApprovalHint {
            target: "run an extremely long command".to_string(),
            options: "[y] once  [N] deny",
        };
        for width in 1..=4 {
            for line in approval_status_lines(&hint, width) {
                assert!(
                    display_width(&line_text(&line)) <= width,
                    "width {width}: {line:?}"
                );
            }
            for line in working_lines(WORKING_FRAMES[0], Some(Duration::from_secs(1)), None, width)
            {
                assert!(
                    display_width(&line_text(&line)) <= width,
                    "width {width}: {line:?}"
                );
            }
        }
    }
}
