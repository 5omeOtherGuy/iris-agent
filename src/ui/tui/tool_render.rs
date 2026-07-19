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
use crate::tool_summary::summarize_output;
use crate::ui::highlight;
use crate::ui::palette;

use super::panel::FooterField;
use super::rows::{ChromeRow, TranscriptRow};
use super::shell_command::{self, ShellCommand};
use super::text::{ansi_spans, strip_ansi_for_text};
use super::wrap::{
    clamp_output_line, display_width, truncate_clusters_with_ellipsis, wrapped_row_estimate,
};
use super::{
    MAX_TOOL_OUTPUT_LINE_CHARS, PANEL_BODY_CHROME_WIDTH, dim_style, err_style, panel_style,
    prompt_style, stdout_style,
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
/// size identically to the rest of the panel body. `preview_rows` is the
/// viewport-aware tool-output preview budget (`clamp(height/5, 8, 24)`) resolved
/// from the last-known pane height — a live tail shows at most this many rows.
pub(super) struct RenderCtx {
    pub(super) width: usize,
    pub(super) preview_rows: usize,
}

impl RenderCtx {
    /// A context at `width` with the floor preview budget. Test-only helper so
    /// body-shape tests that do not exercise the live tail need not spell out
    /// `preview_rows`; the floor equals the historical fixed cap.
    #[cfg(test)]
    fn for_width(width: usize) -> Self {
        Self {
            width,
            preview_rows: super::MAX_TOOL_OUTPUT_ROWS,
        }
    }
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
    /// Awaiting the user's approval decision (`▲ REVIEW`) or refused (`■
    /// DENIED`). The body shows only what is being authorized (the command /
    /// target) — no output, no live `$█` cursor; the affordance and any decision
    /// note ride the footer. Denial reuses this body: a refused call has no
    /// output either.
    Review,
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

    /// Result body rows, rendered between the frameless header and the
    /// hairline footer. EXPLORE returns exactly one row (its grouped summary
    /// line).
    fn body(&self, ctx: &RenderCtx, call: &ToolCall, outcome: &ToolOutcome) -> Vec<TranscriptRow>;

    /// Family extras for the block footer, rendered as `┊`-joined sibling
    /// fields after the state label (SHELL: `EXIT <code>` + result meta).
    /// Every failure keeps one cleaned cause line visible even while folded.
    fn footer_extras(&self, _call: &ToolCall, outcome: &ToolOutcome) -> Vec<FooterField> {
        error_footer_fields(outcome)
    }
}

/// One bounded, control-free error cause for an always-visible footer. The
/// state token already says ERROR, so this field carries only the first useful
/// line of cause; the body retains the complete message when expanded.
pub(super) fn error_footer_field(message: &str) -> FooterField {
    // Split before cleaning: `clean_text` intentionally removes control
    // characters, including newline, and would otherwise weld two diagnostic
    // lines together.
    let first = message
        .lines()
        .map(strip_ansi_for_text)
        .find(|line| !line.trim().is_empty())
        .unwrap_or_else(|| "tool failed".to_string());
    let first = first.split_whitespace().collect::<Vec<_>>().join(" ");
    let first = if first.is_empty() {
        "tool failed".to_string()
    } else {
        truncate_clusters_with_ellipsis(&first, MAX_TOOL_OUTPUT_LINE_CHARS)
    };
    FooterField::styled(first, dim_style())
}

fn error_footer_fields(outcome: &ToolOutcome<'_>) -> Vec<FooterField> {
    match outcome {
        ToolOutcome::Error { message, .. } => vec![error_footer_field(message)],
        _ => Vec::new(),
    }
}

// --- Built-in renderers -----------------------------------------------------

/// read/grep/find/ls -> grouped EXPLORE panel.
struct ExploreRenderer;
/// bash -> self-framing SHELL panel.
struct ShellRenderer;
/// write/edit -> EDIT panel (standard body, `EDIT` title).
struct EditRenderer;
/// spawn_subagent -> DELEGATE dispatch card (type, model, effort, task).
struct SubagentRenderer;
/// Unknown tools -> generic TOOL fallback panel.
struct GenericRenderer;

static EXPLORE: ExploreRenderer = ExploreRenderer;
static SHELL: ShellRenderer = ShellRenderer;
static EDIT: EditRenderer = EditRenderer;
static SUBAGENT: SubagentRenderer = SubagentRenderer;
static GENERIC: GenericRenderer = GenericRenderer;

/// The single source of the tool-name -> renderer map for the TUI. Unknown
/// names fall back to the generic TOOL renderer (mirrors pi-mono's
/// `getResultRenderer` built-in fallback).
pub(super) fn resolve(call: &ToolCall) -> &'static dyn ToolRenderer {
    match call.name.as_str() {
        "read" | "grep" | "find" | "ls" => &EXPLORE,
        "bash" => &SHELL,
        "write" | "edit" => &EDIT,
        "spawn_subagent" => &SUBAGENT,
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

fn path_syntax(path: &str) -> Option<String> {
    let extension = std::path::Path::new(path).extension()?.to_str()?;
    highlight::is_known_syntax(extension).then(|| extension.to_string())
}

/// Infer the syntax of file contents printed by a shell pipeline. This stays
/// intentionally conservative: only a token with a known file extension opts
/// the output into highlighting; ordinary logs keep their existing styling.
fn infer_shell_output_syntax(command: &str) -> Option<String> {
    command.split_whitespace().find_map(|token| {
        let token = token
            .trim_matches(|c: char| matches!(c, '\'' | '"' | '`' | '(' | ')' | ';' | '|' | '&'));
        path_syntax(token)
    })
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

    fn plain_meta(&self, call: &ToolCall) -> String {
        exploration_summary(call)
    }

    fn body(&self, ctx: &RenderCtx, call: &ToolCall, outcome: &ToolOutcome) -> Vec<TranscriptRow> {
        // The grouped EXPLORE panel replaces a single stored row in place, so
        // this renderer is constrained to exactly one row.
        vec![explore_op_row(panel_body_width(ctx.width), call, outcome)]
    }
}

fn panel_body_width(width: usize) -> usize {
    width.saturating_sub(PANEL_BODY_CHROME_WIDTH).max(1)
}

/// One EXPLORE op row (`FramelessExplore` op grammar):
/// `VERB  target [code][after]                           meta(count)` — verb in
/// a fixed 5-cell column, target in ink, a grep/find pattern in cyan, the scope
/// muted, and the honest count right-bound at the block's right rail.
fn explore_op_row(width: usize, call: &ToolCall, outcome: &ToolOutcome) -> TranscriptRow {
    if let ToolOutcome::Error { message, .. } = outcome {
        return styled_row(format!("error: {message}"), err_style());
    }
    let verb = explore_verb(call);
    let mut spans = vec![Span::styled(format!("{verb:<5} "), panel_style())];
    let mut plain = format!("{verb:<5} ");
    let path = tool_path_arg(call).map(display_path);
    match call.name.as_str() {
        "grep" | "find" => {
            let pattern = call
                .arguments
                .get("pattern")
                .and_then(|value| value.as_str())
                .unwrap_or("<missing pattern>");
            let code = format!("\"{pattern}\"");
            spans.push(Span::styled(
                code.clone(),
                Style::default().fg(palette::cyan()),
            ));
            plain.push_str(&code);
            let scope = path.unwrap_or_default();
            if !scope.is_empty() {
                let after = format!(" in {scope}");
                spans.push(Span::styled(after.clone(), dim_style()));
                plain.push_str(&after);
            }
        }
        _ => {
            let target = path.unwrap_or_else(|| ".".to_string());
            spans.push(Span::styled(target.clone(), panel_style()));
            plain.push_str(&target);
        }
    }
    // Right-bound count, only when the op finished and the count is real. A
    // BodyRight chrome row re-aligns the meta to the block's right rail at
    // render time, so op metas, the header elapsed, and the footer diagnostics
    // share one right edge.
    if let ToolOutcome::Done { content, exit_code } = outcome
        && let Some(meta) =
            summarize_output(call, content, *exit_code).map(|summary| summary.render())
    {
        let text =
            super::rows::right_align(&plain, &meta, width, 1, super::rows::Overflow::DropLeft);
        return TranscriptRow::chrome_with_text(
            ChromeRow::BodyRight {
                left: Line::from(spans),
                right: meta,
                right_style: dim_style(),
                bg: None,
            },
            text,
            panel_style(),
        );
    }
    TranscriptRow::chrome_with_text(
        ChromeRow::Body {
            line: Line::from(spans),
            bg: None,
        },
        plain,
        panel_style(),
    )
}

/// The fixed EXPLORE verb column: `Read` · `Grep` · `List` · `Find`.
fn explore_verb(call: &ToolCall) -> &'static str {
    match call.name.as_str() {
        "read" => "Read",
        "grep" => "Grep",
        "ls" => "List",
        "find" => "Find",
        _ => "Read",
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

    /// The frameless SHELL header carries the command as its meta
    /// (`▾ SHELL  cargo test -p context`), truncating with `…` — not the
    /// constant `bash` of the framed design.
    fn header_meta(&self, call: &ToolCall) -> String {
        run_target(call)
    }

    fn body(&self, ctx: &RenderCtx, call: &ToolCall, outcome: &ToolOutcome) -> Vec<TranscriptRow> {
        let mut body = PanelBody::shell(
            ctx.width,
            ctx.preview_rows,
            raw_bash_command(call).and_then(infer_shell_output_syntax),
        );
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
            }
            // The exit status is no longer a body row: it moves to the block
            // footer as `EXIT <code>` (+ result meta) via `footer_extras`.
            ToolOutcome::Done { content, .. } => body.output(content),
            ToolOutcome::Error { message, streamed } => {
                if !streamed.is_empty() {
                    body.output_tail(streamed);
                }
                body.line(&format!("error: {message}"), err_style());
            }
            // Review/Denied: the command block above is the whole body — no live
            // `$█` cursor, no output (the call has not run).
            ToolOutcome::Review => {}
            ToolOutcome::Cancelled { streamed } => {
                if !streamed.is_empty() {
                    body.output_tail(streamed);
                }
            }
        }
        body.into_rows()
    }

    /// SHELL footer extras: `EXIT <code>` (bold, uppercase, tracked, muted)
    /// then the honest result meta as a sibling field. `None` exit codes are
    /// omitted rather than guessed: the bash tool only omits the code for a
    /// cancelled run, a timeout, or an exited session shell
    /// (`src/tools/bash/mod.rs`), each of which already says so in `content`.
    fn footer_extras(&self, call: &ToolCall, outcome: &ToolOutcome) -> Vec<FooterField> {
        let mut fields = error_footer_fields(outcome);
        if let ToolOutcome::Done {
            content,
            exit_code: Some(code),
        } = outcome
        {
            fields.push(FooterField::styled(
                format!("EXIT {code}"),
                dim_style().add_modifier(ratatui::style::Modifier::BOLD),
            ));
            if let Some(meta) =
                summarize_output(call, content, Some(*code)).map(|summary| summary.render())
            {
                fields.push(FooterField::styled(meta, dim_style()));
            }
        }
        fields
    }
}

/// Body shared by EDIT and the generic TOOL fallback (identical apart from the
/// header title).
fn generic_body(ctx: &RenderCtx, call: &ToolCall, outcome: &ToolOutcome) -> Vec<TranscriptRow> {
    let mut body = PanelBody::new(
        ctx.width,
        ctx.preview_rows,
        tool_path_arg(call).and_then(path_syntax),
    );
    match outcome {
        ToolOutcome::Running { .. } => body.line("running\u{2026}", dim_style()),
        ToolOutcome::Done { content, .. } => body.output(content),
        ToolOutcome::Error { message, .. } => {
            body.line(&format!("error: {message}"), err_style());
        }
        // Cancelled generic/edit panels are header-only (no body rows). A
        // pending/refused review is likewise header-only — the header meta
        // carries the target being authorized.
        ToolOutcome::Cancelled { .. } | ToolOutcome::Review => {}
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

    fn body(&self, ctx: &RenderCtx, call: &ToolCall, outcome: &ToolOutcome) -> Vec<TranscriptRow> {
        generic_body(ctx, call, outcome)
    }
}

impl ToolRenderer for SubagentRenderer {
    fn kind(&self) -> ToolPanelKind {
        ToolPanelKind::Generic
    }

    fn title(&self) -> &'static str {
        "DELEGATE"
    }

    /// The dispatch line the operator cares about: which worker type, on which
    /// model, at what effort, for which task. Raw JSON never reaches the header.
    fn header_meta(&self, call: &ToolCall) -> String {
        let subagent_type = subagent_str(call, "subagent_type").unwrap_or("general");
        let model = subagent_str(call, "model").unwrap_or("default model");
        let mut meta = format!("{subagent_type} \u{b7} {model}");
        if let Some(effort) = subagent_str(call, "effort") {
            meta.push_str(&format!(" \u{b7} {effort} effort"));
        }
        if let Some(task) = subagent_task(call) {
            meta.push_str(&format!(" \u{2014} {task}"));
        }
        meta
    }

    fn body(&self, ctx: &RenderCtx, call: &ToolCall, outcome: &ToolOutcome) -> Vec<TranscriptRow> {
        if let ToolOutcome::Done { content, .. } = outcome
            && let Some(rows) = subagent_result_rows(content)
        {
            return rows;
        }
        let mut body = PanelBody::new(ctx.width, ctx.preview_rows, None);
        match outcome {
            ToolOutcome::Running { .. } => body.line("delegating\u{2026}", dim_style()),
            // Review/Denied: show exactly what is being authorized.
            ToolOutcome::Review => body.line(&subagent_grant(call), dim_style()),
            // Unrecognized result shape falls back to the honest raw output.
            ToolOutcome::Done { content, .. } => body.output(content),
            ToolOutcome::Error { message, .. } => {
                body.line(&format!("error: {message}"), err_style());
            }
            ToolOutcome::Cancelled { .. } => {}
        }
        body.into_rows()
    }
}

fn subagent_str<'a>(call: &'a ToolCall, key: &str) -> Option<&'a str> {
    call.arguments
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// The task name: the short `description` when given, else the task's first
/// non-empty line, whitespace-normalized and bounded.
fn subagent_task(call: &ToolCall) -> Option<String> {
    let task = subagent_str(call, "description")
        .map(str::to_string)
        .or_else(|| {
            subagent_str(call, "task").and_then(|task| {
                task.lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .map(str::to_string)
            })
        })?;
    let task = task.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(truncate_clusters_with_ellipsis(&task, 64))
}

/// The review body: worker type, tool grant, and run mode.
fn subagent_grant(call: &ToolCall) -> String {
    let subagent_type = subagent_str(call, "subagent_type").unwrap_or("general");
    let tools = match call
        .arguments
        .get("tools")
        .and_then(serde_json::Value::as_array)
    {
        None => "manifest tools".to_string(),
        Some(values) if values.is_empty() => "no tools".to_string(),
        Some(values) => {
            let names = values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .collect::<Vec<_>>();
            if names.len() != values.len() {
                "invalid tools".to_string()
            } else {
                truncate_clusters_with_ellipsis(&names.join(", "), 64)
            }
        }
    };
    let mode = if call
        .arguments
        .get("background")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
    {
        "background"
    } else {
        "blocking"
    };
    format!("{subagent_type} \u{b7} {tools} \u{b7} {mode}")
}

/// Compact result rows for the recognized spawn result shapes; `None` defers
/// to the raw-output fallback. The rows follow the ambient worker-lane
/// grammar — state glyph, bold short ID, row text — so the dispatch card and
/// the live lane read as one system. Background dispatches close with a quiet
/// pointer at the action surface instead of echoing JSON.
fn subagent_result_rows(content: &str) -> Option<Vec<TranscriptRow>> {
    let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
    let status = value.get("status").and_then(serde_json::Value::as_str)?;
    let worker_id = value.get("worker_id").and_then(serde_json::Value::as_str)?;
    if value.get("summary").is_none() {
        // Background single dispatch: `{worker_id, status}`.
        return Some(vec![
            subagent_worker_row(worker_id, status, status),
            styled_row("background \u{b7} /subagents".to_string(), dim_style()),
        ]);
    }
    // Blocking single-worker result: status + summary + changed-path count.
    let summary = value
        .get("summary")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|summary| !summary.is_empty());
    let mut text = match summary {
        Some(summary) => format!("{status} \u{2014} {summary}"),
        None => status.to_string(),
    };
    if let Some(changed) = value
        .get("changed_paths")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .filter(|count| *count > 0)
    {
        text.push_str(&format!(" \u{b7} {changed} files changed"));
    }
    Some(vec![subagent_worker_row(worker_id, status, &text)])
}

/// One card body row in the worker-lane grammar: state glyph, bold short
/// worker ID, row text.
fn subagent_worker_row(worker_id: &str, status: &str, text: &str) -> TranscriptRow {
    let (glyph, glyph_style) = subagent_status_glyph(status);
    let short = crate::ui::delegation_dashboard::short_id(worker_id);
    let plain = format!("{glyph} {short}  {text}");
    let line = Line::from(vec![
        Span::styled(glyph, glyph_style),
        Span::raw(" "),
        Span::styled(
            short,
            Style::default().add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(text.to_string(), panel_style()),
    ]);
    TranscriptRow::chrome_with_text(ChromeRow::Body { line, bg: None }, plain, panel_style())
}

/// The lane's state-glyph mapping, keyed by the runtime's status strings.
/// Never color alone: the glyph shape carries the state too.
fn subagent_status_glyph(status: &str) -> (&'static str, Style) {
    use crate::ui::symbols;
    match status {
        "completed" => (
            symbols::DONE,
            Style::default().fg(crate::ui::palette::green()),
        ),
        "failed" => (symbols::ERROR, err_style()),
        "cancelled" => (symbols::CANCELLED, dim_style()),
        "initializing" | "running" => (symbols::RUNNING, prompt_style()),
        _ => (symbols::EMPTY, dim_style()),
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

    fn body(&self, ctx: &RenderCtx, call: &ToolCall, outcome: &ToolOutcome) -> Vec<TranscriptRow> {
        generic_body(ctx, call, outcome)
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

/// Free helper used by the gutter output lines: an ANSI-preserving line with an
/// optional leading prefix span. `base` styles any text the program didn't
/// colour itself — `stdout_style()` for SHELL output, so unstyled program text
/// sits in the readable stdout grey rather than the darker `muted`.
fn tool_output_line(prefix: &'static str, line: &str, base: Style) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix, dim_style())];
    spans.extend(linkify_output_spans(ansi_spans(line, base)));
    Line::from(spans)
}

/// Linkify file references without disturbing the styling already carried by
/// ANSI-parsed or syntax-highlighted spans. References split across style runs
/// remain untouched.
fn linkify_output_spans(spans: impl IntoIterator<Item = Span<'static>>) -> Vec<Span<'static>> {
    // Linkify conservative workspace `file:line` references in tool output so
    // they become clickable OSC 8 targets. Applied per styled span; a reference
    // split across style runs is left untouched.
    // The workspace root is only resolved when a line actually holds a match.
    let mut linked = Vec::new();
    let mut root: Option<std::path::PathBuf> = None;
    for span in spans {
        let content = span.content.as_ref();
        if crate::ui::hyperlink::find_file_refs(content).is_empty() {
            linked.push(span);
            continue;
        }
        let root = root.get_or_insert_with(|| std::env::current_dir().unwrap_or_default());
        linked.extend(crate::ui::hyperlink::linkify_file_refs(
            content, span.style, root,
        ));
    }
    linked
}

fn linkify_output_line(mut line: Line<'static>) -> Line<'static> {
    Line::from(linkify_output_spans(line.spans.drain(..)))
}

/// A short-lived builder for tool-panel body rows. Owns the flood-cap and
/// hidden-content logic so renderers and the transcript's thin wrappers share
/// one implementation (no duplicated summary/flood logic). `width` is the
/// transcript wrap width.
struct PanelBody {
    width: usize,
    /// Viewport-aware live-tail preview budget (rows); see [`RenderCtx`].
    preview_rows: usize,
    indent: usize,
    syntax: Option<String>,
    rows: Vec<TranscriptRow>,
}

impl PanelBody {
    fn new(width: usize, preview_rows: usize, syntax: Option<String>) -> Self {
        Self {
            width,
            preview_rows,
            indent: 0,
            syntax,
            rows: Vec::new(),
        }
    }

    fn shell(width: usize, preview_rows: usize, syntax: Option<String>) -> Self {
        Self::new(width, preview_rows, syntax)
    }

    fn into_rows(self) -> Vec<TranscriptRow> {
        self.rows
    }

    fn indented_line(&self, mut line: Line<'static>) -> Line<'static> {
        if self.indent > 0 {
            line.spans.insert(0, Span::raw(" ".repeat(self.indent)));
        }
        line
    }

    fn push_line(&mut self, line: Line<'static>, text: String, style: Style) {
        let line = self.indented_line(line);
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::Body { line, bg: None },
            text,
            style,
        ));
    }

    fn push_right_line(
        &mut self,
        left: Line<'static>,
        left_text: String,
        right: &str,
        row_style: Style,
        right_style: Style,
    ) {
        let text = super::rows::right_align(
            &left_text,
            right,
            self.hint_width(),
            1,
            super::rows::Overflow::DropLeft,
        );
        let left = self.indented_line(left);
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::BodyRight {
                left,
                right: right.to_string(),
                right_style,
                bg: None,
            },
            text,
            row_style,
        ));
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
            self.payload_body(
                &shell_command::format_payload(&payload.body, &payload.lang),
                &payload.lang,
            );
            self.payload_line(&payload.closing);
        }
        for cont in &cmd.trailing {
            self.command_continuation(cont);
        }
    }

    /// A `  {text}` command continuation row (aligned under the command body,
    /// not under `$`), keeping its leading operator. Bounded like output rows so
    /// a pathological segment cannot balloon the stored row.
    fn command_continuation(&mut self, text: &str) {
        let text = truncate_clusters_with_ellipsis(text, MAX_TOOL_OUTPUT_LINE_CHARS);
        self.line(&format!("  {text}"), panel_style());
    }

    /// The dim rule under the `payload  <lang>` label.
    fn payload_rule(&mut self) {
        self.rows.push(TranscriptRow::chrome_with_text(
            ChromeRow::BodyRule {
                prefix: "  ".to_string(),
                rule: '\u{2500}',
                style: dim_style(),
                bg: None,
            },
            "  \u{2500}".to_string(),
            dim_style(),
        ));
    }

    /// One dim heredoc body / closing-delimiter row.
    fn payload_line(&mut self, text: &str) {
        let clamped = truncate_clusters_with_ellipsis(text, MAX_TOOL_OUTPUT_LINE_CHARS);
        self.line(&format!("  {clamped}"), dim_style());
    }

    /// Heredoc body, rendered whole. Collapse is binary and owned by the block
    /// header: an over-budget block arrives collapsed instead of eliding rows.
    fn payload_body(&mut self, body: &[String], lang: &str) {
        let clamped = body
            .iter()
            .map(|line| truncate_clusters_with_ellipsis(line, MAX_TOOL_OUTPUT_LINE_CHARS))
            .collect::<Vec<_>>();
        let code = clamped.join("\n");
        if let Some(lines) = highlight::highlight(&code, Some(lang)) {
            for (plain, mut line) in clamped.iter().zip(lines) {
                line.spans.insert(0, Span::raw("  "));
                self.push_line(line, format!("  {plain}"), panel_style());
            }
        } else {
            for line in body {
                self.payload_line(line);
            }
        }
    }

    /// The SHELL `$ command` row with the timeout rendered as right-aligned
    /// invocation metadata (never inside the command text). A positive timeout
    /// hugs the right border on the same row when it fits; otherwise it drops to
    /// its own right-aligned row so a long command is never truncated to make
    /// room. `Some(0)` ("no timeout") and `None` omit the field entirely.
    fn command_row(&mut self, command: &str, timeout: Option<u64>) {
        let command = truncate_clusters_with_ellipsis(command, MAX_TOOL_OUTPUT_LINE_CHARS);
        let left = format!("$ {command}");
        // The `$ ` prompt is quiet chrome (muted); the command itself is the
        // brightest thing in the body (ShellOutput `cmd` line grammar).
        let cmd_spans = |command: &str| {
            vec![
                Span::styled("$ ", dim_style()),
                Span::styled(command.to_string(), panel_style()),
            ]
        };
        let Some(secs) = timeout.filter(|secs| *secs > 0) else {
            self.push_line(Line::from(cmd_spans(&command)), left, panel_style());
            return;
        };
        let hint = format!("timeout {secs}s");
        let width = self.hint_width();
        let left_w = display_width(&left);
        let hint_w = display_width(&hint);
        if left_w + 1 + hint_w <= width {
            self.push_right_line(
                Line::from(cmd_spans(&command)),
                left,
                &hint,
                panel_style(),
                dim_style(),
            );
        } else {
            self.push_line(Line::from(cmd_spans(&command)), left, panel_style());
            self.push_right_line(
                Line::default(),
                String::new(),
                &hint,
                dim_style(),
                dim_style(),
            );
        }
    }

    /// Push a plain panel body line (ANSI stripped), one row per `\n` segment.
    fn line(&mut self, text: &str, style: Style) {
        for line in text.split('\n') {
            let line = strip_ansi_for_text(line);
            self.push_line(Line::from(Span::styled(line.clone(), style)), line, style);
        }
    }

    /// Inner block-body width available for right-aligning invocation hints
    /// (the timeout field). Matches the transcript's wrap width so the hint
    /// hugs the block's right rail.
    fn hint_width(&self) -> usize {
        self.width
            .saturating_sub(PANEL_BODY_CHROME_WIDTH)
            .saturating_sub(self.indent)
            .max(1)
    }

    /// The honest flood-cap elision marker: `… N lines hidden` (or `… N
    /// earlier lines hidden` for a live tail). A static, non-searchable body
    /// row — NOT a fold affordance; frameless collapse is binary and owned by
    /// the block header.
    fn hidden_lines_marker(&mut self, hidden: usize, earlier: bool) {
        let noun = if earlier { "earlier lines" } else { "lines" };
        let text = format!("\u{2026} {hidden} {noun} hidden");
        self.push_line(
            Line::from(Span::styled(text.clone(), dim_style())),
            text,
            dim_style(),
        );
        if let Some(row) = self.rows.last_mut() {
            row.searchable = false;
        }
    }

    /// Push one gutter-prefixed tool-output line, preserving ANSI styling and
    /// hard-wrapping so leading indentation/aligned columns survive. `first`
    /// selects the `  \u{2514} ` head gutter vs the `    ` continuation gutter.
    fn output_line(&mut self, raw: &str, first: bool) {
        let line = truncate_clusters_with_ellipsis(raw, MAX_TOOL_OUTPUT_LINE_CHARS);
        // One dim connector separates invocation from result without adding a
        // label or rule. Continuations align under the result text, so the eye
        // can scan command → output in a single downward motion.
        let prefix = if first { "\u{2514} " } else { "  " };
        let plain = format!("{prefix}{}", strip_ansi_for_text(&line));
        self.push_line(
            tool_output_line(prefix, &line, stdout_style()),
            plain,
            stdout_style(),
        );
    }

    /// Finalized tool output, stored whole (one row per logical line, each
    /// line length-clamped). There is no head/tail elision: the flood guard is
    /// the block's arrival fold — an over-budget body arrives collapsed to
    /// header + footer, and ctrl+o reveals the full output.
    fn output(&mut self, content: &str) {
        if content.is_empty() {
            self.line("(no output)", dim_style());
            return;
        }
        let lines = content
            .lines()
            .map(|line| truncate_clusters_with_ellipsis(line, MAX_TOOL_OUTPUT_LINE_CHARS))
            .collect::<Vec<_>>();
        self.output_lines(&lines);
    }

    fn output_lines(&mut self, lines: &[String]) {
        let code = lines.join("\n");
        // Program-authored ANSI colors win. Otherwise parse the complete block
        // at once so multiline syntax state (comments/strings) is retained.
        let highlighted = (!code.contains('\u{1b}'))
            .then(|| highlight::highlight(&code, self.syntax.as_deref()))
            .flatten();
        for (i, raw) in lines.iter().enumerate() {
            if let Some(line) = highlighted.as_ref().and_then(|lines| lines.get(i)).cloned() {
                self.highlighted_output_line(raw, line, i == 0);
            } else {
                self.output_line(raw, i == 0);
            }
        }
    }

    fn highlighted_output_line(&mut self, raw: &str, mut line: Line<'static>, first: bool) {
        let prefix = if first { "\u{2514} " } else { "  " };
        line.spans.insert(0, Span::styled(prefix, dim_style()));
        self.push_line(
            linkify_output_line(line),
            format!("{prefix}{}", strip_ansi_for_text(raw)),
            stdout_style(),
        );
    }

    /// TAIL-capped tool output for the LIVE streaming cell: show the most recent
    /// physical rows so a growing stream scrolls instead of freezing on its
    /// head. When earlier output dropped, an honest `… N earlier lines hidden`
    /// marker precedes the tail.
    fn output_tail(&mut self, content: &str) {
        if content.is_empty() {
            self.line("(no output)", dim_style());
            return;
        }
        let width = self.width;
        // Viewport-aware budget: the live tail claims at most a fifth of the
        // pane (floor 8, ceiling 24). Print-time only — a block keeps this size
        // in scrollback even after a resize (reactive-density spec §2).
        let budget = self.preview_rows;
        let lines: Vec<&str> = content.lines().collect();
        let mut physical = 0usize;
        let mut take = 0usize;
        for raw in lines.iter().rev() {
            let rows = wrapped_row_estimate(
                &truncate_clusters_with_ellipsis(raw, MAX_TOOL_OUTPUT_LINE_CHARS),
                width,
            );
            if take > 0 && physical + rows > budget {
                break;
            }
            physical += rows;
            take += 1;
        }
        let start = lines.len() - take;
        if start == 0 {
            let clamped = lines
                .iter()
                .map(|raw| clamp_output_line(raw, width, budget))
                .collect::<Vec<_>>();
            self.output_lines(&clamped);
            return;
        }
        // The earlier-lines marker, then the most recent tail.
        self.hidden_lines_marker(start, true);
        let clamped = lines[start..]
            .iter()
            .map(|raw| clamp_output_line(raw, width, budget))
            .collect::<Vec<_>>();
        self.output_lines(&clamped);
    }
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

    fn body_line(row: &TranscriptRow) -> Option<&Line<'static>> {
        match row.chrome.as_ref() {
            Some(ChromeRow::Body { line, .. }) => Some(line),
            Some(ChromeRow::BodyRight { left, .. }) => Some(left),
            _ => row.line.as_ref(),
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
    fn subagent_header_names_type_model_effort_and_task() {
        let spawn = call(
            "spawn_subagent",
            json!({
                "subagent_type": "review",
                "model": "sonnet-4-5",
                "effort": "high",
                "description": "audit provider adapters",
                "task": "Audit every provider adapter for drift.",
                "tools": ["read_only", "bash"],
                "background": false
            }),
        );
        let renderer = resolve(&spawn);
        assert_eq!(renderer.title(), "DELEGATE");
        assert_eq!(
            renderer.header_meta(&spawn),
            "review \u{b7} sonnet-4-5 \u{b7} high effort \u{2014} audit provider adapters"
        );
        assert_eq!(
            subagent_grant(&spawn),
            "review \u{b7} read_only, bash \u{b7} blocking"
        );

        // Defaults: general manifest, manifest route/tools, no effort; the task
        // falls back to the first non-empty task line.
        let bare = call(
            "spawn_subagent",
            json!({ "task": "\n  Fix the flaky test.\nDetails follow." }),
        );
        assert_eq!(
            resolve(&bare).header_meta(&bare),
            "general \u{b7} default model \u{2014} Fix the flaky test."
        );
        assert_eq!(
            subagent_grant(&bare),
            "general \u{b7} manifest tools \u{b7} background"
        );
        let empty = call("spawn_subagent", json!({ "task": "inspect", "tools": [] }));
        assert_eq!(
            subagent_grant(&empty),
            "general \u{b7} no tools \u{b7} background"
        );
        let invalid = call("spawn_subagent", json!({ "task": "inspect", "tools": [1] }));
        assert_eq!(
            subagent_grant(&invalid),
            "general \u{b7} invalid tools \u{b7} background"
        );
    }

    #[test]
    fn subagent_result_bodies_stay_compact_for_known_shapes() {
        let ctx = RenderCtx::for_width(120);
        let spawn = call("spawn_subagent", json!({ "description": "worker" }));
        let renderer = resolve(&spawn);

        let single = json!({
            "worker_id": "wrk_0123456789abcdef",
            "status": "queued"
        })
        .to_string();
        let rows = renderer.body(
            &ctx,
            &spawn,
            &ToolOutcome::Done {
                content: &single,
                exit_code: None,
            },
        );
        let text = rows
            .iter()
            .filter_map(|row| body_line(row).map(|line| line.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("wrk_01234567  queued"),
            "lane-grammar worker row: {text}"
        );
        assert!(text.contains("background \u{b7} /subagents"), "{text}");
        assert!(!text.contains('{'), "no raw JSON in the body: {text}");

        // A blocking result renders one settled worker row: state glyph,
        // short ID, status + summary + changed-path count.
        let blocking = json!({
            "worker_id": "wrk_0123456789abcdef",
            "status": "completed",
            "summary": "gate passed",
            "changed_paths": ["src/a.rs", "src/b.rs"]
        })
        .to_string();
        let rows = renderer.body(
            &ctx,
            &spawn,
            &ToolOutcome::Done {
                content: &blocking,
                exit_code: None,
            },
        );
        let text = rows
            .iter()
            .filter_map(|row| body_line(row).map(|line| line.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains(&format!(
                "{} wrk_01234567  completed \u{2014} gate passed \u{b7} 2 files changed",
                crate::ui::symbols::DONE
            )),
            "{text}"
        );
    }

    #[test]
    fn shell_header_meta_is_the_command() {
        let renderer = resolve(&call("bash", json!({ "command": "echo hi" })));
        assert_eq!(
            renderer.header_meta(&call("bash", json!({ "command": "echo hi" }))),
            "echo hi"
        );
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
        let ctx = RenderCtx::for_width(80);
        let done = renderer.body(
            &ctx,
            &call("grep", json!({ "pattern": "needle", "path": "src" })),
            &ToolOutcome::Done {
                content: "",
                exit_code: None,
            },
        );
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].text, "Grep  \"needle\" in src");
        let errored = renderer.body(
            &ctx,
            &call("grep", json!({ "pattern": "needle", "path": "src" })),
            &ToolOutcome::Error {
                message: "boom",
                streamed: "",
            },
        );
        assert_eq!(errored[0].style.fg, err_style().fg);

        let listed = resolve(&call("ls", json!({ "path": "." }))).body(
            &ctx,
            &call("ls", json!({ "path": "." })),
            &ToolOutcome::Done {
                content: "a\nb\nc",
                exit_code: None,
            },
        );
        assert_eq!(listed.len(), 1);
        // The honest count is right-bound at the block's right rail via a
        // BodyRight chrome row (render-time alignment).
        assert!(listed[0].text.ends_with("3 entries"), "{}", listed[0].text);
        assert_eq!(
            display_width(&listed[0].text),
            panel_body_width(ctx.width),
            "{}",
            listed[0].text
        );
        let Some(ChromeRow::BodyRight { right, .. }) = listed[0].chrome.as_ref() else {
            panic!("expected a right-bound op meta");
        };
        assert_eq!(right, "3 entries");
    }

    #[test]
    fn shell_command_row_renders_timeout_as_right_aligned_metadata() {
        let args = json!({ "command": "echo hi", "timeout": 120 });
        let renderer = resolve(&call("bash", args.clone()));
        let ctx = RenderCtx::for_width(60);
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
        let Some(ChromeRow::BodyRight { right_style, .. }) = cmd.chrome.as_ref() else {
            panic!("expected a right-aligned Body chrome row");
        };
        assert_eq!(
            right_style.fg,
            dim_style().fg,
            "timeout span should be dim metadata"
        );
    }

    #[test]
    fn shell_command_row_omits_timeout_when_none_or_zero() {
        let ctx = RenderCtx::for_width(60);
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
        let ctx = RenderCtx::for_width(40);
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
    fn shell_output_sanitizes_plain_tabs_carriage_returns_and_osc() {
        let args = json!({ "command": "cat Makefile" });
        let renderer = resolve(&call("bash", args.clone()));
        let ctx = RenderCtx::for_width(64);
        let rows = renderer.body(
            &ctx,
            &call("bash", args),
            &ToolOutcome::Done {
                content: "a\tb\rclobber\x1b]0;owned\x07safe",
                exit_code: None,
            },
        );
        let output = rows
            .iter()
            .find(|row| row.text.contains("clobber") || row.text.contains("owned"))
            .expect("sanitized output row");
        assert!(
            output.text.contains("a       bclobbersafe"),
            "{}",
            output.text
        );
        assert!(!output.text.contains("owned"), "{}", output.text);
        for forbidden in ['\t', '\r', '\x1b', '\x07'] {
            assert!(
                !output.text.contains(forbidden),
                "forbidden control {forbidden:?} leaked in {:?}",
                output.text
            );
        }

        let mut rendered = Vec::new();
        output.render_rows(ctx.width, &mut rendered);
        let mut rendered_has_expanded_content = false;
        for line in rendered {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            rendered_has_expanded_content |= text.contains("a       bclobbersafe");
            assert!(!text.contains("owned"), "{text:?}");
            for forbidden in ['\t', '\r', '\x1b', '\x07'] {
                assert!(
                    !text.contains(forbidden),
                    "forbidden control {forbidden:?} leaked in rendered row {text:?}"
                );
            }
            assert!(display_width(&text) <= ctx.width, "{text:?}");
        }
        assert!(
            rendered_has_expanded_content,
            "rendered output should preserve standard tab stops"
        );
    }

    #[test]
    fn shell_file_output_is_syntax_highlighted_from_pipeline_path() {
        let args = json!({
            "command": "cat projects/iris/src/main.rs | head -300"
        });
        let shell_call = call("bash", args);
        let rows = resolve(&shell_call).body(
            &RenderCtx::for_width(100),
            &shell_call,
            &ToolOutcome::Done {
                content: "fn main() {\n    let answer = 42; // visible\n}",
                exit_code: Some(0),
            },
        );
        let colors = rows
            .iter()
            .filter_map(body_line)
            .flat_map(|line| line.spans.iter().filter_map(|span| span.style.fg))
            .collect::<std::collections::HashSet<_>>();
        assert!(
            colors.len() > 2,
            "expected several token colors, got {colors:?}"
        );
        assert!(rows.iter().any(|row| row.text.contains("let answer = 42")));
    }

    #[test]
    fn ansi_colored_shell_output_takes_precedence_over_inferred_syntax() {
        let args = json!({ "command": "cat src/main.rs" });
        let shell_call = call("bash", args);
        let rows = resolve(&shell_call).body(
            &RenderCtx::for_width(80),
            &shell_call,
            &ToolOutcome::Done {
                content: "\u{1b}[35mfn\u{1b}[0m main() {}",
                exit_code: Some(0),
            },
        );
        let output = rows.iter().find(|row| row.text.starts_with('└')).unwrap();
        let line = body_line(output).unwrap();
        assert!(
            line.spans.iter().any(|span| {
                span.style.fg.is_some()
                    && span.style.fg != stdout_style().fg
                    && span.style.fg != Some(palette::orange())
            }),
            "expected the program's ANSI color: {:?}",
            line.spans
        );
        assert_eq!(output.text, "└ fn main() {}");
    }

    #[test]
    fn file_oriented_generic_tool_output_uses_the_same_highlighter() {
        let custom_call = call("inspect", json!({ "path": "src/lib.py" }));
        let rows = resolve(&custom_call).body(
            &RenderCtx::for_width(80),
            &custom_call,
            &ToolOutcome::Done {
                content: "def answer():\n    return 42",
                exit_code: None,
            },
        );
        let colors = rows
            .iter()
            .filter_map(body_line)
            .flat_map(|line| line.spans.iter().filter_map(|span| span.style.fg))
            .collect::<std::collections::HashSet<_>>();
        assert!(colors.len() > 1, "expected Python token colors: {colors:?}");
    }

    #[test]
    fn shell_splits_and_command_into_prompt_and_continuation_rows() {
        let args = json!({ "command": "cd \"/abs/path\" && cargo fmt" });
        let renderer = resolve(&call("bash", args.clone()));
        let ctx = RenderCtx::for_width(80);
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
        let ctx = RenderCtx::for_width(80);
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
    fn shell_renders_long_heredoc_body_whole() {
        // The heredoc payload is stored whole: collapse is binary and owned by
        // the block header (an over-budget block arrives collapsed), so the
        // body carries no elision markers and no fold-affordance chrome.
        let mut command = String::from("python3 - <<'PY'\n");
        for i in 0..40 {
            command.push_str(&format!("line {i}\n"));
        }
        command.push_str("PY");
        let args = json!({ "command": command });
        let renderer = resolve(&call("bash", args.clone()));
        let ctx = RenderCtx::for_width(80);
        let rows = renderer.body(
            &ctx,
            &call("bash", args),
            &ToolOutcome::Done {
                content: "",
                exit_code: None,
            },
        );
        for i in 0..40 {
            assert!(
                rows.iter().any(|r| r.text.contains(&format!("line {i}"))),
                "payload line {i} missing"
            );
        }
        assert!(
            !rows.iter().any(|r| r.text.contains("lines hidden")),
            "no elision marker in the stored body"
        );
        assert!(
            !rows.iter().any(|r| r.text.contains("ctrl+o")),
            "body rows must not carry fold affordance hints"
        );
    }

    #[test]
    fn tool_output_file_line_ref_becomes_a_clickable_marker() {
        use crate::ui::hyperlink;
        let args = json!({ "command": "cargo build" });
        let renderer = resolve(&call("bash", args.clone()));
        let ctx = RenderCtx::for_width(80);
        let rows = renderer.body(
            &ctx,
            &call("bash", args),
            &ToolOutcome::Done {
                content: "error at src/main.rs:10 here",
                exit_code: Some(1),
            },
        );
        // A rendered body row carries an OSC 8 marker whose target resolves the
        // ref, and the visible (marker-free) text is unchanged.
        let render = |rows: &[TranscriptRow]| -> Vec<Line<'static>> {
            let mut out = Vec::new();
            for row in rows {
                row.render_rows(80, &mut out);
            }
            out
        };
        let lines = render(&rows);
        // File refs are a distinct *file-ref* marker kind (finding 3), never a
        // web-link marker -- so inline serialization emits no OSC 8, but the
        // pager still resolves the click to the file-ref notice.
        let uri = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find_map(|s| hyperlink::fileref_uri(s.content.as_ref()))
            .expect("file:line ref linkified");
        assert!(uri.starts_with("file://"), "uri: {uri}");
        assert!(uri.ends_with("/src/main.rs#L10"), "uri: {uri}");
        assert!(
            !lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|s| hyperlink::marker_uri(s.content.as_ref()).is_some()),
            "file refs must not be web-link markers"
        );
        // Inline serialization of every row emits no OSC 8 for the file ref.
        for line in &lines {
            let bytes = crate::ui::terminal_surface::render_line_for_test(line);
            assert!(
                !bytes.contains("\x1b]8;;"),
                "file ref must not become OSC 8 inline: {bytes:?}"
            );
        }
        // The pager path resolves the click to the (file://) notice target.
        let regions = hyperlink::extract_and_strip_lines(&mut lines.clone());
        assert!(
            regions
                .iter()
                .any(|r| !hyperlink::is_web_url(&r.uri) && r.uri.ends_with("/src/main.rs#L10")),
            "pager resolves the file ref to a notice target: {regions:?}"
        );
        assert!(
            rows.iter().any(|r| r.text.contains("src/main.rs:10")),
            "visible file:line text preserved"
        );
        // A line with no ref stays marker-free (no false positives, e.g. a
        // bare `ratio 3:4`).
        let plain = renderer.body(
            &ctx,
            &call("bash", json!({ "command": "echo hi" })),
            &ToolOutcome::Done {
                content: "just some output with a ratio 3:4",
                exit_code: Some(0),
            },
        );
        assert!(
            !render(&plain)
                .iter()
                .flat_map(|line| &line.spans)
                .any(|s| hyperlink::is_marker(s.content.as_ref())),
            "non-reference output must not be linkified"
        );
    }

    #[test]
    fn forged_markers_in_tool_output_are_neutralized() {
        // The tool-output span path runs every span content through
        // `clean_text` (which strips APC), so a forged marker in tool output
        // cannot survive to be re-interpreted (finding 2). We prove the
        // existing machinery already strips it rather than double-stripping.
        use crate::ui::hyperlink;
        let renderer = resolve(&call("bash", json!({ "command": "echo" })));
        let ctx = RenderCtx::for_width(80);
        let forged = format!(
            "pre{}pwn{}post",
            hyperlink::open_marker("https://evil.example/"),
            hyperlink::CLOSE_MARKER,
        );
        let rows = renderer.body(
            &ctx,
            &call("bash", json!({ "command": "echo" })),
            &ToolOutcome::Done {
                content: &forged,
                exit_code: Some(0),
            },
        );
        let mut lines = Vec::new();
        for row in &rows {
            row.render_rows(80, &mut lines);
        }
        assert!(
            !lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|s| hyperlink::is_marker(s.content.as_ref())),
            "forged tool-output markers must be stripped by clean_text"
        );
        for line in &lines {
            let bytes = crate::ui::terminal_surface::render_line_for_test(line);
            assert!(!bytes.contains("\x1b]8;;"), "no OSC 8: {bytes:?}");
        }
        assert!(
            hyperlink::extract_and_strip_lines(&mut lines).is_empty(),
            "no LinkRegion from forged tool output"
        );
    }

    #[test]
    fn width_dependent_shell_rows_recompose_after_resize() {
        fn rendered_texts(row: &TranscriptRow, width: usize) -> Vec<String> {
            let mut rendered = Vec::new();
            row.render_rows(width, &mut rendered);
            rendered
                .into_iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect()
                })
                .collect()
        }

        fn assert_single_right_aligned(row: &TranscriptRow, width: usize, right: &str) {
            let rendered = rendered_texts(row, width);
            assert_eq!(rendered.len(), 1, "width {width}: {rendered:?}");
            assert!(
                display_width(&rendered[0]) <= width,
                "width {width}: {:?}",
                rendered[0]
            );
            assert!(
                rendered[0].trim_end().ends_with(right),
                "width {width}: {:?}",
                rendered[0]
            );
        }

        let mut command = String::from(
            "printf 'this command is long enough to wrap at narrow widths' && python3 - <<'PY'\n",
        );
        for i in 0..40 {
            command.push_str(&format!("line {i}\n"));
        }
        command.push_str("PY");
        let args = json!({ "command": command, "timeout": 120 });
        let renderer = resolve(&call("bash", args.clone()));
        let rows = renderer.body(
            &RenderCtx::for_width(120),
            &call("bash", args),
            &ToolOutcome::Done {
                content: "ok",
                exit_code: None,
            },
        );

        let timeout = rows
            .iter()
            .find(|row| row.text.contains("timeout 120s"))
            .expect("timeout row");
        let payload_rule = rows
            .iter()
            .find(|row| {
                let trimmed = row.text.trim();
                !trimmed.is_empty() && trimmed.chars().all(|ch| ch == '─')
            })
            .expect("payload rule row");

        for width in [120, 90, 130] {
            assert_single_right_aligned(timeout, width, "timeout 120s");
            let rendered = rendered_texts(payload_rule, width);
            assert_eq!(rendered.len(), 1, "width {width}: {rendered:?}");
            assert!(display_width(&rendered[0]) <= width, "{:?}", rendered[0]);
        }

        let narrow_timeout = rendered_texts(timeout, 44);
        assert!(
            narrow_timeout.iter().any(|row| row.contains("$ printf")),
            "narrow resize must preserve command text: {narrow_timeout:?}"
        );
        assert!(
            narrow_timeout
                .iter()
                .any(|row| row.contains("timeout 120s")),
            "narrow resize must still show timeout metadata: {narrow_timeout:?}"
        );
    }

    #[test]
    fn shell_running_has_one_command_row_when_stream_is_empty() {
        let renderer = resolve(&call("bash", json!({ "command": "echo hi" })));
        let ctx = RenderCtx::for_width(80);
        let rows = renderer.body(
            &ctx,
            &call("bash", json!({ "command": "echo hi" })),
            &ToolOutcome::Running { streamed: "" },
        );
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["$ echo hi"]);
    }

    #[test]
    fn generic_cancelled_has_no_body_rows() {
        let renderer = resolve(&call("zonk", json!({})));
        let ctx = RenderCtx::for_width(80);
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
        let ctx = RenderCtx::for_width(80);
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
    fn failure_footer_keeps_one_clean_cause_line_for_every_renderer() {
        let generic_call = call("zonk", json!({}));
        let generic = resolve(&generic_call).footer_extras(
            &generic_call,
            &ToolOutcome::Error {
                message: "\u{1b}[31mpermission   denied\u{1b}[0m\nsecond line",
                streamed: "",
            },
        );
        assert_eq!(generic.len(), 1);
        assert_eq!(generic[0].plain, "permission denied");

        let shell_call = call("bash", json!({ "command": "make" }));
        let shell = resolve(&shell_call).footer_extras(
            &shell_call,
            &ToolOutcome::Error {
                message: "linker failed\nfull diagnostic",
                streamed: "partial output",
            },
        );
        assert_eq!(shell.len(), 1);
        assert_eq!(shell[0].plain, "linker failed");
    }

    #[test]
    fn shell_footer_extras_carry_exit_code_and_result_meta() {
        // The exit status is a footer field cluster, not a body row: `EXIT 0`
        // (bold, muted) then the honest result meta as a sibling field.
        let renderer = resolve(&call("bash", json!({ "command": "echo hi" })));
        let fields = renderer.footer_extras(
            &call("bash", json!({ "command": "echo hi" })),
            &ToolOutcome::Done {
                content: "hi",
                exit_code: Some(0),
            },
        );
        assert_eq!(fields[0].plain, "EXIT 0");
        let body = renderer.body(
            &RenderCtx::for_width(80),
            &call("bash", json!({ "command": "echo hi" })),
            &ToolOutcome::Done {
                content: "hi",
                exit_code: Some(0),
            },
        );
        assert!(
            !body.iter().any(|r| r.text.contains("EXIT")),
            "exit status must not be a body row"
        );
    }

    #[test]
    fn shell_footer_extras_render_nonzero_exit_code() {
        let renderer = resolve(&call("bash", json!({ "command": "false" })));
        let fields = renderer.footer_extras(
            &call("bash", json!({ "command": "false" })),
            &ToolOutcome::Done {
                content: "",
                exit_code: Some(1),
            },
        );
        assert_eq!(fields[0].plain, "EXIT 1");
    }

    #[test]
    fn shell_done_with_unknown_exit_code_omits_exit_field() {
        // `None` means the shell reported no status at all -- a cancelled run,
        // a timeout, or an exited session shell (`src/tools/bash/mod.rs`),
        // each of which already says so in `content`. Asserting `exit 0` would
        // fabricate a status the run never reported, so no field is shown.
        let renderer = resolve(&call("bash", json!({ "command": "sleep 30" })));
        assert!(
            renderer
                .footer_extras(
                    &call("bash", json!({ "command": "sleep 30" })),
                    &ToolOutcome::Done {
                        content: "Command cancelled by user",
                        exit_code: None,
                    },
                )
                .is_empty()
        );
        let ctx = RenderCtx::for_width(80);
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
        let ctx = RenderCtx::for_width(80);
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
    fn shell_long_output_body_is_capped_without_exit_row_or_fold_hints() {
        // The frameless SHELL body is the flood-capped output alone: the exit
        // status lives in the footer and there is no fold-affordance chrome.
        let content = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let renderer = resolve(&call("bash", json!({ "command": "seq 200" })));
        let ctx = RenderCtx::for_width(80);
        let rows = renderer.body(
            &ctx,
            &call("bash", json!({ "command": "seq 200" })),
            &ToolOutcome::Done {
                content: &content,
                exit_code: Some(0),
            },
        );
        // The body carries no exit status (a footer field), no fold-affordance
        // hints, and no elision — the whole output is stored; the transcript's
        // arrival fold is the flood guard.
        assert!(
            !rows.iter().any(|r| r.text.contains("EXIT 0")),
            "exit status must not be a body row: {:?}",
            rows.iter().map(|r| r.text.clone()).collect::<Vec<_>>()
        );
        assert!(
            !rows.iter().any(|r| r.text.contains("ctrl+o")),
            "body rows must not carry fold affordance hints"
        );
        assert!(
            rows.iter().any(|r| r.text.contains("line 0"))
                && rows.iter().any(|r| r.text.contains("line 199")),
            "the whole output is stored"
        );
    }

    #[test]
    fn generic_done_output_is_stored_whole() {
        // Far more logical lines than the physical-row budget: every line is
        // still stored (searchable, revealable); the transcript folds the
        // block on arrival instead of eliding rows.
        let content = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let renderer = resolve(&call("zonk", json!({})));
        let ctx = RenderCtx::for_width(80);
        let rows = renderer.body(
            &ctx,
            &call("zonk", json!({})),
            &ToolOutcome::Done {
                content: &content,
                exit_code: None,
            },
        );
        assert_eq!(rows.len(), 200);
        assert!(
            !rows.iter().any(|r| r.text.contains("hidden")),
            "no elision marker in the stored body"
        );
        assert!(
            rows.iter().any(|r| r.text.contains("line 199")),
            "the tail is stored"
        );
    }

    #[test]
    fn pathological_tool_lines_report_the_defensive_cap() {
        let renderer = resolve(&call("bash", json!({ "command": "界".repeat(2_100) })));
        let ctx = RenderCtx::for_width(80);
        let rows = renderer.body(
            &ctx,
            &call("bash", json!({ "command": "界".repeat(2_100) })),
            &ToolOutcome::Done {
                content: &"e\u{301}".repeat(2_100),
                exit_code: Some(0),
            },
        );

        let command = rows.first().expect("command row");
        let output = rows.last().expect("output row");
        assert!(command.text.ends_with('…'), "command cap was silent");
        assert!(output.text.ends_with('…'), "output cap was silent");
        assert!(
            !command.text.ends_with('\u{301}') && !output.text.ends_with('\u{301}'),
            "caps stay on grapheme boundaries"
        );
    }

    #[test]
    fn output_preserves_ansi_color_spans() {
        let renderer = resolve(&call("zonk", json!({})));
        let ctx = RenderCtx::for_width(80);
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
        let ctx = RenderCtx::for_width(80);
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

    #[test]
    fn shell_live_tail_previews_the_ctx_budget() {
        // Reactive-density §2: the live SHELL tail previews at most `preview_rows`
        // stream rows — the viewport-aware budget threaded on the `RenderCtx`.
        // A 30-line stream at budget 24 (a 120-row pane) shows 24 rows + the
        // earlier-lines elision marker; at the floor budget 8 (a ≤ 40-row pane)
        // it shows 8 — today's exact behavior on a small terminal.
        let streamed = (0..30)
            .map(|i| format!("row{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        for budget in [8usize, 24] {
            let ctx = RenderCtx {
                width: 80,
                preview_rows: budget,
            };
            let rows = render_body(
                &SHELL,
                &ctx,
                &call("bash", json!({ "command": "seq 30" })),
                &ToolOutcome::Running {
                    streamed: &streamed,
                },
            );
            let shown = rows.iter().filter(|r| r.text.contains("row")).count();
            assert_eq!(shown, budget, "budget {budget}: {shown} tail rows");
            assert!(
                rows.iter().any(|r| r.text.contains("earlier lines hidden")),
                "budget {budget}: missing elision marker"
            );
        }
    }
}
