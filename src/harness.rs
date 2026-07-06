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
use tokio_util::sync::CancellationToken;

use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ApprovalMode,
    ReviewContext, ToolCall, ToolEnv, ToolEventState,
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
