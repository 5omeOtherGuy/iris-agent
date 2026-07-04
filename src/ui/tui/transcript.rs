//! Retained transcript state and event-to-row rendering.

use std::ops::{Deref, Range};
use std::time::{Duration, Instant};

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::nexus::{ApprovalDecision, ProviderUsage, ToolCall};
use crate::tool_display::run_target;
use crate::ui::markdown::{MarkdownTheme, render_markdown_themed};
use crate::ui::{TurnErrorKind, UiEvent};

use super::pane;
use super::panel::{
    PanelHeaderSpec, PanelState, diff_counts, diff_footer_row, diff_table_rows, panel_state,
};
use super::rows::{ChromeRow, FoldVis, TranscriptRow, is_separator_row};
use super::text::ansi_spans;
use super::tool_render::{self, RenderCtx, ToolOutcome, ToolPanelKind};
use super::wrap::{display_width, line_text};
use super::{
    MAX_EXEC_STREAM_BYTES, MAX_STREAMING_MARKDOWN_BYTES, MAX_TRANSCRIPT_ROWS,
    TEXT_COLUMN_X_PADDING, dim_style, err_style, format_elapsed_compact, ok_style, panel_style,
    tool_header_style, turn_divider_label,
};

/// Reasoning-rail label. Uppercase like the other structural labels; the rail
/// (`┊`) and the `▾`/`▸` disclosure arrow carry the fold affordance, so no
/// `Thinking...` ellipsis is needed (ThinkingBlock design-system component).
const THINKING_LABEL: &str = "THINKING";
/// Placeholder for reasoning the provider withheld; the original text is never
/// available and is never rendered.
const REDACTED_THINKING_BODY: &str = "[reasoning withheld by provider]";

/// Fixed-width label field for the header-right status cluster. Wide enough for
/// the longest label across every panel family (` CANCELLED` = 10, ` APPROVED`
/// = 9) with slack, so the glyph column never depends on the label text.
const HEADER_LABEL_WIDTH: usize = 13;
/// Fixed-width right-aligned elapsed field. Wide enough for the compact elapsed
/// forms without shifting the header's right edge as the live timer changes
/// length (e.g. `9.9s` -> `10s`); empty for panels with no duration.
const HEADER_ELAPSED_WIDTH: usize = 8;

/// The single header-right status cluster used by every panel family
/// (EXPLORE / SHELL / EDIT / generic / APPROVAL). Geometry is fixed so the
/// status glyph always lands in the same column regardless of panel type,
/// label text, or elapsed length:
///
/// `symbol` + `label` left-aligned in a [`HEADER_LABEL_WIDTH`]-wide field +
/// `elapsed` right-aligned in a [`HEADER_ELAPSED_WIDTH`]-wide field + a uniform
/// 2-space right pad. Panels with no duration (APPROVAL) pass an empty
/// `elapsed`, which still reserves the field so their glyph aligns too.
fn header_right_cluster(
    symbol: &str,
    symbol_style: Style,
    label: &str,
    label_style: Style,
    elapsed: &str,
) -> Vec<(String, Style)> {
    vec![
        (symbol.to_string(), symbol_style),
        (format!("{label:<HEADER_LABEL_WIDTH$}"), label_style),
        (format!("{elapsed:>HEADER_ELAPSED_WIDTH$}  "), dim_style()),
    ]
}

/// One reasoning-trace row on the muted left rail: the dim `┊ ` rail prefix plus
/// the line, word-wrapped with the rail carried onto continuation rows, and
/// hidden until the block is expanded. A plain (chromeless) row — reasoning gets
/// no box.
fn rail_body_row(mut line: Line<'static>) -> TranscriptRow {
    line.spans.insert(
        0,
        Span::styled(format!("{} ", crate::ui::symbols::SEP), dim_style()),
    );
    let text = line_text(&line);
    TranscriptRow {
        text,
        style: dim_style(),
        // "┊ " — keeps the rail on wrapped continuation lines.
        continuation_prefix: Some("\u{250a} "),
        line: Some(line),
        fold: FoldVis::WhenExpanded,
        word_wrap: true,
        background: None,
        hrule: false,
        chrome: None,
    }
}

/// Pad `left` so `right` hugs the right edge of a `width`-column field; drops
/// `right` when the field is too narrow rather than overflowing.
fn right_align_pair(left: &str, right: &str, width: usize) -> String {
    let left_w = display_width(left);
    let right_w = display_width(right);
    if left_w + 2 + right_w > width {
        return left.to_string();
    }
    format!("{left}{}{right}", " ".repeat(width - left_w - right_w))
}

/// The currently-streaming exec block (issue #90 sub-item 1). `bash` is
/// exclusive, so at most one is ever open. `body_start` is the row index of the
/// block body (its `Running`/`Ran` header). `output` is the bounded live tail
/// re-rendered (and flood-capped) under the gutter on each delta.
struct ActiveExec {
    call: ToolCall,
    output: String,
    body_start: usize,
    started: Instant,
}

struct ActiveExploration {
    call_id: String,
    row: usize,
    started: Instant,
    duration: Option<Duration>,
    failed: bool,
    cancelled: bool,
    done: bool,
}

struct ActiveTool {
    call: ToolCall,
    body_start: usize,
    started: Instant,
}

/// The open EDIT panel for a mutation whose diff arrived via `DiffPreview`.
/// The diff is the canonical EDIT body for the whole lifecycle: the same panel
/// is rebuilt in place as `◇ PREVIEW` → `● RUNNING` → `◆ DONE`/`■ ERROR`.
struct ActiveEdit {
    call_id: String,
    diff: String,
    body_start: usize,
    started: Option<Instant>,
}

/// Cached layout for one logical row: the physical-line range it wraps to plus
/// its fold visibility, resolved once at wrap time. Fold state is part of the
/// cache because every fold mutation (`toggle_latest_panel`, panel rebuilds)
/// already dirties the affected rows, so per-frame rendering can skip the
/// whole-transcript visibility scan and touch only dirty rows.
struct RowLayout {
    lines: Range<usize>,
    /// Whether this row is visible under the panel fold state at wrap time.
    visible: bool,
    /// Cumulative visible physical lines through this row (inclusive), so the
    /// stable-prefix line count for any dirty boundary is an O(1) lookup.
    visible_cum: usize,
    /// Panel `expanded` state carried into the next row's visibility scan.
    expanded_after: bool,
}

#[derive(Default)]
struct WrappedTranscriptCache {
    width: usize,
    rows: Vec<RowLayout>,
    lines: Vec<Line<'static>>,
    dirty_from: usize,
}

impl WrappedTranscriptCache {
    fn invalidate_all(&mut self, width: usize) {
        self.width = width;
        self.rows.clear();
        self.lines.clear();
        self.dirty_from = 0;
    }

    fn mark_dirty(&mut self, row: usize) {
        self.dirty_from = self.dirty_from.min(row);
    }
}

/// Memoized render of the in-flight streaming preview. The stream buffer only
/// ever grows for a given stream and the memo is dropped on every stream
/// start/end transition, so `(len, width)` uniquely identifies the rendered
/// output. This keeps spinner ticks and typing-while-streaming frames from
/// re-parsing the whole markdown preview when the stream did not change.
struct StreamingRender {
    len: usize,
    width: usize,
    lines: Vec<Line<'static>>,
}

#[derive(Debug)]
pub(super) struct TranscriptRender {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) stable_prefix: usize,
    pub(super) total_lines: usize,
}

impl Deref for TranscriptRender {
    type Target = [Line<'static>];

    fn deref(&self) -> &Self::Target {
        &self.lines
    }
}

/// Transcript state and width-aware rendering, separate from editor/spinner UI.
#[derive(Default)]
pub(super) struct Transcript {
    pub(super) rows: Vec<TranscriptRow>,
    /// Live assistant text being streamed; rendered after committed rows and
    /// committed exactly once on `AssistantTextEnd`.
    pub(super) streaming: Option<String>,
    /// The open live exec cell, if a streaming tool is running.
    active_exec: Option<ActiveExec>,
    active_explorations: Vec<ActiveExploration>,
    active_tool: Option<ActiveTool>,
    /// The open diff-bodied EDIT panel, if a mutation is pending/running.
    active_edit: Option<ActiveEdit>,
    pub(super) exploring_open: bool,
    /// Wall-clock start of the current provider turn; drives the honest
    /// "time until reasoning arrived" telemetry on the thinking header.
    provider_turn_started: Option<Instant>,
    /// Row index of the thinking rail header pushed during the current provider
    /// turn, so `ProviderTurnCompleted` can patch its `↓tokens` telemetry once
    /// usage is known. Cleared at each provider-turn start.
    thinking_header_row: Option<usize>,
    /// The elapsed label already shown on that header (combined with the token
    /// count when usage arrives).
    thinking_elapsed: Option<String>,
    /// Last width the transcript was rendered/flushed at, so width-aware
    /// shaping in the width-agnostic `apply` path (the tool-output flood cap)
    /// uses a realistic column count. Zero until the first render.
    last_width: usize,
    wrapped_cache: WrappedTranscriptCache,
    /// Memoized wrapped lines for the current streaming preview; see
    /// [`StreamingRender`].
    streaming_render: Option<StreamingRender>,
    /// Row indices where a committed user prompt starts (first content row of
    /// each `commit_user`), maintained incrementally so the pager's sticky
    /// prompt header is an O(prompts-above-view) lookup, never a row scan.
    user_prompt_starts: Vec<usize>,
}

impl Transcript {
    fn mark_dirty_from(&mut self, row: usize) {
        self.wrapped_cache.mark_dirty(row.min(self.rows.len()));
    }

    fn mark_append_dirty(&mut self) {
        self.mark_dirty_from(self.rows.len());
    }

    /// Append a blank separator row before a new top-level block, unless the
    /// transcript is empty or already ends in a real separator row.
    fn push_blank(&mut self) {
        self.exploring_open = false;
        match self.rows.last() {
            None => {}
            Some(last) if is_separator_row(last) => {}
            _ => {
                self.mark_append_dirty();
                self.rows
                    .push(TranscriptRow::new(String::new(), Style::default()));
            }
        }
    }

    /// Finish any live stream and open a fresh block with a leading separator.
    fn begin_block(&mut self) {
        self.finish_stream();
        self.push_blank();
        self.mark_append_dirty();
    }

    /// Push each line of `text` into the transcript with one style.
    fn push(&mut self, text: &str, style: Style) {
        self.mark_append_dirty();
        for line in text.split('\n') {
            self.rows.push(TranscriptRow::new(line, style));
        }
    }

    fn push_assistant_text(&mut self, text: &str) {
        let width = self.markdown_content_width();
        self.mark_append_dirty();
        pane::push_assistant_rows(&mut self.rows, width, text);
    }

    /// Content width used for width-aware markdown (tables) at append time.
    /// Falls back to a default before the first frame has set `last_width`.
    fn markdown_content_width(&self) -> usize {
        if self.last_width == 0 {
            crate::ui::markdown::DEFAULT_RENDER_WIDTH
        } else {
            self.last_width
        }
    }

    /// Render a model reasoning ("thinking") trace as a chromeless, foldable
    /// left-rail block (the `ThinkingBlock` design-system component): reasoning
    /// is internal, verbose, and secondary, so it gets **no box** — only a muted
    /// `┊` rail on its body and a `THINKING` label with honest telemetry.
    /// Progressive disclosure: the first paragraph shows as a preview, the rest
    /// folds behind an `… N more paragraphs   ctrl+o to expand` affordance
    /// (`toggle_latest_panel`). Short (single-paragraph) reasoning is shown
    /// whole and is not foldable. A `redacted` block has no recoverable text, so
    /// a placeholder is shown and the original reasoning is never rendered.
    fn push_thinking_block(&mut self, text: &str, redacted: bool) {
        // Intentionally do NOT finish the live stream here. Reasoning is emitted
        // at completion, after the answer's text deltas have already streamed
        // into `self.streaming` but before `AssistantTextEnd` commits them.
        // Committing the stream now (via `begin_block`) would render the answer
        // *above* the thinking block and double-commit it when `AssistantTextEnd`
        // arrives. Adding the rail rows while the stream stays pending keeps the
        // reasoning above the answer, which is committed afterwards.
        self.push_blank();
        self.mark_append_dirty();
        let elapsed = self
            .provider_turn_started
            .map(|started| format_elapsed_compact(started.elapsed()));
        // Paragraph groups: rendered markdown lines split at blank lines. The
        // first group is the collapsed preview; the rest hides behind the fold.
        let groups: Vec<Vec<Line<'static>>> = if redacted {
            vec![vec![Line::from(Span::styled(
                REDACTED_THINKING_BODY,
                dim_style(),
            ))]]
        } else {
            let theme = MarkdownTheme::thinking();
            let width = self.markdown_content_width();
            let mut groups: Vec<Vec<Line<'static>>> = Vec::new();
            let mut current: Vec<Line<'static>> = Vec::new();
            for line in render_markdown_themed(text, &theme, width) {
                if line_text(&line).trim().is_empty() {
                    if !current.is_empty() {
                        groups.push(std::mem::take(&mut current));
                    }
                } else {
                    current.push(line);
                }
            }
            if !current.is_empty() {
                groups.push(current);
            }
            groups
        };
        let foldable = groups.len() > 1;
        self.thinking_header_row = Some(self.rows.len());
        self.thinking_elapsed = elapsed.clone();
        self.rows.push(TranscriptRow::chrome(ChromeRow::RailHeader {
            expanded: false,
            label: THINKING_LABEL.to_string(),
            right: elapsed.unwrap_or_default(),
            foldable,
        }));
        let hidden = groups.len().saturating_sub(1);
        for (index, group) in groups.into_iter().enumerate() {
            if index > 0 {
                self.rows
                    .push(rail_body_row(Line::default()).with_fold(FoldVis::WhenExpanded));
            }
            let fold = if index == 0 {
                FoldVis::Always
            } else {
                FoldVis::WhenExpanded
            };
            for line in group {
                self.rows.push(rail_body_row(line).with_fold(fold));
            }
        }
        if foldable {
            let plural = if hidden == 1 { "" } else { "s" };
            let left = format!("\u{2026} {hidden} more paragraph{plural}");
            let text = right_align_pair(&left, "ctrl+o to expand", self.markdown_content_width());
            self.rows
                .push(TranscriptRow::new(text, dim_style()).with_fold(FoldVis::WhenCollapsed));
        }
        self.rows.push(TranscriptRow::chrome(ChromeRow::RailEnd));
    }

    /// Patch the current turn's thinking header with the provider-reported
    /// reasoning token count (`↓2.4k 12s`) once usage is known.
    fn set_thinking_telemetry(&mut self, reasoning_tokens: u64) {
        if reasoning_tokens == 0 {
            return;
        }
        let Some(index) = self.thinking_header_row else {
            return;
        };
        self.mark_dirty_from(index);
        if let Some(ChromeRow::RailHeader { right, .. }) =
            self.rows.get_mut(index).and_then(|row| row.chrome.as_mut())
        {
            let tokens = format!("↓{}", super::screen::compact_count(reasoning_tokens));
            *right = match &self.thinking_elapsed {
                Some(elapsed) => format!("{tokens} {elapsed}"),
                None => tokens,
            };
        }
    }

    /// A quiet unboxed notice row (the `Notice` design-system component):
    /// glyph + message, with an optional right-aligned dim hint. State is the
    /// glyph + message, never color alone.
    fn push_notice_row(&mut self, glyph: &str, glyph_style: Style, message: &str, hint: &str) {
        self.begin_block();
        self.mark_append_dirty();
        let left = format!("{glyph} {message}");
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Notice {
                glyph: glyph.to_string(),
                glyph_style,
                message: message.to_string(),
                hint: hint.to_string(),
            },
            left,
            dim_style(),
        ));
        self.push_blank();
    }

    pub(super) fn push_turn_divider(
        &mut self,
        had_work: bool,
        elapsed: Option<Duration>,
        usage: Option<&ProviderUsage>,
    ) {
        if !had_work {
            return;
        }
        self.finish_stream();
        self.push_blank();
        self.mark_append_dirty();
        self.rows.push(TranscriptRow {
            text: turn_divider_label(elapsed, usage),
            style: dim_style(),
            continuation_prefix: None,
            line: None,
            fold: FoldVis::Always,
            word_wrap: false,
            background: None,
            hrule: true,
            chrome: None,
        });
        self.push_blank();
    }

    /// Commit any in-flight streamed assistant text into the transcript.
    fn finish_stream(&mut self) {
        self.streaming_render = None;
        if let Some(text) = self.streaming.take()
            && !text.is_empty()
        {
            self.push_assistant_text(&text);
            self.push_blank();
        }
    }

    pub(super) fn record_approval(&mut self, call: &ToolCall, decision: ApprovalDecision) {
        let scope = match decision {
            ApprovalDecision::Allow => "this time",
            ApprovalDecision::AllowAlways => "this session",
            ApprovalDecision::AllowProject => "this project",
            ApprovalDecision::Deny => return,
        };
        self.begin_block();
        self.push_approval_panel(approval_line(call, scope), false);
    }

    fn push_approval_panel(&mut self, line: Line<'static>, failed: bool) {
        self.mark_append_dirty();
        self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
        // Approval decisions have no duration; the shared cluster reserves an
        // empty elapsed field so the glyph lands in the same column as every
        // other panel family.
        let base_style = if failed { err_style() } else { ok_style() };
        self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
            expanded: true,
            title: "APPROVAL",
            meta: "decision".to_string(),
            right: header_right_cluster(
                if failed { "■" } else { "◆" },
                base_style,
                if failed { " DENIED" } else { " APPROVED" },
                base_style.add_modifier(ratatui::style::Modifier::BOLD),
                "",
            ),
        }));
        self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
        let text = line_text(&line);
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Body { line, bg: None },
            text,
            panel_style(),
        ));
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
    }

    /// Fallback (non-streamed) result render, used when no live exec cell was
    /// opened: exploration tools group under `Explored`; everything else gets a
    /// `• Ran`/`✗ Ran` header with the same status-colored bullet + duration as
    /// the streamed finalize, so both paths look identical.
    fn push_tool_result(
        &mut self,
        call: &ToolCall,
        content: &str,
        exit_code: Option<i32>,
        duration: Option<std::time::Duration>,
    ) {
        let renderer = tool_render::resolve(call);
        if renderer.kind() == ToolPanelKind::Explore {
            self.push_explored_result(call, content, duration);
            return;
        }
        let failed = exit_code.is_some_and(|code| code != 0);
        self.begin_block();
        self.append_tool_panel(
            renderer,
            call,
            panel_state(false, failed),
            duration,
            None,
            ToolOutcome::Done { content, exit_code },
        );
    }

    fn push_tool_error(&mut self, call: &ToolCall, message: &str) {
        self.begin_block();
        let renderer = tool_render::resolve(call);
        self.append_tool_panel(
            renderer,
            call,
            PanelState::Error,
            None,
            None,
            ToolOutcome::Error {
                message,
                streamed: "",
            },
        );
    }

    fn push_tool_cancelled(&mut self, call: &ToolCall) {
        self.begin_block();
        let renderer = tool_render::resolve(call);
        self.append_tool_panel(
            renderer,
            call,
            PanelState::Cancelled,
            None,
            None,
            ToolOutcome::Cancelled { streamed: "" },
        );
    }

    /// Wrap-width-aware [`RenderCtx`] for renderer body production.
    fn render_ctx(&self) -> RenderCtx {
        RenderCtx {
            width: self.wrap_width(),
        }
    }

    /// Push a complete standard tool panel (Top/Header/Separator/body/Bottom)
    /// to `self.rows`, with the body produced by the renderer registry under
    /// failure isolation. When the body is fold-capped, the header is marked as
    /// a collapsed preview so ctrl+o reveals the hidden lines. This is the
    /// single dispatch path for SHELL/EDIT/generic panels; EXPLORE keeps its
    /// grouped path.
    fn append_tool_panel(
        &mut self,
        renderer: &dyn tool_render::ToolRenderer,
        call: &ToolCall,
        state: PanelState,
        duration: Option<std::time::Duration>,
        started: Option<Instant>,
        outcome: ToolOutcome<'_>,
    ) {
        self.push_tool_header(renderer, call, state, duration, started);
        let ctx = self.render_ctx();
        let body = tool_render::render_body(renderer, &ctx, call, &outcome);
        let foldable = body.iter().any(|row| row.fold != FoldVis::Always);
        self.rows.extend(body);
        if foldable {
            self.mark_panel_preview();
        }
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
    }

    /// Collect the rows of a standard tool panel without committing them, for
    /// in-place replacement of a live/active panel.
    fn collect_tool_panel(
        &mut self,
        renderer: &dyn tool_render::ToolRenderer,
        call: &ToolCall,
        state: PanelState,
        duration: Option<std::time::Duration>,
        started: Option<Instant>,
        outcome: ToolOutcome<'_>,
    ) -> Vec<TranscriptRow> {
        self.collect_rows(|this| {
            this.append_tool_panel(renderer, call, state, duration, started, outcome);
        })
    }

    /// Build the standard panel header from the renderer's title/meta plus the
    /// transcript-owned lifecycle state/duration.
    fn push_tool_header(
        &mut self,
        renderer: &dyn tool_render::ToolRenderer,
        call: &ToolCall,
        state: PanelState,
        duration: Option<std::time::Duration>,
        started: Option<Instant>,
    ) {
        let meta = renderer.header_meta(call);
        let plain = renderer.plain_meta(call);
        self.push_panel_header(PanelHeaderSpec {
            title: renderer.title(),
            meta: &meta,
            plain_meta: &plain,
            state,
            duration,
            started,
        });
    }

    /// Mark the most recent panel header as collapsed (preview) so foldable
    /// output starts capped; toggling reveals the hidden lines.
    fn mark_panel_preview(&mut self) {
        let Some(index) = self
            .rows
            .iter()
            .rposition(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::Header { .. })))
        else {
            return;
        };
        self.mark_dirty_from(index);
        if let Some(ChromeRow::Header { expanded, .. }) = self.rows[index].chrome.as_mut() {
            *expanded = false;
        }
    }

    fn push_panel_header_with_expanded(&mut self, spec: PanelHeaderSpec<'_>, expanded: bool) {
        self.mark_append_dirty();
        // A pending preview has no elapsed time by definition (`◇ PREVIEW`
        // omits the duration; asserting one would fabricate a measurement).
        let elapsed = if spec.state == PanelState::Preview {
            String::new()
        } else if spec.state == PanelState::Running {
            spec.started
                .map(|started| format_elapsed_compact(started.elapsed()))
                .unwrap_or_else(|| "0.0s".to_string())
        } else {
            spec.duration
                .map(format_elapsed_compact)
                .or_else(|| {
                    spec.started
                        .map(|started| format_elapsed_compact(started.elapsed()))
                })
                .unwrap_or_else(|| "0.0s".to_string())
        };
        let plain = format!("{} {}", spec.state.plain_prefix(), spec.plain_meta);
        self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Header {
                expanded,
                title: spec.title,
                meta: spec.meta.to_string(),
                right: header_right_cluster(
                    spec.state.symbol(),
                    spec.state.dot_style(),
                    spec.state.label(),
                    spec.state.label_style(),
                    &elapsed,
                ),
            },
            plain,
            tool_header_style(),
        ));
        self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
    }

    fn push_panel_header(&mut self, spec: PanelHeaderSpec<'_>) {
        self.push_panel_header_with_expanded(spec, true);
    }

    #[cfg(test)]
    pub(super) fn push_shell_header(
        &mut self,
        state: PanelState,
        duration: Option<std::time::Duration>,
        started: Option<Instant>,
        target: &str,
    ) {
        self.push_panel_header(PanelHeaderSpec {
            title: "SHELL",
            meta: "bash",
            plain_meta: target,
            state,
            duration,
            started,
        });
    }

    /// Test-only convenience that renders a finalized shell panel through the
    /// registry dispatch path. Production result/error paths go through
    /// [`Self::append_tool_panel`].
    #[cfg(test)]
    pub(super) fn push_shell_panel(
        &mut self,
        call: &ToolCall,
        content: &str,
        running: bool,
        failed: bool,
        duration: Option<std::time::Duration>,
        error: Option<&str>,
    ) {
        let renderer = tool_render::resolve(call);
        let outcome = if let Some(message) = error {
            ToolOutcome::Error {
                message,
                streamed: content,
            }
        } else if running {
            ToolOutcome::Running { streamed: content }
        } else {
            ToolOutcome::Done {
                content,
                exit_code: Some(if failed { 1 } else { 0 }),
            }
        };
        self.append_tool_panel(
            renderer,
            call,
            panel_state(running, failed),
            duration,
            None,
            outcome,
        );
    }

    /// Open a live exec block: a `• Running {target}` header under a fresh
    /// separator, tracked as the active cell so deltas and the final result
    /// finalize it in place.
    fn begin_exec(&mut self, call: ToolCall) {
        self.begin_block();
        let body_start = self.rows.len();
        let started = Instant::now();
        let renderer = tool_render::resolve(&call);
        self.append_tool_panel(
            renderer,
            &call,
            PanelState::Running,
            None,
            Some(started),
            ToolOutcome::Running { streamed: "" },
        );
        self.active_exec = Some(ActiveExec {
            call,
            output: String::new(),
            body_start,
            started,
        });
    }

    fn begin_tool(&mut self, call: ToolCall) {
        self.begin_block();
        let body_start = self.rows.len();
        let started = Instant::now();
        let renderer = tool_render::resolve(&call);
        self.append_tool_panel(
            renderer,
            &call,
            PanelState::Running,
            None,
            Some(started),
            ToolOutcome::Running { streamed: "" },
        );
        self.active_tool = Some(ActiveTool {
            call,
            body_start,
            started,
        });
    }

    /// Push a complete EDIT panel whose body is the canonical block diff plus
    /// the quiet `+added  −removed` footer (`EditOutput` design-system
    /// component). Long diffs fold to a capped preview.
    fn push_edit_panel(
        &mut self,
        call: &ToolCall,
        diff: &str,
        state: PanelState,
        duration: Option<Duration>,
        started: Option<Instant>,
        error: Option<&str>,
    ) {
        let renderer = tool_render::resolve(call);
        let meta = renderer.header_meta(call);
        let plain = renderer.plain_meta(call);
        self.push_panel_header(PanelHeaderSpec {
            title: "EDIT",
            meta: &meta,
            plain_meta: &plain,
            state,
            duration,
            started,
        });
        let diff_rows = diff_table_rows(diff);
        let cap = super::MAX_TOOL_OUTPUT_ROWS;
        let foldable = diff_rows.len() > cap + 1;
        if foldable {
            let hidden = diff_rows.len() - cap;
            for row in diff_rows.iter().take(cap).cloned() {
                self.rows.push(row.with_fold(FoldVis::WhenCollapsed));
            }
            let left = format!("\u{2026} {hidden} more lines");
            let hint = right_align_pair(&left, "ctrl+o to expand", self.wrap_width());
            self.rows.push(
                TranscriptRow::chrome_with_text(
                    ChromeRow::BodyRight {
                        left: Line::from(Span::styled(left.clone(), dim_style())),
                        right: "ctrl+o to expand".to_string(),
                        right_style: dim_style(),
                        bg: None,
                    },
                    hint,
                    dim_style(),
                )
                .with_fold(FoldVis::WhenCollapsed),
            );
            for row in diff_rows {
                self.rows.push(row.with_fold(FoldVis::WhenExpanded));
            }
        } else if diff_rows.is_empty() && error.is_none() {
            // A preview/edit whose diff is missing or empty (e.g. the edit's
            // old_string did not match the file) would otherwise render an
            // empty frame -- header + bottom border, zero body rows -- leaving
            // nothing to review while the approval modal waits. Emit one honest
            // dim placeholder instead of fabricating a diff.
            let placeholder = "no preview available";
            self.rows.push(TranscriptRow::chrome_with_text(
                ChromeRow::Body {
                    line: Line::from(Span::styled(placeholder, dim_style())),
                    bg: None,
                },
                placeholder.to_string(),
                dim_style(),
            ));
        } else {
            self.rows.extend(diff_rows);
        }
        if let Some(message) = error {
            self.rows.push(TranscriptRow::chrome_with_text(
                ChromeRow::Body {
                    line: Line::from(Span::styled(format!("error: {message}"), err_style())),
                    bg: None,
                },
                format!("error: {message}"),
                err_style(),
            ));
        }
        let (added, removed) = diff_counts(diff);
        if added + removed > 0 {
            self.rows.push(TranscriptRow::chrome_with_text(
                ChromeRow::Body {
                    line: Line::default(),
                    bg: None,
                },
                String::new(),
                panel_style(),
            ));
            let note = diff.contains("--- /dev/null").then_some("new file");
            self.rows.push(diff_footer_row(added, removed, note));
        }
        if foldable {
            self.mark_panel_preview();
        }
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
    }

    /// Render the final task diff (issue #264) as a bordered panel: a `DIFF`
    /// header carrying the summary meta, each per-file summary line, then the
    /// combined unified diff through the shared `diff_table_rows` colorizer.
    /// Read-only presentation -- not tracked as an active edit.
    fn push_task_diff_panel(&mut self, summary: &[String], diff: &str) {
        self.begin_block();
        // The first summary line ("N files changed, +X/-Y") rides in the header
        // meta; the per-file lines render as body rows below it.
        let meta = summary.first().cloned().unwrap_or_default();
        self.push_panel_header(PanelHeaderSpec {
            title: "DIFF",
            meta: &meta,
            plain_meta: &meta,
            state: PanelState::Done,
            duration: None,
            started: None,
        });
        for line in summary.iter().skip(1) {
            self.rows.push(TranscriptRow::chrome_with_text(
                ChromeRow::Body {
                    line: Line::from(Span::styled(line.clone(), dim_style())),
                    bg: None,
                },
                line.clone(),
                dim_style(),
            ));
        }
        self.rows.extend(diff_table_rows(diff));
        let (added, removed) = diff_counts(diff);
        if added + removed > 0 {
            self.rows.push(diff_footer_row(added, removed, None));
        }
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
    }

    /// Open (or reopen) the EDIT panel for a pending mutation: `◇ PREVIEW`,
    /// diff body, tracked as the active edit so the same panel is finalized in
    /// place when the tool runs.
    fn begin_edit_preview(&mut self, call: &ToolCall, diff: String) {
        self.clear_active_tool_for_preview(call);
        self.begin_block();
        let body_start = self.rows.len();
        self.push_edit_panel(call, &diff, PanelState::Preview, None, None, None);
        self.active_edit = Some(ActiveEdit {
            call_id: call.id.clone(),
            diff,
            body_start,
            started: None,
        });
    }

    /// Rebuild the active EDIT panel in place for a new lifecycle state.
    /// Returns false when `call` does not match the tracked edit. `keep_open`
    /// (the running transition) retains the tracked edit for finalization.
    fn rebuild_active_edit(
        &mut self,
        call: &ToolCall,
        state: PanelState,
        duration: Option<Duration>,
        error: Option<&str>,
        keep_open: bool,
    ) -> bool {
        let Some(mut active) = self.active_edit.take() else {
            return false;
        };
        if active.call_id != call.id {
            self.active_edit = Some(active);
            return false;
        }
        if state == PanelState::Running && active.started.is_none() {
            active.started = Some(Instant::now());
        }
        let diff = active.diff.clone();
        let started = active.started;
        let mut replacement = self.collect_rows(|this| {
            this.push_edit_panel(call, &diff, state, duration, started, error);
        });
        let end = self.panel_end_from(active.body_start);
        if let Some(expanded) = self.panel_reveal_state_in(active.body_start, end) {
            Self::preserve_panel_expanded(&mut replacement, expanded);
        }
        self.mark_dirty_from(active.body_start);
        self.rows.splice(active.body_start..end, replacement);
        if keep_open {
            self.active_edit = Some(active);
        }
        true
    }

    fn panel_end_from(&self, start: usize) -> usize {
        self.rows[start..]
            .iter()
            .position(|row| {
                matches!(
                    row.chrome.as_ref(),
                    Some(ChromeRow::Bottom | ChromeRow::RailEnd)
                )
            })
            .map_or(start, |offset| start + offset + 1)
    }

    fn active_tool_panel_end(&self, active: &ActiveTool) -> usize {
        self.panel_end_from(active.body_start)
    }

    /// The reveal state to carry across an in-place panel rebuild, but only when
    /// the old panel was foldable (had capped output). Returning `None` lets a
    /// freshly capped panel keep its built-in preview default instead of
    /// inheriting an unrelated `expanded` flag.
    fn panel_reveal_state_in(&self, start: usize, end: usize) -> Option<bool> {
        if !self.rows[start..end]
            .iter()
            .any(|row| row.fold != FoldVis::Always)
        {
            return None;
        }
        self.rows[start..end]
            .iter()
            .find_map(|row| match row.chrome.as_ref() {
                Some(ChromeRow::Header { expanded, .. }) => Some(*expanded),
                _ => None,
            })
    }

    fn preserve_panel_expanded(replacement: &mut [TranscriptRow], expanded: bool) {
        if let Some(row_expanded) =
            replacement
                .iter_mut()
                .find_map(|row| match row.chrome.as_mut() {
                    Some(ChromeRow::Header { expanded, .. }) => Some(expanded),
                    _ => None,
                })
        {
            *row_expanded = expanded;
        }
    }

    fn replace_active_tool_panel(
        &mut self,
        active: &ActiveTool,
        mut replacement: Vec<TranscriptRow>,
    ) {
        let end = self.active_tool_panel_end(active);
        if let Some(expanded) = self.panel_reveal_state_in(active.body_start, end) {
            Self::preserve_panel_expanded(&mut replacement, expanded);
        }
        self.mark_dirty_from(active.body_start);
        self.rows.splice(active.body_start..end, replacement);
    }

    fn active_exec_panel_end(&self, active: &ActiveExec) -> usize {
        self.panel_end_from(active.body_start)
    }

    fn replace_active_exec_panel(
        &mut self,
        active: &ActiveExec,
        mut replacement: Vec<TranscriptRow>,
    ) {
        let end = self.active_exec_panel_end(active);
        if let Some(expanded) = self.panel_reveal_state_in(active.body_start, end) {
            Self::preserve_panel_expanded(&mut replacement, expanded);
        }
        self.mark_dirty_from(active.body_start);
        self.rows.splice(active.body_start..end, replacement);
    }

    fn collect_rows(&mut self, write: impl FnOnce(&mut Self)) -> Vec<TranscriptRow> {
        let start = self.rows.len();
        write(self);
        self.rows.split_off(start)
    }

    fn finalize_active_tool(
        &mut self,
        call: &ToolCall,
        content: &str,
        duration: Option<std::time::Duration>,
    ) -> bool {
        let Some(active) = self.active_tool.take() else {
            return false;
        };
        if active.call.id != call.id {
            self.active_tool = Some(active);
            return false;
        }
        let renderer = tool_render::resolve(call);
        let rows = self.collect_tool_panel(
            renderer,
            call,
            PanelState::Done,
            duration,
            Some(active.started),
            ToolOutcome::Done {
                content,
                exit_code: None,
            },
        );
        self.replace_active_tool_panel(&active, rows);
        true
    }

    fn finalize_active_tool_error(&mut self, call: &ToolCall, message: &str) -> bool {
        let Some(active) = self.active_tool.take() else {
            return false;
        };
        if active.call.id != call.id {
            self.active_tool = Some(active);
            return false;
        }
        let renderer = tool_render::resolve(call);
        let rows = self.collect_tool_panel(
            renderer,
            call,
            PanelState::Error,
            None,
            Some(active.started),
            ToolOutcome::Error {
                message,
                streamed: "",
            },
        );
        self.replace_active_tool_panel(&active, rows);
        true
    }

    fn finalize_active_tool_cancelled(&mut self, call: &ToolCall) -> bool {
        let Some(active) = self.active_tool.take() else {
            return false;
        };
        if active.call.id != call.id {
            self.active_tool = Some(active);
            return false;
        }
        let renderer = tool_render::resolve(call);
        let rows = self.collect_tool_panel(
            renderer,
            call,
            PanelState::Cancelled,
            None,
            Some(active.started),
            ToolOutcome::Cancelled { streamed: "" },
        );
        self.replace_active_tool_panel(&active, rows);
        true
    }

    fn clear_active_tool_for_preview(&mut self, call: &ToolCall) {
        if self
            .active_tool
            .as_ref()
            .is_some_and(|active| active.call.id == call.id)
            && let Some(active) = self.active_tool.take()
        {
            self.replace_active_tool_panel(&active, Vec::new());
        }
    }

    /// Re-render the open exec block in place from its bounded output buffer: the
    /// `Running` header followed by the flood-capped live tail.
    fn relayout_active_running(&mut self) {
        let Some(active) = self.active_exec.take() else {
            return;
        };
        let renderer = tool_render::resolve(&active.call);
        let rows = self.collect_tool_panel(
            renderer,
            &active.call,
            PanelState::Running,
            None,
            Some(active.started),
            ToolOutcome::Running {
                streamed: &active.output,
            },
        );
        self.replace_active_exec_panel(&active, rows);
        self.active_exec = Some(active);
    }

    /// Finalize the open exec block in place (no new separator): rewrite the
    /// header to `• Ran`/`✗ Ran` with the status-colored bullet and duration,
    /// then render the authoritative final output.
    fn finalize_active(
        &mut self,
        call: &ToolCall,
        content: &str,
        exit_code: Option<i32>,
        duration: Option<std::time::Duration>,
    ) {
        let Some(active) = self.active_exec.take() else {
            return;
        };
        if active.call.id != call.id {
            self.active_exec = Some(active);
            return;
        }
        let failed = exit_code.is_some_and(|code| code != 0);
        let renderer = tool_render::resolve(call);
        let rows = self.collect_tool_panel(
            renderer,
            call,
            panel_state(false, failed),
            duration,
            Some(active.started),
            ToolOutcome::Done { content, exit_code },
        );
        self.replace_active_exec_panel(&active, rows);
    }

    /// Finalize the open exec block as an error/cancellation in place: a red
    /// `✗ Ran` header, whatever streamed so far (so a cancelled command keeps
    /// its partial output), then the error line.
    fn finalize_active_error(&mut self, call: &ToolCall, message: &str) {
        let Some(active) = self.active_exec.take() else {
            return;
        };
        if active.call.id != call.id {
            self.active_exec = Some(active);
            return;
        }
        let renderer = tool_render::resolve(call);
        let rows = self.collect_tool_panel(
            renderer,
            call,
            PanelState::Error,
            None,
            Some(active.started),
            ToolOutcome::Error {
                message,
                streamed: &active.output,
            },
        );
        self.replace_active_exec_panel(&active, rows);
    }

    fn finalize_active_cancelled(&mut self, call: &ToolCall) -> bool {
        let Some(active) = self.active_exec.take() else {
            return false;
        };
        if active.call.id != call.id {
            self.active_exec = Some(active);
            return false;
        }
        let renderer = tool_render::resolve(call);
        let rows = self.collect_tool_panel(
            renderer,
            call,
            PanelState::Cancelled,
            None,
            Some(active.started),
            ToolOutcome::Cancelled {
                streamed: &active.output,
            },
        );
        self.replace_active_exec_panel(&active, rows);
        true
    }

    /// The width to assume when shaping rows during `apply` (before a frame has
    /// set the real width). Falls back to a sane 80 columns.
    fn wrap_width(&self) -> usize {
        if self.last_width == 0 {
            80
        } else {
            self.last_width
        }
    }

    fn explore_header_right(state: PanelState, duration: Option<Duration>) -> Vec<(String, Style)> {
        let elapsed = duration
            .map(format_elapsed_compact)
            .unwrap_or_else(|| "0.0s".to_string());
        header_right_cluster(
            state.symbol(),
            state.dot_style(),
            state.label(),
            state.label_style(),
            &elapsed,
        )
    }

    fn current_explore_header_row(&self) -> Option<usize> {
        self.rows.iter().rposition(|row| {
            matches!(
                row.chrome.as_ref(),
                Some(ChromeRow::Header {
                    title: "EXPLORE",
                    ..
                })
            )
        })
    }

    fn active_exploration_duration(&self, running: bool) -> Option<Duration> {
        if running {
            let started = self
                .active_explorations
                .iter()
                .map(|active| active.started)
                .min()?;
            return Some(started.elapsed());
        }
        self.active_explorations
            .iter()
            .filter_map(|active| active.duration)
            .max()
            .or_else(|| {
                self.active_explorations
                    .iter()
                    .map(|active| active.started)
                    .min()
                    .map(|started| started.elapsed())
            })
    }

    /// EXPLORE header meta, single-sourced from the renderer registry.
    fn explore_meta(&self, call: &ToolCall) -> String {
        tool_render::resolve(call).header_meta(call)
    }

    /// EXPLORE body row, single-sourced from the renderer's one-row body for
    /// the given outcome (falls back to an empty dim row only if a misbehaving
    /// renderer produced nothing).
    fn explore_row(&self, call: &ToolCall, outcome: ToolOutcome<'_>) -> TranscriptRow {
        let renderer = tool_render::resolve(call);
        let ctx = self.render_ctx();
        tool_render::render_body(renderer, &ctx, call, &outcome)
            .into_iter()
            .next()
            .unwrap_or_else(|| {
                TranscriptRow::chrome_with_text(
                    ChromeRow::Body {
                        line: Line::default(),
                        bg: None,
                    },
                    String::new(),
                    dim_style(),
                )
            })
    }

    fn set_explore_header(
        &mut self,
        call: &ToolCall,
        state: PanelState,
        duration: Option<Duration>,
    ) {
        let Some(header) = self.current_explore_header_row() else {
            return;
        };
        let (meta, expanded) = match self.rows[header].chrome.as_ref() {
            Some(ChromeRow::Header { meta, expanded, .. }) => (meta.clone(), *expanded),
            _ => (self.explore_meta(call), true),
        };
        self.mark_dirty_from(header);
        self.rows[header] = TranscriptRow::chrome(ChromeRow::Header {
            expanded,
            title: "EXPLORE",
            meta,
            right: Self::explore_header_right(state, duration),
        });
    }

    fn update_explore_header_from_active(&mut self, call: &ToolCall) {
        let running = self.active_explorations.iter().any(|active| !active.done);
        let failed = self.active_explorations.iter().any(|active| active.failed);
        let cancelled = self
            .active_explorations
            .iter()
            .any(|active| active.cancelled);
        let duration = self.active_exploration_duration(running);
        let state = if running {
            PanelState::Running
        } else if failed {
            PanelState::Error
        } else if cancelled {
            PanelState::Cancelled
        } else {
            PanelState::Done
        };
        self.set_explore_header(call, state, duration);
        if !running {
            self.active_explorations.clear();
        }
    }

    fn pop_trailing_explore_bottom(&mut self) {
        if matches!(
            self.rows.last().and_then(|row| row.chrome.as_ref()),
            Some(ChromeRow::Bottom)
        ) {
            self.mark_dirty_from(self.rows.len().saturating_sub(1));
            self.rows.pop();
        }
    }

    fn push_explore_body(&mut self, call: &ToolCall, content: &str, duration: Option<Duration>) {
        self.mark_append_dirty();
        let meta = self.explore_meta(call);
        let row = self.explore_row(
            call,
            ToolOutcome::Done {
                content,
                exit_code: None,
            },
        );
        if self.exploring_open {
            self.pop_trailing_explore_bottom();
            self.set_explore_header(call, panel_state(false, false), duration);
        } else {
            self.push_blank();
            self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
                expanded: true,
                title: "EXPLORE",
                meta,
                right: Self::explore_header_right(panel_state(false, false), duration),
            }));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
        }
        self.rows.push(row);
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        self.exploring_open = true;
    }

    fn push_explored_result(&mut self, call: &ToolCall, content: &str, duration: Option<Duration>) {
        self.finish_stream();
        let row = self.explore_row(
            call,
            ToolOutcome::Done {
                content,
                exit_code: None,
            },
        );
        if self.finish_exploration(call, row, duration, false, false) {
            return;
        }
        self.push_explore_body(call, content, duration);
    }

    fn push_explored_start(&mut self, call: &ToolCall) {
        self.mark_append_dirty();
        self.finish_stream();
        let started = Instant::now();
        let meta = self.explore_meta(call);
        let body = self.explore_row(call, ToolOutcome::Running { streamed: "" });
        if self.exploring_open {
            self.pop_trailing_explore_bottom();
        } else {
            self.push_blank();
            self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
                expanded: true,
                title: "EXPLORE",
                meta,
                right: Self::explore_header_right(PanelState::Running, Some(Duration::ZERO)),
            }));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
        }
        let row = self.rows.len();
        self.rows.push(body);
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        self.exploring_open = true;
        self.active_explorations.push(ActiveExploration {
            call_id: call.id.clone(),
            row,
            started,
            duration: None,
            failed: false,
            cancelled: false,
            done: false,
        });
        self.update_explore_header_from_active(call);
    }

    fn replace_explore_body_at(&mut self, row: usize, replacement: TranscriptRow) -> bool {
        let Some(slot) = self.rows.get(row) else {
            return false;
        };
        if !matches!(slot.chrome.as_ref(), Some(ChromeRow::Body { .. })) {
            return false;
        }
        self.mark_dirty_from(row);
        self.rows[row] = replacement;
        true
    }

    fn finish_exploration(
        &mut self,
        call: &ToolCall,
        row_body: TranscriptRow,
        duration: Option<Duration>,
        failed: bool,
        cancelled: bool,
    ) -> bool {
        let Some(pos) = self
            .active_explorations
            .iter()
            .position(|active| active.call_id == call.id)
        else {
            return false;
        };
        let row = self.active_explorations[pos].row;
        self.active_explorations[pos].duration = duration;
        self.active_explorations[pos].failed = failed;
        self.active_explorations[pos].cancelled = cancelled;
        self.active_explorations[pos].done = true;
        let replaced = self.replace_explore_body_at(row, row_body);
        debug_assert!(replaced);
        self.update_explore_header_from_active(call);
        true
    }

    fn push_explored_error(&mut self, call: &ToolCall, message: &str) -> bool {
        self.finish_stream();
        let row = self.explore_row(
            call,
            ToolOutcome::Error {
                message,
                streamed: "",
            },
        );
        self.finish_exploration(call, row, None, true, false)
    }

    fn push_explored_cancelled(&mut self, call: &ToolCall) -> bool {
        self.finish_stream();
        let row = self.explore_row(call, ToolOutcome::Cancelled { streamed: "" });
        self.finish_exploration(call, row, None, false, true)
    }

    /// Apply one semantic event to the transcript rows.
    pub(super) fn apply(&mut self, event: UiEvent) {
        let old_len = self.rows.len();
        match event {
            UiEvent::ProviderTurnStarted { .. } => {
                self.provider_turn_started = Some(Instant::now());
                self.thinking_header_row = None;
                self.thinking_elapsed = None;
            }
            UiEvent::ProviderTurnCompleted { usage, .. } => {
                if let Some(usage) = usage {
                    self.set_thinking_telemetry(usage.reasoning_output_tokens);
                }
            }
            UiEvent::CompactionApplied {
                original_tokens_estimate,
                summary_tokens_estimate,
                ..
            } => {
                // A runtime event, not the assistant speaking: a quiet `┊` info
                // notice with honest (runtime-measured) token counts.
                self.finish_stream();
                self.push_notice_row(
                    crate::ui::symbols::SEP,
                    dim_style(),
                    &format!(
                        "Context compacted — {} → {} tokens",
                        super::screen::compact_count(original_tokens_estimate),
                        super::screen::compact_count(summary_tokens_estimate),
                    ),
                    "",
                );
            }
            UiEvent::ProviderTurnCancelled { .. }
            | UiEvent::ProviderTurnError { .. }
            | UiEvent::ToolLifecycle { .. }
            | UiEvent::OutputHandleStored { .. } => {}
            UiEvent::AssistantTextDelta(delta) => {
                if self.streaming.is_none() {
                    // A fresh stream starts here: drop any memoized render of a
                    // prior stream so its `(len, width)` key can never collide.
                    self.streaming_render = None;
                    self.push_blank();
                }
                self.streaming
                    .get_or_insert_with(String::new)
                    .push_str(&delta);
            }
            UiEvent::AssistantTextEnd(text) => {
                // A non-empty end event is authoritative. Some providers only
                // send deltas and finish with an empty end marker; in that case
                // commit the accumulated stream instead of dropping it.
                self.streaming_render = None;
                let text = if text.is_empty() {
                    self.streaming.take().unwrap_or_default()
                } else {
                    self.streaming = None;
                    text
                };
                if !text.is_empty() {
                    self.push_blank();
                    self.push_assistant_text(&text);
                    self.push_blank();
                }
            }
            UiEvent::AssistantText(text) => {
                self.finish_stream();
                if !text.is_empty() {
                    self.push_blank();
                    self.push_assistant_text(&text);
                    self.push_blank();
                }
            }
            UiEvent::AssistantReasoning { text, redacted } => {
                // Reasoning arrives at completion, before the turn's
                // `AssistantText`/`AssistantTextEnd`. The thinking panel is
                // pushed above any still-pending streamed answer (which commits
                // afterwards), so the thinking block renders above the answer
                // without finishing/duplicating the stream here.
                if redacted || !text.is_empty() {
                    self.push_thinking_block(&text, redacted);
                }
            }
            UiEvent::SessionStarted => {
                self.finish_stream();
            }
            UiEvent::ToolProposed(_) => {
                // Non-gated tools show only their result row; nothing to render.
                self.finish_stream();
            }
            UiEvent::ToolStarted(call) => match tool_render::resolve(&call).kind() {
                ToolPanelKind::Explore => self.push_explored_start(&call),
                ToolPanelKind::Shell => self.begin_exec(call),
                ToolPanelKind::Generic => {
                    // A mutation whose diff already arrived keeps its EDIT
                    // panel: the preview flips to `● RUNNING` in place.
                    if !self.rebuild_active_edit(&call, PanelState::Running, None, None, true) {
                        self.begin_tool(call);
                    }
                }
            },
            UiEvent::ToolOutputDelta { call_id, chunk } => {
                if self
                    .active_exec
                    .as_ref()
                    .is_some_and(|a| a.call.id == call_id)
                {
                    if let Some(active) = self.active_exec.as_mut() {
                        active.output.push_str(&chunk);
                        // Bound the re-rendered buffer to its tail; only a few
                        // rows (MAX_TOOL_OUTPUT_ROWS) ever show and the full
                        // output arrives with the result.
                        if active.output.len() > MAX_EXEC_STREAM_BYTES {
                            let cut = active.output.len() - MAX_EXEC_STREAM_BYTES;
                            let cut = active.output.ceil_char_boundary(cut);
                            active.output.drain(..cut);
                        }
                    }
                    self.relayout_active_running();
                }
            }
            UiEvent::ToolAutoApproved(call) => {
                self.record_approval(&call, ApprovalDecision::AllowAlways);
            }
            UiEvent::DiffPreview { call, diff } => {
                self.begin_edit_preview(&call, diff);
            }
            UiEvent::ToolDenied(call) => {
                // The pending EDIT panel (if any) stays as the `◇ PREVIEW`
                // record of what was proposed; the decision is its own block.
                self.active_edit = None;
                self.begin_block();
                let mut spans = Vec::new();
                if call.name == "bash" {
                    spans.push(Span::styled("$ ", dim_style()));
                }
                spans.extend(ansi_spans(&run_target(&call), Style::default()));
                spans.push(Span::styled(
                    format!("  {} denied", crate::ui::symbols::SEP),
                    err_style(),
                ));
                self.push_approval_panel(Line::from(spans), true);
            }
            UiEvent::ToolResult {
                call,
                content,
                exit_code,
                duration,
            } => {
                if self
                    .active_exec
                    .as_ref()
                    .is_some_and(|a| a.call.id == call.id)
                {
                    self.finalize_active(&call, &content, exit_code, duration);
                } else if self.rebuild_active_edit(&call, PanelState::Done, duration, None, false) {
                } else if !self.finalize_active_tool(&call, &content, duration) {
                    self.push_tool_result(&call, &content, exit_code, duration);
                }
            }
            UiEvent::ToolError { call, message } => {
                if self
                    .active_exec
                    .as_ref()
                    .is_some_and(|a| a.call.id == call.id)
                {
                    self.finalize_active_error(&call, &message);
                } else if !self.rebuild_active_edit(
                    &call,
                    PanelState::Error,
                    None,
                    Some(&message),
                    false,
                ) && !self.finalize_active_tool_error(&call, &message)
                    && (tool_render::resolve(&call).kind() != ToolPanelKind::Explore
                        || !self.push_explored_error(&call, &message))
                {
                    self.push_tool_error(&call, &message);
                }
            }
            UiEvent::ToolCancelled(call) => {
                if self
                    .active_exec
                    .as_ref()
                    .is_some_and(|a| a.call.id == call.id)
                {
                    self.finalize_active_cancelled(&call);
                } else if !self.rebuild_active_edit(&call, PanelState::Cancelled, None, None, false)
                    && !self.finalize_active_tool_cancelled(&call)
                    && (tool_render::resolve(&call).kind() != ToolPanelKind::Explore
                        || !self.push_explored_cancelled(&call))
                {
                    self.push_tool_cancelled(&call);
                }
            }
            UiEvent::UserMessage(text) => {
                // A user message the loop injected mid-run (steering/follow-up).
                // Rendered as a user row at this point so the transcript order
                // matches provider context.
                self.commit_user(&text);
            }
            UiEvent::Notice(message) => {
                self.push_notice_row(crate::ui::symbols::SEP, dim_style(), &message, "");
            }
            UiEvent::TaskDiff { summary, diff } => {
                self.push_task_diff_panel(&summary, &diff);
            }
            UiEvent::TurnError { kind, message } => match kind {
                TurnErrorKind::Auth => {
                    self.push_notice_row(
                        crate::ui::symbols::ERROR,
                        err_style(),
                        &format!("auth error: {message}"),
                        "",
                    );
                    self.push(
                        "authentication required; re-run the login command",
                        dim_style(),
                    );
                }
                TurnErrorKind::Provider => {
                    self.push_notice_row(
                        crate::ui::symbols::ERROR,
                        err_style(),
                        &format!("provider error: {message}"),
                        "",
                    );
                }
            },
            UiEvent::TurnComplete => {
                self.finish_stream();
            }
        }
        if self.rows.len() > old_len {
            self.mark_dirty_from(old_len);
        }
        self.trim_history();
    }

    pub(super) fn trim_history(&mut self) {
        if self.rows.len() <= MAX_TRANSCRIPT_ROWS
            || self.streaming.is_some()
            || self.active_exec.is_some()
            || self.active_tool.is_some()
            || self.active_edit.is_some()
            || !self.active_explorations.is_empty()
        {
            return;
        }
        let remove = self.panel_safe_trim_index(self.rows.len() - MAX_TRANSCRIPT_ROWS);
        self.mark_dirty_from(0);
        self.rows.drain(..remove);
        self.thinking_header_row = self
            .thinking_header_row
            .and_then(|index| index.checked_sub(remove));
        // Prompt-start anchors shift with the trimmed head; trimmed-away
        // prompts are dropped.
        self.user_prompt_starts = self
            .user_prompt_starts
            .iter()
            .filter_map(|&index| index.checked_sub(remove))
            .collect();
        self.exploring_open = self.trailing_explore_panel_open();
    }

    fn panel_safe_trim_index(&self, min_remove: usize) -> usize {
        let mut remove = min_remove.min(self.rows.len());
        while remove < self.rows.len() && self.row_is_inside_panel(remove) {
            remove = self.panel_end_from(remove);
        }
        remove
    }

    fn row_is_inside_panel(&self, index: usize) -> bool {
        matches!(
            self.rows.get(index).and_then(|row| row.chrome.as_ref()),
            Some(
                ChromeRow::Header { .. }
                    | ChromeRow::Separator
                    | ChromeRow::Body { .. }
                    | ChromeRow::BodyRight { .. }
                    | ChromeRow::BodyRule { .. }
                    | ChromeRow::Bottom
            )
        )
    }

    fn trailing_explore_panel_open(&self) -> bool {
        let Some(last) = self.rows.iter().rposition(|row| !is_separator_row(row)) else {
            return false;
        };
        if !matches!(self.rows[last].chrome.as_ref(), Some(ChromeRow::Bottom)) {
            return false;
        }
        for row in self.rows[..=last].iter().rev() {
            match row.chrome.as_ref() {
                Some(ChromeRow::Header { title, .. }) => return *title == "EXPLORE",
                Some(ChromeRow::Top) => return false,
                _ => {}
            }
        }
        false
    }

    pub(super) fn toggle_latest_panel(&mut self) -> bool {
        let Some(header) = self.rows.iter().rposition(|row| {
            matches!(
                row.chrome.as_ref(),
                Some(ChromeRow::Header { .. } | ChromeRow::RailHeader { .. })
            )
        }) else {
            return false;
        };
        // Only foldable panels (those with capped output) respond to ctrl+o;
        // a panel with nothing hidden has no reveal state to toggle.
        let end = self.panel_end_from(header);
        if !self.rows[header..end]
            .iter()
            .any(|row| row.fold != FoldVis::Always)
        {
            return false;
        }
        self.mark_dirty_from(header);
        match self.rows[header].chrome.as_mut() {
            Some(ChromeRow::Header { expanded, .. } | ChromeRow::RailHeader { expanded, .. }) => {
                *expanded = !*expanded;
                true
            }
            _ => false,
        }
    }

    /// Row indices of every panel header (EXPLORE/SHELL/EDIT/thinking rails):
    /// the selectable scrollback entries for keyboard navigation. O(rows),
    /// called per selection keypress, never per frame.
    pub(super) fn panel_header_rows(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(idx, row)| {
                matches!(
                    row.chrome.as_ref(),
                    Some(ChromeRow::Header { .. } | ChromeRow::RailHeader { .. })
                )
                .then_some(idx)
            })
            .collect()
    }

    /// Set the fold state of the panel whose header row is `header`. Returns
    /// whether the state changed (false for non-headers, panels with nothing
    /// hidden, or an already-matching state).
    pub(super) fn set_panel_expanded_at(&mut self, header: usize, expand: bool) -> bool {
        if header >= self.rows.len() {
            return false;
        }
        if !matches!(
            self.rows[header].chrome.as_ref(),
            Some(ChromeRow::Header { .. } | ChromeRow::RailHeader { .. })
        ) {
            return false;
        }
        let end = self.panel_end_from(header);
        if !self.rows[header..end]
            .iter()
            .any(|row| row.fold != FoldVis::Always)
        {
            return false;
        }
        match self.rows[header].chrome.as_mut() {
            Some(ChromeRow::Header { expanded, .. } | ChromeRow::RailHeader { expanded, .. })
                if *expanded != expand =>
            {
                *expanded = expand;
                self.mark_dirty_from(header);
                true
            }
            _ => false,
        }
    }

    /// Current fold state of the panel header at `header`, if it is one.
    pub(super) fn panel_expanded_at(&self, header: usize) -> Option<bool> {
        match self.rows.get(header)?.chrome.as_ref() {
            Some(ChromeRow::Header { expanded, .. } | ChromeRow::RailHeader { expanded, .. }) => {
                Some(*expanded)
            }
            _ => None,
        }
    }

    /// Case-insensitive substring search over the VISIBLE physical lines of
    /// the wrap cache (folded-away content is not searched: a match the
    /// pager cannot scroll to would be a dead jump). The cache is refreshed
    /// at its current width first, so appends and fold changes since the last
    /// frame are searched and stale lines are not. Returns ascending
    /// visible-line indices. O(total visible lines) per invocation -- run per
    /// `/find`/`n`/`N` keypress, never per frame.
    pub(super) fn search_visible_lines(&mut self, query: &str) -> Vec<usize> {
        let needle = query.to_lowercase();
        let width = self.wrapped_cache.width;
        if needle.is_empty() || width == 0 {
            return Vec::new();
        }
        // Same cache-warming path rendering uses: dirty rows re-wrap, fold
        // visibility re-resolves.
        self.ensure_wrapped_cache(width);
        let mut out = Vec::new();
        let mut pos = 0usize;
        for layout in &self.wrapped_cache.rows {
            if !layout.visible {
                continue;
            }
            for line in &self.wrapped_cache.lines[layout.lines.clone()] {
                let text: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                if text.to_lowercase().contains(&needle) {
                    out.push(pos);
                }
                pos += 1;
            }
        }
        out
    }

    /// Visible physical line index of logical `row` under the WARM wrap cache
    /// (callers refresh via [`Self::visible_total`] first). `None` when the
    /// row is folded away or the cache does not cover it yet.
    pub(super) fn visible_line_of_row(&self, row: usize) -> Option<usize> {
        let layout = self.wrapped_cache.rows.get(row)?;
        if !layout.visible {
            return None;
        }
        Some(if row == 0 {
            0
        } else {
            self.wrapped_cache.rows[row - 1].visible_cum
        })
    }

    #[cfg(test)]
    pub(super) fn latest_panel_collapsed(&self) -> bool {
        self.rows
            .iter()
            .rev()
            .find_map(|row| match row.chrome.as_ref() {
                Some(
                    ChromeRow::Header { expanded, .. } | ChromeRow::RailHeader { expanded, .. },
                ) => Some(!*expanded),
                _ => None,
            })
            .unwrap_or(false)
    }

    /// Commit a submitted prompt into the transcript as plain pane text.
    /// This is display-only; the raw prompt still goes to Nexus unchanged
    /// through the loop.
    pub(super) fn commit_user(&mut self, text: &str) {
        self.mark_append_dirty();
        self.push_blank();
        self.user_prompt_starts.push(self.rows.len());
        pane::push_user_rows(&mut self.rows, text);
        self.trim_history();
    }

    /// Visible line of the newest user prompt that begins strictly above the
    /// viewport top -- the pager's sticky header anchor. Requires a warm wrap
    /// cache (compose refreshes it every frame). Binary search over the
    /// sorted anchors (prompt rows are always visible and line positions are
    /// monotone in row order), so the per-frame cost is O(log prompts), never
    /// a prompt walk.
    pub(super) fn sticky_prompt_line(&self, top: usize) -> Option<usize> {
        let above = self
            .user_prompt_starts
            .partition_point(|&row| self.visible_line_of_row(row).is_some_and(|line| line < top));
        let row = *self.user_prompt_starts.get(above.checked_sub(1)?)?;
        self.visible_line_of_row(row).filter(|&line| line < top)
    }

    fn ensure_wrapped_cache(&mut self, width: usize) {
        if self.wrapped_cache.width != width {
            self.wrapped_cache.invalidate_all(width);
        }

        let dirty_from = self
            .wrapped_cache
            .dirty_from
            .min(self.rows.len())
            .min(self.wrapped_cache.rows.len());
        if dirty_from < self.wrapped_cache.rows.len() {
            let line_start = self.wrapped_cache.rows[dirty_from].lines.start;
            self.wrapped_cache.rows.truncate(dirty_from);
            self.wrapped_cache.lines.truncate(line_start);
        }

        // Resume the fold-visibility scan from the last cached row instead of
        // rescanning the whole transcript: `expanded` and the cumulative
        // visible-line count carry forward, so re-wrapping and visibility are
        // both proportional to the dirty tail.
        let (mut expanded, mut visible_cum) =
            self.wrapped_cache.rows.last().map_or((true, 0), |layout| {
                (layout.expanded_after, layout.visible_cum)
            });
        for row in &self.rows[self.wrapped_cache.rows.len()..] {
            match row.chrome.as_ref() {
                // A panel opens fully shown until its header sets the state;
                // resetting at Top guards against a missing Bottom leaking a
                // prior panel's collapsed state into the next one.
                Some(ChromeRow::Top) => expanded = true,
                Some(
                    ChromeRow::Header { expanded: e, .. }
                    | ChromeRow::RailHeader { expanded: e, .. },
                ) => expanded = *e,
                _ => {}
            }
            let visible = match row.fold {
                FoldVis::Always => true,
                FoldVis::WhenCollapsed => !expanded,
                FoldVis::WhenExpanded => expanded,
            };
            let start = self.wrapped_cache.lines.len();
            row.render_rows(width, &mut self.wrapped_cache.lines);
            let end = self.wrapped_cache.lines.len();
            if visible {
                visible_cum += end - start;
            }
            if matches!(
                row.chrome.as_ref(),
                Some(ChromeRow::Bottom | ChromeRow::RailEnd)
            ) {
                expanded = true;
            }
            self.wrapped_cache.rows.push(RowLayout {
                lines: start..end,
                visible,
                visible_cum,
                expanded_after: expanded,
            });
        }

        self.wrapped_cache.dirty_from = self.rows.len();
    }

    pub(super) fn render(&mut self, width: u16) -> TranscriptRender {
        self.render_cached(width, false)
    }

    /// Refresh the streaming-preview memo for `width` (no-op when the stream
    /// did not grow). Shared by the full render and the pager's windowed
    /// render so both see the same transient stream rows.
    fn refresh_streaming_memo(&mut self, width: usize) {
        let Some(text) = self.streaming.as_ref() else {
            return;
        };
        let fresh = self
            .streaming_render
            .as_ref()
            .is_some_and(|memo| memo.len == text.len() && memo.width == width);
        if !fresh {
            let mut lines = Vec::new();
            for row in &pane::streaming_assistant_rows(text, width) {
                row.render_rows(width, &mut lines);
            }
            self.streaming_render = Some(StreamingRender {
                len: text.len(),
                width,
                lines,
            });
        }
    }

    /// Number of transient streaming-preview lines at `width` (0 when idle).
    fn streaming_lines(&self) -> usize {
        if self.streaming.is_some() {
            self.streaming_render
                .as_ref()
                .map_or(0, |memo| memo.lines.len())
        } else {
            0
        }
    }

    /// Total visible physical lines (committed rows + streaming preview) at
    /// `width`, refreshing the wrap cache. O(dirty suffix), O(1) once warm --
    /// the pager calls this every frame regardless of transcript length.
    pub(super) fn visible_total(&mut self, width: u16) -> usize {
        let width = usize::from(width);
        self.last_width = width
            .saturating_sub(TEXT_COLUMN_X_PADDING.saturating_mul(2))
            .max(1);
        self.ensure_wrapped_cache(width);
        self.refresh_streaming_memo(width);
        let committed = self
            .wrapped_cache
            .rows
            .last()
            .map_or(0, |layout| layout.visible_cum);
        committed + self.streaming_lines()
    }

    /// Clone exactly the visible physical lines `[top .. top+rows)` out of the
    /// wrap cache (plus the streaming preview when the window reaches it).
    /// Callers must have the window clamped against [`Self::visible_total`]
    /// for the same width. Cost: O(rows + log transcript), never O(transcript)
    /// -- this is the pager's visible-range-only render (ADR-0029).
    pub(super) fn render_window(
        &mut self,
        width: u16,
        top: usize,
        rows: usize,
    ) -> Vec<Line<'static>> {
        let total = self.visible_total(width);
        let committed = total - self.streaming_lines();
        let end = (top + rows).min(total);
        if top >= end {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(end - top);
        if top < committed {
            // First cached row whose cumulative visible count exceeds `top`.
            let mut idx = self
                .wrapped_cache
                .rows
                .partition_point(|layout| layout.visible_cum <= top);
            let mut pos = if idx == 0 {
                0
            } else {
                self.wrapped_cache.rows[idx - 1].visible_cum
            };
            'rows: while idx < self.wrapped_cache.rows.len() {
                let layout = &self.wrapped_cache.rows[idx];
                if layout.visible {
                    for line in &self.wrapped_cache.lines[layout.lines.clone()] {
                        if pos >= end {
                            break 'rows;
                        }
                        if pos >= top {
                            out.push(line.clone());
                        }
                        pos += 1;
                    }
                }
                idx += 1;
            }
        }
        if end > committed
            && let Some(memo) = self.streaming_render.as_ref()
        {
            let from = top.max(committed) - committed;
            let until = end - committed;
            out.extend(memo.lines[from..until].iter().cloned());
        }
        out
    }

    pub(super) fn render_incremental(&mut self, width: u16) -> TranscriptRender {
        self.render_cached(width, true)
    }

    fn render_cached(&mut self, width: u16, suffix_only: bool) -> TranscriptRender {
        let width = usize::from(width);
        self.last_width = width
            .saturating_sub(TEXT_COLUMN_X_PADDING.saturating_mul(2))
            .max(1);
        let width_changed = self.wrapped_cache.width != width;
        if width_changed {
            self.wrapped_cache.invalidate_all(width);
        }
        let dirty_from = if width_changed {
            0
        } else {
            self.wrapped_cache.dirty_from.min(self.rows.len())
        };
        self.ensure_wrapped_cache(width);

        // Emit the cached physical lines of visible rows. Fold visibility and
        // the cumulative visible-line counts were resolved at wrap time, so a
        // frame touches only the dirty suffix: the stable prefix is an O(1)
        // lookup, never a whole-transcript scan. Production uses `suffix_only`
        // so the stable prefix is counted, not cloned into the frame; tests use
        // full output for direct render assertions.
        let start_row = if suffix_only { dirty_from } else { 0 };
        let stable_prefix = if suffix_only && start_row > 0 {
            self.wrapped_cache.rows[start_row - 1].visible_cum
        } else {
            0
        };
        let suffix_lines: usize = self.wrapped_cache.rows[start_row..]
            .iter()
            .filter(|layout| layout.visible)
            .map(|layout| layout.lines.len())
            .sum();
        let mut out = Vec::with_capacity(suffix_lines);
        for layout in &self.wrapped_cache.rows[start_row..] {
            if layout.visible {
                out.extend(
                    self.wrapped_cache.lines[layout.lines.clone()]
                        .iter()
                        .cloned(),
                );
            }
        }
        // The in-flight stream renders as transient rows appended after history;
        // keep it out of the committed cache so spinner/streaming frames never
        // mutate retained row ranges. Its wrapped lines are memoized on
        // `(len, width)` so only frames where the stream actually grew pay the
        // markdown re-parse.
        self.refresh_streaming_memo(width);
        if self.streaming.is_some()
            && let Some(memo) = self.streaming_render.as_ref()
        {
            out.extend(memo.lines.iter().cloned());
        }
        let total_lines = stable_prefix + out.len();
        TranscriptRender {
            lines: out,
            stable_prefix,
            total_lines,
        }
    }
}

pub(super) fn streaming_markdown_preview(text: &str) -> String {
    if text.len() <= MAX_STREAMING_MARKDOWN_BYTES {
        return text.to_string();
    }
    let start = text.ceil_char_boundary(text.len() - MAX_STREAMING_MARKDOWN_BYTES);
    format!(
        "… streaming preview truncated; showing latest content …\n{}",
        &text[start..]
    )
}

/// The APPROVAL body line: the authorized action (with a `$ ` prompt for shell
/// commands) plus a muted `┊ approved <scope>` reason. The decision itself is
/// carried by the header (`◆ APPROVED`); the body never repeats a state glyph.
fn approval_line(call: &ToolCall, scope: &str) -> Line<'static> {
    let mut spans = Vec::new();
    if call.name == "bash" {
        spans.push(Span::styled("$ ", dim_style()));
    }
    spans.extend(ansi_spans(&run_target(call), Style::default()));
    spans.push(Span::styled(
        format!("  {} approved {scope}", crate::ui::symbols::SEP),
        dim_style(),
    ));
    Line::from(spans)
}
