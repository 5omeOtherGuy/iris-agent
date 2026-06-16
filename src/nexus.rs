use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::{Value, json};

// Safety valve against a runaway tool loop. Each round-trip is one model
// response; the loop normally ends earlier when the model stops calling tools.
// Set high so legitimate multi-step tasks complete, and enforced gracefully
// rather than as a fatal error (see complete_turn).
const MAX_TOOL_ROUNDTRIPS: usize = 50;

pub(crate) trait TurnSink {
    fn on_text_delta(&mut self, delta: &str);
}

/// Outcome of an approval review for a single tool call. Provider/UI-neutral so
/// the core loop owns the approval policy without depending on any front-end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ApprovalDecision {
    /// Allow this one call.
    Allow,
    /// Allow this call and auto-approve later calls of the same tool for the
    /// rest of the session. Nexus owns and enforces that session policy.
    AllowAlways,
    /// Refuse this call. Default for empty/invalid/EOF input (safe-by-default).
    Deny,
}

/// The semantic events the loop emits during a turn. Provider- and UI-neutral:
/// a front-end maps these onto its own rendering. Mirrors pi's `AgentEvent`
/// union (`packages/agent/src/types.ts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentEvent {
    AssistantText(String),
    AssistantTextDelta(String),
    AssistantTextEnd(String),
    ToolProposed(ToolCall),
    /// A gated tool was auto-approved by the session allow-policy. Emitted by
    /// Nexus, never inferred by a front-end, so the policy stays Nexus-owned.
    ToolAutoApproved(ToolCall),
    DiffPreview {
        call: ToolCall,
        diff: String,
    },
    ToolDenied(ToolCall),
    ToolResult {
        call: ToolCall,
        content: String,
    },
    ToolError {
        call: ToolCall,
        message: String,
    },
    Notice(String),
    TurnComplete,
}

/// Fire-and-forget event sink the loop emits to. `&self` with no control-flow
/// return; errors only propagate. Mirrors pi's standalone `AgentEventSink`
/// passed as a separate argument, not a config field.
pub(crate) trait AgentObserver {
    fn on_event(&self, event: AgentEvent) -> Result<()>;
}

/// Request/response approval gate. `&self`; the loop branches on the returned
/// decision to control execution. Mirrors pi's `beforeToolCall` config hook,
/// which the loop inspects via `{ block }` -- a seam distinct from the event
/// sink.
pub(crate) trait ApprovalGate {
    fn review(&self, call: &ToolCall) -> Result<ApprovalDecision>;
}

/// Structured result of a successful tool call: the model-facing text plus
/// optional structured metadata. Tier-1 result contract (the analogue of pi's
/// `AgentToolResult`); tools with nothing structured to report use
/// [`ToolOutput::text`] and the metadata is omitted from the wire.
#[derive(Debug)]
pub(crate) struct ToolOutput {
    pub(crate) content: String,
    pub(crate) metadata: serde_json::Map<String, Value>,
}

impl ToolOutput {
    /// A text-only result with no structured metadata.
    pub(crate) fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            metadata: serde_json::Map::new(),
        }
    }

    /// Attach one metadata field, builder-style.
    pub(crate) fn with(mut self, key: &str, value: Value) -> Self {
        self.metadata.insert(key.to_string(), value);
        self
    }
}

/// Execution environment handed to a tool: the workspace root plus the mutable
/// per-session tool state (observed files, bash sessions). The Agent assembles
/// this per call. `ToolState` still lives in `crate::tools` for now; Step C
/// relocates it (and this env's guts) to the Wayland harness tier.
pub(crate) struct ToolEnv<'a> {
    pub(crate) workspace: &'a Path,
    pub(crate) state: &'a mut crate::tools::ToolState,
}

/// A tool the agent can invoke. Mirrors pi-ai's `Tool`
/// (`name`/`description`/`parameters`) plus pi-agent's `AgentTool` (the tool
/// runs itself via `execute`). Nexus enforces the approval policy, but each tool
/// *classifies* itself, so the core loop never matches on tool names.
pub(crate) trait Tool {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema for the arguments, used to build provider tool declarations.
    fn parameters(&self) -> Value;
    fn execute(&self, args: &Value, env: &mut ToolEnv) -> Result<ToolOutput>;

    /// Whether a call to this tool must be approved before it runs.
    fn requires_approval(&self) -> bool {
        false
    }
    /// Whether these arguments perform a destructive, data-losing operation that
    /// must be re-approved every time, even when the tool is "always allowed".
    fn is_destructive(&self, _args: &Value) -> bool {
        false
    }
    /// Whether an "always allow" decision may persist for this tool. Tools that
    /// authorize arbitrary later effects (e.g. shell) opt out, so the loop keeps
    /// prompting each call instead of name-matching `"bash"` in core.
    fn supports_allow_always(&self) -> bool {
        true
    }
    /// Optional pre-approval diff preview (Tier-3 presentation). `None` when the
    /// tool has no preview or the arguments are malformed.
    fn diff_preview(&self, _workspace: &Path, _args: &Value) -> Option<String> {
        None
    }
}

/// Injected collection the agent resolves tool calls against. A thin name lookup
/// over a `Vec<Box<dyn Tool>>` -- no identity keys, override, or dispatch-order
/// machinery (that is issue #18, out of scope). Mirrors pi's `context.tools`
/// resolved with `tools.find(t => t.name === toolCall.name)`.
pub(crate) struct Tools(Vec<Box<dyn Tool>>);

impl Tools {
    pub(crate) fn new(tools: Vec<Box<dyn Tool>>) -> Self {
        Self(tools)
    }

    /// Resolve a call by exact tool name. `None` for an unknown tool.
    pub(crate) fn by_name(&self, name: &str) -> Option<&dyn Tool> {
        self.0
            .iter()
            .map(|tool| &**tool)
            .find(|tool| tool.name() == name)
    }

    /// Iterate the tools in declaration order (for provider tool schemas).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &dyn Tool> {
        self.0.iter().map(|tool| &**tool)
    }
}

pub(crate) trait ChatProvider {
    // Providers translate their native response format into this Nexus-owned turn shape.
    // `tools` is the injected set the provider advertises as callable declarations.
    fn respond(
        &self,
        messages: &[Message],
        tools: &Tools,
        sink: &mut dyn TurnSink,
    ) -> Result<AssistantTurn>;
}

pub(crate) struct Agent<P> {
    pub(crate) provider: P,
    pub(crate) messages: Vec<Message>,
    workspace: PathBuf,
    state: crate::tools::ToolState,
    // Injected tool set, constructed at Tier 3 and resolved by name in the loop.
    // Core names no concrete tool; it only holds the `Tool` contract.
    tools: Tools,
    // Session approval policy: tool names the user chose to "always" allow.
    // Owned and enforced here in Nexus, not in the UI, so a front-end can never
    // silently widen what runs without approval. Granularity is per tool name.
    // ponytail: per-tool-name always-allow. `bash` is excluded on purpose -- an
    // "always" on bash never sticks, so every shell command re-prompts. Upgrade
    // path = per-exact-command keys (e.g. `bash:<cmd>`) once a real audit trail
    // exists (roadmap #14).
    session_allowed: HashSet<String>,
    // Optional transcript persistence. When attached, messages are appended to
    // the JSONL log at the end of each turn (`persisted` tracks how many of
    // `messages` are already on disk). None in tests and when no log could be
    // opened, so the agent runs fully in-memory.
    session: Option<crate::session::SessionLog>,
    persisted: usize,
}

impl<P: ChatProvider> Agent<P> {
    pub(crate) fn new(provider: P, workspace: PathBuf, tools: Tools) -> Self {
        Self {
            provider,
            messages: Vec::new(),
            workspace,
            state: crate::tools::ToolState::new(),
            tools,
            session_allowed: HashSet::new(),
            session: None,
            persisted: 0,
        }
    }

    /// Attach a transcript log; subsequent turns persist their messages.
    pub(crate) fn attach_session_log(&mut self, log: crate::session::SessionLog) {
        self.session = Some(log);
    }

    pub(crate) fn submit_turn(
        &mut self,
        prompt: &str,
        obs: &dyn AgentObserver,
        gate: &dyn ApprovalGate,
    ) -> Result<()> {
        let span = tracing::info_span!("turn");
        let _guard = span.enter();

        self.messages.push(Message::user(prompt));
        crate::signals::reset();
        let result = self.complete_turn(obs, gate);
        // Persist whatever the turn produced even when it ended in an error, so
        // the transcript records the user prompt and any tool work. Persistence
        // is best-effort: a write failure is logged, never fatal to the session.
        self.persist_new_messages();
        result
    }

    /// Append messages not yet written to the transcript log, advancing the
    /// persisted cursor. No-op when no log is attached.
    fn persist_new_messages(&mut self) {
        let Some(log) = self.session.as_mut() else {
            return;
        };
        while self.persisted < self.messages.len() {
            if let Err(error) = log.append(&self.messages[self.persisted]) {
                tracing::warn!(error = %format!("{error:#}"), "failed to persist session message");
                return;
            }
            self.persisted += 1;
        }
    }

    fn complete_turn(&mut self, obs: &dyn AgentObserver, gate: &dyn ApprovalGate) -> Result<()> {
        for roundtrip in 0..MAX_TOOL_ROUNDTRIPS {
            if crate::signals::interrupted() {
                tracing::info!(roundtrips = roundtrip, "turn interrupted by user");
                if roundtrip == 0 {
                    // Nothing was produced this turn yet; drop the unanswered
                    // prompt so the next turn does not push two consecutive
                    // user messages (rejected by some providers).
                    self.messages.pop();
                }
                obs.on_event(AgentEvent::Notice(
                    "interrupted; send another message to continue.".to_string(),
                ))?;
                obs.on_event(AgentEvent::TurnComplete)?;
                return Ok(());
            }
            let mut sink = ObserverTurnSink::new(obs);
            let turn = self
                .provider
                .respond(&self.messages, &self.tools, &mut sink)?;
            let saw_text_delta = sink.saw_text_delta;
            let stream_error = sink.error.take();
            drop(sink);
            if let Some(error) = stream_error {
                return Err(error);
            }

            if let Some(text) = turn.text.as_deref().filter(|text| !text.is_empty()) {
                if saw_text_delta {
                    obs.on_event(AgentEvent::AssistantTextEnd(text.to_string()))?;
                } else {
                    obs.on_event(AgentEvent::AssistantText(text.to_string()))?;
                }
                self.messages.push(Message::assistant(text));
            } else if saw_text_delta {
                obs.on_event(AgentEvent::AssistantTextEnd(String::new()))?;
            }

            if turn.tool_calls.is_empty() {
                tracing::debug!(roundtrips = roundtrip + 1, "turn complete");
                obs.on_event(AgentEvent::TurnComplete)?;
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
                    obs.on_event(AgentEvent::ToolDenied(call.clone()))?;
                    self.messages.push(Message::tool_result(
                        &call.id,
                        &call.name,
                        &denied_tool_result_json(),
                    ));
                }
                obs.on_event(AgentEvent::Notice(
                    "interrupted; send another message to continue.".to_string(),
                ))?;
                obs.on_event(AgentEvent::TurnComplete)?;
                return Ok(());
            }

            for call in turn.tool_calls {
                self.messages.push(Message::assistant_tool_call(&call));

                // Resolve the call against the injected set by name (pi's
                // `tools.find(t => t.name === name)`); `None` is an unknown tool.
                let tool = self.tools.by_name(&call.name);
                if let Some(tool) = tool.filter(|tool| tool.requires_approval()) {
                    // A destructive call (e.g. `rm`) always re-prompts, even when
                    // its tool was "always allowed" this session: a blanket bash
                    // allow must not silently auto-run data-losing commands.
                    let destructive = tool.is_destructive(&call.arguments);
                    let session_allowed = self.session_allowed.contains(&call.name);
                    let auto_approved =
                        session_allowed && !destructive && tool.supports_allow_always();
                    if auto_approved {
                        obs.on_event(AgentEvent::ToolAutoApproved(call.clone()))?;
                    }
                    if let Some(diff) = tool.diff_preview(&self.workspace, &call.arguments) {
                        obs.on_event(AgentEvent::DiffPreview {
                            call: call.clone(),
                            diff,
                        })?;
                    }
                    if !auto_approved {
                        if destructive && session_allowed {
                            obs.on_event(AgentEvent::Notice(
                                "destructive command: approval required even though this tool is allow-always"
                                    .to_string(),
                            ))?;
                        }
                        match gate.review(&call)? {
                            ApprovalDecision::Deny => {
                                tracing::warn!(tool = %call.name, "tool call denied by user");
                                obs.on_event(AgentEvent::ToolDenied(call.clone()))?;
                                self.messages.push(Message::tool_result(
                                    &call.id,
                                    &call.name,
                                    &denied_tool_result_json(),
                                ));
                                continue;
                            }
                            ApprovalDecision::AllowAlways => {
                                if tool.supports_allow_always() {
                                    tracing::info!(tool = %call.name, "tool always-allowed this session");
                                    self.session_allowed.insert(call.name.clone());
                                } else {
                                    obs.on_event(AgentEvent::Notice(
                                        "bash always-allow is disabled; shell commands require approval each time."
                                            .to_string(),
                                    ))?;
                                }
                            }
                            ApprovalDecision::Allow => {}
                        }
                    }
                } else {
                    obs.on_event(AgentEvent::ToolProposed(call.clone()))?;
                }

                // Run the resolved tool with the assembled env; an unknown tool
                // yields the same `unknown tool: <name>` result as before.
                let result = match tool {
                    Some(tool) => {
                        let mut env = ToolEnv {
                            workspace: &self.workspace,
                            state: &mut self.state,
                        };
                        tool.execute(&call.arguments, &mut env)
                    }
                    None => Err(anyhow::anyhow!("unknown tool: {}", call.name)),
                };
                tracing::info!(tool = %call.name, ok = result.is_ok(), "tool executed");
                match &result {
                    Ok(output) => obs.on_event(AgentEvent::ToolResult {
                        call: call.clone(),
                        content: output.content.clone(),
                    })?,
                    Err(error) => obs.on_event(AgentEvent::ToolError {
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
        obs.on_event(AgentEvent::Notice(format!(
            "stopped after {MAX_TOOL_ROUNDTRIPS} tool round-trips; send another message to continue."
        )))?;
        obs.on_event(AgentEvent::TurnComplete)?;
        Ok(())
    }
}

/// Forwards provider text deltas to the observer as `AssistantTextDelta`.
/// Stashes the first emit error and stops emitting, so a front-end failure
/// surfaces once without aborting the provider stream mid-flight.
struct ObserverTurnSink<'a> {
    obs: &'a dyn AgentObserver,
    saw_text_delta: bool,
    error: Option<anyhow::Error>,
}

impl<'a> ObserverTurnSink<'a> {
    fn new(obs: &'a dyn AgentObserver) -> Self {
        Self {
            obs,
            saw_text_delta: false,
            error: None,
        }
    }
}

impl TurnSink for ObserverTurnSink<'_> {
    fn on_text_delta(&mut self, delta: &str) {
        self.saw_text_delta = true;
        if self.error.is_none()
            && let Err(error) = self
                .obs
                .on_event(AgentEvent::AssistantTextDelta(delta.to_string()))
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

    pub(crate) fn assistant_tool_call(call: &ToolCall) -> Self {
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

fn tool_result_json(result: &Result<ToolOutput>) -> String {
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
