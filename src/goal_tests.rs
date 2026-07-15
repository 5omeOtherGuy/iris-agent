use std::fs;
use std::path::PathBuf;

use crate::goal::{
    Goal, GoalCommand, GoalRuntime, GoalStatus, parse_goal_command, render_continuation,
};
use crate::nexus::ProviderUsage;
use crate::session::{SessionLog, read_goal};

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
