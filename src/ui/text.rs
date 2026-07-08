use std::io::{self, BufRead, BufReader, IsTerminal, Stderr, Stdin, Stdout, Write};

use anyhow::Result;

use crate::approval::parse_decision;
use crate::nexus::{ApprovalDecision, ReviewContext, ToolCall};
use crate::tool_display::{
    APPROVAL_ALL_DIRTY_LABEL, APPROVAL_DESTRUCTIVE_NOTE, approval_dirty_note, approval_reason_lead,
    exploration_summary, fold, is_exploration_tool, run_target, summarize,
};
use crate::ui::{TurnErrorKind, Ui, UiEvent};

// Bracketed-paste control sequences. Enabling makes the terminal wrap pasted
// input in START/END markers, so a multi-line paste arrives as one block we can
// fold into a single prompt instead of leaking trailing lines into the next
// prompt or an approval. Marker parsing runs unconditionally (tests feed the
// markers directly); the enable/disable toggles are emitted only on a TTY.
const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";
const PASTE_ENABLE: &str = "\x1b[?2004h";
const PASTE_DISABLE: &str = "\x1b[?2004l";

/// Startup banner. Plain box-drawing so it renders the same on every terminal;
/// the caller colors it only when ANSI is enabled. The mockup's "Churned for ..."
/// line is intentionally omitted: nothing has run at startup, so a time there
/// would be fake.
const BANNER_LINES: &[&str] = &[
    "\u{256d}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256e}",
    "\u{2502}                               \u{2502}",
    "\u{2502}   \u{2588}\u{2588}       \u{2588}\u{2588}                 \u{2502}",
    "\u{2502}   \u{2588}\u{2588} \u{2588}\u{2588}\u{2584}\u{2584}\u{2596} \u{2588}\u{2588} \u{2584}\u{2588}\u{2588}\u{2588}\u{2588}           \u{2502}",
    "\u{2502}   \u{2588}\u{2588} \u{2588}\u{2588}\u{2588}\u{2580}\u{2598} \u{2588}\u{2588} \u{2580}\u{2580}\u{2580}\u{2588}\u{2588}           \u{2502}",
    "\u{2502}   \u{2588}\u{2588} \u{2588}\u{2588}    \u{2588}\u{2588} \u{2584}\u{2584}\u{2584}\u{2588}\u{2588}           \u{2502}",
    "\u{2502}   \u{2588}\u{2588} \u{2588}\u{2588}    \u{2588}\u{2588} \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}           \u{2502}",
    "\u{2502}                               \u{2502}",
    "\u{2502}   \"I'd ship this one!\"        \u{2502}",
    "\u{2502}        \u{2014} Claude Code, 2026    \u{2502}",
    "\u{2502}                               \u{2502}",
    "\u{2570}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{256f}",
];

/// Wrap `text` in an SGR color sequence when `ansi` is set; otherwise return it
/// untouched so piped/CI output stays free of escape codes.
fn sgr(ansi: bool, code: &str, text: &str) -> String {
    if ansi {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

// Width/strip are delegated to the unified text engine. The previous local
// `visible_width` used `chars().count()`, which miscounts CJK/emoji (a wide
// glyph is 2 columns but 1 char); the frame in `write_frame` is built from
// `visible_width`, so that bug produced misaligned right borders and a frame
// that overflowed for non-ASCII content. `crate::ui::textengine::visible_width`
// measures display columns over grapheme clusters after stripping ANSI, fixing
// the alignment. The local strippers were also removed in favor of the engine's
// single ANSI/OSC/APC parser.
use unicode_segmentation::UnicodeSegmentation;

use crate::ui::textengine::{cluster_width, strip_ansi, truncate_to_width, visible_width};

/// Truncate to at most `max` display columns after stripping ANSI, on a
/// grapheme-cluster boundary. The whole frame path measures by display column
/// (fits-check, hard-wrap, and padding), so the title is column-bounded too;
/// for ASCII this is identical to the previous char-count truncation.
fn truncate_plain(input: &str, max: usize) -> String {
    truncate_to_width(&strip_ansi(input), max)
}

/// Hard-wrap a frame body line to `width` display columns. If it already fits,
/// the original (ANSI included) is returned so colored diff lines keep their
/// color; otherwise ANSI is stripped and the line is hard-wrapped at grapheme
/// boundaries by display width. Wrapping by columns (not cluster count) keeps
/// the fixed-width frame aligned for CJK/emoji content while staying
/// byte-identical for ASCII (one cluster == one column).
fn hard_wrap_frame_line(input: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if visible_width(input) <= width {
        return vec![input.to_string()];
    }
    let stripped = strip_ansi(input);
    if stripped.is_empty() {
        return vec![String::new()];
    }
    let mut rows = Vec::new();
    let mut current = String::new();
    let mut used = 0usize;
    for cluster in stripped.graphemes(true) {
        let w = cluster_width(cluster).max(1);
        if used + w > width && used > 0 {
            rows.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push_str(cluster);
        used += w;
    }
    rows.push(current);
    rows
}

pub(crate) struct TextUi<R, W, E> {
    input: R,
    out: W,
    err: E,
    // Color/structure are emitted only on a TTY; captured output (tests, pipes)
    // stays ANSI-free so block-drawing and color never become garbage.
    ansi: bool,
    // Whether to toggle bracketed paste on the terminal (TTY stdin+stdout).
    paste_terminal: bool,
    assistant_stream_open: bool,
    // True while rendering one tool's block (proposal/diff/approval/result), so
    // we emit exactly one blank-line separator before each block, not between
    // its sub-parts.
    in_tool_block: bool,
    exploring_open: bool,
}

impl TextUi<BufReader<Stdin>, Stdout, Stderr> {
    pub(crate) fn stdio() -> Self {
        let ansi = io::stdout().is_terminal();
        let paste_terminal = ansi && io::stdin().is_terminal();
        let mut ui = Self::new(BufReader::new(io::stdin()), io::stdout(), io::stderr());
        ui.ansi = ansi;
        ui.paste_terminal = paste_terminal;
        ui
    }
}

impl<R, W, E> TextUi<R, W, E> {
    pub(crate) fn new(input: R, out: W, err: E) -> Self {
        Self {
            input,
            out,
            err,
            ansi: false,
            paste_terminal: false,
            assistant_stream_open: false,
            in_tool_block: false,
            exploring_open: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_ansi(mut self, ansi: bool) -> Self {
        self.ansi = ansi;
        self
    }

    #[cfg(test)]
    pub(crate) fn into_parts(self) -> (R, W, E) {
        (self.input, self.out, self.err)
    }
}

impl<R: BufRead, W: Write, E: Write> TextUi<R, W, E> {
    fn finish_assistant_stream(&mut self) -> Result<()> {
        if self.assistant_stream_open {
            writeln!(self.out)?;
            self.out.flush()?;
            self.assistant_stream_open = false;
        }
        Ok(())
    }

    /// Emit a single blank-line separator at the start of a tool block.
    fn start_block(&mut self) -> Result<()> {
        if !self.in_tool_block {
            writeln!(self.out)?;
            self.in_tool_block = true;
            self.exploring_open = false;
        }
        Ok(())
    }

    /// Render a square single-line frame. Keep this visually aligned with the
    /// TUI panels; rounded frames become unreadable when copied back into the
    /// terminal transcript and wrapped by Markdown.
    fn write_frame(&mut self, title: &str, body: &[String]) -> Result<()> {
        let ansi = self.ansi;
        let bar = |glyph: &str| sgr(ansi, "2", glyph);
        const WIDTH: usize = 96;
        let title = truncate_plain(title, WIDTH.saturating_sub(4));
        let title_cell = format!(" {title} ");
        // Display columns, not char count, so a CJK/emoji title does not
        // overflow the fixed-width top border (ASCII is unchanged).
        let top_fill = WIDTH.saturating_sub(2 + visible_width(&title_cell));
        writeln!(
            self.out,
            "{}{}{}",
            bar("┌"),
            bar(&title_cell),
            bar(&format!("{}┐", "─".repeat(top_fill)))
        )?;
        for line in body {
            for physical in hard_wrap_frame_line(line, WIDTH.saturating_sub(4)) {
                writeln!(
                    self.out,
                    "{} {}{} {}",
                    bar("│"),
                    physical,
                    " ".repeat(WIDTH.saturating_sub(4 + visible_width(&physical))),
                    bar("│")
                )?;
            }
        }
        writeln!(
            self.out,
            "{}{}{}",
            bar("└"),
            bar(&"─".repeat(WIDTH - 2)),
            bar("┘")
        )?;
        Ok(())
    }

    /// Colorize a unified diff into gutter body lines. Every file-header pair
    /// (`--- a/..` immediately followed by `+++ b/..` and a `@@` hunk) is
    /// dropped -- not just the first -- so a multi-file diff never renders
    /// later files' headers as change lines (the frame title names the file).
    /// Hunk headers are cyan, additions green, removals red, context dimmed.
    /// The `@@` guard keeps a removed content line beginning `-- ` from being
    /// mistaken for a header.
    fn diff_body(&self, diff: &str) -> Vec<String> {
        let lines: Vec<&str> = diff.lines().collect();
        let mut body = Vec::new();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i];
            if crate::ui::is_diff_file_header(&lines, i) {
                i += 2; // skip the `--- `/`+++ ` pair; the `@@` renders next pass
                continue;
            }
            if line.starts_with("@@") {
                body.push(sgr(self.ansi, "36", line));
                i += 1;
                continue;
            }
            let code = match line.chars().next() {
                Some('+') => "32",
                Some('-') => "31",
                _ => "2",
            };
            body.push(sgr(self.ansi, code, line));
            i += 1;
        }
        body
    }

    fn write_header(&mut self, marker_code: &str, marker: &str, head: &str) -> Result<()> {
        self.start_block()?;
        writeln!(self.out, "{} {}", sgr(self.ansi, marker_code, marker), head)?;
        Ok(())
    }

    fn write_tool_output(&mut self, content: &str) -> Result<()> {
        if content.is_empty() {
            writeln!(self.out, "  └ {}", sgr(self.ansi, "2", "(no output)"))?;
            self.in_tool_block = false;
            self.exploring_open = false;
            return Ok(());
        }

        let folded = fold(content);
        let mut first = true;
        for line in folded.preview.lines() {
            let prefix = if first { "  └ " } else { "    " };
            writeln!(
                self.out,
                "{}{}",
                sgr(self.ansi, "2", prefix),
                sgr(self.ansi, "2", line)
            )?;
            first = false;
        }
        if folded.hidden_lines > 0 {
            writeln!(
                self.out,
                "    {}",
                sgr(self.ansi, "2", &format!("… +{} lines", folded.hidden_lines),)
            )?;
        }
        self.in_tool_block = false;
        self.exploring_open = false;
        Ok(())
    }

    fn write_explored(&mut self, call: &ToolCall) -> Result<()> {
        if !self.exploring_open {
            self.start_block()?;
            writeln!(self.out, "{} Explored", sgr(self.ansi, "2", "•"))?;
            self.exploring_open = true;
        }
        writeln!(
            self.out,
            "{} {}",
            sgr(self.ansi, "2", "  └"),
            exploration_summary(call)
        )?;
        self.in_tool_block = false;
        Ok(())
    }

    /// Strip bracketed-paste markers from one raw line, toggling `in_paste`, and
    /// report whether any marker was present.
    fn strip_paste_markers(line: &str, in_paste: &mut bool) -> (String, bool) {
        if !line.contains('\x1b') {
            return (line.to_string(), false);
        }
        let mut out = String::with_capacity(line.len());
        let mut rest = line;
        let mut had = false;
        loop {
            let start = rest.find(PASTE_START);
            let end = rest.find(PASTE_END);
            let next = match (start, end) {
                (None, None) => {
                    out.push_str(rest);
                    break;
                }
                (Some(s), None) => (s, PASTE_START, true),
                (None, Some(e)) => (e, PASTE_END, false),
                (Some(s), Some(e)) if s <= e => (s, PASTE_START, true),
                (Some(_), Some(e)) => (e, PASTE_END, false),
            };
            let (pos, marker, paste_on) = next;
            out.push_str(&rest[..pos]);
            *in_paste = paste_on;
            had = true;
            rest = &rest[pos + marker.len()..];
        }
        (out, had)
    }

    /// Read one logical prompt: a bracketed-paste block collapses into a single
    /// (possibly multi-line) prompt, and a typed line ending in `\` continues on
    /// the next line. A single typed line returns immediately (no slowdown).
    /// Returns `None` only at EOF with nothing buffered.
    fn read_logical_line(&mut self) -> Result<Option<String>> {
        let mut buf = String::new();
        let mut in_paste = false;
        let mut saw_paste = false;
        let mut got_any = false;
        loop {
            let mut line = String::new();
            if self.input.read_line(&mut line)? == 0 {
                if !got_any {
                    return Ok(None);
                }
                break;
            }
            got_any = true;
            let (cleaned, had_marker) = Self::strip_paste_markers(&line, &mut in_paste);
            saw_paste |= had_marker;
            buf.push_str(&cleaned);
            if in_paste {
                continue;
            }
            // Backslash continuation is a typed-input convenience only; never
            // reinterpret bytes that came from inside a paste.
            // ponytail: a trailing literal backslash in typed input is treated
            // as a continuation; rare, and the full-screen multiline editor is
            // the upgrade.
            if !saw_paste && let Some(stripped) = buf.strip_suffix("\\\n") {
                buf = format!("{stripped}\n");
                continue;
            }
            break;
        }
        Ok(Some(buf))
    }
}

impl<R: BufRead, W: Write, E: Write> Ui for TextUi<R, W, E> {
    fn next_prompt(&mut self) -> Result<Option<String>> {
        self.finish_assistant_stream()?;
        self.in_tool_block = false;
        self.exploring_open = false;
        write!(self.out, "{} ", sgr(self.ansi, "1;36", "iris>"))?;
        self.out.flush()?;

        match self.read_logical_line()? {
            Some(line) => Ok(Some(line)),
            None => {
                writeln!(self.out)?;
                Ok(None)
            }
        }
    }

    fn emit(&mut self, event: UiEvent) -> Result<()> {
        match event {
            UiEvent::ProviderTurnStarted { .. }
            | UiEvent::ProviderTurnCompleted { .. }
            | UiEvent::ProviderTurnCancelled { .. }
            | UiEvent::ProviderTurnError { .. }
            | UiEvent::ToolLifecycle { .. }
            | UiEvent::OutputHandleStored { .. }
            | UiEvent::CompactionApplied { .. }
            | UiEvent::FoldApplied { .. } => {}
            UiEvent::CompactionLifecycle {
                state,
                covered_messages,
                original_tokens_estimate,
                message,
                ..
            } => {
                self.finish_assistant_stream()?;
                self.in_tool_block = false;
                self.exploring_open = false;
                let detail = message.unwrap_or_else(|| {
                    format!(
                        "background compaction {} ({} message(s), ~{} tokens)",
                        state.as_str(),
                        covered_messages,
                        original_tokens_estimate
                    )
                });
                writeln!(
                    self.out,
                    "{}",
                    sgr(self.ansi, "2", &format!("note: {detail}"))
                )?;
            }
            UiEvent::SessionStarted => {
                self.finish_assistant_stream()?;
                self.in_tool_block = false;
                self.exploring_open = false;
                for line in BANNER_LINES {
                    writeln!(self.out, "{}", sgr(self.ansi, "38;5;213", line))?;
                }
                writeln!(self.out, "Type /exit to quit.")?;
                if self.ansi {
                    writeln!(
                        self.out,
                        "{}",
                        sgr(
                            true,
                            "2",
                            "multi-line: end a line with \\ to continue \u{b7} paste is safe",
                        )
                    )?;
                }
                if self.paste_terminal {
                    write!(self.out, "{PASTE_ENABLE}")?;
                    self.out.flush()?;
                }
            }
            UiEvent::AssistantText(text) => {
                self.finish_assistant_stream()?;
                self.in_tool_block = false;
                self.exploring_open = false;
                writeln!(self.out, "{} {text}", sgr(self.ansi, "1;35", "assistant>"))?;
            }
            UiEvent::AssistantTextDelta(delta) => {
                if !self.assistant_stream_open {
                    self.in_tool_block = false;
                    self.exploring_open = false;
                    write!(self.out, "{} ", sgr(self.ansi, "1;35", "assistant>"))?;
                    self.assistant_stream_open = true;
                }
                write!(self.out, "{delta}")?;
                self.out.flush()?;
            }
            UiEvent::AssistantTextEnd(_) => {
                self.finish_assistant_stream()?;
            }
            UiEvent::AssistantReasoning { .. }
            | UiEvent::AssistantReasoningDelta(_)
            | UiEvent::AssistantReasoningSectionBreak
            | UiEvent::AssistantRawReasoningDelta(_) => {
                // The non-interactive text fallback intentionally does not render
                // reasoning/thinking blocks (block or streamed): that is a TUI
                // surface. Ignored here so the fallback's output is unchanged.
            }
            UiEvent::ToolProposed(_call) => {
                // Non-gated tools (read/grep/find/ls) show only their result row;
                // claim the block separator here so it is not double-counted.
                self.finish_assistant_stream()?;
                self.start_block()?;
            }
            UiEvent::ToolAutoApproved(call) => {
                self.finish_assistant_stream()?;
                self.write_header(
                    "32",
                    "✔",
                    &format!(
                        "You approved iris to run {} this session",
                        run_target(&call)
                    ),
                )?;
            }
            UiEvent::ToolReview { call, .. } => {
                // Non-interactive: the affordance can't be offered here; surface
                // the pending review honestly. The decision resolves via the
                // following ToolStarted/ToolDenied event.
                self.finish_assistant_stream()?;
                self.write_header(
                    "33",
                    "▲",
                    &format!("Awaiting approval to run {}", run_target(&call)),
                )?;
            }
            UiEvent::DiffPreview { call, diff } => {
                self.finish_assistant_stream()?;
                self.start_block()?;
                let title = format!("diff \u{b7} {}", summarize(&call));
                let body = self.diff_body(&diff);
                self.write_frame(&title, &body)?;
            }
            UiEvent::ToolDenied(call) => {
                self.finish_assistant_stream()?;
                self.write_header("31", "✗", &format!("Denied {}", run_target(&call)))?;
                self.in_tool_block = false;
                self.exploring_open = false;
            }
            UiEvent::ToolStarted(_)
            | UiEvent::ToolOutputDelta { .. }
            | UiEvent::ToolInputDelta { .. } => {
                // Non-interactive fallback: no live viewport, so a running tool
                // stays buffered until its whole result arrives. Streaming
                // deltas (output or freeform tool input) and the start signal
                // render nothing here.
            }
            UiEvent::ToolResult {
                call,
                content,
                exit_code,
                ..
            } => {
                self.finish_assistant_stream()?;
                if is_exploration_tool(&call) {
                    self.write_explored(&call)?;
                } else if exit_code.is_some_and(|code| code != 0) {
                    self.write_header("31", "✗", &format!("Ran {}", run_target(&call)))?;
                    self.write_tool_output(&content)?;
                } else {
                    self.write_header("2", "•", &format!("Ran {}", run_target(&call)))?;
                    self.write_tool_output(&content)?;
                }
            }
            UiEvent::ToolError { call, message } => {
                self.finish_assistant_stream()?;
                self.write_header("31", "✗", &format!("Ran {}", run_target(&call)))?;
                writeln!(
                    self.out,
                    "{}{}",
                    sgr(self.ansi, "2", "  └ "),
                    sgr(self.ansi, "31", &format!("error: {message}"))
                )?;
                self.in_tool_block = false;
                self.exploring_open = false;
            }
            UiEvent::ToolCancelled(call) => {
                self.finish_assistant_stream()?;
                self.write_header("2", "•", &format!("Cancelled {}", run_target(&call)))?;
                self.in_tool_block = false;
                self.exploring_open = false;
            }
            UiEvent::UserMessage(text) => {
                // A mid-run injected user message (steering/follow-up). The text
                // path does not enqueue steering today, so this only arrives if
                // a future caller wires it; echo it as a user line for parity.
                self.finish_assistant_stream()?;
                self.in_tool_block = false;
                self.exploring_open = false;
                writeln!(self.out, "{}", sgr(self.ansi, "1", &format!("> {text}")))?;
            }
            UiEvent::Notice(message) => {
                self.finish_assistant_stream()?;
                self.in_tool_block = false;
                self.exploring_open = false;
                writeln!(
                    self.out,
                    "{}",
                    sgr(self.ansi, "2", &format!("note: {message}"))
                )?;
            }
            UiEvent::TaskDiff { summary, diff } => {
                // The final task diff (issue #264): the per-file summary followed
                // by the colorized unified diff, in one frame -- the same
                // colorizer the DiffPreview arm uses.
                self.finish_assistant_stream()?;
                self.start_block()?;
                let mut body = summary;
                body.extend(self.diff_body(&diff));
                self.write_frame("task diff", &body)?;
            }
            UiEvent::TurnError { kind, message } => {
                self.finish_assistant_stream()?;
                match kind {
                    TurnErrorKind::Auth => {
                        writeln!(self.err, "auth error: {message}")?;
                        writeln!(
                            self.err,
                            "authentication required; re-run the login command"
                        )?;
                    }
                    TurnErrorKind::Provider => {
                        writeln!(self.err, "provider error: {message}")?;
                    }
                }
            }
            UiEvent::TurnComplete => {
                self.finish_assistant_stream()?;
                self.in_tool_block = false;
                self.exploring_open = false;
            }
        }
        Ok(())
    }

    fn request_approval(
        &mut self,
        call: &ToolCall,
        allow_always: bool,
        allow_project: bool,
        ctx: &ReviewContext,
    ) -> Result<ApprovalDecision> {
        self.finish_assistant_stream()?;
        self.start_block()?;
        let summary = summarize(call);
        // The explanatory reason line, printed once above the prompt: the muted
        // base sentence, the danger-toned destructive clause, then the muted
        // dirty-tree clause. Same deterministic Tier-3 copy the TUI renders.
        let dirty_gate = !ctx.dirty_paths.is_empty();
        let mut reason = approval_reason_lead(call);
        if ctx.destructive {
            reason.push(' ');
            reason.push_str(APPROVAL_DESTRUCTIVE_NOTE);
        }
        if let Some(note) = approval_dirty_note(&ctx.dirty_paths, usize::MAX) {
            reason.push(' ');
            reason.push_str(&note);
        }
        writeln!(self.out, "{}", sgr(self.ansi, "2", &reason))?;
        // In the dirty-tree context "always" means "all dirty files (this
        // task)" (ADR-0028), and no per-project grant is offered.
        let always_label = if dirty_gate {
            format!("[a] {APPROVAL_ALL_DIRTY_LABEL}")
        } else {
            "[a] always this session".to_string()
        };
        let prompt = match (allow_always, allow_project) {
            (true, true) => {
                format!("[y] once  {always_label}  [p] always for this project  [N] deny")
            }
            (true, false) => format!("[y] once  {always_label}  [N] deny"),
            (false, true) => "[y] once  [p] always for this project  [N] deny".to_string(),
            (false, false) => "[y] once  [N] deny".to_string(),
        };
        loop {
            let options = sgr(self.ansi, "2", &prompt);
            write!(self.out, "approve {summary}?  {options} \u{203a} ")?;
            self.out.flush()?;

            // Read through the same paste-safe path as the prompt: a multi-line
            // paste collapses into one buffer, fails the single-token check
            // below, and re-prompts instead of leaking lines into the next read.
            let Some(cleaned) = self.read_logical_line()? else {
                writeln!(self.out)?;
                return Ok(ApprovalDecision::Deny);
            };

            let trimmed = cleaned.trim().to_ascii_lowercase();
            let always = matches!(trimmed.as_str(), "a" | "always");
            let project = matches!(trimmed.as_str(), "p" | "project");
            if matches!(trimmed.as_str(), "" | "y" | "yes" | "n" | "no")
                || (always && allow_always)
                || (project && allow_project)
            {
                let decision = parse_decision(&cleaned);
                match decision {
                    ApprovalDecision::Allow => {
                        self.write_header(
                            "32",
                            "✔",
                            &format!("You approved iris to run {} this time", run_target(call)),
                        )?;
                    }
                    ApprovalDecision::AllowAlways => {
                        self.write_header(
                            "32",
                            "✔",
                            &format!("You approved iris to run {} this session", run_target(call)),
                        )?;
                    }
                    ApprovalDecision::AllowProject => {
                        self.write_header(
                            "32",
                            "✔",
                            &format!(
                                "You approved iris to run {} for this project",
                                run_target(call)
                            ),
                        )?;
                    }
                    ApprovalDecision::Deny => {}
                }
                // Leave `in_tool_block` set so the result/denied row Nexus emits
                // next attaches to this same block (no extra separator).
                return Ok(decision);
            }

            let mut keys = vec!["y"];
            if allow_always {
                keys.push("a");
            }
            if allow_project {
                keys.push("p");
            }
            let retry = match keys.as_slice() {
                [only] => format!("please answer {only} or n"),
                many => format!("please answer {}, or n", many.join(", ")),
            };
            writeln!(self.out, "{retry}")?;
        }
    }

    fn shutdown(&mut self) -> Result<()> {
        self.finish_assistant_stream()?;
        if self.paste_terminal {
            write!(self.out, "{PASTE_DISABLE}")?;
            self.out.flush()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        call_args(name, json!({ "path": "note.txt", "content": "hi" }))
    }

    fn call_args(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
            name: name.to_string(),
            arguments,
        }
    }

    #[test]
    fn prompt_returns_single_line_with_newline() -> Result<()> {
        let mut ui = TextUi::new("hello\ny\n".as_bytes(), Vec::new(), Vec::new());
        assert_eq!(ui.next_prompt()?.as_deref(), Some("hello\n"));
        Ok(())
    }

    #[test]
    fn startup_banner_has_no_fake_timing() -> Result<()> {
        let mut ui = TextUi::new("".as_bytes(), Vec::new(), Vec::new());
        ui.emit(UiEvent::SessionStarted)?;
        let (_, out, _) = ui.into_parts();
        let rendered = String::from_utf8(out)?;
        assert!(!rendered.contains("Churned for"));
        Ok(())
    }

    #[test]
    fn prompt_and_approval_share_one_input_owner() -> Result<()> {
        let mut ui = TextUi::new("hello\ny\n".as_bytes(), Vec::new(), Vec::new());

        assert_eq!(ui.next_prompt()?.as_deref(), Some("hello\n"));
        assert_eq!(
            ui.request_approval(&call("write"), true, false, &ReviewContext::default())?,
            ApprovalDecision::Allow
        );

        let (_, out, err) = ui.into_parts();
        assert!(String::from_utf8(out)?.contains("approve write note.txt?"));
        assert!(err.is_empty());
        Ok(())
    }

    #[test]
    fn approval_always_is_parsed() -> Result<()> {
        let mut ui = TextUi::new("a\n".as_bytes(), Vec::new(), Vec::new());
        assert_eq!(
            ui.request_approval(&call("write"), true, false, &ReviewContext::default())?,
            ApprovalDecision::AllowAlways
        );
        Ok(())
    }

    #[test]
    fn approval_without_allow_always_offers_yn_only_and_rejects_always() -> Result<()> {
        // allow_always=false: prompt omits the "always" choice and "a" is invalid.
        let mut ui = TextUi::new("a\ny\n".as_bytes(), Vec::new(), Vec::new());
        assert_eq!(
            ui.request_approval(&call("bash"), false, false, &ReviewContext::default())?,
            ApprovalDecision::Allow
        );
        let (_, out, _) = ui.into_parts();
        let rendered = String::from_utf8(out)?;
        assert!(!rendered.contains("always"));
        assert!(rendered.contains("please answer y or n"));
        Ok(())
    }

    #[test]
    fn approval_project_grant_is_parsed_when_offered() -> Result<()> {
        let mut ui = TextUi::new("p\n".as_bytes(), Vec::new(), Vec::new());
        assert_eq!(
            ui.request_approval(&call("write"), false, true, &ReviewContext::default())?,
            ApprovalDecision::AllowProject
        );
        let (_, out, _) = ui.into_parts();
        assert!(String::from_utf8(out)?.contains("always for this project"));
        Ok(())
    }

    #[test]
    fn approval_reason_line_and_dirty_relabel() -> Result<()> {
        // The reason line carries the base sentence, the destructive clause, and
        // the dirty-tree clause; the dirty gate relabels the `[a]` option.
        let ctx = ReviewContext {
            destructive: true,
            dirty_paths: vec!["src/main.rs".to_string()],
        };
        let mut ui = TextUi::new("y\n".as_bytes(), Vec::new(), Vec::new());
        ui.request_approval(&call("write"), true, false, &ctx)?;
        let (_, out, _) = ui.into_parts();
        let rendered = String::from_utf8(out)?;
        assert!(
            rendered.contains("Creates or overwrites note.txt."),
            "{rendered}"
        );
        assert!(rendered.contains("Flagged destructive"), "{rendered}");
        assert!(
            rendered.contains("Touches uncommitted user changes: src/main.rs"),
            "{rendered}"
        );
        assert!(
            rendered.contains("all dirty files (this task)"),
            "{rendered}"
        );
        Ok(())
    }

    #[test]
    fn approval_without_allow_project_rejects_p() -> Result<()> {
        // allow_project=false (e.g. a destructive call): "p" is invalid input
        // and re-prompts; it must never resolve to a project grant.
        let mut ui = TextUi::new("p\nn\n".as_bytes(), Vec::new(), Vec::new());
        assert_eq!(
            ui.request_approval(&call("bash"), false, false, &ReviewContext::default())?,
            ApprovalDecision::Deny
        );
        let (_, out, _) = ui.into_parts();
        let rendered = String::from_utf8(out)?;
        assert!(!rendered.contains("always for this project"));
        assert!(rendered.contains("please answer y or n"));
        Ok(())
    }

    #[test]
    fn approval_eof_denies() -> Result<()> {
        let mut ui = TextUi::new("".as_bytes(), Vec::new(), Vec::new());

        assert_eq!(
            ui.request_approval(&call("write"), true, false, &ReviewContext::default())?,
            ApprovalDecision::Deny
        );
        Ok(())
    }

    #[test]
    fn approval_reprompts_after_invalid_line() -> Result<()> {
        let mut ui = TextUi::new("huh?\ny\n".as_bytes(), Vec::new(), Vec::new());

        assert_eq!(
            ui.request_approval(&call("write"), true, false, &ReviewContext::default())?,
            ApprovalDecision::Allow
        );

        let (_, out, _) = ui.into_parts();
        let rendered = String::from_utf8(out)?;
        assert!(rendered.contains("please answer y, a, or n"));
        assert_eq!(rendered.matches("approve write note.txt?").count(), 2);
        Ok(())
    }

    #[test]
    fn pasted_block_at_approval_reprompts_without_leak() -> Result<()> {
        // A multi-line paste arriving at an approval prompt must not leak its
        // trailing lines into the next read; it collapses to one invalid answer
        // and re-prompts, then a real "y" allows.
        let input = format!("{PASTE_START}garbage1\ngarbage2{PASTE_END}\ny\n");
        let mut ui = TextUi::new(input.as_bytes(), Vec::new(), Vec::new());
        assert_eq!(
            ui.request_approval(&call("write"), true, false, &ReviewContext::default())?,
            ApprovalDecision::Allow
        );
        let (_, out, _) = ui.into_parts();
        let rendered = String::from_utf8(out)?;
        assert!(rendered.contains("please answer y, a, or n"));
        Ok(())
    }

    #[test]
    fn streaming_deltas_render_one_assistant_line() -> Result<()> {
        let mut ui = TextUi::new("".as_bytes(), Vec::new(), Vec::new());

        ui.emit(UiEvent::AssistantTextDelta("Hel".to_string()))?;
        ui.emit(UiEvent::AssistantTextDelta("lo".to_string()))?;
        ui.emit(UiEvent::AssistantTextEnd("Hello".to_string()))?;

        let (_, out, _) = ui.into_parts();
        assert_eq!(String::from_utf8(out)?, "assistant> Hello\n");
        Ok(())
    }

    #[test]
    fn plain_assistant_link_output_is_byte_identical_and_escape_free() -> Result<()> {
        // The non-TTY plain UI is excluded from OSC 8 hyperlinks by its ANSI-free
        // contract: a markdown link renders as the raw text verbatim, with no
        // escape bytes and no link markers.
        let mut ui = TextUi::new("".as_bytes(), Vec::new(), Vec::new());
        let text = "See [the guide](https://example.com/docs) now";
        ui.emit(UiEvent::AssistantText(text.to_string()))?;
        let (_, out, _) = ui.into_parts();
        let out = String::from_utf8(out)?;
        assert_eq!(out, format!("assistant> {text}\n"));
        // No OSC 8, no ESC, no APC link markers.
        assert!(!out.contains('\x1b'), "plain output must be escape-free");
        assert!(!out.contains("]8;"), "no OSC 8 in plain output");
        assert!(
            !out.contains(crate::ui::hyperlink::CLOSE_MARKER)
                && crate::ui::hyperlink::marker_uri(&out).is_none(),
            "no link markers in plain output"
        );
        Ok(())
    }

    #[test]
    fn pasted_block_is_one_prompt_without_leak() -> Result<()> {
        // A bracketed paste of three lines must become one prompt; nothing may
        // spill into the following approval read.
        let input = format!("{PASTE_START}line1\nline2\nline3{PASTE_END}\ny\n");
        let mut ui = TextUi::new(input.as_bytes(), Vec::new(), Vec::new());

        assert_eq!(ui.next_prompt()?.as_deref(), Some("line1\nline2\nline3\n"));
        // The very next read is the approval answer, proving no paste line leaked.
        assert_eq!(
            ui.request_approval(&call("write"), true, false, &ReviewContext::default())?,
            ApprovalDecision::Allow
        );
        Ok(())
    }

    #[test]
    fn backslash_continues_typed_multiline_prompt() -> Result<()> {
        let mut ui = TextUi::new("first\\\nsecond\n".as_bytes(), Vec::new(), Vec::new());
        assert_eq!(ui.next_prompt()?.as_deref(), Some("first\nsecond\n"));
        Ok(())
    }

    #[test]
    fn frame_helpers_measure_cjk_by_display_column() {
        // The frame width-bug fix: a 2-column CJK glyph counts as 2 columns, and
        // the body hard-wrap keeps every row within the field width (so the
        // fixed-width frame border stays aligned for non-ASCII content).
        assert_eq!(visible_width("\u{4e2d}\u{6587}"), 4);
        let rows = hard_wrap_frame_line("\u{4e2d}\u{6587}\u{5b57}\u{6570}", 4);
        assert!(rows.len() > 1, "CJK line should wrap: {rows:?}");
        for row in &rows {
            assert!(visible_width(row) <= 4, "row overran field width: {row:?}");
        }
        // ASCII parity: a line that fits is returned unchanged (color preserved).
        assert_eq!(
            hard_wrap_frame_line("\x1b[32mhi\x1b[0m", 8),
            vec!["\x1b[32mhi\x1b[0m"]
        );
        // Title truncation is column-bounded.
        assert_eq!(
            visible_width(&truncate_plain("\u{4e2d}\u{6587}\u{5b57}", 4)),
            4
        );
    }

    #[test]
    fn diff_preview_colorizes_and_drops_file_headers_with_ansi() -> Result<()> {
        let diff = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let mut ui = TextUi::new("".as_bytes(), Vec::new(), Vec::new()).with_ansi(true);
        ui.emit(UiEvent::DiffPreview {
            call: call("edit"),
            diff: diff.to_string(),
        })?;
        let (_, out, _) = ui.into_parts();
        let rendered = String::from_utf8(out)?;
        assert!(!rendered.contains("--- a/note.txt"), "file header shown");
        assert!(rendered.contains("\u{1b}[32m+new\u{1b}[0m"), "no green add");
        assert!(
            rendered.contains("\u{1b}[31m-old\u{1b}[0m"),
            "no red remove"
        );
        assert!(rendered.contains("diff \u{b7} edit note.txt"));
        Ok(())
    }

    #[test]
    fn task_diff_renders_summary_and_colorized_diff() -> Result<()> {
        // Issue #264: the text/non-TTY path shows the per-file summary plus the
        // unified diff through the same colorizer as DiffPreview.
        let diff = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let mut ui = TextUi::new("".as_bytes(), Vec::new(), Vec::new()).with_ansi(true);
        ui.emit(UiEvent::TaskDiff {
            summary: vec![
                "1 file changed, +1/-1".to_string(),
                "  +1/-1  note.txt".to_string(),
            ],
            diff: diff.to_string(),
        })?;
        let (_, out, _) = ui.into_parts();
        let rendered = String::from_utf8(out)?;
        assert!(rendered.contains("task diff"), "panel titled");
        assert!(rendered.contains("1 file changed, +1/-1"), "summary shown");
        assert!(!rendered.contains("--- a/note.txt"), "file header dropped");
        assert!(rendered.contains("\u{1b}[32m+new\u{1b}[0m"), "green add");
        assert!(rendered.contains("\u{1b}[31m-old\u{1b}[0m"), "red remove");
        Ok(())
    }

    #[test]
    fn two_file_diff_drops_every_header_pair_with_ansi() -> Result<()> {
        let diff = concat!(
            "--- a/one.txt\n+++ b/one.txt\n@@ -1 +1 @@\n-old1\n+new1\n",
            "--- a/two.txt\n+++ b/two.txt\n@@ -1 +1 @@\n-old2\n+new2\n"
        );
        let mut ui = TextUi::new("".as_bytes(), Vec::new(), Vec::new()).with_ansi(true);
        ui.emit(UiEvent::DiffPreview {
            call: call("edit"),
            diff: diff.to_string(),
        })?;
        let (_, out, _) = ui.into_parts();
        let rendered = String::from_utf8(out)?;
        assert!(
            !rendered.contains("--- a/two.txt"),
            "second file header shown"
        );
        assert!(
            !rendered.contains("+++ b/two.txt"),
            "second file header shown"
        );
        assert!(
            rendered.contains("\u{1b}[32m+new2\u{1b}[0m"),
            "second add not green"
        );
        assert!(
            rendered.contains("\u{1b}[31m-old2\u{1b}[0m"),
            "second remove not red"
        );
        Ok(())
    }

    #[test]
    fn non_tty_output_has_no_ansi_escapes() -> Result<()> {
        let diff = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let mut ui = TextUi::new("".as_bytes(), Vec::new(), Vec::new());
        ui.emit(UiEvent::DiffPreview {
            call: call("edit"),
            diff: diff.to_string(),
        })?;
        ui.emit(UiEvent::ToolResult {
            call: call("read"),
            content: "a\nb\n".to_string(),
            exit_code: None,
            duration: None,
        })?;
        let (_, out, _) = ui.into_parts();
        let rendered = String::from_utf8(out)?;
        assert!(!rendered.contains('\u{1b}'), "ANSI leaked into capture");
        Ok(())
    }

    #[test]
    fn long_result_folds_with_more_indicator() -> Result<()> {
        let content = (0..40)
            .map(|i| format!("row {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut ui = TextUi::new("".as_bytes(), Vec::new(), Vec::new());
        ui.emit(UiEvent::ToolResult {
            call: call_args("bash", json!({ "command": "printf rows" })),
            content,
            exit_code: None,
            duration: None,
        })?;
        let (_, out, _) = ui.into_parts();
        let rendered = String::from_utf8(out)?;
        assert!(rendered.contains("• Ran printf rows"));
        assert!(rendered.contains("  └ row 0"));
        assert!(rendered.contains("    row 1"));
        assert!(!rendered.contains("row 39"));
        assert!(rendered.contains("… +28 lines"));
        assert!(!rendered.contains("ctrl + t"), "no false transcript hint");
        Ok(())
    }
}
