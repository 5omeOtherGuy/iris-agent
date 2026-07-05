//! Tests for the microcompaction fold engine (ADR-0048, issue #378): superseded
//! -read detection (latest-read-wins), the retained-tail and error-classified
//! guards, the workspace-relative security floor, and the deterministic stub.

use super::{FoldPlan, V1_POLICIES, plan_folds, superseded_read_stub};
use crate::nexus::Message;
use crate::tools::test_support::{root_of, temp_dir};
use serde_json::json;

/// A successful path-bearing tool result (ADR-0021 envelope + ADR-0044 target).
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

/// An error-classified result (ADR-0040): `ok:false`, no target.
fn err_result(call: &str, name: &str) -> Message {
    Message::tool_result(
        call,
        name,
        &json!({
            "ok": false,
            "error": "not found",
            "metadata": { "class": "not-found" }
        })
        .to_string(),
    )
}

/// Durable ids for every message (all coverable), so id-availability never
/// masks a policy decision under test.
fn ids(n: usize) -> Vec<Option<String>> {
    (0..n).map(|i| Some(format!("{i:08x}"))).collect()
}

#[test]
fn superseded_read_is_folded_and_the_stub_names_the_path() {
    let dir = temp_dir();
    let root = root_of(&dir);
    let messages = [
        ok_result("c1", "read", "a.rs", "FIRST-NEEDLE body"),
        ok_result("c2", "read", "a.rs", "second body"),
    ];
    let entry_ids = ids(messages.len());
    // tail_start past the end: nothing is in the retained tail.
    let plans = plan_folds(&messages, &entry_ids, messages.len(), &root, V1_POLICIES);
    assert_eq!(plans.len(), 1, "the earlier read is superseded and folds");
    let plan = &plans[0];
    assert_eq!(plan.index, 0);
    assert_eq!(plan.entry_id, "00000000");
    // The stub names the workspace-relative path so it stays recoverable.
    assert!(plan.stub.contains("a.rs"));
    assert!(!plan.stub.contains("FIRST-NEEDLE"));
}

#[test]
fn a_read_that_is_never_re_touched_is_not_folded() {
    let dir = temp_dir();
    let root = root_of(&dir);
    let messages = [
        ok_result("c1", "read", "a.rs", "body a"),
        ok_result("c2", "read", "b.rs", "body b"),
    ];
    let entry_ids = ids(messages.len());
    let plans = plan_folds(&messages, &entry_ids, messages.len(), &root, V1_POLICIES);
    assert!(plans.is_empty(), "distinct paths, nothing superseded");
}

#[test]
fn ls_superseded_by_a_later_edit_of_the_same_path_folds() {
    let dir = temp_dir();
    let root = root_of(&dir);
    let messages = [
        ok_result("c1", "ls", "src", "old listing"),
        ok_result("c2", "ls", "src", "new listing"),
    ];
    let entry_ids = ids(messages.len());
    let plans = plan_folds(&messages, &entry_ids, messages.len(), &root, V1_POLICIES);
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].index, 0);
}

#[test]
fn the_retained_tail_never_folds() {
    let dir = temp_dir();
    let root = root_of(&dir);
    // Two reads of a.rs: the first (index 0) is superseded. With tail_start=0
    // EVERY message is in the retained tail, so nothing folds even though the
    // supersede relationship exists.
    let messages = [
        ok_result("c1", "read", "a.rs", "first"),
        ok_result("c2", "read", "a.rs", "second"),
    ];
    let entry_ids = ids(messages.len());
    let plans = plan_folds(&messages, &entry_ids, 0, &root, V1_POLICIES);
    assert!(plans.is_empty(), "tail is protected");

    // Move the boundary so only index 0 is foldable (index 1 stays in the tail).
    let plans = plan_folds(&messages, &entry_ids, 1, &root, V1_POLICIES);
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].index, 0);
}

#[test]
fn an_error_classified_read_never_folds_even_when_a_path_repeats() {
    let dir = temp_dir();
    let root = root_of(&dir);
    // An earlier ERROR read of a.rs (ok:false, no target) then a later success:
    // the failure is not a candidate (no target), and the later success is not
    // superseded by anything, so nothing folds.
    let messages = [
        err_result("c1", "read"),
        ok_result("c2", "read", "a.rs", "recovered body"),
    ];
    let entry_ids = ids(messages.len());
    let plans = plan_folds(&messages, &entry_ids, messages.len(), &root, V1_POLICIES);
    assert!(
        plans.is_empty(),
        "an error-classified read is never a fold candidate"
    );

    // A successful read followed by a LATER error read of the same path is NOT
    // superseded: the failure carries no target, so it never retires an earlier
    // success (guards against folding a still-valid read behind a failed retry).
    let messages = [
        ok_result("c1", "read", "a.rs", "still-valid body"),
        err_result("c2", "read"),
    ];
    let plans = plan_folds(&messages, &entry_ids, messages.len(), &root, V1_POLICIES);
    assert!(
        plans.is_empty(),
        "a failed re-read does not supersede a good read"
    );
}

#[test]
fn a_path_outside_the_workspace_is_never_folded() {
    let dir = temp_dir();
    let root = root_of(&dir);
    // Absolute and traversal targets are dropped by the workspace-relative floor,
    // so neither becomes a candidate nor a superseder -- no fold names a path
    // outside the workspace.
    let messages = [
        ok_result("c1", "read", "/etc/passwd", "leak"),
        ok_result("c2", "read", "/etc/passwd", "leak again"),
        ok_result("c3", "read", "../../secret", "escape"),
        ok_result("c4", "read", "../../secret", "escape again"),
    ];
    let entry_ids = ids(messages.len());
    let plans = plan_folds(&messages, &entry_ids, messages.len(), &root, V1_POLICIES);
    assert!(plans.is_empty());
}

#[test]
fn only_coverable_results_with_a_durable_id_fold() {
    let dir = temp_dir();
    let root = root_of(&dir);
    let messages = [
        ok_result("c1", "read", "a.rs", "first"),
        ok_result("c2", "read", "a.rs", "second"),
    ];
    // The superseded read (index 0) has no durable id, so it cannot carry a fold
    // entry and is skipped.
    let entry_ids = vec![None, Some("00000001".to_string())];
    let plans = plan_folds(&messages, &entry_ids, messages.len(), &root, V1_POLICIES);
    assert!(plans.is_empty(), "an id-less result cannot be folded");
}

#[test]
fn superseded_read_stub_is_deterministic_and_reproducible() {
    // Pure function of (tool, path): the same fold reproduces byte-for-byte, so
    // live and resumed rebuilds agree.
    let a = superseded_read_stub("read", "src/lib.rs");
    let b = superseded_read_stub("read", "src/lib.rs");
    assert_eq!(a, b);
    assert!(a.contains("src/lib.rs"));
    assert!(a.contains("recall"));
}

#[test]
fn plans_are_ordered_by_index() {
    let dir = temp_dir();
    let root = root_of(&dir);
    // a.rs read at 0 and 2, b.rs read at 1 and 3: indices 0 and 1 both fold, in
    // index order.
    let messages = [
        ok_result("c1", "read", "a.rs", "a1"),
        ok_result("c2", "read", "b.rs", "b1"),
        ok_result("c3", "read", "a.rs", "a2"),
        ok_result("c4", "read", "b.rs", "b2"),
    ];
    let entry_ids = ids(messages.len());
    let plans = plan_folds(&messages, &entry_ids, messages.len(), &root, V1_POLICIES);
    let indices: Vec<usize> = plans.iter().map(|p: &FoldPlan| p.index).collect();
    assert_eq!(indices, vec![0, 1]);
}
