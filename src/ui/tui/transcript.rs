//! Retained transcript state and event-to-row rendering.

use std::ops::{Deref, Range};
use std::time::{Duration, Instant};

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::nexus::{ApprovalDecision, ProviderUsage, ToolCall};
use crate::ui::markdown::{MarkdownTheme, render_markdown_themed};
use crate::ui::{TurnErrorKind, UiEvent};

use super::pane;
use super::panel::{
    FooterField, PanelHeaderSpec, PanelState, ToolDiag, diff_counts, diff_table_rows,
    edit_footer_extras, join_meta_fields, panel_state, review_footer_extras,
};
use super::rows::{ChromeRow, FoldVis, TranscriptRow, is_separator_row};
use super::streaming::StreamController;
use super::tool_render::{self, RenderCtx, ToolOutcome, ToolPanelKind};
use super::wrap::line_text;
use super::{
    MAX_EXEC_STREAM_BYTES, MAX_TRANSCRIPT_ROWS, TEXT_COLUMN_X_PADDING, dim_style, err_style,
    format_elapsed_compact, tool_header_style, turn_divider_label,
};

/// Reasoning-rail label. Uppercase like the other structural labels; the rail
/// (`┊`) and the `▾`/`▸` disclosure arrow carry the fold affordance, so no
/// `Thinking...` ellipsis is needed (ThinkingBlock design-system component).
const THINKING_LABEL: &str = "THINKING";
/// Placeholder for reasoning the provider withheld; the original text is never
/// available and is never rendered.
const REDACTED_THINKING_BODY: &str = "[reasoning withheld by provider]";

/// The always-visible footer row of a frameless block: the state label as the
/// row's actor (no `┊` between it and what follows — a 2-space layout gap
/// separates), then `┊`-joined family extras, then the right-bound dim
/// diagnostics cluster.
fn block_footer_row(
    state: PanelState,
    extras: Vec<FooterField>,
    diag: Option<&ToolDiag>,
    diag_call: Option<&str>,
) -> TranscriptRow {
    let (extra_spans, extra_plain) = join_meta_fields(extras);
    // The footer state token: the colored state glyph, then the label. The glyph
    // is the at-a-glance semantic mark (green `◆` / red `■` / orange `▲`); the
    // label's weight is proportional — bold for the consequential states, muted
    // for settled-success and transient ones (see `PanelState::label_style`).
    let glyph = state.glyph();
    let label = state.label();
    let mut spans = vec![
        Span::styled(format!("{glyph} "), state.glyph_style()),
        Span::styled(label.to_string(), state.label_style()),
    ];
    let mut plain = format!("{glyph} {label}");
    if !extra_plain.is_empty() {
        spans.push(Span::raw("  "));
        spans.extend(extra_spans);
        plain.push_str("  ");
        plain.push_str(&extra_plain);
    }
    let right = diag.and_then(ToolDiag::render).unwrap_or_default();
    if !right.is_empty() {
        plain.push_str("  ");
        plain.push_str(&right);
    }
    TranscriptRow::chrome_with_text(
        ChromeRow::Footer {
            left: Line::from(spans),
            right,
            diag_call: diag_call.map(str::to_string),
        },
        plain,
        state.label_style(),
    )
}

/// One reasoning-trace row on the muted left rail: the dim `┊ ` rail prefix plus
/// the line, word-wrapped with the rail carried onto continuation rows. A plain
/// (chromeless) row — reasoning gets no box.
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
        searchable: true,
    }
}

/// Build the transient thinking-preview rows for live reasoning text: the
/// `THINKING` rail header plus dim rail-body lines of the rendered markdown.
/// Shown expanded (`▾ THINKING`) and whole while it streams — a live, growing
/// trace shows everything, exactly as a running tool block stays open on its
/// tail — then replaced by the committed block, which arrives collapsed, once
/// the trace finalizes. The markdown is rendered at the same `content_width` the
/// committed block uses, so finalizing never reflows the already-shown lines.
fn live_reasoning_preview_rows(text: &str, content_width: usize) -> Vec<TranscriptRow> {
    let theme = MarkdownTheme::thinking()
        .with_code_highlighting()
        .with_hyperlinks();
    let mut rows = Vec::new();
    rows.push(TranscriptRow::chrome(ChromeRow::RailHeader {
        expanded: true,
        label: THINKING_LABEL.to_string(),
        right: String::new(),
        foldable: true,
    }));
    for line in render_markdown_themed(text, &theme, content_width) {
        rows.push(rail_body_row(line));
    }
    rows.push(TranscriptRow::chrome(ChromeRow::RailEnd));
    rows
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
    /// Explicit user fold intent (ctrl+o/selection/click), preserved across
    /// in-place rebuilds so a finalized block honors the user, not its default.
    user_expanded: Option<bool>,
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
    /// Explicit user fold intent; see [`ActiveExec::user_expanded`].
    user_expanded: Option<bool>,
}

/// The offered choices for a pending in-block approval — mirrors the loop's
/// `ApprovalRequest` so the `▲ REVIEW` footer only shows keys the loop honors.
struct ReviewGate {
    allow_always: bool,
    allow_project: bool,
    dirty_gate: bool,
    reason: Option<String>,
}

/// The open EDIT panel for a mutation whose diff arrived via `DiffPreview`.
/// The diff is the canonical EDIT body for the whole lifecycle: the same panel
/// is rebuilt in place as `◇ PREVIEW` → `● RUNNING` → `◆ DONE`/`■ ERROR`.
struct ActiveEdit {
    call_id: String,
    diff: String,
    body_start: usize,
    started: Option<Instant>,
    /// Explicit user fold intent; see [`ActiveExec::user_expanded`].
    user_expanded: Option<bool>,
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

/// One `/find` match in the canonical transcript content, located by the
/// logical row that owns it (`row`) and the physical sub-line offset within
/// that row's wrapped lines (`sub`). Row-relative rather than an absolute
/// visible-line index because expanding a fold to reveal the match shifts
/// every visible line after it -- the visible index is resolved only after
/// the reveal, via [`Transcript::reveal_and_locate`].
pub(super) struct SearchMatch {
    pub(super) row: usize,
    pub(super) sub: usize,
}

#[derive(Clone)]
struct UserPromptAnchor {
    row: usize,
    text: String,
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

/// Memoized wrapped physical lines of the stream controller's mutable active
/// tail. Keyed by the controller's `(buffered_len, emitted)` tail signature and
/// the frame width, so spinner ticks and typing-while-streaming frames only
/// re-wrap the tail when it actually changed.
struct StreamingRender {
    key: (usize, usize),
    width: usize,
    lines: Vec<Line<'static>>,
    /// Whether these lines are the live reasoning preview (vs the
    /// assistant-answer active tail). The two sources are mutually exclusive per
    /// frame; the flag keeps a source switch from reusing a stale memo.
    is_reasoning: bool,
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
    /// Live assistant-message stream: newline-gated collection, block-safe
    /// incremental commit to scrollback, and a single mutable active tail
    /// (issue #87). Committed rows land in `rows`; the tail renders after them.
    pub(super) stream: StreamController,
    /// Row index where the current turn's streamed answer block begins (after
    /// its opening separator). A late `AssistantReasoning` block is spliced here
    /// so reasoning renders above an already-committed answer.
    stream_answer_start: Option<usize>,
    /// Accumulated live reasoning summary for the current turn, shown as a
    /// transient thinking preview while the provider is still thinking and
    /// before the answer streams. Committed as the collapsed thinking body by
    /// [`Self::finish_live_reasoning_if_any`] on the first non-reasoning event.
    /// `None` when no reasoning summary is streaming.
    live_reasoning_summary: Option<String>,
    /// Accumulated live raw reasoning for the current turn. Committed as the
    /// expanded thinking body; never used as the collapsed body while a summary
    /// is available.
    live_reasoning_raw: Option<String>,
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
    /// Memoized wrapped lines for the current active-tail preview; see
    /// [`StreamingRender`].
    streaming_render: Option<StreamingRender>,
    /// Row indices and raw text for committed user prompts, maintained
    /// incrementally so the pager's sticky prompt card is an O(prompts-above-view)
    /// lookup, never a row scan.
    user_prompts: Vec<UserPromptAnchor>,
    /// Per-tool-call token diagnostics (`↑sent ↓received ┊ cache ┊ ctx`),
    /// keyed by call id and rendered right-bound in the block footer. Forward
    /// attribution: `↓received` is stamped from the proposing turn; `↑sent`,
    /// `cache`, and `ctx` are patched in later from the following turn that
    /// ingests the tool results. Numbers are honest: fields exist only when the
    /// runtime measured them.
    tool_diags: std::collections::HashMap<String, ToolDiag>,
    /// Per-call review affordance, set while a gated call is `▲ REVIEW` so its
    /// footer can render `y approve ┊ n deny ┊ …`; dropped once it runs or is
    /// refused. Approval lives inside the tool block — there is no separate
    /// approval panel.
    review_gates: std::collections::HashMap<String, ReviewGate>,
    /// Per-call decision note (`approved this time/session/project`) for a
    /// *manually* approved call — appended as a muted footer field on the
    /// running/done rebuilds. Auto-approved calls carry none.
    approval_notes: std::collections::HashMap<String, &'static str>,
    /// The proposing turn's `↓received` diagnostic (output tokens it generated),
    /// stamped onto every tool call that turn proposes. Holds only the output
    /// side; the input side (`↑/cache/ctx`) comes from the following turn.
    /// `None` until the turn reports usage; reset each turn start.
    current_turn_diag: Option<ToolDiag>,
    /// Call ids proposed by the current proposing turn, awaiting the following
    /// turn's input-side numbers (`↑/cache/ctx`). Drained and patched onto the
    /// rendered footers when the next `ProviderTurnCompleted` usage arrives.
    awaiting_input_calls: Vec<String>,
    /// `input_tokens` of the previous completed provider turn, so the following
    /// turn's `ctx` field can report signed context growth against it. `None`
    /// before the first completed turn (no prior turn to diff against).
    last_turn_input_tokens: Option<u64>,
    /// Context-window cap in tokens (same source as the session-bar meter),
    /// used to scale the `ctx` growth delta into a percentage. `None` when the
    /// model's window is unknown, in which case `ctx` is omitted.
    context_cap: Option<u64>,
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

    fn reasoning_groups(&self, text: &str) -> Vec<Vec<Line<'static>>> {
        let theme = MarkdownTheme::thinking()
            .with_code_highlighting()
            .with_hyperlinks();
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
    }

    /// Render a model reasoning ("thinking") trace as a chromeless, foldable
    /// left-rail block (the `ThinkingBlock` design-system component): collapsed
    /// state shows the provider summary; expanded state reveals raw reasoning
    /// when available. Reasoning gets **no box** — only a muted `┊` rail on its
    /// body and a `THINKING` label with honest telemetry.
    fn push_thinking_block(&mut self, summary: &str, raw: Option<&str>, redacted: bool) {
        // Reasoning is emitted at completion, after the answer's text deltas.
        // Some of the answer may already have been paced into scrollback, so the
        // rail block is spliced ABOVE the current turn's answer rows (tracked by
        // `stream_answer_start`) rather than appended; that keeps reasoning above
        // the answer without finishing or double-committing the live stream.
        let elapsed = self
            .provider_turn_started
            .map(|started| format_elapsed_compact(started.elapsed()));
        let collapsed_text = if summary.trim().is_empty() {
            raw.unwrap_or_default()
        } else {
            summary
        };
        let expanded_text = raw
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(collapsed_text);
        let has_distinct_expanded_body = !redacted && expanded_text.trim() != collapsed_text.trim();
        let collapsed_groups: Vec<Vec<Line<'static>>> = if redacted {
            vec![vec![Line::from(Span::styled(
                REDACTED_THINKING_BODY,
                dim_style(),
            ))]]
        } else {
            self.reasoning_groups(collapsed_text)
        };
        let expanded_groups: Vec<Vec<Line<'static>>> = if redacted {
            vec![vec![Line::from(Span::styled(
                REDACTED_THINKING_BODY,
                dim_style(),
            ))]]
        } else {
            self.reasoning_groups(expanded_text)
        };
        self.thinking_elapsed = elapsed.clone();
        // Build the rail rows into a detached block; the header is always at
        // local index 0.
        let mut block: Vec<TranscriptRow> = Vec::new();
        block.push(TranscriptRow::chrome(ChromeRow::RailHeader {
            expanded: false,
            label: THINKING_LABEL.to_string(),
            right: elapsed.unwrap_or_default(),
            foldable: has_distinct_expanded_body,
        }));
        for (index, group) in collapsed_groups.into_iter().enumerate() {
            if index > 0 {
                block.push(rail_body_row(Line::default()).with_fold(
                    if has_distinct_expanded_body {
                        FoldVis::WhenCollapsed
                    } else {
                        FoldVis::Always
                    },
                ));
            }
            for line in group {
                block.push(
                    rail_body_row(line).with_fold(if has_distinct_expanded_body {
                        FoldVis::WhenCollapsed
                    } else {
                        FoldVis::Always
                    }),
                );
            }
        }
        if has_distinct_expanded_body {
            for (index, group) in expanded_groups.into_iter().enumerate() {
                if index > 0 {
                    block.push(rail_body_row(Line::default()).with_fold(FoldVis::WhenExpanded));
                }
                for line in group {
                    block.push(rail_body_row(line).with_fold(FoldVis::WhenExpanded));
                }
            }
        }
        block.push(TranscriptRow::chrome(ChromeRow::RailEnd));

        match self.stream_answer_start {
            Some(start) => {
                // Splice above the (possibly already-committed) answer, with a
                // trailing blank separating the rail from the answer below.
                block.push(TranscriptRow::new(String::new(), Style::default()));
                let n = block.len();
                // Keep every retained row-index anchor at or after the insert
                // point valid (pager prompt anchors, any open-panel body rows).
                self.shift_row_anchors(start, n);
                self.thinking_header_row = Some(start);
                self.mark_dirty_from(start);
                self.rows.splice(start..start, block);
                self.stream_answer_start = Some(start + n);
            }
            None => {
                self.push_blank();
                self.mark_append_dirty();
                self.thinking_header_row = Some(self.rows.len());
                self.rows.extend(block);
            }
        }
    }

    /// Append one live reasoning-summary delta to the transient thinking preview
    /// and collapsed thinking body.
    /// Display-only: never committed to `rows` until finalize, never stored.
    fn push_reasoning_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.live_reasoning_summary
            .get_or_insert_with(String::new)
            .push_str(delta);
        // The preview is re-derived from live reasoning; drop the memo so the
        // next frame re-renders with the appended text.
        self.streaming_render = None;
    }

    /// Append one live raw reasoning delta to the expanded thinking body.
    fn push_raw_reasoning_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        self.live_reasoning_raw
            .get_or_insert_with(String::new)
            .push_str(delta);
        self.streaming_render = None;
    }

    /// Insert a blank line between two reasoning-summary parts. No-op before any
    /// reasoning has streamed or when the buffer already ends with a break.
    fn push_reasoning_section_break(&mut self) {
        let append = self
            .live_reasoning_summary
            .as_deref()
            .is_some_and(|buf| !buf.is_empty() && !buf.ends_with("\n\n"));
        if append {
            self.live_reasoning_summary
                .as_mut()
                .expect("checked non-empty above")
                .push_str("\n\n");
            self.streaming_render = None;
        }
    }

    /// Commit any live reasoning trace to scrollback as a thinking block. Called
    /// before handling any non-reasoning event, so the reasoning ends up above
    /// the answer that streams afterwards. Idempotent: a no-op when nothing
    /// streamed. `stream_answer_start` is `None` here (reasoning precedes the
    /// answer), so `push_thinking_block` appends rather than splices.
    fn finish_live_reasoning_if_any(&mut self) {
        let summary = self.live_reasoning_summary.take().unwrap_or_default();
        let raw = self.live_reasoning_raw.take().unwrap_or_default();
        if summary.is_empty() && raw.is_empty() {
            return;
        };
        // Drop the transient reasoning-preview memo; the block below is committed
        // through the wrap cache instead.
        self.streaming_render = None;
        let summary = summary.trim();
        let raw = raw.trim();
        if !summary.is_empty() || !raw.is_empty() {
            self.push_thinking_block(summary, (!raw.is_empty()).then_some(raw), false);
        }
    }

    /// Shift every retained row-index anchor at or after `at` by `delta`, for a
    /// mid-array row insert (the reasoning splice above a committed answer).
    /// Keeps pager prompt anchors and any open-panel body-row indices valid.
    /// Open tool/exec/edit panels do not normally overlap an active assistant
    /// stream (tools run after the turn's text), but shifting them is correct
    /// and cheap, and makes the insert self-consistent.
    fn shift_row_anchors(&mut self, at: usize, delta: usize) {
        for prompt in &mut self.user_prompts {
            if prompt.row >= at {
                prompt.row += delta;
            }
        }
        if let Some(exec) = self.active_exec.as_mut()
            && exec.body_start >= at
        {
            exec.body_start += delta;
        }
        if let Some(tool) = self.active_tool.as_mut()
            && tool.body_start >= at
        {
            tool.body_start += delta;
        }
        if let Some(edit) = self.active_edit.as_mut()
            && edit.body_start >= at
        {
            edit.body_start += delta;
        }
        for expl in &mut self.active_explorations {
            if expl.row >= at {
                expl.row += delta;
            }
        }
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
    fn push_notice_row(&mut self, glyph: &str, glyph_style: Style, message: &str) {
        // Coalesce a run of notices onto one rail: the first notice of a run
        // opens a block (one blank separator above); each subsequent notice
        // reclaims the previous notice's trailing blank so siblings sit directly
        // under it, sharing the `┊` rail with no interior gap — one quiet system
        // aside, not scattered ticks. A trailing blank always closes the run.
        let continues_run = self.rows.len() >= 2
            && is_separator_row(&self.rows[self.rows.len() - 1])
            && matches!(
                self.rows[self.rows.len() - 2].chrome,
                Some(ChromeRow::Notice { .. })
            );
        if continues_run {
            self.rows.pop();
            self.mark_append_dirty();
        } else {
            self.begin_block();
            self.mark_append_dirty();
        }
        let left = format!("{glyph} {message}");
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Notice {
                glyph: glyph.to_string(),
                glyph_style,
                message: message.to_string(),
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
            searchable: true,
        });
        self.push_blank();
    }

    /// Commit any in-flight streamed assistant text into the transcript. The
    /// controller renders the complete accumulated source once (so a streamed
    /// markdown table never reflows on finalize) and returns only the rendered
    /// lines not already paced into scrollback.
    fn finish_stream(&mut self) {
        self.streaming_render = None;
        if self.stream.is_active() {
            let width = self.markdown_content_width();
            let rows = self.stream.finalize(width);
            if !rows.is_empty() {
                self.mark_append_dirty();
                self.rows.extend(rows);
                self.push_blank();
            }
        }
        self.stream_answer_start = None;
    }

    /// Drive one paced commit tick: move any newly-stable streamed lines into
    /// scrollback. Returns `true` when rows were committed (a redraw is due).
    /// Called from the render loop's tick while a turn runs.
    pub(super) fn commit_stream_tick(&mut self, now: Instant) -> bool {
        if !self.stream.is_active() {
            return false;
        }
        let width = self.markdown_content_width();
        let rows = self.stream.commit_tick(now, width);
        if rows.is_empty() {
            return false;
        }
        self.mark_append_dirty();
        self.rows.extend(rows);
        true
    }

    /// Whether the stream has not-yet-committed content, so the loop should keep
    /// driving commit ticks.
    pub(super) fn has_stream_work(&self) -> bool {
        self.stream.has_work()
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

    /// Push a complete frameless tool block (header · hanging body · hairline
    /// footer) to `self.rows`, with the body produced by the renderer registry
    /// under failure isolation. Collapse is binary: the whole body folds
    /// behind the header's disclosure and the footer stays. Compact by
    /// default: every block arrives COLLAPSED (header + footer answer what
    /// ran · outcome · cost) except a RUNNING block, which stays expanded so
    /// its live tail is watchable and collapses on finalize unless the user
    /// explicitly expanded it. This is the single dispatch path for
    /// SHELL/EDIT/generic blocks; EXPLORE keeps its grouped path.
    fn append_tool_panel(
        &mut self,
        renderer: &dyn tool_render::ToolRenderer,
        call: &ToolCall,
        state: PanelState,
        duration: Option<std::time::Duration>,
        started: Option<Instant>,
        outcome: ToolOutcome<'_>,
    ) {
        let ctx = self.render_ctx();
        let mut body = tool_render::render_body(renderer, &ctx, call, &outcome);
        for row in &mut body {
            row.fold = FoldVis::WhenExpanded;
        }
        // Compact by default: a RUNNING block (live tail) and a pending REVIEW
        // block (the user must see what they are authorizing) arrive expanded.
        // Finalized blocks arrive collapsed; an explicit user expand is
        // reapplied by the rebuild path.
        let expanded = matches!(outcome, ToolOutcome::Running { .. } | ToolOutcome::Review);
        self.push_tool_header(renderer, call, state, duration, started, expanded);
        self.rows.extend(body);
        let mut extras = renderer.footer_extras(call, &outcome);
        extras.extend(self.approval_footer_fields(&call.id, state));
        let diag = self.tool_diags.get(&call.id).cloned();
        self.push_block_footer(state, extras, diag.as_ref(), Some(&call.id));
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

    /// Build the standard block header from the renderer's title/meta plus the
    /// transcript-owned lifecycle state/duration.
    fn push_tool_header(
        &mut self,
        renderer: &dyn tool_render::ToolRenderer,
        call: &ToolCall,
        state: PanelState,
        duration: Option<std::time::Duration>,
        started: Option<Instant>,
        expanded: bool,
    ) {
        let meta = renderer.header_meta(call);
        let plain = renderer.plain_meta(call);
        self.push_panel_header_with_expanded(
            PanelHeaderSpec {
                title: renderer.title(),
                meta: &meta,
                plain_meta: &plain,
                state,
                duration,
                started,
            },
            expanded,
        );
    }

    /// Push the always-visible block footer: the hairline rule, the footer row
    /// (state label · `┊`-joined extras · right-bound diagnostics), and the
    /// end-of-block marker. The footer never folds.
    fn push_block_footer(
        &mut self,
        state: PanelState,
        extras: Vec<FooterField>,
        diag: Option<&ToolDiag>,
        diag_call: Option<&str>,
    ) {
        self.rows.push(TranscriptRow::chrome(ChromeRow::FooterRule));
        self.rows
            .push(block_footer_row(state, extras, diag, diag_call));
        self.rows.push(TranscriptRow::chrome(ChromeRow::BlockEnd));
    }

    /// Fold the approval decision into a tool block's own footer (approval lives
    /// in the tool block, never a separate panel): the `y approve ┊ …` affordance
    /// while the block is `▲ REVIEW`, or the muted `approved this time/session/
    /// project` note once the user has manually allowed it. Auto-approved calls
    /// carry neither — the tool block alone is the record.
    fn approval_footer_fields(&self, call_id: &str, state: PanelState) -> Vec<FooterField> {
        if let Some(note) = self.approval_notes.get(call_id) {
            return vec![FooterField::styled(*note, dim_style())];
        }
        if state == PanelState::Review
            && let Some(gate) = self.review_gates.get(call_id)
        {
            // The safety caution (danger-toned) leads, then the affordance.
            let mut fields = Vec::new();
            if let Some(reason) = &gate.reason {
                fields.push(FooterField::styled(reason.clone(), err_style()));
            }
            fields.extend(review_footer_extras(
                gate.allow_always,
                gate.allow_project,
                gate.dirty_gate,
            ));
            return fields;
        }
        Vec::new()
    }

    /// Record per-call token diagnostics for a tool call's footer. The values
    /// are preformatted, runtime-measured strings; blocks rendered after this
    /// call carry them right-bound in the footer. Bounded: a finalized footer
    /// bakes its own `right` string, so the map is only consulted at build
    /// time; clearing it never disturbs a rendered footer. A single very long
    /// session cannot grow it without bound.
    pub(super) fn set_tool_diag(&mut self, call_id: &str, diag: ToolDiag) {
        if self.tool_diags.len() >= 1024 {
            self.tool_diags.clear();
        }
        self.tool_diags.insert(call_id.to_string(), diag);
    }

    /// Set the context-window cap (tokens) used to scale the `ctx` growth
    /// delta, mirroring the session-bar meter's source.
    pub(super) fn set_context_cap(&mut self, cap: Option<u64>) {
        self.context_cap = cap;
    }

    /// The proposing turn's `↓received` diagnostic: the output tokens it
    /// generated. This is the only field known when the tool footer is first
    /// rendered; the input side is patched in later by the following turn.
    fn proposing_turn_diag(usage: &ProviderUsage) -> ToolDiag {
        ToolDiag {
            received: Some(super::screen::compact_count(usage.output_tokens)),
            ..ToolDiag::default()
        }
    }

    /// Patch the input-side diagnostics (`↑/cache/ctx`) onto every tool call
    /// awaiting them, from the following turn's usage that ingested the tool
    /// results: `↑` fresh (non-cached) input it processed, `cache` its
    /// prompt-cache reads (omitted when zero), and the signed `ctx` growth
    /// against the proposing turn (omitted without a prior turn or cap).
    /// `ProviderUsage` docs state cache reads are already counted in
    /// `input_tokens`, so fresh input subtracts them out. Parallel/same-turn
    /// tool calls share these numbers (the runtime cannot split finer). Must be
    /// called before `last_turn_input_tokens` is advanced so `ctx` diffs against
    /// the proposing turn. The stamped `↓received` is preserved.
    fn apply_following_turn_usage(&mut self, usage: &ProviderUsage) {
        let fresh_input = usage
            .input_tokens
            .saturating_sub(usage.cache_read_input_tokens);
        let sent = Some(super::screen::compact_count(fresh_input));
        let cache = (usage.cache_read_input_tokens > 0)
            .then(|| super::screen::compact_count(usage.cache_read_input_tokens));
        let ctx = self.context_growth_pct(usage.input_tokens);
        let calls: Vec<String> = std::mem::take(&mut self.awaiting_input_calls);
        for id in &calls {
            if let Some(diag) = self.tool_diags.get_mut(id) {
                diag.sent = sent.clone();
                diag.cache = cache.clone();
                diag.ctx = ctx.clone();
            }
            self.patch_footer_diag(id);
        }
    }

    /// Rewrite the already-rendered footer row tagged with `call_id` to reflect
    /// its current `tool_diags` entry (forward-attribution patch). Both the
    /// baked `right` diagnostics string and the plain `text` mirror are updated;
    /// a no-op if the footer was trimmed away or never rendered.
    fn patch_footer_diag(&mut self, call_id: &str) {
        let Some(diag) = self.tool_diags.get(call_id).cloned() else {
            return;
        };
        let right = diag.render().unwrap_or_default();
        let Some(idx) = self.rows.iter().position(|row| {
            matches!(
                row.chrome.as_ref(),
                Some(ChromeRow::Footer { diag_call: Some(id), .. }) if id == call_id
            )
        }) else {
            return;
        };
        let Some(ChromeRow::Footer { left, .. }) = self.rows[idx].chrome.as_ref() else {
            return;
        };
        let left_plain = line_text(left);
        let text = if right.is_empty() {
            left_plain
        } else {
            format!("{left_plain}  {right}")
        };
        self.mark_dirty_from(idx);
        self.rows[idx].text = text;
        if let Some(ChromeRow::Footer { right: baked, .. }) = self.rows[idx].chrome.as_mut() {
            *baked = right;
        }
    }

    /// Signed context-growth percentage: the following turn's `input_tokens`
    /// minus the proposing turn's, as a fraction of the context cap (`"+0.9%"`).
    /// `None` without a prior turn or a known cap — never a fabricated delta.
    fn context_growth_pct(&self, input_tokens: u64) -> Option<String> {
        let prev = self.last_turn_input_tokens?;
        let cap = self.context_cap.filter(|&cap| cap > 0)?;
        let delta = input_tokens as i64 - prev as i64;
        let pct = delta as f64 / cap as f64 * 100.0;
        Some(format!("{pct:+.1}%"))
    }

    /// Call ids of every in-flight tool block, so the tools-first event order
    /// (a `ToolStarted` that precedes its turn's `ProviderTurnCompleted`) still
    /// stamps the diagnostics onto the already-open footers.
    fn active_call_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        if let Some(active) = &self.active_exec {
            ids.push(active.call.id.clone());
        }
        if let Some(active) = &self.active_tool {
            ids.push(active.call.id.clone());
        }
        if let Some(active) = &self.active_edit {
            ids.push(active.call_id.clone());
        }
        ids.extend(self.active_explorations.iter().map(|a| a.call_id.clone()));
        ids
    }

    /// Stamp the proposing turn's `↓received` diagnostic onto a newly-known
    /// call and record it as awaiting the following turn's input-side numbers,
    /// if the turn reported usage.
    fn assign_turn_diag(&mut self, call_id: &str) {
        if let Some(diag) = self.current_turn_diag.clone() {
            self.set_tool_diag(call_id, diag);
            self.mark_awaiting_input(call_id);
        }
    }

    /// Record a proposing-turn tool call as awaiting the following turn's
    /// input-side diagnostics (`↑/cache/ctx`). Deduplicated: `DiffPreview`,
    /// `ToolStarted`, and the turn-completion stamp can all name the same call.
    fn mark_awaiting_input(&mut self, call_id: &str) {
        if !self.awaiting_input_calls.iter().any(|id| id == call_id) {
            self.awaiting_input_calls.push(call_id.to_string());
        }
    }

    fn push_panel_header_with_expanded(&mut self, spec: PanelHeaderSpec<'_>, expanded: bool) {
        self.mark_append_dirty();
        // A pending preview/review or a refused call has no elapsed time by
        // definition (the duration is omitted; asserting one would fabricate a
        // measurement).
        let elapsed = if matches!(
            spec.state,
            PanelState::Preview | PanelState::Review | PanelState::Denied
        ) {
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
        self.rows.push(TranscriptRow::chrome(ChromeRow::BlockStart));
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Header {
                expanded,
                title: spec.title,
                meta: spec.meta.to_string(),
                elapsed,
            },
            plain,
            tool_header_style(),
        ));
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
            user_expanded: None,
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
            user_expanded: None,
        });
    }

    /// Open a pending in-block review for a gated call: the tool block itself
    /// renders `▲ REVIEW` with the affordance on its footer, so the whole
    /// approval lifecycle lives inside the tool block (no separate panel). The
    /// block is adopted by `ToolStarted` (→ RUNNING) on approve, or flipped to
    /// `DENIED` in place on deny.
    fn begin_review(&mut self, call: ToolCall, gate: ReviewGate) {
        self.review_gates.insert(call.id.clone(), gate);
        match tool_render::resolve(&call).kind() {
            ToolPanelKind::Shell => self.begin_exec_review(call),
            ToolPanelKind::Generic => {
                // A mutation whose diff already arrived (DiffPreview) keeps that
                // block: `◇ PREVIEW` flips to `▲ REVIEW` in place — the diff IS
                // the review body, never a second block.
                if !self.rebuild_active_edit(&call, PanelState::Review, None, None, true) {
                    self.begin_tool_review(call);
                }
            }
            // Read-side tools are auto-approved in practice; if one is ever
            // gated a generic review block still surfaces the affordance.
            ToolPanelKind::Explore => self.begin_tool_review(call),
        }
    }

    fn begin_exec_review(&mut self, call: ToolCall) {
        self.begin_block();
        let body_start = self.rows.len();
        let started = Instant::now();
        let renderer = tool_render::resolve(&call);
        self.append_tool_panel(
            renderer,
            &call,
            PanelState::Review,
            None,
            Some(started),
            ToolOutcome::Review,
        );
        self.active_exec = Some(ActiveExec {
            call,
            output: String::new(),
            body_start,
            started,
            user_expanded: None,
        });
    }

    fn begin_tool_review(&mut self, call: ToolCall) {
        self.begin_block();
        let body_start = self.rows.len();
        let started = Instant::now();
        let renderer = tool_render::resolve(&call);
        self.append_tool_panel(
            renderer,
            &call,
            PanelState::Review,
            None,
            Some(started),
            ToolOutcome::Review,
        );
        self.active_tool = Some(ActiveTool {
            call,
            body_start,
            started,
            user_expanded: None,
        });
    }

    /// Record a *manual* approval on the tool block's own footer (the muted
    /// `approved this time/session/project` note) and drop the affordance in
    /// place — no separate approval panel. The block stays `▲ REVIEW` only until
    /// the imminent `ToolStarted` flips it to RUNNING.
    pub(super) fn note_approval(&mut self, call: &ToolCall, decision: ApprovalDecision) {
        let note = match decision {
            ApprovalDecision::Allow => "approved this time",
            ApprovalDecision::AllowAlways => "approved this session",
            ApprovalDecision::AllowProject => "approved this project",
            // Denial has no note; it flows through the `ToolDenied` event.
            ApprovalDecision::Deny => return,
        };
        self.approval_notes.insert(call.id.clone(), note);
        self.review_gates.remove(&call.id);
        self.rerender_active_review(call);
    }

    /// Re-render the active REVIEW block in place (staying `▲ REVIEW`) so a
    /// just-set approval note replaces the affordance without waiting for the
    /// next lifecycle event.
    fn rerender_active_review(&mut self, call: &ToolCall) {
        if self
            .active_exec
            .as_ref()
            .is_some_and(|a| a.call.id == call.id)
        {
            let Some(active) = self.active_exec.take() else {
                return;
            };
            let rows = self.collect_tool_panel(
                tool_render::resolve(&active.call),
                &active.call,
                PanelState::Review,
                None,
                Some(active.started),
                ToolOutcome::Review,
            );
            self.replace_active_exec_panel(&active, rows);
            self.active_exec = Some(active);
        } else if self
            .active_edit
            .as_ref()
            .is_some_and(|a| a.call_id == call.id)
        {
            self.rebuild_active_edit(call, PanelState::Review, None, None, true);
        } else if self
            .active_tool
            .as_ref()
            .is_some_and(|a| a.call.id == call.id)
        {
            let Some(active) = self.active_tool.take() else {
                return;
            };
            let rows = self.collect_tool_panel(
                tool_render::resolve(&active.call),
                &active.call,
                PanelState::Review,
                None,
                Some(active.started),
                ToolOutcome::Review,
            );
            self.replace_active_tool_panel(&active, rows);
            self.active_tool = Some(active);
        }
    }

    /// Flip a generic active tool block RUNNING in place (adopting a `▲ REVIEW`
    /// block once the call is approved). Generic tools do not stream, so the
    /// body is empty until the result arrives.
    fn relayout_active_tool_running(&mut self) {
        let Some(active) = self.active_tool.take() else {
            return;
        };
        let rows = self.collect_tool_panel(
            tool_render::resolve(&active.call),
            &active.call,
            PanelState::Running,
            None,
            Some(active.started),
            ToolOutcome::Running { streamed: "" },
        );
        self.replace_active_tool_panel(&active, rows);
        self.active_tool = Some(active);
    }

    /// Flip the active exec block to `■ DENIED` in place (the tool never ran, so
    /// the body is just the refused command). Returns false when the call does
    /// not match the tracked exec.
    fn finalize_active_denied(&mut self, call: &ToolCall) -> bool {
        let Some(active) = self.active_exec.take() else {
            return false;
        };
        if active.call.id != call.id {
            self.active_exec = Some(active);
            return false;
        }
        let rows = self.collect_tool_panel(
            tool_render::resolve(call),
            call,
            PanelState::Denied,
            None,
            Some(active.started),
            ToolOutcome::Review,
        );
        self.replace_active_exec_panel(&active, rows);
        true
    }

    /// Flip the active generic tool block to `■ DENIED` in place.
    fn finalize_active_tool_denied(&mut self, call: &ToolCall) -> bool {
        let Some(active) = self.active_tool.take() else {
            return false;
        };
        if active.call.id != call.id {
            self.active_tool = Some(active);
            return false;
        }
        let rows = self.collect_tool_panel(
            tool_render::resolve(call),
            call,
            PanelState::Denied,
            None,
            Some(active.started),
            ToolOutcome::Review,
        );
        self.replace_active_tool_panel(&active, rows);
        true
    }

    /// Fallback `■ DENIED` block when a denial arrives with no pending review
    /// block to adopt (e.g. a read-side tool, or a decision that raced ahead of
    /// its review event).
    fn push_denied_block(&mut self, call: &ToolCall) {
        self.begin_block();
        self.append_tool_panel(
            tool_render::resolve(call),
            call,
            PanelState::Denied,
            None,
            Some(Instant::now()),
            ToolOutcome::Review,
        );
    }

    /// Push a complete EDIT block whose body is the canonical block diff
    /// (`DiffBlock` rows verbatim) and whose footer carries the `+n −n` counts
    /// and note as fields after the state label. The body folds whole behind
    /// the header disclosure; the footer never folds.
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
        let diff_rows = diff_table_rows(diff);
        // EXCEPTION to compact-by-default: a pending EDIT preview/review exists
        // to be reviewed, so it (and the running edit) arrives EXPANDED; it
        // collapses once the edit is finalized (state Done/Error). An explicit
        // user fold is reapplied by the rebuild path.
        let expanded = matches!(
            state,
            PanelState::Preview | PanelState::Review | PanelState::Running
        );
        self.push_panel_header_with_expanded(
            PanelHeaderSpec {
                title: "EDIT",
                meta: &meta,
                plain_meta: &plain,
                state,
                duration,
                started,
            },
            expanded,
        );
        let body_from = self.rows.len();
        if diff_rows.is_empty() && error.is_none() {
            if diff.trim().is_empty() {
                // A preview/edit whose diff is genuinely empty (e.g. the edit's
                // old_string did not match the file) would otherwise render an
                // empty frame -- header + bottom border, zero body rows --
                // leaving nothing to review while the approval modal waits.
                // Emit one honest dim placeholder instead of fabricating a diff.
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
                // Non-empty preview text that does not parse into diff rows --
                // e.g. "diff unavailable: preview too large" -- carries
                // actionable meaning. Render it verbatim as dim body rows
                // rather than hiding it behind the generic placeholder.
                for line in diff.trim_end_matches('\n').split('\n') {
                    self.rows.push(TranscriptRow::chrome_with_text(
                        ChromeRow::Body {
                            line: Line::from(Span::styled(line.to_string(), dim_style())),
                            bg: None,
                        },
                        line.to_string(),
                        dim_style(),
                    ));
                }
            }
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
        // The whole body (diff rows, placeholders, error line) folds as one
        // unit behind the header disclosure.
        for row in &mut self.rows[body_from..] {
            if row.fold == FoldVis::Always {
                row.fold = FoldVis::WhenExpanded;
            }
        }
        let (added, removed) = diff_counts(diff);
        let note = diff.contains("--- /dev/null").then_some("new file");
        let diag = self.tool_diags.get(&call.id).cloned();
        let mut extras = edit_footer_extras(added, removed, note);
        extras.extend(self.approval_footer_fields(&call.id, state));
        self.push_block_footer(state, extras, diag.as_ref(), Some(&call.id));
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
        let body_from = self.rows.len();
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
        for row in &mut self.rows[body_from..] {
            row.fold = FoldVis::WhenExpanded;
        }
        let (added, removed) = diff_counts(diff);
        self.push_block_footer(
            PanelState::Done,
            edit_footer_extras(added, removed, None),
            None,
            None,
        );
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
            user_expanded: None,
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
        if let Some(expanded) = active.user_expanded {
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
                    Some(ChromeRow::BlockEnd | ChromeRow::RailEnd)
                )
            })
            .map_or(start, |offset| start + offset + 1)
    }

    fn active_tool_panel_end(&self, active: &ActiveTool) -> usize {
        self.panel_end_from(active.body_start)
    }

    /// Reapply an active block's recorded fold state (`user_expanded`) onto a
    /// fresh in-place rebuild. Only an explicit user toggle survives across
    /// running -> done / preview -> done; otherwise the fresh block keeps its
    /// compact-by-default arrival state.
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
        if let Some(expanded) = active.user_expanded {
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
        if let Some(expanded) = active.user_expanded {
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

    fn explore_elapsed(duration: Option<Duration>) -> String {
        duration
            .map(format_elapsed_compact)
            .unwrap_or_else(|| "0.0s".to_string())
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

    /// Rewrite the open EXPLORE block's header (elapsed) and footer (state
    /// label) in place. The header right edge carries only the elapsed time;
    /// the state lives in the footer.
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
            elapsed: Self::explore_elapsed(duration),
        });
        let diag = self.tool_diags.get(&call.id).cloned();
        if let Some(offset) = self.rows[header..]
            .iter()
            .position(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::Footer { .. })))
        {
            self.rows[header + offset] =
                block_footer_row(state, Vec::new(), diag.as_ref(), Some(&call.id));
        }
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

    /// Pop the open EXPLORE block's trailing footer (rule · footer · end
    /// marker) so a new op row can be appended before the footer is re-pushed.
    fn pop_trailing_explore_footer(&mut self) {
        if !matches!(
            self.rows.last().and_then(|row| row.chrome.as_ref()),
            Some(ChromeRow::BlockEnd)
        ) {
            return;
        }
        let rule = self.rows.len().saturating_sub(3);
        if matches!(
            self.rows.get(rule).and_then(|row| row.chrome.as_ref()),
            Some(ChromeRow::FooterRule)
        ) {
            self.mark_dirty_from(rule);
            self.rows.truncate(rule);
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
            self.pop_trailing_explore_footer();
        } else {
            self.push_blank();
            self.rows.push(TranscriptRow::chrome(ChromeRow::BlockStart));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
                expanded: true,
                title: "EXPLORE",
                meta,
                elapsed: Self::explore_elapsed(duration),
            }));
        }
        self.rows.push(row.with_fold(FoldVis::WhenExpanded));
        let state = panel_state(false, false);
        let diag = self.tool_diags.get(&call.id).cloned();
        self.push_block_footer(state, Vec::new(), diag.as_ref(), Some(&call.id));
        if self.exploring_open {
            self.set_explore_header(call, state, duration);
        }
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
            self.pop_trailing_explore_footer();
        } else {
            self.push_blank();
            self.rows.push(TranscriptRow::chrome(ChromeRow::BlockStart));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
                expanded: true,
                title: "EXPLORE",
                meta,
                elapsed: Self::explore_elapsed(Some(Duration::ZERO)),
            }));
        }
        let row = self.rows.len();
        self.rows.push(body.with_fold(FoldVis::WhenExpanded));
        self.push_block_footer(PanelState::Running, Vec::new(), None, Some(&call.id));
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
        if !matches!(
            slot.chrome.as_ref(),
            Some(ChromeRow::Body { .. } | ChromeRow::BodyRight { .. })
        ) {
            return false;
        }
        self.mark_dirty_from(row);
        self.rows[row] = replacement.with_fold(FoldVis::WhenExpanded);
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
        // Reasoning summaries stream before the answer. The transient preview can
        // only render below every committed row, so a live reasoning trace must
        // be committed as a thinking block the moment ANY non-reasoning event
        // arrives (first answer delta, tool, completion, cancel, error, ...).
        // This idempotent guard is the single finalize point; the two reasoning
        // events below are the only ones that keep the live trace open.
        if !matches!(
            event,
            UiEvent::AssistantReasoningDelta(_)
                | UiEvent::AssistantReasoningSectionBreak
                | UiEvent::AssistantRawReasoningDelta(_)
        ) {
            self.finish_live_reasoning_if_any();
        }
        match event {
            UiEvent::AssistantReasoningDelta(delta) => {
                self.push_reasoning_delta(&delta);
            }
            UiEvent::AssistantRawReasoningDelta(delta) => {
                self.push_raw_reasoning_delta(&delta);
            }
            UiEvent::AssistantReasoningSectionBreak => {
                self.push_reasoning_section_break();
            }
            UiEvent::ProviderTurnStarted { .. } => {
                self.provider_turn_started = Some(Instant::now());
                self.thinking_header_row = None;
                self.thinking_elapsed = None;
                self.current_turn_diag = None;
            }
            UiEvent::ProviderTurnCompleted { usage, .. } => {
                if let Some(usage) = usage {
                    self.set_thinking_telemetry(usage.reasoning_output_tokens);
                    // Forward attribution. This turn's INPUT side ingested the
                    // previous turn's tool results: patch their footers with
                    // ↑/cache/ctx (ctx diffs against the still-current baseline),
                    // then advance the baseline to this turn's input.
                    self.apply_following_turn_usage(&usage);
                    self.last_turn_input_tokens = Some(usage.input_tokens);
                    // This turn's OUTPUT side is the ↓ for the tools IT proposes.
                    let diag = Self::proposing_turn_diag(&usage);
                    // Common order: this completes before its tools start, so
                    // the stored ↓ is stamped on later `ToolStarted`s. Also cover
                    // the tools-first order by stamping any open call now and
                    // enrolling it to await the NEXT turn's input side.
                    for id in self.active_call_ids() {
                        self.set_tool_diag(&id, diag.clone());
                        self.mark_awaiting_input(&id);
                    }
                    self.current_turn_diag = Some(diag);
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
                );
            }
            UiEvent::CompactionLifecycle {
                state,
                covered_messages,
                original_tokens_estimate,
                message,
                ..
            } => {
                self.finish_stream();
                let detail = message.unwrap_or_else(|| {
                    format!(
                        "Background compaction {} — {} message(s), ~{} tokens",
                        state.as_str(),
                        covered_messages,
                        super::screen::compact_count(original_tokens_estimate),
                    )
                });
                self.push_notice_row(crate::ui::symbols::SEP, dim_style(), &detail);
            }
            UiEvent::FoldApplied {
                folds,
                reclaimed_tokens_estimate,
                trigger,
            } => {
                // A runtime event, not the assistant speaking: a quiet info
                // notice itemizing what the fold pass reclaimed and why it ran
                // (issue #400, trigger-tagged so an opt-in history rewrite is
                // always visible and priced).
                self.finish_stream();
                self.push_notice_row(
                    crate::ui::symbols::SEP,
                    dim_style(),
                    &format!(
                        "Folded {folds} spent tool result(s) \u{2014} reclaimed ~{} tokens [{}]",
                        super::screen::compact_count(reclaimed_tokens_estimate),
                        trigger.code(),
                    ),
                );
            }
            UiEvent::ProviderTurnCancelled { .. }
            | UiEvent::ProviderTurnError { .. }
            | UiEvent::ToolLifecycle { .. }
            | UiEvent::OutputHandleStored { .. } => {}
            // Freeform tool-input deltas (ADR-0039) are display-only. The live
            // preview cell is deferred until a freeform tool (`apply_patch`, V4A)
            // exists to render; until then the event is inert here. The guard
            // above still commits any open reasoning trace, since this is a
            // non-reasoning event.
            UiEvent::ToolInputDelta { .. } => {}
            UiEvent::AssistantTextDelta(delta) => {
                if !self.stream.is_active() {
                    // A fresh stream starts here: drop any memoized tail render
                    // of a prior stream, open the block with a separator, and
                    // remember where the answer begins so a late reasoning block
                    // can be spliced above it.
                    self.streaming_render = None;
                    self.push_blank();
                    self.stream_answer_start = Some(self.rows.len());
                }
                let width = self.markdown_content_width();
                self.stream.push_delta(&delta, width);
            }
            UiEvent::AssistantTextEnd(text) => {
                self.streaming_render = None;
                if self.stream.is_active() {
                    // Prefer the accumulated deltas: the controller renders the
                    // complete source once and commits only what is not already
                    // in scrollback (ADR: streamed answer committed exactly once).
                    self.finish_stream();
                } else if !text.is_empty() {
                    // Some providers send only a terminal text event with no
                    // deltas; commit it directly.
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
                    self.push_thinking_block(&text, None, redacted);
                }
            }
            UiEvent::SessionStarted => {
                self.finish_stream();
                // A fresh session: drop measured diagnostics, pending footer
                // patches, and the ctx baseline so a new conversation never
                // inherits stale numbers.
                self.tool_diags.clear();
                self.review_gates.clear();
                self.approval_notes.clear();
                self.current_turn_diag = None;
                self.awaiting_input_calls.clear();
                self.last_turn_input_tokens = None;
            }
            UiEvent::ToolProposed(_) => {
                // Non-gated tools show only their result row; nothing to render.
                self.finish_stream();
            }
            UiEvent::ToolStarted(call) => {
                // Stamp the proposing turn's ↓ and enroll the call to await the
                // following turn's input side, before dispatch, so the block's
                // first footer build already carries ↓.
                self.assign_turn_diag(&call.id);
                match tool_render::resolve(&call).kind() {
                    ToolPanelKind::Explore => self.push_explored_start(&call),
                    ToolPanelKind::Shell => {
                        // Adopt a pending `▲ REVIEW` block (approved): flip it to
                        // `● RUNNING` in place instead of opening a second block.
                        if self
                            .active_exec
                            .as_ref()
                            .is_some_and(|a| a.call.id == call.id)
                        {
                            self.review_gates.remove(&call.id);
                            self.relayout_active_running();
                        } else {
                            self.begin_exec(call);
                        }
                    }
                    ToolPanelKind::Generic => {
                        // A mutation whose diff/review block already exists keeps
                        // it: the preview/review flips to `● RUNNING` in place.
                        if self.rebuild_active_edit(&call, PanelState::Running, None, None, true) {
                            self.review_gates.remove(&call.id);
                        } else if self
                            .active_tool
                            .as_ref()
                            .is_some_and(|a| a.call.id == call.id)
                        {
                            self.review_gates.remove(&call.id);
                            self.relayout_active_tool_running();
                        } else {
                            self.begin_tool(call);
                        }
                    }
                }
            }
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
            UiEvent::ToolReview {
                call,
                allow_always,
                allow_project,
                dirty_gate,
                reason,
            } => {
                self.assign_turn_diag(&call.id);
                self.begin_review(
                    call,
                    ReviewGate {
                        allow_always,
                        allow_project,
                        dirty_gate,
                        reason,
                    },
                );
            }
            UiEvent::ToolAutoApproved(_call) => {
                // Auto-approval is implicit in the policy; the tool block alone
                // is the record. No approval chrome, no separate panel.
            }
            UiEvent::DiffPreview { call, diff } => {
                self.assign_turn_diag(&call.id);
                self.begin_edit_preview(&call, diff);
            }
            UiEvent::ToolDenied(call) => {
                // The decision lives in the tool block: flip the pending review
                // (or edit preview) to `■ DENIED` in place — never a second
                // block, never a duplicated command.
                self.review_gates.remove(&call.id);
                // Flip the first matching pending block (exec review, edit
                // preview, or generic tool review) to `■ DENIED` in place,
                // short-circuiting on the first adopter; fall back to a
                // standalone denied block when none matches.
                let adopted = self.finalize_active_denied(&call)
                    || self.rebuild_active_edit(&call, PanelState::Denied, None, None, false)
                    || self.finalize_active_tool_denied(&call);
                if !adopted {
                    self.push_denied_block(&call);
                }
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
                self.push_notice_row(crate::ui::symbols::SEP, dim_style(), &message);
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
            || self.stream.is_active()
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
        // Prompt anchors shift with the trimmed head; trimmed-away prompts are
        // dropped.
        self.user_prompts = self
            .user_prompts
            .iter()
            .filter_map(|prompt| {
                Some(UserPromptAnchor {
                    row: prompt.row.checked_sub(remove)?,
                    text: prompt.text.clone(),
                })
            })
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
                    | ChromeRow::Body { .. }
                    | ChromeRow::BodyRight { .. }
                    | ChromeRow::BodyRule { .. }
                    | ChromeRow::FooterRule
                    | ChromeRow::Footer { .. }
                    | ChromeRow::BlockEnd
            )
        )
    }

    fn trailing_explore_panel_open(&self) -> bool {
        let Some(last) = self.rows.iter().rposition(|row| !is_separator_row(row)) else {
            return false;
        };
        if !matches!(self.rows[last].chrome.as_ref(), Some(ChromeRow::BlockEnd)) {
            return false;
        }
        for row in self.rows[..=last].iter().rev() {
            match row.chrome.as_ref() {
                Some(ChromeRow::Header { title, .. }) => return *title == "EXPLORE",
                Some(ChromeRow::BlockStart) => return false,
                _ => {}
            }
        }
        false
    }

    /// Test helper: toggle the newest foldable panel. Production ctrl+o goes
    /// through [`Self::toggle_all_panels`].
    #[cfg(test)]
    pub(super) fn toggle_latest_panel(&mut self) -> bool {
        let Some(header) = self.rows.iter().rposition(|row| {
            matches!(
                row.chrome.as_ref(),
                Some(ChromeRow::Header { .. } | ChromeRow::RailHeader { .. })
            )
        }) else {
            return false;
        };
        match self.panel_expanded_at(header) {
            Some(expanded) => self.set_panel_expanded_at(header, !expanded),
            None => false,
        }
    }

    /// Whether the panel whose header row is `header` has any foldable body
    /// (a row that hides when collapsed); a panel with nothing hidden cannot
    /// toggle.
    fn header_is_foldable(&self, header: usize) -> bool {
        let end = self.panel_end_from(header);
        self.rows[header..end]
            .iter()
            .any(|row| row.fold != FoldVis::Always)
    }

    /// ctrl+o: expand every foldable panel (tool blocks AND thinking rails) if
    /// ANY is currently collapsed, else collapse them all. Returns whether any
    /// header changed; dirties from the first affected row via
    /// [`Self::set_panel_expanded_at`].
    pub(super) fn toggle_all_panels(&mut self) -> bool {
        let headers: Vec<usize> = self
            .panel_header_rows()
            .into_iter()
            .filter(|&header| self.header_is_foldable(header))
            .collect();
        if headers.is_empty() {
            return false;
        }
        // Expand all if any foldable block is currently collapsed.
        let expand = headers
            .iter()
            .any(|&header| self.panel_expanded_at(header) == Some(false));
        let mut changed = false;
        for header in headers {
            if self.set_panel_expanded_at(header, expand) {
                changed = true;
            }
        }
        changed
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
    /// hidden, or an already-matching state). An explicit set on an active
    /// block's header is recorded as user intent so it survives finalize,
    /// even when it is a no-op on the current row (e.g. re-affirming a running
    /// block should stay expanded).
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
        if !self.header_is_foldable(header) {
            return false;
        }
        self.record_user_fold(header, expand);
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

    /// Record an explicit user fold on an active block so it survives the
    /// in-place rebuild at finalize. A block's header row is `body_start + 1`
    /// (the row after its `BlockStart`); at most one exec/tool/edit is active.
    fn record_user_fold(&mut self, header: usize, expanded: bool) {
        if let Some(active) = self.active_exec.as_mut()
            && active.body_start + 1 == header
        {
            active.user_expanded = Some(expanded);
        }
        if let Some(active) = self.active_tool.as_mut()
            && active.body_start + 1 == header
        {
            active.user_expanded = Some(expanded);
        }
        if let Some(active) = self.active_edit.as_mut()
            && active.body_start + 1 == header
        {
            active.user_expanded = Some(expanded);
        }
    }

    /// The foldable header row whose visible physical-line span covers `line`
    /// (a whole header row is the click target, not just the disclosure
    /// glyph). Requires a warm wrap cache; `None` outside any header.
    pub(super) fn header_row_at_visible_line(&self, line: usize) -> Option<usize> {
        self.panel_header_rows().into_iter().find(|&header| {
            let Some(start) = self.visible_line_of_row(header) else {
                return false;
            };
            let span = self
                .wrapped_cache
                .rows
                .get(header)
                .map_or(1, |layout| layout.lines.len().max(1));
            (start..start + span).contains(&line)
        })
    }

    /// Case-insensitive substring search over searchable transcript content,
    /// including folded-away panel bodies and visible collapsed rows. Control
    /// chrome such as fold affordance hints is marked `searchable == false`
    /// and skipped so a query never matches hidden UI. The cache is
    /// refreshed at its
    /// current width first, so appends and fold changes since the last frame
    /// are searched and stale lines are not. Returns matches in ascending
    /// (row, sub-line) order, each identified by the logical row that owns it
    /// so a jump can expand the enclosing fold before resolving a visible
    /// line. O(total physical lines) per invocation -- run per
    /// `/find`/`n`/`N` keypress, never per frame.
    pub(super) fn search_matches(&mut self, query: &str) -> Vec<SearchMatch> {
        let needle = query.to_lowercase();
        let width = self.wrapped_cache.width;
        if needle.is_empty() || width == 0 {
            return Vec::new();
        }
        // Same cache-warming path rendering uses: dirty rows re-wrap, fold
        // visibility re-resolves. Folded-away bodies are still rendered into
        // the cache (only their visible-line accounting is suppressed), so
        // their physical lines are here to search.
        self.ensure_wrapped_cache(width);
        let mut out = Vec::new();
        for (row_idx, layout) in self.wrapped_cache.rows.iter().enumerate() {
            let row = &self.rows[row_idx];
            if !row.searchable {
                continue;
            }
            for (sub, line) in self.wrapped_cache.lines[layout.lines.clone()]
                .iter()
                .enumerate()
            {
                let text: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                if text.to_lowercase().contains(&needle) {
                    out.push(SearchMatch { row: row_idx, sub });
                }
            }
        }
        out
    }

    /// Expand the fold enclosing logical `row` (when a collapsed panel hides
    /// it) and return the visible physical-line index of the row's `sub`-th
    /// wrapped line under the refreshed cache. The reveal is what makes a
    /// `/find` jump into a collapsed panel land on a visible row instead of a
    /// dead offset. `None` only when the row cannot be made visible (no
    /// enclosing panel, or the cache does not cover it).
    pub(super) fn reveal_and_locate(&mut self, row: usize, sub: usize) -> Option<usize> {
        let hidden = self
            .wrapped_cache
            .rows
            .get(row)
            .is_some_and(|layout| !layout.visible);
        if hidden
            && let Some(header) = self
                .panel_header_rows()
                .into_iter()
                .rev()
                .find(|&header| header <= row)
        {
            self.set_panel_expanded_at(header, true);
        }
        // Re-warm after a possible expansion so the returned index matches the
        // next frame's layout.
        let width = self.wrapped_cache.width;
        self.ensure_wrapped_cache(width);
        Some(self.visible_line_of_row(row)? + sub)
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
        self.user_prompts.push(UserPromptAnchor {
            row: self.rows.len(),
            text: text.to_string(),
        });
        pane::push_user_rows(&mut self.rows, text);
        self.trim_history();
    }

    /// Text of the newest user prompt that begins strictly above the viewport top
    /// -- the pager's sticky prompt-card anchor. Requires a warm wrap cache
    /// (compose refreshes it every frame). Binary search over the sorted anchors
    /// (prompt rows are always visible and line positions are monotone in row
    /// order), so the per-frame cost is O(log prompts), never a prompt walk.
    pub(super) fn sticky_prompt_text(&self, top: usize) -> Option<&str> {
        let above = self.user_prompts.partition_point(|prompt| {
            self.visible_line_of_row(prompt.row)
                .is_some_and(|line| line < top)
        });
        let prompt = self.user_prompts.get(above.checked_sub(1)?)?;
        self.visible_line_of_row(prompt.row)
            .filter(|&line| line < top)
            .map(|_| prompt.text.as_str())
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
                // A block opens fully shown until its header sets the state;
                // resetting at BlockStart guards against a missing BlockEnd
                // leaking a prior block's collapsed state into the next one.
                Some(ChromeRow::BlockStart) => expanded = true,
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
                Some(ChromeRow::BlockEnd | ChromeRow::RailEnd)
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
        // The answer active tail and the live reasoning preview are mutually
        // exclusive per frame (reasoning fully precedes the answer). The answer
        // stream takes precedence if somehow both are set.
        if self.stream.is_active() {
            let key = self.stream.tail_signature();
            let fresh = self
                .streaming_render
                .as_ref()
                .is_some_and(|memo| !memo.is_reasoning && memo.key == key && memo.width == width);
            if !fresh {
                let content_width = width
                    .saturating_sub(TEXT_COLUMN_X_PADDING.saturating_mul(2))
                    .max(1);
                let mut lines = Vec::new();
                for row in &self.stream.tail_rows(content_width) {
                    row.render_rows(width, &mut lines);
                }
                self.streaming_render = Some(StreamingRender {
                    key,
                    width,
                    lines,
                    is_reasoning: false,
                });
            }
            return;
        }
        let preview_text = self
            .live_reasoning_summary
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .or_else(|| {
                self.live_reasoning_raw
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
            });
        if let Some(text) = preview_text {
            // Key on both buffers: either stream can grow, so a longer buffer
            // means new content to re-render.
            let key = (
                self.live_reasoning_summary.as_deref().map_or(0, str::len),
                self.live_reasoning_raw.as_deref().map_or(0, str::len),
            );
            let fresh = self
                .streaming_render
                .as_ref()
                .is_some_and(|memo| memo.is_reasoning && memo.key == key && memo.width == width);
            if !fresh {
                let content_width = width
                    .saturating_sub(TEXT_COLUMN_X_PADDING.saturating_mul(2))
                    .max(1);
                let rows = live_reasoning_preview_rows(text, content_width);
                let mut lines = Vec::new();
                for row in &rows {
                    row.render_rows(width, &mut lines);
                }
                self.streaming_render = Some(StreamingRender {
                    key,
                    width,
                    lines,
                    is_reasoning: true,
                });
            }
            return;
        }
        self.streaming_render = None;
    }

    /// Number of transient streaming-preview lines at `width` (0 when idle).
    /// Covers both preview sources (answer tail and reasoning preview) via the
    /// memo, which the callers refresh for the current width first.
    fn streaming_lines(&self) -> usize {
        self.streaming_render
            .as_ref()
            .map_or(0, |memo| memo.lines.len())
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
        if let Some(memo) = self.streaming_render.as_ref() {
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
