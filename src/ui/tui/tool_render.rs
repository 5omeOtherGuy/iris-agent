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
use crate::tool_display::{
    bash_timeout_secs, command_display, display_path, exploration_summary, run_target, summarize,
};
use crate::ui::symbols;

use super::rows::{ChromeRow, FoldVis, TranscriptRow};
use super::shell_command::{self, ShellCommand};
use super::text::{ansi_spans, strip_ansi_for_text};
use super::wrap::{clamp_output_line, display_width, truncate_chars, wrapped_row_estimate};
use super::{
    MAX_TOOL_OUTPUT_LINE_CHARS, MAX_TOOL_OUTPUT_ROWS, dim_style, err_style, ok_style, panel_style,
};

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
    /// `exit_code` is the process's exit status when one is known (e.g. a
    /// shell command's exit code). SHELL is the only renderer that reads it
    /// (for its closing result row); `None` means no status was reported at
    /// all (a cancelled run, a timeout, or an exited session shell, per
    /// `src/tools/bash/mod.rs`) and the row is omitted rather than guessed.
    /// Other renderers ignore this field.
    Done {
        content: &'a str,
        exit_code: Option<i32>,
    },
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

/// The raw `bash` command string, when the call is a bash tool with a string
/// `command` argument. The SHELL panel builds its structured command display
/// from this; everything else keeps using `tool_display` summaries.
fn raw_bash_command(call: &ToolCall) -> Option<&str> {
    if call.name != "bash" {
        return None;
    }
    call.arguments
        .get("command")
        .and_then(|value| value.as_str())
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
        vec![styled_row(text, style)]
    }
}

/// Build a single-row, single-style panel body line carrying both the styled
/// line and its plain text mirror: the EXPLORE grouped summary row, the SHELL
/// exit-status row, and the panic-fallback error row.
fn styled_row(text: String, style: Style) -> TranscriptRow {
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
        let timeout = bash_timeout_secs(call);
        match raw_bash_command(call) {
            Some(raw) => {
                // Clean ANSI/OSC/control sequences per line (so a `;`/`|`
                // buried in an escape sequence can't wrongly split the command)
                // while preserving newlines for heredoc detection.
                let cleaned = raw
                    .split('\n')
                    .map(strip_ansi_for_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                body.command_block(&shell_command::build(&cleaned), timeout);
            }
            // Non-string/absent command: keep the single-row fallback.
            None => body.command_row(&command_display(call), timeout),
        }
        match outcome {
            ToolOutcome::Running { streamed } => {
                if !streamed.is_empty() {
                    body.output_tail(streamed);
                }
                body.line("$ \u{2588}", panel_style());
            }
            ToolOutcome::Done { content, exit_code } => {
                body.output(content);
                // `None` is not "an unknown but presumably fine" code: the bash
                // tool only omits it for a cancelled run, a timeout, or a
                // session shell that exited (`src/tools/bash/mod.rs`), each of
                // which already says so in `content`. Asserting `exit 0` here
                // would fabricate a status the run never reported, so the row
                // is omitted rather than guessed.
                if let Some(code) = exit_code {
                    // Per the design-system `ShellOutput`, the exit-status row
                    // hides in a collapsed capped preview and reappears when
                    // expanded -- but only once the OUTPUT itself has made the
                    // panel foldable. On a short panel that shows in full, the
                    // row must stay `Always`, otherwise a non-`Always` row would
                    // itself force an always-expanded panel to fold.
                    let fold = if body.is_foldable() {
                        FoldVis::WhenExpanded
                    } else {
                        FoldVis::Always
                    };
                    body.push(shell_result_row(*code, fold));
                }
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

/// The SHELL panel's closing exit-status row: `◆ exit 0` on success, `■ exit
/// <code>` on failure, glyph and label sharing one color (mirrors the
/// design-system `ShellOutput` `ResultRow` and Iris's own EDIT diff footer).
/// `fold` matches the `ShellOutput` reference: `WhenExpanded` (hidden in a
/// collapsed capped preview, shown when expanded) for a foldable panel, and
/// `Always` for a short panel whose output already shows in full.
fn shell_result_row(code: i32, fold: FoldVis) -> TranscriptRow {
    let failed = code != 0;
    let symbol = if failed {
        symbols::ERROR
    } else {
        symbols::DONE
    };
    let style = if failed { err_style() } else { ok_style() };
    styled_row(format!("{symbol} exit {code}"), style).with_fold(fold)
}

/// Body shared by EDIT and the generic TOOL fallback (identical apart from the
/// header title).
fn generic_body(ctx: &RenderCtx, outcome: &ToolOutcome) -> Vec<TranscriptRow> {
    let mut body = PanelBody::new(ctx.width);
    match outcome {
        ToolOutcome::Running { .. } => body.line("running\u{2026}", dim_style()),
        ToolOutcome::Done { content, .. } => body.output(content),
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
            Err(_) => vec![styled_row("(render error)".to_string(), err_style())],
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

    /// Push a pre-built row (e.g. the SHELL exit-status row) verbatim.
    fn push(&mut self, row: TranscriptRow) {
        self.rows.push(row);
    }

    /// Whether the body accumulated so far has made the panel foldable: true
    /// once any row carries a non-`Always` fold (a capped-preview / expanded
    /// pair). Mirrors the transcript's own foldability test so the SHELL result
    /// row can track the panel's existing collapse state instead of creating
    /// it.
    fn is_foldable(&self) -> bool {
        self.rows.iter().any(|row| row.fold != FoldVis::Always)
    }

    /// Render the structured command region: the `$` prompt row (carrying the
    /// timeout), operator-preserving continuation rows, an optional heredoc
    /// payload section, and trailing commands after the heredoc.
    fn command_block(&mut self, cmd: &ShellCommand, timeout: Option<u64>) {
        let mut segments = cmd.command.iter();
        let Some(first) = segments.next() else {
            return;
        };
        self.command_row(first, timeout);
        for cont in segments {
            self.command_continuation(cont);
        }
        if let Some(payload) = &cmd.payload {
            self.line("", panel_style());
            self.line(&format!("  payload  {}", payload.lang), dim_style());
            self.payload_rule();
            self.payload_body(&payload.body);
            self.payload_line(&payload.closing, FoldVis::Always);
        }
        for cont in &cmd.trailing {
            self.command_continuation(cont);
        }
    }

    /// A `  {text}` command continuation row (aligned under the command body,
    /// not under `$`), keeping its leading operator. Bounded like output rows so
    /// a pathological segment cannot balloon the stored row.
    fn command_continuation(&mut self, text: &str) {
        let text = truncate_chars(text, MAX_TOOL_OUTPUT_LINE_CHARS);
        self.line(&format!("  {text}"), panel_style());
    }

    /// The dim rule under the `payload  <lang>` label.
    fn payload_rule(&mut self) {
        let rule = "\u{2500}".repeat(self.width.saturating_sub(2).max(1));
        self.line(&format!("  {rule}"), dim_style());
    }

    /// One dim heredoc body / closing-delimiter row.
    fn payload_line(&mut self, text: &str, fold: FoldVis) {
        let clamped = truncate_chars(text, MAX_TOOL_OUTPUT_LINE_CHARS);
        self.line_folded(&format!("  {clamped}"), dim_style(), fold);
    }

    /// Heredoc body with middle-folding: short bodies render whole; long bodies
    /// show a head/tail slice plus a `\u{2026} N lines hidden` affordance while
    /// collapsed, and the full body while expanded (ctrl+o toggles).
    fn payload_body(&mut self, body: &[String]) {
        const HEAD: usize = 4;
        const TAIL: usize = 2;
        if body.len() <= HEAD + TAIL + 1 {
            for line in body {
                self.payload_line(line, FoldVis::Always);
            }
            return;
        }
        let hidden = body.len() - HEAD - TAIL;
        for line in &body[..HEAD] {
            self.payload_line(line, FoldVis::WhenCollapsed);
        }
        self.fold_expand_hint(hidden, false);
        for line in &body[body.len() - TAIL..] {
            self.payload_line(line, FoldVis::WhenCollapsed);
        }
        for line in body {
            self.payload_line(line, FoldVis::WhenExpanded);
        }
        self.fold_collapse_hint();
    }

    /// The SHELL `$ command` row with the timeout rendered as right-aligned
    /// invocation metadata (never inside the command text). A positive timeout
    /// hugs the right border on the same row when it fits; otherwise it drops to
    /// its own right-aligned row so a long command is never truncated to make
    /// room. `Some(0)` ("no timeout") and `None` omit the field entirely.
    fn command_row(&mut self, command: &str, timeout: Option<u64>) {
        let command = truncate_chars(command, MAX_TOOL_OUTPUT_LINE_CHARS);
        let left = format!("$ {command}");
        let Some(secs) = timeout.filter(|secs| *secs > 0) else {
            self.line(&left, panel_style());
            return;
        };
        let hint = format!("timeout {secs}s");
        let width = self.fold_hint_width();
        let left_w = display_width(&left);
        let hint_w = display_width(&hint);
        if left_w + 1 + hint_w <= width {
            let gap = width - left_w - hint_w;
            let plain = format!("{left}{}{hint}", " ".repeat(gap));
            let spans = vec![
                Span::styled(left, panel_style()),
                Span::styled(" ".repeat(gap), panel_style()),
                Span::styled(hint, dim_style()),
            ];
            self.rows.push(TranscriptRow::chrome_with_text(
                ChromeRow::Body {
                    line: Line::from(spans),
                    bg: None,
                },
                plain,
                panel_style(),
            ));
        } else {
            self.line(&left, panel_style());
            let text = right_align_hint("", &hint, width);
            self.line_folded(&text, dim_style(), FoldVis::Always);
        }
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
            &ToolOutcome::Done {
                content: "",
                exit_code: None,
            },
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
    fn shell_command_row_renders_timeout_as_right_aligned_metadata() {
        let args = json!({ "command": "echo hi", "timeout": 120 });
        let renderer = resolve(&call("bash", args.clone()));
        let ctx = RenderCtx { width: 60 };
        let rows = renderer.body(
            &ctx,
            &call("bash", args),
            &ToolOutcome::Done {
                content: "ok",
                exit_code: None,
            },
        );
        let cmd = &rows[0];
        assert!(cmd.text.starts_with("$ echo hi"), "{}", cmd.text);
        assert!(cmd.text.ends_with("timeout 120s"), "{}", cmd.text);
        // The timeout is invocation metadata, not part of the command string.
        assert!(!cmd.text.contains("(timeout"), "{}", cmd.text);
        let Some(ChromeRow::Body { line, .. }) = cmd.chrome.as_ref() else {
            panic!("expected a Body chrome row");
        };
        assert_eq!(
            line.spans.last().unwrap().style.fg,
            dim_style().fg,
            "timeout span should be dim metadata"
        );
    }

    #[test]
    fn shell_command_row_omits_timeout_when_none_or_zero() {
        let ctx = RenderCtx { width: 60 };
        for args in [
            json!({ "command": "echo hi" }),
            json!({ "command": "echo hi", "timeout": 0 }),
        ] {
            let renderer = resolve(&call("bash", args.clone()));
            let rows = renderer.body(
                &ctx,
                &call("bash", args),
                &ToolOutcome::Done {
                    content: "",
                    exit_code: None,
                },
            );
            assert_eq!(rows[0].text, "$ echo hi", "timeout field must be omitted");
        }
    }

    #[test]
    fn shell_command_row_drops_timeout_below_when_command_fills_width() {
        let command = "echo this is a fairly long command line that fills the panel width";
        let args = json!({ "command": command, "timeout": 120 });
        let renderer = resolve(&call("bash", args.clone()));
        let ctx = RenderCtx { width: 40 };
        let rows = renderer.body(
            &ctx,
            &call("bash", args),
            &ToolOutcome::Done {
                content: "",
                exit_code: None,
            },
        );
        // The command keeps its own row (untruncated here), timeout drops below.
        assert_eq!(rows[0].text, format!("$ {command}"));
        assert!(rows[1].text.ends_with("timeout 120s"), "{}", rows[1].text);
        assert_eq!(rows[1].text.trim(), "timeout 120s");
    }

    #[test]
    fn shell_splits_and_command_into_prompt_and_continuation_rows() {
        let args = json!({ "command": "cd \"/abs/path\" && cargo fmt" });
        let renderer = resolve(&call("bash", args.clone()));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("bash", args),
            &ToolOutcome::Done {
                content: "",
                exit_code: None,
            },
        );
        assert!(
            rows[0].text.starts_with("$ cd \"/abs/path\""),
            "{}",
            rows[0].text
        );
        assert_eq!(rows[1].text, "  && cargo fmt");
    }

    #[test]
    fn shell_renders_heredoc_payload_section_and_trailing_command() {
        let command = "cd \"/abs\" && python3 - <<'PY'\nfrom pathlib import Path\np = Path('x')\nPY\ncargo fmt";
        let args = json!({ "command": command });
        let renderer = resolve(&call("bash", args.clone()));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("bash", args),
            &ToolOutcome::Done {
                content: "",
                exit_code: None,
            },
        );
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.trim() == "$ cd \"/abs\"".trim()),
            "{texts:?}"
        );
        assert!(texts.contains(&"  && python3 - <<'PY'"), "{texts:?}");
        assert!(texts.contains(&"  payload  python"), "{texts:?}");
        assert!(
            texts.iter().any(|t| t.contains("from pathlib import Path")),
            "{texts:?}"
        );
        assert!(texts.contains(&"  PY"), "{texts:?}");
        assert!(texts.contains(&"  cargo fmt"), "{texts:?}");
    }

    #[test]
    fn shell_folds_long_heredoc_body_into_collapsed_preview_and_full() {
        let mut command = String::from("python3 - <<'PY'\n");
        for i in 0..40 {
            command.push_str(&format!("line {i}\n"));
        }
        command.push_str("PY");
        let args = json!({ "command": command });
        let renderer = resolve(&call("bash", args.clone()));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("bash", args),
            &ToolOutcome::Done {
                content: "",
                exit_code: None,
            },
        );
        assert!(
            rows.iter()
                .any(|r| r.fold == FoldVis::WhenCollapsed && r.text.contains("lines hidden")),
            "expected a collapsed hidden-lines affordance"
        );
        assert!(
            rows.iter()
                .any(|r| r.fold == FoldVis::WhenExpanded && r.text.contains("line 39")),
            "expected the full body to include the elided tail"
        );
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
    fn shell_done_appends_success_exit_status_row() {
        let renderer = resolve(&call("bash", json!({ "command": "echo hi" })));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("bash", json!({ "command": "echo hi" })),
            &ToolOutcome::Done {
                content: "hi",
                exit_code: Some(0),
            },
        );
        let last = rows.last().expect("expected a result row");
        assert_eq!(last.text, "\u{25c6} exit 0", "{}", last.text);
        assert_eq!(last.style.fg, ok_style().fg);
    }

    #[test]
    fn shell_done_with_nonzero_exit_renders_red_error_result_row() {
        let renderer = resolve(&call("bash", json!({ "command": "false" })));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("bash", json!({ "command": "false" })),
            &ToolOutcome::Done {
                content: "",
                exit_code: Some(1),
            },
        );
        let last = rows.last().expect("expected a result row");
        assert_eq!(last.text, "\u{25a0} exit 1", "{}", last.text);
        assert_eq!(last.style.fg, err_style().fg);
    }

    #[test]
    fn shell_done_with_unknown_exit_code_omits_result_row() {
        // `None` means the shell reported no status at all -- a cancelled run,
        // a timeout, or an exited session shell (`src/tools/bash/mod.rs`),
        // each of which already says so in `content`. Asserting `exit 0` would
        // fabricate a status the run never reported, so no row is shown.
        let renderer = resolve(&call("bash", json!({ "command": "sleep 30" })));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("bash", json!({ "command": "sleep 30" })),
            &ToolOutcome::Done {
                content: "Command cancelled by user",
                exit_code: None,
            },
        );
        assert!(
            !rows.iter().any(|r| r.text.contains("exit")),
            "{:?}",
            rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn generic_done_outcome_has_no_exit_status_row() {
        // The exit-status row is SHELL-specific; EDIT/generic panels never show
        // a fabricated exit code.
        let renderer = resolve(&call("zonk", json!({})));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("zonk", json!({})),
            &ToolOutcome::Done {
                content: "ok",
                exit_code: Some(0),
            },
        );
        assert!(
            !rows.iter().any(|r| r.text.contains("exit")),
            "{:?}",
            rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn shell_result_row_hides_when_collapsed_and_shows_when_expanded_for_folded_output() {
        // Per the design-system `ShellOutput`, the exit-status row is hidden in
        // a collapsed capped preview (`WhenExpanded`) and reappears when the
        // panel is expanded -- so a folded panel's preview stays a clean
        // head/tail slice.
        let content = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let renderer = resolve(&call("bash", json!({ "command": "seq 200" })));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("bash", json!({ "command": "seq 200" })),
            &ToolOutcome::Done {
                content: &content,
                exit_code: Some(0),
            },
        );
        let result_rows: Vec<_> = rows.iter().filter(|r| r.text.contains("exit 0")).collect();
        assert_eq!(
            result_rows.len(),
            1,
            "expected exactly one result row, got {:?}",
            rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>()
        );
        assert!(
            result_rows[0].fold == FoldVis::WhenExpanded,
            "folded panel's result row must hide while collapsed: {}",
            result_rows[0].text
        );
        // Hidden from the collapsed-visible set (fold != WhenExpanded), present
        // in the expanded-visible set (fold != WhenCollapsed).
        assert!(
            !rows
                .iter()
                .filter(|r| r.fold != FoldVis::WhenExpanded)
                .any(|r| r.text.contains("exit 0")),
            "result row must not appear in the collapsed preview"
        );
        assert!(
            rows.iter()
                .filter(|r| r.fold != FoldVis::WhenCollapsed)
                .any(|r| r.text.contains("exit 0")),
            "result row must appear in the expanded set"
        );
    }

    #[test]
    fn shell_result_row_stays_always_visible_for_short_unfolded_output() {
        // A short panel shows its output in full and is not foldable; the
        // result row must stay `Always` so it never forces the panel to fold.
        let renderer = resolve(&call("bash", json!({ "command": "echo hi" })));
        let ctx = RenderCtx { width: 80 };
        let rows = renderer.body(
            &ctx,
            &call("bash", json!({ "command": "echo hi" })),
            &ToolOutcome::Done {
                content: "hi",
                exit_code: Some(0),
            },
        );
        assert!(
            rows.iter().all(|r| r.fold == FoldVis::Always),
            "short shell panel must not become foldable: {:?}",
            rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>()
        );
        assert_eq!(rows.last().unwrap().text, "\u{25c6} exit 0");
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
            &ToolOutcome::Done {
                content: &content,
                exit_code: None,
            },
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
                exit_code: None,
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
                exit_code: None,
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
