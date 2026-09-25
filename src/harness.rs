//! Benchmark harness façade — the ONLY public surface the `iris-bench`
//! workspace crate depends on. It wraps the internal agent/provider/tool
//! machinery so callers never touch `nexus`, `tools`, `config`, or `mimir`
//! directly. One real-provider cell in, rich per-turn/per-tool metrics out.
//!
//! Design (ADR: iris-agent lib/bin split): the reduction arm is toggled by the
//! internal, harness-only `ToolState::with_reduce_output`; fixtures, workload
//! catalogs, success checks, matrix expansion, parallelism, logging, and any UI
//! live in `iris-bench`, not here.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use futures::StreamExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::mimir::providers::openai_compatible_chat::{
    OpenAiCompatibleChatConfig, OpenAiCompatibleChatProvider,
};
use crate::mimir::retry::RetryPolicy;
use crate::mimir::selection::{PromptCacheRetention, ProviderId, ReasoningEffort};
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ApprovalMode,
    ChatProvider, Message, ProviderEvent, ProviderUsage, ReviewContext, Tool, ToolCall, ToolEnv,
    ToolEventState, ToolFuture, ToolOutput, Tools as NexusTools,
};
use crate::tools::{ToolState, built_in_tools};

/// The reduction arm for one cell. `Defaults` runs with Iris's default-on tool
/// output reductions; `Baseline` forces them off. This is the only benchmark
/// lever and is held identical to production behavior except for that switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm {
    /// Tool-output reductions ON (Iris default).
    Defaults,
    /// Tool-output reductions OFF (comparison baseline).
    Baseline,
}

impl Arm {
    /// Whether tool-output reductions are enabled for this arm.
    pub fn reduce(self) -> bool {
        matches!(self, Arm::Defaults)
    }

    /// Stable lowercase label for logs/UI.
    pub fn label(self) -> &'static str {
        match self {
            Arm::Defaults => "defaults",
            Arm::Baseline => "baseline",
        }
    }
}

/// Opaque, resolved model selection. Wraps the internal `ModelSelection` so the
/// public API never leaks `mimir` types. Build one with [`selection_for_spec`].
pub struct ModelSelection {
    inner: crate::mimir::selection::ModelSelection,
}

/// One benchmark cell to execute against a real provider.
pub struct CellSpec<'a> {
    /// Workspace directory the agent operates in. Must already exist and be
    /// populated (the caller materializes fixtures). For `skip_permissions`
    /// runs it must be OUTSIDE the iris-agent source tree.
    pub workspace: &'a Path,
    /// The user turn/prompt to submit.
    pub prompt: &'a str,
    /// Reduction arm.
    pub arm: Arm,
    /// Bypass the approval gate for every gated call (ADR-0049). Only enable
    /// for confined temp workspaces running trusted benchmark workloads.
    pub skip_permissions: bool,
    /// Resolved model selection from [`selection_for_spec`].
    pub selection: &'a ModelSelection,
    /// Cancellation token; cancel aborts the provider turn.
    pub cancel: &'a CancellationToken,
}

/// Rich per-cell observation. Mirrors what the JSONL log records, minus the
/// success/validity judgement (the caller runs the workload check).
#[derive(Clone, Debug, Default)]
pub struct CellResult {
    /// Final assistant text of the turn.
    pub final_text: String,
    /// Number of provider turns (round-trips) in the completion.
    pub turns: u32,
    /// Cumulative real input tokens across turns.
    pub input_tokens: u64,
    /// Cumulative output tokens.
    pub output_tokens: u64,
    /// Cumulative reasoning (thinking) output tokens.
    pub reasoning_tokens: u64,
    /// Cumulative cache-read input tokens.
    pub cache_read_tokens: u64,
    /// Cumulative total tokens as reported by the provider.
    pub total_tokens: u64,
    /// Successful tool executions keyed by tool name.
    pub tool_counts: BTreeMap<String, u32>,
    /// Count of large outputs offloaded behind a handle.
    pub handles_stored: u32,
    /// Per provider turn: (input_tokens, output_tokens), in order.
    pub per_turn: Vec<(u64, u64)>,
    /// Whether the approval gate was consulted (a prompt occurred). Under the
    /// auto preset with auto-approvable tools this must stay false.
    pub approvals_consulted: bool,
    /// Calls auto-approved by skip-permissions (ADR-0049).
    pub dangerous_approvals: u32,
    /// Ordered tool-call names as executed (every attempt).
    pub tool_sequence: Vec<String>,
    /// Tool errors as (name, truncated message).
    pub tool_errors: Vec<(String, String)>,
    /// Total bytes of tool RESULT content that entered context.
    pub tool_result_bytes: u64,
    /// Per-tool result bytes (same total, split by tool name).
    pub tool_result_bytes_by_tool: BTreeMap<String, u64>,
    /// Exit codes reported by `bash` results, in order.
    pub bash_exit_codes: Vec<i32>,
}

/// Resolve a `provider:model` spec into a [`ModelSelection`], overriding
/// provider/model/base-URL/reasoning on top of the config-resolved base (so
/// cache/retry/context-management defaults are inherited). `reasoning` is an
/// optional effort string (e.g. `"low"`, `"none"`); invalid values error.
pub fn selection_for_spec(
    config_cwd: &Path,
    spec: &str,
    reasoning: Option<&str>,
) -> std::result::Result<ModelSelection, String> {
    use crate::mimir::selection::{
        ModelSelection as Inner, ProviderId, ReasoningEffort, base_url_for,
    };
    let (provider_str, model) = spec
        .split_once(':')
        .ok_or_else(|| format!("model spec {spec:?} must be 'provider:model'"))?;
    let provider = ProviderId::parse(provider_str).map_err(|e| e.to_string())?;
    let reasoning = match reasoning {
        None => None,
        Some(raw) if raw.trim().eq_ignore_ascii_case("none") => None,
        Some(raw) => Some(ReasoningEffort::parse(raw.trim()).map_err(|e| e.to_string())?),
    };
    let settings = crate::config::Settings::load(config_cwd).map_err(|e| e.to_string())?;
    let mut selection = Inner::resolve(&settings).map_err(|e| e.to_string())?;
    selection.provider = provider;
    selection.model = model.trim().to_string();
    selection.base_url = base_url_for(provider, None);
    selection.reasoning = reasoning;
    crate::mimir::model_capabilities::validate(&selection).map_err(|e| e.to_string())?;
    Ok(ModelSelection { inner: selection })
}

/// Validate that a `provider:model` spec (with optional reasoning) is reachable
/// and well-formed, without running a turn. Cheap pre-flight for the UI.
pub fn validate_model(
    config_cwd: &Path,
    spec: &str,
    reasoning: Option<&str>,
) -> std::result::Result<(), String> {
    selection_for_spec(config_cwd, spec, reasoning).map(|_| ())
}

/// Execute one real-provider cell and return its observation. Fallible: a
/// provider/build error is returned as `Err(message)` so the caller can record
/// per-cell reachability instead of aborting the whole matrix.
pub fn run_cell(spec: &CellSpec<'_>) -> std::result::Result<CellResult, String> {
    let cwd = spec.workspace;
    // Confinement guard: skip-permissions runs execute shell; never allow one
    // inside the iris-agent source tree. Return an error, do not panic.
    if spec.skip_permissions {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let real = cwd
            .canonicalize()
            .map_err(|e| format!("workspace {}: {e}", cwd.display()))?;
        if real.starts_with(manifest) {
            return Err(format!(
                "refusing skip-permissions run inside the source tree: {}",
                real.display()
            ));
        }
    }

    let tools = built_in_tools();
    let system_prompt = crate::wayland::system_prompt::assemble(cwd, &tools);
    let settings = crate::config::Settings::load(cwd).map_err(|e| e.to_string())?;
    let session_id = crate::session::new_session_id();
    let provider = crate::build_provider(&spec.selection.inner, &system_prompt, &session_id)
        .map_err(|e| format!("build provider: {e}"))?;
    let mut agent = Agent::new(provider, built_in_tools())
        .with_max_tool_roundtrips(settings.max_tool_roundtrips());
    if spec.skip_permissions {
        agent = agent.with_skip_permissions(true);
    }
    agent.set_approval_mode(ApprovalMode::Auto);

    let state = RefCell::new(ToolState::new().with_reduce_output(spec.arm.reduce()));
    let env = ToolEnv {
        workspace: cwd,
        state: &state,
        output_store: None,
        output_sink: None,
        mutation_guard: None,
        session_span: None,
    };
    let observer = BenchObserver::default();
    let gate = ZeroPromptGate::default();
    block_on(agent.submit_turn(spec.prompt, &observer, &gate, &env, spec.cancel, None))
        .map_err(|e| format!("provider turn: {e}"))?;

    Ok(CellResult {
        final_text: observer.final_text(),
        turns: observer.provider_turns.get(),
        input_tokens: observer.usage_input_tokens.get(),
        output_tokens: observer.output_tokens.get(),
        reasoning_tokens: observer.reasoning_tokens.get(),
        cache_read_tokens: observer.cache_read.get(),
        total_tokens: observer.total_tokens.get(),
        tool_counts: observer.tool_counts.borrow().clone(),
        handles_stored: observer.handles_stored.get(),
        per_turn: observer.per_turn.borrow().clone(),
        approvals_consulted: gate.consulted.get(),
        dangerous_approvals: observer.dangerous_approvals.get(),
        tool_sequence: observer.tool_sequence.borrow().clone(),
        tool_errors: observer.tool_errors.borrow().clone(),
        tool_result_bytes: observer.tool_result_bytes.get(),
        tool_result_bytes_by_tool: observer.tool_result_bytes_by_tool.borrow().clone(),
        bash_exit_codes: observer.bash_exit_codes.borrow().clone(),
    })
}

/// Drive one async future to completion on a fresh current-thread runtime.
/// Callers MUST invoke `run_cell` from an ordinary OS thread, never from inside
/// an async Tokio task (that would panic on nested runtime creation).
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(future)
}

// ---------------------------------------------------------------------------
// Internal instrumentation (shared with the `#[cfg(test)]` replay bench via a
// re-export in `bench_tokens/observer.rs`).
// ---------------------------------------------------------------------------

/// Rich per-run instrumentation implementing the internal `AgentObserver`.
#[derive(Default)]
pub(crate) struct BenchObserver {
    pub(crate) final_text: RefCell<String>,
    pub(crate) usage_input_tokens: Cell<u64>,
    pub(crate) output_tokens: Cell<u64>,
    pub(crate) reasoning_tokens: Cell<u64>,
    pub(crate) cache_read: Cell<u64>,
    pub(crate) total_tokens: Cell<u64>,
    pub(crate) provider_turns: Cell<u32>,
    pub(crate) tool_counts: RefCell<BTreeMap<String, u32>>,
    pub(crate) handles_stored: Cell<u32>,
    pub(crate) per_turn: RefCell<Vec<(u64, u64)>>,
    pub(crate) dangerous_approvals: Cell<u32>,
    pub(crate) tool_sequence: RefCell<Vec<String>>,
    pub(crate) tool_errors: RefCell<Vec<(String, String)>>,
    pub(crate) tool_result_bytes: Cell<u64>,
    pub(crate) tool_result_bytes_by_tool: RefCell<BTreeMap<String, u64>>,
    pub(crate) bash_exit_codes: RefCell<Vec<i32>>,
}

impl BenchObserver {
    pub(crate) fn final_text(&self) -> String {
        self.final_text.borrow().clone()
    }
}

impl AgentObserver for BenchObserver {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        match event {
            AgentEvent::AssistantText(text) | AgentEvent::AssistantTextEnd(text)
                if !text.is_empty() =>
            {
                *self.final_text.borrow_mut() = text;
            }
            AgentEvent::ProviderTurnCompleted { usage, .. } => {
                self.provider_turns.set(self.provider_turns.get() + 1);
                let (mut inp, mut out) = (0u64, 0u64);
                if let Some(usage) = usage {
                    inp = usage.input_tokens;
                    out = usage.output_tokens;
                    self.usage_input_tokens
                        .set(self.usage_input_tokens.get() + usage.input_tokens);
                    self.output_tokens
                        .set(self.output_tokens.get() + usage.output_tokens);
                    self.reasoning_tokens
                        .set(self.reasoning_tokens.get() + usage.reasoning_output_tokens);
                    self.cache_read
                        .set(self.cache_read.get() + usage.cache_read_input_tokens);
                    self.total_tokens
                        .set(self.total_tokens.get() + usage.total_tokens);
                }
                self.per_turn.borrow_mut().push((inp, out));
            }
            AgentEvent::ToolLifecycle {
                name,
                state: ToolEventState::Succeeded,
                ..
            } => {
                *self.tool_counts.borrow_mut().entry(name).or_insert(0) += 1;
            }
            AgentEvent::OutputHandleStored { .. } => {
                self.handles_stored.set(self.handles_stored.get() + 1);
            }
            AgentEvent::ToolStarted(call) => {
                self.tool_sequence.borrow_mut().push(call.name);
            }
            AgentEvent::ToolAutoApprovedDangerous(_) => {
                self.dangerous_approvals
                    .set(self.dangerous_approvals.get() + 1);
            }
            AgentEvent::ToolResult {
                call,
                content,
                exit_code,
                ..
            } => {
                let bytes = content.len() as u64;
                self.tool_result_bytes
                    .set(self.tool_result_bytes.get() + bytes);
                *self
                    .tool_result_bytes_by_tool
                    .borrow_mut()
                    .entry(call.name.clone())
                    .or_insert(0) += bytes;
                if call.name == "bash" {
                    self.bash_exit_codes
                        .borrow_mut()
                        .push(exit_code.unwrap_or(0));
                }
            }
            AgentEvent::ToolError { call, message } => {
                let message: String = message.chars().take(200).collect();
                self.tool_errors.borrow_mut().push((call.name, message));
            }
            _ => {}
        }
        Ok(())
    }
}

/// Approval gate that must never be consulted under the auto preset with only
/// auto-approvable tools. If consulted it records the fact and denies.
#[derive(Default)]
pub(crate) struct ZeroPromptGate {
    pub(crate) consulted: Cell<bool>,
}

impl ApprovalGate for ZeroPromptGate {
    fn review<'a>(
        &'a self,
        _call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        _ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        self.consulted.set(true);
        Box::pin(async move { Ok(ApprovalDecision::Deny) })
    }
}

// ===========================================================================
// Leaf-consumer API (spike S1). A minimal public seam for an external binary
// to drive one OpenAI-compatible provider turn and one Nexus tool-loop turn
// with custom request headers, a custom system prompt, and an injected tool.
// ===========================================================================

/// Request options for a minimal OpenAI-compatible (`chat/completions`)
/// provider. `system_prompt` is carried by the provider itself, so an injected
/// `Agent` uses a custom prompt without the Wayland settings stack.
#[derive(Clone, Debug, Default)]
pub struct LeafProviderOptions {
    pub base_url: String,
    pub model: String,
    pub system_prompt: String,
    /// Bearer API key. Never rendered by this crate.
    pub api_key: String,
    /// Extra request headers applied after the adapter defaults, so a caller
    /// can override `User-Agent` and add e.g. `x-opencode-session`.
    pub extra_headers: Vec<(String, String)>,
    /// Optional reasoning effort token (`low`/`medium`/`high`/`xhigh`/`max`).
    pub reasoning: Option<String>,
    /// Optional graceful soft cap on tool round-trips for [`leaf_tool_turn`].
    /// `None` (the default) means no cap: the loop ends when the model stops
    /// calling tools. `Some(n)` applies Nexus' normal soft-cap behaviour (ends
    /// the turn gracefully with a notice after `n` tool rounds).
    pub max_tool_roundtrips: Option<usize>,
}

/// Token accounting for one leaf turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LeafUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub total_tokens: u64,
}

impl LeafUsage {
    fn from_usage(usage: &ProviderUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: usage.reasoning_output_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            total_tokens: usage.total_tokens,
        }
    }

    fn saturating_add_assign(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(other.reasoning_output_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }
}

/// A caller-provided tool for [`leaf_tool_turn`]. Synchronous by design: the
/// adapter runs it inside the Nexus async tool future.
pub trait LeafTool {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn execute(&self, arguments: &Value) -> std::result::Result<String, String>;
}

struct LeafToolAdapter {
    inner: Box<dyn LeafTool>,
}

impl Tool for LeafToolAdapter {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters(&self) -> Value {
        self.inner.parameters()
    }

    fn is_concurrency_safe(&self) -> bool {
        true
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            self.inner
                .execute(args)
                .map(ToolOutput::text)
                .map_err(|message| anyhow::anyhow!("{message}"))
        })
    }
}

/// One event observed during a leaf tool-loop turn.
#[derive(Clone, Debug, PartialEq)]
pub enum LeafEvent {
    ToolCall { name: String, arguments: Value },
    ToolResult { name: String, content: String },
}

/// Outcome of a one-shot provider turn.
#[derive(Clone, Debug, Default)]
pub struct LeafOneShot {
    pub text: String,
    pub reasoning: String,
    pub usage: Option<LeafUsage>,
}

/// Outcome of a leaf tool-loop turn.
#[derive(Clone, Debug, Default)]
pub struct LeafToolTurn {
    pub final_text: String,
    pub events: Vec<LeafEvent>,
    /// Token usage summed (saturating) over every provider turn of the run that
    /// reported usage. `None` only if no provider turn reported usage.
    pub usage: Option<LeafUsage>,
    /// Per-provider-turn usage, in order, for each turn that reported usage.
    pub turn_usage: Vec<LeafUsage>,
    /// Number of provider turns in the run, with or without usage.
    pub provider_turns: usize,
    /// True iff the run stopped because the tool round-trip cap was reached.
    pub hit_roundtrip_cap: bool,
}

fn leaf_provider(
    options: &LeafProviderOptions,
) -> std::result::Result<OpenAiCompatibleChatProvider, String> {
    let reasoning = match options.reasoning.as_deref() {
        None => None,
        Some(raw) => Some(ReasoningEffort::parse(raw.trim()).map_err(|e| e.to_string())?),
    };
    OpenAiCompatibleChatProvider::new(OpenAiCompatibleChatConfig {
        provider: ProviderId::OpenAiCompatible,
        model: &options.model,
        base_url: &options.base_url,
        reasoning,
        system_prompt: &options.system_prompt,
        api_key: Some(options.api_key.clone()),
        supports_reasoning: true,
        api_key_required: true,
        prompt_cache_key: None,
        cache_retention: PromptCacheRetention::None,
        retry_policy: RetryPolicy::default(),
        extra_headers: options.extra_headers.clone(),
    })
    .map_err(|e| format!("{e:#}"))
}

/// Run exactly one user message through the OpenAI-compatible provider and
/// return its assembled text, reasoning, and usage.
pub fn leaf_oneshot(
    options: &LeafProviderOptions,
    user_message: &str,
) -> std::result::Result<LeafOneShot, String> {
    let provider = leaf_provider(options)?;
    let tools = NexusTools::new(Vec::new());
    let cancel = CancellationToken::new();
    let messages = vec![Message::user(user_message)];
    block_on(async {
        let mut stream = provider
            .respond_stream(&messages, &tools, &cancel)
            .map_err(|e| format!("{e:#}"))?;
        let mut outcome = LeafOneShot::default();
        while let Some(item) = stream.next().await {
            match item.map_err(|e| format!("{e:#}"))? {
                ProviderEvent::TextDelta(delta) => outcome.text.push_str(&delta),
                ProviderEvent::ReasoningDelta(delta) | ProviderEvent::RawReasoningDelta(delta) => {
                    outcome.reasoning.push_str(&delta)
                }
                ProviderEvent::Completed(turn) => {
                    if let Some(text) = turn.text {
                        outcome.text = text;
                    }
                    outcome.usage = turn.usage.as_ref().map(LeafUsage::from_usage);
                }
                _ => {}
            }
        }
        Ok(outcome)
    })
}

/// Stable fragment of Nexus' graceful soft-cap notice. Nexus emits
/// `"stopped after {cap} tool round-trips; send another message to continue."`
/// only on the cap path, so this text is the signal for [`LeafToolTurn::hit_roundtrip_cap`].
const ROUNDTRIP_CAP_NOTICE_FRAGMENT: &str = "tool round-trips; send another message to continue.";

#[derive(Default)]
struct LeafObserver {
    final_text: RefCell<String>,
    events: RefCell<Vec<LeafEvent>>,
    usage: RefCell<Option<LeafUsage>>,
    turn_usage: RefCell<Vec<LeafUsage>>,
    provider_turns: Cell<usize>,
    hit_roundtrip_cap: Cell<bool>,
}

impl AgentObserver for LeafObserver {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        match event {
            AgentEvent::AssistantText(text) | AgentEvent::AssistantTextEnd(text)
                if !text.is_empty() =>
            {
                *self.final_text.borrow_mut() = text;
            }
            AgentEvent::ToolStarted(call) => self.events.borrow_mut().push(LeafEvent::ToolCall {
                name: call.name,
                arguments: call.arguments,
            }),
            AgentEvent::ToolResult { call, content, .. } => {
                self.events.borrow_mut().push(LeafEvent::ToolResult {
                    name: call.name,
                    content,
                });
            }
            AgentEvent::ProviderTurnCompleted { usage, .. } => {
                self.provider_turns.set(self.provider_turns.get() + 1);
                if let Some(usage) = usage {
                    let leaf = LeafUsage::from_usage(&usage);
                    self.turn_usage.borrow_mut().push(leaf.clone());
                    let mut total = self.usage.borrow_mut();
                    match total.as_mut() {
                        Some(total) => total.saturating_add_assign(&leaf),
                        None => *total = Some(leaf),
                    }
                }
            }
            AgentEvent::Notice(message) if message.contains(ROUNDTRIP_CAP_NOTICE_FRAGMENT) => {
                self.hit_roundtrip_cap.set(true);
            }
            _ => {}
        }
        Ok(())
    }
}

/// Always-allow approval gate: every gated call is approved for this session.
struct LeafAlwaysAllowGate;

impl ApprovalGate for LeafAlwaysAllowGate {
    fn review<'a>(
        &'a self,
        _call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        _ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::AllowAlways) })
    }
}

/// Run one Nexus agent turn with a custom system prompt (via the provider), the
/// injected `tools`, and an always-allow approval gate. Returns the final
/// assistant text and the observed tool-call/tool-result events.
pub fn leaf_tool_turn(
    options: &LeafProviderOptions,
    tools: Vec<Box<dyn LeafTool>>,
    user_message: &str,
) -> std::result::Result<LeafToolTurn, String> {
    let provider = leaf_provider(options)?;
    run_leaf_tool_turn(provider, options, tools, user_message)
}

/// Run the leaf tool loop against an already-built provider. Split out so tests
/// can drive the same observer and cap wiring with a scripted provider.
fn run_leaf_tool_turn<P: ChatProvider>(
    provider: P,
    options: &LeafProviderOptions,
    tools: Vec<Box<dyn LeafTool>>,
    user_message: &str,
) -> std::result::Result<LeafToolTurn, String> {
    let nexus_tools: Vec<Box<dyn Tool>> = tools
        .into_iter()
        .map(|inner| Box::new(LeafToolAdapter { inner }) as Box<dyn Tool>)
        .collect();
    let mut agent = Agent::new(provider, NexusTools::new(nexus_tools))
        .with_max_tool_roundtrips(options.max_tool_roundtrips);
    agent.set_approval_mode(ApprovalMode::Auto);

    let state = RefCell::new(ToolState::new());
    let env = ToolEnv {
        workspace: Path::new("."),
        state: &state,
        output_store: None,
        output_sink: None,
        mutation_guard: None,
        session_span: None,
    };
    let observer = LeafObserver::default();
    let gate = LeafAlwaysAllowGate;
    let cancel = CancellationToken::new();
    block_on(agent.submit_turn(user_message, &observer, &gate, &env, &cancel, None))
        .map_err(|e| format!("{e:#}"))?;
    Ok(LeafToolTurn {
        final_text: observer.final_text.into_inner(),
        events: observer.events.into_inner(),
        usage: observer.usage.into_inner(),
        turn_usage: observer.turn_usage.into_inner(),
        provider_turns: observer.provider_turns.into_inner(),
        hit_roundtrip_cap: observer.hit_roundtrip_cap.into_inner(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::{AssistantTurn, ProviderStream, Tools};
    use std::collections::VecDeque;

    /// Scripted provider: one terminal `Completed` turn per call, in order. The
    /// leaf tests bypass `leaf_provider` and drive [`run_leaf_tool_turn`]
    /// directly, so no network or credentials are involved.
    struct ScriptedLeafProvider {
        turns: RefCell<VecDeque<AssistantTurn>>,
    }

    impl ScriptedLeafProvider {
        fn new(turns: Vec<AssistantTurn>) -> Self {
            Self {
                turns: RefCell::new(turns.into()),
            }
        }
    }

    impl ChatProvider for ScriptedLeafProvider {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            let turn = self
                .turns
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| AssistantTurn::text("(script exhausted)"));
            let event: Result<ProviderEvent> = Ok(ProviderEvent::Completed(turn));
            Ok(Box::pin(futures::stream::once(async move { event })))
        }
    }

    fn usage(
        input_tokens: u64,
        output_tokens: u64,
        reasoning_output_tokens: u64,
        cache_read_input_tokens: u64,
    ) -> ProviderUsage {
        ProviderUsage {
            provider: "test".to_string(),
            model: "test".to_string(),
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens: 0,
            reasoning_output_tokens,
            total_tokens: input_tokens + output_tokens,
            cache_creation: None,
        }
    }

    fn tool_turn(call_id: &str, usage: Option<ProviderUsage>) -> AssistantTurn {
        AssistantTurn {
            tool_calls: vec![ToolCall {
                id: call_id.to_string(),
                name: "echo".to_string(),
                arguments: Value::Null,
                thought_signature: None,
            }],
            usage,
            ..AssistantTurn::default()
        }
    }

    fn counting_tool() -> Box<dyn LeafTool> {
        struct CountingTool;
        impl LeafTool for CountingTool {
            fn name(&self) -> &str {
                "echo"
            }
            fn description(&self) -> &str {
                "echo"
            }
            fn parameters(&self) -> Value {
                serde_json::json!({ "type": "object" })
            }
            fn execute(&self, _arguments: &Value) -> std::result::Result<String, String> {
                Ok("ok".to_string())
            }
        }
        Box::new(CountingTool)
    }

    fn tool_results(turn: &LeafToolTurn) -> usize {
        turn.events
            .iter()
            .filter(|event| matches!(event, LeafEvent::ToolResult { .. }))
            .count()
    }

    fn run(turns: Vec<AssistantTurn>, max_tool_roundtrips: Option<usize>) -> LeafToolTurn {
        let provider = ScriptedLeafProvider::new(turns);
        let options = LeafProviderOptions {
            max_tool_roundtrips,
            ..LeafProviderOptions::default()
        };
        run_leaf_tool_turn(provider, &options, vec![counting_tool()], "go").expect("leaf tool turn")
    }

    #[test]
    fn leaf_tool_turn_sums_usage_over_all_provider_turns() {
        let turn = run(
            vec![
                tool_turn("c1", Some(usage(10, 1, 2, 3))),
                tool_turn("c2", Some(usage(20, 2, 4, 5))),
                AssistantTurn {
                    text: Some("done".to_string()),
                    usage: Some(usage(30, 3, 6, 7)),
                    ..AssistantTurn::default()
                },
            ],
            None,
        );

        assert_eq!(turn.provider_turns, 3);
        assert_eq!(turn.turn_usage.len(), 3);
        assert_eq!(tool_results(&turn), 2);
        assert_eq!(
            turn.usage,
            Some(LeafUsage {
                input_tokens: 60,
                output_tokens: 6,
                reasoning_output_tokens: 12,
                cache_read_input_tokens: 15,
                total_tokens: 66,
            })
        );
        assert!(!turn.hit_roundtrip_cap);
    }

    #[test]
    fn leaf_tool_turn_without_cap_runs_past_eight_roundtrips() {
        let mut turns: Vec<AssistantTurn> = (0..10)
            .map(|i| tool_turn(&format!("c{i}"), Some(usage(1, 1, 0, 0))))
            .collect();
        turns.push(AssistantTurn::text("done"));

        let turn = run(turns, None);

        assert_eq!(tool_results(&turn), 10);
        assert_eq!(turn.provider_turns, 11);
        assert!(!turn.hit_roundtrip_cap);
    }

    #[test]
    fn leaf_tool_turn_with_cap_reports_it() {
        let mut turns: Vec<AssistantTurn> = (0..10)
            .map(|i| tool_turn(&format!("c{i}"), Some(usage(1, 1, 0, 0))))
            .collect();
        turns.push(AssistantTurn::text("done"));

        let turn = run(turns, Some(2));

        assert_eq!(tool_results(&turn), 2);
        assert_eq!(turn.provider_turns, 2);
        assert!(turn.hit_roundtrip_cap);
    }

    #[test]
    fn leaf_tool_turn_usage_skips_a_turn_without_usage() {
        let turn = run(
            vec![
                tool_turn("c1", Some(usage(10, 1, 2, 3))),
                tool_turn("c2", None),
                AssistantTurn {
                    text: Some("done".to_string()),
                    usage: Some(usage(30, 3, 6, 7)),
                    ..AssistantTurn::default()
                },
            ],
            None,
        );

        assert_eq!(turn.provider_turns, 3);
        assert_eq!(turn.turn_usage.len(), 2);
        assert_eq!(tool_results(&turn), 2);
        assert_eq!(
            turn.usage,
            Some(LeafUsage {
                input_tokens: 40,
                output_tokens: 4,
                reasoning_output_tokens: 8,
                cache_read_input_tokens: 10,
                total_tokens: 44,
            })
        );
    }
}
