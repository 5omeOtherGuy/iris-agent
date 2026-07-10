use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use super::{ApplyContext, Harness, SummarizerKind};
use crate::config::CompactionTriggerConfig;
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate,
    AssistantTurn, BoundaryContext, ChatProvider, CompactionLifecycleState, CompactionOrigin,
    ContextDirective, Message, ProviderEvent, ProviderStream, ProviderUsage, ReviewContext,
    ToolCall, Tools,
};
use crate::session::{SessionLog, SessionStore};
use crate::tools::{ToolState, built_in_tools};
use crate::ui::steering::SteeringQueue;

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

#[derive(Clone)]
struct MidTurnProvider {
    call: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    steering: Option<Rc<SteeringQueue>>,
}

impl ChatProvider for MidTurnProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let call = self.call.fetch_add(1, Ordering::SeqCst);
        if call == 1
            && let Some(queue) = &self.steering
        {
            queue.enqueue_steering("STEER-VERBATIM: inspect the retained tail".to_string());
        }
        let mut turn = match call {
            0 | 1 => AssistantTurn {
                tool_calls: vec![ToolCall {
                    id: format!("call_midturn_{call}"),
                    name: "read".to_string(),
                    arguments: serde_json::json!({ "path": "note.txt" }),
                    thought_signature: None,
                }],
                ..AssistantTurn::default()
            },
            2 => AssistantTurn::text("finished after compaction"),
            _ => panic!("unexpected parent provider call {call}"),
        };
        if call == 0 {
            turn.usage = Some(ProviderUsage {
                provider: "test-parent".to_string(),
                model: "test-parent-model".to_string(),
                input_tokens: 59_900,
                output_tokens: 100,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 60_000,
                cache_creation: None,
            });
        }
        Ok(Box::pin(futures::stream::once(async move {
            if call == 1 {
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            Ok(ProviderEvent::Completed(turn))
        })))
    }
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

    fn applied_metadata(&self) -> Option<(CompactionOrigin, Option<ProviderUsage>)> {
        self.events.borrow().iter().find_map(|event| match event {
            AgentEvent::CompactionApplied {
                origin,
                worker_usage,
                ..
            } => Some((*origin, worker_usage.clone())),
            _ => None,
        })
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

#[derive(Clone)]
struct PendingSummaryProvider {
    started: Arc<AtomicBool>,
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
            let mut turn = AssistantTurn::text(&text);
            turn.usage = Some(ProviderUsage {
                provider: "test-provider".to_string(),
                model: "test-summary-model".to_string(),
                input_tokens: 120,
                output_tokens: 30,
                cache_read_input_tokens: 80,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 150,
                cache_creation: None,
            });
            Ok(ProviderEvent::Completed(turn))
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

impl PendingSummaryProvider {
    fn factory(
        started: Arc<AtomicBool>,
    ) -> Arc<dyn Fn() -> Result<Box<dyn ChatProvider>> + Send + Sync + 'static> {
        Arc::new(move || {
            Ok(Box::new(PendingSummaryProvider {
                started: started.clone(),
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

impl ChatProvider for PendingSummaryProvider {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        self.started.store(true, Ordering::SeqCst);
        Ok(Box::pin(futures::stream::pending()))
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
    let (origin, worker_usage) = obs.applied_metadata().expect("applied metadata");
    assert_eq!(origin, CompactionOrigin::Subagent);
    let worker_usage = worker_usage.expect("worker usage from live summarizer lane");
    assert_eq!(worker_usage.total_tokens, 150);
    assert_eq!(worker_usage.cache_read_input_tokens, 80);
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
    assert_eq!(entries[0]["origin"], "subagent");
    assert_eq!(entries[0]["workerUsage"]["totalTokens"], 150);
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
fn ready_summary_applies_mid_turn_before_queued_steering_is_injected_verbatim() {
    let root = temp_dir();
    let workspace = temp_dir();
    std::fs::write(workspace.path.join("note.txt"), "mid-turn read\n").unwrap();
    let mut log = SessionLog::create_in(&root.path, &workspace.path).unwrap();
    let old = format!("{OLD_NEEDLE} :: {}", "covered context ".repeat(7_500));
    log.append(&Message::user(&old)).unwrap();
    log.append(&Message::assistant("old context acknowledged"))
        .unwrap();
    let path = log.path().to_path_buf();
    drop(log);

    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .unwrap()
        .into_iter()
        .find(|meta| meta.path == path)
        .unwrap();
    let stored = store.open(&meta).unwrap();
    let session = SessionLog::resume(&path).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let steering = Rc::new(SteeringQueue::default());
    let provider = MidTurnProvider {
        call: Arc::new(AtomicUsize::new(0)),
        requests: requests.clone(),
        steering: Some(steering.clone()),
    };
    let agent = Agent::resumed(provider, built_in_tools(), stored.messages);
    let mut harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(session),
        stored.entry_ids,
        Some(131_072),
    );
    harness.set_compaction_trigger(
        131_072,
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 1_000,
            hard_wait_ms: 10,
            max_consecutive_failures: 3,
        },
    );
    let ladder = harness.compaction.ladder.as_mut().unwrap();
    ladder.warn = 40_000;
    ladder.start = 50_000;
    ladder.hard = 100_000;
    ladder.deterministic_only = false;
    harness.set_summarizer(SummarizerKind::Subagent);
    harness.set_steering_source(steering);
    harness.set_compaction_summarizer_factory(SummaryProvider::factory(
        Arc::new(Mutex::new(VecDeque::from([format!(
            "Goal: continue. State: compacted mid-turn. Decisions: none. Key facts: {SUMMARY_NEEDLE}. Next steps: finish."
        )]))),
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
    ));
    let obs = Recorder::default();

    block_on(harness.submit_turn(
        "perform two reads",
        &obs,
        &AllowGate,
        &CancellationToken::new(),
    ))
    .unwrap();

    assert_eq!(obs.lifecycle(CompactionLifecycleState::Running), 1);
    assert_eq!(obs.applied(), 1, "ready summary lands inside the turn");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1]
            .iter()
            .any(|message| message.content.contains(OLD_NEEDLE)),
        "the worker runs while the parent continues"
    );
    let third = requests[2]
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(third.contains(SUMMARY_NEEDLE), "{third}");
    assert!(!third.contains(OLD_NEEDLE), "{third}");
    let summary_at = third.find(SUMMARY_NEEDLE).unwrap();
    let steering_at = third.find("STEER-VERBATIM").unwrap();
    assert!(
        summary_at < steering_at,
        "queued steering must be injected verbatim after the swap: {third}"
    );
    drop(requests);

    let reopened = store.open(&meta).unwrap();
    assert_eq!(
        reopened.messages,
        harness.messages(),
        "live context and resume rebuild stay byte-equivalent"
    );
    assert_eq!(compaction_entries(&path).len(), 1);
}

#[test]
fn manual_compact_uses_subagent_before_provider_fallback() {
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
    let parent_replies = Arc::new(Mutex::new(VecDeque::from([format!(
        "Goal: continue. State: provider fallback. Decisions: none. Key facts: {SUMMARY_NEEDLE}. Next steps: proceed."
    )])));
    let parent_prompts = Arc::new(Mutex::new(Vec::new()));
    let parent_tools = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::resumed(
        SummaryProvider {
            replies: parent_replies,
            prompts: parent_prompts.clone(),
            visible_tools: parent_tools,
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
    let worker_replies = Arc::new(Mutex::new(VecDeque::from(["".to_string()])));
    let worker_prompts = Arc::new(Mutex::new(Vec::new()));
    let worker_tools = Arc::new(Mutex::new(Vec::new()));
    harness.set_compaction_summarizer_factory(SummaryProvider::factory(
        worker_replies,
        worker_prompts.clone(),
        worker_tools.clone(),
    ));
    let obs = Recorder::default();
    let token = CancellationToken::new();

    block_on(harness.compact_now(&obs, &token)).unwrap();

    assert_eq!(worker_prompts.lock().unwrap().len(), 1);
    assert_eq!(parent_prompts.lock().unwrap().len(), 1);
    let worker_tools = worker_tools.lock().unwrap();
    assert_eq!(worker_tools.len(), 1);
    assert!(worker_tools[0].contains(&"read".to_string()));
    assert!(!worker_tools[0].contains(&"write".to_string()));
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
}

#[test]
fn background_subagent_falls_back_to_provider_before_excerpts() {
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
    let agent = Agent::resumed(SilentProvider, built_in_tools(), stored.messages);
    let mut harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        stored.entry_ids,
        Some(300),
    );
    harness.set_summarizer(SummarizerKind::Subagent);
    let replies = Arc::new(Mutex::new(VecDeque::from([
        "".to_string(),
        format!(
            "Goal: continue. State: provider fallback. Decisions: none. Key facts: {SUMMARY_NEEDLE}. Next steps: proceed."
        ),
    ])));
    let prompts = Arc::new(Mutex::new(Vec::new()));
    let visible_tools = Arc::new(Mutex::new(Vec::new()));
    harness.set_compaction_summarizer_factory(SummaryProvider::factory(
        replies,
        prompts.clone(),
        visible_tools.clone(),
    ));
    let obs = Recorder::default();
    let token = CancellationToken::new();

    block_on(harness.maybe_auto_compact(&obs, &token, true)).unwrap();
    for _ in 0..50 {
        block_on(harness.maybe_auto_compact(&obs, &token, true)).unwrap();
        if obs.applied() == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(obs.applied(), 1);
    assert_eq!(prompts.lock().unwrap().len(), 2);
    let visible_tools = visible_tools.lock().unwrap();
    assert_eq!(visible_tools.len(), 2);
    assert!(visible_tools[0].contains(&"read".to_string()));
    assert!(!visible_tools[0].contains(&"write".to_string()));
    assert!(
        visible_tools[1].is_empty(),
        "provider fallback summary is tool-free"
    );
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

#[test]
fn hard_tier_bounds_wait_then_cancels_and_applies_deterministic_fallback() {
    let root = temp_dir();
    let workspace = temp_dir();
    let mut seeded = seed_harness(&root.path, &workspace.path);
    let worker_prompts = Arc::new(Mutex::new(Vec::new()));
    seeded
        .harness
        .set_compaction_summarizer_factory(BlockingSummaryProvider::factory(
            worker_prompts.clone(),
        ));
    let obs = Recorder::default();
    let token = CancellationToken::new();

    // Start through the legacy test seam, then engage v2 with an immediate
    // hard-wait deadline. The production host installs v2 at startup.
    block_on(seeded.harness.maybe_auto_compact(&obs, &token, true)).unwrap();
    assert_eq!(obs.lifecycle(CompactionLifecycleState::Running), 1);
    for _ in 0..50 {
        if !worker_prompts.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    seeded.harness.set_compaction_trigger(
        300,
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 20_000,
            hard_wait_ms: 0,
            max_consecutive_failures: 3,
        },
    );

    let messages = seeded.harness.messages().to_vec();
    let task_state = seeded.harness.compaction_task_state();
    let started = Instant::now();
    let directive = block_on(seeded.harness.compaction.govern(
        BoundaryContext {
            messages: &messages,
            last_usage: None,
            round_trip: 1,
            turn_continues: true,
        },
        ApplyContext {
            workspace: &workspace.path,
            output_store: seeded.harness.output_store.as_ref(),
            task_state: task_state.as_ref(),
            observer: &obs,
        },
        &token,
    ))
    .unwrap();
    assert!(started.elapsed() < Duration::from_millis(100));
    let ContextDirective::Replace { messages } = directive else {
        panic!("hard tier must return deterministic relief");
    };
    seeded.harness.agent.replace_messages(messages);
    assert_eq!(obs.lifecycle(CompactionLifecycleState::Cancelled), 1);
    assert_eq!(obs.applied(), 1);
}

#[test]
fn turn_cancellation_preempts_the_governor_hard_wait_without_applying() {
    let root = temp_dir();
    let workspace = temp_dir();
    let mut seeded = seed_harness(&root.path, &workspace.path);
    let worker_started = Arc::new(AtomicBool::new(false));
    seeded
        .harness
        .set_compaction_summarizer_factory(PendingSummaryProvider::factory(worker_started.clone()));
    let obs = Recorder::default();
    let token = CancellationToken::new();

    block_on(seeded.harness.maybe_auto_compact(&obs, &token, true)).unwrap();
    for _ in 0..50 {
        if worker_started.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    seeded.harness.set_compaction_trigger(
        300,
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 20_000,
            hard_wait_ms: 5_000,
            max_consecutive_failures: 3,
        },
    );

    let messages = seeded.harness.messages().to_vec();
    let task_state = seeded.harness.compaction_task_state();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let started = Instant::now();
    let directive = runtime
        .block_on(async {
            let cancel = async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                token.cancel();
            };
            let govern = seeded.harness.compaction.govern(
                BoundaryContext {
                    messages: &messages,
                    last_usage: None,
                    round_trip: 1,
                    turn_continues: true,
                },
                ApplyContext {
                    workspace: &workspace.path,
                    output_store: seeded.harness.output_store.as_ref(),
                    task_state: task_state.as_ref(),
                    observer: &obs,
                },
                &token,
            );
            let (_, result) = tokio::join!(cancel, govern);
            result
        })
        .unwrap();

    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(directive, ContextDirective::Proceed);
    assert_eq!(obs.lifecycle(CompactionLifecycleState::Cancelled), 1);
    assert_eq!(obs.applied(), 0);
    assert!(compaction_entries(&seeded.path).is_empty());
    drop(runtime);
}

#[test]
fn v2_off_switch_leaves_automatic_rewrites_disabled() {
    let root = temp_dir();
    let workspace = temp_dir();
    let mut seeded = seed_harness(&root.path, &workspace.path);
    seeded.harness.set_compaction_trigger(
        300,
        CompactionTriggerConfig {
            enabled: false,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 20_000,
            hard_wait_ms: 0,
            max_consecutive_failures: 3,
        },
    );
    let obs = Recorder::default();
    block_on(
        seeded
            .harness
            .maybe_auto_compact(&obs, &CancellationToken::new(), false),
    )
    .unwrap();
    assert_eq!(obs.applied(), 0);
    assert_eq!(obs.lifecycle(CompactionLifecycleState::Running), 0);
}

#[test]
fn breaker_disables_model_jobs_but_keeps_deterministic_compaction() {
    let root = temp_dir();
    let workspace = temp_dir();
    let mut seeded = seed_harness(&root.path, &workspace.path);
    seeded.harness.set_compaction_trigger(
        300,
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.1,
            start: 0.2,
            hard: 0.9,
            keep_recent_tokens: 20_000,
            hard_wait_ms: 0,
            max_consecutive_failures: 3,
        },
    );
    let ladder = seeded.harness.compaction.ladder.as_mut().unwrap();
    ladder.warn = 1;
    ladder.start = 2;
    ladder.hard = u64::MAX;
    ladder.deterministic_only = false;
    for _ in 0..3 {
        seeded.harness.record_compaction_failure();
    }
    let obs = Recorder::default();
    block_on(
        seeded
            .harness
            .maybe_auto_compact(&obs, &CancellationToken::new(), true),
    )
    .unwrap();

    assert_eq!(obs.lifecycle(CompactionLifecycleState::Running), 0);
    assert_eq!(obs.applied(), 1);
    assert!(obs.events.borrow().iter().any(|event| matches!(
        event,
        AgentEvent::Notice(message) if message.contains("disabled after 3 consecutive failures")
    )));
}
