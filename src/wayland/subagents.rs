//! Wayland-owned subagent backend contract and read-only MVP (#460).
#![allow(dead_code)]
//!
//! The backend owns child orchestration and execution-environment state. Nexus
//! remains the provider/tool loop: a child is a fresh bare [`Agent`] wrapped with
//! a read-only tool registry and an independent [`ToolState`].

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ChatProvider,
    Message, ReviewContext, Role, ToolCall,
};
use crate::tools::{ToolState, built_in_tools};
use crate::wayland::Harness;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub(crate) struct SubagentId(String);

impl SubagentId {
    fn new(seq: u64) -> Self {
        Self(format!("subagent_{seq:08x}"))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubagentCapabilityMode {
    ReadOnly,
    ReadWrite,
    Execute,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubagentIsolation {
    None,
    Worktree,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SubagentBudgets {
    pub(crate) max_tool_roundtrips: Option<usize>,
    pub(crate) max_output_bytes: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SubagentRequest {
    pub(crate) prompt: String,
    pub(crate) capability_mode: SubagentCapabilityMode,
    pub(crate) isolation: Option<SubagentIsolation>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) tool_allowlist: Option<Vec<String>>,
    pub(crate) budgets: SubagentBudgets,
}

impl SubagentRequest {
    pub(crate) fn read_only(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            capability_mode: SubagentCapabilityMode::ReadOnly,
            isolation: Some(SubagentIsolation::None),
            cwd: None,
            tool_allowlist: None,
            budgets: SubagentBudgets::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SubagentStatus {
    Started,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl SubagentStatus {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SubagentHandle {
    pub(crate) id: SubagentId,
    pub(crate) status: SubagentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SubagentOutputHandle {
    pub(crate) id: String,
    pub(crate) bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SubagentLifecycleEvent {
    pub(crate) worker_id: SubagentId,
    pub(crate) status: SubagentStatus,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SubagentResult {
    pub(crate) worker_id: SubagentId,
    pub(crate) status: SubagentStatus,
    pub(crate) summary: String,
    pub(crate) output_handles: Vec<SubagentOutputHandle>,
    pub(crate) events: Vec<SubagentLifecycleEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SubagentSnapshot {
    pub(crate) handle: SubagentHandle,
    pub(crate) result: Option<SubagentResult>,
    pub(crate) events: Vec<SubagentLifecycleEvent>,
}

pub(crate) struct SubagentBackend<P> {
    workspace: PathBuf,
    next_seq: Cell<u64>,
    workers: RefCell<BTreeMap<SubagentId, Worker<P>>>,
}

struct Worker<P> {
    request: SubagentRequest,
    workspace: PathBuf,
    provider: Option<P>,
    token: CancellationToken,
    status: SubagentStatus,
    result: Option<SubagentResult>,
    events: Vec<SubagentLifecycleEvent>,
}

impl<P: ChatProvider> SubagentBackend<P> {
    pub(crate) fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            next_seq: Cell::new(0),
            workers: RefCell::new(BTreeMap::new()),
        }
    }

    pub(crate) fn spawn(&self, provider: P, request: SubagentRequest) -> Result<SubagentHandle> {
        validate_request(&request)?;
        let workspace = match &request.cwd {
            Some(cwd) => validate_cwd(&self.workspace, cwd)?,
            None => self.workspace.clone(),
        };
        let id = SubagentId::new(self.next_seq.get());
        self.next_seq.set(self.next_seq.get() + 1);
        let mut worker = Worker {
            request,
            workspace,
            provider: Some(provider),
            token: CancellationToken::new(),
            status: SubagentStatus::Started,
            result: None,
            events: Vec::new(),
        };
        worker.push_event(&id, SubagentStatus::Started, None);
        worker.status = SubagentStatus::Running;
        worker.push_event(&id, SubagentStatus::Running, None);
        let handle = SubagentHandle {
            id: id.clone(),
            status: worker.status,
        };
        self.workers.borrow_mut().insert(id, worker);
        Ok(handle)
    }

    pub(crate) fn poll(&self, id: &SubagentId) -> Result<SubagentSnapshot> {
        let workers = self.workers.borrow();
        let worker = workers
            .get(id)
            .with_context(|| format!("unknown subagent: {}", id.as_str()))?;
        Ok(worker.snapshot(id))
    }

    pub(crate) fn cancel(&self, id: &SubagentId) -> Result<SubagentSnapshot> {
        let mut workers = self.workers.borrow_mut();
        let worker = workers
            .get_mut(id)
            .with_context(|| format!("unknown subagent: {}", id.as_str()))?;
        worker.token.cancel();
        if !worker.status.is_terminal() {
            worker.status = SubagentStatus::Cancelled;
            worker.push_event(id, SubagentStatus::Cancelled, Some("cancelled".to_string()));
            worker.result = Some(worker.terminal_result(id, "cancelled"));
        }
        Ok(worker.snapshot(id))
    }

    pub(crate) async fn wait(&self, id: &SubagentId) -> Result<SubagentResult> {
        let (provider, request, workspace, token) = {
            let mut workers = self.workers.borrow_mut();
            let worker = workers
                .get_mut(id)
                .with_context(|| format!("unknown subagent: {}", id.as_str()))?;
            if let Some(result) = &worker.result {
                return Ok(result.clone());
            }
            if worker.token.is_cancelled() || worker.status == SubagentStatus::Cancelled {
                worker.status = SubagentStatus::Cancelled;
                worker.push_event(id, SubagentStatus::Cancelled, Some("cancelled".to_string()));
                let result = worker.terminal_result(id, "cancelled");
                worker.result = Some(result.clone());
                return Ok(result);
            }
            (
                worker
                    .provider
                    .take()
                    .context("subagent provider already consumed")?,
                worker.request.clone(),
                worker.workspace.clone(),
                worker.token.clone(),
            )
        };

        let tools = read_only_tools(request.tool_allowlist.as_deref());
        let agent = Agent::new(provider, tools)
            .with_max_tool_roundtrips(request.budgets.max_tool_roundtrips);
        let mut harness = Harness::new(agent, workspace, ToolState::new(), None, None);
        let observer = ChildObserver;
        let gate = DenyGate;
        let run = harness
            .submit_turn(&request.prompt, &observer, &gate, &token)
            .await;

        let mut workers = self.workers.borrow_mut();
        let worker = workers
            .get_mut(id)
            .with_context(|| format!("unknown subagent: {}", id.as_str()))?;
        if token.is_cancelled() || worker.status == SubagentStatus::Cancelled {
            worker.status = SubagentStatus::Cancelled;
            worker.push_event(id, SubagentStatus::Cancelled, Some("cancelled".to_string()));
            let result = worker.terminal_result(id, "cancelled");
            worker.result = Some(result.clone());
            return Ok(result);
        }
        match run {
            Ok(_) => {
                worker.status = SubagentStatus::Completed;
                let summary = limit_summary(
                    final_assistant_text(harness.messages()),
                    request.budgets.max_output_bytes,
                );
                worker.push_event(id, SubagentStatus::Completed, None);
                let result = SubagentResult {
                    worker_id: id.clone(),
                    status: SubagentStatus::Completed,
                    summary,
                    output_handles: Vec::new(),
                    events: worker.events.clone(),
                };
                worker.result = Some(result.clone());
                Ok(result)
            }
            Err(error) => {
                let message = limit_summary(format!("{error:#}"), request.budgets.max_output_bytes);
                worker.status = SubagentStatus::Failed;
                worker.push_event(id, SubagentStatus::Failed, Some(message.clone()));
                let result = SubagentResult {
                    worker_id: id.clone(),
                    status: SubagentStatus::Failed,
                    summary: message,
                    output_handles: Vec::new(),
                    events: worker.events.clone(),
                };
                worker.result = Some(result.clone());
                Ok(result)
            }
        }
    }
}

impl<P> Worker<P> {
    fn push_event(&mut self, id: &SubagentId, status: SubagentStatus, message: Option<String>) {
        self.events.push(SubagentLifecycleEvent {
            worker_id: id.clone(),
            status,
            message,
        });
    }

    fn terminal_result(&self, id: &SubagentId, summary: &str) -> SubagentResult {
        SubagentResult {
            worker_id: id.clone(),
            status: self.status,
            summary: summary.to_string(),
            output_handles: Vec::new(),
            events: self.events.clone(),
        }
    }

    fn snapshot(&self, id: &SubagentId) -> SubagentSnapshot {
        SubagentSnapshot {
            handle: SubagentHandle {
                id: id.clone(),
                status: self.status,
            },
            result: self.result.clone(),
            events: self.events.clone(),
        }
    }
}

fn validate_request(request: &SubagentRequest) -> Result<()> {
    if request.prompt.trim().is_empty() {
        bail!("subagent prompt must not be empty");
    }
    if request.cwd.is_some() && matches!(request.isolation, Some(SubagentIsolation::Worktree)) {
        bail!("subagent request cannot specify both cwd and worktree isolation");
    }
    match request.capability_mode {
        SubagentCapabilityMode::ReadOnly => {}
        SubagentCapabilityMode::ReadWrite => {
            bail!("unsupported subagent capability_mode: read_write")
        }
        SubagentCapabilityMode::Execute => bail!("unsupported subagent capability_mode: execute"),
        SubagentCapabilityMode::All => bail!("unsupported subagent capability_mode: all"),
    }
    match request.isolation.unwrap_or(SubagentIsolation::None) {
        SubagentIsolation::None => Ok(()),
        SubagentIsolation::Worktree => bail!("unsupported subagent isolation: worktree"),
    }
}

fn validate_cwd(workspace: &Path, cwd: &Path) -> Result<PathBuf> {
    let root = workspace
        .canonicalize()
        .with_context(|| format!("subagent workspace does not exist: {}", workspace.display()))?;
    let path = cwd
        .canonicalize()
        .with_context(|| format!("subagent cwd does not exist: {}", cwd.display()))?;
    if !path.is_dir() {
        bail!("subagent cwd is not a directory: {}", path.display());
    }
    if path != root && !path.starts_with(&root) {
        bail!(
            "subagent cwd must stay inside the parent workspace: {}",
            cwd.display()
        );
    }
    Ok(path)
}

fn read_only_tools(allowlist: Option<&[String]>) -> crate::nexus::Tools {
    let tools = built_in_tools().into_read_only();
    match allowlist {
        Some(names) => tools.into_allowlist(names),
        None => tools,
    }
}

fn limit_summary(mut summary: String, max_bytes: Option<usize>) -> String {
    let Some(max_bytes) = max_bytes else {
        return summary;
    };
    if summary.len() <= max_bytes {
        return summary;
    }
    let mut cut = max_bytes;
    while cut > 0 && !summary.is_char_boundary(cut) {
        cut -= 1;
    }
    summary.truncate(cut);
    summary
}

fn final_assistant_text(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .map(|message| message.content.clone())
        .unwrap_or_default()
}

#[derive(Default)]
struct ChildObserver;

impl AgentObserver for ChildObserver {
    fn on_event(&self, _event: AgentEvent) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;
    use std::future::Future;
    use std::rc::Rc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::anyhow;
    use serde_json::{Value, json};

    use crate::nexus::{AssistantTurn, ProviderEvent, ProviderStream, Tool, ToolEnv, ToolFuture};

    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn turn_stream(turn: AssistantTurn) -> ProviderStream<'static> {
        Box::pin(futures::stream::once(async move {
            Ok(ProviderEvent::Completed(turn))
        }))
    }

    #[derive(Clone)]
    struct RecordingProvider {
        turns: Rc<RefCell<Vec<AssistantTurn>>>,
        visible_tools: Rc<RefCell<Vec<Vec<String>>>>,
    }

    impl RecordingProvider {
        fn new(turns: Vec<AssistantTurn>) -> Self {
            Self {
                turns: Rc::new(RefCell::new(turns.into_iter().rev().collect())),
                visible_tools: Rc::new(RefCell::new(Vec::new())),
            }
        }
    }

    impl ChatProvider for RecordingProvider {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            tools: &'a crate::nexus::Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            self.visible_tools.borrow_mut().push(
                tools
                    .iter()
                    .map(|tool| tool.name().to_string())
                    .collect::<Vec<_>>(),
            );
            let turn = self
                .turns
                .borrow_mut()
                .pop()
                .ok_or_else(|| anyhow!("unexpected provider call"))?;
            Ok(turn_stream(turn))
        }
    }

    struct PendingProvider;

    impl ChatProvider for PendingProvider {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a crate::nexus::Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    fn single_call_turn(name: &str, arguments: Value) -> AssistantTurn {
        AssistantTurn {
            text: None,
            reasoning: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: name.to_string(),
                arguments,
                thought_signature: None,
            }],
            response_id: None,
            usage: None,
            completion_reason: None,
        }
    }

    struct TestDir {
        path: PathBuf,
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn test_dir() -> Result<TestDir> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("iris-subagent-test-{nanos}-{seq}"));
        fs::create_dir(&path)?;
        Ok(TestDir { path })
    }

    #[test]
    fn request_contract_serializes_stable_tokens() -> Result<()> {
        let request = SubagentRequest {
            prompt: "inspect".to_string(),
            capability_mode: SubagentCapabilityMode::ReadOnly,
            isolation: Some(SubagentIsolation::None),
            cwd: None,
            tool_allowlist: Some(vec!["read".to_string()]),
            budgets: SubagentBudgets {
                max_tool_roundtrips: Some(2),
                max_output_bytes: Some(1024),
            },
        };

        let encoded = serde_json::to_value(&request)?;

        assert_eq!(encoded["capability_mode"], "read_only");
        assert_eq!(encoded["isolation"], "none");
        assert_eq!(encoded["budgets"]["max_tool_roundtrips"], 2);
        let decoded: SubagentRequest = serde_json::from_value(encoded)?;
        assert_eq!(decoded, request);
        Ok(())
    }

    #[test]
    fn validation_rejects_unsupported_capabilities_and_worktree_isolation() -> Result<()> {
        let workspace = test_dir()?;
        let provider = RecordingProvider::new(vec![AssistantTurn::text("unused")]);
        let backend = SubagentBackend::new(workspace.path.clone());

        let mut read_write = SubagentRequest::read_only("go");
        read_write.capability_mode = SubagentCapabilityMode::ReadWrite;
        let err = backend.spawn(provider.clone(), read_write).unwrap_err();
        assert!(err.to_string().contains("read_write"));

        let mut worktree = SubagentRequest::read_only("go");
        worktree.isolation = Some(SubagentIsolation::Worktree);
        let err = backend.spawn(provider.clone(), worktree).unwrap_err();
        assert!(err.to_string().contains("worktree"));

        let mut both = SubagentRequest::read_only("go");
        both.cwd = Some(workspace.path.clone());
        both.isolation = Some(SubagentIsolation::Worktree);
        let err = backend.spawn(provider, both).unwrap_err();
        assert!(err.to_string().contains("both cwd and worktree isolation"));
        Ok(())
    }

    #[test]
    fn request_cwd_must_stay_inside_parent_workspace() -> Result<()> {
        let workspace = test_dir()?;
        let outside = test_dir()?;
        let backend = SubagentBackend::new(workspace.path.clone());
        let provider = RecordingProvider::new(vec![AssistantTurn::text("unused")]);
        let mut request = SubagentRequest::read_only("inspect");
        request.cwd = Some(outside.path.clone());

        let err = backend.spawn(provider, request).unwrap_err();

        assert!(err.to_string().contains("inside the parent workspace"));
        Ok(())
    }

    #[test]
    fn max_output_budget_limits_returned_summary() -> Result<()> {
        let workspace = test_dir()?;
        let provider = RecordingProvider::new(vec![AssistantTurn::text("abcdef")]);
        let backend = SubagentBackend::new(workspace.path.clone());
        let mut request = SubagentRequest::read_only("summarize");
        request.budgets.max_output_bytes = Some(3);
        let handle = backend.spawn(provider, request)?;

        let result = block_on(backend.wait(&handle.id))?;

        assert_eq!(result.summary, "abc");
        Ok(())
    }

    #[test]
    fn read_only_worker_returns_structured_result() -> Result<()> {
        let workspace = test_dir()?;
        fs::write(workspace.path.join("note.txt"), "hello")?;
        let provider = RecordingProvider::new(vec![
            single_call_turn("read", json!({ "path": "note.txt" })),
            AssistantTurn::text("saw hello"),
        ]);
        let visible = provider.visible_tools.clone();
        let backend = SubagentBackend::new(workspace.path.clone());
        let handle = backend.spawn(provider, SubagentRequest::read_only("read note"))?;

        let result = block_on(backend.wait(&handle.id))?;

        assert_eq!(result.status, SubagentStatus::Completed);
        assert_eq!(result.summary, "saw hello");
        assert_eq!(result.events[0].status, SubagentStatus::Started);
        assert_eq!(result.events[1].status, SubagentStatus::Running);
        assert_eq!(
            result.events.last().unwrap().status,
            SubagentStatus::Completed
        );
        let first_visible = &visible.borrow()[0];
        assert!(first_visible.contains(&"read".to_string()));
        assert!(!first_visible.contains(&"write".to_string()));
        assert!(!first_visible.contains(&"edit".to_string()));
        assert!(!first_visible.contains(&"bash".to_string()));
        Ok(())
    }

    #[test]
    fn mutating_tool_call_is_rejected_before_execution_even_if_hidden() -> Result<()> {
        let workspace = test_dir()?;
        let provider = RecordingProvider::new(vec![
            single_call_turn("write", json!({ "path": "out.txt", "content": "nope" })),
            AssistantTurn::text("done"),
        ]);
        let backend = SubagentBackend::new(workspace.path.clone());
        let handle = backend.spawn(provider, SubagentRequest::read_only("try write"))?;

        let result = block_on(backend.wait(&handle.id))?;

        assert_eq!(result.status, SubagentStatus::Completed);
        assert!(!workspace.path.join("out.txt").exists());
        Ok(())
    }

    struct ClassifiedTool {
        name: &'static str,
        mutating: bool,
        approval: bool,
    }

    impl Tool for ClassifiedTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "test tool"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object" })
        }

        fn execute<'a>(
            &'a self,
            _args: &'a Value,
            _env: &'a ToolEnv<'_>,
            _cancel: CancellationToken,
        ) -> ToolFuture<'a> {
            Box::pin(async { Ok(crate::nexus::ToolOutput::text("ok")) })
        }

        fn requires_approval(&self) -> bool {
            self.approval
        }

        fn is_mutating(&self) -> bool {
            self.mutating
        }
    }

    #[test]
    fn read_only_filter_uses_tool_classification_not_names() {
        let tools = crate::nexus::Tools::new(vec![
            Box::new(ClassifiedTool {
                name: "innocent_name",
                mutating: true,
                approval: false,
            }),
            Box::new(ClassifiedTool {
                name: "custom_read",
                mutating: false,
                approval: false,
            }),
            Box::new(ClassifiedTool {
                name: "custom_prompted",
                mutating: false,
                approval: true,
            }),
        ])
        .into_read_only();

        assert!(tools.by_name("innocent_name").is_none());
        assert!(tools.by_name("custom_prompted").is_none());
        assert!(tools.by_name("custom_read").is_some());
    }

    #[test]
    fn allowlist_cannot_readd_mutating_tools() {
        let tools = read_only_tools(Some(&["write".to_string(), "read".to_string()]));
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();

        assert_eq!(names, vec!["read"]);
        assert!(tools.by_name("write").is_none());
    }

    #[test]
    fn cancellation_reaches_terminal_cancelled_state_before_wait() -> Result<()> {
        let workspace = test_dir()?;
        let provider = RecordingProvider::new(vec![AssistantTurn::text("unused")]);
        let backend = SubagentBackend::new(workspace.path.clone());
        let handle = backend.spawn(provider, SubagentRequest::read_only("wait"))?;

        let snapshot = backend.cancel(&handle.id)?;
        let result = block_on(backend.wait(&handle.id))?;

        assert_eq!(snapshot.handle.status, SubagentStatus::Cancelled);
        assert_eq!(result.status, SubagentStatus::Cancelled);
        assert_eq!(
            backend.poll(&handle.id)?.handle.status,
            SubagentStatus::Cancelled
        );
        Ok(())
    }

    #[test]
    fn cancellation_reaches_terminal_cancelled_state_while_waiting() -> Result<()> {
        let workspace = test_dir()?;
        let backend = SubagentBackend::new(workspace.path.clone());
        let handle = backend.spawn(PendingProvider, SubagentRequest::read_only("wait"))?;

        let result = block_on(async {
            let wait = backend.wait(&handle.id);
            let cancel = async {
                tokio::task::yield_now().await;
                backend.cancel(&handle.id)
            };
            let (wait_result, cancel_result) = tokio::join!(wait, cancel);
            cancel_result?;
            wait_result
        })?;

        assert_eq!(result.status, SubagentStatus::Cancelled);
        assert_eq!(
            backend.poll(&handle.id)?.handle.status,
            SubagentStatus::Cancelled
        );
        Ok(())
    }
}
