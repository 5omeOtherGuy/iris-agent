use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use super::Harness;
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate,
    AssistantTurn, ChatProvider, Message, ProviderEvent, ProviderStream, ReviewContext, Role,
    ToolCall, Tools,
};
use crate::session::{SessionLog, SessionStore};
use crate::tools::{ToolState, built_in_tools};

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

#[derive(Clone)]
struct CrashProbeProvider {
    calls: Arc<AtomicUsize>,
    second_request_started: Arc<AtomicBool>,
}

impl ChatProvider for CrashProbeProvider {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => {
                let turn = AssistantTurn {
                    tool_calls: vec![ToolCall {
                        id: "call_incremental_read".to_string(),
                        name: "read".to_string(),
                        arguments: serde_json::json!({ "path": "note.txt" }),
                        thought_signature: None,
                    }],
                    ..AssistantTurn::default()
                };
                Ok(Box::pin(futures::stream::once(async move {
                    Ok(ProviderEvent::Completed(turn))
                })))
            }
            1 => {
                self.second_request_started.store(true, Ordering::SeqCst);
                Ok(Box::pin(futures::stream::pending()))
            }
            call => panic!("unexpected provider call {call}"),
        }
    }
}

#[derive(Clone)]
struct FinalProvider {
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl ChatProvider for FinalProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        self.requests.lock().unwrap().push(messages.to_vec());
        Ok(Box::pin(futures::stream::once(async {
            Ok(ProviderEvent::Completed(AssistantTurn::text(
                "resumed cleanly",
            )))
        })))
    }
}

/// Simulate a process dying after one tool round trip, while the next provider
/// request is still in flight. The completed call/result group must already be
/// durable, carry entry ids, and reopen byte-for-byte as the live context.
#[test]
fn crash_mid_turn_reopens_at_the_last_complete_round_trip() -> Result<()> {
    let root = crate::tools::test_support::temp_dir();
    let workspace = crate::tools::test_support::temp_dir();
    std::fs::write(workspace.path.join("note.txt"), "durable boundary\n")?;

    let session = SessionLog::create_in(&root.path, &workspace.path)?;
    let session_id = session.id().to_string();
    let session_path = session.path().to_path_buf();
    let calls = Arc::new(AtomicUsize::new(0));
    let second_request_started = Arc::new(AtomicBool::new(false));
    let provider = CrashProbeProvider {
        calls: calls.clone(),
        second_request_started: second_request_started.clone(),
    };
    let mut harness = Harness::new(
        Agent::new(provider, built_in_tools()),
        workspace.path.clone(),
        ToolState::new(),
        Some(session),
        None,
    );
    let observer = Recorder::default();
    let token = CancellationToken::new();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let mut turn = Box::pin(harness.submit_turn(
            "Read note.txt before replying.",
            &observer,
            &AllowGate,
            &token,
        ));
        for _ in 0..200 {
            if second_request_started.load(Ordering::SeqCst) {
                break;
            }
            tokio::select! {
                result = &mut turn => panic!("turn ended before crash point: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
        assert!(
            second_request_started.load(Ordering::SeqCst),
            "second provider request did not begin"
        );

        let store = SessionStore::with_root(root.path.clone());
        let meta = store.find(&session_id)?.expect("session is discoverable");
        let durable = store.open(&meta)?;
        assert_eq!(durable.messages.len(), 3, "one complete tool round trip");
        assert_eq!(durable.messages[0].role, Role::User);
        assert_eq!(durable.messages[1].role, Role::AssistantToolCall);
        assert_eq!(durable.messages[2].role, Role::Tool);
        assert_eq!(
            durable.messages[1].tool_call_id, durable.messages[2].tool_call_id,
            "durable tool call and result stay paired"
        );
        assert!(
            durable.entry_ids.iter().all(Option::is_some),
            "every incrementally persisted message has a durable entry id"
        );

        // Dropping the in-flight future is the crash: the harness's post-turn
        // persistence backstop never runs. The boundary callback already made
        // disk and memory identical.
        drop(turn);
        assert_eq!(durable.messages, harness.agent.messages());
        assert_eq!(durable.entry_ids, harness.compaction.entry_ids);
        Result::<()>::Ok(())
    })?;

    drop(harness);

    // A fresh process can continue from the same file without Nexus inserting
    // a synthetic dangling-call repair: the original pair was atomic at the
    // persistence boundary.
    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .find(&session_id)?
        .expect("session remains discoverable");
    let stored = store.open(&meta)?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = FinalProvider {
        requests: requests.clone(),
    };
    let agent = Agent::resumed(provider, built_in_tools(), stored.messages);
    let session = SessionLog::resume(&session_path)?;
    let mut resumed = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(session),
        stored.entry_ids,
        None,
    );
    runtime.block_on(resumed.submit_turn(
        "Continue after the crash.",
        &Recorder::default(),
        &AllowGate,
        &CancellationToken::new(),
    ))?;

    let seen = requests.lock().unwrap();
    let first = seen.first().expect("resumed provider was called");
    let call_index = first
        .iter()
        .position(|message| message.role == Role::AssistantToolCall)
        .expect("original call remains in resumed context");
    assert_eq!(first[call_index + 1].role, Role::Tool);
    assert_eq!(
        first[call_index].tool_call_id,
        first[call_index + 1].tool_call_id
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let reopened = store.open(&meta)?;
    assert_eq!(
        reopened.messages.len(),
        5,
        "three prior + resumed user/reply"
    );
    assert_eq!(reopened.entry_ids.len(), reopened.messages.len());
    Ok(())
}
