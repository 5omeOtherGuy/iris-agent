use std::path::PathBuf;

use anyhow::Result;
use serde_json::{Value, json};

use crate::approval::ApprovalDecision;
use crate::ui::{Ui, UiEvent};

// Safety valve against a runaway tool loop. Each round-trip is one model
// response; the loop normally ends earlier when the model stops calling tools.
// Set high so legitimate multi-step tasks complete, and enforced gracefully
// rather than as a fatal error (see complete_turn).
const MAX_TOOL_ROUNDTRIPS: usize = 50;

pub(crate) trait TurnSink {
    fn on_text_delta(&mut self, delta: &str);
}

pub(crate) trait ChatProvider {
    // Providers translate their native response format into this Nexus-owned turn shape.
    fn respond(&self, messages: &[Message], sink: &mut dyn TurnSink) -> Result<AssistantTurn>;
}

pub(crate) struct Agent<P> {
    pub(crate) provider: P,
    pub(crate) messages: Vec<Message>,
    workspace: PathBuf,
}

impl<P: ChatProvider> Agent<P> {
    pub(crate) fn new(provider: P, workspace: PathBuf) -> Self {
        Self {
            provider,
            messages: Vec::new(),
            workspace,
        }
    }

    pub(crate) fn submit_turn(&mut self, prompt: &str, ui: &mut dyn Ui) -> Result<()> {
        let span = tracing::info_span!("turn");
        let _guard = span.enter();

        self.messages.push(Message::user(prompt));
        crate::signals::reset();
        self.complete_turn(ui)
    }

    fn complete_turn(&mut self, ui: &mut dyn Ui) -> Result<()> {
        for roundtrip in 0..MAX_TOOL_ROUNDTRIPS {
            if crate::signals::interrupted() {
                tracing::info!(roundtrips = roundtrip, "turn interrupted by user");
                if roundtrip == 0 {
                    // Nothing was produced this turn yet; drop the unanswered
                    // prompt so the next turn does not push two consecutive
                    // user messages (rejected by some providers).
                    self.messages.pop();
                }
                ui.emit(UiEvent::Notice(
                    "interrupted; send another message to continue.".to_string(),
                ))?;
                ui.emit(UiEvent::TurnComplete)?;
                return Ok(());
            }
            let mut sink = UiTurnSink::new(ui);
            let turn = self.provider.respond(&self.messages, &mut sink)?;
            let saw_text_delta = sink.saw_text_delta;
            let stream_error = sink.error.take();
            drop(sink);
            if let Some(error) = stream_error {
                return Err(error);
            }

            if let Some(text) = turn.text.as_deref().filter(|text| !text.is_empty()) {
                if saw_text_delta {
                    ui.emit(UiEvent::AssistantTextEnd(text.to_string()))?;
                } else {
                    ui.emit(UiEvent::AssistantText(text.to_string()))?;
                }
                self.messages.push(Message::assistant(text));
            } else if saw_text_delta {
                ui.emit(UiEvent::AssistantTextEnd(String::new()))?;
            }

            if turn.tool_calls.is_empty() {
                tracing::debug!(roundtrips = roundtrip + 1, "turn complete");
                ui.emit(UiEvent::TurnComplete)?;
                return Ok(());
            }

            if crate::signals::interrupted() {
                // Ctrl-C arrived while the model was responding: do not run the
                // tools it just proposed. Record each as denied so the assistant
                // turn stays paired with tool results, then end the turn cleanly.
                tracing::info!(
                    roundtrips = roundtrip,
                    "turn interrupted after model response; pending tools denied"
                );
                for call in &turn.tool_calls {
                    self.messages.push(Message::assistant_tool_call(call));
                    ui.emit(UiEvent::ToolDenied(call.clone()))?;
                    self.messages.push(Message::tool_result(
                        &call.id,
                        &call.name,
                        &denied_tool_result_json(),
                    ));
                }
                ui.emit(UiEvent::Notice(
                    "interrupted; send another message to continue.".to_string(),
                ))?;
                ui.emit(UiEvent::TurnComplete)?;
                return Ok(());
            }

            for call in turn.tool_calls {
                self.messages.push(Message::assistant_tool_call(&call));

                if crate::tools::requires_approval(&call.name) {
                    if let Some(diff) =
                        crate::tools::diff_preview(&self.workspace, &call.name, &call.arguments)
                    {
                        ui.emit(UiEvent::DiffPreview {
                            call: call.clone(),
                            diff,
                        })?;
                    }
                    if matches!(ui.request_approval(&call)?, ApprovalDecision::Deny) {
                        tracing::warn!(tool = %call.name, "tool call denied by user");
                        ui.emit(UiEvent::ToolDenied(call.clone()))?;
                        self.messages.push(Message::tool_result(
                            &call.id,
                            &call.name,
                            &denied_tool_result_json(),
                        ));
                        continue;
                    }
                } else {
                    ui.emit(UiEvent::ToolProposed(call.clone()))?;
                }

                let result = self.execute_tool(&call);
                tracing::info!(tool = %call.name, ok = result.is_ok(), "tool executed");
                match &result {
                    Ok(content) => ui.emit(UiEvent::ToolResult {
                        call: call.clone(),
                        content: content.clone(),
                    })?,
                    Err(error) => ui.emit(UiEvent::ToolError {
                        call: call.clone(),
                        message: format!("{error:#}"),
                    })?,
                }
                self.messages.push(Message::tool_result(
                    &call.id,
                    &call.name,
                    &tool_result_json(&result),
                ));
            }
        }

        tracing::warn!(
            cap = MAX_TOOL_ROUNDTRIPS,
            "tool round-trip cap reached; ending turn"
        );
        // Reached the round-trip guard while the model still wants to call
        // tools. End the turn gracefully so completed tool work and
        // conversation state are preserved and the REPL keeps running; this is
        // a soft limit, not a provider failure.
        ui.emit(UiEvent::Notice(format!(
            "stopped after {MAX_TOOL_ROUNDTRIPS} tool round-trips; send another message to continue."
        )))?;
        ui.emit(UiEvent::TurnComplete)?;
        Ok(())
    }

    fn execute_tool(&self, call: &ToolCall) -> Result<String> {
        crate::tools::dispatch(&self.workspace, &call.name, &call.arguments)
    }
}

struct UiTurnSink<'a> {
    ui: &'a mut dyn Ui,
    saw_text_delta: bool,
    error: Option<anyhow::Error>,
}

impl<'a> UiTurnSink<'a> {
    fn new(ui: &'a mut dyn Ui) -> Self {
        Self {
            ui,
            saw_text_delta: false,
            error: None,
        }
    }
}

impl TurnSink for UiTurnSink<'_> {
    fn on_text_delta(&mut self, delta: &str) {
        self.saw_text_delta = true;
        if self.error.is_none()
            && let Err(error) = self.ui.emit(UiEvent::AssistantTextDelta(delta.to_string()))
        {
            self.error = Some(error);
        }
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

fn tool_result_json(result: &Result<String>) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::run_session;
    use crate::ui::text::TextUi;
    use anyhow::anyhow;
    use std::cell::RefCell;
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

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
        fn respond(&self, messages: &[Message], _sink: &mut dyn TurnSink) -> Result<AssistantTurn> {
            self.seen.borrow_mut().push(messages.to_vec());
            match self.responses.borrow_mut().pop() {
                Some(Ok(turn)) => Ok(turn),
                Some(Err(error)) => Err(anyhow!(error)),
                None => Err(anyhow!("unexpected call")),
            }
        }
    }

    fn run_text_session<P: ChatProvider>(
        agent: &mut Agent<P>,
        input: &[u8],
        output: &mut Vec<u8>,
        errors: &mut Vec<u8>,
    ) -> Result<()> {
        let mut ui = TextUi::new(input, Vec::new(), Vec::new());
        run_session(agent, &mut ui)?;
        let (_, out, err) = ui.into_parts();
        *output = out;
        *errors = err;
        Ok(())
    }

    #[test]
    fn submit_turn_emits_non_gated_tool_sequence() -> Result<()> {
        use crate::ui::{Ui, UiEvent};

        struct EventUi {
            events: Vec<UiEvent>,
        }

        impl Ui for EventUi {
            fn next_prompt(&mut self) -> Result<Option<String>> {
                Ok(None)
            }

            fn emit(&mut self, event: UiEvent) -> Result<()> {
                self.events.push(event);
                Ok(())
            }

            fn request_approval(&mut self, _call: &ToolCall) -> Result<ApprovalDecision> {
                panic!("read should not request approval")
            }
        }

        let workspace = test_workspace()?;
        fs::write(workspace.path.join("note.txt"), "hello")?;
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn("read", json!({ "path": "note.txt" }))),
            Ok(AssistantTurn::text("done")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut ui = EventUi { events: Vec::new() };

        agent.submit_turn("read note", &mut ui)?;

        assert!(matches!(ui.events[0], UiEvent::ToolProposed(_)));
        assert!(matches!(ui.events[1], UiEvent::ToolResult { .. }));
        assert!(matches!(ui.events[2], UiEvent::AssistantText(_)));
        assert!(matches!(ui.events[3], UiEvent::TurnComplete));
        Ok(())
    }

    #[test]
    fn gated_write_emits_diff_preview_before_approval() -> Result<()> {
        use crate::ui::{Ui, UiEvent};

        struct EventUi {
            events: Vec<UiEvent>,
            decision: ApprovalDecision,
        }

        impl Ui for EventUi {
            fn next_prompt(&mut self) -> Result<Option<String>> {
                Ok(None)
            }

            fn emit(&mut self, event: UiEvent) -> Result<()> {
                self.events.push(event);
                Ok(())
            }

            fn request_approval(&mut self, _call: &ToolCall) -> Result<ApprovalDecision> {
                assert!(matches!(
                    self.events.last(),
                    Some(UiEvent::DiffPreview { .. })
                ));
                Ok(self.decision)
            }
        }

        let workspace = test_workspace()?;
        fs::write(workspace.path.join("out.txt"), "old\n")?;
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn(
                "write",
                json!({ "path": "out.txt", "content": "new\n" }),
            )),
            Ok(AssistantTurn::text("done")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut ui = EventUi {
            events: Vec::new(),
            decision: ApprovalDecision::Allow,
        };

        agent.submit_turn("write it", &mut ui)?;

        assert!(matches!(ui.events[0], UiEvent::DiffPreview { .. }));
        assert!(matches!(ui.events[1], UiEvent::ToolResult { .. }));
        assert_eq!(fs::read_to_string(workspace.path.join("out.txt"))?, "new\n");
        Ok(())
    }

    #[test]
    fn malformed_denial_skips_diff_preview() -> Result<()> {
        use crate::ui::{Ui, UiEvent};

        struct EventUi {
            events: Vec<UiEvent>,
        }

        impl Ui for EventUi {
            fn next_prompt(&mut self) -> Result<Option<String>> {
                Ok(None)
            }

            fn emit(&mut self, event: UiEvent) -> Result<()> {
                self.events.push(event);
                Ok(())
            }

            fn request_approval(&mut self, _call: &ToolCall) -> Result<ApprovalDecision> {
                assert!(self.events.is_empty(), "malformed args must not preflight");
                Ok(ApprovalDecision::Deny)
            }
        }

        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn("write", json!({ "path": "out.txt" }))),
            Ok(AssistantTurn::text("done")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut ui = EventUi { events: Vec::new() };

        agent.submit_turn("write it", &mut ui)?;

        assert!(
            ui.events
                .iter()
                .all(|event| !matches!(event, UiEvent::DiffPreview { .. }))
        );
        assert!(matches!(ui.events[0], UiEvent::ToolDenied(_)));
        assert!(!workspace.path.join("out.txt").exists());
        Ok(())
    }

    #[test]
    fn repl_keeps_conversation_across_turns() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(AssistantTurn::text("hello")),
            Ok(AssistantTurn::text("goodbye")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "hi\nbye\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

        assert!(String::from_utf8(output)?.contains("assistant> hello"));
        assert!(errors.is_empty());
        assert_eq!(agent.provider.seen.borrow().len(), 2);
        assert_eq!(agent.provider.seen.borrow()[1][0].content, "hi");
        assert_eq!(agent.provider.seen.borrow()[1][1].content, "hello");
        assert_eq!(agent.provider.seen.borrow()[1][2].content, "bye");
        Ok(())
    }

    struct AuthFailProvider;
    impl ChatProvider for AuthFailProvider {
        fn respond(
            &self,
            _messages: &[Message],
            _sink: &mut dyn TurnSink,
        ) -> Result<AssistantTurn> {
            Err(crate::errors::AuthError::new("token expired").into())
        }
    }

    struct DeltaProvider;
    impl ChatProvider for DeltaProvider {
        fn respond(&self, _messages: &[Message], sink: &mut dyn TurnSink) -> Result<AssistantTurn> {
            sink.on_text_delta("Hel");
            sink.on_text_delta("lo");
            Ok(AssistantTurn::text("Hello"))
        }
    }

    #[test]
    fn streamed_deltas_render_in_order_and_commit_once() -> Result<()> {
        let workspace = test_workspace()?;
        let mut agent = Agent::new(DeltaProvider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "hello\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

        assert_eq!(
            String::from_utf8(output)?,
            "Iris MVP. Type /exit to quit.\niris> assistant> Hello\niris> "
        );
        assert!(errors.is_empty());
        assert_eq!(agent.messages.len(), 2);
        assert_eq!(agent.messages[1], Message::assistant("Hello"));
        Ok(())
    }

    #[test]
    fn repl_reports_auth_errors_with_login_hint() -> Result<()> {
        let workspace = test_workspace()?;
        let mut agent = Agent::new(AuthFailProvider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "hello\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

        let rendered = String::from_utf8(errors)?;
        assert!(rendered.contains("auth error:"));
        assert!(rendered.contains("re-run the login command"));
        assert!(!rendered.contains("provider error:"));
        Ok(())
    }

    #[test]
    fn repl_reports_provider_errors_and_continues() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![Err("boom"), Ok(AssistantTurn::text("recovered"))]);
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "fail\nagain\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "read note\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "read note\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "use bad tool\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

        let rendered = String::from_utf8(output)?;
        assert!(rendered.contains("tool error>"));
        assert!(rendered.contains("unknown tool: unknown"));
        assert!(rendered.contains("assistant> recovered"));
        assert!(errors.is_empty());
        Ok(())
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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "read forever\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "use bad tool\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
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

        let result = tool_result_json(&read_file(&workspace.path, "missing.txt"));

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "write it\ny\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

        assert!(errors.is_empty());
        assert_eq!(fs::read_to_string(workspace.path.join("out.txt"))?, "hi");
        let seen = agent.provider.seen.borrow();
        assert!(seen[1].last().unwrap().content.contains("\"ok\":true"));
        Ok(())
    }

    #[test]
    fn approved_write_renders_prompt_and_result_without_raw_json() -> Result<()> {
        let workspace = test_workspace()?;
        let provider = FakeProvider::new(vec![
            Ok(single_call_turn(
                "write",
                json!({ "path": "out.txt", "content": "hi" }),
            )),
            Ok(AssistantTurn::text("done")),
        ]);
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "write it\ny\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

        let rendered = String::from_utf8(output)?;
        // The approval prompt carries the summary; the result line follows it.
        assert!(rendered.contains("approve write out.txt?"));
        assert!(rendered.contains("result>"));
        // No separate proposed line and no raw `name({json})` argument dump.
        assert!(!rendered.contains("tool> write({"));
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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "write it\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

        assert!(errors.is_empty());
        assert!(!workspace.path.join("out.txt").exists());
        let rendered = String::from_utf8(output)?;
        assert!(rendered.contains("denied> write out.txt"));
        // Gated calls no longer double-print a raw `tool> write({...})` line.
        assert!(!rendered.contains("tool> write({"));

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "read note\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "run it\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "edit it\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "write it\ny\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "write it\nn\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "write it\ny\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "write it\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

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
        let mut agent = Agent::new(provider, workspace.path.clone());
        let mut output = Vec::new();
        let mut errors = Vec::new();

        run_text_session(
            &mut agent,
            "write both\ny\nn\n/exit\n".as_bytes(),
            &mut output,
            &mut errors,
        )?;

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
        // nanos alone collide across parallel tests; a process-unique counter
        // guarantees a distinct directory per call.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("iris-agent-test-{nanos}-{seq}"));
        fs::create_dir(&path)?;
        Ok(TestWorkspace { path })
    }
}
