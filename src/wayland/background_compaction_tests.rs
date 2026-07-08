use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use super::{Harness, SummarizerKind};
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate,
    AssistantTurn, ChatProvider, CompactionLifecycleState, Message, ProviderEvent, ProviderStream,
    ReviewContext, ToolCall, Tools,
};
use crate::session::{SessionLog, SessionStore};
use crate::tools::{ToolState, built_in_tools};

const OLD_NEEDLE: &str = "BACKGROUND-COMPACTION-OLD-NEEDLE";
const SUMMARY_NEEDLE: &str = "BACKGROUND-COMPACTION-SUMMARY-NEEDLE";

struct TempDir {
    path: PathBuf,
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn temp_dir() -> TempDir {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("iris-bg-compact-{nanos}-{seq}"));
    std::fs::create_dir(&path).unwrap();
    TempDir { path }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

struct SilentProvider;

impl ChatProvider for SilentProvider {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        Ok(Box::pin(futures::stream::empty()))
    }
}

#[derive(Clone)]
struct TurnProvider {
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl ChatProvider for TurnProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        self.requests.lock().unwrap().push(messages.to_vec());
        Ok(Box::pin(futures::stream::once(async {
            Ok(ProviderEvent::Completed(AssistantTurn::text(
                "turn complete",
            )))
        })))
    }
}

struct AllowGate;

impl ApprovalGate for AllowGate {
    fn review<'a>(
        &'a self,
        _call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        _ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::Allow) })
    }
}

#[derive(Default)]
struct Recorder {
    events: RefCell<Vec<AgentEvent>>,
}

impl AgentObserver for Recorder {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        self.events.borrow_mut().push(event);
        Ok(())
    }
}

impl Recorder {
    fn lifecycle(&self, state: CompactionLifecycleState) -> usize {
        self.events
            .borrow()
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    AgentEvent::CompactionLifecycle { state: seen, .. } if *seen == state
                )
            })
            .count()
    }

    fn applied(&self) -> usize {
        self.events
            .borrow()
            .iter()
            .filter(|event| matches!(event, AgentEvent::CompactionApplied { .. }))
            .count()
    }
}

#[derive(Clone)]
struct SummaryProvider {
    replies: Arc<Mutex<VecDeque<String>>>,
    prompts: Arc<Mutex<Vec<String>>>,
    visible_tools: Arc<Mutex<Vec<Vec<String>>>>,
}

#[derive(Clone)]
struct BlockingSummaryProvider {
    prompts: Arc<Mutex<Vec<String>>>,
}

struct SeededHarness {
    harness: Harness<SilentProvider>,
    path: PathBuf,
    prompts: Arc<Mutex<Vec<String>>>,
    visible_tools: Arc<Mutex<Vec<Vec<String>>>>,
}

impl SummaryProvider {
    fn factory(
        replies: Arc<Mutex<VecDeque<String>>>,
        prompts: Arc<Mutex<Vec<String>>>,
        visible_tools: Arc<Mutex<Vec<Vec<String>>>>,
    ) -> Arc<dyn Fn() -> Result<Box<dyn ChatProvider>> + Send + Sync + 'static> {
        Arc::new(move || {
            Ok(Box::new(SummaryProvider {
                replies: replies.clone(),
                prompts: prompts.clone(),
                visible_tools: visible_tools.clone(),
            }))
        })
    }
}

impl ChatProvider for SummaryProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        self.prompts.lock().unwrap().push(
            messages
                .last()
                .map(|m| m.content.clone())
                .unwrap_or_default(),
        );
        self.visible_tools.lock().unwrap().push(
            tools
                .iter()
                .map(|tool| tool.name().to_string())
                .collect::<Vec<_>>(),
        );
        let text = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| {
                format!(
                    "Goal: continue. State: compacted. Decisions: none. Key facts: {SUMMARY_NEEDLE}. Next steps: proceed."
                )
            });
        Ok(Box::pin(futures::stream::once(async move {
            Ok(ProviderEvent::Completed(AssistantTurn::text(&text)))
        })))
    }
}

impl BlockingSummaryProvider {
    fn factory(
        prompts: Arc<Mutex<Vec<String>>>,
    ) -> Arc<dyn Fn() -> Result<Box<dyn ChatProvider>> + Send + Sync + 'static> {
        Arc::new(move || {
            Ok(Box::new(BlockingSummaryProvider {
                prompts: prompts.clone(),
            }))
        })
    }
}

impl ChatProvider for BlockingSummaryProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a Tools,
        cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        self.prompts.lock().unwrap().push(
            messages
                .last()
                .map(|m| m.content.clone())
                .unwrap_or_default(),
        );
        while !cancel.is_cancelled() {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(Box::pin(futures::stream::once(async {
            Ok(ProviderEvent::Completed(AssistantTurn::text(
                "Goal: cancelled. State: stale. Decisions: none. Key facts: stale. Next steps: none.",
            )))
        })))
    }
}

fn seed_harness(root: &Path, workspace: &Path) -> SeededHarness {
    let mut log = SessionLog::create_in(root, workspace).unwrap();
    let big = format!("{OLD_NEEDLE} :: {}", "long covered context. ".repeat(500));
    for message in [
        Message::user(&big),
        Message::assistant("ok"),
        Message::user("small retained turn"),
        Message::assistant("ok2"),
    ] {
        log.append(&message).unwrap();
    }
    let path = log.path().to_path_buf();
    drop(log);

    let store = SessionStore::with_root(root.to_path_buf());
    let meta = store
        .list()
        .unwrap()
        .into_iter()
        .find(|m| m.path == path)
        .unwrap();
    let stored = store.open(&meta).unwrap();
    let log = SessionLog::resume(&path).unwrap();
    let agent = Agent::resumed(SilentProvider, built_in_tools(), stored.messages);
    let mut harness = Harness::resumed(
        agent,
        workspace.to_path_buf(),
        ToolState::new(),
        Some(log),
        stored.entry_ids,
        Some(300),
    );
    harness.set_summarizer(SummarizerKind::Subagent);
    let replies = Arc::new(Mutex::new(VecDeque::from([format!(
        "Goal: continue. State: compacted. Decisions: none. Key facts: {SUMMARY_NEEDLE}. Next steps: proceed."
    )])));
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let visible_tools = Arc::new(Mutex::new(Vec::new()));
    harness.set_compaction_summarizer_factory(SummaryProvider::factory(
        replies.clone(),
        prompts.clone(),
        visible_tools.clone(),
    ));
    SeededHarness {
        harness,
        path,
        prompts,
        visible_tools,
    }
}

fn compaction_entries(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["type"] == "compaction")
        .collect()
}

#[test]
fn background_subagent_compaction_runs_read_only_and_parent_applies_result() {
    let root = temp_dir();
    let workspace = temp_dir();
    let seeded = seed_harness(&root.path, &workspace.path);
    let SeededHarness {
        mut harness,
        path,
        prompts,
        visible_tools,
    } = seeded;
    let obs = Recorder::default();
    let token = CancellationToken::new();

    block_on(harness.maybe_auto_compact(&obs, &token, true)).unwrap();
    assert_eq!(obs.lifecycle(CompactionLifecycleState::Running), 1);
    assert!(
        compaction_entries(&path).is_empty(),
        "worker text is not persisted until the parent drains and validates it"
    );
    assert!(
        harness
            .messages()
            .iter()
            .any(|message| message.content.contains(OLD_NEEDLE)),
        "context must remain unchanged while the background worker runs"
    );

    for _ in 0..50 {
        block_on(harness.maybe_auto_compact(&obs, &token, true)).unwrap();
        if obs.applied() == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(obs.applied(), 1);
    let tools = visible_tools.lock().unwrap();
    assert_eq!(tools.len(), 1);
    assert!(tools[0].contains(&"read".to_string()));
    assert!(!tools[0].contains(&"write".to_string()));
    assert!(!tools[0].contains(&"bash".to_string()));
    assert!(prompts.lock().unwrap()[0].contains(OLD_NEEDLE));

    let live = harness
        .messages()
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(live.contains(SUMMARY_NEEDLE), "{live}");
    assert!(
        !live.contains(OLD_NEEDLE),
        "covered text should only remain behind recall"
    );

    let entries = compaction_entries(&path);
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0]["summary"]
            .as_str()
            .unwrap()
            .contains(SUMMARY_NEEDLE)
    );

    let reopened = SessionStore::with_root(root.path.clone())
        .list()
        .unwrap()
        .into_iter()
        .find(|m| m.path == path)
        .map(|meta| {
            SessionStore::with_root(root.path.clone())
                .open(&meta)
                .unwrap()
        })
        .unwrap();
    let rebuilt = reopened
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rebuilt.contains(SUMMARY_NEEDLE), "{rebuilt}");
}

#[test]
fn pending_background_compaction_falls_back_before_next_provider_request() {
    let root = temp_dir();
    let workspace = temp_dir();
    let mut log = SessionLog::create_in(&root.path, &workspace.path).unwrap();
    let big = format!("{OLD_NEEDLE} :: {}", "long covered context. ".repeat(500));
    for message in [
        Message::user(&big),
        Message::assistant("ok"),
        Message::user("small retained turn"),
        Message::assistant("ok2"),
    ] {
        log.append(&message).unwrap();
    }
    let path = log.path().to_path_buf();
    drop(log);

    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .unwrap()
        .into_iter()
        .find(|m| m.path == path)
        .unwrap();
    let stored = store.open(&meta).unwrap();
    let log = SessionLog::resume(&path).unwrap();
    let turn_requests = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::resumed(
        TurnProvider {
            requests: turn_requests.clone(),
        },
        built_in_tools(),
        stored.messages,
    );
    let mut harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        stored.entry_ids,
        Some(300),
    );
    harness.set_summarizer(SummarizerKind::Subagent);
    let worker_prompts = Arc::new(Mutex::new(Vec::new()));
    harness.set_compaction_summarizer_factory(BlockingSummaryProvider::factory(
        worker_prompts.clone(),
    ));
    let obs = Recorder::default();
    let token = CancellationToken::new();

    block_on(harness.maybe_auto_compact(&obs, &token, true)).unwrap();
    assert_eq!(obs.lifecycle(CompactionLifecycleState::Running), 1);
    for _ in 0..50 {
        if !worker_prompts.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(worker_prompts.lock().unwrap().len(), 1);

    block_on(harness.submit_turn("next small prompt", &obs, &AllowGate, &token)).unwrap();

    assert_eq!(obs.lifecycle(CompactionLifecycleState::Cancelled), 1);
    assert_eq!(obs.applied(), 1);
    let requests = turn_requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let sent_tokens = super::context_tokens(&requests[0]);
    assert!(
        sent_tokens <= 300,
        "provider saw over-budget context: {sent_tokens} tokens"
    );
}

#[test]
fn stale_background_result_is_discarded_after_parent_revalidation() {
    let root = temp_dir();
    let workspace = temp_dir();
    let seeded = seed_harness(&root.path, &workspace.path);
    let SeededHarness {
        mut harness, path, ..
    } = seeded;
    let obs = Recorder::default();
    let token = CancellationToken::new();

    block_on(harness.maybe_auto_compact(&obs, &token, true)).unwrap();
    block_on(harness.compact_now(&obs, &token)).unwrap();
    assert_eq!(compaction_entries(&path).len(), 1);

    for _ in 0..50 {
        block_on(harness.maybe_auto_compact(&obs, &token, true)).unwrap();
        if obs.lifecycle(CompactionLifecycleState::Discarded) == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(obs.lifecycle(CompactionLifecycleState::Discarded), 1);
    assert_eq!(
        compaction_entries(&path).len(),
        1,
        "stale worker result must not append a second compaction"
    );
}
