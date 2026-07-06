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
use crate::ui::palette;

use super::panel::FooterField;
use super::rows::{ChromeRow, TranscriptRow};
use super::shell_command::{self, ShellCommand};
use super::text::{ansi_spans, strip_ansi_for_text};
use super::wrap::{clamp_output_line, display_width, truncate_chars, wrapped_row_estimate};
use super::{
    MAX_TOOL_OUTPUT_LINE_CHARS, MAX_TOOL_OUTPUT_ROWS, PANEL_BODY_CHROME_WIDTH, dim_style,
    err_style, panel_style, stdout_style,
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
    /// Default: none (EXPLORE/EDIT-generic footers are state + diagnostics).
    fn footer_extras(&self, _call: &ToolCall, _outcome: &ToolOutcome) -> Vec<FooterField> {
        Vec::new()
    }
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
        let mut body = PanelBody::shell(ctx.width);
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
        let ToolOutcome::Done {
            content,
            exit_code: Some(code),
        } = outcome
        else {
            return Vec::new();
        };
        let mut fields = vec![FooterField::styled(
            format!("EXIT {code}"),
            dim_style().add_modifier(ratatui::style::Modifier::BOLD),
        )];
        if let Some(meta) =
            summarize_output(call, content, Some(*code)).map(|summary| summary.render())
        {
            fields.push(FooterField::styled(meta, dim_style()));
        }
        fields
    }
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

/// Free helper used by the gutter output lines: an ANSI-preserving line with an
/// optional leading prefix span. `base` styles any text the program didn't
/// colour itself — `stdout_style()` for SHELL output, so unstyled program text
/// sits in the readable stdout grey rather than the darker `muted`.
fn tool_output_line(prefix: &'static str, line: &str, base: Style) -> Line<'static> {
    let mut spans = vec![Span::styled(prefix, dim_style())];
    // Linkify conservative workspace `file:line` references in tool output so
    // they become clickable OSC 8 targets. Applied per parsed ANSI span (styling
    // preserved); a reference split across ANSI style runs is left untouched.
    // The workspace root is only resolved when a line actually holds a match.
    let mut root: Option<std::path::PathBuf> = None;
    for span in ansi_spans(line, base) {
        let content = span.content.as_ref();
        if crate::ui::hyperlink::find_file_refs(content).is_empty() {
            spans.push(span);
            continue;
        }
        let root = root.get_or_insert_with(|| std::env::current_dir().unwrap_or_default());
        spans.extend(crate::ui::hyperlink::linkify_file_refs(
            content, span.style, root,
        ));
    }
    Line::from(spans)
}

/// A short-lived builder for tool-panel body rows. Owns the flood-cap and
/// hidden-content logic so renderers and the transcript's thin wrappers share
/// one implementation (no duplicated summary/flood logic). `width` is the
/// transcript wrap width.
struct PanelBody {
    width: usize,
    indent: usize,
    rows: Vec<TranscriptRow>,
}

impl PanelBody {
    fn new(width: usize) -> Self {
        Self {
            width,
            indent: 0,
            rows: Vec::new(),
        }
    }

    fn shell(width: usize) -> Self {
        Self::new(width)
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
            self.payload_body(&payload.body);
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
        let text = truncate_chars(text, MAX_TOOL_OUTPUT_LINE_CHARS);
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
        let clamped = truncate_chars(text, MAX_TOOL_OUTPUT_LINE_CHARS);
        self.line(&format!("  {clamped}"), dim_style());
    }

    /// Heredoc body, rendered whole. Collapse is binary and owned by the block
    /// header: an over-budget block arrives collapsed instead of eliding rows.
    fn payload_body(&mut self, body: &[String]) {
        for line in body {
            self.payload_line(line);
        }
    }

    /// The SHELL `$ command` row with the timeout rendered as right-aligned
    /// invocation metadata (never inside the command text). A positive timeout
    /// hugs the right border on the same row when it fits; otherwise it drops to
    /// its own right-aligned row so a long command is never truncated to make
    /// room. `Some(0)` ("no timeout") and `None` omit the field entirely.
    fn command_row(&mut self, command: &str, timeout: Option<u64>) {
        let command = truncate_chars(command, MAX_TOOL_OUTPUT_LINE_CHARS);
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
        let line = truncate_chars(raw, MAX_TOOL_OUTPUT_LINE_CHARS);
        let legacy = if first {
            format!("  \u{2514} {line}")
        } else {
            format!("    {line}")
        };
        self.push_line(
            tool_output_line("", &line, stdout_style()),
            strip_ansi_for_text(&legacy),
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
        for (i, raw) in content.lines().enumerate() {
            self.output_line(raw, i == 0);
        }
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
                self.output_line(&clamped, offset == 0);
            }
            return;
        }
        // The earlier-lines marker, then the most recent tail.
        self.hidden_lines_marker(start, true);
        for raw in &lines[start..] {
            let clamped = clamp_output_line(raw, width, MAX_TOOL_OUTPUT_ROWS);
            self.output_line(&clamped, false);
        }
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
    fn shell_output_sanitizes_plain_tabs_carriage_returns_and_osc() {
        let args = json!({ "command": "cat Makefile" });
        let renderer = resolve(&call("bash", args.clone()));
        let ctx = RenderCtx { width: 64 };
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
        assert!(output.text.contains("a   bclobbersafe"), "{}", output.text);
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
        let ctx = RenderCtx { width: 80 };
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
        let ctx = RenderCtx { width: 80 };
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
        let ctx = RenderCtx { width: 80 };
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
            &RenderCtx { width: 120 },
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
            &RenderCtx { width: 80 },
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
    fn shell_long_output_body_is_capped_without_exit_row_or_fold_hints() {
        // The frameless SHELL body is the flood-capped output alone: the exit
        // status lives in the footer and there is no fold-affordance chrome.
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
        let ctx = RenderCtx { width: 80 };
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
