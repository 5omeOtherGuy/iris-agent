//! Built-in tool-renderer registry: the single source for the tool-name -> panel
//! mapping on the TUI surface.
//!
//! This replaces the scattered taxonomy (`tool_panel_title` EXPLORE/EDIT/TOOL,
//! `is_exploration_tool`, and `call.name == "bash"` checks) with one
//! [`ToolRenderer`] trait + [`resolve`] registry. Each renderer produces Iris's
//! existing panel primitives:
//!
//! * a panel [`title`](ToolRenderer::title) (the renderCall header label),
//! * header [`meta`](ToolRenderer::header_meta) / [`plain_meta`](ToolRenderer::plain_meta),
//! * and result [`body`](ToolRenderer::body) rows (the renderResult equivalent),
//!
//! plus a [`kind`](ToolRenderer::kind) that selects the stateful dispatch path in
//! [`super::transcript`] (the renderShell `default`/`self` distinction).
//!
//! Conceptual reference: pi-mono `ToolDefinition` (renderCall/renderResult/
//! renderShell) and `ToolExecutionComponent` (registry fallback + try/catch).
//! Iris reimplements the trait shape and fallback discipline idiomatically; it
//! does NOT adopt pi-mono's `Component` return type, its long-lived
//! `rendererState` object (Iris keeps cross-frame state in transcript rows), or
//! any extension/registration API (deferred).
//!
//! Renderers are pure functions over `(ctx, call, outcome)`. Presentation
//! summaries are reused from [`crate::tool_display`]; they are never re-derived
//! here.

use std::panic::{AssertUnwindSafe, catch_unwind};

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::nexus::ToolCall;
use crate::tool_display::{display_path, exploration_summary, run_target, summarize};

use super::rows::{ChromeRow, FoldVis, TranscriptRow};
use super::text::{ansi_spans, strip_ansi_for_text};
use super::wrap::{clamp_output_line, display_width, truncate_chars, wrapped_row_estimate};
use super::{MAX_TOOL_OUTPUT_LINE_CHARS, MAX_TOOL_OUTPUT_ROWS, dim_style, err_style, panel_style};

/// The panel family a tool renders as. Selects the stateful dispatch path in
/// [`super::transcript`] (grouped EXPLORE, streaming SHELL exec cell, or a
/// standard GENERIC/EDIT panel). Mirrors pi-mono's renderShell `self`/`default`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolPanelKind {
    /// read/grep/find/ls: grouped, single-line "Explored" rows.
    Explore,
    /// bash: self-framing shell panel with a `$ command` row and live output.
    Shell,
    /// write/edit and unknown tools: standard tool panel body.
    Generic,
}

/// Shared, immutable context handed to a renderer. Width matches the
/// transcript's wrap width so flood-capping and the hidden-content affordance
/// size identically to the rest of the panel body.
pub(super) struct RenderCtx {
    pub(super) width: usize,
}

/// The result state a renderer is asked to render a body for. The header state
/// (running/done/error/cancelled dot + duration) is owned by the transcript
/// lifecycle and passed separately; this drives only the body rows.
pub(super) enum ToolOutcome<'a> {
    /// Live cell: `streamed` is the bounded output tail so far (may be empty).
    Running { streamed: &'a str },
    /// Finalized success. `content` is the authoritative output (may be empty).
    Done { content: &'a str },
    /// Failure. `streamed` is partial output captured before the error.
    Error { message: &'a str, streamed: &'a str },
    /// Cancelled. `streamed` is whatever streamed before cancellation.
    Cancelled { streamed: &'a str },
}

/// A built-in tool renderer. Adopted from pi-mono's `ToolDefinition` render
/// hooks, reimplemented over Iris's panel primitives.
pub(super) trait ToolRenderer: Sync {
    /// Panel family; selects the transcript dispatch path.
    fn kind(&self) -> ToolPanelKind;

    /// Header title label (e.g. `EXPLORE`, `SHELL`, `EDIT`, `TOOL`).
    fn title(&self) -> &'static str;

    /// Header meta shown next to the title (the colored display string).
    fn header_meta(&self, call: &ToolCall) -> String;

    /// Plain-text mirror of the header meta for the accessible/text path.
    /// Defaults to [`header_meta`](ToolRenderer::header_meta); SHELL overrides it
    /// with the run target.
    fn plain_meta(&self, call: &ToolCall) -> String {
        self.header_meta(call)
    }

    /// Result body rows, rendered between the header separator and the bottom
    /// border. EXPLORE returns exactly one row (its grouped summary line).
    fn body(&self, ctx: &RenderCtx, call: &ToolCall, outcome: &ToolOutcome) -> Vec<TranscriptRow>;
}

// --- Built-in renderers -----------------------------------------------------

/// read/grep/find/ls -> grouped EXPLORE panel.
struct ExploreRenderer;
/// bash -> self-framing SHELL panel.
struct ShellRenderer;
/// write/edit -> EDIT panel (standard body, `EDIT` title).
struct EditRenderer;
/// Unknown tools -> generic TOOL fallback panel.
struct GenericRenderer;

static EXPLORE: ExploreRenderer = ExploreRenderer;
static SHELL: ShellRenderer = ShellRenderer;
static EDIT: EditRenderer = EditRenderer;
static GENERIC: GenericRenderer = GenericRenderer;

/// The single source of the tool-name -> renderer map for the TUI. Unknown
/// names fall back to the generic TOOL renderer (mirrors pi-mono's
/// `getResultRenderer` built-in fallback).
pub(super) fn resolve(call: &ToolCall) -> &'static dyn ToolRenderer {
    match call.name.as_str() {
        "read" | "grep" | "find" | "ls" => &EXPLORE,
        "bash" => &SHELL,
        "write" | "edit" => &EDIT,
        _ => &GENERIC,
    }
}

/// The path argument a file-style tool carries, if any.
fn tool_path_arg(call: &ToolCall) -> Option<&str> {
    call.arguments
        .get("file_path")
        .or_else(|| call.arguments.get("path"))
        .and_then(|value| value.as_str())
}

impl ToolRenderer for ExploreRenderer {
    fn kind(&self) -> ToolPanelKind {
        ToolPanelKind::Explore
    }

    fn title(&self) -> &'static str {
        "EXPLORE"
    }

    fn header_meta(&self, call: &ToolCall) -> String {
        tool_path_arg(call)
            .map(display_path)
            .unwrap_or_else(|| "workspace".to_string())
    }

    fn body(&self, _ctx: &RenderCtx, call: &ToolCall, outcome: &ToolOutcome) -> Vec<TranscriptRow> {
        // The grouped EXPLORE panel replaces a single stored row in place, so
        // this renderer is constrained to exactly one row.
        let (text, style) = match outcome {
            ToolOutcome::Error { message, .. } => (format!("error: {message}"), err_style()),
            _ => (exploration_summary(call), dim_style()),
        };
        vec![explore_row(text, style)]
    }
}

/// Build the single EXPLORE body row exactly as the transcript grouping path
/// stores it (a `Body` chrome row carrying both the styled line and its plain
/// text mirror).
fn explore_row(text: String, style: Style) -> TranscriptRow {
    TranscriptRow::chrome_with_text(
        ChromeRow::Body {
            line: Line::from(Span::styled(text.clone(), style)),
            bg: None,
        },
        text,
        style,
    )
}

impl ToolRenderer for ShellRenderer {
    fn kind(&self) -> ToolPanelKind {
        ToolPanelKind::Shell
    }

    fn title(&self) -> &'static str {
        "SHELL"
    }

    fn header_meta(&self, _call: &ToolCall) -> String {
        "bash".to_string()
    }

    fn plain_meta(&self, call: &ToolCall) -> String {
        run_target(call)
    }

    fn body(&self, ctx: &RenderCtx, call: &ToolCall, outcome: &ToolOutcome) -> Vec<TranscriptRow> {
        let mut body = PanelBody::new(ctx.width);
        let target = run_target(call);
        body.line(&format!("$ {target}"), panel_style());
        match outcome {
            ToolOutcome::Running { streamed } => {
                if !streamed.is_empty() {
                    body.output_tail(streamed);
                }
                body.line("$ \u{2588}", panel_style());
            }
            ToolOutcome::Done { content } => {
                body.output(content);
            }
            ToolOutcome::Error { message, streamed } => {
                if !streamed.is_empty() {
                    body.output_tail(streamed);
                }
                body.line(&format!("error: {message}"), err_style());
            }
            ToolOutcome::Cancelled { streamed } => {
                if !streamed.is_empty() {
                    body.output_tail(streamed);
                }
            }
        }
        body.into_rows()
    }
}

/// Body shared by EDIT and the generic TOOL fallback (identical apart from the
/// header title).
fn generic_body(ctx: &RenderCtx, outcome: &ToolOutcome) -> Vec<TranscriptRow> {
    let mut body = PanelBody::new(ctx.width);
    match outcome {
        ToolOutcome::Running { .. } => body.line("running\u{2026}", dim_style()),
        ToolOutcome::Done { content } => body.output(content),
        ToolOutcome::Error { message, .. } => {
            body.line(&format!("error: {message}"), err_style());
        }
        // Cancelled generic/edit panels are header-only (no body rows).
        ToolOutcome::Cancelled { .. } => {}
    }
    body.into_rows()
}

impl ToolRenderer for EditRenderer {
    fn kind(&self) -> ToolPanelKind {
        ToolPanelKind::Generic
    }

    fn title(&self) -> &'static str {
        "EDIT"
    }

    fn header_meta(&self, call: &ToolCall) -> String {
        generic_meta(call)
    }

    fn body(&self, ctx: &RenderCtx, _call: &ToolCall, outcome: &ToolOutcome) -> Vec<TranscriptRow> {
        generic_body(ctx, outcome)
    }
}

impl ToolRenderer for GenericRenderer {
    fn kind(&self) -> ToolPanelKind {
        ToolPanelKind::Generic
    }

    fn title(&self) -> &'static str {
        "TOOL"
    }

    fn header_meta(&self, call: &ToolCall) -> String {
        generic_meta(call)
    }

    fn body(&self, ctx: &RenderCtx, _call: &ToolCall, outcome: &ToolOutcome) -> Vec<TranscriptRow> {
        generic_body(ctx, outcome)
    }
}

/// Header meta for EDIT/generic panels: the file path when present, else the
/// one-line tool summary.
fn generic_meta(call: &ToolCall) -> String {
    tool_path_arg(call)
        .map(display_path)
        .unwrap_or_else(|| summarize(call))
}

// --- Failure-isolating dispatch ---------------------------------------------

/// Render a renderer's body with failure isolation: a panicking renderer falls
/// back to the generic TOOL body instead of crashing the TUI or corrupting the
/// panel. The renderer returns a freshly-built `Vec`, so a panic discards a
/// partial body cleanly. This is the seam the deferred extension phase relies
/// on; built-in renderers never panic.
///
/// The generic fallback is itself wrapped, so a double fault (a bug in the
/// shared generic body or `PanelBody`) still degrades to a single visible
/// error row rather than unwinding into the caller, which has already pushed
/// the panel header and would otherwise leave an unterminated panel.
pub(super) fn render_body(
    renderer: &dyn ToolRenderer,
    ctx: &RenderCtx,
    call: &ToolCall,
    outcome: &ToolOutcome,
) -> Vec<TranscriptRow> {
    match catch_unwind(AssertUnwindSafe(|| renderer.body(ctx, call, outcome))) {
        Ok(rows) => rows,
        Err(_) => match catch_unwind(AssertUnwindSafe(|| GENERIC.body(ctx, call, outcome))) {
            Ok(rows) => rows,
            Err(_) => vec![explore_row("(render error)".to_string(), err_style())],
        },
    }
}

// --- Panel body builder -----------------------------------------------------

/// Free helper used by the gutter output lines: a dim, ANSI-preserving line with
/// an optional leading prefix span.
fn tool_output_line(prefix: &'static str, line: &str) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix, dim_style())];
    spans.extend(ansi_spans(line, dim_style()));
    Line::from(spans)
}

/// A short-lived builder for tool-panel body rows. Owns the flood-cap and
/// hidden-content logic so renderers and the transcript's thin wrappers share
/// one implementation (no duplicated summary/flood logic). `width` is the
/// transcript wrap width.
struct PanelBody {
    width: usize,
    rows: Vec<TranscriptRow>,
}

impl PanelBody {
    fn new(width: usize) -> Self {
        Self {
            width,
            rows: Vec::new(),
        }
    }

    fn into_rows(self) -> Vec<TranscriptRow> {
        self.rows
    }

    /// Push a plain panel body line (ANSI stripped), one row per `\n` segment.
    fn line(&mut self, text: &str, style: Style) {
        self.line_folded(text, style, FoldVis::Always);
    }

    fn line_folded(&mut self, text: &str, style: Style, fold: FoldVis) {
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

    /// Inner panel-body width available for right-aligning fold hints. Matches
    /// the transcript's `fold_hint_width` (the wrap width already equals the
    /// panel body width), so the hint hugs the right border.
    fn fold_hint_width(&self) -> usize {
        self.width.max(1)
    }

    /// Preview-state fold affordance: `\u{2026} N lines hidden    ctrl+o to expand`.
    fn fold_expand_hint(&mut self, hidden: usize, earlier: bool) {
        let noun = if earlier { "earlier lines" } else { "lines" };
        let left = format!("\u{2026} {hidden} {noun} hidden");
        let text = right_align_hint(&left, "ctrl+o to expand", self.fold_hint_width());
        self.line_folded(&text, dim_style(), FoldVis::WhenCollapsed);
    }

    /// Expanded-state fold affordance: right-aligned `ctrl+o to collapse`.
    fn fold_collapse_hint(&mut self) {
        let text = right_align_hint("", "ctrl+o to collapse", self.fold_hint_width());
        self.line_folded(&text, dim_style(), FoldVis::WhenExpanded);
    }

    /// Push one gutter-prefixed tool-output line, preserving ANSI styling and
    /// hard-wrapping so leading indentation/aligned columns survive. `first`
    /// selects the `  \u{2514} ` head gutter vs the `    ` continuation gutter.
    fn output_line(&mut self, raw: &str, first: bool, fold: FoldVis) {
        let line = truncate_chars(raw, MAX_TOOL_OUTPUT_LINE_CHARS);
        let legacy = if first {
            format!("  \u{2514} {line}")
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

    /// Flood-safe AND compact tool output: wrap each line to the transcript
    /// width FIRST, then keep a head slice and a tail slice that together fit
    /// the physical-row budget. When output is elided, emit a collapsed preview
    /// (`WhenCollapsed`) plus the full output (`WhenExpanded`) so ctrl+o can
    /// toggle between them; otherwise rows stay `Always`.
    fn output(&mut self, content: &str) {
        if content.is_empty() {
            self.line("(no output)", dim_style());
            return;
        }
        let width = self.width;
        let lines: Vec<&str> = content.lines().collect();
        let cost = |raw: &str| {
            wrapped_row_estimate(&truncate_chars(raw, MAX_TOOL_OUTPUT_LINE_CHARS), width)
        };
        let total_rows: usize = lines.iter().map(|raw| cost(raw)).sum();
        if total_rows <= MAX_TOOL_OUTPUT_ROWS {
            for (i, raw) in lines.iter().enumerate() {
                self.output_line(raw, i == 0, FoldVis::Always);
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
                self.output_line(&clamped, i == 0, FoldVis::Always);
            }
            for raw in &lines[tail_start..] {
                let clamped = clamp_output_line(raw, width, tail_budget);
                self.output_line(&clamped, false, FoldVis::Always);
            }
            return;
        }
        // Preview set (shown while collapsed): clamped head slice, the hidden
        // affordance, then the clamped tail slice.
        for (i, raw) in lines[..head_end].iter().enumerate() {
            let clamped = clamp_output_line(raw, width, head_budget);
            self.output_line(&clamped, i == 0, FoldVis::WhenCollapsed);
        }
        self.fold_expand_hint(hidden, false);
        for raw in &lines[tail_start..] {
            let clamped = clamp_output_line(raw, width, tail_budget);
            self.output_line(&clamped, false, FoldVis::WhenCollapsed);
        }
        // Full set (shown while expanded): every line, then the collapse hint.
        for (i, raw) in lines.iter().enumerate() {
            self.output_line(raw, i == 0, FoldVis::WhenExpanded);
        }
        self.fold_collapse_hint();
    }

    /// TAIL-capped tool output for the LIVE streaming cell: show the most recent
    /// physical rows so a growing stream scrolls instead of freezing on its
    /// head. When output dropped, emit a collapsed preview (earlier-lines note +
    /// recent tail) plus the full output, matching the finalized fold pair.
    fn output_tail(&mut self, content: &str) {
        if content.is_empty() {
            self.line("(no output)", dim_style());
            return;
        }
        let width = self.width;
        let lines: Vec<&str> = content.lines().collect();
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
                self.output_line(&clamped, offset == 0, FoldVis::Always);
            }
            return;
        }
        // Preview set: the earlier-lines affordance, then the most recent tail.
        self.fold_expand_hint(start, true);
        for raw in &lines[start..] {
            let clamped = clamp_output_line(raw, width, MAX_TOOL_OUTPUT_ROWS);
            self.output_line(&clamped, false, FoldVis::WhenCollapsed);
        }
        // Full set: every line in order (unclamped, matching the finalized path
        // so revealing a streamed run shows the same rows), then the hint.
        for (offset, raw) in lines.iter().enumerate() {
            self.output_line(raw, offset == 0, FoldVis::WhenExpanded);
        }
        self.fold_collapse_hint();
    }
}

/// Pad `left` so `hint` hugs the right edge within `width`; drop `left` when too
/// narrow rather than overflowing/wrapping the row. Mirrors the transcript
/// helper so fold affordances render identically.
fn right_align_hint(left: &str, hint: &str, width: usize) -> String {
    let hint_w = display_width(hint);
    let left_w = display_width(left);
    if left.is_empty() || left_w + 1 + hint_w > width {
        return format!("{}{hint}", " ".repeat(width.saturating_sub(hint_w)));
    }
    let gap = width.saturating_sub(left_w).saturating_sub(hint_w).max(1);
    format!("{left}{}{hint}", " ".repeat(gap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
            name: name.to_string(),
            arguments,
        }
    }

    #[test]
    fn registry_maps_builtin_tool_names_to_kinds_and_titles() {
        let cases = [
            ("read", ToolPanelKind::Explore, "EXPLORE"),
            ("grep", ToolPanelKind::Explore, "EXPLORE"),
            ("find", ToolPanelKind::Explore, "EXPLORE"),
            ("ls", ToolPanelKind::Explore, "EXPLORE"),
            ("bash", ToolPanelKind::Shell, "SHELL"),
            ("write", ToolPanelKind::Generic, "EDIT"),
            ("edit", ToolPanelKind::Generic, "EDIT"),
        ];
        for (name, kind, title) in cases {
            let renderer = resolve(&call(name, json!({})));
            assert!(renderer.kind() == kind, "{name} kind");
            assert_eq!(renderer.title(), title, "{name} title");
        }
    }

    #[test]
    fn unknown_tool_resolves_to_generic_fallback() {
        let renderer = resolve(&call("totally_unknown", json!({})));
        assert!(renderer.kind() == ToolPanelKind::Generic);
        assert_eq!(renderer.title(), "TOOL");
    }

    #[test]
    fn shell_meta_is_constant_but_plain_meta_is_run_target() {
        let renderer = resolve(&call("bash", json!({ "command": "echo hi" })));
        assert_eq!(renderer.header_meta(&call("bash", json!({}))), "bash");
        assert_eq!(
            renderer.plain_meta(&call("bash", json!({ "command": "echo hi" }))),
            "echo hi"
        );
    }

    #[test]
    fn generic_meta_prefers_path_then_summary() {
        let edit = resolve(&call("edit", json!({ "file_path": "src/x.rs" })));
        assert_eq!(
            edit.header_meta(&call("edit", json!({ "file_path": "src/x.rs" }))),
            "src/x.rs"
        );
        let unknown = resolve(&call("zonk", json!({ "a": 1 })));
        assert!(
            unknown
                .header_meta(&call("zonk", json!({ "a": 1 })))
                .starts_with("zonk ")
        );
    }

    #[test]
    fn explore_body_is_one_row_summary_or_error() {
        let renderer = resolve(&call("grep", json!({ "pattern": "needle", "path": "src" })));
        let ctx = RenderCtx { width: 80 };
        let done = renderer.body(
            &ctx,
            &call("grep", json!({ "pattern": "needle", "path": "src" })),
            &ToolOutcome::Done { content: "" },
        );
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].text, "Search needle in src");
        let errored = renderer.body(
            &ctx,
            &call("grep", json!({ "pattern": "needle", "path": "src" })),
            &ToolOutcome::Error {
                message: "boom",
                streamed: "",
            },
        );
        assert_eq!(errored.len(), 1);
        assert_eq!(errored[0].text, "error: boom");
        assert_eq!(errored[0].style.fg, err_style().fg);
    }

    #[test]
    fn shell_running_skips_no_output_when_stream_empty() {
        let renderer = resolve(&call("bash", json!({ "command": "echo hi" })));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("bash", json!({ "command": "echo hi" })),
            &ToolOutcome::Running { streamed: "" },
        );
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["$ echo hi", "$ \u{2588}"]);
    }

    #[test]
    fn generic_cancelled_has_no_body_rows() {
        let renderer = resolve(&call("zonk", json!({})));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("zonk", json!({})),
            &ToolOutcome::Cancelled { streamed: "" },
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn shell_error_renders_streamed_tail_then_error_line() {
        let renderer = resolve(&call("bash", json!({ "command": "make" })));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("bash", json!({ "command": "make" })),
            &ToolOutcome::Error {
                message: "exit 2",
                streamed: "compiling\nlinking",
            },
        );
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        // Command row, then the streamed output tail, then the error line last.
        assert_eq!(texts.first(), Some(&"$ make"));
        assert_eq!(texts.last(), Some(&"error: exit 2"));
        assert_eq!(rows.last().unwrap().style.fg, err_style().fg);
        assert!(
            texts.iter().any(|t| t.contains("linking")),
            "expected streamed tail before error, got {texts:?}"
        );
    }

    #[test]
    fn generic_done_output_capped_into_collapsed_preview_and_expanded_full() {
        // Far more logical lines than the physical-row budget forces a foldable
        // body: a capped preview (WhenCollapsed) with a hidden marker, plus the
        // full output (WhenExpanded) that ctrl+o reveals.
        let content = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let renderer = resolve(&call("zonk", json!({})));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("zonk", json!({})),
            &ToolOutcome::Done { content: &content },
        );
        // The collapsed-visible set (Always + WhenCollapsed) stays within the
        // physical-row budget plus the single hidden-affordance row.
        let collapsed_visible = rows
            .iter()
            .filter(|r| r.fold != FoldVis::WhenExpanded)
            .count();
        assert!(
            collapsed_visible <= MAX_TOOL_OUTPUT_ROWS + 1,
            "collapsed-visible {collapsed_visible} exceeds budget {}",
            MAX_TOOL_OUTPUT_ROWS + 1
        );
        assert!(
            rows.iter()
                .any(|r| r.fold == FoldVis::WhenCollapsed && r.text.contains("hidden")),
            "expected a collapsed hidden-content marker"
        );
        assert!(
            rows.iter().any(|r| r.fold == FoldVis::WhenExpanded),
            "expected the full expanded output set"
        );
        assert!(
            rows.iter()
                .any(|r| r.fold == FoldVis::WhenExpanded && r.text.contains("line 199")),
            "expected the full set to include the elided tail"
        );
    }

    #[test]
    fn output_preserves_ansi_color_spans() {
        let renderer = resolve(&call("zonk", json!({})));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("zonk", json!({})),
            &ToolOutcome::Done {
                content: "\x1b[31mred\x1b[0m",
            },
        );
        let Some(ChromeRow::Body { line, .. }) = rows[0].chrome.as_ref() else {
            panic!("expected a Body chrome row");
        };
        assert!(
            line.spans
                .iter()
                .any(|span| span.style.fg == Some(ratatui::style::Color::Red)),
            "expected a red span from the ANSI escape"
        );
    }

    struct FailingRenderer;
    impl ToolRenderer for FailingRenderer {
        fn kind(&self) -> ToolPanelKind {
            ToolPanelKind::Generic
        }
        fn title(&self) -> &'static str {
            "BOOM"
        }
        fn header_meta(&self, _call: &ToolCall) -> String {
            "boom".to_string()
        }
        fn body(
            &self,
            _ctx: &RenderCtx,
            _call: &ToolCall,
            _outcome: &ToolOutcome,
        ) -> Vec<TranscriptRow> {
            panic!("renderer exploded");
        }
    }

    #[test]
    fn render_body_isolates_a_panicking_renderer_and_falls_back_to_generic() {
        // Silence the panic backtrace the default hook would print.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let ctx = RenderCtx { width: 80 };
        let c = call("anything", json!({}));
        let rows = render_body(
            &FailingRenderer,
            &ctx,
            &c,
            &ToolOutcome::Done {
                content: "hello world",
            },
        );
        std::panic::set_hook(prev);
        // Fallback is the generic body: the output line, not a crash.
        assert!(
            rows.iter().any(|r| r.text.contains("hello world")),
            "expected generic fallback body, got {:?}",
            rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>()
        );
    }
}
