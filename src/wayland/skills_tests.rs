use std::cell::RefCell;
use std::fs;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use super::Harness;
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate,
    AssistantTurn, ChatProvider, Message, ProviderEvent, ProviderStream, ReviewContext, Role,
    ToolCall, Tools,
};
use crate::tools::{ToolState, test_support::temp_dir};

#[derive(Clone)]
struct RecordingProvider {
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl ChatProvider for RecordingProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        self.requests.lock().unwrap().push(messages.to_vec());
        Ok(Box::pin(futures::stream::once(async {
            Ok(ProviderEvent::Completed(AssistantTurn::text("done")))
        })))
    }
}

#[derive(Default)]
struct Frontend {
    events: RefCell<Vec<AgentEvent>>,
}

impl AgentObserver for Frontend {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        self.events.borrow_mut().push(event);
        Ok(())
    }
}

impl ApprovalGate for Frontend {
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

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn harness_injects_catalog_and_explicit_body_without_rewriting_visible_prompt() {
    let dir = temp_dir();
    let skill_dir = dir
        .path
        .join(".agents/skills/iris-native-skill-integration");
    fs::create_dir_all(&skill_dir).unwrap();
    let skill_path = skill_dir.join("SKILL.md");
    fs::write(
        &skill_path,
        "---\nname: iris-native-skill-integration\ndescription: First description.\n---\nFollow this body.\n",
    )
    .unwrap();

    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = RecordingProvider {
        requests: requests.clone(),
    };
    let agent = Agent::new(provider, Tools::new(vec![]));
    let mut harness = Harness::new(
        agent,
        dir.path.clone(),
        ToolState::new(),
        None,
        Some(128_000),
    );
    let frontend = Frontend::default();

    block_on(harness.submit_turn(
        "$iris-native-skill-integration do the work",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))
    .unwrap();

    let first = requests.lock().unwrap()[0].clone();
    assert_eq!(first[0].role, Role::Developer);
    assert!(first[0].content.contains("First description."));
    assert_eq!(first[1].role, Role::User);
    assert!(first[1].content.starts_with("<skill>"));
    assert!(first[1].content.contains("Follow this body."));
    assert_eq!(
        first[2].content,
        "$iris-native-skill-integration do the work"
    );
    let shown_prompts = frontend
        .events
        .borrow()
        .iter()
        .filter_map(|event| match event {
            AgentEvent::UserMessage(text) => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        shown_prompts.is_empty(),
        "hidden skill context must not emit a visible user-message event"
    );

    fs::write(
        &skill_path,
        "---\nname: iris-native-skill-integration\ndescription: Updated description.\n---\nUpdated body.\n",
    )
    .unwrap();
    block_on(harness.submit_turn(
        "plain follow-up",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))
    .unwrap();

    let second = requests.lock().unwrap()[1].clone();
    let new_tail = &second[first.len() + 1..]; // prior assistant reply is also retained
    assert_eq!(new_tail[0].role, Role::Developer);
    assert!(new_tail[0].content.contains("Updated description."));
    assert_eq!(new_tail[1].content, "plain follow-up");
}
