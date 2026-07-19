//! Built-in tool adapters (Tier 3).
//!
//! Each struct is a thin [`Tool`] impl over the per-tool `execute`/`parameters`
//! functions plus the self-classification (`requires_approval`,
//! `is_destructive`, `is_concurrency_safe`, `diff_preview`) the core loop used to
//! compute by tool name. [`built_in_tools`] is the injection point: the CLI
//! constructs the set and passes it into the agent, so Nexus instantiates no
//! tool itself.
//!
//! The pure read-only tools (`grep`/`find`/`ls`) touch no [`ToolState`], so
//! `execute` runs their blocking body on `tokio::task::spawn_blocking` and
//! awaits the handle: they are `is_concurrency_safe` and a parallel batch runs
//! them genuinely concurrently on the blocking pool, while awaiting the handle
//! lets the loop's cancellation race abandon a cancelled call. `read` mutates
//! `state.observed` (read-before-write tracking) through the env's `!Send`
//! `RefCell`, so it cannot move off-thread and stays exclusive. Mutating file
//! tools (`edit`/`write`) wrap their synchronous body in a ready future and run
//! exclusively; each borrows the shared `ToolState` only for its synchronous
//! duration, never across an `.await`. `bash` also runs exclusively, but its
//! long, blocking body (poll loop + pump threads) would starve the executor if
//! run inline, so it is offloaded to `tokio::task::spawn_blocking`: its registry
//! is shared via `Arc<Mutex<_>>` (see [`ToolState`]) and its live-output sink is
//! bridged over a channel so `ToolOutputDelta` events stream while the command
//! runs and the UI loop keeps polling.

use std::cell::RefMut;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::nexus::{Tool, ToolCapability, ToolEnv, ToolFuture, ToolOutput, Tools};

use super::{
    Preview, ToolState, ask_user_question, bash, edit, find, goal, grep, ls, path, read,
    read_output, read_web_page, recall, render_preview, request_compaction, web, web_search, write,
};
use web::WebToolsConfig;

/// Construct the workspace tools the CLI injects into the agent. The order is
/// the provider-declaration order (`read, bash, edit, write, grep, find, ls`),
/// followed by `AskUserQuestion` and the Iris-specific session tools.
pub(crate) fn built_in_tools() -> Tools {
    built_in_tools_for(false, false)
}

/// Resolved tool-surface configuration built once from `Settings` (+ the auth
/// store for web-tool keys). Replaces the growing positional-bool signature of
/// [`built_in_tools_for`]: new opt-in tools add a field here instead of another
/// parameter at every call site.
#[derive(Debug, Clone, Default)]
pub(crate) struct ToolsConfig {
    pub(crate) bash_tool_mode: bool,
    pub(crate) model_compaction_tool: bool,
    /// Resolved web-tool backends + keys. Default = both tools off.
    pub(crate) web: WebToolsConfig,
    /// Shared backend/model factory for first-class delegated workers.
    pub(crate) subagents: Option<SubagentToolsConfig>,
}

#[derive(Clone)]
pub(crate) struct SubagentToolsConfig {
    pub(crate) backend: Arc<crate::wayland::subagents::SubagentBackend>,
    pub(crate) provider_factory: crate::wayland::subagents::ChildProviderFactory,
    pub(crate) selection: Arc<std::sync::Mutex<crate::mimir::selection::ModelSelection>>,
    /// Authenticated model catalog captured at session start. Drives the
    /// `spawn_subagent` model/provider `enum`s and the pre-spawn resolution so
    /// schema and execution share one source of truth and stay deterministic in
    /// tests. A mid-session `/login` is not reflected until the next start.
    pub(crate) catalog: Vec<crate::mimir::model_catalog::SubagentCatalogEntry>,
    pub(crate) manifests: Arc<[crate::wayland::subagents::SubagentTypeManifest]>,
    pub(crate) capability_ceiling: iris_subagent_runtime::CapabilityMode,
    pub(crate) approved_api_vendors: Arc<
        std::sync::Mutex<std::collections::BTreeSet<crate::mimir::model_catalog::ProviderVendor>>,
    >,
    pub(crate) session_id: String,
    pub(crate) nesting_depth: u32,
    pub(crate) max_nesting_depth: u32,
    pub(crate) approval: Option<Arc<dyn iris_subagent_runtime::ApprovalPort>>,
}

impl std::fmt::Debug for SubagentToolsConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubagentToolsConfig")
            .finish_non_exhaustive()
    }
}

/// Construct the tool set for the configured bash-tool-mode setting
/// (`bashToolMode`). Off (`false`) is the full built-in surface. On (`true`)
/// deactivates the workspace filesystem tools whose job the shell can do
/// (`read`, `write`, `grep`, `find`, `ls`), so the model drives those
/// operations through `bash`; `edit` stays registered because exact-string
/// edits are too delicate to route through ad-hoc shell rewrites. The
/// session-plumbing tools `read_output` and `recall` also stay: they page
/// offloaded oversized outputs (which `bash` results can still produce) and
/// compacted transcript turns back in -- neither is reachable via the shell.
/// The system prompt's generated tool blocks adapt automatically (the
/// guidelines fall back to the bash-only file-operations bullet).
pub(crate) fn built_in_tools_for(bash_tool_mode: bool, model_compaction_tool: bool) -> Tools {
    built_in_tools_with(&ToolsConfig {
        bash_tool_mode,
        model_compaction_tool,
        web: WebToolsConfig::default(),
        subagents: None,
    })
}

/// Construct the tool set from a resolved [`ToolsConfig`]. Web tools are pushed
/// only when their backend is configured (not off), so a disabled tool is
/// invisible to the model (no prompt bloat).
pub(crate) fn built_in_tools_with(config: &ToolsConfig) -> Tools {
    let bash_tool_mode = config.bash_tool_mode;
    let model_compaction_tool = config.model_compaction_tool;
    let mut tools: Vec<Box<dyn Tool>> = if bash_tool_mode {
        vec![
            Box::new(BashTool),
            Box::new(EditTool),
            Box::new(AskUserQuestionTool),
            Box::new(GetGoalTool),
            Box::new(CreateGoalTool),
            Box::new(UpdateGoalTool),
            Box::new(ReadOutputTool),
            Box::new(RecallTool),
        ]
    } else {
        vec![
            Box::new(ReadTool),
            Box::new(BashTool),
            Box::new(EditTool),
            Box::new(WriteTool),
            Box::new(GrepTool),
            Box::new(FindTool),
            Box::new(LsTool),
            Box::new(AskUserQuestionTool),
            Box::new(GetGoalTool),
            Box::new(CreateGoalTool),
            Box::new(UpdateGoalTool),
            Box::new(ReadOutputTool),
            Box::new(RecallTool),
        ]
    };
    if model_compaction_tool {
        tools.push(Box::new(RequestCompactionTool));
    }
    // Web tools: registered only when a backend is selected (plan §2: off = not
    // registered at all). They stay available in bash-tool-mode too -- the
    // shell cannot reach the network under the SSRF-gated pinned client.
    if let Some(backend) = config.web.web_search {
        tools.push(Box::new(web_search::WebSearchTool::new(
            config.web.clone(),
            backend,
        )));
    }
    if let Some(backend) = config.web.read_web_page {
        tools.push(Box::new(read_web_page::ReadWebPageTool::new(
            config.web.clone(),
            backend,
        )));
    }
    if let Some(config) = &config.subagents {
        tools.push(Box::new(SpawnSubagentTool::new(config.clone())));
        tools.push(Box::new(SubagentStatusTool(config.backend.clone())));
        tools.push(Box::new(SubagentArtifactTool(config.backend.clone())));
        tools.push(Box::new(CancelSubagentTool(config.backend.clone())));
        tools.push(Box::new(PlanSubagentApplyTool(config.backend.clone())));
        tools.push(Box::new(ApplySubagentTool(config.backend.clone())));
    }
    Tools::new(tools)
}

/// Boxed `read_output` tool for integration tests that pair it with a custom
/// tool (e.g. one that emits an oversized output) in a single [`Tools`] set.
#[cfg(test)]
pub(crate) fn read_output_tool() -> Box<dyn Tool> {
    Box::new(ReadOutputTool)
}

/// Resolve the canonicalized workspace root for an execution. Centralized here
/// (it was the first line of the old `dispatch`) so every tool enforces the
/// same path boundary.
fn root(env: &ToolEnv) -> Result<PathBuf> {
    path::workspace_root(env.workspace)
}

/// Borrow the shared tool state mutably for a synchronous tool body. Uses
/// `try_borrow_mut` so a (theoretical) overlapping borrow becomes a tool error
/// rather than a panic; tool bodies never hold this across an `.await`, so it
/// never actually contends.
fn state_mut<'e>(env: &'e ToolEnv<'_>) -> Result<RefMut<'e, ToolState>> {
    env.state
        .try_borrow_mut()
        .map_err(|_| anyhow!("tool state is busy; concurrent mutation is not allowed"))
}

/// The benchmark arm's output-reduction flag for this run (issue #210). Read
/// from the shared [`ToolState`] so `grep`/`find`/`bash` render the shipped
/// (arm A) or the pre-reduction baseline (arm B) form. Defaults to enabled if
/// the state is momentarily borrowed (never happens on the read-only path).
fn reduce_output(env: &ToolEnv) -> bool {
    env.state
        .try_borrow()
        .map(|state| state.reduce_output)
        .unwrap_or(true)
}

fn workspace_restrictions(env: &ToolEnv<'_>) -> Option<bool> {
    env.state
        .try_borrow()
        .ok()
        .and_then(|state| state.workspace_restrictions)
}

fn read_workspace_restrictions(env: &ToolEnv<'_>) -> Option<bool> {
    env.state
        .try_borrow()
        .ok()
        .and_then(|state| state.read_workspace_restrictions)
}

/// Run a pure read-only tool body (`grep`/`find`/`ls`) on the blocking pool.
/// The body touches no [`ToolState`], so the resolved root and owned args move
/// into a `spawn_blocking` task: a parallel batch then runs genuinely
/// concurrently, and awaiting the join handle makes the future yield so the
/// loop's cancellation race can abandon a cancelled call (the orphaned walk
/// finishes on the pool and its result is discarded -- `spawn_blocking` cannot
/// be force-aborted).
fn run_off_thread(
    root: Result<PathBuf>,
    args: Value,
    label: &'static str,
    reduce: bool,
    restrictions: Option<bool>,
    body: fn(&Path, &Value, bool) -> Result<ToolOutput>,
) -> ToolFuture<'static> {
    Box::pin(async move {
        let root = root?;
        match tokio::task::spawn_blocking(move || {
            path::with_restrictions(restrictions, || body(&root, &args, reduce))
        })
        .await
        {
            Ok(result) => result,
            Err(_join_err) => Err(anyhow!("{} tool task failed: {}", label, _join_err)),
        }
    })
}

/// Render a mutating tool's preview, resolving the root from the raw workspace
/// exactly as the old `diff_preview` free function did.
fn render(workspace: &Path, preview: impl FnOnce(&Path) -> Preview) -> Option<String> {
    let root = match path::workspace_root(workspace) {
        Ok(root) => root,
        Err(error) => return Some(format!("diff unavailable: {error:#}")),
    };
    render_preview(preview(&root))
}

struct SpawnSubagentTool {
    config: SubagentToolsConfig,
    description: String,
}

impl SpawnSubagentTool {
    fn new(config: SubagentToolsConfig) -> Self {
        let triggers = config
            .manifests
            .iter()
            .map(|manifest| format!("{}: {}", manifest.id, manifest.when_to_use))
            .collect::<Vec<_>>()
            .join(" ");
        Self {
            config,
            description: format!(
                "Start one delegated worker; mutations stay isolated until separately applied. Subagent types: {triggers}"
            ),
        }
    }
}

struct ResolvedWorkerTools {
    names: Vec<String>,
    mutation_capable: bool,
}

fn delegated_capability(capability: ToolCapability) -> bool {
    matches!(
        capability,
        ToolCapability::Read | ToolCapability::Write | ToolCapability::Execute
    )
}

fn grant_allows(grant: iris_subagent_runtime::CapabilityMode, capability: ToolCapability) -> bool {
    match grant {
        iris_subagent_runtime::CapabilityMode::ReadOnly => capability == ToolCapability::Read,
        iris_subagent_runtime::CapabilityMode::ReadWrite => {
            matches!(capability, ToolCapability::Read | ToolCapability::Write)
        }
        iris_subagent_runtime::CapabilityMode::Execute => {
            matches!(capability, ToolCapability::Read | ToolCapability::Execute)
        }
        iris_subagent_runtime::CapabilityMode::All => delegated_capability(capability),
        _ => false,
    }
}

fn shorthand_allows(token: &str, capability: ToolCapability) -> Option<bool> {
    match token {
        "read_only" => Some(capability == ToolCapability::Read),
        "read_write" => Some(matches!(
            capability,
            ToolCapability::Read | ToolCapability::Write
        )),
        "shell" => Some(matches!(
            capability,
            ToolCapability::Read | ToolCapability::Execute
        )),
        "all" => Some(delegated_capability(capability)),
        _ => None,
    }
}

fn delegated_tool_tokens() -> Vec<String> {
    let mut values = vec![
        "read_only".to_string(),
        "read_write".to_string(),
        "shell".to_string(),
        "all".to_string(),
    ];
    values.extend(
        built_in_tools()
            .iter()
            .filter(|tool| delegated_capability(tool.capability()))
            .map(|tool| tool.name().to_string()),
    );
    values
}

fn resolve_worker_tools(
    tokens: &[String],
    ceiling: iris_subagent_runtime::CapabilityMode,
) -> Result<ResolvedWorkerTools> {
    let tools = built_in_tools();
    let mut requested = std::collections::BTreeSet::new();
    for token in tokens {
        if shorthand_allows(token, ToolCapability::Read).is_some() {
            for tool in tools
                .iter()
                .filter(|tool| shorthand_allows(token, tool.capability()).unwrap_or(false))
            {
                requested.insert(tool.name().to_string());
            }
            continue;
        }
        let tool = tools
            .by_name(token)
            .ok_or_else(|| anyhow!("unknown delegated tool id: {token}"))?;
        if !delegated_capability(tool.capability()) {
            return Err(anyhow!("tool '{token}' is not delegatable"));
        }
        requested.insert(token.clone());
    }
    let mut names = Vec::new();
    let mut mutation_capable = false;
    for tool in tools
        .iter()
        .filter(|tool| requested.contains(tool.name()) && grant_allows(ceiling, tool.capability()))
    {
        names.push(tool.name().to_string());
        mutation_capable |= tool.is_mutating();
    }
    Ok(ResolvedWorkerTools {
        names,
        mutation_capable,
    })
}

fn requested_tool_tokens(
    args: &Value,
    manifest: &crate::wayland::subagents::SubagentTypeManifest,
) -> Result<Vec<String>> {
    let Some(value) = args.get("tools") else {
        return Ok(manifest.tool_profile.clone());
    };
    let values = value
        .as_array()
        .ok_or_else(|| anyhow!("spawn_subagent tools must be an array"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("spawn_subagent tools entries must be strings"))
        })
        .collect()
}

impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        "spawn_subagent"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        let choices =
            crate::mimir::model_catalog::subagent_schema_choices_from(&self.config.catalog);
        let manifest_ids = self
            .config
            .manifests
            .iter()
            .map(|manifest| manifest.id.clone())
            .collect::<Vec<_>>();
        let mut properties = serde_json::json!({
            "task": {
                "type": "string",
                "minLength": 1,
                "description": "Self-contained work order for a fresh worker."
            },
            "subagent_type": {
                "type": "string",
                "enum": manifest_ids,
                "default": "general",
                "description": "Manifest id; defaults to general."
            },
            "model": {
                "type": "string",
                "enum": choices.models,
                "description": "Active-credential worker model; omit to use manifest fallbacks."
            },
            "effort": {
                "type": "string",
                "enum": ["off", "minimal", "low", "medium", "high", "xhigh", "max"],
                "description": "Reasoning effort; omit to inherit."
            },
            "tools": {
                "type": "array",
                "items": {
                    "type": "string",
                    "enum": delegated_tool_tokens()
                },
                "uniqueItems": true,
                "description": "Tool ids or grant shorthands; replaces the manifest profile and is clamped to the parent ceiling."
            },
            "system_prompt": {
                "type": "string",
                "description": "Worker system-prompt override; defaults to the manifest prompt."
            },
            "description": {
                "type": "string",
                "description": "Short label; defaults to subagent_type."
            },
            "background": {
                "type": "boolean",
                "default": true,
                "description": "Return immediately when true; wait for completion when false."
            },
            "isolation": {
                "type": "string",
                "enum": ["none", "worktree"],
                "description": "Execution isolation; defaults to worktree when resolved tools can mutate."
            },
            "cwd": {
                "type": "string",
                "description": "Existing in-workspace directory for non-isolated work."
            }
        })
        .as_object()
        .expect("spawn properties are an object")
        .clone();
        if let Some(providers) = choices.providers {
            properties.insert(
                "provider".to_string(),
                serde_json::json!({
                    "type": "string",
                    "enum": providers,
                    "description": "Credential lane; use only when multiple active lanes share a vendor."
                }),
            );
        }
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": ["task"],
            "additionalProperties": false
        })
    }

    fn capability(&self) -> ToolCapability {
        ToolCapability::Execute
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _env: &'a ToolEnv<'_>,
        cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let task = args
                .get("task")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow!("spawn_subagent requires a non-empty task"))?;
            let subagent_type = args
                .get("subagent_type")
                .and_then(Value::as_str)
                .unwrap_or("general");
            let manifest = self
                .config
                .manifests
                .iter()
                .find(|manifest| manifest.id == subagent_type)
                .ok_or_else(|| anyhow!("unknown subagent_type: {subagent_type}"))?;
            let tokens = requested_tool_tokens(args, manifest)?;
            let resolved_tools = resolve_worker_tools(&tokens, self.config.capability_ceiling)?;
            let isolation = match args.get("isolation").and_then(Value::as_str) {
                Some("none") if resolved_tools.mutation_capable => {
                    return Err(anyhow!(
                        "mutation-capable worker tools require worktree isolation"
                    ));
                }
                Some("none") => iris_subagent_runtime::IsolationMode::None,
                Some("worktree") => iris_subagent_runtime::IsolationMode::Worktree,
                Some(other) => return Err(anyhow!("unsupported subagent isolation: {other}")),
                None if resolved_tools.mutation_capable => {
                    iris_subagent_runtime::IsolationMode::Worktree
                }
                None => iris_subagent_runtime::IsolationMode::None,
            };
            let model = optional_string_arg(args, "model")?;
            let provider = optional_string_arg(args, "provider")?;
            let effort = optional_string_arg(args, "effort")?;
            let parent_selection = self
                .config
                .selection
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone();
            let approved_api_vendors = self
                .config
                .approved_api_vendors
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone();
            let resolved = resolve_subagent_selection(
                &parent_selection,
                &self.config.catalog,
                manifest,
                model,
                provider,
                effort,
                &approved_api_vendors,
            )?;
            if let Some(lane) = &resolved.lane
                && lane.kind == crate::mimir::model_catalog::CredentialLaneKind::Api
            {
                self.config
                    .approved_api_vendors
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .insert(lane.vendor);
            }
            let route = crate::wayland::subagents::ChildRoute::new(
                resolved.selection.provider.as_str(),
                resolved.selection.model.clone(),
                resolved.selection.base_url.clone(),
                resolved
                    .selection
                    .reasoning
                    .map(crate::mimir::selection::ReasoningEffort::as_str),
            )
            .with_credential_lane(resolved.lane.as_ref().map(|lane| lane.kind));
            let mut request = iris_subagent_runtime::WorkerRequest::read_only(task);
            crate::wayland::subagents::attach_route(&mut request, &route)?;
            request.description = args
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or(subagent_type)
                .to_string();
            request.kind = manifest.worker_kind.clone();
            request.system_prompt = args
                .get("system_prompt")
                .and_then(Value::as_str)
                .unwrap_or(&manifest.system_prompt)
                .to_string();
            request.policy.tools = Some(resolved_tools.names);
            request.policy.isolation = isolation;
            request.policy.cwd = args.get("cwd").and_then(Value::as_str).map(PathBuf::from);
            request.policy.allow_outside_workspace = manifest.allow_outside_workspace;
            request.policy.nesting_depth = self.config.nesting_depth.saturating_add(1);
            request.policy.max_nesting_depth = self.config.max_nesting_depth;
            request.session_id = Some(self.config.session_id.clone());
            request.budgets.max_provider_rounds = Some(manifest.max_provider_rounds);
            let background = args
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let id = self.config.backend.spawn(
                self.config.provider_factory.clone(),
                request,
                self.config.approval.clone(),
            )?;
            if background {
                Ok(ToolOutput::text(
                    serde_json::json!({ "worker_id": id, "status": "queued" }).to_string(),
                ))
            } else {
                let wait = self.config.backend.runtime().handle().wait(&id);
                tokio::pin!(wait);
                let result = tokio::select! {
                    result = &mut wait => result?,
                    _ = cancel.cancelled() => {
                        self.config.backend.cancel(&id)?;
                        return Err(anyhow!("foreground subagent cancelled"));
                    }
                };
                Ok(ToolOutput::text(serde_json::to_string(&result)?))
            }
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn supports_allow_always(&self) -> bool {
        false
    }
}

struct SubagentStatusTool(Arc<crate::wayland::subagents::SubagentBackend>);

impl Tool for SubagentStatusTool {
    fn name(&self) -> &str {
        "subagent_status"
    }
    fn description(&self) -> &str {
        "Return a non-waiting snapshot for one worker."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "worker_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Worker ID from spawn_subagent."
                }
            },
            "required": ["worker_id"],
            "additionalProperties": false
        })
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let id = parse_optional_worker_id(args)?
                .ok_or_else(|| anyhow!("subagent_status requires worker_id"))?;
            let value = serde_json::to_value(self.0.poll(&id)?)?;
            Ok(ToolOutput::text(serde_json::to_string(&value)?))
        })
    }
}

struct SubagentArtifactTool(Arc<crate::wayland::subagents::SubagentBackend>);

impl Tool for SubagentArtifactTool {
    fn name(&self) -> &str {
        "read_subagent_output"
    }

    fn description(&self) -> &str {
        "Page UTF-8 output from an artifact in a terminal worker result."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "artifact_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Artifact ID from a terminal result."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "UTF-8 byte offset; continue at next_offset."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50000,
                    "default": 16000,
                    "description": "Bytes to return (1..=50000)."
                }
            },
            "required": ["artifact_id"],
            "additionalProperties": false
        })
    }

    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let id: iris_subagent_runtime::ArtifactId = args
                .get("artifact_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("artifact_id is required"))?
                .parse()?;
            let bytes = self.0.read_artifact(&id)?;
            let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
            let offset = usize::try_from(offset).map_err(|_| anyhow!("offset is too large"))?;
            if offset > bytes.len() {
                return Err(anyhow!("offset exceeds artifact length"));
            }
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(16_000);
            if !(1..=50_000).contains(&limit) {
                return Err(anyhow!("limit must be between 1 and 50000"));
            }
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| anyhow!("subagent output artifact is not UTF-8"))?;
            if !text.is_char_boundary(offset) {
                return Err(anyhow!("offset must be a UTF-8 character boundary"));
            }
            let mut end = offset.saturating_add(limit as usize).min(bytes.len());
            while end > offset && !text.is_char_boundary(end) {
                end -= 1;
            }
            let content = &text[offset..end];
            Ok(ToolOutput::text(
                serde_json::json!({
                    "artifact_id": id,
                    "offset": offset,
                    "next_offset": (end < bytes.len()).then_some(end),
                    "total_bytes": bytes.len(),
                    "content": content
                })
                .to_string(),
            ))
        })
    }
}

struct CancelSubagentTool(Arc<crate::wayland::subagents::SubagentBackend>);

impl Tool for CancelSubagentTool {
    fn name(&self) -> &str {
        "cancel_subagent"
    }
    fn description(&self) -> &str {
        "Cancel one worker cooperatively; hard-abort after the grace period."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "worker_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Worker ID from spawn_subagent."
                }
            },
            "required": ["worker_id"],
            "additionalProperties": false
        })
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let id = parse_optional_worker_id(args)?
                .ok_or_else(|| anyhow!("cancel_subagent requires worker_id"))?;
            let value = serde_json::to_value(self.0.cancel(&id)?)?;
            Ok(ToolOutput::text(serde_json::to_string(&value)?))
        })
    }
}

struct PlanSubagentApplyTool(Arc<crate::wayland::subagents::SubagentBackend>);

impl Tool for PlanSubagentApplyTool {
    fn name(&self) -> &str {
        "plan_subagent_apply"
    }
    fn description(&self) -> &str {
        "Create an immutable, digest-checked apply plan for a completed isolated worker; parent files stay unchanged."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "worker_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Completed worker with an isolated worktree."
                }
            },
            "required": ["worker_id"],
            "additionalProperties": false
        })
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let worker_id =
                parse_optional_worker_id(args)?.ok_or_else(|| anyhow!("worker_id is required"))?;
            let plan = self.0.plan_apply(&worker_id)?;
            Ok(ToolOutput::text(serde_json::to_string(&plan)?))
        })
    }
}

struct ApplySubagentTool(Arc<crate::wayland::subagents::SubagentBackend>);

impl Tool for ApplySubagentTool {
    fn name(&self) -> &str {
        "apply_subagent"
    }
    fn description(&self) -> &str {
        "Apply one immutable plan to the parent workspace after revalidation. This separately approval-gated step requires explicit authorization for dirty, base-drifted, and escaping-symlink paths."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan_id": {
                    "type": "string",
                    "minLength": 1,
                    "description": "ID from plan_subagent_apply."
                },
                "approved_overwrites": {
                    "type": "array",
                    "items": { "type": "string" },
                    "uniqueItems": true,
                    "default": [],
                    "description": "Dirty or base-drifted relative paths authorized for overwrite."
                },
                "approved_escaping_symlinks": {
                    "type": "array",
                    "items": { "type": "string" },
                    "uniqueItems": true,
                    "default": [],
                    "description": "Relative symlink paths authorized to escape the parent workspace."
                },
                "skipped_paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "uniqueItems": true,
                    "default": [],
                    "description": "Relative plan paths to skip; skipping overrides approvals."
                }
            },
            "required": ["plan_id"],
            "additionalProperties": false
        })
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Write
    }
    fn requires_approval(&self) -> bool {
        true
    }
    fn supports_allow_always(&self) -> bool {
        false
    }
    fn is_mutating(&self) -> bool {
        true
    }
    fn mutates_paths(&self, args: &Value) -> Vec<PathBuf> {
        self.load(args)
            .map(|plan| {
                plan.operations
                    .into_iter()
                    .map(|operation| operation.path)
                    .collect()
            })
            .unwrap_or_default()
    }
    fn diff_preview(&self, _workspace: &Path, args: &Value) -> Option<String> {
        let plan = self.load(args).ok()?;
        let mut output = format!("Apply plan {} ({})\n", plan.id, plan.digest);
        for operation in plan.operations {
            output.push_str(&format!(
                "{:?} {}",
                operation.change,
                operation.path.display()
            ));
            if operation.dirty_parent {
                output.push_str(" [dirty parent]");
            }
            if operation.base_drift {
                output.push_str(" [base drift]");
            }
            if operation.escaping_symlink {
                output.push_str(" [escaping symlink]");
            }
            output.push('\n');
        }
        Some(output)
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let plan = self.load(args)?;
            let mut options = iris_subagent_runtime::worktree::ApplyOptions::new();
            options.approved_overwrites = path_set(args, "approved_overwrites");
            options.approved_escaping_symlinks = path_set(args, "approved_escaping_symlinks");
            options.skipped_paths = path_set(args, "skipped_paths");
            let result = self.0.apply(&plan, &options)?;
            Ok(ToolOutput::text(serde_json::to_string(&result)?))
        })
    }
}

impl ApplySubagentTool {
    fn load(&self, args: &Value) -> Result<iris_subagent_runtime::worktree::ApplyPlan> {
        let id: iris_subagent_runtime::ApplyPlanId = args
            .get("plan_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("plan_id is required"))?
            .parse()?;
        self.0.load_apply_plan(&id)
    }
}

fn path_set(args: &Value, key: &str) -> std::collections::BTreeSet<PathBuf> {
    args.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .collect()
}

fn optional_string_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match args.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(anyhow!("spawn_subagent {key} must be a string")),
    }
}

struct ResolvedSubagentSelection {
    selection: crate::mimir::selection::ModelSelection,
    lane: Option<crate::mimir::model_catalog::SubagentCredentialLane>,
}

fn selection_for_subagent_entry(
    parent: &crate::mimir::selection::ModelSelection,
    entry: crate::mimir::model_catalog::SubagentCatalogEntry,
    effort: Option<&str>,
) -> Result<ResolvedSubagentSelection> {
    let lane = entry
        .lane
        .ok_or_else(|| anyhow!("subagent model has no active credential lane"))?;
    let selection = crate::mimir::selection::selection_for_catalog_model(
        parent,
        lane.provider,
        &entry.model.id,
        effort,
    )?;
    Ok(ResolvedSubagentSelection {
        selection,
        lane: Some(lane),
    })
}

fn resolve_subagent_selection(
    parent: &crate::mimir::selection::ModelSelection,
    catalog: &[crate::mimir::model_catalog::SubagentCatalogEntry],
    manifest: &crate::wayland::subagents::SubagentTypeManifest,
    model: Option<&str>,
    provider: Option<&str>,
    effort: Option<&str>,
    approved_api_vendors: &std::collections::BTreeSet<crate::mimir::model_catalog::ProviderVendor>,
) -> Result<ResolvedSubagentSelection> {
    if let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) {
        let entry =
            crate::mimir::model_catalog::resolve_subagent_model_in(catalog, model, provider)?;
        return selection_for_subagent_entry(parent, entry, effort);
    }
    if provider.is_some_and(|value| !value.trim().is_empty()) {
        return Err(anyhow!(
            "spawn_subagent provider requires model; set model to the target id"
        ));
    }
    for fallback in &manifest.model_fallbacks {
        let Ok(entry) =
            crate::mimir::model_catalog::resolve_subagent_model_in(catalog, fallback, None)
        else {
            continue;
        };
        let Some(lane) = entry.lane.as_ref() else {
            continue;
        };
        if lane.kind == crate::mimir::model_catalog::CredentialLaneKind::Api
            && !approved_api_vendors.contains(&lane.vendor)
        {
            continue;
        }
        return selection_for_subagent_entry(parent, entry, effort);
    }
    Ok(ResolvedSubagentSelection {
        selection: crate::mimir::selection::apply_selection_overrides(parent, None, effort)?,
        lane: None,
    })
}

fn parse_optional_worker_id(args: &Value) -> Result<Option<iris_subagent_runtime::WorkerId>> {
    args.get("worker_id")
        .and_then(Value::as_str)
        .map(str::parse)
        .transpose()
        .map_err(Into::into)
}

struct AskUserQuestionTool;
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "AskUserQuestion"
    }

    fn description(&self) -> &str {
        ask_user_question::DESCRIPTION
    }

    fn parameters(&self) -> Value {
        ask_user_question::parameters()
    }

    fn capability(&self) -> ToolCapability {
        ToolCapability::UserInteraction
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let input = ask_user_question::parse_input(args)?;
            if input.answers.len() != input.questions.len()
                || input
                    .answers
                    .values()
                    .any(|answer| answer.trim().is_empty())
            {
                anyhow::bail!("AskUserQuestion requires an answer for every question");
            }
            Ok(ToolOutput::text(ask_user_question::format_result(&input)))
        })
    }

    fn requires_user_interaction(&self) -> bool {
        true
    }
}

struct GetGoalTool;
impl Tool for GetGoalTool {
    fn name(&self) -> &str {
        "get_goal"
    }
    fn description(&self) -> &str {
        goal::GET_DESCRIPTION
    }
    fn parameters(&self) -> Value {
        goal::empty_parameters()
    }
    fn execute<'a>(
        &'a self,
        _args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let state = env.state.borrow();
            goal::get(state.goal_controller())
        })
    }
}

struct CreateGoalTool;
impl Tool for CreateGoalTool {
    fn name(&self) -> &str {
        "create_goal"
    }
    fn description(&self) -> &str {
        goal::CREATE_DESCRIPTION
    }
    fn parameters(&self) -> Value {
        goal::create_parameters()
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let state = env.state.borrow();
            goal::create(args, state.goal_controller(), env.output_store)
        })
    }
}

struct UpdateGoalTool;
impl Tool for UpdateGoalTool {
    fn name(&self) -> &str {
        "update_goal"
    }
    fn description(&self) -> &str {
        goal::UPDATE_DESCRIPTION
    }
    fn parameters(&self) -> Value {
        goal::update_parameters()
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let state = env.state.borrow();
            goal::update(args, state.goal_controller())
        })
    }
}

struct ReadTool;
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        read::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        read::parameters()
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let root = root(env)?;
            let mut state = state_mut(env)?;
            let state = &mut *state;
            path::with_restrictions(state.read_workspace_restrictions, || {
                read::execute(&root, args, &mut state.observed, &state.skill_read_roots)
            })
        })
    }
    // `read` mutates `state.observed` (read-before-write tracking) behind the
    // env's `!Send` RefCell, so it cannot run off-thread and is not
    // concurrency-safe; it takes the exclusive path (default).
}

/// Bridges the bash tool's live-output sink across the `spawn_blocking`
/// boundary. The blocking body holds `Some(&ChannelSink)` and forwards each
/// chunk over the channel; the async side forwards them into the real
/// (non-`Send`) [`crate::nexus::ToolOutputSink`]. A closed receiver (dropped
/// future) makes `send` fail silently -- streaming is best-effort.
struct ChannelSink {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
}

impl crate::nexus::ToolOutputSink for ChannelSink {
    fn emit_chunk(&self, chunk: &str) {
        let _ = self.tx.send(chunk.to_string());
    }
}

struct BashTool;
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        bash::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        bash::parameters()
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Execute
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let root = root(env)?;
            let args = args.clone();
            // Share the bash registry (not the env's `!Send` `RefCell`) with the
            // blocking task. The bash tool is exclusive, so this lock never
            // contends; the `Arc` clone keeps the registry alive even if this
            // future is dropped on cancel and the blocking task is detached.
            let bash_state = std::sync::Arc::clone(&state_mut(env)?.bash);
            // Benchmark arm switch (issue #210): arm B forces raw (unfiltered)
            // bash output. Copied out before the blocking task so no borrow of
            // the `!Send` env crosses the thread boundary.
            let reduce = reduce_output(env);
            let strict = workspace_restrictions(env);

            // Bridge the live-output sink across the thread boundary: the
            // blocking body forwards each chunk over an unbounded channel and the
            // async side (below) drains it into the real, non-`Send` sink while
            // the command runs, so `ToolOutputDelta` events reach the UI live
            // instead of only when the command returns.
            let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            let cancel_for_task = cancel.clone();
            let mut handle = tokio::task::spawn_blocking(move || {
                let sink = ChannelSink { tx: chunk_tx };
                // Poisoning means a previous bash run panicked mid-mutation, so
                // the session/job registry may be half-updated. Surface it as a
                // tool error rather than recovering onto inconsistent state.
                let mut guard = bash_state.lock().map_err(|_| {
                    anyhow!("bash state poisoned by a previous panic; restart the session")
                })?;
                path::with_restrictions(strict, || {
                    bash::execute(
                        &root,
                        &args,
                        &mut guard,
                        &cancel_for_task,
                        Some(&sink),
                        reduce,
                    )
                })
            });

            // Keep polling the executor while the command runs: forward each
            // streamed chunk as it arrives, and finish when the blocking task
            // joins. Once the sender drops, `recv` yields `None`; disable that
            // select branch (`chunks_open`) so the loop stops polling the closed
            // receiver -- otherwise it busy-spins on the current-thread runtime
            // in the window before the join handle is ready.
            let mut chunks_open = true;
            let result = loop {
                tokio::select! {
                    chunk = chunk_rx.recv(), if chunks_open => {
                        match chunk {
                            Some(chunk) => {
                                if let Some(sink) = env.output_sink {
                                    sink.emit_chunk(&chunk);
                                }
                            }
                            None => chunks_open = false,
                        }
                    }
                    joined = &mut handle => {
                        break joined.map_err(|e| anyhow!("bash tool task failed: {e}"))?;
                    }
                }
            };
            // Drain any chunks the task produced just before it finished.
            while let Ok(chunk) = chunk_rx.try_recv() {
                if let Some(sink) = env.output_sink {
                    sink.emit_chunk(&chunk);
                }
            }
            result
        })
    }
    fn requires_approval(&self) -> bool {
        // Approval is independent of workspace/path confinement. Print mode
        // denies this by default, and interactive mode asks before running it.
        true
    }
    fn is_destructive(&self, args: &Value) -> bool {
        bash_command_is_destructive(args)
    }
    fn supports_allow_always(&self) -> bool {
        // A blanket "always" on bash would authorize any later shell command;
        // shell stays approval-per-call.
        false
    }
    fn is_mutating(&self) -> bool {
        // A shell command may write anything: it opens the dirty-tree task and
        // is bracketed by the guard's snapshot/verify (issue #262). No static
        // path set, so `mutates_paths` stays empty and detection runs instead.
        true
    }
}

struct EditTool;
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        edit::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        edit::parameters()
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Write
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let root = root(env)?;
            let mut state = state_mut(env)?;
            path::with_restrictions(state.workspace_restrictions, || {
                edit::execute(&root, args, &mut state.observed)
            })
        })
    }
    fn requires_approval(&self) -> bool {
        // Approval is independent of workspace/path confinement. Print mode
        // denies this by default, and interactive mode asks before running it.
        true
    }
    fn supports_allow_always(&self) -> bool {
        // A blanket "always" on edit would authorize arbitrary later edits to
        // any workspace file; edits stay approval-per-call until policy is
        // path/exact-call scoped (roadmap #14).
        false
    }
    fn is_mutating(&self) -> bool {
        true
    }
    fn mutates_paths(&self, args: &Value) -> Vec<PathBuf> {
        // `edit` targets its `file_path` argument. The guard normalizes it
        // against the workspace, so a relative or absolute value both resolve.
        mutated_path(args, "file_path")
    }
    fn auto_approvable(&self, workspace: &Path, args: &Value) -> bool {
        // Auto preset (ADR-0032): auto-run only an in-workspace target. The
        // dirty/destructive floors are enforced by Nexus before this is
        // consulted; here we only keep an outside-workspace edit on the prompt
        // path (fail closed on a missing/escaping path).
        auto_target_in_workspace(workspace, args, "file_path")
    }
    fn diff_preview(&self, workspace: &Path, args: &Value) -> Option<String> {
        render(workspace, |root| edit::preview(root, args))
    }
}

struct WriteTool;
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> &str {
        write::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        write::parameters()
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Write
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let root = root(env)?;
            let mut state = state_mut(env)?;
            path::with_restrictions(state.workspace_restrictions, || {
                write::execute(&root, args, &mut state.observed)
            })
        })
    }
    fn requires_approval(&self) -> bool {
        // Approval is independent of workspace/path confinement. Print mode
        // denies this by default, and interactive mode asks before running it.
        true
    }
    fn supports_allow_always(&self) -> bool {
        // A blanket "always" on write would authorize arbitrary later writes to
        // any workspace file; writes stay approval-per-call until policy is
        // path/exact-call scoped (roadmap #14).
        false
    }
    fn is_mutating(&self) -> bool {
        true
    }
    fn mutates_paths(&self, args: &Value) -> Vec<PathBuf> {
        // `write` targets its `path` argument.
        mutated_path(args, "path")
    }
    fn auto_approvable(&self, workspace: &Path, args: &Value) -> bool {
        // Auto preset (ADR-0032): auto-run only an in-workspace target.
        auto_target_in_workspace(workspace, args, "path")
    }
    fn diff_preview(&self, workspace: &Path, args: &Value) -> Option<String> {
        render(workspace, |root| write::preview(root, args))
    }
}

struct GrepTool;
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        grep::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        grep::parameters()
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        run_off_thread(
            root(env),
            args.clone(),
            "grep",
            reduce_output(env),
            read_workspace_restrictions(env),
            grep::execute,
        )
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
}

struct FindTool;
impl Tool for FindTool {
    fn name(&self) -> &str {
        "find"
    }
    fn description(&self) -> &str {
        find::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        find::parameters()
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        run_off_thread(
            root(env),
            args.clone(),
            "find",
            reduce_output(env),
            read_workspace_restrictions(env),
            find::execute,
        )
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
}

struct ReadOutputTool;
impl Tool for ReadOutputTool {
    fn name(&self) -> &str {
        "read_output"
    }
    fn description(&self) -> &str {
        read_output::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        read_output::parameters()
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        // Reads back an offloaded output via the `ToolOutputStore` contract. The
        // store is a non-`'static` borrow (`env.output_store`), so this cannot
        // move the body onto `run_off_thread`'s blocking pool the way
        // `grep`/`find`/`ls` do; it does the small store read inline in the async
        // body like `read`/`edit`. It touches no `ToolState`, only the immutable
        // store, so it is still `is_concurrency_safe` and may join a parallel
        // read-only batch.
        Box::pin(async move { read_output::execute(env.output_store, args) })
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
}

struct RecallTool;
impl Tool for RecallTool {
    fn name(&self) -> &str {
        recall::RECALL_TOOL_NAME
    }
    fn description(&self) -> &str {
        recall::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        recall::parameters()
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        // Read-only over this session's own transcript via the same
        // `ToolOutputStore` contract `read_output` uses (ADR-0011 / ADR-0046):
        // no workspace path, no shell, no `ToolState`. Kept sequential (the
        // default) rather than opted into safe-parallel: it needs no such
        // guarantee and the task keeps recall sequential-by-default.
        Box::pin(async move { recall::execute(env.output_store, env.session_span, args) })
    }
}

struct RequestCompactionTool;
impl Tool for RequestCompactionTool {
    fn name(&self) -> &str {
        "request_compaction"
    }
    fn description(&self) -> &str {
        request_compaction::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        request_compaction::parameters()
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async move {
            let state = env
                .state
                .try_borrow()
                .map_err(|_| anyhow!("tool state is busy; compaction request was not scheduled"))?;
            request_compaction::execute(args, &state)
        })
    }
}

struct LsTool;
impl Tool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }
    fn description(&self) -> &str {
        ls::DESCRIPTION
    }
    fn parameters(&self) -> Value {
        ls::parameters()
    }
    fn capability(&self) -> ToolCapability {
        ToolCapability::Read
    }
    fn execute<'a>(
        &'a self,
        args: &'a Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        run_off_thread(
            root(env),
            args.clone(),
            "ls",
            reduce_output(env),
            read_workspace_restrictions(env),
            ls::execute,
        )
    }
    fn is_concurrency_safe(&self) -> bool {
        true
    }
}

/// Whether a mutating tool's string-valued path argument resolves inside the
/// workspace, for the ADR-0032 auto preset. A missing/non-string/escaping path
/// fails closed (`false`), keeping such a call on the approval-prompt path.
///
/// This is an approval-time CLASSIFICATION, not the execution boundary: the
/// tool body still re-resolves the path through `path::resolve_*`, which
/// re-canonicalizes the deepest existing ancestor and bails on an escape when
/// confinement is active. So auto never bypasses an active confinement, and it
/// is strictly more conservative than execution (it refuses to auto-approve an
/// outside-workspace target even where execution would not confine one). It is
/// deliberately not a write-time TOCTOU boundary; closing the open()-follows
/// -symlink race belongs to the execution path uniformly, not to this preset.
fn auto_target_in_workspace(workspace: &Path, args: &Value, key: &str) -> bool {
    args.get(key)
        .and_then(Value::as_str)
        .is_some_and(|requested| path::is_inside_workspace(workspace, requested))
}

/// Extract a single mutated path from a string-valued tool argument (issue
/// #262). Returns an empty vec when the argument is missing or not a string, so
/// a malformed call is simply not dirty-gated (it fails in the tool body).
fn mutated_path(args: &Value, key: &str) -> Vec<PathBuf> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| vec![PathBuf::from(value)])
        .unwrap_or_default()
}

/// Whether a bash command performs a destructive, data-losing operation. The
/// check is deliberately conservative and biased toward flagging: a false
/// positive costs one extra prompt, a false negative could auto-run an `rm`.
fn bash_command_is_destructive(args: &Value) -> bool {
    let Some(command) = args.get("command").and_then(Value::as_str) else {
        return false;
    };
    let lower = command.to_ascii_lowercase();
    // Whole-word commands that destroy files/filesystems/devices.
    const DANGER_TOKENS: &[&str] = &[
        "rm", "rmdir", "shred", "mkfs", "dd", "truncate", "fdisk", "mkswap", "wipefs",
    ];
    let token_danger = lower
        .split(|c: char| c.is_whitespace() || matches!(c, '&' | '|' | ';' | '(' | ')' | '`'))
        .filter(|token| !token.is_empty())
        .any(|token| {
            let command = token.rsplit('/').next().unwrap_or(token);
            let command = destructive_command_basename(command);
            DANGER_TOKENS.contains(&command.as_str()) || command.starts_with("mkfs.")
        });
    if token_danger {
        return true;
    }
    // Multi-word / flag patterns a single-token scan cannot catch.
    const DANGER_PHRASES: &[&str] = &[
        "-delete",
        "git reset --hard",
        "git clean",
        // Recoverability destroyers that discard uncommitted work (ADR-0028):
        // both restore working-tree paths from the index/HEAD, wiping edits.
        "git checkout --",
        "git restore",
        "git push --force",
        "git push -f",
        "chmod -r",
        "chown -r",
        ":(){",
        "of=/dev/",
        "> /dev/sd",
    ];
    DANGER_PHRASES.iter().any(|phrase| lower.contains(phrase))
}

/// Normalize the command word enough for destructive-command classification:
/// path-qualified basenames (`/bin/rm`), quoted command words (`'rm'`), and
/// escaped spellings (`\rm`, `r\m`) all invoke the same shell command. This is
/// intentionally conservative; false positives cost a prompt, false negatives
/// could persist or auto-approve a destructive command.
fn destructive_command_basename(token: &str) -> String {
    token
        .chars()
        .filter(|c| !matches!(c, '\\' | '\'' | '"'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::{ChatProvider, Message, ProviderEvent, ProviderStream};
    use crate::tools::test_support::{root_of, temp_dir};
    use serde_json::json;
    use std::rc::Rc;

    struct PendingProvider(Rc<()>);

    impl ChatProvider for PendingProvider {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a crate::nexus::Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            let _ = &self.0;
            Ok(Box::pin(futures::stream::pending::<Result<ProviderEvent>>()))
        }
    }

    fn bash_args(command: &str) -> Value {
        json!({ "command": command })
    }

    #[test]
    fn delegated_capability_modes_filter_visibility_and_execution_lookup() {
        use crate::nexus::WorkerCapabilityGrant;

        let read_only = built_in_tools().into_capability(WorkerCapabilityGrant::ReadOnly);
        assert!(read_only.by_name("read").is_some());
        assert!(read_only.by_name("edit").is_none());
        assert!(read_only.by_name("bash").is_none());
        assert!(read_only.by_name("AskUserQuestion").is_none());

        let read_write = built_in_tools().into_capability(WorkerCapabilityGrant::ReadWrite);
        assert!(read_write.by_name("edit").is_some());
        assert!(read_write.by_name("write").is_some());
        assert!(read_write.by_name("bash").is_none());

        let execute = built_in_tools().into_capability(WorkerCapabilityGrant::Execute);
        assert!(execute.by_name("bash").is_some());
        assert!(execute.by_name("edit").is_none());
        assert!(execute.by_name("write").is_none());

        let all = built_in_tools().into_capability(WorkerCapabilityGrant::All);
        assert!(all.by_name("edit").is_some());
        assert!(all.by_name("write").is_some());
        assert!(all.by_name("bash").is_some());
        assert!(all.by_name("AskUserQuestion").is_none());
    }

    #[test]
    fn destructive_bash_detection_catches_path_qualified_variants() {
        for command in [
            "/bin/rm -rf target",
            "/usr/bin/dd if=/dev/zero of=file",
            "mkfs.ext4 /dev/sdz",
        ] {
            assert!(
                bash_command_is_destructive(&bash_args(command)),
                "{command} should be destructive"
            );
        }
    }

    #[test]
    fn destructive_bash_detection_catches_recoverability_destroyers() {
        // ADR-0028: commands that discard uncommitted work must re-prompt.
        for command in [
            "git checkout -- .",
            "git checkout -- src/main.rs",
            "git clean -fd",
            "git restore .",
            "git restore --staged --worktree file",
            "rm -rf target",
            "git reset --hard HEAD",
        ] {
            assert!(
                bash_command_is_destructive(&bash_args(command)),
                "{command} should be destructive"
            );
        }
    }

    #[test]
    fn destructive_bash_detection_catches_quoted_and_escaped_commands() {
        for command in [
            "\\rm -rf target",
            "r\\m -rf target",
            "'rm' -rf target",
            "\"rm\" -rf target",
            "git status; /bin/r\\m -rf target",
        ] {
            assert!(
                bash_command_is_destructive(&bash_args(command)),
                "{command} should be destructive"
            );
        }
    }

    /// A sink that records the wall-clock offset (from a shared start) of every
    /// forwarded chunk, so a test can assert deltas arrive *while* the command
    /// runs rather than only after it returns.
    struct TimingSink {
        start: std::time::Instant,
        first_delta: std::cell::RefCell<Option<std::time::Duration>>,
    }
    impl crate::nexus::ToolOutputSink for TimingSink {
        fn emit_chunk(&self, _chunk: &str) {
            let mut slot = self.first_delta.borrow_mut();
            if slot.is_none() {
                *slot = Some(self.start.elapsed());
            }
        }
    }

    fn current_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn test_selection() -> Arc<std::sync::Mutex<crate::mimir::selection::ModelSelection>> {
        Arc::new(std::sync::Mutex::new(
            crate::mimir::selection::ModelSelection::resolve(&crate::config::Settings::default())
                .unwrap(),
        ))
    }

    /// A fixed authenticated catalog for subagent routing tests: one OAuth Codex
    /// model and one Anthropic model, so resolution is deterministic and does not
    /// depend on the machine's real auth store.
    fn test_catalog() -> Vec<crate::mimir::model_catalog::SubagentCatalogEntry> {
        use crate::mimir::model_catalog::{
            CatalogModel, CredentialLaneKind, ProviderVendor, SubagentCatalogEntry,
            SubagentCredentialLane,
        };
        use crate::mimir::selection::ProviderId;
        [
            (
                ProviderId::OpenAiCodex,
                "gpt-5.4-mini",
                "openai-codex",
                ProviderVendor::OpenAi,
            ),
            (
                ProviderId::Anthropic,
                "claude-opus-4-6",
                "anthropic-oauth",
                ProviderVendor::Anthropic,
            ),
        ]
        .into_iter()
        .map(|(provider, id, lane_id, vendor)| SubagentCatalogEntry {
            model: CatalogModel {
                provider,
                id: id.to_string(),
                ctx_label: None,
            },
            lane: Some(SubagentCredentialLane {
                id: lane_id.to_string(),
                vendor,
                provider,
                kind: CredentialLaneKind::OAuth,
            }),
        })
        .collect()
    }

    fn test_subagent_config(
        backend: Arc<crate::wayland::subagents::SubagentBackend>,
        provider_factory: crate::wayland::subagents::ChildProviderFactory,
        catalog: Vec<crate::mimir::model_catalog::SubagentCatalogEntry>,
        session_id: &str,
    ) -> SubagentToolsConfig {
        SubagentToolsConfig {
            backend,
            provider_factory,
            selection: test_selection(),
            catalog,
            manifests: crate::wayland::subagents::default_subagent_type_manifests(),
            capability_ceiling: iris_subagent_runtime::CapabilityMode::All,
            approved_api_vendors: Arc::new(
                std::sync::Mutex::new(std::collections::BTreeSet::new()),
            ),
            session_id: session_id.to_string(),
            nesting_depth: 0,
            max_nesting_depth: 2,
            approval: None,
        }
    }

    #[test]
    fn spawn_subagent_schema_matches_the_manifest_driven_surface() {
        let dir = temp_dir();
        let workspace = root_of(&dir);
        let backend = Arc::new(
            crate::wayland::subagents::SubagentBackend::open(
                workspace.clone(),
                &workspace.join("worker-state-schema-red"),
                workspace.join("worktrees-schema-red"),
            )
            .unwrap(),
        );
        let mut schema_catalog = test_catalog();
        schema_catalog.push(crate::mimir::model_catalog::SubagentCatalogEntry {
            model: crate::mimir::model_catalog::CatalogModel {
                provider: crate::mimir::selection::ProviderId::Anthropic,
                id: "unauthenticated-model".to_string(),
                ctx_label: None,
            },
            lane: None,
        });
        let config = test_subagent_config(
            backend.clone(),
            Arc::new(|_| Err(anyhow!("schema contract must not construct a provider"))),
            schema_catalog,
            "schema-contract",
        );
        let tools = built_in_tools_with(&ToolsConfig {
            subagents: Some(config),
            ..ToolsConfig::default()
        });
        let spawn = tools.by_name("spawn_subagent").unwrap();
        assert!(spawn.requires_approval());
        assert!(!spawn.supports_allow_always());
        let schema = spawn.parameters();
        let properties = schema["properties"].as_object().unwrap();
        let mut fields = properties.keys().map(String::as_str).collect::<Vec<_>>();
        fields.sort_unstable();
        assert_eq!(
            fields,
            vec![
                "background",
                "cwd",
                "description",
                "effort",
                "isolation",
                "model",
                "subagent_type",
                "system_prompt",
                "task",
                "tools",
            ]
        );
        assert_eq!(schema["required"], json!(["task"]));
        assert_eq!(
            properties["model"]["enum"],
            json!(["gpt-5.4-mini", "claude-opus-4-6"])
        );
        assert_eq!(properties["subagent_type"]["default"], "general");
        assert!(spawn.description().contains("general:"));
        assert!(spawn.description().contains("explore:"));
        assert!(spawn.description().contains("review:"));
        assert!(tools.by_name("select_subagent_candidate").is_none());
        assert!(
            tools.by_name("subagent_status").unwrap().parameters()["properties"]
                .get("group_id")
                .is_none()
        );
        assert!(
            tools.by_name("cancel_subagent").unwrap().parameters()["properties"]
                .get("group_id")
                .is_none()
        );

        use crate::mimir::model_catalog::{
            CatalogModel, CredentialLaneKind, ProviderVendor, SubagentCatalogEntry,
            SubagentCredentialLane,
        };
        use crate::mimir::selection::ProviderId;
        let mut multi_lane_catalog = test_catalog();
        multi_lane_catalog.push(SubagentCatalogEntry {
            model: CatalogModel {
                provider: ProviderId::OpenAi,
                id: "gpt-5.4-mini".to_string(),
                ctx_label: None,
            },
            lane: Some(SubagentCredentialLane {
                id: "openai".to_string(),
                vendor: ProviderVendor::OpenAi,
                provider: ProviderId::OpenAi,
                kind: CredentialLaneKind::Api,
            }),
        });
        let multi_lane = SpawnSubagentTool::new(test_subagent_config(
            backend,
            Arc::new(|_| Err(anyhow!("schema contract must not construct a provider"))),
            multi_lane_catalog,
            "schema-contract-multi-lane",
        ));
        let provider = &multi_lane.parameters()["properties"]["provider"];
        assert_eq!(
            provider["enum"],
            json!(["openai-codex", "anthropic-oauth", "openai"])
        );
    }

    fn bash_env<'a>(
        workspace: &'a std::path::Path,
        state: &'a std::cell::RefCell<ToolState>,
        sink: Option<&'a dyn crate::nexus::ToolOutputSink>,
    ) -> ToolEnv<'a> {
        ToolEnv {
            workspace,
            state,
            output_store: None,
            session_span: None,
            output_sink: sink,
            mutation_guard: None,
        }
    }

    #[test]
    fn invalid_routing_overrides_are_rejected_before_worker_acceptance() {
        let dir = temp_dir();
        let workspace = root_of(&dir);
        let backend = Arc::new(
            crate::wayland::subagents::SubagentBackend::open(
                workspace.clone(),
                &workspace.join("worker-state"),
                workspace.join("worktrees"),
            )
            .unwrap(),
        );
        let provider_factory: crate::wayland::subagents::ChildProviderFactory =
            Arc::new(|_| Err(anyhow!("invalid route must not construct a provider")));
        let tool = SpawnSubagentTool::new(test_subagent_config(
            backend.clone(),
            provider_factory,
            test_catalog(),
            "invalid-route",
        ));
        let state = std::cell::RefCell::new(ToolState::new());
        let env = bash_env(&workspace, &state, None);

        for invalid in [
            // Not in the authenticated catalog at all.
            json!({ "task": "invalid", "model": "gpt-4.1" }),
            // A valid id, but not offered by the named provider.
            json!({ "task": "invalid", "model": "gpt-5.4-mini", "provider": "openai" }),
            // provider without model has nothing to disambiguate.
            json!({ "task": "invalid", "provider": "anthropic" }),
            // Bad reasoning level still fails before acceptance.
            json!({ "task": "invalid", "effort": "ultra" }),
        ] {
            let error = current_thread_runtime()
                .block_on(tool.execute(&invalid, &env, CancellationToken::new()))
                .unwrap_err();
            assert!(
                error.to_string().contains("model")
                    || error.to_string().contains("provider")
                    || error.to_string().contains("reasoning"),
                "unexpected routing error: {error:#}"
            );
            assert!(
                backend
                    .runtime()
                    .handle()
                    .list(&iris_subagent_runtime::WorkerFilter::default())
                    .is_empty(),
                "invalid routing must fail before durable worker acceptance"
            );
        }
    }

    #[test]
    fn omitted_subagent_type_uses_general_manifest_prompt_and_turn_budget() {
        let dir = temp_dir();
        let workspace = root_of(&dir);
        let backend = Arc::new(
            crate::wayland::subagents::SubagentBackend::open(
                workspace.clone(),
                &workspace.join("worker-state"),
                workspace.join("worktrees"),
            )
            .unwrap(),
        );
        let provider_factory: crate::wayland::subagents::ChildProviderFactory =
            Arc::new(|_| Ok(Box::new(PendingProvider(Rc::new(()))) as Box<dyn ChatProvider>));
        let tool = SpawnSubagentTool::new(test_subagent_config(
            backend.clone(),
            provider_factory,
            test_catalog(),
            "direct-route",
        ));
        let general_prompt = tool
            .config
            .manifests
            .iter()
            .find(|manifest| manifest.id == "general")
            .unwrap()
            .system_prompt
            .clone();
        let state = std::cell::RefCell::new(ToolState::new());
        let env = bash_env(&workspace, &state, None);

        let output = current_thread_runtime()
            .block_on(tool.execute(
                &json!({
                    "task": "route",
                    "model": "claude-opus-4-6",
                    "effort": "xhigh"
                }),
                &env,
                CancellationToken::new(),
            ))
            .unwrap();
        let value: Value = serde_json::from_str(&output.content).unwrap();
        let worker_id: iris_subagent_runtime::WorkerId =
            value["worker_id"].as_str().unwrap().parse().unwrap();
        let snapshot = backend.poll(&worker_id).unwrap();
        let route = crate::wayland::subagents::route_from_request(&snapshot.request)
            .unwrap()
            .unwrap();
        assert_eq!(route.provider, "anthropic");
        assert_eq!(route.model, "claude-opus-4-6");
        assert_eq!(route.effort.as_deref(), Some("xhigh"));
        assert_eq!(
            route.credential_lane,
            Some(crate::mimir::model_catalog::CredentialLaneKind::OAuth)
        );
        assert_eq!(
            snapshot.request.kind,
            iris_subagent_runtime::WorkerKind::General
        );
        assert_eq!(snapshot.request.system_prompt, general_prompt);
        assert_ne!(snapshot.request.system_prompt, snapshot.request.prompt);
        assert_eq!(snapshot.request.budgets.max_provider_rounds, Some(200));
        assert!(snapshot.request.profile_id.is_none());
        backend.cancel(&worker_id).unwrap();
    }

    #[test]
    fn explicit_tools_replace_manifest_profile_and_clamp_to_parent_ceiling() {
        let dir = temp_dir();
        let workspace = root_of(&dir);
        let backend = Arc::new(
            crate::wayland::subagents::SubagentBackend::open(
                workspace.clone(),
                &workspace.join("worker-state-tools"),
                workspace.join("worktrees-tools"),
            )
            .unwrap(),
        );
        let provider_factory: crate::wayland::subagents::ChildProviderFactory =
            Arc::new(|_| Ok(Box::new(PendingProvider(Rc::new(()))) as Box<dyn ChatProvider>));
        let mut config = test_subagent_config(
            backend.clone(),
            provider_factory,
            test_catalog(),
            "tool-override",
        );
        config.capability_ceiling = iris_subagent_runtime::CapabilityMode::Execute;
        let tool = SpawnSubagentTool::new(config);
        let state = std::cell::RefCell::new(ToolState::new());
        let env = bash_env(&workspace, &state, None);

        let output = current_thread_runtime()
            .block_on(tool.execute(
                &json!({
                    "task": "inspect",
                    "subagent_type": "explore",
                    "tools": ["grep", "bash", "write"]
                }),
                &env,
                CancellationToken::new(),
            ))
            .unwrap();
        let value: Value = serde_json::from_str(&output.content).unwrap();
        let worker_id: iris_subagent_runtime::WorkerId =
            value["worker_id"].as_str().unwrap().parse().unwrap();
        let snapshot = backend.poll(&worker_id).unwrap();
        assert_eq!(
            snapshot.request.kind,
            iris_subagent_runtime::WorkerKind::Explore
        );
        assert_eq!(
            snapshot.request.policy.tools,
            Some(vec!["bash".to_string(), "grep".to_string()])
        );
        assert_eq!(
            snapshot.request.policy.isolation,
            iris_subagent_runtime::IsolationMode::Worktree
        );
        backend.cancel(&worker_id).unwrap();
    }

    #[test]
    fn explicit_api_lane_selection_is_session_sticky() {
        use crate::mimir::model_catalog::{
            CatalogModel, CredentialLaneKind, ProviderVendor, SubagentCatalogEntry,
            SubagentCredentialLane,
        };
        use crate::mimir::selection::ProviderId;

        let dir = temp_dir();
        let workspace = root_of(&dir);
        let backend = Arc::new(
            crate::wayland::subagents::SubagentBackend::open(
                workspace.clone(),
                &workspace.join("worker-state-api-lane"),
                workspace.join("worktrees-api-lane"),
            )
            .unwrap(),
        );
        let catalog = [
            (
                "openai-codex",
                ProviderId::OpenAiCodex,
                CredentialLaneKind::OAuth,
                "gpt-oauth",
            ),
            (
                "openai",
                ProviderId::OpenAi,
                CredentialLaneKind::Api,
                "gpt-api",
            ),
        ]
        .into_iter()
        .map(|(id, provider, kind, model)| SubagentCatalogEntry {
            model: CatalogModel {
                provider,
                id: model.to_string(),
                ctx_label: None,
            },
            lane: Some(SubagentCredentialLane {
                id: id.to_string(),
                vendor: ProviderVendor::OpenAi,
                provider,
                kind,
            }),
        })
        .collect();
        let provider_factory: crate::wayland::subagents::ChildProviderFactory =
            Arc::new(|_| Ok(Box::new(PendingProvider(Rc::new(()))) as Box<dyn ChatProvider>));
        let mut config =
            test_subagent_config(backend.clone(), provider_factory, catalog, "api-lane");
        let mut manifests = config.manifests.to_vec();
        manifests
            .iter_mut()
            .find(|manifest| manifest.id == "general")
            .unwrap()
            .model_fallbacks = vec!["gpt-api".to_string()];
        config.manifests = Arc::from(manifests);
        let tool = SpawnSubagentTool::new(config);
        let state = std::cell::RefCell::new(ToolState::new());
        let env = bash_env(&workspace, &state, None);

        let spawn_route = |args: Value| {
            let output = current_thread_runtime()
                .block_on(tool.execute(&args, &env, CancellationToken::new()))
                .unwrap();
            let value: Value = serde_json::from_str(&output.content).unwrap();
            let worker_id: iris_subagent_runtime::WorkerId =
                value["worker_id"].as_str().unwrap().parse().unwrap();
            let route = crate::wayland::subagents::route_from_request(
                &backend.poll(&worker_id).unwrap().request,
            )
            .unwrap()
            .unwrap();
            (worker_id, route)
        };

        let (before_id, before) =
            spawn_route(json!({ "task": "before approval", "tools": ["read"] }));
        assert_eq!(before.provider, "openai-codex");
        assert_eq!(before.credential_lane, None);
        backend.cancel(&before_id).unwrap();

        let (explicit_id, explicit) = spawn_route(json!({
            "task": "approve API lane",
            "model": "gpt-api",
            "provider": "openai",
            "tools": ["read"]
        }));
        assert_eq!(explicit.provider, "openai");
        assert_eq!(explicit.credential_lane, Some(CredentialLaneKind::Api));
        assert!(
            tool.config
                .approved_api_vendors
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .contains(&ProviderVendor::OpenAi)
        );
        backend.cancel(&explicit_id).unwrap();

        let (after_id, after) = spawn_route(json!({ "task": "after approval", "tools": ["read"] }));
        assert_eq!(after.provider, "openai");
        assert_eq!(after.credential_lane, Some(CredentialLaneKind::Api));
        backend.cancel(&after_id).unwrap();
    }

    #[test]
    fn cancelling_foreground_spawn_cancels_the_independent_worker() {
        let dir = temp_dir();
        let workspace = root_of(&dir);
        let backend = Arc::new(
            crate::wayland::subagents::SubagentBackend::open(
                workspace.clone(),
                &workspace.join("worker-state"),
                workspace.join("worktrees"),
            )
            .unwrap(),
        );
        let provider_factory: crate::wayland::subagents::ChildProviderFactory =
            Arc::new(|_| Ok(Box::new(PendingProvider(Rc::new(()))) as Box<dyn ChatProvider>));
        let tool = SpawnSubagentTool::new(test_subagent_config(
            backend.clone(),
            provider_factory,
            Vec::new(),
            "foreground-cancel",
        ));
        let state = std::cell::RefCell::new(ToolState::new());
        let env = bash_env(&workspace, &state, None);
        let cancel = CancellationToken::new();
        let cancel_from_thread = cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            cancel_from_thread.cancel();
        });

        let error = current_thread_runtime()
            .block_on(tool.execute(
                &json!({
                    "task": "wait",
                    "tools": ["read_only"],
                    "background": false
                }),
                &env,
                cancel,
            ))
            .unwrap_err();
        canceller.join().unwrap();
        assert!(error.to_string().contains("foreground subagent cancelled"));
        let snapshot = backend
            .runtime()
            .handle()
            .list(&iris_subagent_runtime::WorkerFilter::default())
            .pop()
            .unwrap();
        let result = backend
            .runtime()
            .handle()
            .wait_blocking(&snapshot.worker_id)
            .unwrap();
        assert_eq!(
            result.status,
            iris_subagent_runtime::WorkerStatus::Cancelled
        );
    }

    #[test]
    fn request_compaction_is_opt_in_and_only_schedules_the_boundary() {
        let absent = built_in_tools_for(false, false);
        assert!(absent.by_name("request_compaction").is_none());

        let tools = built_in_tools_for(false, true);
        let tool = tools.by_name("request_compaction").unwrap();
        assert!(
            built_in_tools_for(true, true)
                .by_name("request_compaction")
                .is_some()
        );
        assert_eq!(
            tool.parameters(),
            json!({ "type": "object", "properties": {}, "additionalProperties": false })
        );
        let dir = temp_dir();
        let root = root_of(&dir);
        let state = std::cell::RefCell::new(ToolState::new());
        let env = bash_env(&root, &state, None);
        let error = current_thread_runtime()
            .block_on(tool.execute(
                &json!({ "focus": "not supported" }),
                &env,
                CancellationToken::new(),
            ))
            .unwrap_err();
        assert!(error.to_string().contains("accepts no arguments"));
        assert!(
            !state
                .borrow()
                .compaction_requested
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        let output = current_thread_runtime()
            .block_on(tool.execute(&json!({}), &env, CancellationToken::new()))
            .unwrap();
        assert_eq!(
            output.content,
            "Compaction is scheduled for the next safe boundary; it has not happened yet."
        );
        assert!(
            state
                .borrow()
                .compaction_requested
                .load(std::sync::atomic::Ordering::SeqCst)
        );
    }

    #[test]
    fn bash_execute_does_not_block_the_executor() {
        // Regression for the freeze bug: on a current-thread runtime (the TUI's
        // runtime flavor) a running `bash` call must not starve the executor.
        // A concurrent 100ms timer must complete long before a `sleep 1` bash
        // call finishes -- if the tool body ran inline on the executor thread
        // the timer could not be polled until the command returned (~1s).
        let dir = temp_dir();
        let root = root_of(&dir);
        let state = std::cell::RefCell::new(ToolState::new());
        let env = bash_env(&root, &state, None);
        let args = json!({ "command": "sleep 1" });

        current_thread_runtime().block_on(async {
            let start = std::time::Instant::now();
            let tool = BashTool.execute(&args, &env, CancellationToken::new());
            let timer = async {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                start.elapsed()
            };
            let (tool_result, _timer_elapsed) = tokio::join!(tool, timer);
            let tool_elapsed = start.elapsed();

            tool_result.expect("bash tool should succeed");
            assert!(
                _timer_elapsed < std::time::Duration::from_millis(500),
                "timer was starved by bash: fired at {_timer_elapsed:?} (executor blocked)"
            );
            assert!(
                tool_elapsed >= std::time::Duration::from_millis(900),
                "sleep 1 returned too fast ({tool_elapsed:?}); test premise is wrong"
            );
        });
    }

    #[test]
    fn bash_execute_streams_deltas_while_the_command_runs() {
        // The sink must see output *before* the tool future resolves: the
        // command prints immediately, then sleeps 1s. The first delta must land
        // well within that window, proving live streaming (not a post-return
        // flush).
        let dir = temp_dir();
        let root = root_of(&dir);
        let state = std::cell::RefCell::new(ToolState::new());
        let sink = TimingSink {
            start: std::time::Instant::now(),
            first_delta: std::cell::RefCell::new(None),
        };
        let env = bash_env(&root, &state, Some(&sink));
        let args = json!({ "command": "echo start; sleep 1" });

        let tool_elapsed = current_thread_runtime().block_on(async {
            let start = std::time::Instant::now();
            BashTool
                .execute(&args, &env, CancellationToken::new())
                .await
                .expect("bash tool should succeed");
            start.elapsed()
        });

        let first = sink
            .first_delta
            .borrow()
            .expect("sink never received a live delta");
        assert!(
            first < std::time::Duration::from_millis(500),
            "first delta arrived too late ({first:?}); output was not streamed live"
        );
        assert!(
            tool_elapsed >= std::time::Duration::from_millis(900),
            "command returned before its sleep completed ({tool_elapsed:?})"
        );
    }

    #[test]
    fn bash_execute_completes_promptly_with_no_output() {
        // A command that emits nothing drops the chunk sender almost immediately,
        // so `chunk_rx.recv()` yields `None` before the join handle is ready. The
        // select loop must disable that branch (`chunks_open`) and fall through to
        // the join instead of busy-spinning on the closed receiver on the
        // current-thread runtime. Guard with a timeout: a spinning loop still
        // eventually joins, but the command itself is instant, so a generous
        // bound catches a hang without being flaky on the happy path.
        let dir = temp_dir();
        let root = root_of(&dir);
        let state = std::cell::RefCell::new(ToolState::new());
        let env = bash_env(&root, &state, None);
        let args = json!({ "command": "true" });

        current_thread_runtime().block_on(async {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                BashTool.execute(&args, &env, CancellationToken::new()),
            )
            .await
            .expect("no-output bash command hung (loop spun on the closed channel)");
            result.expect("bash tool should succeed");
        });
    }

    #[test]
    fn bash_execute_preserves_sessions_across_calls() {
        // The shared `Arc<Mutex<BashState>>` must carry persistent-session state
        // across `execute` calls the same way the old in-place `&mut` did: a
        // `cd` in one call is visible to a later `pwd` in the same session.
        let dir = temp_dir();
        let root = root_of(&dir);
        std::fs::create_dir(root.join("sub")).unwrap();
        let state = std::cell::RefCell::new(ToolState::new());
        let env = bash_env(&root, &state, None);
        let runtime = current_thread_runtime();

        runtime
            .block_on(BashTool.execute(
                &json!({ "command": "cd sub", "session": "s1" }),
                &env,
                CancellationToken::new(),
            ))
            .unwrap();
        let pwd = runtime
            .block_on(BashTool.execute(
                &json!({ "command": "pwd", "session": "s1" }),
                &env,
                CancellationToken::new(),
            ))
            .unwrap();
        assert!(
            pwd.content.trim_end().ends_with("/sub"),
            "session state lost across calls: {}",
            pwd.content
        );
    }

    /// Recursively check a schema (sub)tree for constructs outside the
    /// provider-safe subset: a top-level combinator keyword or a `$ref`/
    /// `$defs`/`definitions` anywhere. `at_top` is true only for the schema
    /// object's own top level -- combinators are only checked there (a nested
    /// property is free to use them; providers reject only the top-level
    /// combinator), while the ref/defs ban applies everywhere in the tree.
    fn find_schema_violation(value: &Value, at_top: bool) -> Option<String> {
        if let Value::Object(map) = value {
            if at_top {
                for combinator in ["oneOf", "anyOf", "allOf", "not", "if", "then", "else"] {
                    if map.contains_key(combinator) {
                        return Some(format!("top-level `{combinator}`"));
                    }
                }
            }
            for banned in ["$ref", "$defs", "definitions"] {
                if map.contains_key(banned) {
                    return Some(format!("`{banned}` anywhere in the schema tree"));
                }
            }
            for (key, child) in map {
                // Only the schema object's own top level is "top" for the
                // combinator check; every nested value (including each
                // property's schema) is not.
                let _ = key;
                if let Some(found) = find_schema_violation(child, false) {
                    return Some(found);
                }
            }
        } else if let Value::Array(items) = value {
            for item in items {
                if let Some(found) = find_schema_violation(item, false) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// Assert one tool's `parameters()` schema stays in the subset every
    /// configured provider (Anthropic, OpenAI/Codex) accepts as a tool
    /// `input_schema`: object-typed at the top level, no top-level combinator
    /// keyword (`oneOf`/`anyOf`/`allOf`/`not`/`if`/`then`/`else` -- Anthropic
    /// rejects these with a 400 on the whole request), and no `$ref`/`$defs`/
    /// `definitions` anywhere (this codebase never needs them; a stray one
    /// would signal an accidental schema-generation dependency). Deliberately
    /// does NOT enforce the fuller issue-#475 subset (e.g. no `minLength`/
    /// `minimum`): existing tools legitimately use those and Anthropic accepts
    /// them.
    fn assert_provider_safe_schema(tool_name: &str, params: &Value) {
        assert_eq!(
            params.get("type"),
            Some(&json!("object")),
            "{tool_name}: parameters() must be top-level type:object, got {params}"
        );
        if let Some(violation) = find_schema_violation(params, true) {
            panic!("{tool_name}: parameters() schema contains {violation}: {params}");
        }
    }

    #[test]
    fn fixed_context_contract_budget() {
        let subagent_dir = temp_dir();
        let workspace = root_of(&subagent_dir);
        let backend = Arc::new(
            crate::wayland::subagents::SubagentBackend::open(
                workspace.clone(),
                &workspace.join("worker-state-budget"),
                workspace.join("worktrees-budget"),
            )
            .unwrap(),
        );
        let subagents = test_subagent_config(
            backend,
            Arc::new(|_| Err(anyhow!("budget test must not execute a provider"))),
            Vec::new(),
            "fixed-context-budget",
        );
        let web = WebToolsConfig {
            web_search: Some(web::SearchBackend::Native),
            read_web_page: Some(web::ReadBackend::Native),
            ..WebToolsConfig::default()
        };
        let registries = vec![
            ("default", built_in_tools()),
            ("bash", built_in_tools_for(true, false)),
            ("compaction", built_in_tools_for(false, true)),
            (
                "web",
                built_in_tools_with(&ToolsConfig {
                    web: web.clone(),
                    ..ToolsConfig::default()
                }),
            ),
            (
                "subagents",
                built_in_tools_with(&ToolsConfig {
                    subagents: Some(subagents.clone()),
                    ..ToolsConfig::default()
                }),
            ),
            (
                "maximal",
                built_in_tools_with(&ToolsConfig {
                    model_compaction_tool: true,
                    web,
                    subagents: Some(subagents),
                    ..ToolsConfig::default()
                }),
            ),
        ];

        for (name, tools) in registries {
            let prompt = crate::wayland::system_prompt::assemble_defaults_at(
                Path::new("/workspace/project"),
                &tools,
                "2026-07-15",
            );
            let usage = crate::print::UsageBase::estimate(&prompt, &tools);
            let declarations = tools
                .iter()
                .map(|tool| {
                    let json = serde_json::to_string(&serde_json::json!({
                        "name": tool.name(),
                        "description": tool.description(),
                        "input_schema": tool.parameters(),
                    }))
                    .unwrap();
                    (tool.name(), json.len())
                })
                .collect::<Vec<_>>();
            let declaration_bytes = declarations.iter().map(|(_, bytes)| bytes).sum::<usize>();
            eprintln!(
                "CONTEXT_BUDGET {name} tools={} system_bytes={} system_tokens={} declaration_bytes={} declaration_tokens={} base_bytes={} base_tokens={}",
                tools.iter().count(),
                prompt.len(),
                usage.system_prompt_tokens,
                declaration_bytes,
                usage.tools_total_tokens,
                prompt.len() + declaration_bytes,
                usage.base_total_tokens,
            );
            // Explicit profile ceilings leave small room for harmless JSON
            // serialization drift while rejecting a lost fixed-context saving.
            let (
                system_bytes,
                declarations_bytes,
                base_bytes,
                system_tokens,
                declarations_tokens,
                base_tokens,
            ) = match name {
                "default" => (7_525, 10_300, 17_900, 1_890, 2_580, 4_500),
                "bash" => (7_450, 6_300, 13_850, 1_870, 1_580, 3_475),
                "compaction" => (7_550, 10_550, 18_100, 1_895, 2_650, 4_550),
                "web" => (7_675, 12_050, 19_800, 1_930, 3_025, 5_000),
                "subagents" => (8_575, 16_100, 24_800, 2_150, 4_050, 6_250),
                "maximal" => (8_725, 18_100, 26_900, 2_190, 4_550, 6_775),
                _ => unreachable!("unknown registry budget"),
            };
            assert!(prompt.len() <= system_bytes, "{name} system bytes regrew");
            assert!(
                declaration_bytes <= declarations_bytes,
                "{name} declaration bytes regrew"
            );
            assert!(
                prompt.len() + declaration_bytes <= base_bytes,
                "{name} combined fixed bytes regrew"
            );
            assert!(
                usage.system_prompt_tokens <= system_tokens,
                "{name} system tokens regrew"
            );
            assert!(
                usage.tools_total_tokens <= declarations_tokens,
                "{name} declaration tokens regrew"
            );
            assert!(
                usage.base_total_tokens <= base_tokens,
                "{name} combined fixed base regrew"
            );
            if name == "maximal" {
                for ((tool_name, bytes), tool_usage) in declarations.iter().zip(&usage.tools) {
                    eprintln!(
                        "CONTEXT_TOOL {tool_name} bytes={bytes} tokens={}",
                        tool_usage.schema_tokens
                    );
                    let (max_bytes, max_tokens) = match *tool_name {
                        "read" => (750, 188),
                        "bash" => (1_025, 257),
                        "edit" => (690, 173),
                        "write" => (380, 95),
                        "grep" => (1_330, 333),
                        "find" => (590, 148),
                        "ls" => (980, 245),
                        "AskUserQuestion" => (1_200, 300),
                        "get_goal" => (210, 53),
                        "create_goal" => (960, 240),
                        "update_goal" => (360, 90),
                        "read_output" => (560, 140),
                        "recall" => (1_320, 330),
                        "request_compaction" => (230, 58),
                        "web_search" => (1_130, 283),
                        "read_web_page" => (620, 155),
                        // Budget covers the fixed structure only; the dynamic
                        // model/provider enums are authenticated-catalog content
                        // (empty here), like web keys, and are not budgeted.
                        "spawn_subagent" => (2_650, 665),
                        "subagent_status" => (435, 109),
                        "read_subagent_output" => (550, 138),
                        "cancel_subagent" => (430, 108),
                        "plan_subagent_apply" => (400, 100),
                        "apply_subagent" => (960, 240),
                        _ => panic!("unbudgeted tool: {tool_name}"),
                    };
                    assert!(
                        *bytes <= max_bytes,
                        "{tool_name} declaration regrew: {bytes} > {max_bytes} bytes"
                    );
                    assert!(
                        tool_usage.schema_tokens <= max_tokens,
                        "{tool_name} declaration regrew: {} > {max_tokens} tokens",
                        tool_usage.schema_tokens
                    );
                }
            }
        }
    }

    #[test]
    fn all_tool_schemas_stay_in_provider_safe_subset() {
        // Keep this scratch root alive until after every registry drops its
        // scheduler-backed subagent tools.
        let subagent_dir = temp_dir();
        let subagent_workspace = root_of(&subagent_dir);
        // Every registry configuration the CLI can build, plus the
        // test-only-injectable `read_output` tool: PR #593 added a top-level
        // `oneOf` to exactly one tool (`recall`) and it was invisible to every
        // test that only exercised OpenAI/Codex, because Anthropic is the only
        // provider that rejects it. This test walks every declared tool across
        // every configuration so a future combinator regression on ANY tool
        // fails here regardless of which provider it would break.
        let mut registries: Vec<(&str, Tools)> = vec![
            ("built_in_tools", built_in_tools()),
            (
                "built_in_tools_for(false, false)",
                built_in_tools_for(false, false),
            ),
            (
                "built_in_tools_for(true, false)",
                built_in_tools_for(true, false),
            ),
            (
                "built_in_tools_for(false, true)",
                built_in_tools_for(false, true),
            ),
            (
                "built_in_tools_for(true, true)",
                built_in_tools_for(true, true),
            ),
        ];
        // Both web tools are opt-in and otherwise unregistered (no prompt
        // bloat when off, see `built_in_tools_with`), so cover them under a
        // config with both backends selected.
        let web_config = ToolsConfig {
            bash_tool_mode: false,
            model_compaction_tool: false,
            web: WebToolsConfig {
                web_search: Some(web::SearchBackend::Native),
                read_web_page: Some(web::ReadBackend::Native),
                ..WebToolsConfig::default()
            },
            subagents: None,
        };
        registries.push((
            "built_in_tools_with(web enabled)",
            built_in_tools_with(&web_config),
        ));

        let backend = Arc::new(
            crate::wayland::subagents::SubagentBackend::open(
                subagent_workspace.clone(),
                &subagent_workspace.join("worker-state"),
                subagent_workspace.join("worktrees"),
            )
            .unwrap(),
        );
        let provider_factory: crate::wayland::subagents::ChildProviderFactory =
            Arc::new(|_| Err(anyhow!("schema test must not execute a provider")));
        let subagent_config = ToolsConfig {
            subagents: Some(test_subagent_config(
                backend,
                provider_factory,
                Vec::new(),
                "schema-test",
            )),
            ..ToolsConfig::default()
        };
        let subagent_tools = built_in_tools_with(&subagent_config);
        let names = subagent_tools
            .iter()
            .map(|tool| tool.name())
            .collect::<Vec<_>>();
        for expected in [
            "spawn_subagent",
            "subagent_status",
            "read_subagent_output",
            "cancel_subagent",
            "plan_subagent_apply",
            "apply_subagent",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
        assert!(!names.contains(&"select_subagent_candidate"));
        for (name, needles) in [
            (
                "spawn_subagent",
                &["one delegated worker", "isolated", "Subagent types"][..],
            ),
            ("subagent_status", &["one worker"][..]),
            (
                "read_subagent_output",
                &["UTF-8", "artifact", "terminal"][..],
            ),
            (
                "cancel_subagent",
                &["one worker", "hard-abort", "grace"][..],
            ),
            (
                "plan_subagent_apply",
                &["immutable", "digest", "unchanged"][..],
            ),
            (
                "apply_subagent",
                &[
                    "revalidation",
                    "approval-gated",
                    "dirty",
                    "base-drifted",
                    "escaping-symlink",
                ][..],
            ),
        ] {
            let description = subagent_tools.by_name(name).unwrap().description();
            for needle in needles {
                assert!(
                    description.contains(needle),
                    "{name} missing {needle}: {description}"
                );
            }
        }
        assert!(
            !subagent_tools
                .by_name("plan_subagent_apply")
                .unwrap()
                .description()
                .contains("Select group")
        );
        let spawn = subagent_tools.by_name("spawn_subagent").unwrap();
        let spawn_schema = spawn.parameters();
        assert!(
            spawn
                .diff_preview(Path::new("/ws"), &json!({ "task": "inspect" }))
                .is_none(),
            "spawn_subagent must not provide an EDIT diff preview"
        );
        let properties = spawn_schema["properties"].as_object().unwrap();
        let expected_fields = [
            "task",
            "subagent_type",
            "model",
            "effort",
            "tools",
            "system_prompt",
            "description",
            "background",
            "isolation",
            "cwd",
        ];
        assert_eq!(properties.len(), expected_fields.len());
        for field in expected_fields {
            assert!(properties.contains_key(field), "missing {field}");
            assert!(
                properties[field]["description"].is_string(),
                "spawn_subagent.{field} needs call-time guidance"
            );
        }
        for removed in [
            "prompt",
            "kind",
            "provider",
            "capability",
            "count",
            "max_provider_rounds",
            "max_tool_rounds",
            "max_tokens",
            "allow_outside_workspace",
            "resume_from",
            "profile",
        ] {
            assert!(
                !properties.contains_key(removed),
                "removed field {removed} is still advertised"
            );
        }
        assert_eq!(spawn_schema["required"], json!(["task"]));
        assert_eq!(properties["subagent_type"]["default"], "general");
        assert_eq!(properties["background"]["default"], true);
        assert_eq!(
            properties["subagent_type"]["enum"],
            json!(["general", "explore", "review"])
        );
        let tool_tokens = properties["tools"]["items"]["enum"].as_array().unwrap();
        for shorthand in ["read_only", "read_write", "shell", "all"] {
            assert!(tool_tokens.contains(&json!(shorthand)));
        }
        let tools_description = properties["tools"]["description"].as_str().unwrap();
        assert!(tools_description.contains("replaces"));
        assert!(tools_description.contains("clamped"));
        assert!(!tools_description.contains("only narrows"));

        for tool_name in ["subagent_status", "cancel_subagent"] {
            let schema = subagent_tools.by_name(tool_name).unwrap().parameters();
            assert_eq!(schema["required"], json!(["worker_id"]));
            assert_eq!(schema["properties"].as_object().unwrap().len(), 1);
            assert_eq!(schema["properties"]["worker_id"]["minLength"], 1);
            assert!(schema["properties"].get("group_id").is_none());
        }
        for (tool_name, field) in [
            ("plan_subagent_apply", "worker_id"),
            ("apply_subagent", "plan_id"),
        ] {
            let schema = subagent_tools.by_name(tool_name).unwrap().parameters();
            assert_eq!(
                schema["properties"][field]["minLength"], 1,
                "{tool_name}.{field} must reject empty IDs"
            );
        }
        let apply_schema = subagent_tools
            .by_name("apply_subagent")
            .unwrap()
            .parameters();
        for (field, needle) in [
            ("approved_overwrites", "base-drifted"),
            ("approved_escaping_symlinks", "escape"),
            ("skipped_paths", "overrides approvals"),
        ] {
            assert_eq!(
                apply_schema["properties"][field]["default"],
                json!([]),
                "missing apply_subagent.{field} default"
            );
            assert!(
                apply_schema["properties"][field]["description"]
                    .as_str()
                    .unwrap()
                    .contains(needle),
                "apply_subagent.{field} missing {needle}"
            );
        }
        let artifact_schema = subagent_tools
            .by_name("read_subagent_output")
            .unwrap()
            .parameters();
        assert_eq!(artifact_schema["properties"]["artifact_id"]["minLength"], 1);
        assert_eq!(artifact_schema["properties"]["offset"]["default"], 0);
        assert_eq!(artifact_schema["properties"]["limit"]["default"], 16_000);
        assert_eq!(artifact_schema["properties"]["limit"]["maximum"], 50_000);

        let state = std::cell::RefCell::new(ToolState::new());
        let env = bash_env(&subagent_workspace, &state, None);
        for (args, needle) in [
            (
                json!({ "task": "invalid", "subagent_type": "missing" }),
                "unknown subagent_type",
            ),
            (
                json!({ "task": "invalid", "tools": ["not-a-tool"] }),
                "unknown delegated tool",
            ),
            (
                json!({ "task": "invalid", "tools": ["write"], "isolation": "none" }),
                "worktree isolation",
            ),
            (
                json!({ "task": "invalid", "tools": ["read"], "isolation": "worktree", "cwd": "." }),
                "mutually exclusive",
            ),
        ] {
            let error = current_thread_runtime()
                .block_on(spawn.execute(&args, &env, CancellationToken::new()))
                .unwrap_err();
            assert!(
                error.to_string().contains(needle),
                "unexpected validation error for {args}: {error:#}"
            );
        }
        let worker_id = iris_subagent_runtime::WorkerId::new().to_string();
        for tool_name in ["subagent_status", "cancel_subagent"] {
            let tool = subagent_tools.by_name(tool_name).unwrap();
            let missing = current_thread_runtime()
                .block_on(tool.execute(&json!({}), &env, CancellationToken::new()))
                .unwrap_err();
            assert!(missing.to_string().contains("requires worker_id"));
            let not_found = current_thread_runtime()
                .block_on(tool.execute(
                    &json!({ "worker_id": worker_id }),
                    &env,
                    CancellationToken::new(),
                ))
                .unwrap_err();
            assert!(
                not_found.to_string().contains("worker not found"),
                "unexpected {tool_name} error: {not_found:#}"
            );
        }

        let failed = current_thread_runtime()
            .block_on(spawn.execute(
                &json!({
                    "task": "must fail before inference",
                    "tools": ["read_only"],
                    "background": false
                }),
                &env,
                CancellationToken::new(),
            ))
            .unwrap();
        let failed: iris_subagent_runtime::WorkerResult =
            serde_json::from_str(&failed.content).unwrap();
        assert_eq!(failed.status, iris_subagent_runtime::WorkerStatus::Failed);
        assert!(!subagent_workspace.join("result.txt").exists());
        registries.push(("built_in_tools_with(subagents)", subagent_tools));

        let mut checked_any = false;
        for (config_name, tools) in &registries {
            for tool in tools.iter() {
                checked_any = true;
                assert_provider_safe_schema(
                    &format!("{config_name} -> {}", tool.name()),
                    &tool.parameters(),
                );
            }
        }
        assert!(checked_any, "no tools were checked; test setup is broken");

        // `read_output` is only reachable in the built-in sets above through
        // `ReadOutputTool`, but exercise the dedicated test constructor too so
        // this test stays the canonical place a new caller of that seam is
        // guarded.
        let read_output = read_output_tool();
        assert_provider_safe_schema(
            &format!("read_output_tool() -> {}", read_output.name()),
            &read_output.parameters(),
        );
    }
}
