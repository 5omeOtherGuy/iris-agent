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
use crate::ui::slash::{self, Palette, SlashCommand};

use super::text::{ansi_spans, strip_ansi_for_text};
use super::transcript::Transcript;
use super::wrap::{
    display_width, line_text, pad_line_left, push_wrapped_line, truncate_line, wrap_to_width,
};
use super::{
    BOX_X_PADDING_U16, COMPOSER_HINT, EDITOR_VERTICAL_CHROME_ROWS, GLOBAL_STATUS_H,
    MAX_EDITOR_ROWS, MAX_MENU_ROWS, MIN_EDITOR_H, MIN_INLINE_DOCUMENT_ROWS, TEXT_COLUMN_X_PADDING,
    TEXT_X_PADDING_U16, WORKING_FRAMES, border_style, dim_style, format_elapsed_compact,
    panel_style, prompt_style, tool_header_style,
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
    /// Latest provider-reported usage, if the provider surfaced it.
    usage: Option<ProviderUsage>,
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

    /// Whether a picker/dialog is currently open.
    pub(crate) fn modal_open(&self) -> bool {
        self.modal.is_some()
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
        let usage = self.footer.as_ref().and_then(|footer| footer.usage.clone());
        self.footer = Some(Footer {
            model,
            effort,
            context,
            cwd,
            usage,
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

/// Render the slash popup into an offscreen Ratatui buffer: plain above-composer
/// rows with the selected row accented by foreground color only.
fn render_palette(buf: &mut Buffer, area: Rect, matches: &[&SlashCommand], selected: usize) {
    let inner = Rect {
        x: area.x + u16::try_from(TEXT_COLUMN_X_PADDING).unwrap_or(u16::MAX),
        y: area.y + u16::from(area.height > 1),
        width: area
            .width
            .saturating_sub(
                u16::try_from(TEXT_COLUMN_X_PADDING.saturating_mul(2)).unwrap_or(u16::MAX),
            )
            .max(1),
        height: area.height.saturating_sub(2).max(1),
    };
    let mut rows = Vec::new();
    let command_width = matches
        .iter()
        .map(|cmd| display_width(cmd.name))
        .max()
        .unwrap_or(0);
    for (i, cmd) in matches.iter().enumerate() {
        let selected_row = i == selected;
        let name_style = if selected_row {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };
        let description_style = if selected_row {
            Style::default().fg(Color::Cyan)
        } else {
            dim_style()
        };
        let gap = command_width
            .saturating_sub(display_width(cmd.name))
            .saturating_add(2);
        rows.push(Line::from(vec![
            Span::styled(cmd.name.to_string(), name_style),
            Span::raw(" ".repeat(gap)),
            Span::styled(cmd.description, description_style),
        ]));
    }
    Paragraph::new(Text::from(rows)).render(inner, buf);
}

fn render_plain_menu_lines(buf: &mut Buffer, area: Rect, lines: Vec<Line<'static>>) {
    let inner = Rect {
        x: area.x + u16::try_from(TEXT_COLUMN_X_PADDING).unwrap_or(u16::MAX),
        y: area.y + u16::from(area.height > 1),
        width: area
            .width
            .saturating_sub(
                u16::try_from(TEXT_COLUMN_X_PADDING.saturating_mul(2)).unwrap_or(u16::MAX),
            )
            .max(1),
        height: area.height.saturating_sub(2).max(1),
    };
    Paragraph::new(Text::from(lines)).render(inner, buf);
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
    transcript.extend(working_block);
    transcript.extend(chrome);
    (transcript, volatile_tail)
}

pub(super) fn global_status_line(screen: &Screen, width: u16) -> Option<Line<'static>> {
    let footer = screen.footer.as_ref()?;
    let clean_cwd = strip_ansi_for_text(&footer.cwd);
    let (cwd, branch) = split_cwd_branch(&clean_cwd);
    let model = strip_ansi_for_text(&footer.model);
    let model_flag = (!screen.spinner.active)
        .then_some(footer.effort.as_ref())
        .flatten()
        .map(|effort| strip_ansi_for_text(effort));
    let fields = status_fields_for_width(
        usize::from(width),
        &model,
        model_flag.as_deref(),
        footer.context.as_deref(),
        &cwd,
        branch.as_deref(),
    );
    let mut spans = vec![
        Span::raw(" ".repeat(statusline_indent(usize::from(width)))),
        Span::styled("● ", prompt_style()),
    ];
    for (idx, field) in fields.iter().enumerate() {
        if idx > 0 {
            spans.push(rail_sep());
        }
        spans.push(Span::styled(format!("{} ", field.label), panel_style()));
        spans.push(Span::styled(field.value.clone(), tool_header_style()));
    }
    let mut line = Line::from(spans);
    truncate_line(&mut line, usize::from(width).max(1));
    Some(line)
}

#[derive(Clone)]
struct StatusField {
    label: &'static str,
    value: String,
}

fn statusline_indent(width: usize) -> usize {
    TEXT_COLUMN_X_PADDING.min(width.saturating_sub(1))
}

fn status_fields_for_width(
    width: usize,
    model: &str,
    model_flag: Option<&str>,
    context: Option<&str>,
    cwd: &str,
    branch: Option<&str>,
) -> Vec<StatusField> {
    let candidates = [
        (true, true, true, true, true),
        (true, true, true, true, false),
        (true, false, true, true, false),
        (true, false, true, false, false),
        (true, false, false, false, false),
        (false, false, false, false, false),
    ];
    candidates
        .into_iter()
        .map(
            |(include_model, include_model_flag, include_context, include_cwd, include_branch)| {
                build_status_fields(
                    if include_model { model } else { "" },
                    (include_model && include_model_flag)
                        .then_some(model_flag)
                        .flatten(),
                    include_context.then_some(context).flatten(),
                    include_cwd.then_some(cwd),
                    include_branch.then_some(branch).flatten(),
                )
            },
        )
        .find(|fields| statusline_width(fields, width) <= width || fields.len() <= 1)
        .unwrap_or_else(|| build_status_fields("", None, None, None, None))
}

fn build_status_fields(
    model: &str,
    model_flag: Option<&str>,
    context: Option<&str>,
    cwd: Option<&str>,
    branch: Option<&str>,
) -> Vec<StatusField> {
    let mut fields = vec![StatusField {
        label: "MODE",
        value: "code".to_string(),
    }];
    if !model.is_empty() {
        let value = if let Some(flag) = model_flag.filter(|flag| !flag.is_empty()) {
            format!("{model} {flag}")
        } else {
            model.to_string()
        };
        fields.push(StatusField {
            label: "MODEL",
            value,
        });
    }
    if let Some(context) = context.filter(|context| !context.is_empty()) {
        fields.push(StatusField {
            label: "CTX",
            value: context.to_string(),
        });
    }
    if let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) {
        fields.push(StatusField {
            label: "CWD",
            value: cwd.to_string(),
        });
    }
    if let Some(branch) = branch.filter(|branch| !branch.is_empty()) {
        fields.push(StatusField {
            label: "BRANCH",
            value: branch.to_string(),
        });
    }
    fields
}

fn statusline_width(fields: &[StatusField], terminal_width: usize) -> usize {
    let content = fields
        .iter()
        .map(|field| format!("{} {}", field.label, field.value))
        .collect::<Vec<_>>()
        .join("  ┊  ");
    statusline_indent(terminal_width) + display_width("● ") + display_width(&content)
}

fn rail_sep() -> Span<'static> {
    Span::styled("  ┊  ", dim_style())
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
    global_status: u16,
    editor: u16,
}

fn chrome_heights(
    height: u16,
    menu_wanted: u16,
    desired_global_status_h: u16,
    editor_rows: u16,
) -> ChromeHeights {
    let global_status = desired_global_status_h.min(GLOBAL_STATUS_H);
    let menu = menu_wanted.min(
        height
            .saturating_sub(MIN_EDITOR_H)
            .saturating_sub(global_status),
    );
    let max_editor_h = height
        .saturating_sub(menu)
        .saturating_sub(global_status)
        .max(1);
    let wanted_editor_h = editor_rows.saturating_add(EDITOR_VERTICAL_CHROME_ROWS);
    let editor = if max_editor_h >= MIN_EDITOR_H {
        wanted_editor_h.clamp(MIN_EDITOR_H, max_editor_h)
    } else {
        max_editor_h.max(1)
    };
    ChromeHeights {
        menu,
        global_status,
        editor,
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
    let modal_lines = screen.modal.as_ref().map(|modal| {
        modal.render(u16::try_from(content_width(usize::from(area.width))).unwrap_or(u16::MAX))
    });
    let palette_active = modal_lines.is_none() && screen.palette.is_active(&input_text);
    let palette_matches: Vec<&SlashCommand> = if palette_active {
        slash::matches(&input_text)
    } else {
        Vec::new()
    };
    let menu_wanted = if let Some(lines) = &modal_lines {
        u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2)
            .min(MAX_MENU_ROWS)
    } else if palette_active {
        (palette_matches.len() as u16 + 2).min(MAX_MENU_ROWS)
    } else {
        0
    };

    // Bottom-anchored, clamped to the fixed viewport. The editor owns its hint
    // row inside the border; there is no bottom telemetry bar.
    let global_status_lines: Vec<Line<'static>> =
        global_status_line(screen, area.width).into_iter().collect();
    let global_status_h = u16::try_from(global_status_lines.len()).unwrap_or(GLOBAL_STATUS_H);
    let heights = chrome_heights(area.height, menu_wanted, global_status_h, editor_rows);
    let chrome_h = heights
        .menu
        .saturating_add(heights.global_status)
        .saturating_add(heights.editor);
    let chrome_area = Rect::new(0, 0, width, chrome_h.max(1));
    let chunks = Layout::vertical([
        Constraint::Length(heights.menu),
        Constraint::Length(heights.global_status),
        Constraint::Length(heights.editor),
    ])
    .split(chrome_area);
    let menu_area = chunks[0];
    let rail_area = chunks[1];
    let editor_area = chunks[2];

    let mut buf = Buffer::empty(chrome_area);

    if heights.menu > 0 {
        if let Some(lines) = modal_lines {
            render_plain_menu_lines(&mut buf, menu_area, lines);
        } else {
            render_palette(
                &mut buf,
                menu_area,
                &palette_matches,
                screen.palette.selected(),
            );
        }
    }
    if heights.global_status > 0 {
        Paragraph::new(Text::from(global_status_lines)).render(rail_area, &mut buf);
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

    use super::{ApprovalHint, approval_status_lines, display_width, line_text, working_lines};
    use crate::ui::tui::WORKING_FRAMES;

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
