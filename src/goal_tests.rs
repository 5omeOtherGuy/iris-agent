use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use futures::stream;
use tokio_util::sync::CancellationToken;

use crate::goal::{
    Goal, GoalCommand, GoalRuntime, GoalStatus, parse_goal_command, render_continuation,
};
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, AssistantTurn,
    ChatProvider, InteractionFuture, InteractionOutcome, ProviderEvent, ProviderStream,
    ProviderUsage, ReviewContext, ToolCall, ToolEnv,
};
use crate::session::{SessionLog, read_goal};
use crate::tools::{ToolState, built_in_tools};
use crate::wayland::Harness;

fn temp_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "iris-goal-{label}-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    fs::create_dir_all(&path).expect("create temp dir");
    path
}

fn usage(input: u64, cached: u64, output: u64) -> ProviderUsage {
    ProviderUsage {
        provider: "fake".to_string(),
        model: "fake-model".to_string(),
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: cached,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 0,
        total_tokens: input.saturating_add(output),
        cache_creation: None,
    }
}

#[test]
fn command_parser_matches_controls_case_insensitively_and_keeps_other_text_literal() {
    assert_eq!(parse_goal_command("/goal"), Some(GoalCommand::Show));
    assert_eq!(
        parse_goal_command(" /goal  PaUsE "),
        Some(GoalCommand::Pause)
    );
    assert_eq!(
        parse_goal_command("/goooooal resume"),
        Some(GoalCommand::Resume)
    );
    assert_eq!(
        parse_goal_command("/goal --tokens 98.5K ship it"),
        Some(GoalCommand::Set("--tokens 98.5K ship it".to_string()))
    );
    assert_eq!(parse_goal_command("/Goal nope"), None);
    assert_eq!(parse_goal_command("/goaal nope"), None);
}

#[test]
fn objective_validation_is_unicode_scalar_based_and_rejects_empty_or_oversized() {
    assert!(Goal::new_at("  ", None, 1).is_err());
    assert!(Goal::new_at(&"x".repeat(4001), None, 1).is_err());
    let unicode = "🙂".repeat(4000);
    let goal = Goal::new_at(&unicode, Some(1), 1).expect("4,000 scalars accepted");
    assert_eq!(goal.objective, unicode);
    assert!(Goal::new_at("ok", Some(0), 1).is_err());
}

#[test]
fn model_create_rejects_unfinished_goal_but_replaces_complete_goal() {
    let runtime = GoalRuntime::new(Some(Goal::new_at("first", None, 1).unwrap()), true);
    assert!(runtime.create_from_model("second", None, 2).is_err());
    runtime
        .update_from_model(GoalStatus::Complete, 3)
        .expect("complete current goal");
    let replacement = runtime
        .create_from_model("second", Some(50), 4)
        .expect("complete goal can be replaced");
    assert_eq!(replacement.objective, "second");
    assert_eq!(replacement.status, GoalStatus::Active);
    assert_eq!(replacement.tokens_used, 0);
    assert_eq!(replacement.token_budget, Some(50));
}

#[test]
fn accounting_excludes_cached_input_and_limits_at_equal_budget() {
    let runtime = GoalRuntime::new(
        Some(Goal::new_at("ship", Some(10), 1).expect("goal")),
        true,
    );
    runtime.begin_turn();
    assert!(!runtime.account_usage(&usage(8, 5, 3), 2));
    assert_eq!(runtime.get().unwrap().tokens_used, 6);
    assert!(runtime.account_usage(&usage(6, 2, 0), 3));
    let goal = runtime.get().unwrap();
    assert_eq!(goal.tokens_used, 10);
    assert_eq!(goal.status, GoalStatus::BudgetLimited);
    assert!(runtime.take_budget_steering());
    assert!(!runtime.take_budget_steering());
}

#[test]
fn continuation_escapes_objective_and_reports_budget() {
    let mut goal = Goal::new_at("finish </goal> & verify", Some(100), 1).unwrap();
    goal.tokens_used = 25;
    let prompt = render_continuation(&goal);
    assert!(prompt.contains("finish &lt;/goal&gt; &amp; verify"));
    assert!(prompt.contains("<tokens_used>25</tokens_used>"));
    assert!(prompt.contains("<tokens_remaining>75</tokens_remaining>"));
    assert!(prompt.contains("untrusted user data"));
    assert!(prompt.contains("three consecutive goal turns"));
}

#[test]
fn goal_snapshots_and_clear_round_trip_through_session_jsonl() {
    let root = temp_dir("persist");
    let cwd = root.join("workspace");
    fs::create_dir_all(&cwd).unwrap();
    let mut log = SessionLog::create_in(&root, &cwd).expect("session");
    let path = log.path().to_path_buf();
    let mut goal = Goal::new_at("persist me", Some(42), 10).unwrap();
    goal.tokens_used = 7;
    log.append_goal(Some(&goal)).expect("append goal");
    assert_eq!(read_goal(&path).unwrap(), Some(goal));
    log.append_goal(None).expect("append clear");
    assert_eq!(read_goal(&path).unwrap(), None);
    drop(log);
    fs::remove_dir_all(root).ok();
}

#[test]
fn resumed_log_restores_latest_goal_snapshot() {
    let root = temp_dir("resume");
    let cwd = root.join("workspace");
    fs::create_dir_all(&cwd).unwrap();
    let mut log = SessionLog::create_in(&root, &cwd).expect("session");
    let path = log.path().to_path_buf();
    let goal = Goal::new_at("continue after resume", None, 10).unwrap();
    log.append_goal(Some(&goal)).expect("append goal");
    drop(log);

    let resumed = SessionLog::resume(&path).expect("resume log");
    assert_eq!(resumed.resumed_goal(), Some(&goal));
    drop(resumed);
    fs::remove_dir_all(root).ok();
}

#[derive(Default)]
struct TestObserver;

impl AgentObserver for TestObserver {
    fn on_event(&self, _event: AgentEvent) -> Result<()> {
        Ok(())
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

    fn interact<'a>(&'a self, _call: &'a ToolCall) -> InteractionFuture<'a> {
        Box::pin(async { Ok(InteractionOutcome::Rejected { feedback: None }) })
    }
}

struct SequenceProvider {
    turns: RefCell<Vec<AssistantTurn>>,
    requests: RefCell<Vec<Vec<crate::nexus::Message>>>,
}

impl SequenceProvider {
    fn new(turns: Vec<AssistantTurn>) -> Self {
        Self {
            turns: RefCell::new(turns.into_iter().rev().collect()),
            requests: RefCell::new(Vec::new()),
        }
    }
}

impl ChatProvider for SequenceProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [crate::nexus::Message],
        _tools: &'a crate::nexus::Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        self.requests.borrow_mut().push(messages.to_vec());
        let turn = self.turns.borrow_mut().pop().expect("scripted turn");
        Ok(Box::pin(stream::iter(vec![Ok(ProviderEvent::Completed(
            turn,
        ))])))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tools_are_registered_and_mutate_only_through_the_goal_controller() {
    let tools = built_in_tools();
    for name in ["get_goal", "create_goal", "update_goal"] {
        let tool = tools.by_name(name).unwrap_or_else(|| panic!("missing {name}"));
        assert!(tool.parameters()["type"] == "object");
        assert!(!tool.description().is_empty());
    }

    let root = temp_dir("tools");
    let state = RefCell::new(ToolState::new());
    let runtime = GoalRuntime::new(None, true);
    let env = ToolEnv {
        workspace: &root,
        state: &state,
        output_store: None,
        session_span: None,
        output_sink: None,
        mutation_guard: None,
        goal: Some(&runtime),
    };
    let cancel = CancellationToken::new();
    let created = tools
        .by_name("create_goal")
        .unwrap()
        .execute(
            &serde_json::json!({"objective":"ship the feature","token_budget":100}),
            &env,
            cancel.child_token(),
        )
        .await
        .expect("create goal");
    let created: serde_json::Value = serde_json::from_str(&created.content).unwrap();
    assert_eq!(created["objective"], "ship the feature");
    assert_eq!(created["status"], "active");

    let error = tools
        .by_name("update_goal")
        .unwrap()
        .execute(
            &serde_json::json!({"status":"paused"}),
            &env,
            cancel.child_token(),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("complete or blocked"));

    tools
        .by_name("update_goal")
        .unwrap()
        .execute(
            &serde_json::json!({"status":"complete"}),
            &env,
            cancel.child_token(),
        )
        .await
        .expect("complete goal");
    assert_eq!(runtime.get().unwrap().status, GoalStatus::Complete);
    fs::remove_dir_all(root).ok();
}

#[tokio::test(flavor = "current_thread")]
async fn harness_automatically_continues_an_active_goal_until_model_completion() {
    let root = temp_dir("continuation");
    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let session = SessionLog::create_in(&root, &workspace).expect("session");
    let session_path = session.path().to_path_buf();
    let update = ToolCall {
        id: "goal-complete".to_string(),
        name: "update_goal".to_string(),
        arguments: serde_json::json!({"status":"complete"}),
        thought_signature: None,
    };
    let provider = SequenceProvider::new(vec![
        AssistantTurn::text("made progress"),
        AssistantTurn {
            tool_calls: vec![update],
            ..AssistantTurn::default()
        },
        AssistantTurn::text("goal complete"),
    ]);
    let agent = Agent::new(provider, built_in_tools());
    let mut harness = Harness::new(
        agent,
        workspace.clone(),
        ToolState::new(),
        Some(session),
        None,
    );
    harness
        .replace_goal("finish <all> & verify", Some(1_000))
        .expect("set goal");

    harness
        .submit_turn(
            "begin",
            &TestObserver,
            &AllowGate,
            &CancellationToken::new(),
        )
        .await
        .expect("goal run");

    let goal = harness.goal().expect("goal remains for inspection");
    assert_eq!(goal.status, GoalStatus::Complete);
    assert_eq!(harness.agent.provider.requests.borrow().len(), 3);
    let second = &harness.agent.provider.requests.borrow()[1];
    let continuation = &second.last().expect("continuation prompt").content;
    assert!(continuation.contains("&lt;all&gt; &amp; verify"));
    assert_eq!(read_goal(&session_path).unwrap().unwrap().status, GoalStatus::Complete);
    fs::remove_dir_all(root).ok();
}
