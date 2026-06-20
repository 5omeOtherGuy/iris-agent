use std::io::{self, BufRead, BufReader, IsTerminal, Stderr, Stdin, Stdout, Write};

use anyhow::Result;

use crate::approval::parse_decision;
use crate::nexus::{ApprovalDecision, ToolCall};
use crate::tool_display::{exploration_summary, fold, is_exploration_tool, run_target, summarize};
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

    /// Render a left-gutter frame: a titled top rule, gutter-prefixed body
    /// lines, and a closing rule. Open on the right so arbitrary-width content
    /// needs no width math or truncation, and degrades cleanly without color.
    fn write_frame(&mut self, title: &str, body: &[String]) -> Result<()> {
        let ansi = self.ansi;
        let bar = |glyph: &str| sgr(ansi, "2", glyph);
        writeln!(self.out, "{} {}", bar("\u{256d}\u{2500}"), title)?;
        for line in body {
            writeln!(self.out, "{} {}", bar("\u{2502}"), line)?;
        }
        writeln!(self.out, "{}", bar("\u{2570}\u{2500}"))?;
        Ok(())
    }

    /// Colorize a unified diff into gutter body lines. The leading two file
    /// headers (`--- a/..`, `+++ b/..`) are dropped (the frame title already
    /// names the file); hunk headers are cyan, additions green, removals red,
    /// context dimmed. Header detection is stateful (headers only precede the
    /// first `@@`) so added content beginning with `+`/`-` is never mistaken
    /// for a header.
    fn diff_body(&self, diff: &str) -> Vec<String> {
        let mut seen_hunk = false;
        let mut body = Vec::new();
        for line in diff.lines() {
            if !seen_hunk && (line.starts_with("--- ") || line.starts_with("+++ ")) {
                continue;
            }
            if line.starts_with("@@") {
                seen_hunk = true;
                body.push(sgr(self.ansi, "36", line));
                continue;
            }
            let code = match line.chars().next() {
                Some('+') => "32",
                Some('-') => "31",
                _ => "2",
            };
            body.push(sgr(self.ansi, code, line));
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
                sgr(
                    self.ansi,
                    "2",
                    &format!(
                        "… +{} lines (ctrl + t to view transcript)",
                        folded.hidden_lines
                    ),
                )
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
            // as a continuation; rare, and raw-mode (Alt+Enter) is the upgrade.
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
            UiEvent::ToolResult { call, content } => {
                self.finish_assistant_stream()?;
                if is_exploration_tool(&call) {
                    self.write_explored(&call)?;
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
    ) -> Result<ApprovalDecision> {
        self.finish_assistant_stream()?;
        self.start_block()?;
        let summary = summarize(call);
        let prompt = if allow_always {
            "[y] once  [a] always this session  [N] deny"
        } else {
            "[y] once  [N] deny"
        };
        loop {
            let options = sgr(self.ansi, "2", prompt);
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
            if matches!(trimmed.as_str(), "" | "y" | "yes" | "n" | "no") || (always && allow_always)
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
                    ApprovalDecision::Deny => {}
                }
                // Leave `in_tool_block` set so the result/denied row Nexus emits
                // next attaches to this same block (no extra separator).
                return Ok(decision);
            }

            let retry = if allow_always {
                "please answer y, a, or n"
            } else {
                "please answer y or n"
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
            ui.request_approval(&call("write"), true)?,
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
            ui.request_approval(&call("write"), true)?,
            ApprovalDecision::AllowAlways
        );
        Ok(())
    }

    #[test]
    fn approval_without_allow_always_offers_yn_only_and_rejects_always() -> Result<()> {
        // allow_always=false: prompt omits the "always" choice and "a" is invalid.
        let mut ui = TextUi::new("a\ny\n".as_bytes(), Vec::new(), Vec::new());
        assert_eq!(
            ui.request_approval(&call("bash"), false)?,
            ApprovalDecision::Allow
        );
        let (_, out, _) = ui.into_parts();
        let rendered = String::from_utf8(out)?;
        assert!(!rendered.contains("always"));
        assert!(rendered.contains("please answer y or n"));
        Ok(())
    }

    #[test]
    fn approval_eof_denies() -> Result<()> {
        let mut ui = TextUi::new("".as_bytes(), Vec::new(), Vec::new());

        assert_eq!(
            ui.request_approval(&call("write"), true)?,
            ApprovalDecision::Deny
        );
        Ok(())
    }

    #[test]
    fn approval_reprompts_after_invalid_line() -> Result<()> {
        let mut ui = TextUi::new("huh?\ny\n".as_bytes(), Vec::new(), Vec::new());

        assert_eq!(
            ui.request_approval(&call("write"), true)?,
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
            ui.request_approval(&call("write"), true)?,
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
    fn pasted_block_is_one_prompt_without_leak() -> Result<()> {
        // A bracketed paste of three lines must become one prompt; nothing may
        // spill into the following approval read.
        let input = format!("{PASTE_START}line1\nline2\nline3{PASTE_END}\ny\n");
        let mut ui = TextUi::new(input.as_bytes(), Vec::new(), Vec::new());

        assert_eq!(ui.next_prompt()?.as_deref(), Some("line1\nline2\nline3\n"));
        // The very next read is the approval answer, proving no paste line leaked.
        assert_eq!(
            ui.request_approval(&call("write"), true)?,
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
        })?;
        let (_, out, _) = ui.into_parts();
        let rendered = String::from_utf8(out)?;
        assert!(rendered.contains("• Ran printf rows"));
        assert!(rendered.contains("  └ row 0"));
        assert!(rendered.contains("    row 1"));
        assert!(!rendered.contains("row 39"));
        assert!(rendered.contains("… +28 lines (ctrl + t to view transcript)"));
        Ok(())
    }
}
