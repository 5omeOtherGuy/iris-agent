use std::cell::RefCell;
use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use anyhow::{Result, bail};
use futures::Stream;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// Safety valve against a runaway tool loop. Each round-trip is one model
// response; the loop normally ends earlier when the model stops calling tools.
// Set high so legitimate multi-step tasks complete, and enforced gracefully
// rather than as a fatal error (see complete_turn).
const MAX_TOOL_ROUNDTRIPS: usize = 50;

// ponytail: small fixed cap. If tool batches need more throughput, make this a
// runtime setting after measuring disk/blocking-pool pressure.
const MAX_PARALLEL_TOOL_CALLS: usize = 8;

// Shared between every cancellation exit path so the front-end renders one
// consistent message whether the interrupt landed before, during, or after the
// provider stream.
const INTERRUPT_NOTICE: &str = "interrupted; send another message to continue.";

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

/// A provider-neutral streamed model event. The async [`ChatProvider`] yields a
/// sequence of these instead of one blocking whole-turn result, so the loop can
/// race each read against cancellation. Mirrors Codex's `ResponseEvent`
/// (`core/src/client.rs`): incremental text deltas, then one terminal
/// `Completed` carrying the assembled turn (text + tool calls).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderEvent {
    /// Incremental assistant text.
    TextDelta(String),
    /// Terminal event: the fully assembled assistant turn.
    Completed(AssistantTurn),
}

/// A `!Send` boxed stream of provider events tied to the borrow of the provider
/// and its inputs. Boxed (not `impl Stream`) so the loop code is uniform and the
/// real provider can back it with a channel fed by a blocking task.
pub(crate) type ProviderStream<'a> = Pin<Box<dyn Stream<Item = Result<ProviderEvent>> + 'a>>;

/// A `!Send` boxed tool-execution future, so `Box<dyn Tool>` stays object-safe
/// while `execute` is async.
pub(crate) type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<ToolOutput>> + 'a>>;

/// A `!Send` boxed approval future, so `&dyn ApprovalGate` stays object-safe
/// while `review` is async (and therefore raceable against cancellation).
pub(crate) type ApprovalFuture<'a> = Pin<Box<dyn Future<Output = Result<ApprovalDecision>> + 'a>>;

/// Fire-and-forget event sink the loop emits to. `&self` with no control-flow
/// return; errors only propagate. Mirrors pi's standalone `AgentEventSink`
/// passed as a separate argument, not a config field.
pub(crate) trait AgentObserver {
    fn on_event(&self, event: AgentEvent) -> Result<()>;
}

/// Request/response approval gate. Async so the loop can race a pending approval
/// against cancellation (`tokio::select!`); the loop branches on the returned
/// decision to control execution. Mirrors pi's `beforeToolCall` config hook,
/// which the loop inspects via `{ block }` -- a seam distinct from the event
/// sink.
pub(crate) trait ApprovalGate {
    /// `allow_always` mirrors the tool's [`Tool::supports_allow_always`] so the
    /// front-end only offers an "always allow" choice the loop will honor (shell
    /// tools opt out, so their prompt is y/N only).
    fn review<'a>(&'a self, call: &'a ToolCall, allow_always: bool) -> ApprovalFuture<'a>;
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

/// Execution environment handed to a tool: the workspace root plus the shared
/// per-session tool state (observed files, bash sessions). The state is behind a
/// [`RefCell`] so the loop can hand a shared `&ToolEnv` to several
/// concurrency-safe tools at once (safe-parallel execution); each tool's body is
/// synchronous and never holds the borrow across an `.await`. Owned by the
/// Tier-2 Wayland harness and injected into each turn, mirroring how pi's
/// `AgentHarness` feeds its `ExecutionEnv` into the loop. `ToolState` is defined
/// in `crate::tools`.
pub(crate) struct ToolEnv<'a> {
    pub(crate) workspace: &'a Path,
    pub(crate) state: &'a RefCell<crate::tools::ToolState>,
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
    /// Run the tool. Async + given a child [`CancellationToken`]: a long-running
    /// tool (e.g. shell) should observe the token and stop promptly. The loop
    /// also races this future against the token, so a tool that ignores it is
    /// still abandoned (with a synthetic cancelled result).
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        cancel: CancellationToken,
    ) -> ToolFuture<'a>;

    /// Whether this tool may run concurrently with other concurrency-safe tools
    /// in the same model turn. Default: exclusive. Only read-only tools whose
    /// behavior is unaffected by concurrent peers opt in; the loop never runs an
    /// exclusive tool alongside anything else.
    fn is_concurrency_safe(&self) -> bool {
        false
    }

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
    /// Begin a streamed model response. Providers translate their native wire
    /// format into a stream of Nexus-owned [`ProviderEvent`]s; setup errors
    /// (e.g. a bad URL) surface synchronously via the `Result`, stream errors
    /// arrive as `Err` items. `tools` is the injected set the provider
    /// advertises as callable declarations. `cancel` is the turn token: a
    /// provider that does blocking work off-thread should observe it so a
    /// cancelled turn stops issuing/retrying requests instead of running to
    /// completion in the background.
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a Tools,
        cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>>;
}

/// Forward the contract through a boxed provider so the front-end can select
/// one of several concrete providers at runtime (`Box<dyn ChatProvider>`)
/// without making every downstream type generic over the choice.
impl ChatProvider for Box<dyn ChatProvider> {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a Tools,
        cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        (**self).respond_stream(messages, tools, cancel)
    }
}

pub(crate) struct Agent<P> {
    pub(crate) provider: P,
    messages: Vec<Message>,
    // Injected tool set, constructed at Tier 3 and resolved by name in the loop.
    // Core names no concrete tool; it only holds the `Tool` contract.
    tools: Tools,
    // Session approval policy: tool names the user chose to "always" allow.
    // Owned and enforced here in Nexus, not in the UI, so a front-end can never
    // silently widen what runs without approval. Granularity is per tool name.
    // ponytail: per-tool-name always-allow. The mutating tools (`bash`, `write`,
    // `edit`) opt out (`supports_allow_always() == false`), so an "always" on
    // them never sticks and every call re-prompts -- a blanket allow would
    // authorize arbitrary later effects. Upgrade path = per-exact-command/path
    // keys (e.g. `bash:<cmd>`, `write:<path>`) once a real audit trail exists
    // (roadmap #14).
    session_allowed: HashSet<String>,
}

/// Result of consuming one provider stream to its terminal event (or to a
/// cancellation). Owned so the borrow of `self.messages`/`self.tools` taken by
/// the stream is released before the loop mutates the transcript.
enum StreamResult {
    Completed {
        turn: AssistantTurn,
        saw_delta: bool,
    },
    Cancelled {
        partial: String,
        saw_delta: bool,
    },
}

/// Whether the tool phase wants another model round-trip or ended the turn
/// itself (a cancellation already emitted `TurnComplete`).
enum ToolsPhase {
    Continue,
    Ended,
}

/// Internal per-call execution outcome, mapped to a transcript message + event
/// by [`record_call`].
enum ToolOutcome {
    Ok(ToolOutput),
    Err(anyhow::Error),
    Cancelled,
    Denied,
}

impl<P: ChatProvider> Agent<P> {
    /// A bare, in-memory agent: it owns the provider, conversation, injected
    /// tools, and approval policy, but no filesystem or persistence. Mirrors
    /// pi's bare `Agent`; the Tier-2 Wayland harness wraps it with the execution
    /// env and session store.
    pub(crate) fn new(provider: P, tools: Tools) -> Self {
        Self {
            provider,
            messages: Vec::new(),
            tools,
            session_allowed: HashSet::new(),
        }
    }

    /// A bare agent seeded with a prior conversation, for resuming a session.
    /// The reconstructed `messages` become the provider-visible context for the
    /// next turn; the approval policy starts fresh (allow-always is per-process,
    /// not persisted). Mirrors pi's harness loading session entries and
    /// rebuilding context before continuing the conversation.
    pub(crate) fn resumed(provider: P, tools: Tools, mut messages: Vec<Message>) -> Self {
        repair_dangling_tool_call(&mut messages);
        Self {
            provider,
            messages,
            tools,
            session_allowed: HashSet::new(),
        }
    }

    /// Read access to the in-memory transcript so the harness can persist it
    /// without the core loop owning a session store.
    pub(crate) fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Replace the in-memory provider-visible context. The Tier-2 harness uses
    /// this to install a compacted context (summary + retained tail) before the
    /// next turn; the bare agent stays oblivious to compaction policy and
    /// persistence, just as [`resumed`](Self::resumed) seeds context on resume.
    pub(crate) fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
    }

    pub(crate) async fn submit_turn(
        &mut self,
        prompt: &str,
        obs: &dyn AgentObserver,
        gate: &dyn ApprovalGate,
        env: &ToolEnv<'_>,
        token: &CancellationToken,
    ) -> Result<()> {
        self.messages.push(Message::user(prompt));
        // The bare agent does no persistence: the harness diffs `messages()`
        // onto its session store after the turn returns (even on error).
        self.complete_turn(obs, gate, env, token).await
    }

    async fn complete_turn(
        &mut self,
        obs: &dyn AgentObserver,
        gate: &dyn ApprovalGate,
        env: &ToolEnv<'_>,
        token: &CancellationToken,
    ) -> Result<()> {
        for roundtrip in 0..MAX_TOOL_ROUNDTRIPS {
            if token.is_cancelled() {
                tracing::info!(roundtrips = roundtrip, "turn interrupted by user");
                if roundtrip == 0 {
                    // Nothing was produced this turn yet; drop the unanswered
                    // prompt so the next turn does not push two consecutive
                    // user messages (rejected by some providers).
                    self.messages.pop();
                }
                self.emit_interrupted(obs)?;
                return Ok(());
            }

            match self.stream_turn(obs, token).await? {
                StreamResult::Cancelled { partial, saw_delta } => {
                    // Commit any partial assistant text so the transcript stays
                    // valid (paired with the user prompt); otherwise drop the
                    // unanswered first-round prompt.
                    if !partial.is_empty() {
                        if saw_delta {
                            obs.on_event(AgentEvent::AssistantTextEnd(partial.clone()))?;
                        } else {
                            obs.on_event(AgentEvent::AssistantText(partial.clone()))?;
                        }
                        self.messages.push(Message::assistant(&partial));
                    } else if roundtrip == 0 {
                        self.messages.pop();
                    }
                    tracing::info!(
                        roundtrips = roundtrip,
                        "turn interrupted during model stream"
                    );
                    self.emit_interrupted(obs)?;
                    return Ok(());
                }
                StreamResult::Completed { turn, saw_delta } => {
                    if let Some(text) = turn.text.as_deref().filter(|text| !text.is_empty()) {
                        if saw_delta {
                            obs.on_event(AgentEvent::AssistantTextEnd(text.to_string()))?;
                        } else {
                            obs.on_event(AgentEvent::AssistantText(text.to_string()))?;
                        }
                        self.messages.push(Message::assistant(text));
                    } else if saw_delta {
                        obs.on_event(AgentEvent::AssistantTextEnd(String::new()))?;
                    }

                    if turn.tool_calls.is_empty() {
                        tracing::debug!(roundtrips = roundtrip + 1, "turn complete");
                        obs.on_event(AgentEvent::TurnComplete)?;
                        return Ok(());
                    }

                    match self
                        .run_tools(turn.tool_calls, obs, gate, env, token)
                        .await?
                    {
                        ToolsPhase::Ended => return Ok(()),
                        ToolsPhase::Continue => {}
                    }
                }
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

    /// Consume one provider stream to its terminal event, emitting text deltas
    /// and racing every read against cancellation. Borrows `&self` (messages +
    /// tools) only for the stream's lifetime; the owned [`StreamResult`] lets
    /// the caller mutate the transcript afterward.
    async fn stream_turn(
        &self,
        obs: &dyn AgentObserver,
        token: &CancellationToken,
    ) -> Result<StreamResult> {
        let mut stream = self
            .provider
            .respond_stream(&self.messages, &self.tools, token)?;
        let mut saw_delta = false;
        let mut partial = String::new();
        loop {
            tokio::select! {
                biased;
                _ = token.cancelled() => {
                    return Ok(StreamResult::Cancelled { partial, saw_delta });
                }
                item = stream.next() => match item {
                    Some(Ok(ProviderEvent::TextDelta(delta))) => {
                        saw_delta = true;
                        partial.push_str(&delta);
                        obs.on_event(AgentEvent::AssistantTextDelta(delta))?;
                    }
                    Some(Ok(ProviderEvent::Completed(turn))) => {
                        return Ok(StreamResult::Completed { turn, saw_delta });
                    }
                    Some(Err(error)) => return Err(error),
                    None => bail!("provider stream closed before completion"),
                },
            }
        }
    }

    /// Execute the model's tool calls: consecutive concurrency-safe calls run in
    /// parallel, every other call runs exclusively (one at a time). Transcript
    /// order is preserved regardless of completion order. On cancellation, every
    /// not-yet-executed call still gets a synthetic cancelled result so the next
    /// model request stays valid.
    async fn run_tools(
        &mut self,
        calls: Vec<ToolCall>,
        obs: &dyn AgentObserver,
        gate: &dyn ApprovalGate,
        env: &ToolEnv<'_>,
        token: &CancellationToken,
    ) -> Result<ToolsPhase> {
        let mut idx = 0;
        while idx < calls.len() {
            if token.is_cancelled() {
                tracing::info!(
                    pending = calls.len() - idx,
                    "turn interrupted during tools; remaining calls cancelled"
                );
                for call in &calls[idx..] {
                    record_call(&mut self.messages, obs, call, ToolOutcome::Cancelled)?;
                }
                self.emit_interrupted(obs)?;
                return Ok(ToolsPhase::Ended);
            }

            if self.is_parallelizable(&calls[idx]) {
                let mut end = idx;
                while end < calls.len() && self.is_parallelizable(&calls[end]) {
                    end += 1;
                }
                for call in &calls[idx..end] {
                    obs.on_event(AgentEvent::ToolProposed(call.clone()))?;
                }
                // Scope the borrow of `self.tools` so it drops before the
                // transcript pushes below.
                let outcomes = run_parallel(&self.tools, &calls[idx..end], env, token).await;
                for (call, outcome) in calls[idx..end].iter().zip(outcomes) {
                    record_call(&mut self.messages, obs, call, outcome)?;
                }
                idx = end;
            } else {
                let outcome = self
                    .run_gated_single(&calls[idx], obs, gate, env, token)
                    .await?;
                record_call(&mut self.messages, obs, &calls[idx], outcome)?;
                idx += 1;
            }
        }

        // The model gets another round-trip to react to the tool results; the
        // turn is not complete here (only an empty tool-call response, the
        // round-trip cap, or a cancellation ends it).
        Ok(ToolsPhase::Continue)
    }

    /// Whether a call may join a parallel batch: it resolves to a known tool
    /// that is concurrency-safe and ungated. Gated tools always take the
    /// exclusive path so their approval prompt runs alone.
    fn is_parallelizable(&self, call: &ToolCall) -> bool {
        self.tools
            .by_name(&call.name)
            .is_some_and(|tool| tool.is_concurrency_safe() && !tool.requires_approval())
    }

    /// The exclusive (default) path for one call: approval policy, then a single
    /// cancellation-raced execution. Returns the outcome; the caller records it.
    async fn run_gated_single(
        &mut self,
        call: &ToolCall,
        obs: &dyn AgentObserver,
        gate: &dyn ApprovalGate,
        env: &ToolEnv<'_>,
        token: &CancellationToken,
    ) -> Result<ToolOutcome> {
        if let Some(tool) = self
            .tools
            .by_name(&call.name)
            .filter(|t| t.requires_approval())
        {
            // A destructive call (e.g. `rm`) always re-prompts, even when its
            // tool was "always allowed" this session: a blanket bash allow must
            // not silently auto-run data-losing commands.
            let destructive = tool.is_destructive(&call.arguments);
            let session_allowed = self.session_allowed.contains(&call.name);
            let auto_approved = session_allowed && !destructive && tool.supports_allow_always();
            if auto_approved {
                obs.on_event(AgentEvent::ToolAutoApproved(call.clone()))?;
            }
            if let Some(diff) = tool.diff_preview(env.workspace, &call.arguments) {
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
                // Race the approval against cancellation so a pending prompt
                // does not pin the turn open after a Ctrl-C. Cancellation is
                // recorded as a cancelled call (not a denial) so the transcript
                // reflects user intent rather than a refusal.
                let decision = tokio::select! {
                    biased;
                    _ = token.cancelled() => return Ok(ToolOutcome::Cancelled),
                    decision = gate.review(call, tool.supports_allow_always()) => decision?,
                };
                // A blocking front-end prompt (real terminal) cannot observe
                // the token mid-read, so it may still return a decision after a
                // Ctrl-C landed. Treat the turn cancellation as authoritative so
                // a late Allow/Deny neither runs the tool nor mutates the
                // session allow-policy.
                if token.is_cancelled() {
                    return Ok(ToolOutcome::Cancelled);
                }
                match decision {
                    ApprovalDecision::Deny => {
                        tracing::warn!(tool = %call.name, "tool call denied by user");
                        return Ok(ToolOutcome::Denied);
                    }
                    ApprovalDecision::AllowAlways => {
                        if tool.supports_allow_always() {
                            tracing::info!(tool = %call.name, "tool always-allowed this session");
                            self.session_allowed.insert(call.name.clone());
                        } else {
                            obs.on_event(AgentEvent::Notice(format!(
                                "always-allow is disabled for `{}`; it requires approval each time.",
                                call.name
                            )))?;
                        }
                    }
                    ApprovalDecision::Allow => {}
                }
            }
        } else {
            obs.on_event(AgentEvent::ToolProposed(call.clone()))?;
        }

        // Resolve again for execution (the approval borrow above has ended); an
        // unknown tool yields the same `unknown tool: <name>` result as before.
        let outcome = match self.tools.by_name(&call.name) {
            Some(tool) => run_tool(tool, &call.arguments, env, token.child_token()).await,
            None => ToolOutcome::Err(anyhow::anyhow!("unknown tool: {}", call.name)),
        };
        Ok(outcome)
    }

    fn emit_interrupted(&self, obs: &dyn AgentObserver) -> Result<()> {
        obs.on_event(AgentEvent::Notice(INTERRUPT_NOTICE.to_string()))?;
        obs.on_event(AgentEvent::TurnComplete)
    }
}

/// Run a bounded batch of concurrency-safe calls concurrently, returning outcomes
/// in the same order as `calls`. Each call gets its own child cancellation token.
/// Uses ordered buffering (not `tokio::spawn`) so the `!Send` borrowed futures run
/// on the loop's executor without queuing unbounded blocking work.
async fn run_parallel(
    tools: &Tools,
    calls: &[ToolCall],
    env: &ToolEnv<'_>,
    token: &CancellationToken,
) -> Vec<ToolOutcome> {
    futures::stream::iter(calls.iter())
        .map(|call| {
            let cancel = token.child_token();
            async move {
                match tools.by_name(&call.name) {
                    Some(tool) => run_tool(tool, &call.arguments, env, cancel).await,
                    None => ToolOutcome::Err(anyhow::anyhow!("unknown tool: {}", call.name)),
                }
            }
        })
        .buffered(MAX_PARALLEL_TOOL_CALLS)
        .collect()
        .await
}

/// Run one tool, racing its future against the (child) cancellation token. The
/// pre-check matters: a synchronous tool body would otherwise run to completion
/// on the first poll even when already cancelled (the select is `biased` toward
/// the tool so a cooperative tool's own result wins over the synthetic one). The
/// post-check maps sync tools that observe cancellation internally to the same
/// transcript-valid cancelled outcome.
async fn run_tool<'a>(
    tool: &'a dyn Tool,
    args: &'a Value,
    env: &'a ToolEnv<'_>,
    cancel: CancellationToken,
) -> ToolOutcome {
    if cancel.is_cancelled() {
        return ToolOutcome::Cancelled;
    }
    tokio::select! {
        biased;
        result = tool.execute(args, env, cancel.clone()) => match result {
            _ if cancel.is_cancelled() => ToolOutcome::Cancelled,
            Ok(output) => ToolOutcome::Ok(output),
            Err(error) => ToolOutcome::Err(error),
        },
        _ = cancel.cancelled() => ToolOutcome::Cancelled,
    }
}

/// Append one tool call and its result to the transcript and emit the matching
/// event. Every model-emitted call goes through here exactly once, so the
/// assistant-tool-call / tool-result pairing is always complete.
fn record_call(
    messages: &mut Vec<Message>,
    obs: &dyn AgentObserver,
    call: &ToolCall,
    outcome: ToolOutcome,
) -> Result<()> {
    // Append the assistant tool call and its paired result together, BEFORE
    // emitting the observer event. An observer error must not early-return
    // between the two pushes: Wayland persists the transcript even on a turn
    // error, so a dangling assistant-tool-call with no matching tool-result
    // would be flushed to disk and rejected by the next provider request.
    messages.push(Message::assistant_tool_call(call));
    let event = match outcome {
        ToolOutcome::Ok(output) => {
            tracing::info!(tool = %call.name, ok = true, "tool executed");
            let content = output.content.clone();
            messages.push(Message::tool_result(
                &call.id,
                &call.name,
                &tool_result_json(&Ok(output)),
            ));
            AgentEvent::ToolResult {
                call: call.clone(),
                content,
            }
        }
        ToolOutcome::Err(error) => {
            tracing::info!(tool = %call.name, ok = false, "tool executed");
            let message = format!("{error:#}");
            messages.push(Message::tool_result(
                &call.id,
                &call.name,
                &tool_result_json(&Err(error)),
            ));
            AgentEvent::ToolError {
                call: call.clone(),
                message,
            }
        }
        ToolOutcome::Cancelled => {
            tracing::info!(tool = %call.name, "tool cancelled");
            messages.push(Message::tool_result(
                &call.id,
                &call.name,
                &cancelled_tool_result_json(),
            ));
            AgentEvent::ToolError {
                call: call.clone(),
                message: "cancelled".to_string(),
            }
        }
        ToolOutcome::Denied => {
            tracing::warn!(tool = %call.name, "tool call denied");
            messages.push(Message::tool_result(
                &call.id,
                &call.name,
                &denied_tool_result_json(),
            ));
            AgentEvent::ToolDenied(call.clone())
        }
    };
    obs.on_event(event)
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

    /// Inverse of [`as_str`](Self::as_str): parse a persisted role string back
    /// into a `Role`. Used by the session store to reconstruct messages when
    /// reading a transcript. `None` for an unknown role.
    pub(crate) fn from_wire(role: &str) -> Option<Self> {
        match role {
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            "assistant_tool_call" => Some(Self::AssistantToolCall),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }
}

/// Pair a trailing tool call that has no recorded result. A prior session that
/// crashed between persisting an `AssistantToolCall` and its `Tool` result
/// leaves the call unanswered as the last entry; appending a new user prompt
/// then yields a sequence providers reject (every tool call must be answered).
/// At most one such call can dangle (the loop records each call's result
/// adjacently), so one synthetic result restores validity. The appended message
/// is new (beyond the persisted cursor), so the harness writes it to the same
/// log, keeping disk and memory consistent.
fn repair_dangling_tool_call(messages: &mut Vec<Message>) {
    let Some(last) = messages.last() else { return };
    if last.role != Role::AssistantToolCall {
        return;
    }
    let (Some(call_id), Some(name)) = (last.tool_call_id.clone(), last.tool_name.clone()) else {
        return;
    };
    messages.push(Message::tool_result(
        &call_id,
        &name,
        &cancelled_tool_result_json(),
    ));
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

// Model-facing cancellation payload, distinct from a tool error so the model can
// tell "interrupted by the user" apart from "the tool failed".
fn cancelled_tool_result_json() -> String {
    json!({ "ok": false, "error": "tool call cancelled by user", "cancelled": true }).to_string()
}

#[cfg(test)]
#[path = "nexus_tests.rs"]
mod tests;
