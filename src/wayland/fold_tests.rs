//! Focused local tool-result compaction planner tests.

use super::{FoldPlan, FoldReason, plan_folds};
use crate::config::{Settings, ToolClearingBackend, ToolClearingMode};
use crate::nexus::Message;
use crate::tools::test_support::{root_of, temp_dir};
use serde_json::json;

fn ok_result(call: &str, name: &str, target: &str, body: &str) -> Message {
    Message::tool_result(
        call,
        name,
        &json!({
            "ok": true,
            "content": body,
            "metadata": { "target": target }
        })
        .to_string(),
    )
}

fn plain_result(call: &str, name: &str, ok: bool, body: &str) -> Message {
    Message::tool_result(call, name, &json!({"ok": ok, "content": body}).to_string())
}

fn ids(n: usize) -> Vec<Option<String>> {
    (0..n).map(|i| Some(format!("{i:08x}"))).collect()
}

fn policy() -> crate::config::ToolResultCompactionPolicy {
    let mut policy = Settings {
        microcompaction: Some(true),
        ..Settings::default()
    }
    .tool_result_compaction()
    .unwrap();
    policy.semantic_dedupe.protect_recent_tool_results = 0;
    policy.semantic_dedupe.protect_recent_tokens = 0;
    policy
}

#[test]
fn retain_per_path_keeps_latest_n_even_when_bodies_are_duplicated() {
    let dir = temp_dir();
    let root = root_of(&dir);
    let messages = [
        ok_result("c1", "read", "a.rs", "same"),
        ok_result("c2", "read", "a.rs", "same"),
        ok_result("c3", "read", "a.rs", "same"),
        ok_result("c4", "read", "a.rs", "same"),
    ];
    let mut policy = policy();
    policy.semantic_dedupe.retain_per_path = 2;
    let plans = plan_folds(
        &messages,
        &ids(messages.len()),
        messages.len(),
        &root,
        &policy,
    );
    assert_eq!(
        plans.iter().map(|plan| plan.index).collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[test]
fn later_edit_supersedes_prior_reads_for_the_same_path() {
    let dir = temp_dir();
    let root = root_of(&dir);
    let messages = [
        ok_result("c1", "read", "src/lib.rs", "old one"),
        ok_result("c2", "read", "src/lib.rs", "old two"),
        ok_result("c3", "edit", "src/lib.rs", "changed"),
    ];
    let mut policy = policy();
    policy.semantic_dedupe.retain_per_path = 9;
    let plans = plan_folds(
        &messages,
        &ids(messages.len()),
        messages.len(),
        &root,
        &policy,
    );
    assert_eq!(
        plans.iter().map(|plan| plan.index).collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[test]
fn local_clearing_folds_older_eligible_results_and_keeps_recent_uses() {
    let dir = temp_dir();
    let root = root_of(&dir);
    let messages = [
        plain_result("c1", "grep", true, &"old ".repeat(40)),
        plain_result("c2", "grep", true, &"middle ".repeat(40)),
        plain_result("c3", "grep", true, &"recent ".repeat(40)),
    ];
    let mut policy = policy();
    policy.semantic_dedupe.enabled = false;
    policy.tool_clearing.enabled = true;
    policy.tool_clearing.backend = ToolClearingBackend::Local;
    policy.tool_clearing.mode = ToolClearingMode::Selected;
    policy.tool_clearing.eligible_tools = vec!["grep".to_string()];
    policy.tool_clearing.keep_recent_tool_uses = 1;
    policy.tool_clearing.clear_at_least_tokens = 1;
    let plans = plan_folds(
        &messages,
        &ids(messages.len()),
        messages.len(),
        &root,
        &policy,
    );
    assert_eq!(
        plans.iter().map(|plan| plan.index).collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert!(
        plans
            .iter()
            .all(|plan| plan.has_reason(FoldReason::ToolClearing))
    );
}

#[test]
fn exclusions_and_failures_default_off_are_hard_guards() {
    let dir = temp_dir();
    let root = root_of(&dir);
    let messages = [
        plain_result("c1", "bash", true, &"success ".repeat(40)),
        plain_result("c2", "bash", false, &"failure ".repeat(40)),
        plain_result("c3", "bash", true, &"recent ".repeat(40)),
    ];
    let mut policy = policy();
    policy.semantic_dedupe.enabled = false;
    policy.tool_clearing.enabled = true;
    policy.tool_clearing.mode = ToolClearingMode::Selected;
    policy.tool_clearing.eligible_tools = vec!["bash".to_string()];
    policy.tool_clearing.keep_recent_tool_uses = 1;
    policy.tool_clearing.clear_at_least_tokens = 1;
    policy.tool_clearing.excluded_tools = vec!["bash".to_string()];
    assert!(
        plan_folds(
            &messages,
            &ids(messages.len()),
            messages.len(),
            &root,
            &policy
        )
        .is_empty()
    );

    policy.tool_clearing.excluded_tools.clear();
    let plans = plan_folds(
        &messages,
        &ids(messages.len()),
        messages.len(),
        &root,
        &policy,
    );
    assert_eq!(
        plans.iter().map(|plan| plan.index).collect::<Vec<_>>(),
        vec![0]
    );
    policy.tool_clearing.include_failures = true;
    let plans = plan_folds(
        &messages,
        &ids(messages.len()),
        messages.len(),
        &root,
        &policy,
    );
    assert_eq!(
        plans.iter().map(|plan| plan.index).collect::<Vec<_>>(),
        vec![0, 1]
    );
}

#[test]
fn token_and_result_recency_guards_win_over_both_reducers() {
    let dir = temp_dir();
    let root = root_of(&dir);
    let messages = [
        ok_result("c1", "read", "a.rs", &"old ".repeat(40)),
        ok_result("c2", "read", "a.rs", &"new ".repeat(40)),
    ];
    let mut policy = policy();
    policy.tool_clearing.enabled = true;
    policy.tool_clearing.keep_recent_tool_uses = 1;
    policy.tool_clearing.clear_at_least_tokens = 1;
    policy.semantic_dedupe.protect_recent_tool_results = 2;
    assert!(
        plan_folds(
            &messages,
            &ids(messages.len()),
            messages.len(),
            &root,
            &policy
        )
        .is_empty()
    );
    policy.semantic_dedupe.protect_recent_tool_results = 0;
    assert!(
        plan_folds(&messages, &ids(messages.len()), 0, &root, &policy).is_empty(),
        "token tail protects every message"
    );
}

#[test]
fn overlapping_b_and_c_write_one_fold_with_combined_reasons_and_recovery_id() {
    let dir = temp_dir();
    let root = root_of(&dir);
    let messages = [
        ok_result("call-old", "read", "a.rs", &"old ".repeat(40)),
        ok_result("call-new", "read", "a.rs", &"new ".repeat(40)),
    ];
    let mut policy = policy();
    policy.tool_clearing.enabled = true;
    policy.tool_clearing.keep_recent_tool_uses = 1;
    policy.tool_clearing.clear_at_least_tokens = 1;
    let plans = plan_folds(
        &messages,
        &ids(messages.len()),
        messages.len(),
        &root,
        &policy,
    );
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    assert!(plan.has_reason(FoldReason::SemanticDedupe));
    assert!(plan.has_reason(FoldReason::ToolClearing));
    assert!(plan.stub.contains("semantic stale-read dedupe"));
    assert!(plan.stub.contains("local age/count"));
    assert!(plan.stub.contains("Durable entry id: `00000000`"));
    assert!(plan.stub.contains("tool_call_id: \"call-old\""));
    assert!(plan.stub.contains("recall(tool_call_id=\"call-old\")"));
    assert!(
        !plan.stub.contains("old old"),
        "folded content never leaks into stub"
    );
}

#[test]
fn only_durable_in_workspace_results_fold_and_plans_stay_ordered() {
    let dir = temp_dir();
    let root = root_of(&dir);
    let messages = [
        ok_result("c1", "read", "/etc/passwd", "outside"),
        ok_result("c2", "read", "/etc/passwd", "outside again"),
        ok_result("c3", "read", "b.rs", "b1"),
        ok_result("c4", "read", "a.rs", "a1"),
        ok_result("c5", "read", "b.rs", "b2"),
        ok_result("c6", "read", "a.rs", "a2"),
    ];
    let mut entry_ids = ids(messages.len());
    entry_ids[2] = None;
    let plans = plan_folds(&messages, &entry_ids, messages.len(), &root, &policy());
    let indices: Vec<usize> = plans.iter().map(|plan: &FoldPlan| plan.index).collect();
    assert_eq!(indices, vec![3]);
}

#[test]
fn stub_json_quotes_untrusted_call_ids_and_paths() {
    let dir = temp_dir();
    let root = root_of(&dir);
    let messages = [
        ok_result("call\nold", "read", "odd\npath.rs", "old"),
        ok_result("new", "read", "odd\npath.rs", "new"),
    ];
    let plans = plan_folds(
        &messages,
        &ids(messages.len()),
        messages.len(),
        &root,
        &policy(),
    );
    assert_eq!(plans.len(), 1);
    assert!(plans[0].stub.contains("call\\nold"));
    assert!(plans[0].stub.contains("odd\\npath.rs"));
    assert!(!plans[0].stub.contains("call\nold"));
}
