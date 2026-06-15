use std::io::{self, BufRead, BufReader, Stderr, Stdin, Stdout, Write};

use anyhow::Result;

use crate::approval::{ApprovalDecision, parse_decision};
use crate::nexus::ToolCall;
use crate::ui::{TurnErrorKind, Ui, UiEvent};

pub(crate) struct TextUi<R, W, E> {
    input: R,
    out: W,
    err: E,
    assistant_stream_open: bool,
}

impl TextUi<BufReader<Stdin>, Stdout, Stderr> {
    pub(crate) fn stdio() -> Self {
        Self::new(BufReader::new(io::stdin()), io::stdout(), io::stderr())
    }
}

impl<R, W, E> TextUi<R, W, E> {
    pub(crate) fn new(input: R, out: W, err: E) -> Self {
        Self {
            input,
            out,
            err,
            assistant_stream_open: false,
        }
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
}

impl<R: BufRead, W: Write, E: Write> Ui for TextUi<R, W, E> {
    fn next_prompt(&mut self) -> Result<Option<String>> {
        self.finish_assistant_stream()?;
        write!(self.out, "iris> ")?;
        self.out.flush()?;

        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            writeln!(self.out)?;
            return Ok(None);
        }
        Ok(Some(line))
    }

    fn emit(&mut self, event: UiEvent) -> Result<()> {
        match event {
            UiEvent::SessionStarted => {
                self.finish_assistant_stream()?;
                writeln!(self.out, "Iris MVP. Type /exit to quit.")?;
            }
            UiEvent::AssistantText(text) => {
                self.finish_assistant_stream()?;
                writeln!(self.out, "assistant> {text}")?;
            }
            UiEvent::AssistantTextDelta(delta) => {
                if !self.assistant_stream_open {
                    write!(self.out, "assistant> ")?;
                    self.assistant_stream_open = true;
                }
                write!(self.out, "{delta}")?;
                self.out.flush()?;
            }
            UiEvent::AssistantTextEnd(_) => {
                self.finish_assistant_stream()?;
            }
            UiEvent::ToolProposed(call) => {
                self.finish_assistant_stream()?;
                writeln!(self.out, "{}", crate::tool_display::proposed_line(&call))?;
            }
            UiEvent::DiffPreview { call: _, diff } => {
                self.finish_assistant_stream()?;
                writeln!(self.out, "diff> {}", diff.replace('\n', "\ndiff> "))?;
            }
            UiEvent::ToolDenied(call) => {
                self.finish_assistant_stream()?;
                writeln!(self.out, "{}", crate::tool_display::denied_line(&call))?;
            }
            UiEvent::ToolResult { call: _, content } => {
                self.finish_assistant_stream()?;
                writeln!(self.out, "{}", crate::tool_display::result_line(&content))?;
            }
            UiEvent::ToolError { call: _, message } => {
                self.finish_assistant_stream()?;
                writeln!(self.out, "{}", crate::tool_display::error_line(&message))?;
            }
            UiEvent::Notice(message) => {
                self.finish_assistant_stream()?;
                writeln!(self.out, "note: {message}")?;
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
            }
        }
        Ok(())
    }

    fn request_approval(&mut self, call: &ToolCall) -> Result<ApprovalDecision> {
        self.finish_assistant_stream()?;
        write!(self.out, "{}", crate::tool_display::approval_prompt(call))?;
        self.out.flush()?;

        let mut line = String::new();
        if self.input.read_line(&mut line)? == 0 {
            writeln!(self.out)?;
            return Ok(ApprovalDecision::Deny);
        }

        Ok(parse_decision(&line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments: json!({ "path": "note.txt", "content": "hi" }),
        }
    }

    #[test]
    fn prompt_and_approval_share_one_input_owner() -> Result<()> {
        let mut ui = TextUi::new("hello\ny\n".as_bytes(), Vec::new(), Vec::new());

        assert_eq!(ui.next_prompt()?.as_deref(), Some("hello\n"));
        assert_eq!(
            ui.request_approval(&call("write"))?,
            ApprovalDecision::Allow
        );

        let (_, out, err) = ui.into_parts();
        assert!(String::from_utf8(out)?.contains("iris> approve write note.txt?"));
        assert!(err.is_empty());
        Ok(())
    }

    #[test]
    fn approval_eof_denies() -> Result<()> {
        let mut ui = TextUi::new("".as_bytes(), Vec::new(), Vec::new());

        assert_eq!(ui.request_approval(&call("write"))?, ApprovalDecision::Deny);
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
}
