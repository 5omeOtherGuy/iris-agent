//! Retained transcript state and event-to-row rendering.

use std::time::{Duration, Instant};

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::nexus::{ApprovalDecision, ProviderUsage, ToolCall};
use crate::tool_display::{is_exploration_tool, run_target};
use crate::ui::{TurnErrorKind, UiEvent};

use super::component::{self, Component};
use super::pane;
use super::panel::{
    PanelHeaderSpec, PanelState, diff_table_rows, explore_body, explore_panel_meta, panel_state,
    tool_panel_meta, tool_panel_title,
};
use super::rows::{ChromeRow, FoldVis, TranscriptRow, is_separator_row};
use super::text::{ansi_spans, strip_ansi_for_text};
use super::wrap::{
    clamp_output_line, display_width, line_text, truncate_chars, wrapped_row_estimate,
};
use super::{
    MAX_EXEC_STREAM_BYTES, MAX_STREAMING_MARKDOWN_BYTES, MAX_TOOL_OUTPUT_LINE_CHARS,
    MAX_TOOL_OUTPUT_ROWS, MAX_TRANSCRIPT_ROWS, TEXT_COLUMN_X_PADDING, dim_style, err_style,
    format_elapsed_compact, ok_style, panel_style, tool_header_style, turn_divider_label,
};

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
        pane::push_assistant_rows(&mut self.rows, text);
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
        if is_exploration_tool(call) {
            self.push_explored_result(call, duration);
            return;
        }
        let failed = exit_code.is_some_and(|code| code != 0);
        self.begin_block();
        if call.name == "bash" {
            self.push_shell_panel(call, content, false, failed, duration, None);
        } else {
            self.push_generic_tool_panel(call, content, false, failed, duration, None);
        }
    }

    fn push_tool_error(&mut self, call: &ToolCall, message: &str) {
        self.begin_block();
        if call.name == "bash" {
            self.push_shell_panel(call, "", false, true, None, Some(message));
        } else {
            self.push_generic_tool_panel(call, "", false, true, None, Some(message));
        }
    }

    fn push_tool_cancelled(&mut self, call: &ToolCall) {
        self.begin_block();
        if call.name == "bash" {
            let target = run_target(call);
            self.push_shell_header(PanelState::Cancelled, None, None, &target);
            self.push_panel_body(&format!("$ {target}"), panel_style());
        } else {
            self.push_generic_tool_header(call, PanelState::Cancelled, None, None);
        }
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
    }

    fn push_panel_body(&mut self, text: &str, style: Style) {
        self.push_panel_body_folded(text, style, FoldVis::Always);
    }

    fn push_panel_body_folded(&mut self, text: &str, style: Style, fold: FoldVis) {
        for line in text.split('\n') {
            let line = strip_ansi_for_text(line);
            self.rows.push(
                TranscriptRow::chrome_with_text(
                    ChromeRow::Body {
                        line: Line::from(Span::styled(line.clone(), style)),
                        bg: None,
                    },
                    line,
                    style,
                )
                .with_fold(fold),
            );
        }
    }

    /// Inner panel-body text width available for right-aligning fold hints.
    /// `wrap_width()` already equals the panel body width (terminal width minus
    /// outer padding and panel chrome), so the hint can hug the right border.
    fn fold_hint_width(&self) -> usize {
        self.wrap_width().max(1)
    }

    /// Preview-state fold affordance: `… N lines hidden        ctrl+o to expand`.
    /// Only rendered while the panel is collapsed (capped).
    fn push_fold_expand_hint(&mut self, hidden: usize, earlier: bool) {
        let noun = if earlier { "earlier lines" } else { "lines" };
        let left = format!("… {hidden} {noun} hidden");
        let text = right_align_hint(&left, "ctrl+o to expand", self.fold_hint_width());
        self.push_panel_body_folded(&text, dim_style(), FoldVis::WhenCollapsed);
    }

    /// Expanded-state fold affordance: right-aligned `ctrl+o to collapse`.
    /// Only rendered while the panel is fully revealed.
    fn push_fold_collapse_hint(&mut self) {
        let text = right_align_hint("", "ctrl+o to collapse", self.fold_hint_width());
        self.push_panel_body_folded(&text, dim_style(), FoldVis::WhenExpanded);
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

    pub(super) fn push_shell_panel(
        &mut self,
        call: &ToolCall,
        content: &str,
        running: bool,
        failed: bool,
        duration: Option<std::time::Duration>,
        error: Option<&str>,
    ) {
        let target = run_target(call);
        self.push_shell_header(panel_state(running, failed), duration, None, &target);
        self.push_panel_body(&format!("$ {target}"), panel_style());
        if !content.is_empty() {
            self.push_tool_output(content);
        } else if error.is_none() {
            self.push_panel_body("(no output)", dim_style());
        }
        if let Some(error) = error {
            self.push_panel_body(&format!("error: {error}"), err_style());
        }
        if running {
            self.push_panel_body("$ █", panel_style());
        }
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
    }

    fn push_generic_tool_header(
        &mut self,
        call: &ToolCall,
        state: PanelState,
        duration: Option<std::time::Duration>,
        started: Option<Instant>,
    ) {
        let meta = tool_panel_meta(call);
        self.push_panel_header(PanelHeaderSpec {
            title: tool_panel_title(call),
            meta: &meta,
            plain_meta: &meta,
            state,
            duration,
            started,
        });
    }

    fn push_generic_tool_panel(
        &mut self,
        call: &ToolCall,
        content: &str,
        running: bool,
        failed: bool,
        duration: Option<std::time::Duration>,
        error: Option<&str>,
    ) {
        self.push_generic_tool_header(call, panel_state(running, failed), duration, None);
        if !content.is_empty() {
            self.push_tool_output(content);
        } else if error.is_none() {
            self.push_panel_body("(no output)", dim_style());
        }
        if let Some(error) = error {
            self.push_panel_body(&format!("error: {error}"), err_style());
        }
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
    }

    /// Open a live exec block: a `• Running {target}` header under a fresh
    /// separator, tracked as the active cell so deltas and the final result
    /// finalize it in place.
    fn begin_exec(&mut self, call: ToolCall) {
        self.begin_block();
        let body_start = self.rows.len();
        let started = Instant::now();
        let target = run_target(&call);
        self.push_shell_header(PanelState::Running, None, Some(started), &target);
        self.push_panel_body(&format!("$ {target}"), panel_style());
        self.push_panel_body("$ █", panel_style());
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
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
        self.push_generic_tool_header(&call, PanelState::Running, None, Some(started));
        self.push_panel_body("running…", dim_style());
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
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

    fn finalized_tool_rows(
        &mut self,
        call: &ToolCall,
        content: &str,
        duration: Option<std::time::Duration>,
        started: Instant,
    ) -> Vec<TranscriptRow> {
        self.collect_rows(|this| {
            this.push_generic_tool_header(call, PanelState::Done, duration, Some(started));
            if !content.is_empty() {
                this.push_tool_output(content);
            } else {
                this.push_panel_body("(no output)", dim_style());
            }
            this.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        })
    }

    fn errored_tool_rows(
        &mut self,
        call: &ToolCall,
        message: &str,
        started: Instant,
    ) -> Vec<TranscriptRow> {
        self.collect_rows(|this| {
            this.push_generic_tool_header(call, PanelState::Error, None, Some(started));
            this.push_panel_body(&format!("error: {}", message), err_style());
            this.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        })
    }

    fn cancelled_tool_rows(&mut self, call: &ToolCall, started: Instant) -> Vec<TranscriptRow> {
        self.collect_rows(|this| {
            this.push_generic_tool_header(call, PanelState::Cancelled, None, Some(started));
            this.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        })
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
        let rows = self.finalized_tool_rows(call, content, duration, active.started);
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
        let rows = self.errored_tool_rows(call, message, active.started);
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
        let rows = self.cancelled_tool_rows(call, active.started);
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

    fn running_exec_rows(&mut self, active: &ActiveExec) -> Vec<TranscriptRow> {
        self.collect_rows(|this| {
            let target = run_target(&active.call);
            this.push_shell_header(PanelState::Running, None, Some(active.started), &target);
            this.push_panel_body(&format!("$ {target}"), panel_style());
            this.push_tool_output_tail(&active.output);
            this.push_panel_body("$ █", panel_style());
            this.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        })
    }

    fn finalized_exec_rows(
        &mut self,
        call: &ToolCall,
        content: &str,
        exit_code: Option<i32>,
        duration: Option<std::time::Duration>,
        started: Instant,
    ) -> Vec<TranscriptRow> {
        self.collect_rows(|this| {
            let target = run_target(call);
            let failed = exit_code.is_some_and(|code| code != 0);
            this.push_shell_header(panel_state(false, failed), duration, Some(started), &target);
            this.push_panel_body(&format!("$ {target}"), panel_style());
            this.push_tool_output(content);
            this.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        })
    }

    fn errored_exec_rows(
        &mut self,
        call: &ToolCall,
        message: &str,
        streamed_output: &str,
        started: Instant,
    ) -> Vec<TranscriptRow> {
        self.collect_rows(|this| {
            let target = run_target(call);
            this.push_shell_header(PanelState::Error, None, Some(started), &target);
            this.push_panel_body(&format!("$ {target}"), panel_style());
            if !streamed_output.is_empty() {
                this.push_tool_output_tail(streamed_output);
            }
            this.push_panel_body(&format!("error: {}", message), err_style());
            this.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        })
    }

    fn cancelled_exec_rows(
        &mut self,
        call: &ToolCall,
        streamed_output: &str,
        started: Instant,
    ) -> Vec<TranscriptRow> {
        self.collect_rows(|this| {
            let target = run_target(call);
            this.push_shell_header(PanelState::Cancelled, None, Some(started), &target);
            this.push_panel_body(&format!("$ {target}"), panel_style());
            if !streamed_output.is_empty() {
                this.push_tool_output_tail(streamed_output);
            }
            this.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        })
    }

    /// Re-render the open exec block in place from its bounded output buffer: the
    /// `Running` header followed by the flood-capped live tail.
    fn relayout_active_running(&mut self) {
        let Some(active) = self.active_exec.take() else {
            return;
        };
        let rows = self.running_exec_rows(&active);
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
        let rows = self.finalized_exec_rows(call, content, exit_code, duration, active.started);
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
        let rows = self.errored_exec_rows(call, message, &active.output, active.started);
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
        let rows = self.cancelled_exec_rows(call, &active.output, active.started);
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

    /// Push one gutter-prefixed tool-output line, preserving ANSI styling and
    /// hard-wrapping so leading indentation/aligned columns survive. `first`
    /// selects the `  └ ` head gutter vs the `    ` continuation gutter.
    fn push_output_line(&mut self, raw: &str, first: bool, fold: FoldVis) {
        let line = truncate_chars(raw, MAX_TOOL_OUTPUT_LINE_CHARS);
        let legacy = if first {
            format!("  └ {line}")
        } else {
            format!("    {line}")
        };
        if line.contains("\x1b[") {
            self.rows.push(
                TranscriptRow::chrome_with_text(
                    ChromeRow::Body {
                        line: tool_output_line("", &line),
                        bg: None,
                    },
                    strip_ansi_for_text(&legacy),
                    dim_style(),
                )
                .with_fold(fold),
            );
        } else {
            self.rows.push(
                TranscriptRow::chrome_with_text(
                    ChromeRow::Body {
                        line: Line::from(Span::styled(line.to_string(), dim_style())),
                        bg: None,
                    },
                    legacy,
                    dim_style(),
                )
                .with_fold(fold),
            );
        }
    }

    fn push_tool_output(&mut self, content: &str) {
        if content.is_empty() {
            self.push_panel_body("(no output)", dim_style());
            return;
        }
        // Flood-safe AND compact (Codex parity): wrap each line to the transcript
        // width FIRST, then keep a head slice and a tail slice that together fit
        // the physical-row budget, with a `… +N lines` marker between. Showing
        // the tail keeps a command's final/summary line visible instead of only
        // its head. The omitted count is logical lines. The live cell still uses
        // the tail-only variant while output is still growing.
        let width = self.wrap_width();
        let lines: Vec<&str> = content.lines().collect();
        let cost = |raw: &str| {
            wrapped_row_estimate(&truncate_chars(raw, MAX_TOOL_OUTPUT_LINE_CHARS), width)
        };
        let total_rows: usize = lines.iter().map(|raw| cost(raw)).sum();
        if total_rows <= MAX_TOOL_OUTPUT_ROWS {
            for (i, raw) in lines.iter().enumerate() {
                self.push_output_line(raw, i == 0, FoldVis::Always);
            }
            return;
        }
        // One row is reserved for the ellipsis marker; the rest splits in half.
        let budget = MAX_TOOL_OUTPUT_ROWS.saturating_sub(1).max(1);
        let head_budget = budget / 2;
        let tail_budget = budget - head_budget;
        let mut head_rows = 0usize;
        let mut head_end = 0usize;
        // Always keep at least the first line so a single over-budget line never
        // collapses the cell to just a marker (and so the head gutter is always
        // emitted); only later lines are gated on the head budget.
        while head_end < lines.len() {
            let rows = cost(lines[head_end]);
            if head_end > 0 && head_rows + rows > head_budget {
                break;
            }
            head_rows += rows;
            head_end += 1;
        }
        let mut tail_rows = 0usize;
        let mut tail_start = lines.len();
        while tail_start > head_end {
            let rows = cost(lines[tail_start - 1]);
            if tail_rows + rows > tail_budget {
                break;
            }
            tail_rows += rows;
            tail_start -= 1;
        }
        let hidden = tail_start.saturating_sub(head_end);
        if hidden == 0 {
            // Nothing elided (e.g. one over-budget line): keep the original
            // clamped head/tail slices so a single huge line cannot blow the
            // row cap, and stay non-foldable.
            for (i, raw) in lines[..head_end].iter().enumerate() {
                let clamped = clamp_output_line(raw, width, head_budget);
                self.push_output_line(&clamped, i == 0, FoldVis::Always);
            }
            for raw in &lines[tail_start..] {
                let clamped = clamp_output_line(raw, width, tail_budget);
                self.push_output_line(&clamped, false, FoldVis::Always);
            }
            return;
        }
        // Preview set (shown while collapsed): clamped head slice, the hidden
        // affordance, then the clamped tail slice.
        for (i, raw) in lines[..head_end].iter().enumerate() {
            let clamped = clamp_output_line(raw, width, head_budget);
            self.push_output_line(&clamped, i == 0, FoldVis::WhenCollapsed);
        }
        self.push_fold_expand_hint(hidden, false);
        for raw in &lines[tail_start..] {
            let clamped = clamp_output_line(raw, width, tail_budget);
            self.push_output_line(&clamped, false, FoldVis::WhenCollapsed);
        }
        // Full set (shown while expanded): every line, then the collapse hint.
        for (i, raw) in lines.iter().enumerate() {
            self.push_output_line(raw, i == 0, FoldVis::WhenExpanded);
        }
        self.push_fold_collapse_hint();
        self.mark_panel_preview();
    }

    /// TAIL-capped tool output for the LIVE streaming cell: show the most recent
    /// physical rows so a growing stream scrolls instead of freezing on its
    /// head, with a leading `… +N earlier lines` note when output was dropped.
    fn push_tool_output_tail(&mut self, content: &str) {
        if content.is_empty() {
            self.push_panel_body("(no output)", dim_style());
            return;
        }
        let width = self.wrap_width();
        let lines: Vec<&str> = content.lines().collect();
        // Walk from the end, accumulating physical rows until the budget, so the
        // newest output is what stays visible.
        let mut physical = 0usize;
        let mut take = 0usize;
        for raw in lines.iter().rev() {
            let rows =
                wrapped_row_estimate(&truncate_chars(raw, MAX_TOOL_OUTPUT_LINE_CHARS), width);
            if take > 0 && physical + rows > MAX_TOOL_OUTPUT_ROWS {
                break;
            }
            physical += rows;
            take += 1;
        }
        let start = lines.len() - take;
        if start == 0 {
            for (offset, raw) in lines.iter().enumerate() {
                let clamped = clamp_output_line(raw, width, MAX_TOOL_OUTPUT_ROWS);
                self.push_output_line(&clamped, offset == 0, FoldVis::Always);
            }
            return;
        }
        // Preview set: the earlier-lines affordance, then the most recent tail.
        self.push_fold_expand_hint(start, true);
        for raw in &lines[start..] {
            let clamped = clamp_output_line(raw, width, MAX_TOOL_OUTPUT_ROWS);
            self.push_output_line(&clamped, false, FoldVis::WhenCollapsed);
        }
        // Full set: every line in order (unclamped, matching the finalized
        // path so revealing a streamed run shows the same rows), then the
        // collapse hint.
        for (offset, raw) in lines.iter().enumerate() {
            self.push_output_line(raw, offset == 0, FoldVis::WhenExpanded);
        }
        self.push_fold_collapse_hint();
        self.mark_panel_preview();
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
            _ => (explore_panel_meta(call), true),
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
        if self.exploring_open {
            self.pop_trailing_explore_bottom();
            self.set_explore_header(call, panel_state(false, failed), duration);
        } else {
            self.push_blank();
            self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
                expanded: true,
                title: "EXPLORE",
                meta: explore_panel_meta(call),
                right: Self::explore_header_right(panel_state(false, failed), duration),
            }));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
        }
        let text = explore_body(call);
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Body {
                line: Line::from(Span::styled(
                    text.clone(),
                    if failed { err_style() } else { dim_style() },
                )),
                bg: None,
            },
            text,
            if failed { err_style() } else { dim_style() },
        ));
        self.rows.push(TranscriptRow::chrome(ChromeRow::Bottom));
        self.exploring_open = true;
    }

    fn push_explored_result(&mut self, call: &ToolCall, duration: Option<Duration>) {
        self.finish_stream();
        if self.finish_exploration(
            call,
            explore_body(call),
            dim_style(),
            duration,
            false,
            false,
        ) {
            return;
        }
        self.push_explore_body(call, false, duration);
    }

    fn push_explored_start(&mut self, call: &ToolCall) {
        self.finish_stream();
        let started = Instant::now();
        if self.exploring_open {
            self.pop_trailing_explore_bottom();
        } else {
            self.push_blank();
            self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
                expanded: true,
                title: "EXPLORE",
                meta: explore_panel_meta(call),
                right: Self::explore_header_right(PanelState::Running, Some(Duration::ZERO)),
            }));
            self.rows.push(TranscriptRow::chrome(ChromeRow::Separator));
        }
        let row = self.rows.len();
        let text = explore_body(call);
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Body {
                line: Line::from(Span::styled(text.clone(), dim_style())),
                bg: None,
            },
            text,
            dim_style(),
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
        self.finish_exploration(
            call,
            format!("error: {}", message),
            err_style(),
            None,
            true,
            false,
        )
    }

    fn push_explored_cancelled(&mut self, call: &ToolCall) -> bool {
        self.finish_stream();
        self.finish_exploration(call, explore_body(call), dim_style(), None, false, true)
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
            UiEvent::SessionStarted => {
                self.finish_stream();
            }
            UiEvent::ToolProposed(_) => {
                // Non-gated tools show only their result row; nothing to render.
                self.finish_stream();
            }
            UiEvent::ToolStarted(call) => {
                if is_exploration_tool(&call) {
                    self.push_explored_start(&call);
                } else if call.name == "bash" {
                    self.begin_exec(call);
                } else {
                    self.begin_tool(call);
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
            UiEvent::ToolAutoApproved(call) => {
                self.record_approval(&call, ApprovalDecision::AllowAlways);
            }
            UiEvent::DiffPreview { call, diff } => {
                self.clear_active_tool_for_preview(&call);
                self.begin_block();
                self.rows.push(TranscriptRow::chrome(ChromeRow::Top));
                self.rows.push(TranscriptRow::chrome(ChromeRow::Header {
                    expanded: true,
                    title: tool_panel_title(&call),
                    meta: tool_panel_meta(&call),
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
                } else if !is_exploration_tool(&call) || !self.push_explored_error(&call, &message)
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
                } else if !is_exploration_tool(&call) || !self.push_explored_cancelled(&call) {
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
            .map(|text| pane::streaming_assistant_rows(text))
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

/// Right-align `hint` within `width`, after optional `left` text. Used for the
/// fold affordances so the keybinding hint hugs the panel's right edge.
fn right_align_hint(left: &str, hint: &str, width: usize) -> String {
    let hint_w = display_width(hint);
    let left_w = display_width(left);
    // Too narrow for both: keep the actionable hint, right-aligned, dropping the
    // descriptive left text rather than overflowing and wrapping the row.
    if left.is_empty() || left_w + 1 + hint_w > width {
        return format!("{}{hint}", " ".repeat(width.saturating_sub(hint_w)));
    }
    let gap = width.saturating_sub(left_w).saturating_sub(hint_w).max(1);
    format!("{left}{}{hint}", " ".repeat(gap))
}

fn tool_output_line(prefix: &'static str, line: &str) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix, dim_style())];
    spans.extend(ansi_spans(line, dim_style()));
    Line::from(spans)
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
