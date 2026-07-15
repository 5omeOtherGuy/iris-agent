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
    pub(crate) capability_ceiling: iris_subagent_runtime::CapabilityMode,
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
        tools.push(Box::new(SpawnSubagentTool(config.clone())));
        tools.push(Box::new(SubagentStatusTool(config.backend.clone())));
        tools.push(Box::new(SubagentArtifactTool(config.backend.clone())));
        tools.push(Box::new(CancelSubagentTool(config.backend.clone())));
        tools.push(Box::new(SelectSubagentCandidateTool(
            config.backend.clone(),
        )));
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

struct SpawnSubagentTool(SubagentToolsConfig);

impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        "spawn_subagent"
    }

    fn description(&self) -> &str {
        "Spawn an independently scheduled subagent. Mutation-capable workers always run in a managed isolated worktree."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "Complete delegated instruction." },
                "description": { "type": "string", "description": "Short worker label." },
                "kind": { "type": "string", "enum": ["general", "explore", "review"] },
                "capability": { "type": "string", "enum": ["read_only", "read_write", "execute", "all"] },
                "isolation": { "type": "string", "enum": ["none", "worktree"] },
                "tools": { "type": "array", "items": { "type": "string" } },
                "cwd": { "type": "string", "description": "Optional directory inside the parent workspace; incompatible with worktree isolation." },
                "allow_outside_workspace": { "type": "boolean", "description": "Explicitly allow read-only filesystem tools to follow paths outside the effective workspace. Mutation remains confined. Defaults to false." },
                "background": { "type": "boolean", "description": "Return immediately when true (default); otherwise wait for the durable result." },
                "resume_from": { "type": "string", "description": "Optional prior worker ID used as the explicit recovery source." },
                "max_provider_rounds": { "type": "integer", "minimum": 1 },
                "max_tool_rounds": { "type": "integer", "minimum": 1 },
                "max_tokens": { "type": "integer", "minimum": 1 },
                "count": { "type": "integer", "minimum": 1, "maximum": 8, "description": "Independent candidates for explicit best-of-N selection." }
            },
            "required": ["prompt"],
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
            let prompt = args
                .get("prompt")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| anyhow!("spawn_subagent requires a non-empty prompt"))?;
            let kind = args
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("general");
            let requested = match args
                .get("capability")
                .and_then(Value::as_str)
                .unwrap_or("read_only")
            {
                "read_only" => iris_subagent_runtime::CapabilityMode::ReadOnly,
                "read_write" => iris_subagent_runtime::CapabilityMode::ReadWrite,
                "execute" => iris_subagent_runtime::CapabilityMode::Execute,
                "all" => iris_subagent_runtime::CapabilityMode::All,
                other => return Err(anyhow!("unsupported subagent capability: {other}")),
            };
            let kind_ceiling = match kind {
                "general" => self.0.capability_ceiling,
                "explore" | "review" => iris_subagent_runtime::CapabilityMode::ReadOnly,
                other => return Err(anyhow!("unknown subagent kind: {other}")),
            };
            let mut request = iris_subagent_runtime::WorkerRequest::read_only(prompt);
            request.description = args
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or(kind)
                .to_string();
            request.kind = match kind {
                "explore" => iris_subagent_runtime::WorkerKind::Explore,
                "review" => iris_subagent_runtime::WorkerKind::Review,
                _ => iris_subagent_runtime::WorkerKind::General,
            };
            request.policy.capability = requested;
            request.policy.parent_capability = kind_ceiling;
            request.policy.isolation = match args.get("isolation").and_then(Value::as_str) {
                Some("none") => iris_subagent_runtime::IsolationMode::None,
                Some("worktree") => iris_subagent_runtime::IsolationMode::Worktree,
                Some(other) => return Err(anyhow!("unsupported subagent isolation: {other}")),
                None if requested.is_mutation_capable() => {
                    iris_subagent_runtime::IsolationMode::Worktree
                }
                None => iris_subagent_runtime::IsolationMode::None,
            };
            request.policy.cwd = args.get("cwd").and_then(Value::as_str).map(PathBuf::from);
            request.policy.tool_allowlist = args
                .get("tools")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            request.policy.allow_outside_workspace = args
                .get("allow_outside_workspace")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            request.policy.nesting_depth = self.0.nesting_depth.saturating_add(1);
            request.policy.max_nesting_depth = self.0.max_nesting_depth;
            request.session_id = Some(self.0.session_id.clone());
            request.resume_from = args
                .get("resume_from")
                .and_then(Value::as_str)
                .map(str::parse)
                .transpose()?;
            request.budgets.max_provider_rounds =
                args.get("max_provider_rounds").and_then(Value::as_u64);
            request.budgets.max_tool_rounds = args.get("max_tool_rounds").and_then(Value::as_u64);
            request.budgets.max_tokens = args.get("max_tokens").and_then(Value::as_u64);
            let count = args.get("count").and_then(Value::as_u64).unwrap_or(1);
            if !(1..=8).contains(&count) {
                return Err(anyhow!("subagent count must be between 1 and 8"));
            }
            let background = args
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            if count == 1 {
                let id = self.0.backend.spawn(
                    self.0.provider_factory.clone(),
                    request,
                    self.0.approval.clone(),
                )?;
                if background {
                    Ok(ToolOutput::text(
                        serde_json::json!({ "worker_id": id, "status": "queued" }).to_string(),
                    ))
                } else {
                    let wait = self.0.backend.runtime().handle().wait(&id);
                    tokio::pin!(wait);
                    let result = tokio::select! {
                        result = &mut wait => result?,
                        _ = cancel.cancelled() => {
                            self.0.backend.cancel(&id)?;
                            return Err(anyhow!("foreground subagent cancelled"));
                        }
                    };
                    Ok(ToolOutput::text(serde_json::to_string(&result)?))
                }
            } else {
                let group_id = self.0.backend.spawn_group(
                    self.0.provider_factory.clone(),
                    vec![request; count as usize],
                    self.0.approval.clone(),
                )?;
                if background {
                    let group = self.0.backend.poll_group(&group_id)?;
                    Ok(ToolOutput::text(
                        serde_json::json!({
                            "group_id": group_id,
                            "worker_ids": group.workers,
                            "status": "queued"
                        })
                        .to_string(),
                    ))
                } else {
                    let wait = self.0.backend.runtime().handle().wait_group(&group_id);
                    tokio::pin!(wait);
                    let result = tokio::select! {
                        result = &mut wait => result?,
                        _ = cancel.cancelled() => {
                            self.0.backend.cancel_group(&group_id)?;
                            return Err(anyhow!("foreground subagent group cancelled"));
                        }
                    };
                    Ok(ToolOutput::text(serde_json::to_string(&result)?))
                }
            }
        })
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn supports_allow_always(&self) -> bool {
        false
    }

    fn diff_preview(&self, _workspace: &Path, args: &Value) -> Option<String> {
        Some(format!(
            "Spawn {} subagent candidate(s) with capability={} and isolation={}",
            args.get("count").and_then(Value::as_u64).unwrap_or(1),
            args.get("capability")
                .and_then(Value::as_str)
                .unwrap_or("read_only"),
            args.get("isolation")
                .and_then(Value::as_str)
                .unwrap_or("automatic")
        ))
    }
}

struct SubagentStatusTool(Arc<crate::wayland::subagents::SubagentBackend>);

impl Tool for SubagentStatusTool {
    fn name(&self) -> &str {
        "subagent_status"
    }
    fn description(&self) -> &str {
        "Poll a delegated worker without consuming its result."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "worker_id": { "type": "string" },
                "group_id": { "type": "string" }
            },
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
            let value = if let Some(id) = parse_optional_worker_id(args)? {
                serde_json::to_value(self.0.poll(&id)?)?
            } else if let Some(id) = parse_optional_group_id(args)? {
                serde_json::to_value(self.0.poll_group(&id)?)?
            } else {
                return Err(anyhow!("subagent_status requires worker_id or group_id"));
            };
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
        "Read a UTF-8 page from an oversized subagent output artifact."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "artifact_id": { "type": "string" },
                "offset": { "type": "integer", "minimum": 0 },
                "limit": { "type": "integer", "minimum": 1, "maximum": 50000 }
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
        "Cancel a delegated worker cooperatively, with bounded hard abort."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "worker_id": { "type": "string" },
                "group_id": { "type": "string" }
            },
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
            let value = if let Some(id) = parse_optional_worker_id(args)? {
                serde_json::to_value(self.0.cancel(&id)?)?
            } else if let Some(id) = parse_optional_group_id(args)? {
                serde_json::to_value(self.0.cancel_group(&id)?)?
            } else {
                return Err(anyhow!("cancel_subagent requires worker_id or group_id"));
            };
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
        "Build an immutable reviewed apply plan for a completed mutable worker. This does not mutate the parent workspace."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": { "worker_id": { "type": "string" } },
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
        "Apply an immutable reviewed worker plan to the parent filesystem. Dirty/base-drifted and escaping-symlink paths require explicit per-path approvals."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "plan_id": { "type": "string" },
                "approved_overwrites": { "type": "array", "items": { "type": "string" } },
                "approved_escaping_symlinks": { "type": "array", "items": { "type": "string" } },
                "skipped_paths": { "type": "array", "items": { "type": "string" } }
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

struct SelectSubagentCandidateTool(Arc<crate::wayland::subagents::SubagentBackend>);

impl Tool for SelectSubagentCandidateTool {
    fn name(&self) -> &str {
        "select_subagent_candidate"
    }
    fn description(&self) -> &str {
        "Explicitly select one successful member of a completed best-of-N group. Selection does not apply files."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "group_id": { "type": "string" },
                "worker_id": { "type": "string" }
            },
            "required": ["group_id", "worker_id"],
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
            let group_id =
                parse_optional_group_id(args)?.ok_or_else(|| anyhow!("group_id is required"))?;
            let worker_id =
                parse_optional_worker_id(args)?.ok_or_else(|| anyhow!("worker_id is required"))?;
            let group = self.0.poll_group(&group_id)?;
            if !group.workers.contains(&worker_id) {
                return Err(anyhow!("selected worker is not a member of the group"));
            }
            if group
                .snapshots
                .iter()
                .any(|snapshot| !snapshot.status.is_terminal())
            {
                return Err(anyhow!(
                    "all group candidates must be terminal before selection"
                ));
            }
            let selected = group
                .snapshots
                .into_iter()
                .find(|snapshot| snapshot.worker_id == worker_id)
                .and_then(|snapshot| snapshot.result)
                .ok_or_else(|| anyhow!("selected candidate has no terminal result"))?;
            if selected.status != iris_subagent_runtime::WorkerStatus::Completed {
                return Err(anyhow!("selected candidate did not complete successfully"));
            }
            if let Some(worktree) = &selected.worktree {
                self.0.select_worktree_candidate(&worktree.id)?;
            }
            Ok(ToolOutput::text(serde_json::to_string(&selected)?))
        })
    }
}

fn parse_optional_worker_id(args: &Value) -> Result<Option<iris_subagent_runtime::WorkerId>> {
    args.get("worker_id")
        .and_then(Value::as_str)
        .map(str::parse)
        .transpose()
        .map_err(Into::into)
}

fn parse_optional_group_id(args: &Value) -> Result<Option<iris_subagent_runtime::GroupId>> {
    args.get("group_id")
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
            Arc::new(|| Ok(Box::new(PendingProvider(Rc::new(()))) as Box<dyn ChatProvider>));
        let tool = SpawnSubagentTool(SubagentToolsConfig {
            backend: backend.clone(),
            provider_factory,
            capability_ceiling: iris_subagent_runtime::CapabilityMode::All,
            session_id: "foreground-cancel".to_string(),
            nesting_depth: 0,
            max_nesting_depth: 2,
            approval: None,
        });
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
                &json!({ "prompt": "wait", "background": false }),
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
            Arc::new(|| Err(anyhow!("schema test must not execute a provider")));
        let subagent_config = ToolsConfig {
            subagents: Some(SubagentToolsConfig {
                backend,
                provider_factory,
                capability_ceiling: iris_subagent_runtime::CapabilityMode::All,
                session_id: "schema-test".to_string(),
                nesting_depth: 0,
                max_nesting_depth: 2,
                approval: None,
            }),
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
            "select_subagent_candidate",
            "plan_subagent_apply",
            "apply_subagent",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
        assert!(
            names
                .iter()
                .filter(|name| name.contains("subagent"))
                .all(|name| !name.contains("task"))
        );
        let spawn_schema = subagent_tools
            .by_name("spawn_subagent")
            .unwrap()
            .parameters();
        let properties = spawn_schema["properties"].as_object().unwrap();
        for required_field in [
            "prompt",
            "description",
            "kind",
            "capability",
            "isolation",
            "cwd",
            "background",
            "resume_from",
        ] {
            assert!(
                properties.contains_key(required_field),
                "missing {required_field}"
            );
        }
        let state = std::cell::RefCell::new(ToolState::new());
        let env = bash_env(&subagent_workspace, &state, None);
        let spawn = subagent_tools.by_name("spawn_subagent").unwrap();
        for invalid in [
            json!({
                "prompt": "invalid",
                "kind": "review",
                "capability": "read_write"
            }),
            json!({
                "prompt": "invalid",
                "capability": "read_only",
                "isolation": "worktree",
                "cwd": "."
            }),
            json!({
                "prompt": "invalid",
                "capability": "read_write",
                "isolation": "none"
            }),
        ] {
            let error = current_thread_runtime()
                .block_on(spawn.execute(&invalid, &env, CancellationToken::new()))
                .unwrap_err();
            assert!(
                error.to_string().contains("worker")
                    || error.to_string().contains("worktree")
                    || error.to_string().contains("capability"),
                "unexpected validation error: {error:#}"
            );
        }
        let failed = current_thread_runtime()
            .block_on(spawn.execute(
                &json!({
                    "prompt": "must fail before inference",
                    "capability": "read_write",
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
