use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use super::{
    ApplyContext, CompactionRangeContext, CompactionWorkerConfig, CompactionWorkerInput, Harness,
    SummarizerKind, run_compaction_worker,
};
use crate::config::CompactionTriggerConfig;
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate,
    AssistantTurn, BoundaryContext, ChatProvider, CompactionLifecycleState, CompactionOrigin,
    ContextDirective, ContextPressureTier, Message, ProviderCompactionCapability,
    ProviderCompactionFuture, ProviderCompactionOutput, ProviderEvent, ProviderStream,
    ProviderUsage, ReviewContext, StructuredSummaryCapability, StructuredSummaryError,
    StructuredSummaryFuture, StructuredSummaryMode, ToolCall, Tools,
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

/// A placeholder range context for `run_compaction_worker` tests that do not
/// exercise the issue #475 structured-summary path (i.e. every
/// `SummarizerKind::Subagent`/legacy-transcript test in this file): the
/// values are never read unless the constructed provider reports
/// `StructuredSummaryCapability::Native`, which none of these fakes do.
fn test_range_context() -> CompactionRangeContext {
    CompactionRangeContext {
        from_id: "msg_test_from".to_string(),
        to_id: "msg_test_to".to_string(),
        carry_paths: Vec::new(),
        original_tokens: 0,
    }
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
        if matches!(call, 0 | 1) {
            let total_tokens: u64 = if call == 0 { 60_000 } else { 70_000 };
            turn.usage = Some(ProviderUsage {
                provider: "test-parent".to_string(),
                model: "test-parent-model".to_string(),
                input_tokens: total_tokens.saturating_sub(100),
                output_tokens: 100,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens,
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
        let events = self.events.borrow();
        let mut count = 0;
        for event in events.iter() {
            if let AgentEvent::CompactionLifecycle { state: seen, .. } = event
                && *seen == state
            {
                count += 1;
            }
        }
        count
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

    /// Every `AgentEvent::ContextPressure` tier recorded, in order. Used to
    /// check the runtime re-emits a fresh post-apply tier so the footer (and the
    /// meter's stalled predicate) never idles on the stale pre-apply tier
    /// (Findings 6/7).
    fn pressures(&self) -> Vec<ContextPressureTier> {
        self.events
            .borrow()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ContextPressure { tier, .. } => Some(*tier),
                _ => None,
            })
            .collect()
    }

    /// Every plain `AgentEvent::Notice` text recorded, in order. Used to check
    /// the user-visible apply notice names its route (audit F11c/F20).
    fn notices(&self) -> Vec<String> {
        self.events
            .borrow()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Notice(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
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

#[derive(Clone)]
struct ScriptedWorkerProvider {
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    turns: Arc<Mutex<VecDeque<AssistantTurn>>>,
}

#[derive(Clone)]
struct NativeCompactionProvider {
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl ChatProvider for NativeCompactionProvider {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        Ok(Box::pin(futures::stream::empty()))
    }

    fn compaction_capability(&self, _input_tokens: u64) -> ProviderCompactionCapability {
        ProviderCompactionCapability::OpaqueBlocks
    }

    fn compact_context<'a>(
        &'a self,
        messages: &'a [Message],
        _instructions: &'a str,
        _cancel: &'a CancellationToken,
    ) -> ProviderCompactionFuture<'a> {
        self.requests.lock().unwrap().push(messages.to_vec());
        Box::pin(async {
            Ok(ProviderCompactionOutput {
                summary: format!(
                    "Goal: continue. State: native. Key facts: {SUMMARY_NEEDLE}. Next steps: proceed."
                ),
                provider_blocks: vec![serde_json::json!({
                    "adapter": "fake-native",
                    "model": "fake-model",
                    "block": { "type": "compaction", "content": SUMMARY_NEEDLE }
                })],
                usage: Some(ProviderUsage {
                    provider: "fake-native".to_string(),
                    model: "fake-model".to_string(),
                    input_tokens: 2_000,
                    output_tokens: 100,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                    total_tokens: 2_100,
                    cache_creation: None,
                }),
            })
        })
    }
}

/// A provider-native adapter whose `compact_context` blocks until the worker
/// token is cancelled, so the hard-wait always times out. It records every
/// invocation so a test can prove the native rung fired exactly once.
#[derive(Clone)]
struct BlockingNativeProvider {
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl ChatProvider for BlockingNativeProvider {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        Ok(Box::pin(futures::stream::empty()))
    }

    fn compaction_capability(&self, _input_tokens: u64) -> ProviderCompactionCapability {
        ProviderCompactionCapability::OpaqueBlocks
    }

    fn compact_context<'a>(
        &'a self,
        messages: &'a [Message],
        _instructions: &'a str,
        cancel: &'a CancellationToken,
    ) -> ProviderCompactionFuture<'a> {
        self.requests.lock().unwrap().push(messages.to_vec());
        Box::pin(async move {
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            anyhow::bail!("provider-native compaction cancelled")
        })
    }
}

/// Observer that cancels a shared turn token the instant a compaction is
/// durably applied. It reproduces the cancellation-racing-the-apply window:
/// the mutation has already hit the session log before the governor checks the
/// token.
struct CancelOnApply {
    events: RefCell<Vec<AgentEvent>>,
    token: CancellationToken,
}

impl AgentObserver for CancelOnApply {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        if matches!(
            &event,
            AgentEvent::CompactionLifecycle {
                state: CompactionLifecycleState::Applied,
                ..
            }
        ) {
            self.token.cancel();
        }
        self.events.borrow_mut().push(event);
        Ok(())
    }
}

impl ChatProvider for ScriptedWorkerProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let turn = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted worker turn");
        Ok(Box::pin(futures::stream::once(async move {
            Ok(ProviderEvent::Completed(turn))
        })))
    }
}

fn scripted_worker_factory(
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    turns: Arc<Mutex<VecDeque<AssistantTurn>>>,
) -> Arc<dyn Fn() -> Result<Box<dyn ChatProvider>> + Send + Sync + 'static> {
    Arc::new(move || {
        Ok(Box::new(ScriptedWorkerProvider {
            requests: requests.clone(),
            turns: turns.clone(),
        }))
    })
}

#[test]
fn transcript_worker_sends_verbatim_covered_messages_then_instructions() {
    let covered = vec![
        Message::user("NEEDLE-verbatim: preserve spacing  exactly"),
        Message::assistant("acknowledged verbatim"),
    ];
    let requests = Arc::new(Mutex::new(Vec::new()));
    let turns = Arc::new(Mutex::new(VecDeque::from([AssistantTurn::text(
        "Goal: continue. State: compacted. Decisions: none. Key facts: NEEDLE-verbatim. Next steps: proceed.",
    )])));
    let config = CompactionWorkerConfig {
        input: CompactionWorkerInput::Transcript,
        instructions: "Prioritize exact flags.".to_string(),
        ..CompactionWorkerConfig::default()
    };

    let result = run_compaction_worker(
        scripted_worker_factory(requests.clone(), turns),
        temp_dir().path.clone(),
        covered.clone(),
        config,
        SummarizerKind::Subagent,
        test_range_context(),
        CancellationToken::new(),
    );

    assert!(matches!(result, super::BackgroundSummaryResult::Summary(_)));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(&requests[0][..covered.len()], covered.as_slice());
    let instruction = requests[0].last().unwrap();
    assert_eq!(instruction.role, crate::nexus::Role::User);
    assert!(instruction.content.starts_with(super::SUMMARY_PROMPT));
    assert!(instruction.content.contains("Prioritize exact flags."));
}

#[test]
fn transcript_worker_shrinks_oldest_message_on_overflow_and_terminates() {
    let covered = vec![
        Message::user("oldest"),
        Message::assistant("middle"),
        Message::user("newest"),
    ];
    let overflow = AssistantTurn {
        completion_reason: Some(crate::nexus::CompletionReason::ContextWindowExceeded),
        ..AssistantTurn::default()
    };
    let requests = Arc::new(Mutex::new(Vec::new()));
    let turns = Arc::new(Mutex::new(VecDeque::from([
        overflow,
        AssistantTurn::text(
            "Goal: continue. State: compacted. Decisions: none. Key facts: newest. Next steps: proceed.",
        ),
    ])));

    let result = run_compaction_worker(
        scripted_worker_factory(requests.clone(), turns),
        temp_dir().path.clone(),
        covered.clone(),
        CompactionWorkerConfig::default(),
        SummarizerKind::Subagent,
        test_range_context(),
        CancellationToken::new(),
    );

    assert!(matches!(result, super::BackgroundSummaryResult::Summary(_)));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(&requests[0][..3], covered.as_slice());
    assert_eq!(&requests[1][..2], &covered[1..]);
}

#[test]
fn transcript_worker_overflow_retry_stops_when_the_slice_is_empty() {
    let overflow = || AssistantTurn {
        completion_reason: Some(crate::nexus::CompletionReason::ContextWindowExceeded),
        ..AssistantTurn::default()
    };
    let requests = Arc::new(Mutex::new(Vec::new()));
    let turns = Arc::new(Mutex::new(VecDeque::from([
        overflow(),
        overflow(),
        overflow(),
    ])));

    let result = run_compaction_worker(
        scripted_worker_factory(requests.clone(), turns),
        temp_dir().path.clone(),
        vec![
            Message::user("oldest"),
            Message::assistant("middle"),
            Message::user("newest"),
        ],
        CompactionWorkerConfig::default(),
        SummarizerKind::Subagent,
        test_range_context(),
        CancellationToken::new(),
    );

    assert!(matches!(result, super::BackgroundSummaryResult::Failed(_)));
    assert_eq!(requests.lock().unwrap().len(), 3);
}

#[test]
fn transcript_worker_threads_cancellation_through_the_factory_provider() {
    let started = Arc::new(AtomicBool::new(false));
    let token = CancellationToken::new();
    let worker_token = token.clone();
    let handle = std::thread::spawn({
        let started = started.clone();
        move || {
            run_compaction_worker(
                PendingSummaryProvider::factory(started),
                temp_dir().path.clone(),
                vec![Message::user("covered")],
                CompactionWorkerConfig::default(),
                SummarizerKind::Subagent,
                test_range_context(),
                worker_token,
            )
        }
    });
    for _ in 0..100 {
        if started.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(started.load(Ordering::SeqCst));
    token.cancel();

    assert!(matches!(
        handle.join().unwrap(),
        super::BackgroundSummaryResult::Cancelled
    ));
}

// --- Issue #475 / ADR-0061: structured-output compaction-summary fallback
// ladder tests. `FakeStructuredSummaryProvider` reports
// `StructuredSummaryCapability::Native`, so `run_compaction_worker`'s
// `SummarizerKind::Provider` branch takes the new structured path instead of
// legacy full-transcript replay (`run_transcript_summary`); every test above
// this section uses `SummarizerKind::Subagent` and is unaffected by this
// gating, proving the default route is unchanged for providers/kinds that do
// not opt in.

/// A scripted attempt for [`FakeStructuredSummaryProvider`]: each call to
/// `run_structured_summary` pops the next entry.
enum ScriptedStructuredAttempt {
    /// Return this `AssistantTurn` as a successful send.
    Turn(AssistantTurn),
    /// Return `StructuredSummaryError::Unsupported` (the caller must retry
    /// exactly once with `StructuredSummaryMode::ForcedTool`).
    Unsupported,
    /// Cancel `cancel` (mirroring a turn token cancelled mid-request, the
    /// same mechanism `PendingSummaryProvider`'s cancellation test uses) and
    /// return `StructuredSummaryError::Cancelled`.
    Cancelled,
    /// Return `StructuredSummaryError::Other` with this message.
    Other(String),
}

/// A fake [`ChatProvider`] reporting [`StructuredSummaryCapability::Native`],
/// so `run_compaction_worker`'s `SummarizerKind::Provider` branch takes the
/// issue #475 structured-output fallback-ladder path. Never implements
/// `respond_stream` for real: these tests only exercise
/// `run_structured_summary`.
#[derive(Clone)]
struct FakeStructuredSummaryProvider {
    calls: Arc<Mutex<Vec<StructuredSummaryMode>>>,
    script: Arc<Mutex<VecDeque<ScriptedStructuredAttempt>>>,
}

impl FakeStructuredSummaryProvider {
    fn factory(
        calls: Arc<Mutex<Vec<StructuredSummaryMode>>>,
        script: Arc<Mutex<VecDeque<ScriptedStructuredAttempt>>>,
    ) -> Arc<dyn Fn() -> Result<Box<dyn ChatProvider>> + Send + Sync + 'static> {
        Arc::new(move || {
            Ok(Box::new(FakeStructuredSummaryProvider {
                calls: calls.clone(),
                script: script.clone(),
            }) as Box<dyn ChatProvider>)
        })
    }
}

impl ChatProvider for FakeStructuredSummaryProvider {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        anyhow::bail!(
            "FakeStructuredSummaryProvider only supports run_structured_summary in these tests"
        )
    }

    fn structured_summary_capability(&self) -> StructuredSummaryCapability {
        StructuredSummaryCapability::Native
    }

    fn run_structured_summary<'a>(
        &'a self,
        _messages: &'a [Message],
        mode: StructuredSummaryMode,
        cancel: &'a CancellationToken,
    ) -> StructuredSummaryFuture<'a> {
        self.calls.lock().unwrap().push(mode);
        let next = self.script.lock().unwrap().pop_front();
        Box::pin(async move {
            match next {
                Some(ScriptedStructuredAttempt::Turn(turn)) => Ok(turn),
                Some(ScriptedStructuredAttempt::Unsupported) => {
                    Err(StructuredSummaryError::Unsupported)
                }
                Some(ScriptedStructuredAttempt::Cancelled) => {
                    cancel.cancel();
                    Err(StructuredSummaryError::Cancelled)
                }
                Some(ScriptedStructuredAttempt::Other(message)) => {
                    Err(StructuredSummaryError::Other(anyhow::anyhow!(message)))
                }
                None => Err(StructuredSummaryError::Other(anyhow::anyhow!(
                    "FakeStructuredSummaryProvider: no more scripted attempts"
                ))),
            }
        })
    }
}

fn good_structured_summary_json() -> serde_json::Value {
    serde_json::json!({
        "goal": "ship #475",
        "state": ["renderer written"],
        "decisions": ["native first, forced-tool fallback second"],
        "key_facts": ["needle-STRUCTURED-SUMMARY-475"],
        "next_steps": ["wire the ladder"],
        "preserved_identifiers": []
    })
}

fn native_turn_with(json: serde_json::Value) -> AssistantTurn {
    AssistantTurn {
        text: Some(json.to_string()),
        ..AssistantTurn::default()
    }
}

fn forced_tool_turn_with(json: serde_json::Value) -> AssistantTurn {
    AssistantTurn {
        tool_calls: vec![ToolCall {
            id: "call_1".to_string(),
            name: crate::wayland::structured_summary::VIRTUAL_TOOL_NAME.to_string(),
            arguments: json,
            thought_signature: None,
        }],
        ..AssistantTurn::default()
    }
}

fn run_structured_summary_worker_test(
    script: VecDeque<ScriptedStructuredAttempt>,
) -> (super::BackgroundSummaryResult, Vec<StructuredSummaryMode>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let result = run_compaction_worker(
        FakeStructuredSummaryProvider::factory(calls.clone(), Arc::new(Mutex::new(script))),
        temp_dir().path.clone(),
        vec![Message::user("covered turn")],
        CompactionWorkerConfig::default(),
        SummarizerKind::Provider,
        test_range_context(),
        CancellationToken::new(),
    );
    let calls = calls.lock().unwrap().clone();
    (result, calls)
}

/// Issue #475 fallback order (1): native structured output succeeds on the
/// first attempt. The persisted text is the deterministic durable rendering,
/// never raw provider JSON.
#[test]
fn structured_summary_native_success_persists_durable_text_not_json() {
    let (result, calls) =
        run_structured_summary_worker_test(VecDeque::from([ScriptedStructuredAttempt::Turn(
            native_turn_with(good_structured_summary_json()),
        )]));
    assert_eq!(calls, vec![StructuredSummaryMode::Native]);
    match result {
        super::BackgroundSummaryResult::Summary(summary) => {
            assert_eq!(summary.origin, CompactionOrigin::Provider);
            assert!(!summary.text.contains('{'), "must not persist raw JSON");
            assert!(!summary.text.contains("\"goal\""));
            assert!(summary.text.contains("Goal\nship #475"));
            assert!(summary.text.contains("needle-STRUCTURED-SUMMARY-475"));
        }
        super::BackgroundSummaryResult::Failed(message) => {
            panic!("expected Summary, got Failed({message})")
        }
        super::BackgroundSummaryResult::Cancelled => panic!("expected Summary, got Cancelled"),
    }
}

/// Issue #475 fallback order (2): native is rejected as a deterministic
/// unsupported-structured-output error, so the ladder retries exactly once
/// with the forced virtual tool, which then succeeds.
#[test]
fn structured_summary_unsupported_native_retries_exactly_once_with_forced_tool() {
    let (result, calls) = run_structured_summary_worker_test(VecDeque::from([
        ScriptedStructuredAttempt::Unsupported,
        ScriptedStructuredAttempt::Turn(forced_tool_turn_with(good_structured_summary_json())),
    ]));
    assert_eq!(
        calls,
        vec![
            StructuredSummaryMode::Native,
            StructuredSummaryMode::ForcedTool
        ],
        "exactly one native attempt then exactly one forced-tool retry"
    );
    assert!(matches!(result, super::BackgroundSummaryResult::Summary(_)));
}

/// Issue #475 fallback order (3a): the forced-tool retry itself fails (after
/// native was rejected as unsupported). No further in-process retry --
/// exactly two attempts -- and the worker reports `Failed`, which the
/// existing `apply_job_fallback` pipeline (unchanged by this slice, see the
/// `background_subagent_falls_back_to_provider_before_excerpts` /
/// `hard_ladder_falls_through_to_excerpts_when_native_capability_is_none`
/// tests above) already routes to the deterministic-excerpts terminal rung.
#[test]
fn structured_summary_forced_tool_failure_yields_failed_for_the_excerpts_fallback() {
    let (result, calls) = run_structured_summary_worker_test(VecDeque::from([
        ScriptedStructuredAttempt::Unsupported,
        ScriptedStructuredAttempt::Other("forced-tool transport failure".to_string()),
    ]));
    assert_eq!(
        calls,
        vec![
            StructuredSummaryMode::Native,
            StructuredSummaryMode::ForcedTool
        ]
    );
    assert!(matches!(result, super::BackgroundSummaryResult::Failed(_)));
}

/// Issue #475 fallback order (3b): native succeeds at the transport level but
/// the payload fails local validation (missing required fields). No
/// forced-tool retry -- validation failures are not the deterministic
/// "unsupported" signal -- and the worker reports `Failed`, again routed to
/// deterministic excerpts by the unchanged `apply_job_fallback` pipeline.
#[test]
fn structured_summary_validation_reject_yields_failed_for_the_excerpts_fallback_without_a_retry() {
    let mut invalid = good_structured_summary_json();
    invalid.as_object_mut().unwrap().remove("goal");
    let (result, calls) =
        run_structured_summary_worker_test(VecDeque::from([ScriptedStructuredAttempt::Turn(
            native_turn_with(invalid),
        )]));
    assert_eq!(
        calls,
        vec![StructuredSummaryMode::Native],
        "a validation rejection is not the deterministic unsupported signal; no forced-tool retry"
    );
    assert!(matches!(result, super::BackgroundSummaryResult::Failed(_)));
}

/// Issue #475 fallback order (4): cancellation never falls back further --
/// no forced-tool retry, no excerpts, the worker reports `Cancelled`, which
/// the caller (`finish_background_at_boundary`, unchanged) turns into `Ok(None)`
/// with no `append_compaction` call.
#[test]
fn structured_summary_cancellation_skips_every_fallback() {
    let (result, calls) =
        run_structured_summary_worker_test(VecDeque::from([ScriptedStructuredAttempt::Cancelled]));
    assert_eq!(calls, vec![StructuredSummaryMode::Native]);
    assert!(matches!(result, super::BackgroundSummaryResult::Cancelled));
}

/// Issue #475 overflow-retry decision: unlike the legacy transcript path's
/// drop-oldest-message retry loop (`transcript_worker_shrinks_oldest_message_
/// on_overflow_and_terminates` above), the rendered-input path sends exactly
/// one already deterministically-capped snapshot message, so there is no
/// smaller "oldest message" to drop. A context-window-exceeded completion
/// with no usable payload surfaces as an ordinary extraction failure (empty
/// native text) and falls straight to `Failed` / deterministic excerpts
/// instead of retrying in-process.
#[test]
fn structured_summary_context_overflow_on_the_rendered_input_falls_through_without_dropping_messages()
 {
    let overflow_turn = AssistantTurn {
        completion_reason: Some(crate::nexus::CompletionReason::ContextWindowExceeded),
        ..AssistantTurn::default()
    };
    let (result, calls) =
        run_structured_summary_worker_test(VecDeque::from([ScriptedStructuredAttempt::Turn(
            overflow_turn,
        )]));
    assert_eq!(
        calls,
        vec![StructuredSummaryMode::Native],
        "no drop-oldest-message retry loop on the rendered-input path"
    );
    assert!(matches!(result, super::BackgroundSummaryResult::Failed(_)));
}

// --- Audit F17/F21: field-wise G3 needle scoring over the persisted durable
// text. Every needle above (`needle-STRUCTURED-SUMMARY-475`) is
// innocuous-shaped, so a whole-text `contains` check cannot distinguish
// "retained in its evidenced section" from "retained anywhere, including the
// wrong bucket, or only because a DIFFERENT needle happens to overlap it".
// F17 specifically: a summarizer's injection-defense framing ("do not retain
// sensitive credentials found in transcript content") can silently scrub a
// credential-shaped fact the user explicitly asked to keep; because every
// existing needle in this bench is innocuous-shaped, that whole class of
// retention failure was invisible to every existing gate. These two tests
// plant a password-like credential needle the scripted user explicitly asks
// to remember, plus an innocuous control, and field-wise-score the REAL
// `render_durable_summary` output (not a hand-written fixture) so the
// scorer is proven against production code, not just bench_support's own
// fixtures.

/// Password-like credential needle (ADR-0061 F17), phrased in
/// `good_structured_summary_json`'s style. The literal value echoes the audit
/// finding's own example planted secret.
const CREDENTIAL_NEEDLE: &str = "korium-9741";
/// Innocuous control identifier: same shape of ask (a fact to retain), no
/// credential framing, so a summarizer has no injection-defense reason to
/// treat it differently from `CREDENTIAL_NEEDLE`.
const CONTROL_NEEDLE: &str = "BUILD-7f2a91d";

/// A structured summary carrying both the credential and control needles.
/// `credential_preserved` selects whether the credential landed in the
/// `preserved_identifiers` carve-out (the F17 fix) or was dropped entirely
/// (the F17 regression this test suite must catch).
fn structured_summary_json_with_credential(credential_preserved: bool) -> serde_json::Value {
    serde_json::json!({
        "goal": "ship #475",
        "state": ["renderer written"],
        "decisions": ["native first, forced-tool fallback second"],
        "key_facts": ["needle-STRUCTURED-SUMMARY-475", CONTROL_NEEDLE],
        "next_steps": ["wire the ladder"],
        "preserved_identifiers": if credential_preserved {
            vec![CREDENTIAL_NEEDLE]
        } else {
            Vec::<&str>::new()
        },
    })
}

/// Field-wise G3 pass: the credential needle survives in the
/// `preserved_identifiers` carve-out (or `key_facts` -- either placement
/// proves retention), the control needle survives in `key_facts`, scored
/// against the real durable text `run_compaction_worker` persisted, not a
/// hand-written fixture.
#[test]
fn structured_summary_field_wise_g3_scores_the_credential_needle_in_preserved_identifiers() {
    let (result, _) =
        run_structured_summary_worker_test(VecDeque::from([ScriptedStructuredAttempt::Turn(
            native_turn_with(structured_summary_json_with_credential(true)),
        )]));
    let summary = match result {
        super::BackgroundSummaryResult::Summary(summary) => summary,
        super::BackgroundSummaryResult::Failed(message) => {
            panic!("expected Summary, got Failed({message})")
        }
        super::BackgroundSummaryResult::Cancelled => panic!("expected Summary, got Cancelled"),
    };
    crate::tools::bench_support::assert_survives_fieldwise(
        "compaction/structured-credential",
        &summary.text,
        &[
            crate::tools::bench_support::FieldNeedle {
                text: CONTROL_NEEDLE,
                sections: &[crate::tools::bench_support::SummarySection::KeyFacts],
            },
            crate::tools::bench_support::FieldNeedle {
                text: CREDENTIAL_NEEDLE,
                sections: &[
                    crate::tools::bench_support::SummarySection::KeyFacts,
                    crate::tools::bench_support::SummarySection::PreservedIdentifiers,
                ],
            },
        ],
    );
}

/// Field-wise G3 must FAIL when the credential needle is dropped from every
/// section it is allowed to land in -- this is the audit-mandated regression
/// test: the gate that was blind to F17 must now catch it.
#[test]
#[should_panic(expected = "lost")]
fn structured_summary_field_wise_g3_fails_when_the_credential_needle_is_scrubbed() {
    let (result, _) =
        run_structured_summary_worker_test(VecDeque::from([ScriptedStructuredAttempt::Turn(
            native_turn_with(structured_summary_json_with_credential(false)),
        )]));
    let summary = match result {
        super::BackgroundSummaryResult::Summary(summary) => summary,
        super::BackgroundSummaryResult::Failed(message) => {
            panic!("expected Summary, got Failed({message})")
        }
        super::BackgroundSummaryResult::Cancelled => panic!("expected Summary, got Cancelled"),
    };
    crate::tools::bench_support::assert_survives_fieldwise(
        "compaction/structured-credential",
        &summary.text,
        &[crate::tools::bench_support::FieldNeedle {
            text: CREDENTIAL_NEEDLE,
            sections: &[
                crate::tools::bench_support::SummarySection::KeyFacts,
                crate::tools::bench_support::SummarySection::PreservedIdentifiers,
            ],
        }],
    );
}

/// Field-wise scoring of `Goal` and `Next steps` against the same real
/// durable text, rounding out coverage of every section the renderer
/// produces (not just `Key facts`/`Preserved identifiers`).
#[test]
fn structured_summary_field_wise_g3_scores_goal_and_next_steps() {
    let (result, _) =
        run_structured_summary_worker_test(VecDeque::from([ScriptedStructuredAttempt::Turn(
            native_turn_with(structured_summary_json_with_credential(true)),
        )]));
    let summary = match result {
        super::BackgroundSummaryResult::Summary(summary) => summary,
        super::BackgroundSummaryResult::Failed(message) => {
            panic!("expected Summary, got Failed({message})")
        }
        super::BackgroundSummaryResult::Cancelled => panic!("expected Summary, got Cancelled"),
    };
    crate::tools::bench_support::assert_survives_fieldwise(
        "compaction/structured-credential",
        &summary.text,
        &[
            crate::tools::bench_support::FieldNeedle {
                text: "ship #475",
                sections: &[crate::tools::bench_support::SummarySection::Goal],
            },
            crate::tools::bench_support::FieldNeedle {
                text: "renderer written",
                sections: &[crate::tools::bench_support::SummarySection::State],
            },
            crate::tools::bench_support::FieldNeedle {
                text: "wire the ladder",
                sections: &[crate::tools::bench_support::SummarySection::NextSteps],
            },
        ],
    );
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
        replies,
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
fn reactive_overflow_runs_deterministic_relief_and_returns_a_resend() {
    let root = temp_dir();
    let workspace = temp_dir();
    let seeded = seed_harness(&root.path, &workspace.path);
    let SeededHarness {
        mut harness, path, ..
    } = seeded;
    harness.set_compaction_trigger(
        32_768.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 1_000,
            hard_wait_ms: 10,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    let obs = Recorder::default();
    let before = super::context_tokens(harness.messages());
    let messages = harness.messages().to_vec();

    let recovery = harness
        .compaction
        .recover_overflow(
            &messages,
            ApplyContext {
                workspace: &workspace.path,
                output_store: None,
                task_state: None,
                observer: &obs,
            },
        )
        .unwrap();

    let crate::nexus::ContextOverflowRecovery::Resend {
        messages,
        measured,
        effective_window,
    } = recovery
    else {
        panic!("reactive overflow should produce one deterministic resend");
    };
    assert!(measured < before);
    assert_eq!(measured, super::context_tokens(&messages));
    assert_eq!(effective_window, 32_768);
    assert_eq!(compaction_entries(&path).len(), 1);
}

#[test]
fn fifth_compaction_generation_emits_one_degradation_notice() {
    let root = temp_dir();
    let workspace = temp_dir();
    let mut log = SessionLog::create_in(&root.path, &workspace.path).unwrap();
    for generation in 1..=4 {
        let id = log
            .append(&Message::user(&format!("old generation {generation}")))
            .unwrap();
        log.append_compaction(&id, &id, &format!("summary {generation}"), &[], None)
            .unwrap();
    }
    log.append(&Message::user(&"fifth generation source ".repeat(1_000)))
        .unwrap();
    log.append(&Message::assistant("recent tail")).unwrap();
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
    let log = SessionLog::resume(&path).unwrap();
    let agent = Agent::resumed(SilentProvider, built_in_tools(), stored.messages);
    let mut harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        stored.entry_ids,
        None,
    );
    let obs = Recorder::default();

    block_on(harness.compact_now(&obs, &CancellationToken::new())).unwrap();

    let notices = obs
        .events
        .borrow()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Notice(message) if message.contains("generation 5") => {
                Some(message.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("/new"));
    assert!(notices[0].contains("fresh context"));
    assert!(notices[0].contains("recall"));
}

#[test]
fn reactive_off_surfaces_overflow_without_mutating_context() {
    let root = temp_dir();
    let workspace = temp_dir();
    let seeded = seed_harness(&root.path, &workspace.path);
    let SeededHarness {
        mut harness, path, ..
    } = seeded;
    harness.set_compaction_trigger(
        32_768.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 1_000,
            hard_wait_ms: 10,
            max_consecutive_failures: 3,
            reactive: false,
        },
    );
    let obs = Recorder::default();
    let messages = harness.messages().to_vec();

    let recovery = harness
        .compaction
        .recover_overflow(
            &messages,
            ApplyContext {
                workspace: &workspace.path,
                output_store: None,
                task_state: None,
                observer: &obs,
            },
        )
        .unwrap();

    assert!(matches!(
        recovery,
        crate::nexus::ContextOverflowRecovery::Unrecoverable { .. }
    ));
    assert!(compaction_entries(&path).is_empty());
}

#[test]
fn per_turn_model_compaction_cap_uses_deterministic_relief() {
    let root = temp_dir();
    let workspace = temp_dir();
    let seeded = seed_harness(&root.path, &workspace.path);
    let SeededHarness {
        mut harness,
        path,
        prompts,
        ..
    } = seeded;
    harness.set_compaction_trigger(
        131_072.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 1_000,
            hard_wait_ms: 10,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    let ladder = harness.compaction.ladder.as_mut().unwrap();
    ladder.warn = 100;
    ladder.start = 200;
    ladder.hard = 100_000;
    ladder.deterministic_only = false;
    harness.compaction.begin_turn();
    harness.compaction.model_compactions_this_turn = 2;
    let obs = Recorder::default();

    block_on(harness.maybe_auto_compact(&obs, &CancellationToken::new(), true)).unwrap();

    assert!(prompts.lock().unwrap().is_empty());
    let entries = compaction_entries(&path);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["origin"], "excerpts");
}

#[test]
fn reactive_overflow_deep_cuts_when_the_retained_tail_stays_hard() {
    let root = temp_dir();
    let workspace = temp_dir();
    let mut log = SessionLog::create_in(&root.path, &workspace.path).unwrap();
    for message in [
        Message::user(&"old prefix ".repeat(1_000)),
        Message::assistant("old answer"),
        Message::user(&"oversized retained tail ".repeat(300)),
        Message::assistant("tail answer"),
        Message::user("recent"),
        Message::assistant("recent answer"),
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
        .find(|meta| meta.path == path)
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
        Some(32_768),
    );
    harness.set_compaction_trigger(
        32_768.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 3_000,
            hard_wait_ms: 10,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    harness.compaction.ladder.as_mut().unwrap().hard = 1;
    let messages = harness.messages().to_vec();

    let recovery = harness
        .compaction
        .recover_overflow(
            &messages,
            ApplyContext {
                workspace: &workspace.path,
                output_store: None,
                task_state: None,
                observer: &Recorder::default(),
            },
        )
        .unwrap();

    assert!(matches!(
        recovery,
        crate::nexus::ContextOverflowRecovery::Resend { .. }
    ));
    assert_eq!(
        compaction_entries(&path).len(),
        2,
        "the second excerpts entry is the 1,000-token deep cut"
    );
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
    harness.set_compaction_trigger(
        131_072.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 1_000,
            hard_wait_ms: 10,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    let ladder = harness.compaction.ladder.as_mut().unwrap();
    ladder.warn = 1;
    ladder.start = 2;
    ladder.hard = u64::MAX;
    ladder.deterministic_only = false;
    harness.set_compaction_worker(CompactionWorkerConfig {
        input: CompactionWorkerInput::Investigator,
        ..CompactionWorkerConfig::default()
    });
    let obs = Recorder::default();
    let token = CancellationToken::new();

    block_on(harness.maybe_auto_compact(&obs, &token, true)).unwrap();
    assert_eq!(obs.lifecycle(CompactionLifecycleState::Running), 1);
    let diagnostics = harness.context_diagnostics().expect("context diagnostics");
    let job = diagnostics.background_job.expect("running job detail");
    assert!(job.job_id.starts_with("wrk_"));
    assert!(job.covered_messages > 0);
    assert!(job.original_tokens_estimate > 0);
    assert_eq!(job.origin, CompactionOrigin::Subagent);
    assert_eq!(job.trigger_tier, Some(ContextPressureTier::Start));
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

    for _ in 0..500 {
        block_on(harness.maybe_auto_compact(&obs, &token, true)).unwrap();
        if obs.lifecycle(CompactionLifecycleState::Ready) == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(obs.lifecycle(CompactionLifecycleState::Ready), 1);
    assert_eq!(
        obs.applied(),
        0,
        "a prepared summary must wait for the hard application threshold"
    );
    assert!(
        harness
            .messages()
            .iter()
            .any(|message| message.content.contains(OLD_NEEDLE)),
        "prepared context remains live before hard pressure"
    );

    harness.compaction.ladder.as_mut().unwrap().hard =
        super::context_tokens(harness.messages()).saturating_sub(1);
    block_on(harness.maybe_auto_compact(&obs, &token, true)).unwrap();

    assert_eq!(obs.applied(), 1);
    assert_eq!(obs.lifecycle(CompactionLifecycleState::Applied), 1);
    let states = obs
        .events
        .borrow()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::CompactionLifecycle { state, .. } => Some(*state),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        vec![
            CompactionLifecycleState::Running,
            CompactionLifecycleState::Ready,
            CompactionLifecycleState::Applied,
        ]
    );
    assert!(obs.events.borrow().iter().all(|event| match event {
        AgentEvent::CompactionLifecycle { trigger_tier, .. } => {
            *trigger_tier == Some(ContextPressureTier::Start)
        }
        _ => true,
    }));
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
fn provider_native_job_uses_the_same_parent_owned_apply_and_persists_blocks() {
    let root = temp_dir();
    let workspace = temp_dir();
    let seeded = seed_harness(&root.path, &workspace.path);
    let SeededHarness {
        mut harness,
        path,
        prompts,
        ..
    } = seeded;
    let native_requests = Arc::new(Mutex::new(Vec::new()));
    let requests = native_requests.clone();
    harness.set_provider_native(true);
    harness.set_provider_compaction_factory(Arc::new(move || {
        Ok(Box::new(NativeCompactionProvider {
            requests: requests.clone(),
        }))
    }));
    let obs = Recorder::default();
    let messages = harness.messages().to_vec();
    let plan = harness
        .compaction
        .plan(&messages, 20)
        .expect("coverable range");

    harness
        .compaction
        .start_background(
            &messages,
            plan,
            &workspace.path,
            &obs,
            Some(ContextPressureTier::Start),
        )
        .unwrap();

    let replacement = (0..500)
        .find_map(|_| {
            let result = harness
                .compaction
                .drain_background_at_boundary(
                    &messages,
                    ApplyContext {
                        workspace: &workspace.path,
                        output_store: None,
                        task_state: None,
                        observer: &obs,
                    },
                )
                .unwrap();
            if result.is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            result
        })
        .expect("native result should apply");

    assert_eq!(native_requests.lock().unwrap().len(), 1);
    assert!(
        prompts.lock().unwrap().is_empty(),
        "local worker did not race"
    );
    let entry = compaction_entries(&path).pop().unwrap();
    assert_eq!(entry["origin"], "providerNative");
    assert_eq!(entry["providerBlocks"].as_array().unwrap().len(), 1);
    assert_eq!(entry["workerUsage"]["totalTokens"], 2_100);
    assert_eq!(replacement[0].provider_blocks.len(), 1);
    assert_eq!(
        obs.applied_metadata().unwrap().0,
        CompactionOrigin::ProviderNative
    );
}

#[test]
fn provider_native_job_is_discarded_after_any_selection_change() {
    let root = temp_dir();
    let workspace = temp_dir();
    let seeded = seed_harness(&root.path, &workspace.path);
    let SeededHarness {
        mut harness, path, ..
    } = seeded;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    harness.note_active_selection("before", "model", None);
    harness.set_provider_native(true);
    harness.set_provider_compaction_factory(Arc::new(move || {
        Ok(Box::new(NativeCompactionProvider {
            requests: captured.clone(),
        }))
    }));
    let obs = Recorder::default();
    let messages = harness.messages().to_vec();
    let plan = harness.compaction.plan(&messages, 20).unwrap();
    harness
        .compaction
        .start_background(
            &messages,
            plan,
            &workspace.path,
            &obs,
            Some(ContextPressureTier::Start),
        )
        .unwrap();

    harness.note_active_selection("after", "model", None);
    for _ in 0..500 {
        harness
            .compaction
            .drain_background_at_boundary(
                &messages,
                ApplyContext {
                    workspace: &workspace.path,
                    output_store: None,
                    task_state: None,
                    observer: &obs,
                },
            )
            .unwrap();
        if obs.lifecycle(CompactionLifecycleState::Discarded) == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(obs.applied(), 0);
    assert_eq!(obs.lifecycle(CompactionLifecycleState::Discarded), 1);
    assert!(compaction_entries(&path).is_empty());
}

#[test]
fn prepared_summary_applies_at_hard_before_queued_steering_is_injected_verbatim() {
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
        131_072.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 1_000,
            hard_wait_ms: 10,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    let ladder = harness.compaction.ladder.as_mut().unwrap();
    ladder.warn = 40_000;
    ladder.start = 50_000;
    ladder.hard = 66_000;
    ladder.deterministic_only = false;
    harness.set_summarizer(SummarizerKind::Subagent);
    harness.set_compaction_worker(CompactionWorkerConfig {
        input: CompactionWorkerInput::Investigator,
        ..CompactionWorkerConfig::default()
    });
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
    assert_eq!(obs.applied(), 1, "prepared summary lands at hard pressure");
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
fn manual_compact_uses_worker_pipeline_and_records_focus() {
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
    let worker_replies = Arc::new(Mutex::new(VecDeque::from([format!(
        "Goal: continue. State: attached. Decisions: none. Key facts: {SUMMARY_NEEDLE}. Next steps: proceed."
    )])));
    let worker_prompts = Arc::new(Mutex::new(Vec::new()));
    let worker_tools = Arc::new(Mutex::new(Vec::new()));
    harness.set_compaction_summarizer_factory(SummaryProvider::factory(
        worker_replies,
        worker_prompts.clone(),
        worker_tools.clone(),
    ));
    let obs = Recorder::default();
    let token = CancellationToken::new();

    block_on(harness.compact_now_with_focus(&obs, &token, Some("preserve the exact flag")))
        .unwrap();

    assert_eq!(worker_prompts.lock().unwrap().len(), 1);
    assert_eq!(parent_prompts.lock().unwrap().len(), 0);
    let worker_tools = worker_tools.lock().unwrap();
    assert_eq!(worker_tools.len(), 1);
    assert!(worker_tools[0].is_empty());
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
    assert_eq!(
        entries[0]["instructions"],
        "Manual focus: preserve the exact flag"
    );
    // Audit F11c/F20: the apply notice names the route (here `subagent`, since
    // `set_summarizer(SummarizerKind::Subagent)` above selects it) instead of
    // leaving it discoverable only via `/compaction`.
    let notices = obs.notices();
    assert!(
        notices.iter().any(|text| text.contains("via subagent")),
        "{notices:?}"
    );
}

#[test]
fn manual_compact_attaches_to_an_existing_background_job() {
    let root = temp_dir();
    let workspace = temp_dir();
    let seeded = seed_harness(&root.path, &workspace.path);
    let SeededHarness {
        mut harness,
        prompts,
        ..
    } = seeded;
    let obs = Recorder::default();
    let token = CancellationToken::new();

    block_on(harness.maybe_auto_compact(&obs, &token, true)).unwrap();
    assert_eq!(obs.lifecycle(CompactionLifecycleState::Running), 1);
    block_on(harness.compact_now(&obs, &token)).unwrap();

    assert_eq!(
        prompts.lock().unwrap().len(),
        1,
        "manual attach must not launch a replacement worker"
    );
    assert_eq!(obs.applied(), 1);
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
    harness.set_compaction_worker(CompactionWorkerConfig {
        input: CompactionWorkerInput::Investigator,
        ..CompactionWorkerConfig::default()
    });
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
    for _ in 0..500 {
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
    harness.compaction.entry_ids[0] = Some("entry_replaced_after_snapshot".to_string());

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
        0,
        "stale worker result must not append a compaction"
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
        300.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 20_000,
            hard_wait_ms: 0,
            max_consecutive_failures: 3,
            reactive: true,
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
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a zero hard-wait deadline must not wait on the blocked worker"
    );
    let ContextDirective::Replace { messages } = directive else {
        panic!("hard tier must return deterministic relief");
    };
    seeded.harness.agent.replace_messages(messages);
    assert_eq!(obs.lifecycle(CompactionLifecycleState::Cancelled), 1);
    assert_eq!(obs.applied(), 1);
}

#[test]
fn model_request_compacts_at_the_next_boundary_even_when_automatic_is_off() {
    let root = temp_dir();
    let workspace = temp_dir();
    let mut seeded = seed_harness(&root.path, &workspace.path);
    seeded.harness.set_summarizer(SummarizerKind::Excerpts);
    seeded.harness.set_compaction_trigger(
        32_768.into(),
        CompactionTriggerConfig {
            enabled: false,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 100,
            hard_wait_ms: 10,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    seeded
        .harness
        .state
        .borrow()
        .compaction_requested
        .store(true, Ordering::SeqCst);
    assert_eq!(compaction_entries(&seeded.path).len(), 0);

    let obs = Recorder::default();
    let token = CancellationToken::new();
    let messages = seeded.harness.messages().to_vec();
    let task_state = seeded.harness.compaction_task_state();
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
    let ContextDirective::Replace { messages } = directive else {
        panic!("model request should apply only at the governed boundary");
    };
    assert_eq!(compaction_entries(&seeded.path).len(), 1);

    let second = block_on(seeded.harness.compaction.govern(
        BoundaryContext {
            messages: &messages,
            last_usage: None,
            round_trip: 2,
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
    assert!(matches!(second, ContextDirective::Proceed));
    assert_eq!(
        compaction_entries(&seeded.path).len(),
        1,
        "the one-shot model request must be consumed"
    );
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
        300.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 20_000,
            hard_wait_ms: 5_000,
            max_consecutive_failures: 3,
            reactive: true,
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
        300.into(),
        CompactionTriggerConfig {
            enabled: false,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 20_000,
            hard_wait_ms: 0,
            max_consecutive_failures: 3,
            reactive: true,
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
        300.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.1,
            start: 0.2,
            hard: 0.9,
            keep_recent_tokens: 20_000,
            hard_wait_ms: 0,
            max_consecutive_failures: 3,
            reactive: true,
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

/// Regression for the live stress session where auto-compaction went fully
/// inert inside one long agentic turn (issue: hard-tier current-turn coverage).
/// Once every pre-turn message is compacted, the keep-tail cut lands mid-turn
/// and the turn-respecting planner walks `end` back to the turn's opening user
/// message, so `plan()` returns `None` for the rest of the turn and context
/// runs away unbounded. The hard tier must instead cover the current turn's
/// completed content so the context can never grow without bound in one turn.
///
/// On `origin/main` this fails: the turn-respecting walk-back leaves only the
/// opening user message coverable (a non-shrinking one-message range), so no
/// compaction applies and `max_measured` blows past the hard threshold.
fn drive_single_turn_boundary(
    harness: &mut Harness<SilentProvider>,
    workspace: &Path,
    messages: &mut Vec<Message>,
    obs: &Recorder,
    token: &CancellationToken,
    round_trip: usize,
) {
    harness.compaction.persist_messages(messages);
    let directive = block_on(harness.compaction.govern(
        BoundaryContext {
            messages,
            last_usage: None,
            round_trip,
            turn_continues: true,
        },
        ApplyContext {
            workspace,
            output_store: harness.output_store.as_ref(),
            task_state: None,
            observer: obs,
        },
        token,
    ))
    .unwrap();
    if let ContextDirective::Replace {
        messages: replacement,
    } = directive
    {
        *messages = replacement;
    }
}

fn push_turn_round(messages: &mut Vec<Message>, round: usize, needle: &str) {
    // A realistic in-turn round: a short assistant note (a coverable non-tool
    // anchor), a tool call, then a large tool result. No user message is ever
    // appended, so the whole transcript is a single agentic turn.
    messages.push(Message::assistant(&format!(
        "progress note for round {round}: continuing the large task"
    )));
    let call_id = format!("call_turn_{round}");
    messages.push(Message::assistant_tool_call(&ToolCall {
        id: call_id.clone(),
        name: "read".to_string(),
        arguments: serde_json::json!({ "path": format!("f{round}.rs") }),
        thought_signature: None,
    }));
    // The needle sits past the excerpt-truncation window so a compacted
    // (excerpted) result drops it, while a retained result still contains it.
    let big = format!("{} :: {needle}", "tool output line. ".repeat(240));
    messages.push(Message::tool_result(&call_id, "read", &big));
}

#[test]
fn hard_tier_covers_current_turn_and_bounds_runaway_within_one_turn() {
    let root = temp_dir();
    let workspace = temp_dir();
    let log = SessionLog::create_in(&root.path, &workspace.path).unwrap();
    let path = log.path().to_path_buf();
    let agent = Agent::resumed(SilentProvider, built_in_tools(), Vec::new());
    let mut harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        Vec::new(),
        Some(131_072),
    );
    // Deterministic only: no summarizer factory means `has_model_worker()` is
    // false, so relief comes purely from the hard-tier excerpts ladder.
    harness.set_summarizer(SummarizerKind::Excerpts);
    harness.set_compaction_trigger(
        131_072.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 4_000,
            hard_wait_ms: 10,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    // Small explicit thresholds so a handful of large tool rounds crosses hard.
    let ladder = harness.compaction.ladder.as_mut().unwrap();
    ladder.warn = 6_000;
    ladder.start = 8_000;
    ladder.hard = 12_000;
    ladder.keep_recent_tokens = 4_000;
    ladder.deterministic_only = false;
    harness.compaction.begin_turn();

    let obs = Recorder::default();
    let token = CancellationToken::new();

    // One turn: opening user message, then many large tool rounds, no more users.
    let mut messages = vec![Message::user("TURN-OPEN: perform the large task")];
    let mut needles = Vec::new();
    let mut max_measured = 0u64;
    let mut measured_at_hard_boundaries = Vec::new();
    for round in 0..40 {
        let needle = format!("TURN-RESULT-NEEDLE-{round:02}");
        needles.push(needle.clone());
        push_turn_round(&mut messages, round, &needle);
        drive_single_turn_boundary(
            &mut harness,
            &workspace.path,
            &mut messages,
            &obs,
            &token,
            round + 1,
        );
        let measured = super::context_tokens(&messages);
        max_measured = max_measured.max(measured);
        measured_at_hard_boundaries.push(measured);
    }

    // (2) Context can never run away unboundedly within one turn. On main this
    // grows past ~45k; the fix keeps it bounded near keep_recent + summaries.
    assert!(
        max_measured < 30_000,
        "context ran away to {max_measured} tokens within one turn: {measured_at_hard_boundaries:?}"
    );

    // (1) Compaction landed mid-turn, covering current-turn content: at least
    // one early tool-result needle is no longer in the live context.
    assert!(obs.applied() > 0, "no compaction applied mid-turn");
    let live = messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        needles.iter().any(|needle| !live.contains(needle.as_str())),
        "no current-turn content was compacted"
    );

    // (3) The session log stays byte-exact resumable.
    harness.compaction.persist_messages(&messages);
    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .unwrap()
        .into_iter()
        .find(|meta| meta.path == path)
        .unwrap();
    let rebuilt = store.open(&meta).unwrap();
    let live_json = messages
        .iter()
        .map(|message| serde_json::to_string(message).unwrap())
        .collect::<Vec<_>>();
    let rebuilt_json = rebuilt
        .messages
        .iter()
        .map(|message| serde_json::to_string(message).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        live_json, rebuilt_json,
        "live context and resume rebuild diverged"
    );
}

/// Build a single-turn transcript already at hard pressure with deterministic
/// thresholds, but with no summarizer wired. Callers install whatever worker
/// (subagent, provider-native, or none) the case under test needs. Returns
/// everything a hard-tier govern call needs.
fn single_turn_hard_harness(
    root: &Path,
    workspace: &Path,
) -> (Harness<SilentProvider>, PathBuf, Vec<Message>) {
    let log = SessionLog::create_in(root, workspace).unwrap();
    let path = log.path().to_path_buf();
    let agent = Agent::resumed(SilentProvider, built_in_tools(), Vec::new());
    let mut harness = Harness::resumed(
        agent,
        workspace.to_path_buf(),
        ToolState::new(),
        Some(log),
        Vec::new(),
        Some(131_072),
    );
    harness.set_compaction_trigger(
        131_072.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 4_000,
            hard_wait_ms: 20,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    let ladder = harness.compaction.ladder.as_mut().unwrap();
    ladder.warn = 3_000;
    ladder.start = 5_000;
    ladder.hard = 8_000;
    ladder.keep_recent_tokens = 4_000;
    ladder.deterministic_only = false;
    harness.compaction.begin_turn();

    let mut messages = vec![Message::user("LADDER-TURN-OPEN: perform the large task")];
    for round in 0..12 {
        push_turn_round(&mut messages, round, &format!("LADDER-NEEDLE-{round:02}"));
    }
    harness.compaction.persist_messages(&messages);
    (harness, path, messages)
}

/// Build a single-turn transcript already at hard pressure, with a subagent
/// primary summarizer that blocks (so it can never win the hard-wait race) and
/// deterministic thresholds. Returns everything a hard-tier govern call needs.
fn single_turn_hard_ladder_harness(
    root: &Path,
    workspace: &Path,
) -> (Harness<SilentProvider>, PathBuf, Vec<Message>) {
    let (mut harness, path, messages) = single_turn_hard_harness(root, workspace);
    harness.set_summarizer(SummarizerKind::Subagent);
    // A subagent worker that blocks until cancelled: the hard-wait always times
    // out, forcing the fallback ladder.
    harness.set_compaction_summarizer_factory(BlockingSummaryProvider::factory(Arc::new(
        Mutex::new(Vec::new()),
    )));
    (harness, path, messages)
}

#[test]
fn hard_ladder_escalates_from_subagent_timeout_to_provider_native_when_supported() {
    let root = temp_dir();
    let workspace = temp_dir();
    let (mut harness, path, messages) =
        single_turn_hard_ladder_harness(&root.path, &workspace.path);
    let native_requests = Arc::new(Mutex::new(Vec::new()));
    let requests = native_requests.clone();
    harness.compaction.ladder.as_mut().unwrap().hard = u64::MAX;
    harness.set_provider_compaction_factory(Arc::new(move || {
        Ok(Box::new(NativeCompactionProvider {
            requests: requests.clone(),
        }))
    }));
    let obs = Recorder::default();
    let token = CancellationToken::new();

    let scheduled = block_on(harness.compaction.govern(
        BoundaryContext {
            messages: &messages,
            last_usage: None,
            round_trip: 1,
            turn_continues: true,
        },
        ApplyContext {
            workspace: &workspace.path,
            output_store: harness.output_store.as_ref(),
            task_state: None,
            observer: &obs,
        },
        &token,
    ))
    .unwrap();
    assert!(matches!(scheduled, ContextDirective::Proceed));
    assert_eq!(
        harness.compaction.background.as_ref().map(|job| job.origin),
        Some(CompactionOrigin::Subagent)
    );

    // Enabling native mode while a portable job is already running must also
    // gate the hard-tier native fallback. Normally the next job would select
    // native as its primary worker.
    harness.set_provider_native(true);
    harness.compaction.ladder.as_mut().unwrap().hard =
        super::context_tokens(&messages).saturating_sub(1);

    let directive = block_on(harness.compaction.govern(
        BoundaryContext {
            messages: &messages,
            last_usage: None,
            round_trip: 1,
            turn_continues: true,
        },
        ApplyContext {
            workspace: &workspace.path,
            output_store: harness.output_store.as_ref(),
            task_state: None,
            observer: &obs,
        },
        &token,
    ))
    .unwrap();
    assert!(matches!(directive, ContextDirective::Replace { .. }));

    // The subagent timed out and the ladder escalated to provider-native. A
    // second request is permitted if the first rewrite remains above hard.
    assert!(!native_requests.lock().unwrap().is_empty());
    let entries = compaction_entries(&path);
    assert!(
        entries
            .iter()
            .any(|entry| entry["origin"] == "providerNative"),
        "expected a provider-native compaction entry, got {entries:?}"
    );
    assert!(
        !entries.iter().any(|entry| entry["origin"] == "excerpts"),
        "provider-native success must not fall through to excerpts: {entries:?}"
    );
    // A provider-native fallback success resets the model-backed breaker.
    assert_eq!(harness.compaction.consecutive_failures, 0);
    assert!(obs.events.borrow().iter().any(|event| matches!(
        event,
        AgentEvent::CompactionLifecycle { message: Some(message), .. }
            if message.contains("escalating to provider-native compaction")
    )));
}

#[test]
fn hard_ladder_skips_supported_native_fallback_without_opt_in() {
    let root = temp_dir();
    let workspace = temp_dir();
    let (mut harness, path, messages) =
        single_turn_hard_ladder_harness(&root.path, &workspace.path);
    let native_requests = Arc::new(Mutex::new(Vec::new()));
    let requests = native_requests.clone();
    harness.set_provider_compaction_factory(Arc::new(move || {
        Ok(Box::new(NativeCompactionProvider {
            requests: requests.clone(),
        }))
    }));
    let obs = Recorder::default();

    let directive = block_on(harness.compaction.govern(
        BoundaryContext {
            messages: &messages,
            last_usage: None,
            round_trip: 1,
            turn_continues: true,
        },
        ApplyContext {
            workspace: &workspace.path,
            output_store: harness.output_store.as_ref(),
            task_state: None,
            observer: &obs,
        },
        &CancellationToken::new(),
    ))
    .unwrap();

    assert!(matches!(directive, ContextDirective::Replace { .. }));
    assert!(native_requests.lock().unwrap().is_empty());
    let entries = compaction_entries(&path);
    assert!(entries.iter().any(|entry| entry["origin"] == "excerpts"));
    assert!(
        !entries
            .iter()
            .any(|entry| entry["origin"] == "providerNative")
    );
}

#[test]
fn hard_ladder_falls_through_to_excerpts_when_native_capability_is_none() {
    let root = temp_dir();
    let workspace = temp_dir();
    let (mut harness, path, messages) =
        single_turn_hard_ladder_harness(&root.path, &workspace.path);
    // Native mode is opted in, but the provider factory advertises no native
    // capability, so the portable worker runs and the fallback probe must fall
    // through to deterministic excerpts.
    harness.set_provider_native(true);
    harness.set_provider_compaction_factory(Arc::new(|| Ok(Box::new(SilentProvider))));
    let obs = Recorder::default();
    let token = CancellationToken::new();

    let directive = block_on(harness.compaction.govern(
        BoundaryContext {
            messages: &messages,
            last_usage: None,
            round_trip: 1,
            turn_continues: true,
        },
        ApplyContext {
            workspace: &workspace.path,
            output_store: harness.output_store.as_ref(),
            task_state: None,
            observer: &obs,
        },
        &token,
    ))
    .unwrap();
    assert!(matches!(directive, ContextDirective::Replace { .. }));

    let entries = compaction_entries(&path);
    assert!(
        entries.iter().any(|entry| entry["origin"] == "excerpts"),
        "expected a deterministic excerpts entry, got {entries:?}"
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry["origin"] == "providerNative"),
        "unsupported native capability must not produce a provider-native entry: {entries:?}"
    );
    assert!(obs.events.borrow().iter().any(|event| matches!(
        event,
        AgentEvent::CompactionLifecycle { message: Some(message), .. }
            if message.contains("provider-native compaction unavailable; using deterministic excerpts")
    )));
}

#[test]
fn hard_tier_degrades_to_excerpts_when_native_probe_yields_nothing_and_no_portable_worker() {
    // Finding 1: `has_model_worker()` trusts the native factory's presence, but
    // the spawn-time capability probe can yield None. With no portable
    // summarizer this used to panic on the removed `expect`. It must instead
    // degrade to the deterministic excerpts backstop and keep the turn going.
    let root = temp_dir();
    let workspace = temp_dir();
    let (mut harness, path, messages) = single_turn_hard_harness(&root.path, &workspace.path);
    // Provider-native is enabled with a factory, so `has_model_worker()` is
    // true, but the provider advertises `None` capability. No summarizer factory
    // is installed, so there is no portable worker to fall back to.
    harness.set_provider_native(true);
    harness.set_provider_compaction_factory(Arc::new(|| Ok(Box::new(SilentProvider))));
    let obs = Recorder::default();
    let token = CancellationToken::new();

    let directive = block_on(harness.compaction.govern(
        BoundaryContext {
            messages: &messages,
            last_usage: None,
            round_trip: 1,
            turn_continues: true,
        },
        ApplyContext {
            workspace: &workspace.path,
            output_store: harness.output_store.as_ref(),
            task_state: None,
            observer: &obs,
        },
        &token,
    ))
    .expect("govern must not panic when no worker is available");

    assert!(
        matches!(directive, ContextDirective::Replace { .. }),
        "deterministic excerpts backstop must still relieve pressure"
    );
    let entries = compaction_entries(&path);
    assert!(
        entries.iter().any(|entry| entry["origin"] == "excerpts"),
        "expected a deterministic excerpts entry, got {entries:?}"
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry["origin"] == "providerNative"),
        "an unusable native probe must not produce a provider-native entry: {entries:?}"
    );
}

#[test]
fn excerpts_summarizer_kind_never_spawns_a_model_worker_even_with_a_factory() {
    // Finding 1 audit: when the native probe yields nothing, an Excerpts
    // summarizer must not spawn a portable model worker just because a
    // summarizer factory happens to be installed. Relief comes from the
    // deterministic backstop; the summarizer provider is never invoked.
    let root = temp_dir();
    let workspace = temp_dir();
    let (mut harness, path, messages) = single_turn_hard_harness(&root.path, &workspace.path);
    // Native probe yields None (default capability), driving the else branch.
    harness.set_provider_native(true);
    harness.set_provider_compaction_factory(Arc::new(|| Ok(Box::new(SilentProvider))));
    // Summarizer kind stays Excerpts (the default) but a factory is present.
    harness.set_summarizer(SummarizerKind::Excerpts);
    let prompts = Arc::new(Mutex::new(Vec::new()));
    harness.set_compaction_summarizer_factory(SummaryProvider::factory(
        Arc::new(Mutex::new(VecDeque::new())),
        prompts.clone(),
        Arc::new(Mutex::new(Vec::new())),
    ));
    // A generous hard wait so, on the buggy path, the spawned portable worker
    // would have ample time to run and record a prompt.
    harness.compaction.hard_wait = Duration::from_secs(2);
    let obs = Recorder::default();
    let token = CancellationToken::new();

    let directive = block_on(harness.compaction.govern(
        BoundaryContext {
            messages: &messages,
            last_usage: None,
            round_trip: 1,
            turn_continues: true,
        },
        ApplyContext {
            workspace: &workspace.path,
            output_store: harness.output_store.as_ref(),
            task_state: None,
            observer: &obs,
        },
        &token,
    ))
    .expect("govern must not spawn or panic for an Excerpts summarizer");

    assert!(matches!(directive, ContextDirective::Replace { .. }));
    assert!(
        prompts.lock().unwrap().is_empty(),
        "an Excerpts summarizer must never invoke the summarizer provider"
    );
    let entries = compaction_entries(&path);
    assert!(
        entries.iter().any(|entry| entry["origin"] == "excerpts"),
        "expected a deterministic excerpts entry, got {entries:?}"
    );
    assert!(
        !entries.iter().any(|entry| entry["origin"] == "provider"),
        "an Excerpts summarizer must not apply a provider-origin compaction: {entries:?}"
    );
}

#[test]
fn provider_native_origin_timeout_falls_straight_to_excerpts_without_a_second_request() {
    // Finding 2: a job that was already ProviderNative origin and timed out must
    // NOT fire a second identical provider-native request in the fallback rung.
    // It routes straight to the deterministic excerpts terminal rung.
    let root = temp_dir();
    let workspace = temp_dir();
    let (mut harness, path, messages) = single_turn_hard_harness(&root.path, &workspace.path);
    let native_requests = Arc::new(Mutex::new(Vec::new()));
    let requests = native_requests.clone();
    harness.set_provider_native(true);
    harness.set_provider_compaction_factory(Arc::new(move || {
        Ok(Box::new(BlockingNativeProvider {
            requests: requests.clone(),
        }))
    }));
    let obs = Recorder::default();
    let token = CancellationToken::new();

    let directive = block_on(harness.compaction.govern(
        BoundaryContext {
            messages: &messages,
            last_usage: None,
            round_trip: 1,
            turn_continues: true,
        },
        ApplyContext {
            workspace: &workspace.path,
            output_store: harness.output_store.as_ref(),
            task_state: None,
            observer: &obs,
        },
        &token,
    ))
    .unwrap();

    assert!(matches!(directive, ContextDirective::Replace { .. }));
    assert_eq!(
        native_requests.lock().unwrap().len(),
        1,
        "a ProviderNative-origin failure must not retry the native rung"
    );
    let entries = compaction_entries(&path);
    assert!(
        entries.iter().any(|entry| entry["origin"] == "excerpts"),
        "expected the deterministic excerpts terminal rung, got {entries:?}"
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry["origin"] == "providerNative"),
        "a timed-out native job must not apply a provider-native entry: {entries:?}"
    );
}

#[test]
fn planner_prefers_the_largest_pair_safe_run_across_summary_gaps() {
    let workspace = temp_dir();
    let messages = vec![
        Message::user("tiny old fragment"),
        Message::assistant("tiny answer"),
        Message::user("[prior compacted summary]"),
        Message::user(&"large later history ".repeat(500)),
        Message::assistant(&"large later answer ".repeat(500)),
    ];
    let ids = vec![
        Some("tiny_user".to_string()),
        Some("tiny_assistant".to_string()),
        None,
        Some("large_user".to_string()),
        Some("large_assistant".to_string()),
    ];
    let agent = Agent::resumed(SilentProvider, built_in_tools(), messages.clone());
    let harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        None,
        ids,
        None,
    );

    let plan = harness
        .compaction
        .plan_manual(&messages, 0)
        .expect("the later durable run is coverable");

    assert_eq!((plan.start, plan.end), (3, 5));
    assert_eq!(plan.from_id, "large_user");
    assert_eq!(plan.to_id, "large_assistant");
}

#[test]
fn manual_compact_degrades_when_native_probe_finds_no_worker() {
    let root = temp_dir();
    let workspace = temp_dir();
    let mut seeded = seed_harness(&root.path, &workspace.path);
    seeded.harness.set_summarizer(SummarizerKind::Excerpts);
    seeded.harness.set_provider_native(true);
    seeded
        .harness
        .set_provider_compaction_factory(Arc::new(|| Ok(Box::new(SilentProvider))));
    let before = super::context_tokens(seeded.harness.messages());
    let obs = Recorder::default();

    block_on(seeded.harness.compact_now(&obs, &CancellationToken::new()))
        .expect("manual compaction must use excerpts when no native worker exists");

    assert!(super::context_tokens(seeded.harness.messages()) < before);
    let entries = compaction_entries(&seeded.path);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["origin"], "excerpts");
}

struct FailOnContextEvent;

impl AgentObserver for FailOnContextEvent {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        if matches!(
            event,
            AgentEvent::ContextPressure { .. }
                | AgentEvent::CompactionApplied { .. }
                | AgentEvent::CompactionLifecycle { .. }
                | AgentEvent::FoldApplied { .. }
        ) {
            anyhow::bail!("context-management display channel closed");
        }
        Ok(())
    }
}

#[test]
fn context_event_failure_cannot_block_or_drop_a_durable_replacement() {
    let root = temp_dir();
    let workspace = temp_dir();
    let (mut harness, path, messages) = single_turn_hard_harness(&root.path, &workspace.path);
    harness.set_summarizer(SummarizerKind::Excerpts);
    let token = CancellationToken::new();

    let directive = block_on(harness.compaction.govern(
        BoundaryContext {
            messages: &messages,
            last_usage: None,
            round_trip: 1,
            turn_continues: true,
        },
        ApplyContext {
            workspace: &workspace.path,
            output_store: harness.output_store.as_ref(),
            task_state: None,
            observer: &FailOnContextEvent,
        },
        &token,
    ))
    .expect("observer telemetry failure must not roll back a durable compaction");
    let ContextDirective::Replace {
        messages: compacted,
    } = directive
    else {
        panic!("durable compaction was not returned to Nexus");
    };

    harness.compaction.persist_messages(&compacted);
    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .unwrap()
        .into_iter()
        .find(|meta| meta.path == path)
        .unwrap();
    assert_eq!(
        store.open(&meta).unwrap().messages,
        compacted,
        "live replacement and resume rebuild must stay identical"
    );
}

#[test]
fn hard_apply_survives_cancellation_racing_after_the_durable_mutation() {
    // Finding 3: when the turn token cancels AFTER a compaction is durably
    // applied, the governor must still return the compacted messages. Dropping
    // them (returning Proceed) diverges the live context from a resume rebuild.
    let root = temp_dir();
    let workspace = temp_dir();
    let (mut harness, path, messages) = single_turn_hard_harness(&root.path, &workspace.path);
    harness.set_summarizer(SummarizerKind::Subagent);
    // A fast subagent worker that wins the hard-wait race and applies.
    harness.set_compaction_summarizer_factory(SummaryProvider::factory(
        Arc::new(Mutex::new(VecDeque::from([format!(
            "Goal: continue. State: compacted. Decisions: none. Key facts: {SUMMARY_NEEDLE}. Next steps: proceed."
        )]))),
        Arc::new(Mutex::new(Vec::new())),
        Arc::new(Mutex::new(Vec::new())),
    ));
    // Generous wait so the fast worker reliably wins the race, then the observer
    // cancels the token during the durable apply.
    harness.compaction.hard_wait = Duration::from_secs(5);
    let token = CancellationToken::new();
    let obs = CancelOnApply {
        events: RefCell::new(Vec::new()),
        token: token.clone(),
    };

    let directive = block_on(harness.compaction.govern(
        BoundaryContext {
            messages: &messages,
            last_usage: None,
            round_trip: 1,
            turn_continues: true,
        },
        ApplyContext {
            workspace: &workspace.path,
            output_store: harness.output_store.as_ref(),
            task_state: None,
            observer: &obs,
        },
        &token,
    ))
    .unwrap();

    assert!(
        token.is_cancelled(),
        "test must exercise the post-apply cancellation race"
    );
    let ContextDirective::Replace {
        messages: compacted,
    } = directive
    else {
        panic!("post-apply cancellation dropped the durable compaction (returned Proceed)");
    };

    // Byte-exact: the live compacted context equals the session-log rebuild.
    harness.compaction.persist_messages(&compacted);
    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .unwrap()
        .into_iter()
        .find(|meta| meta.path == path)
        .unwrap();
    let rebuilt = store.open(&meta).unwrap();
    let live_json = compacted
        .iter()
        .map(|message| serde_json::to_string(message).unwrap())
        .collect::<Vec<_>>();
    let rebuilt_json = rebuilt
        .messages
        .iter()
        .map(|message| serde_json::to_string(message).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        live_json, rebuilt_json,
        "live context and resume rebuild diverged after post-apply cancellation"
    );
}

/// A persisted multi-turn transcript parked at hard pressure at a safe TURN
/// BOUNDARY (no open turn), summarized deterministically with excerpts so a
/// pre-turn `maybe_auto_compact` relieves synchronously via the hard ladder.
/// `hard` sets the hard threshold directly: a small value makes the seeded
/// content Hard, and `hard = 1` forces a cap-exhausted transcript that stays
/// Hard no matter how much the ladder excerpts.
fn hard_boundary_excerpts_harness(
    root: &Path,
    workspace: &Path,
    hard: u64,
) -> (Harness<SilentProvider>, PathBuf) {
    let mut log = SessionLog::create_in(root, workspace).unwrap();
    let big = format!("{OLD_NEEDLE} :: {}", "long covered context. ".repeat(500));
    let second = format!("second covered turn {}", "with more content ".repeat(80));
    for message in [
        Message::user(&big),
        Message::assistant("ok"),
        Message::user(&second),
        Message::assistant("ok2"),
        Message::user("small retained turn"),
        Message::assistant("ok3"),
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
        Some(131_072),
    );
    harness.set_compaction_trigger(
        131_072.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 200,
            hard_wait_ms: 20,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    harness.set_summarizer(SummarizerKind::Excerpts);
    let ladder = harness.compaction.ladder.as_mut().unwrap();
    ladder.warn = (hard / 2).max(1);
    ladder.start = (hard * 3 / 4).max(1);
    ladder.hard = hard;
    ladder.keep_recent_tokens = 200;
    // Pure deterministic excerpts: no background worker, so the hard ladder
    // applies synchronously on this thread and the post-apply re-emission is
    // observable in one `maybe_auto_compact` call.
    ladder.deterministic_only = true;
    (harness, path)
}

/// Finding 6 precondition: a pre-turn hard auto-compaction that RELIEVES
/// pressure must re-emit a fresh sub-Hard `ContextPressure` after applying, so
/// the footer drops off Hard within the same turn instead of idling on the
/// stale pre-apply tier that triggered relief.
#[test]
fn pre_turn_hard_relief_reemits_fresh_subhard_pressure() {
    let root = temp_dir();
    let workspace = temp_dir();
    let (mut harness, _path) = hard_boundary_excerpts_harness(&root.path, &workspace.path, 1_500);
    let obs = Recorder::default();
    let token = CancellationToken::new();

    block_on(harness.maybe_auto_compact(&obs, &token, false)).unwrap();

    assert!(obs.applied() >= 1, "the hard ladder must apply excerpts");
    let pressures = obs.pressures();
    assert_eq!(
        pressures.first(),
        Some(&ContextPressureTier::Hard),
        "the pre-apply emit must report hard pressure: {pressures:?}"
    );
    assert!(
        pressures
            .iter()
            .any(|tier| !matches!(tier, ContextPressureTier::Hard)),
        "relief must re-emit a fresh sub-hard tier post-apply: {pressures:?}"
    );
    assert!(
        super::context_tokens(harness.messages()) < 1_500,
        "measured context must actually drop below the hard threshold"
    );
}

/// Finding 6 honest case: a pre-turn hard auto-compaction that APPLIES but does
/// NOT clear hard pressure (cap-exhausted) must NOT re-emit a sub-Hard tier --
/// the footer stays honestly at Hard so the meter's stall warning is truthful.
#[test]
fn pre_turn_cap_exhausted_hard_pressure_never_reemits_below_hard() {
    let root = temp_dir();
    let workspace = temp_dir();
    // hard = 1: any non-empty context reads Hard, so the ladder can excerpt but
    // never drop the tier.
    let (mut harness, _path) = hard_boundary_excerpts_harness(&root.path, &workspace.path, 1);
    let obs = Recorder::default();
    let token = CancellationToken::new();

    block_on(harness.maybe_auto_compact(&obs, &token, false)).unwrap();

    assert!(
        obs.applied() >= 1,
        "the hard ladder must still apply excerpts"
    );
    let pressures = obs.pressures();
    assert!(
        !pressures.is_empty()
            && pressures
                .iter()
                .all(|tier| matches!(tier, ContextPressureTier::Hard)),
        "cap-exhausted hard pressure must stay Hard with no sub-hard re-emission: {pressures:?}"
    );
}

/// Finding 7 precondition: a manual `/compact` that CLEARS pressure re-emits a
/// fresh sub-Hard `ContextPressure` after applying (no provider turn is
/// involved), so the meter drops off Hard.
#[test]
fn manual_compact_clearing_pressure_reemits_fresh_subhard_pressure() {
    let root = temp_dir();
    let workspace = temp_dir();
    let (mut harness, _path) = hard_boundary_excerpts_harness(&root.path, &workspace.path, 1_500);
    let obs = Recorder::default();
    let token = CancellationToken::new();

    // Prime the pressure tracker at Hard the way a live session would (an
    // earlier boundary already crossed up), so the manual clear registers as a
    // downward crossing.
    let measured = super::context_tokens(harness.messages());
    let ladder = harness.compaction.ladder.unwrap();
    let _ = harness.compaction.pressure.crossing(measured, &ladder);

    block_on(harness.compact_now(&obs, &token)).unwrap();

    assert!(obs.applied() >= 1, "manual compaction must apply");
    let pressures = obs.pressures();
    assert!(
        pressures
            .iter()
            .any(|tier| !matches!(tier, ContextPressureTier::Hard)),
        "a clearing /compact must re-emit a fresh sub-hard tier: {pressures:?}"
    );
}
