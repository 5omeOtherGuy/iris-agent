use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{Value, json};

use crate::approval::{ApprovalDecision, Approver};

// Safety valve against a runaway tool loop. Each round-trip is one model
// response; the loop normally ends earlier when the model stops calling tools.
// Set high so legitimate multi-step tasks complete, and enforced gracefully
// rather than as a fatal error (see complete_turn).
const MAX_TOOL_ROUNDTRIPS: usize = 50;
// Display caps for tool output; presentation-only and never affect what is sent to the model.
const MAX_DISPLAY_LINES: usize = 20;
const MAX_DISPLAY_CHARS: usize = 2000;

pub(crate) trait ChatProvider {
    // Providers translate their native response format into this Nexus-owned turn shape.
    fn respond(&self, messages: &[Message]) -> Result<AssistantTurn>;
}

pub(crate) struct Agent<P, A> {
    pub(crate) provider: P,
    pub(crate) messages: Vec<Message>,
    workspace: PathBuf,
    approver: A,
}

impl<P: ChatProvider, A: Approver> Agent<P, A> {
    pub(crate) fn new(provider: P, workspace: PathBuf, approver: A) -> Self {
        Self {
            provider,
            messages: Vec::new(),
            workspace,
            approver,
        }
    }

    pub(crate) fn run(&mut self) -> Result<()> {
        let stdin = io::stdin();
        let mut stdout = io::stdout();
        let mut stderr = io::stderr();
        self.run_with(stdin.lock(), &mut stdout, &mut stderr)
    }

    pub(crate) fn run_with<R: BufRead, W: Write, E: Write>(
        &mut self,
        mut input: R,
        output: &mut W,
        errors: &mut E,
    ) -> Result<()> {
        writeln!(output, "Iris MVP. Type /exit to quit.")?;

        loop {
            write!(output, "iris> ")?;
            output.flush()?;

            let mut line = String::new();
            if input.read_line(&mut line)? == 0 {
                writeln!(output)?;
                return Ok(());
            }

            let prompt = line.trim();
            if prompt.is_empty() {
                continue;
            }
            if matches!(prompt, "/exit" | "/quit") {
                return Ok(());
            }

            self.messages.push(Message::user(prompt));
            if let Err(error) = self.complete_turn(&mut input, output) {
                writeln!(errors, "provider error: {error:#}")?;
            }
        }
    }

    fn complete_turn<R: BufRead, W: Write>(&mut self, input: &mut R, output: &mut W) -> Result<()> {
        for _ in 0..MAX_TOOL_ROUNDTRIPS {
            let turn = self.provider.respond(&self.messages)?;
            if let Some(text) = turn.text.as_deref().filter(|text| !text.is_empty()) {
                writeln!(output, "assistant> {text}")?;
                self.messages.push(Message::assistant(text));
            }

            if turn.tool_calls.is_empty() {
                return Ok(());
            }

            for call in turn.tool_calls {
                writeln!(output, "tool> {}({})", call.name, call.arguments)?;
                self.messages.push(Message::assistant_tool_call(&call));

                if crate::tools::requires_approval(&call.name)
                    && matches!(
                        self.approver.review(&call, input, output)?,
                        ApprovalDecision::Deny
                    )
                {
                    write_denied_outcome(output, &call)?;
                    self.messages.push(Message::tool_result(
                        &call.id,
                        &call.name,
                        &denied_tool_result_json(),
                    ));
                    continue;
                }

                let result = self.execute_tool(&call);
                write_tool_outcome(output, &result)?;
                self.messages.push(Message::tool_result(
                    &call.id,
                    &call.name,
                    &tool_result_json(result),
                ));
            }
        }

        // Reached the round-trip guard while the model still wants to call
        // tools. End the turn gracefully so completed tool work and
        // conversation state are preserved and the REPL keeps running; this is
        // a soft limit, not a provider failure.
        writeln!(
            output,
            "note: stopped after {MAX_TOOL_ROUNDTRIPS} tool round-trips; send another message to continue."
        )?;
        Ok(())
    }

    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        crate::tools::dispatch(&self.workspace, &call.name, &call.arguments)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AssistantTurn {
    pub(crate) text: Option<String>,
    pub(crate) tool_calls: Vec<ToolCall>,
}

impl AssistantTurn {
    #[cfg(test)]
    pub(crate) fn text(text: &str) -> Self {
        Self {
            text: Some(text.to_string()),
            tool_calls: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Message {
    pub(crate) role: Role,
    pub(crate) content: String,
    // Tool-call and tool-result messages must carry both fields; text messages leave them empty.
    pub(crate) tool_call_id: Option<String>,
    pub(crate) tool_name: Option<String>,
}

impl Message {
    pub(crate) fn user(content: &str) -> Self {
        Self::new(Role::User, content)
    }

    pub(crate) fn assistant(content: &str) -> Self {
        Self::new(Role::Assistant, content)
    }

    fn assistant_tool_call(call: &ToolCall) -> Self {
        Self {
            role: Role::AssistantToolCall,
            content: call.arguments.to_string(),
            tool_call_id: Some(call.id.clone()),
            tool_name: Some(call.name.clone()),
        }
    }

    fn tool_result(call_id: &str, name: &str, content: &str) -> Self {
        Self {
            role: Role::Tool,
            content: content.to_string(),
            tool_call_id: Some(call_id.to_string()),
            tool_name: Some(name.to_string()),
        }
    }

    fn new(role: Role, content: &str) -> Self {
        Self {
            role,
            content: content.to_string(),
            tool_call_id: None,
            tool_name: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    User,
    Assistant,
    AssistantToolCall,
    Tool,
}

impl Role {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::AssistantToolCall => "assistant_tool_call",
            Self::Tool => "tool",
        }
    }
}

// Renders a tool outcome for the user. Display only: the full result still goes to the model.
fn write_tool_outcome<W: Write>(output: &mut W, result: &Result<String>) -> io::Result<()> {
    match result {
        Ok(content) => writeln!(output, "result> {}", truncate_for_display(content)),
        Err(error) => writeln!(output, "tool error> {error:#}"),
    }
}

fn truncate_for_display(text: &str) -> String {
    let mut out = String::new();
    let mut truncated = false;

    for (index, line) in text.lines().enumerate() {
        if index >= MAX_DISPLAY_LINES {
            truncated = true;
            break;
        }
        if index > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }

    if out.chars().count() > MAX_DISPLAY_CHARS {
        out = out.chars().take(MAX_DISPLAY_CHARS).collect();
        truncated = true;
    }

    if truncated {
        out.push_str("\n\u{2026} (truncated)");
    }
    out
}

fn tool_result_json(result: Result<String>) -> String {
    match result {
        Ok(content) => json!({ "ok": true, "content": content }).to_string(),
        Err(error) => json!({ "ok": false, "error": error.to_string() }).to_string(),
    }
}

// Model-facing denial payload. Denial is a distinct pre-execution branch, not an
// `Err` routed through `tool_result_json`, so the `denied` signal is preserved.
fn denied_tool_result_json() -> String {
    json!({ "ok": false, "error": "tool call denied by user", "denied": true }).to_string()
}

// Renders a denied call for the user; sibling of `result>` / `tool error>`.
fn write_denied_outcome<W: Write>(output: &mut W, call: &ToolCall) -> io::Result<()> {
    writeln!(output, "denied> {}({})", call.name, call.arguments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::Approver;
    use anyhow::anyhow;
    use std::cell::RefCell;
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Test-only approvers. They ignore the live streams; the gate only inspects
    // the returned decision.
    struct AutoAllow;
    impl Approver for AutoAllow {
        fn review<R: BufRead, W: Write>(
            &mut self,
            _call: &ToolCall,
            _input: &mut R,
            _output: &mut W,
        ) -> Result<ApprovalDecision> {
            Ok(ApprovalDecision::Allow)
        }
    }

    struct AutoDeny;
    impl Approver for AutoDeny {
        fn review<R: BufRead, W: Write>(
            &mut self,
            _call: &ToolCall,
            _input: &mut R,
            _output: &mut W,
        ) -> Result<ApprovalDecision> {
            Ok(ApprovalDecision::Deny)
        }
    }

    // Returns queued decisions in order; panics if consulted more often than
    // scripted so over-consumption is caught.
    struct ScriptedApprover {
        decisions: Vec<ApprovalDecision>,
    }
    impl ScriptedApprover {
        fn new(decisions: Vec<ApprovalDecision>) -> Self {
            Self {
                decisions: decisions.into_iter().rev().collect(),
            }
        }
    }
    impl Approver for ScriptedApprover {
        fn review<R: BufRead, W: Write>(
            &mut self,
            _call: &ToolCall,
            _input: &mut R,
            _output: &mut W,
        ) -> Result<ApprovalDecision> {
            Ok(self
                .decisions
                .pop()
                .expect("approver consulted more times than scripted"))
        }
    }

    struct FakeProvider {
        responses: RefCell<Vec<Result<AssistantTurn, String>>>,
        seen: RefCell<Vec<Vec<Message>>>,
    }

    impl FakeProvider {
        fn new(responses: Vec<Result<AssistantTurn, &str>>) -> Self {
            Self {
                responses: RefCell::new(
                    responses
                        .into_iter()
                        .map(|result| result.map_err(str::to_string))
                        .rev()
                        .collect(),
                ),
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl ChatProvider for FakeProvider {
        fn respond(&self, messages: &[Message]) -> Result<AssistantTurn> {
            self.seen.borrow_mut().push(messages.to_vec());
            match self.responses.borrow_mut().pop() {
                Some(Ok(turn)) => Ok(turn),
                Some(Err(error)) => Err(anyhow!(error)),
                None => Err(anyhow!("unexpected call")),
            }
        }
    }

    #[test]
    fn repl_keeps_conversation_across_turns() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(AssistantTurn::text("hello")),
            Ok(AssistantTurn::text("goodbye")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone(), AutoAllow);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("hi\nbye\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(String::from_utf8(output)?.contains("assistant> hello"));
        assert!(errors.is_empty());
        assert_eq!(agent.provider.seen.borrow().len(), 2);
        assert_eq!(agent.provider.seen.borrow()[1][0].content, "hi");
        assert_eq!(agent.provider.seen.borrow()[1][1].content, "hello");
        assert_eq!(agent.provider.seen.borrow()[1][2].content, "bye");
        Ok(())
    }

    #[test]
    fn repl_reports_provider_errors_and_continues() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![Err("boom"), Ok(AssistantTurn::text("recovered"))]);
        let mut agent = Agent::new(provider, workspace.path.clone(), AutoAllow);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("fail\nagain\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(String::from_utf8(errors)?.contains("provider error: boom"));
        assert!(String::from_utf8(output)?.contains("assistant> recovered"));
        assert_eq!(agent.messages.len(), 3);
        assert_eq!(agent.messages[0].content, "fail");
        assert_eq!(agent.messages[1].content, "again");
        assert_eq!(agent.messages[2].content, "recovered");
        Ok(())
    }

    #[test]
    fn tool_loop_reads_workspace_file_and_returns_result_to_model() -> Result<()> {
        let workspace = test_workspace()?;
        fs::write(workspace.path.join("note.txt"), "hello from file")?;
        let provider = FakeProvider::new(vec![
            Ok(AssistantTurn {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    arguments: json!({ "path": "note.txt" }),
                }],
            }),
            Ok(AssistantTurn::text("The file says hello from file.")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone(), AutoAllow);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("read note\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(errors.is_empty());
        assert!(String::from_utf8(output)?.contains("assistant> The file says hello from file."));
        let seen = agent.provider.seen.borrow();
        assert_eq!(seen.len(), 2);
        let tool_result = seen[1].last().unwrap();
        assert_eq!(tool_result.role, Role::Tool);
        assert_eq!(tool_result.tool_call_id.as_deref(), Some("call_1"));
        assert!(tool_result.content.contains("hello from file"));
        Ok(())
    }

    #[test]
    fn tool_result_is_displayed_to_user() -> Result<()> {
        let workspace = test_workspace()?;
        fs::write(workspace.path.join("note.txt"), "hello from file")?;
        let provider = FakeProvider::new(vec![
            Ok(AssistantTurn {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    arguments: json!({ "path": "note.txt" }),
                }],
            }),
            Ok(AssistantTurn::text("done")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone(), AutoAllow);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("read note\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        let rendered = String::from_utf8(output)?;
        assert!(rendered.contains("result>"));
        assert!(rendered.contains("hello from file"));
        assert!(errors.is_empty());
        Ok(())
    }

    #[test]
    fn tool_error_is_displayed_and_loop_continues() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(AssistantTurn {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "unknown".to_string(),
                    arguments: json!({}),
                }],
            }),
            Ok(AssistantTurn::text("recovered")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone(), AutoAllow);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("use bad tool\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        let rendered = String::from_utf8(output)?;
        assert!(rendered.contains("tool error>"));
        assert!(rendered.contains("unknown tool: unknown"));
        assert!(rendered.contains("assistant> recovered"));
        assert!(errors.is_empty());
        Ok(())
    }

    #[test]
    fn truncate_for_display_caps_long_output() {
        let text = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let shown = truncate_for_display(&text);
        assert!(shown.contains("line 0"));
        assert!(shown.contains("(truncated)"));
        assert!(!shown.contains("line 99"));
    }

    #[test]
    fn truncate_for_display_keeps_short_output() {
        assert_eq!(truncate_for_display("short output"), "short output");
    }

    #[test]
    fn tool_loop_stops_gracefully_at_roundtrip_limit() -> Result<()> {
        let workspace = test_workspace()?;
        fs::write(workspace.path.join("note.txt"), "hello from file")?;
        let repeated_call = || {
            Ok(AssistantTurn {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    arguments: json!({ "path": "note.txt" }),
                }],
            })
        };
        let provider =
            FakeProvider::new((0..MAX_TOOL_ROUNDTRIPS).map(|_| repeated_call()).collect());
        let mut agent = Agent::new(provider, workspace.path.clone(), AutoAllow);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("read forever\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        // Hitting the guard ends the turn gracefully: a user-visible notice,
        // no provider error, and the REPL keeps running (it consumes /exit).
        let rendered = String::from_utf8(output)?;
        assert!(rendered.contains("stopped after"));
        assert!(errors.is_empty());
        // The provider is consulted exactly the capped number of times, then
        // the loop stops without one extra round-trip.
        assert_eq!(agent.provider.seen.borrow().len(), MAX_TOOL_ROUNDTRIPS);
        Ok(())
    }

    #[test]
    fn unknown_tool_call_returns_tool_error_to_model() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(AssistantTurn {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "unknown".to_string(),
                    arguments: json!({}),
                }],
            }),
            Ok(AssistantTurn::text("I could not use that tool.")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone(), AutoAllow);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("use bad tool\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(errors.is_empty());
        assert_tool_error_contains(&agent.provider.seen.borrow()[1], "unknown tool: unknown");
        Ok(())
    }

    #[test]
    fn malformed_read_arguments_return_tool_error_to_model() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(AssistantTurn {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    arguments: json!({ "not_path": "note.txt" }),
                }],
            }),
            Ok(AssistantTurn::text("The read call was malformed.")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone(), AutoAllow);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with(
            "read malformed\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

        assert!(errors.is_empty());
        assert_tool_error_contains(
            &agent.provider.seen.borrow()[1],
            "read tool arguments must include path",
        );
        Ok(())
    }

    #[test]
    fn read_tool_rejects_paths_outside_workspace() -> Result<()> {
        let workspace = test_workspace()?;
        let outside = workspace.path.parent().unwrap().join("outside.txt");
        fs::write(&outside, "secret")?;

        let result = read_file(&workspace.path, "../outside.txt");

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("escapes workspace")
        );
        fs::remove_file(outside)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn read_tool_rejects_symlink_escape_from_workspace() -> Result<()> {
        let workspace = test_workspace()?;
        let outside_dir = test_workspace()?;
        let outside = outside_dir.path.join("outside.txt");
        fs::write(&outside, "secret")?;
        std::os::unix::fs::symlink(&outside, workspace.path.join("link.txt"))?;

        let result = read_file(&workspace.path, "link.txt");

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("escapes workspace")
        );
        Ok(())
    }

    #[test]
    fn read_tool_returns_missing_file_error() -> Result<()> {
        let workspace = test_workspace()?;

        let result = tool_result_json(read_file(&workspace.path, "missing.txt"));

        assert!(result.contains("\"ok\":false"));
        assert!(result.contains("failed to resolve path"));
        Ok(())
    }

    fn single_call_turn(name: &str, arguments: Value) -> AssistantTurn {
        AssistantTurn {
            text: None,
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: name.to_string(),
                arguments,
            }],
        }
    }

    #[test]
    fn approved_write_executes_and_creates_file() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn(
                "write",
                json!({ "path": "out.txt", "content": "hi" }),
            )),
            Ok(AssistantTurn::text("done")),
        ]);
        let mut agent = Agent::new(
            provider,
            workspace.path.clone(),
            ScriptedApprover::new(vec![ApprovalDecision::Allow]),
        );
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("write it\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(errors.is_empty());
        assert_eq!(fs::read_to_string(workspace.path.join("out.txt"))?, "hi");
        let seen = agent.provider.seen.borrow();
        assert!(seen[1].last().unwrap().content.contains("\"ok\":true"));
        Ok(())
    }

    #[test]
    fn denied_write_skips_execution_and_records_denial() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn(
                "write",
                json!({ "path": "out.txt", "content": "hi" }),
            )),
            Ok(AssistantTurn::text("understood")),
        ]);
        let mut agent = Agent::new(
            provider,
            workspace.path.clone(),
            ScriptedApprover::new(vec![ApprovalDecision::Deny]),
        );
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("write it\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(errors.is_empty());
        assert!(!workspace.path.join("out.txt").exists());
        assert!(String::from_utf8(output)?.contains("denied> write"));

        let seen = agent.provider.seen.borrow();
        let denial = seen[1].last().unwrap();
        assert_eq!(denial.role, Role::Tool);
        assert!(denial.content.contains("\"denied\":true"));
        assert!(denial.content.contains("denied by user"));
        assert_eq!(denial.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(denial.tool_name.as_deref(), Some("write"));

        // Assistant-tool-call -> tool-result pairing preserved on deny.
        let tool_call = seen[1]
            .iter()
            .find(|m| m.role == Role::AssistantToolCall)
            .unwrap();
        assert_eq!(tool_call.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_call.tool_name.as_deref(), Some("write"));
        Ok(())
    }

    #[test]
    fn read_is_never_gated_even_under_auto_deny() -> Result<()> {
        let workspace = test_workspace()?;
        fs::write(workspace.path.join("note.txt"), "hello from file")?;
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn("read", json!({ "path": "note.txt" }))),
            Ok(AssistantTurn::text("done")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone(), AutoDeny);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("read note\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(errors.is_empty());
        let seen = agent.provider.seen.borrow();
        let tool_result = seen[1].last().unwrap();
        assert_eq!(tool_result.role, Role::Tool);
        assert!(tool_result.content.contains("hello from file"));
        Ok(())
    }

    #[test]
    fn denied_bash_does_not_run_command() -> Result<()> {
        let workspace = test_workspace()?;
        let marker = workspace.path.join("marker");
        let command = format!("touch {}", marker.display());
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn("bash", json!({ "command": command }))),
            Ok(AssistantTurn::text("ok")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone(), AutoDeny);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("run it\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(!marker.exists());
        let seen = agent.provider.seen.borrow();
        assert!(seen[1].last().unwrap().content.contains("\"denied\":true"));
        Ok(())
    }

    #[test]
    fn denied_edit_leaves_file_unchanged() -> Result<()> {
        let workspace = test_workspace()?;
        fs::write(workspace.path.join("note.txt"), "original")?;
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn(
                "edit",
                json!({ "path": "note.txt", "old": "original", "new": "changed" }),
            )),
            Ok(AssistantTurn::text("ok")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone(), AutoDeny);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("edit it\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert_eq!(
            fs::read_to_string(workspace.path.join("note.txt"))?,
            "original"
        );
        let seen = agent.provider.seen.borrow();
        assert!(seen[1].last().unwrap().content.contains("\"denied\":true"));
        Ok(())
    }

    #[test]
    fn denied_hashline_edit_leaves_file_unchanged() -> Result<()> {
        let workspace = test_workspace()?;
        fs::write(workspace.path.join("note.txt"), "original")?;
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn(
                "hashline_edit",
                json!({ "path": "note.txt", "edits": [] }),
            )),
            Ok(AssistantTurn::text("ok")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone(), AutoDeny);
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("hashline it\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert_eq!(
            fs::read_to_string(workspace.path.join("note.txt"))?,
            "original"
        );
        let seen = agent.provider.seen.borrow();
        assert!(seen[1].last().unwrap().content.contains("\"denied\":true"));
        Ok(())
    }

    #[test]
    fn terminal_approver_allows_write_end_to_end() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn(
                "write",
                json!({ "path": "out.txt", "content": "hi" }),
            )),
            Ok(AssistantTurn::text("done")),
        ]);
        let mut agent = Agent::new(
            provider,
            workspace.path.clone(),
            crate::approval::TerminalApprover,
        );
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("write it\ny\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(errors.is_empty());
        assert_eq!(fs::read_to_string(workspace.path.join("out.txt"))?, "hi");
        Ok(())
    }

    #[test]
    fn terminal_approver_denies_write_end_to_end() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn(
                "write",
                json!({ "path": "out.txt", "content": "hi" }),
            )),
            Ok(AssistantTurn::text("understood")),
        ]);
        let mut agent = Agent::new(
            provider,
            workspace.path.clone(),
            crate::approval::TerminalApprover,
        );
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("write it\nn\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(!workspace.path.join("out.txt").exists());
        Ok(())
    }

    #[test]
    fn allowed_malformed_args_reach_tool_validation() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn("write", json!({ "path": "out.txt" }))),
            Ok(AssistantTurn::text("ok")),
        ]);
        let mut agent = Agent::new(
            provider,
            workspace.path.clone(),
            ScriptedApprover::new(vec![ApprovalDecision::Allow]),
        );
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("write it\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        let seen = agent.provider.seen.borrow();
        let tool_result = seen[1].last().unwrap();
        assert!(tool_result.content.contains("\"ok\":false"));
        assert!(!tool_result.content.contains("\"denied\":true"));
        Ok(())
    }

    #[test]
    fn denied_malformed_args_return_denial_without_validation() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn("write", json!({ "path": "out.txt" }))),
            Ok(AssistantTurn::text("ok")),
        ]);
        let mut agent = Agent::new(
            provider,
            workspace.path.clone(),
            ScriptedApprover::new(vec![ApprovalDecision::Deny]),
        );
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("write it\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        let seen = agent.provider.seen.borrow();
        assert!(seen[1].last().unwrap().content.contains("\"denied\":true"));
        Ok(())
    }

    #[test]
    fn multiple_gated_calls_consume_one_decision_each() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(AssistantTurn {
                text: None,
                tool_calls: vec![
                    ToolCall {
                        id: "call_1".to_string(),
                        name: "write".to_string(),
                        arguments: json!({ "path": "a.txt", "content": "a" }),
                    },
                    ToolCall {
                        id: "call_2".to_string(),
                        name: "write".to_string(),
                        arguments: json!({ "path": "b.txt", "content": "b" }),
                    },
                ],
            }),
            Ok(AssistantTurn::text("done")),
        ]);
        let mut agent = Agent::new(
            provider,
            workspace.path.clone(),
            ScriptedApprover::new(vec![ApprovalDecision::Allow, ApprovalDecision::Deny]),
        );
        let mut output = Vec::new();
        let mut errors = Vec::new();

        agent.run_with("write both\n/exit\n".as_bytes(), &mut output, &mut errors)?;

        assert!(workspace.path.join("a.txt").exists());
        assert!(!workspace.path.join("b.txt").exists());
        Ok(())
    }

    struct TestWorkspace {
        path: PathBuf,
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn assert_tool_error_contains(messages: &[Message], expected: &str) {
        let tool_result = messages.last().unwrap();
        assert_eq!(tool_result.role, Role::Tool);
        assert!(tool_result.content.contains("\"ok\":false"));
        assert!(tool_result.content.contains(expected));
    }

    fn read_file(workspace: &Path, path: &str) -> Result<String> {
        crate::tools::read_file(workspace, path)
    }

    fn test_workspace() -> Result<TestWorkspace> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!("iris-agent-test-{nanos}"));
        fs::create_dir(&path)?;
        Ok(TestWorkspace { path })
    }
}
