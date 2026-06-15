use std::collections::HashSet;
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
    state: crate::tools::ToolState,
    // Session approval policy: tool names the user chose to "always" allow.
    // Owned and enforced here in Nexus, not in the UI, so a front-end can never
    // silently widen what runs without approval. Granularity is per tool name.
    // ponytail: per-tool-name always-allow; an "always" on `bash` authorizes any
    // later shell command this session. Upgrade path = per-exact-command keys
    // (e.g. `bash:<cmd>`) once a real audit trail exists (roadmap #14).
    session_allowed: HashSet<String>,
}

impl<P: ChatProvider> Agent<P> {
    pub(crate) fn new(provider: P, workspace: PathBuf) -> Self {
        Self {
            provider,
            messages: Vec::new(),
            workspace,
            state: crate::tools::ToolState::new(),
            session_allowed: HashSet::new(),
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
                    // A destructive call (e.g. `rm`) always re-prompts, even when
                    // its tool was "always allowed" this session: a blanket bash
                    // allow must not silently auto-run data-losing commands.
                    let destructive = crate::tools::is_destructive(&call.name, &call.arguments);
                    let session_allowed = self.session_allowed.contains(&call.name);
                    let auto_approved = session_allowed && !destructive;
                    if auto_approved {
                        ui.emit(UiEvent::ToolAutoApproved(call.clone()))?;
                    }
                    if let Some(diff) =
                        crate::tools::diff_preview(&self.workspace, &call.name, &call.arguments)
                    {
                        ui.emit(UiEvent::DiffPreview {
                            call: call.clone(),
                            diff,
                        })?;
                    }
                    if !auto_approved {
                        if destructive && session_allowed {
                            ui.emit(UiEvent::Notice(
                                "destructive command: approval required even though this tool is allow-always"
                                    .to_string(),
                            ))?;
                        }
                        match ui.request_approval(&call)? {
                            ApprovalDecision::Deny => {
                                tracing::warn!(tool = %call.name, "tool call denied by user");
                                ui.emit(UiEvent::ToolDenied(call.clone()))?;
                                self.messages.push(Message::tool_result(
                                    &call.id,
                                    &call.name,
                                    &denied_tool_result_json(),
                                ));
                                continue;
                            }
                            ApprovalDecision::AllowAlways => {
                                tracing::info!(tool = %call.name, "tool always-allowed this session");
                                self.session_allowed.insert(call.name.clone());
                            }
                            ApprovalDecision::Allow => {}
                        }
                    }
                } else {
                    ui.emit(UiEvent::ToolProposed(call.clone()))?;
                }

                let result = self.execute_tool(&call);
                tracing::info!(tool = %call.name, ok = result.is_ok(), "tool executed");
                match &result {
                    Ok(output) => ui.emit(UiEvent::ToolResult {
                        call: call.clone(),
                        content: output.content.clone(),
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

    fn execute_tool(&mut self, call: &ToolCall) -> Result<crate::tools::ToolOutput> {
        crate::tools::dispatch(
            &self.workspace,
            &call.name,
            &call.arguments,
            &mut self.state,
        )
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

fn tool_result_json(result: &Result<crate::tools::ToolOutput>) -> String {
    match result {
        Ok(output) => {
            let mut obj = serde_json::Map::new();
            obj.insert("ok".to_string(), Value::Bool(true));
            obj.insert("content".to_string(), Value::String(output.content.clone()));
            // Only emit the metadata object when a tool reported something
            // structured, keeping text-only results on the wire as before.
            if !output.metadata.is_empty() {
                obj.insert(
                    "metadata".to_string(),
                    Value::Object(output.metadata.clone()),
                );
            }
            Value::Object(obj).to_string()
        }
        Err(error) => json!({ "ok": false, "error": error.to_string() }).to_string(),
    }
}

// Model-facing denial payload. Denial is a distinct pre-execution branch, not an
// `Err` routed through `tool_result_json`, so the `denied` signal is preserved.
fn denied_tool_result_json() -> String {
    json!({ "ok": false, "error": "tool call denied by user", "denied": true }).to_string()
}

#[cfg(test)]
#[path = "nexus_tests.rs"]
mod tests;
