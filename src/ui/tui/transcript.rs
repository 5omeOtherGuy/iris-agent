//! Retained transcript state and event-to-row rendering.

use std::time::{Duration, Instant};

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::nexus::{ApprovalDecision, ProviderUsage, ToolCall};
use crate::tool_display::run_target;
use crate::ui::markdown::{MarkdownTheme, render_markdown_themed};
use crate::ui::{TurnErrorKind, UiEvent};

use super::component::{self, Component};
use super::pane;
use super::panel::{PanelHeaderSpec, PanelState, diff_table_rows, panel_state};
use super::rows::{ChromeRow, FoldVis, TranscriptRow, is_separator_row};
use super::text::ansi_spans;
use super::tool_render::{self, RenderCtx, ToolOutcome, ToolPanelKind};
use super::wrap::line_text;
use super::{
    MAX_EXEC_STREAM_BYTES, MAX_STREAMING_MARKDOWN_BYTES, MAX_TRANSCRIPT_ROWS,
    TEXT_COLUMN_X_PADDING, dim_style, err_style, format_elapsed_compact, ok_style, panel_style,
    tool_header_style, turn_divider_label,
};

/// Collapsed-state label for a reasoning panel (mirrors pi-mono's
/// `hiddenThinkingLabel`).
const THINKING_LABEL: &str = "Thinking...";
/// Placeholder for reasoning the provider withheld; the original text is never
/// available and is never rendered.
const REDACTED_THINKING_BODY: &str = "[reasoning withheld by provider]";

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
    pub(super) exploring_open: bool,
    /// Last width the transcript was rendered/flushed at, so width-aware
    /// shaping in the width-agnostic `apply` path (the tool-output flood cap)
    /// uses a realistic column count. Zero until the first render.
    last_width: usize,
}

impl Transcript {
    /// Append a blank separator row before a new top-level block, unless the
    /// transcript is empty or already ends in a real separator row.
    fn push_blank(&mut self) {
        self.exploring_open = false;
        match self.rows.last() {
            None => {}
            Some(last) if is_separator_row(last) => {}
            _ => self
                .rows
                .push(TranscriptRow::new(String::new(), Style::default())),
        }
    }

    /// Finish any live stream and open a fresh block with a leading separator.
    fn begin_block(&mut self) {
        self.finish_stream();
        self.push_blank();
    }

    /// Push each line of `text` into the transcript with one style.
    fn push(&mut self, text: &str, style: Style) {
        for line in text.split('\n') {
            self.rows.push(TranscriptRow::new(line, style));
        }
    }

    fn push_assistant_text(&mut self, text: &str) {
        let width = self.markdown_content_width();
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

    /// Render a model reasoning ("thinking") trace as a collapsible panel.
    ///
    /// The panel is collapsed by default, showing only a `Thinking...` label;
    /// the existing `ctrl+o` panel toggle (`toggle_latest_panel`) expands it to
    /// the markdown-rendered trace, styled dim + italic. A `redacted` block has
    /// no recoverable text, so a placeholder is shown and the original reasoning
    /// is never rendered. Mirrors pi-mono's hide/show thinking block.
    fn push_thinking_block(&mut self, text: &str, redacted: bool) {
        // Intentionally do NOT finish the live stream here. Reasoning is emitted
        // at completion, after the answer's text deltas have already streamed
        // into `self.streaming` but before `AssistantTextEnd` commits them.
        // Committing the stream now (via `begin_block`) would render the answer
        // *above* the thinking block and double-commit it when `AssistantTextEnd`
        // arrives. Adding the panel rows while the stream stays pending keeps the
        // thinking block above the answer, which is committed afterwards.
        self.push_blank();
        self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Header {
                expanded: false,
                title: "THINKING",
                meta: String::new(),
                right: vec![(THINKING_LABEL.to_string(), dim_style())],
            },
            THINKING_LABEL.to_string(),
            dim_style(),
        ));
        // Separator + trace are tagged `WhenExpanded` so the new fold-based
        // collapse model hides them while collapsed, leaving just the header's
        // `Thinking...` label; `ctrl+o` (`toggle_latest_panel`) reveals them.
        self.rows
            .push(TranscriptRow::chrome(ChromeRow::Separator).with_fold(FoldVis::WhenExpanded));
        if redacted {
            self.rows.push(
                TranscriptRow::chrome_with_text(
                    ChromeRow::Body {
                        line: Line::from(Span::styled(
                            REDACTED_THINKING_BODY.to_string(),
                            dim_style(),
                        )),
                        bg: None,
                    },
                    REDACTED_THINKING_BODY.to_string(),
                    dim_style(),
                )
                .with_fold(FoldVis::WhenExpanded),
            );
        } else {
            let theme = MarkdownTheme::thinking();
            let width = self.markdown_content_width();
            for line in render_markdown_themed(text, &theme, width) {
                let plain = line_text(&line);
                self.rows.push(
                    TranscriptRow::chrome_with_text(
                        ChromeRow::Body { line, bg: None },
                        plain,
                        dim_style(),
                    )
                    .with_fold(FoldVis::WhenExpanded),
                );
            }
        }
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
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
            ApprovalDecision::Deny => return,
        };
        self.begin_block();
        self.push_approval_panel(approval_line(call, scope), false);
    }

    fn push_approval_panel(&mut self, line: Line<'static>, failed: bool) {
        self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
        self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
            expanded: true,
            title: "APPROVAL",
            meta: "decision".to_string(),
            right: vec![
                (
                    "●".to_string(),
                    if failed { err_style() } else { ok_style() },
                ),
                (
                    if failed {
                        " DENIED      "
                    } else {
                        " APPROVED    "
                    }
                    .to_string(),
                    panel_style(),
                ),
            ],
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
            self.push_explored_result(call, duration);
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
            ToolOutcome::Done { content },
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
        for row in self.rows.iter_mut().rev() {
            if let Some(ChromeRow::Header { expanded, .. }) = row.chrome.as_mut() {
                *expanded = false;
                return;
            }
        }
    }

    fn push_panel_header_with_expanded(&mut self, spec: PanelHeaderSpec<'_>, expanded: bool) {
        let elapsed = if spec.state == PanelState::Running {
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
                right: vec![
                    ("●".to_string(), spec.state.dot_style()),
                    (spec.state.label().to_string(), panel_style()),
                    (format!("     {elapsed:>10}  "), dim_style()),
                ],
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
            ToolOutcome::Done { content }
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

    fn panel_end_from(&self, start: usize) -> usize {
        self.rows[start..]
            .iter()
            .position(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::Bottom)))
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
            ToolOutcome::Done { content },
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
            ToolOutcome::Done { content },
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
        vec![
            ("●".to_string(), state.dot_style()),
            (format!("{:<13}", state.label()), panel_style()),
            // Fixed-width elapsed so the live timer does not shift the header
            // right edge as the compact label changes length (e.g. 9.9s -> 10s).
            (format!("{elapsed:>8}  "), dim_style()),
        ]
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

    /// EXPLORE body summary `(text, style)`, single-sourced from the renderer's
    /// one-row body for the given outcome (falls back to an empty dim row only
    /// if a misbehaving renderer produced nothing).
    fn explore_text_style(&self, call: &ToolCall, outcome: ToolOutcome<'_>) -> (String, Style) {
        let renderer = tool_render::resolve(call);
        let ctx = self.render_ctx();
        tool_render::render_body(renderer, &ctx, call, &outcome)
            .into_iter()
            .next()
            .map(|row| (row.text, row.style))
            .unwrap_or_else(|| (String::new(), dim_style()))
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
            self.rows.pop();
        }
    }

    fn push_explore_body(&mut self, call: &ToolCall, failed: bool, duration: Option<Duration>) {
        let meta = self.explore_meta(call);
        let (text, _) = self.explore_text_style(call, ToolOutcome::Done { content: "" });
        let style = if failed { err_style() } else { dim_style() };
        if self.exploring_open {
            self.pop_trailing_explore_bottom();
            self.set_explore_header(call, panel_state(false, failed), duration);
        } else {
            self.push_blank();
            self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
                expanded: true,
                title: "EXPLORE",
                meta,
                right: Self::explore_header_right(panel_state(false, failed), duration),
            }));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
        }
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Body {
                line: Line::from(Span::styled(text.clone(), style)),
                bg: None,
            },
            text,
            style,
        ));
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        self.exploring_open = true;
    }

    fn push_explored_result(&mut self, call: &ToolCall, duration: Option<Duration>) {
        self.finish_stream();
        let (text, style) = self.explore_text_style(call, ToolOutcome::Done { content: "" });
        if self.finish_exploration(call, text, style, duration, false, false) {
            return;
        }
        self.push_explore_body(call, false, duration);
    }

    fn push_explored_start(&mut self, call: &ToolCall) {
        self.finish_stream();
        let started = Instant::now();
        let meta = self.explore_meta(call);
        let (text, style) = self.explore_text_style(call, ToolOutcome::Done { content: "" });
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
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Body {
                line: Line::from(Span::styled(text.clone(), style)),
                bg: None,
            },
            text,
            style,
        ));
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

    fn replace_explore_body_at(&mut self, row: usize, text: String, style: Style) -> bool {
        let Some(slot) = self.rows.get_mut(row) else {
            return false;
        };
        if !matches!(slot.chrome.as_ref(), Some(ChromeRow::Body { .. })) {
            return false;
        }
        *slot = TranscriptRow::chrome_with_text(
            ChromeRow::Body {
                line: Line::from(Span::styled(text.clone(), style)),
                bg: None,
            },
            text,
            style,
        );
        true
    }

    fn finish_exploration(
        &mut self,
        call: &ToolCall,
        text: String,
        style: Style,
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
        let replaced = self.replace_explore_body_at(row, text, style);
        debug_assert!(replaced);
        self.update_explore_header_from_active(call);
        true
    }

    fn push_explored_error(&mut self, call: &ToolCall, message: &str) -> bool {
        self.finish_stream();
        let (text, style) = self.explore_text_style(
            call,
            ToolOutcome::Error {
                message,
                streamed: "",
            },
        );
        self.finish_exploration(call, text, style, None, true, false)
    }

    fn push_explored_cancelled(&mut self, call: &ToolCall) -> bool {
        self.finish_stream();
        let (text, style) = self.explore_text_style(call, ToolOutcome::Cancelled { streamed: "" });
        self.finish_exploration(call, text, style, None, false, true)
    }

    /// Apply one semantic event to the transcript rows.
    pub(super) fn apply(&mut self, event: UiEvent) {
        match event {
            UiEvent::ProviderTurnStarted { .. }
            | UiEvent::ProviderTurnCompleted { .. }
            | UiEvent::ProviderTurnCancelled { .. }
            | UiEvent::ProviderTurnError { .. }
            | UiEvent::ToolLifecycle { .. }
            | UiEvent::OutputHandleStored { .. }
            | UiEvent::CompactionApplied { .. } => {}
            UiEvent::AssistantTextDelta(delta) => {
                if self.streaming.is_none() {
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
                ToolPanelKind::Generic => self.begin_tool(call),
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
                self.clear_active_tool_for_preview(&call);
                self.begin_block();
                self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
                let renderer = tool_render::resolve(&call);
                self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
                    expanded: true,
                    title: renderer.title(),
                    meta: renderer.header_meta(&call),
                    right: vec![
                        ("●".to_string(), dim_style()),
                        (" PREVIEW     ".to_string(), panel_style()),
                    ],
                }));
                self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
                self.rows.extend(diff_table_rows(&diff));
                self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
            }
            UiEvent::ToolDenied(call) => {
                self.begin_block();
                let mut spans = vec![Span::styled("✗", err_style()), Span::raw(" Denied ")];
                spans.extend(ansi_spans(&run_target(&call), Style::default()));
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
                } else if self.finalize_active_tool_error(&call, &message) {
                } else if tool_render::resolve(&call).kind() != ToolPanelKind::Explore
                    || !self.push_explored_error(&call, &message)
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
                } else if self.finalize_active_tool_cancelled(&call) {
                } else if tool_render::resolve(&call).kind() != ToolPanelKind::Explore
                    || !self.push_explored_cancelled(&call)
                {
                    self.push_tool_cancelled(&call);
                }
            }
            UiEvent::Notice(message) => {
                self.begin_block();
                self.push(&format!("note: {}", message), dim_style());
            }
            UiEvent::TurnError { kind, message } => {
                self.begin_block();
                match kind {
                    TurnErrorKind::Auth => {
                        self.push(&format!("auth error: {}", message), err_style());
                        self.push(
                            "authentication required; re-run the login command",
                            err_style(),
                        );
                    }
                    TurnErrorKind::Provider => {
                        self.push(&format!("provider error: {}", message), err_style());
                    }
                }
            }
            UiEvent::TurnComplete => {
                self.finish_stream();
            }
        }
        self.trim_history();
    }

    pub(super) fn trim_history(&mut self) {
        if self.rows.len() <= MAX_TRANSCRIPT_ROWS
            || self.streaming.is_some()
            || self.active_exec.is_some()
            || self.active_tool.is_some()
            || !self.active_explorations.is_empty()
        {
            return;
        }
        let remove = self.panel_safe_trim_index(self.rows.len() - MAX_TRANSCRIPT_ROWS);
        self.rows.drain(..remove);
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
        let Some(header) = self
            .rows
            .iter()
            .rposition(|row| matches!(row.chrome.as_ref(), Some(ChromeRow::Header { .. })))
        else {
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
        if let Some(ChromeRow::Header { expanded, .. }) = self.rows[header].chrome.as_mut() {
            *expanded = !*expanded;
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub(super) fn latest_panel_collapsed(&self) -> bool {
        self.rows
            .iter()
            .rev()
            .find_map(|row| match row.chrome.as_ref() {
                Some(ChromeRow::Header { expanded, .. }) => Some(!*expanded),
                _ => None,
            })
            .unwrap_or(false)
    }

    /// Commit a submitted prompt into the transcript as plain pane text.
    /// This is display-only; the raw prompt still goes to Nexus unchanged
    /// through the loop.
    pub(super) fn commit_user(&mut self, text: &str) {
        self.push_blank();
        pane::push_user_rows(&mut self.rows, text);
        self.trim_history();
    }

    pub(super) fn render(&mut self, width: u16) -> Vec<Line<'static>> {
        let width = usize::from(width);
        self.last_width = width
            .saturating_sub(TEXT_COLUMN_X_PADDING.saturating_mul(2))
            .max(1);
        // Select the visible fold-tagged rows, then composite them through the
        // `Component` contract. Borrowing `&dyn Component` avoids boxing the
        // rows every frame while still routing every row through the shared path.
        let mut visible: Vec<&dyn Component> = Vec::with_capacity(self.rows.len());
        let mut expanded = true;
        for row in &self.rows {
            match row.chrome.as_ref() {
                // A panel opens fully shown until its header sets the state;
                // resetting at Top guards against a missing Bottom leaking a
                // prior panel's collapsed state into the next one.
                Some(ChromeRow::Top) => expanded = true,
                Some(ChromeRow::Header { expanded: e, .. }) => expanded = *e,
                _ => {}
            }
            let skip = match row.fold {
                FoldVis::Always => false,
                FoldVis::WhenCollapsed => expanded,
                FoldVis::WhenExpanded => !expanded,
            };
            if !skip {
                visible.push(row);
            }
            if matches!(row.chrome.as_ref(), Some(ChromeRow::Bottom)) {
                expanded = true;
            }
        }
        // The in-flight stream renders as transient rows appended after history;
        // hold them locally so they can join the same borrowed composite.
        let streaming_rows = self
            .streaming
            .as_ref()
            .map(|text| pane::streaming_assistant_rows(width, text))
            .unwrap_or_default();
        visible.extend(streaming_rows.iter().map(|row| row as &dyn Component));
        component::composite(visible, width)
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

fn approval_line(call: &ToolCall, scope: &str) -> Line<'static> {
    let mut spans = vec![
        Span::styled("✔", ok_style()),
        Span::raw(" You approved iris to run "),
    ];
    spans.extend(ansi_spans(&run_target(call), Style::default()));
    spans.push(Span::raw(format!(" {scope}")));
    Line::from(spans)
}
