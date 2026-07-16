//! Wayland adapter between the host-neutral worker runtime and Nexus child agents.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use iris_subagent_runtime::worktree::{
    WorktreeCancellation, WorktreeConfig, WorktreeCreateRequest, WorktreeService,
};
use iris_subagent_runtime::{
    ApprovalDecision as RuntimeApprovalDecision, ApprovalPort, ExecutorError, ExecutorOutput,
    IsolationMode, LocalExecutorFuture, Usage, WorkerContext, WorkerExecutor, WorkerRequest,
    WorkerWorktree,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ChatProvider,
    Message, ProviderUsage, ReviewContext, Role, ToolCall, WorkerCapabilityGrant,
};
use crate::tools::{ToolState, built_in_tools};
use crate::wayland::worker_runtime::WorkerRuntime;
use crate::wayland::{Harness, HarnessRuntimeConfig, MutationSafetyConfig};

pub(crate) const IRIS_ROUTE_ID_PREFIX: &str = "iris_model_route_v1_";
pub(crate) const IRIS_ROUTE_PAYLOAD_KIND: &str = "iris_model_route";
const IRIS_ROUTE_SCHEMA_VERSION: u32 = 1;

/// Persisted, non-secret effective provider route for one accepted worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChildRoute {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) base_url: String,
    pub(crate) effort: Option<String>,
}

impl ChildRoute {
    pub(crate) fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        base_url: impl Into<String>,
        effort: Option<&str>,
    ) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            base_url: base_url.into(),
            effort: effort.map(str::to_string),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.provider.trim().is_empty() || self.model.trim().is_empty() {
            anyhow::bail!("route provider and model must not be empty");
        }
        let url = reqwest::Url::parse(self.base_url.trim())
            .context("route base URL must be an absolute URL")?;
        if !url.username().is_empty() || url.password().is_some() {
            anyhow::bail!("route base URL must not contain credentials");
        }
        Ok(())
    }
}

/// Attach the effective route before durable worker acceptance. `profile_id` is
/// left untouched so a later profile resolver can add provenance without
/// changing this execution contract.
pub(crate) fn attach_route(request: &mut WorkerRequest, route: &ChildRoute) -> Result<()> {
    route.validate()?;
    let value = serde_json::to_value(route)?;
    let id = route_id(route)?;
    request.route_id = Some(id);
    let mut payload = iris_subagent_runtime::HostPayload::default();
    payload.schema_version = IRIS_ROUTE_SCHEMA_VERSION;
    payload.kind = IRIS_ROUTE_PAYLOAD_KIND.to_string();
    payload.value = value;
    request.host = payload;
    Ok(())
}

/// Decode and authenticate the route persisted on an accepted request. Requests
/// without `route_id` predate direct routing and retain live-parent inheritance.
pub(crate) fn route_from_request(request: &WorkerRequest) -> Result<Option<ChildRoute>> {
    let Some(id) = request.route_id.as_deref() else {
        return Ok(None);
    };
    if !id.starts_with(IRIS_ROUTE_ID_PREFIX) {
        anyhow::bail!("unsupported worker route identifier '{id}'");
    }
    let malformed = || anyhow::anyhow!("malformed Iris worker route '{id}'");
    let payload = crate::wayland::worker_runtime::original_host_payload(request)
        .map_err(|_| malformed())?
        .ok_or_else(malformed)?;
    if payload.schema_version != IRIS_ROUTE_SCHEMA_VERSION
        || payload.kind != IRIS_ROUTE_PAYLOAD_KIND
    {
        return Err(malformed());
    }
    let route: ChildRoute = serde_json::from_value(payload.value).map_err(|_| malformed())?;
    route.validate().map_err(|_| malformed())?;
    if route_id(&route).map_err(|_| malformed())? != id {
        return Err(malformed());
    }
    Ok(Some(route))
}

fn route_id(route: &ChildRoute) -> Result<String> {
    let digest = Sha256::digest(serde_json::to_vec(route)?);
    let mut suffix = String::with_capacity(32);
    for byte in &digest[..16] {
        write!(&mut suffix, "{byte:02x}")?;
    }
    Ok(format!("{IRIS_ROUTE_ID_PREFIX}{suffix}"))
}

/// Factory that constructs a fresh `!Send` provider on the scheduler thread.
pub(crate) type ChildProviderFactory =
    Arc<dyn Fn(&WorkerRequest) -> Result<Box<dyn ChatProvider>> + Send + Sync + 'static>;

type ChildAgentConfigurator = fn(&WorkerRequest, &mut Agent<Box<dyn ChatProvider>>);

fn configure_child_agent(request: &WorkerRequest, agent: &mut Agent<Box<dyn ChatProvider>>) {
    let config = agent.config_mut();
    config.strict_workspace = Some(true);
    config.allow_outside_workspace_reads = request.policy.allow_outside_workspace;
}

/// Iris-owned child-agent adapter sharing the single backend scheduler.
pub(crate) struct SubagentBackend {
    runtime: Arc<WorkerRuntime>,
    workspace: PathBuf,
    worktrees: Arc<WorktreeService>,
    configure_agent: ChildAgentConfigurator,
}

impl SubagentBackend {
    pub(crate) fn open(
        workspace: PathBuf,
        state_dir: &Path,
        worktree_root: PathBuf,
    ) -> Result<Self> {
        Ok(Self {
            runtime: WorkerRuntime::open(state_dir)?,
            workspace,
            worktrees: Arc::new(WorktreeService::open(WorktreeConfig::new(worktree_root))?),
            configure_agent: configure_child_agent,
        })
    }

    pub(crate) fn spawn(
        &self,
        factory: ChildProviderFactory,
        request: WorkerRequest,
        approval: Option<Arc<dyn ApprovalPort>>,
    ) -> Result<iris_subagent_runtime::WorkerId> {
        let workspace = self.workspace.clone();
        let worktrees = self.worktrees.clone();
        let worker_runtime = self.runtime.clone();
        let configure_agent = self.configure_agent;
        let preauthorized_isolated = request.policy.isolation == IsolationMode::Worktree;
        self.runtime.spawn(
            request,
            Box::new(move || {
                Ok(Box::new(NexusWorker {
                    provider_factory: factory,
                    parent_workspace: workspace,
                    worktrees,
                    worker_runtime,
                    configure_agent,
                    approval,
                    preauthorized_isolated,
                }) as Box<dyn WorkerExecutor>)
            }),
        )
    }

    pub(crate) fn spawn_group(
        &self,
        factory: ChildProviderFactory,
        requests: Vec<WorkerRequest>,
        approval: Option<Arc<dyn ApprovalPort>>,
    ) -> Result<iris_subagent_runtime::GroupId> {
        let configure_agent = self.configure_agent;
        let jobs = requests
            .into_iter()
            .map(|request| {
                let provider_factory = factory.clone();
                let workspace = self.workspace.clone();
                let worktrees = self.worktrees.clone();
                let worker_runtime = self.runtime.clone();
                let approval = approval.clone();
                let preauthorized_isolated = request.policy.isolation == IsolationMode::Worktree;
                (
                    request,
                    Box::new(move || {
                        Ok(Box::new(NexusWorker {
                            provider_factory,
                            parent_workspace: workspace,
                            worktrees,
                            worker_runtime,
                            configure_agent,
                            approval,
                            preauthorized_isolated,
                        }) as Box<dyn WorkerExecutor>)
                    })
                        as Box<
                            dyn FnOnce() -> std::result::Result<
                                    Box<dyn WorkerExecutor>,
                                    iris_subagent_runtime::RuntimeError,
                                > + Send,
                        >,
                )
            })
            .collect();
        self.runtime.spawn_group(jobs)
    }

    pub(crate) fn runtime(&self) -> &Arc<WorkerRuntime> {
        &self.runtime
    }

    pub(crate) fn poll(
        &self,
        id: &iris_subagent_runtime::WorkerId,
    ) -> Result<iris_subagent_runtime::WorkerSnapshot> {
        Ok(self.runtime.handle().poll(id)?)
    }

    pub(crate) fn read_artifact(&self, id: &iris_subagent_runtime::ArtifactId) -> Result<Vec<u8>> {
        Ok(self.runtime.handle().read_artifact(id)?)
    }

    pub(crate) fn select_worktree_candidate(
        &self,
        id: &iris_subagent_runtime::WorktreeId,
    ) -> Result<iris_subagent_runtime::worktree::WorktreeRecord> {
        Ok(self.worktrees.select_group_candidate(id)?)
    }

    pub(crate) fn list_worktrees(
        &self,
    ) -> Result<Vec<iris_subagent_runtime::worktree::WorktreeRecord>> {
        Ok(self
            .worktrees
            .list(&iris_subagent_runtime::worktree::WorktreeFilter::default())?)
    }

    pub(crate) fn show_worktree(
        &self,
        id: &iris_subagent_runtime::WorktreeId,
    ) -> Result<iris_subagent_runtime::worktree::WorktreeRecord> {
        Ok(self.worktrees.show(id)?)
    }

    pub(crate) fn remove_worktree(
        &self,
        id: &iris_subagent_runtime::WorktreeId,
        force: bool,
    ) -> Result<iris_subagent_runtime::worktree::RemoveOutcome> {
        let options = if force {
            iris_subagent_runtime::worktree::RemoveOptions::force()
        } else {
            iris_subagent_runtime::worktree::RemoveOptions::default()
        };
        Ok(self
            .worktrees
            .remove(id, options, &WorktreeCancellation::default())?)
    }

    pub(crate) fn gc_worktrees(&self) -> Result<iris_subagent_runtime::worktree::GcReport> {
        Ok(self.worktrees.gc(
            iris_subagent_runtime::worktree::RemoveOptions::default(),
            &WorktreeCancellation::default(),
        )?)
    }

    pub(crate) fn adopt_worktree(
        &self,
        id: &iris_subagent_runtime::WorktreeId,
    ) -> Result<iris_subagent_runtime::worktree::WorktreeRecord> {
        Ok(self.worktrees.adopt(id, &WorktreeCancellation::default())?)
    }

    pub(crate) fn ignore_worktree(
        &self,
        id: &iris_subagent_runtime::WorktreeId,
    ) -> Result<iris_subagent_runtime::worktree::WorktreeRecord> {
        Ok(self.worktrees.ignore(id)?)
    }

    pub(crate) fn rebuild_worktree_registry(
        &self,
    ) -> Result<Vec<iris_subagent_runtime::worktree::WorktreeRecord>> {
        Ok(self.worktrees.rebuild(&WorktreeCancellation::default())?)
    }

    pub(crate) fn plan_apply(
        &self,
        worker_id: &iris_subagent_runtime::WorkerId,
    ) -> Result<iris_subagent_runtime::worktree::ApplyPlan> {
        let snapshot = self.poll(worker_id)?;
        let result = snapshot
            .result
            .ok_or_else(|| anyhow::anyhow!("worker has no terminal result"))?;
        if result.status != iris_subagent_runtime::WorkerStatus::Completed {
            anyhow::bail!("only a completed worker can produce an apply plan");
        }
        let worktree = result
            .worktree
            .ok_or_else(|| anyhow::anyhow!("worker has no isolated worktree"))?;
        let manifest = iris_subagent_runtime::worktree::MutationManifest::new(
            result
                .changed_paths
                .into_iter()
                .map(iris_subagent_runtime::worktree::MutationEntry::path)
                .collect(),
        );
        Ok(self
            .worktrees
            .plan_apply(&worktree.id, &manifest, &WorktreeCancellation::default())?)
    }

    pub(crate) fn load_apply_plan(
        &self,
        id: &iris_subagent_runtime::ApplyPlanId,
    ) -> Result<iris_subagent_runtime::worktree::ApplyPlan> {
        Ok(self.worktrees.load_apply_plan(id)?)
    }

    pub(crate) fn apply(
        &self,
        plan: &iris_subagent_runtime::worktree::ApplyPlan,
        options: &iris_subagent_runtime::worktree::ApplyOptions,
    ) -> Result<iris_subagent_runtime::worktree::ApplyResult> {
        Ok(self
            .worktrees
            .apply(plan, options, &WorktreeCancellation::default())?)
    }

    pub(crate) fn poll_group(
        &self,
        id: &iris_subagent_runtime::GroupId,
    ) -> Result<iris_subagent_runtime::GroupSnapshot> {
        Ok(self.runtime.handle().poll_group(id)?)
    }

    pub(crate) fn cancel_group(
        &self,
        id: &iris_subagent_runtime::GroupId,
    ) -> Result<iris_subagent_runtime::GroupSnapshot> {
        Ok(self.runtime.handle().cancel_group(id)?)
    }

    pub(crate) fn cancel(
        &self,
        id: &iris_subagent_runtime::WorkerId,
    ) -> Result<iris_subagent_runtime::WorkerSnapshot> {
        Ok(self.runtime.handle().cancel(id)?)
    }
}

struct NexusWorker {
    provider_factory: ChildProviderFactory,
    parent_workspace: PathBuf,
    worktrees: Arc<WorktreeService>,
    worker_runtime: Arc<WorkerRuntime>,
    configure_agent: ChildAgentConfigurator,
    approval: Option<Arc<dyn ApprovalPort>>,
    preauthorized_isolated: bool,
}

impl WorkerExecutor for NexusWorker {
    fn execute<'a>(&'a mut self, context: WorkerContext) -> LocalExecutorFuture<'a> {
        Box::pin(async move {
            let request = context.request().clone();
            let (workspace, worktree) = self
                .resolve_workspace(&request, &context)
                .await
                .map_err(executor_error)?;
            if context.cancellation().is_cancelled() {
                return Err(ExecutorError::cancelled(
                    "worker cancelled during initialization",
                ));
            }
            let provider = (self.provider_factory)(&request).map_err(executor_error)?;
            let grant = capability_grant(request.policy.capability);
            let mut tools = built_in_tools().into_capability(grant);
            if !request.policy.tool_allowlist.is_empty() {
                tools = tools.into_allowlist(&request.policy.tool_allowlist);
            }
            let max_rounds = request
                .budgets
                .max_tool_rounds
                .and_then(|value| usize::try_from(value).ok());
            let mut agent = Agent::new(provider, tools).with_max_tool_roundtrips(max_rounds);
            (self.configure_agent)(&request, &mut agent);
            let mut harness = Harness::new_configured(
                agent,
                workspace,
                ToolState::new(),
                None,
                None,
                HarnessRuntimeConfig::new(MutationSafetyConfig {
                    enabled: true,
                    native_jj: false,
                })
                .with_worker_runtime(self.worker_runtime.clone()),
            );
            let observer = ChildObserver {
                context: context.clone(),
                usage: RefCell::new(Usage::default()),
            };
            let gate = ChildApprovalGate {
                context: context.clone(),
                approval: self.approval.clone(),
                preauthorized_isolated: self.preauthorized_isolated,
            };
            let token = context.cancellation().token();
            let cancellation = context.cancellation().clone();
            let result = {
                let run = harness.submit_turn(&request.prompt, &observer, &gate, &token);
                tokio::pin!(run);
                tokio::select! {
                    result = &mut run => result,
                    _ = cancellation.cancelled() => {
                        token.cancel();
                        (&mut run).await
                    }
                }
            };
            if context.cancellation().is_cancelled() {
                return Err(ExecutorError::cancelled("worker cancelled"));
            }
            result.map_err(executor_error)?;
            let summary = final_assistant_text(harness.messages());
            let changed_paths = harness.worker_mutation_paths();
            let usage = observer.usage.borrow().clone();
            let mut output = ExecutorOutput::text(summary.clone(), summary.into_bytes());
            output.usage = usage;
            output.changed_paths = changed_paths;
            output.worktree = worktree;
            Ok(output)
        })
    }
}

impl NexusWorker {
    async fn resolve_workspace(
        &self,
        request: &WorkerRequest,
        context: &WorkerContext,
    ) -> Result<(PathBuf, Option<WorkerWorktree>)> {
        match request.policy.isolation {
            IsolationMode::None => {
                let root = self.parent_workspace.canonicalize().with_context(|| {
                    format!(
                        "parent workspace does not exist: {}",
                        self.parent_workspace.display()
                    )
                })?;
                let workspace = match &request.policy.cwd {
                    Some(cwd) => cwd
                        .canonicalize()
                        .with_context(|| format!("worker cwd does not exist: {}", cwd.display()))?,
                    None => root.clone(),
                };
                if !workspace.is_dir() || (workspace != root && !workspace.starts_with(&root)) {
                    anyhow::bail!("worker cwd must stay inside the validated parent workspace");
                }
                Ok((workspace, None))
            }
            IsolationMode::Worktree => {
                let service = self.worktrees.clone();
                let source = self.parent_workspace.clone();
                let session_id = request.session_id.clone();
                let worker_id = context.worker_id().clone();
                let group_id = context.group_id().cloned();
                let parent_worker_id = request.parent_worker_id.clone();
                let cancel = WorktreeCancellation::default();
                let cancel_for_task = cancel.clone();
                let task = tokio::task::spawn_blocking(move || {
                    let mut create = WorktreeCreateRequest::worker(source);
                    create.session_id = session_id;
                    create.worker_id = Some(worker_id);
                    create.group_id = group_id;
                    create.parent_worker_id = parent_worker_id;
                    service.create(create, &cancel_for_task)
                });
                tokio::pin!(task);
                let record = tokio::select! {
                    result = &mut task => result
                        .map_err(|error| anyhow::anyhow!("worktree creation task failed: {error}"))??,
                    _ = context.cancellation().cancelled() => {
                        cancel.cancel();
                        let _ = (&mut task).await;
                        anyhow::bail!("worker cancelled during worktree creation")
                    }
                };
                let metadata = WorkerWorktree::new(
                    record.id,
                    record.path.clone(),
                    record.base_commit,
                    record.creation_mode.as_str(),
                );
                Ok((record.path, Some(metadata)))
            }
            _ => anyhow::bail!("unsupported worker isolation mode"),
        }
    }
}

struct ChildObserver {
    context: WorkerContext,
    usage: RefCell<Usage>,
}

impl AgentObserver for ChildObserver {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        match event {
            AgentEvent::ProviderTurnStarted { .. } => {
                self.context.progress("provider round started");
            }
            AgentEvent::ProviderTurnCompleted {
                usage: provider_usage,
                ..
            } => {
                let mut usage = self.usage.borrow_mut();
                usage.provider_rounds = usage.provider_rounds.saturating_add(1);
                if let Some(provider_usage) = provider_usage {
                    merge_usage(&mut usage, &provider_usage);
                }
                self.context.usage(usage.clone());
            }
            AgentEvent::ToolStarted(call) => {
                let mut usage = self.usage.borrow_mut();
                usage.tool_rounds = usage.tool_rounds.saturating_add(1);
                self.context.progress(format!("running {}", call.name));
                self.context.usage(usage.clone());
            }
            _ => {}
        }
        Ok(())
    }
}

struct ChildApprovalGate {
    context: WorkerContext,
    approval: Option<Arc<dyn ApprovalPort>>,
    preauthorized_isolated: bool,
}

impl ApprovalGate for ChildApprovalGate {
    fn review<'a>(
        &'a self,
        call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        _ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        Box::pin(async move {
            let Some(port) = &self.approval else {
                return Ok(if self.preauthorized_isolated {
                    ApprovalDecision::Allow
                } else {
                    ApprovalDecision::Deny
                });
            };
            self.context.waiting_for_approval(
                call.id.clone(),
                format!("{} requires approval", call.name),
                None,
            );
            let decision = port
                .review(iris_subagent_runtime::ApprovalRequest::new(
                    call.id.clone(),
                    format!("{} requires approval", call.name),
                    None,
                ))
                .await?;
            self.context.progress("approval resolved");
            Ok(match decision {
                RuntimeApprovalDecision::Approve => ApprovalDecision::Allow,
                RuntimeApprovalDecision::Deny => ApprovalDecision::Deny,
                _ => ApprovalDecision::Deny,
            })
        })
    }
}

fn capability_grant(mode: iris_subagent_runtime::CapabilityMode) -> WorkerCapabilityGrant {
    match mode {
        iris_subagent_runtime::CapabilityMode::ReadOnly => WorkerCapabilityGrant::ReadOnly,
        iris_subagent_runtime::CapabilityMode::ReadWrite => WorkerCapabilityGrant::ReadWrite,
        iris_subagent_runtime::CapabilityMode::Execute => WorkerCapabilityGrant::Execute,
        iris_subagent_runtime::CapabilityMode::All => WorkerCapabilityGrant::All,
        _ => WorkerCapabilityGrant::ReadOnly,
    }
}

fn merge_usage(total: &mut Usage, usage: &ProviderUsage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
}

fn final_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| message.content.clone())
        .unwrap_or_default()
}

fn executor_error(error: impl std::fmt::Display) -> ExecutorError {
    ExecutorError::failed(error.to_string())
}

pub(crate) fn resolve_worker_state_dir(session_id: &str) -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("IRIS_SESSION_DIR")
        && !root.is_empty()
    {
        return Ok(PathBuf::from(root).join("workers").join(session_id));
    }
    let home = std::env::var_os("HOME").context("HOME is unset; cannot resolve worker state")?;
    Ok(PathBuf::from(home).join(".iris/workers").join(session_id))
}

pub(crate) fn resolve_worktree_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("IRIS_WORKTREE_DIR")
        && !root.is_empty()
    {
        return Ok(PathBuf::from(root));
    }
    let home = std::env::var_os("HOME").context("HOME is unset; cannot resolve worktree root")?;
    Ok(PathBuf::from(home).join(".iris/worktrees"))
}

pub(crate) async fn run_read_only_provider(
    provider: Box<dyn ChatProvider>,
    workspace: PathBuf,
    prompt: String,
    token: &CancellationToken,
    max_tool_roundtrips: usize,
) -> Result<(String, Option<ProviderUsage>)> {
    let agent = Agent::new(
        provider,
        built_in_tools().into_capability(WorkerCapabilityGrant::ReadOnly),
    )
    .with_max_tool_roundtrips(Some(max_tool_roundtrips));
    let mut harness = Harness::new_configured(
        agent,
        workspace,
        ToolState::new(),
        None,
        None,
        HarnessRuntimeConfig::new(MutationSafetyConfig {
            enabled: true,
            native_jj: false,
        }),
    );
    let observer = SummaryObserver::default();
    let gate = DenyGate;
    harness
        .submit_turn(&prompt, &observer, &gate, token)
        .await?;
    let summary = final_assistant_text(harness.messages());
    if summary.trim().is_empty() {
        anyhow::bail!("subagent returned empty summary");
    }
    Ok((summary, observer.usage.into_inner()))
}

#[derive(Default)]
struct SummaryObserver {
    usage: RefCell<Option<ProviderUsage>>,
}

impl AgentObserver for SummaryObserver {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        if let AgentEvent::ProviderTurnCompleted {
            usage: Some(usage), ..
        } = event
        {
            let mut total = self.usage.borrow_mut();
            match total.as_mut() {
                Some(total) => merge_provider_usage(total, &usage),
                None => *total = Some(usage),
            }
        }
        Ok(())
    }
}

struct DenyGate;

impl ApprovalGate for DenyGate {
    fn review<'a>(
        &'a self,
        _call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        _ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::Deny) })
    }
}

fn merge_provider_usage(total: &mut ProviderUsage, usage: &ProviderUsage) {
    total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
    total.cache_read_input_tokens = total
        .cache_read_input_tokens
        .saturating_add(usage.cache_read_input_tokens);
    total.cache_write_input_tokens = total
        .cache_write_input_tokens
        .saturating_add(usage.cache_write_input_tokens);
    total.reasoning_output_tokens = total
        .reasoning_output_tokens
        .saturating_add(usage.reasoning_output_tokens);
    total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::process::Command;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::nexus::{AssistantTurn, CompletionReason, ProviderEvent, ProviderStream};
    use serde_json::json;

    #[test]
    fn effective_route_serialization_round_trips_and_sets_a_stable_id() {
        let route = ChildRoute::new(
            "anthropic",
            "claude-opus-4-6",
            "https://api.anthropic.com",
            Some("high"),
        );
        let mut request = WorkerRequest::read_only("route");

        attach_route(&mut request, &route).unwrap();

        assert!(
            request
                .route_id
                .as_deref()
                .is_some_and(|id| id.starts_with(IRIS_ROUTE_ID_PREFIX))
        );
        assert_eq!(route_from_request(&request).unwrap(), Some(route));
        assert_eq!(request.profile_id, None);
    }

    #[test]
    fn malformed_claimed_iris_route_fails_closed_while_legacy_requests_inherit() {
        let legacy = WorkerRequest::read_only("legacy");
        assert_eq!(route_from_request(&legacy).unwrap(), None);

        let mut malformed = WorkerRequest::read_only("malformed");
        malformed.route_id = Some(format!("{IRIS_ROUTE_ID_PREFIX}bad"));
        malformed.host.kind = IRIS_ROUTE_PAYLOAD_KIND.to_string();
        malformed.host.value = json!({ "provider": "anthropic" });

        let error = route_from_request(&malformed).unwrap_err().to_string();
        assert!(error.contains("malformed Iris worker route"), "{error}");

        let credentialed = ChildRoute::new(
            "openai-compatible",
            "local",
            "http://secret@localhost:11434/v1",
            None,
        );
        let mut request = WorkerRequest::read_only("secret");
        let error = attach_route(&mut request, &credentialed)
            .unwrap_err()
            .to_string();
        assert!(error.contains("must not contain credentials"), "{error}");
        assert!(request.route_id.is_none());
    }

    struct TextProvider(Rc<()>);

    impl ChatProvider for TextProvider {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            tools: &'a crate::nexus::Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            assert!(tools.by_name("read").is_some());
            assert!(tools.by_name("edit").is_none());
            assert!(tools.by_name("write").is_none());
            assert!(tools.by_name("bash").is_none());
            let _ = &self.0;
            Ok(Box::pin(futures::stream::once(async {
                Ok(ProviderEvent::Completed(AssistantTurn::text(
                    "finished independently",
                )))
            })))
        }
    }

    struct ReadPathProvider {
        path: String,
        expect_secret: bool,
        round: Cell<u8>,
        _not_send: Rc<()>,
    }

    impl ReadPathProvider {
        fn new(path: impl Into<String>, expect_secret: bool) -> Self {
            Self {
                path: path.into(),
                expect_secret,
                round: Cell::new(0),
                _not_send: Rc::new(()),
            }
        }
    }

    impl ChatProvider for ReadPathProvider {
        fn respond_stream<'a>(
            &'a self,
            messages: &'a [Message],
            _tools: &'a crate::nexus::Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            let round = self.round.replace(self.round.get().saturating_add(1));
            let turn = if round == 0 {
                AssistantTurn {
                    text: None,
                    reasoning: Vec::new(),
                    tool_calls: vec![ToolCall {
                        id: "read-path".to_string(),
                        thought_signature: None,
                        name: "read".to_string(),
                        arguments: json!({ "path": self.path }),
                    }],
                    response_id: None,
                    usage: None,
                    completion_reason: Some(CompletionReason::ToolUse),
                }
            } else {
                let result = messages
                    .iter()
                    .rev()
                    .find(|message| message.role == Role::Tool)
                    .expect("read tool result");
                if self.expect_secret {
                    assert!(
                        result.content.contains("outside-secret"),
                        "{}",
                        result.content
                    );
                } else {
                    assert!(
                        result.content.contains("path escapes workspace"),
                        "{}",
                        result.content
                    );
                    assert!(
                        !result.content.contains("outside-secret"),
                        "{}",
                        result.content
                    );
                }
                AssistantTurn::text(if self.expect_secret {
                    "outside read allowed"
                } else {
                    "outside read blocked"
                })
            };
            Ok(Box::pin(futures::stream::once(async move {
                Ok(ProviderEvent::Completed(turn))
            })))
        }
    }

    struct WritePathProvider {
        path: String,
        round: Cell<u8>,
        _not_send: Rc<()>,
    }

    impl WritePathProvider {
        fn new(path: impl Into<String>) -> Self {
            Self {
                path: path.into(),
                round: Cell::new(0),
                _not_send: Rc::new(()),
            }
        }
    }

    impl ChatProvider for WritePathProvider {
        fn respond_stream<'a>(
            &'a self,
            messages: &'a [Message],
            _tools: &'a crate::nexus::Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            let round = self.round.replace(self.round.get().saturating_add(1));
            let turn = if round == 0 {
                AssistantTurn {
                    text: None,
                    reasoning: Vec::new(),
                    tool_calls: vec![ToolCall {
                        id: "write-path".to_string(),
                        thought_signature: None,
                        name: "write".to_string(),
                        arguments: json!({ "path": self.path, "content": "forged\n" }),
                    }],
                    response_id: None,
                    usage: None,
                    completion_reason: Some(CompletionReason::ToolUse),
                }
            } else {
                let result = messages
                    .iter()
                    .rev()
                    .find(|message| message.role == Role::Tool)
                    .expect("write tool result");
                assert!(
                    result.content.contains("path escapes workspace"),
                    "{}",
                    result.content
                );
                AssistantTurn::text("outside write blocked")
            };
            Ok(Box::pin(futures::stream::once(async move {
                Ok(ProviderEvent::Completed(turn))
            })))
        }
    }

    struct BashWriteProvider {
        command: String,
        round: Cell<u8>,
        _not_send: Rc<()>,
    }

    impl BashWriteProvider {
        fn new(command: String) -> Self {
            Self {
                command,
                round: Cell::new(0),
                _not_send: Rc::new(()),
            }
        }
    }

    impl ChatProvider for BashWriteProvider {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a crate::nexus::Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            let round = self.round.replace(self.round.get().saturating_add(1));
            let turn = if round == 0 {
                AssistantTurn {
                    text: None,
                    reasoning: Vec::new(),
                    tool_calls: vec![ToolCall {
                        id: "bash-parent-write".to_string(),
                        thought_signature: None,
                        name: "bash".to_string(),
                        arguments: json!({ "command": self.command }),
                    }],
                    response_id: None,
                    usage: None,
                    completion_reason: Some(CompletionReason::ToolUse),
                }
            } else {
                AssistantTurn::text("outside shell write attempted")
            };
            Ok(Box::pin(futures::stream::once(async move {
                Ok(ProviderEvent::Completed(turn))
            })))
        }
    }

    struct HangingProvider(Rc<()>);

    impl ChatProvider for HangingProvider {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a crate::nexus::Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            let _ = &self.0;
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    struct ScriptProvider {
        turns: RefCell<VecDeque<AssistantTurn>>,
        _not_send: Rc<()>,
    }

    impl ScriptProvider {
        fn write(path: impl Into<String>, content: impl Into<String>, summary: String) -> Self {
            Self {
                turns: RefCell::new(VecDeque::from([
                    AssistantTurn {
                        text: None,
                        reasoning: Vec::new(),
                        tool_calls: vec![ToolCall {
                            id: "write-child".to_string(),
                            thought_signature: None,
                            name: "write".to_string(),
                            arguments: json!({
                                "path": path.into(),
                                "content": content.into(),
                            }),
                        }],
                        response_id: None,
                        usage: None,
                        completion_reason: Some(CompletionReason::ToolUse),
                    },
                    AssistantTurn::text(&summary),
                ])),
                _not_send: Rc::new(()),
            }
        }
    }

    impl ChatProvider for ScriptProvider {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            tools: &'a crate::nexus::Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            assert!(tools.by_name("write").is_some());
            let turn = self
                .turns
                .borrow_mut()
                .pop_front()
                .expect("provider script exhausted");
            Ok(Box::pin(futures::stream::once(async move {
                Ok(ProviderEvent::Completed(turn))
            })))
        }
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn repo(root: &Path) -> PathBuf {
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        git(&workspace, &["init", "-q"]);
        git(
            &workspace,
            &["config", "user.email", "worker@example.invalid"],
        );
        git(&workspace, &["config", "user.name", "Worker Test"]);
        std::fs::write(workspace.join("candidate.txt"), "parent\n").unwrap();
        git(&workspace, &["add", "candidate.txt"]);
        git(&workspace, &["commit", "-qm", "base"]);
        workspace
    }

    fn mutable_request(prompt: &str) -> WorkerRequest {
        let mut request = WorkerRequest::read_only(prompt);
        request.policy.capability = iris_subagent_runtime::CapabilityMode::ReadWrite;
        request.policy.isolation = IsolationMode::Worktree;
        request.session_id = Some("test-session".to_string());
        request
    }

    fn read_path_factory(path: String, expect_secret: bool) -> ChildProviderFactory {
        Arc::new(move |_| {
            Ok(Box::new(ReadPathProvider::new(path.clone(), expect_secret))
                as Box<dyn ChatProvider>)
        })
    }

    fn write_path_factory(path: String) -> ChildProviderFactory {
        Arc::new(move |_| {
            Ok(Box::new(WritePathProvider::new(path.clone())) as Box<dyn ChatProvider>)
        })
    }

    fn bash_write_factory(command: String) -> ChildProviderFactory {
        Arc::new(move |_| {
            Ok(Box::new(BashWriteProvider::new(command.clone())) as Box<dyn ChatProvider>)
        })
    }

    fn wait_worker(
        backend: &SubagentBackend,
        id: &iris_subagent_runtime::WorkerId,
    ) -> iris_subagent_runtime::WorkerResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(backend.runtime().handle().wait(id))
            .unwrap()
    }

    fn wait_group(
        backend: &SubagentBackend,
        id: &iris_subagent_runtime::GroupId,
    ) -> iris_subagent_runtime::GroupResult {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(backend.runtime().handle().wait_group(id))
            .unwrap()
    }

    #[test]
    fn nexus_worker_runs_independently_on_backend_scheduler() {
        let root = std::env::temp_dir().join(format!(
            "iris-wayland-worker-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let backend =
            SubagentBackend::open(workspace, &root.join("state"), root.join("worktrees")).unwrap();
        let factory: ChildProviderFactory = Arc::new(|_| Ok(Box::new(TextProvider(Rc::new(())))));
        let id = backend
            .spawn(factory, WorkerRequest::read_only("run"), None)
            .unwrap();

        std::thread::sleep(Duration::from_millis(100));
        let snapshot = backend.runtime.handle().poll(&id).unwrap();

        assert_eq!(
            snapshot.status,
            iris_subagent_runtime::WorkerStatus::Completed
        );
        assert_eq!(snapshot.result.unwrap().summary, "finished independently");
        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queued_worker_keeps_its_accepted_route_after_parent_switching() {
        let root = std::env::temp_dir().join(format!(
            "iris-wayland-worker-route-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let backend =
            SubagentBackend::open(workspace, &root.join("state"), root.join("worktrees")).unwrap();

        let mut blockers = Vec::new();
        for _ in 0..4 {
            blockers.push(
                backend
                    .spawn(
                        Arc::new(|_| Ok(Box::new(HangingProvider(Rc::new(()))))),
                        WorkerRequest::read_only("block"),
                        None,
                    )
                    .unwrap(),
            );
        }
        for _ in 0..200 {
            if blockers.iter().all(|id| {
                backend.poll(id).unwrap().status == iris_subagent_runtime::WorkerStatus::Running
            }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(blockers.iter().all(|id| {
            backend.poll(id).unwrap().status == iris_subagent_runtime::WorkerStatus::Running
        }));

        let accepted = ChildRoute::new(
            "anthropic",
            "claude-opus-4-6",
            "https://api.anthropic.com",
            Some("low"),
        );
        let switched = ChildRoute::new(
            "openai-codex",
            "gpt-5.6-sol",
            "https://chatgpt.com/backend-api",
            Some("high"),
        );
        let parent = Arc::new(std::sync::Mutex::new(accepted.clone()));
        let observed = Arc::new(std::sync::Mutex::new(None));
        let factory_parent = parent.clone();
        let factory_observed = observed.clone();
        let factory: ChildProviderFactory = Arc::new(move |request| {
            let fallback = factory_parent
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone();
            let effective = route_from_request(request)?.unwrap_or(fallback);
            *factory_observed
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(effective);
            Ok(Box::new(TextProvider(Rc::new(()))))
        });
        let mut request = WorkerRequest::read_only("routed");
        attach_route(&mut request, &accepted).unwrap();
        let routed = backend.spawn(factory, request, None).unwrap();
        assert_eq!(
            backend.poll(&routed).unwrap().status,
            iris_subagent_runtime::WorkerStatus::Queued
        );

        *parent.lock().unwrap_or_else(|poison| poison.into_inner()) = switched;
        for id in &blockers {
            backend.cancel(id).unwrap();
        }
        let result = wait_worker(&backend, &routed);
        assert_eq!(
            result.status,
            iris_subagent_runtime::WorkerStatus::Completed
        );
        assert_eq!(
            observed
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_ref(),
            Some(&accepted)
        );
        let snapshot = backend.poll(&routed).unwrap();
        assert_eq!(
            route_from_request(&snapshot.request).unwrap(),
            Some(accepted)
        );

        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_request_without_route_uses_the_factory_live_parent_value() {
        let root = std::env::temp_dir().join(format!(
            "iris-wayland-worker-legacy-route-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let backend =
            SubagentBackend::open(workspace, &root.join("state"), root.join("worktrees")).unwrap();
        let live = ChildRoute::new(
            "openai-codex",
            "gpt-5.6-sol",
            "https://chatgpt.com/backend-api",
            Some("high"),
        );
        let parent = Arc::new(std::sync::Mutex::new(live.clone()));
        let observed = Arc::new(std::sync::Mutex::new(None));
        let factory_observed = observed.clone();
        let factory: ChildProviderFactory = Arc::new(move |request| {
            let fallback = parent
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone();
            let effective = route_from_request(request)?.unwrap_or(fallback);
            *factory_observed
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(effective);
            Ok(Box::new(TextProvider(Rc::new(()))))
        });

        let id = backend
            .spawn(factory, WorkerRequest::read_only("legacy"), None)
            .unwrap();
        assert_eq!(
            wait_worker(&backend, &id).status,
            iris_subagent_runtime::WorkerStatus::Completed
        );
        assert_eq!(
            observed
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_ref(),
            Some(&live)
        );
        assert_eq!(backend.poll(&id).unwrap().request.route_id, None);

        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn best_of_n_workers_share_one_auditable_route_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "iris-wayland-worker-group-route-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let backend =
            SubagentBackend::open(workspace, &root.join("state"), root.join("worktrees")).unwrap();
        let route = ChildRoute::new(
            "anthropic",
            "claude-opus-4-6",
            "https://api.anthropic.com",
            Some("xhigh"),
        );
        let mut request = WorkerRequest::read_only("candidate");
        attach_route(&mut request, &route).unwrap();
        let expected_id = request.route_id.clone();
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let factory_observed = observed.clone();
        let factory: ChildProviderFactory = Arc::new(move |request| {
            factory_observed
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(route_from_request(request)?.expect("accepted route"));
            Ok(Box::new(TextProvider(Rc::new(()))))
        });

        let group_id = backend
            .spawn_group(factory, vec![request; 3], None)
            .unwrap();
        let result = wait_group(&backend, &group_id);
        assert!(
            result
                .results
                .iter()
                .all(|result| { result.status == iris_subagent_runtime::WorkerStatus::Completed })
        );
        let group = backend.poll_group(&group_id).unwrap();
        assert!(group.snapshots.iter().all(|snapshot| {
            snapshot.request.route_id == expected_id
                && snapshot.request.profile_id.is_none()
                && route_from_request(&snapshot.request).unwrap() == Some(route.clone())
        }));
        assert_eq!(
            *observed.lock().unwrap_or_else(|poison| poison.into_inner()),
            vec![route; 3]
        );

        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn child_workspace_confinement_is_default_and_outside_access_is_explicit() {
        let root = std::env::temp_dir().join(format!(
            "iris-wayland-worker-paths-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = repo(&root);
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, "outside-secret\n").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("escape")).unwrap();
        let backend = SubagentBackend::open(
            workspace.clone(),
            &root.join("state"),
            root.join("worktrees"),
        )
        .unwrap();

        for path in [
            secret.to_string_lossy().into_owned(),
            "escape/secret.txt".to_string(),
        ] {
            let id = backend
                .spawn(
                    read_path_factory(path, false),
                    WorkerRequest::read_only("try outside read"),
                    None,
                )
                .unwrap();
            let result = wait_worker(&backend, &id);
            assert_eq!(
                result.status,
                iris_subagent_runtime::WorkerStatus::Completed
            );
            assert_eq!(result.summary, "outside read blocked");
        }

        let mut unrestricted = WorkerRequest::read_only("explicit outside read");
        unrestricted.policy.allow_outside_workspace = true;
        let id = backend
            .spawn(
                read_path_factory("escape/secret.txt".to_string(), true),
                unrestricted,
                None,
            )
            .unwrap();
        let result = wait_worker(&backend, &id);
        assert_eq!(
            result.status,
            iris_subagent_runtime::WorkerStatus::Completed
        );
        assert_eq!(result.summary, "outside read allowed");

        let mut mutable = mutable_request("attempt outside write");
        mutable.policy.allow_outside_workspace = true;
        let id = backend
            .spawn(
                write_path_factory(secret.to_string_lossy().into_owned()),
                mutable,
                None,
            )
            .unwrap();
        let result = wait_worker(&backend, &id);
        assert_eq!(
            result.status,
            iris_subagent_runtime::WorkerStatus::Completed
        );
        assert_eq!(result.summary, "outside write blocked");
        assert_eq!(
            std::fs::read_to_string(&secret).unwrap(),
            "outside-secret\n"
        );

        let mut shell_worker = mutable_request("attempt parent write through shell");
        shell_worker.policy.capability = iris_subagent_runtime::CapabilityMode::All;
        let command = format!(
            "printf forged > {}",
            workspace.join("candidate.txt").display()
        );
        let id = backend
            .spawn(bash_write_factory(command), shell_worker, None)
            .unwrap();
        let result = wait_worker(&backend, &id);
        assert_eq!(
            result.status,
            iris_subagent_runtime::WorkerStatus::Completed
        );
        assert_eq!(result.summary, "outside shell write attempted");
        assert_eq!(
            std::fs::read_to_string(workspace.join("candidate.txt")).unwrap(),
            "parent\n"
        );

        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutable_worker_cancellation_is_terminal_and_leaves_parent_unchanged() {
        let root = std::env::temp_dir().join(format!(
            "iris-wayland-cancel-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = repo(&root);
        let backend = SubagentBackend::open(
            workspace.clone(),
            &root.join("state"),
            root.join("worktrees"),
        )
        .unwrap();
        let factory: ChildProviderFactory =
            Arc::new(|_| Ok(Box::new(HangingProvider(Rc::new(())))));
        let id = backend
            .spawn(factory, mutable_request("wait"), None)
            .unwrap();
        for _ in 0..100 {
            let status = backend.poll(&id).unwrap().status;
            if matches!(
                status,
                iris_subagent_runtime::WorkerStatus::Running
                    | iris_subagent_runtime::WorkerStatus::Initializing
            ) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        backend.cancel(&id).unwrap();
        let first = wait_worker(&backend, &id);
        let second = wait_worker(&backend, &id);
        assert_eq!(first.status, iris_subagent_runtime::WorkerStatus::Cancelled);
        assert_eq!(second, first);
        assert_eq!(
            std::fs::read_to_string(workspace.join("candidate.txt")).unwrap(),
            "parent\n"
        );

        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutable_worker_edits_only_its_worktree_until_reviewed_apply() {
        let root = std::env::temp_dir().join(format!(
            "iris-wayland-mutable-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = repo(&root);
        let backend = SubagentBackend::open(
            workspace.clone(),
            &root.join("state"),
            root.join("worktrees"),
        )
        .unwrap();
        let factory: ChildProviderFactory = Arc::new(|_| {
            Ok(Box::new(ScriptProvider::write(
                "result.txt",
                "child\n",
                "created result".to_string(),
            )))
        });
        let id = backend
            .spawn(factory, mutable_request("create result"), None)
            .unwrap();
        let result = wait_worker(&backend, &id);

        assert_eq!(
            result.status,
            iris_subagent_runtime::WorkerStatus::Completed
        );
        assert_eq!(result.summary, "created result");
        assert_eq!(
            std::fs::read_to_string(workspace.join("candidate.txt")).unwrap(),
            "parent\n"
        );
        assert!(!workspace.join("result.txt").exists());
        let worktree = result.worktree.as_ref().expect("worktree metadata");
        assert_eq!(
            std::fs::read_to_string(worktree.path.join("result.txt")).unwrap(),
            "child\n"
        );
        assert_eq!(result.changed_paths, vec![PathBuf::from("result.txt")]);
        let record: iris_subagent_runtime::worktree::WorktreeRecord = serde_json::from_slice(
            &std::fs::read(
                root.join("worktrees")
                    .join("control")
                    .join(format!("{}.json", worktree.id)),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(record.worker_id.as_ref(), Some(&id));
        assert_eq!(record.session_id.as_deref(), Some("test-session"));

        let plan = backend.plan_apply(&id).unwrap();
        assert_eq!(plan.operations.len(), 1);
        let applied = backend
            .apply(&plan, &iris_subagent_runtime::worktree::ApplyOptions::new())
            .unwrap();
        assert_eq!(
            applied.disposition,
            iris_subagent_runtime::worktree::ApplyDisposition::Complete
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("result.txt")).unwrap(),
            "child\n"
        );

        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn best_of_n_keeps_candidates_isolated_and_applies_only_selected_worker() {
        let root = std::env::temp_dir().join(format!(
            "iris-wayland-group-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = repo(&root);
        let backend = SubagentBackend::open(
            workspace.clone(),
            &root.join("state"),
            root.join("worktrees"),
        )
        .unwrap();
        let sequence = Arc::new(AtomicUsize::new(0));
        let factory: ChildProviderFactory = Arc::new(move |_| {
            let index = sequence.fetch_add(1, Ordering::SeqCst);
            if index == 2 {
                anyhow::bail!("intentional candidate failure");
            }
            let content = format!("candidate-{index}\n");
            Ok(Box::new(ScriptProvider::write(
                "result.txt",
                content.clone(),
                format!("produced {content}"),
            )) as Box<dyn ChatProvider>)
        });
        let group_id = backend
            .spawn_group(factory, vec![mutable_request("candidate"); 3], None)
            .unwrap();
        let group = wait_group(&backend, &group_id);

        assert_eq!(group.results.len(), 3);
        let successful = group
            .results
            .iter()
            .filter(|result| result.status == iris_subagent_runtime::WorkerStatus::Completed)
            .collect::<Vec<_>>();
        assert_eq!(successful.len(), 2);
        assert_eq!(
            group
                .results
                .iter()
                .filter(|result| result.status == iris_subagent_runtime::WorkerStatus::Failed)
                .count(),
            1
        );
        let paths = successful
            .iter()
            .map(|result| result.worktree.as_ref().unwrap().path.clone())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(paths.len(), 2);
        assert!(!workspace.join("result.txt").exists());

        let selected = successful[1];
        let selected_content =
            std::fs::read_to_string(selected.worktree.as_ref().unwrap().path.join("result.txt"))
                .unwrap();
        let selected_record: iris_subagent_runtime::worktree::WorktreeRecord =
            serde_json::from_slice(
                &std::fs::read(
                    root.join("worktrees")
                        .join("control")
                        .join(format!("{}.json", selected.worktree.as_ref().unwrap().id)),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            selected_record.worker_id.as_ref(),
            Some(&selected.worker_id)
        );
        assert_eq!(selected_record.group_id.as_ref(), Some(&group_id));
        let selected_record = backend
            .select_worktree_candidate(&selected.worktree.as_ref().unwrap().id)
            .unwrap();
        assert!(selected_record.selected);
        assert!(backend.plan_apply(&successful[0].worker_id).is_err());
        let plan = backend.plan_apply(&selected.worker_id).unwrap();
        backend
            .apply(&plan, &iris_subagent_runtime::worktree::ApplyOptions::new())
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.join("result.txt")).unwrap(),
            selected_content
        );
        let non_winner = successful[0];
        assert!(non_winner.worktree.as_ref().unwrap().path.exists());
        backend
            .remove_worktree(&selected.worktree.as_ref().unwrap().id, true)
            .unwrap();
        assert!(
            backend
                .select_worktree_candidate(&non_winner.worktree.as_ref().unwrap().id)
                .is_err()
        );

        drop(backend);
        std::fs::remove_dir_all(root).unwrap();
    }
}
