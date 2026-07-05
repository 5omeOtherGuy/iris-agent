//! Microcompaction fold engine (ADR-0048, issue #378).
//!
//! A fold replaces a spent `Tool`-role result's content with a deterministic
//! stub, keeping the tool call and the message itself intact (the durable
//! `fold` entry in `session.rs` is the persistence; this module decides WHAT to
//! fold and renders the stub). Recovery stays one step away: the stub names the
//! workspace-relative path so the model can re-read it and the #373 recall tool
//! can fetch the original turn.
//!
//! # V1 policy: superseded reads only
//!
//! The committed M2 benchmark (`docs/benchmarks/issue-378-residual-tool-mass.md`)
//! re-scoped this milestone: superseded reads (latest-read-wins) carry the
//! entire detectable "spent" signal (~18% of tool-result mass), retired-failure
//! output is negligible (~1.5%, an unmeasurable upper bound because bash exit
//! status is not persisted), and single-use bash/read output dominates but is
//! not "spent". So V1 folds ONLY superseded reads.
//!
//! The engine is deliberately shaped as a set of pluggable policies so the
//! DEFERRED classes (retired-failure-output folding; bash `ToolOutputStore`
//! handle folding) can be added as further [`FoldPolicy`] arms without
//! reworking the pass: [`plan_folds`] dispatches over enabled policies and
//! returns a flat [`FoldPlan`] list the harness applies uniformly.

use crate::nexus::{Message, Role};
use serde_json::Value;
use std::path::Path;

/// One planned fold: the message-list index whose result content is replaced,
/// its durable entry id (the `fold` entry's target), and the deterministic stub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FoldPlan {
    pub(super) index: usize,
    pub(super) entry_id: String,
    pub(super) stub: String,
}

/// The fold policies that can each contribute candidates. V1 enables only
/// [`FoldPolicy::SupersededReads`]; the deferred classes are named here so the
/// dispatch surface documents the intended extension points without
/// implementing them (benchmark evidence, ADR-0048).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FoldPolicy {
    /// Latest-read-wins: an earlier successful `read`/`ls` whose workspace path
    /// is read/edited/written again later. Recoverable from the workspace.
    SupersededReads,
    // DEFERRED (do not implement in V1; benchmark says they reclaim ~1.5%):
    //   RetiredFailureOutput -- a failing command's output a later success of
    //     the same command retired. Needs a persisted exit code to classify.
    //   BashHandleOffload -- single-use bash output registered in the
    //     `ToolOutputStore` at fold time, the stub carrying the handle id.
}

/// The V1 enabled-policy set: superseded reads only.
pub(super) const V1_POLICIES: &[FoldPolicy] = &[FoldPolicy::SupersededReads];

/// Plan every fold under the enabled `policies`, over the coverable prefix only.
///
/// `entry_ids` is parallel to `messages` (`Some(id)` = a durable, coverable
/// on-disk result; `None` = a summary position or an id-less legacy entry).
/// `tail_start` is the index where the retained recent tail begins: indices at
/// or after it NEVER fold (the model's immediate working set stays verbatim).
/// `workspace` re-validates every persisted target as workspace-relative, so a
/// crafted or legacy transcript carrying an absolute or `..`-escaping path is
/// dropped (a fold never names a path outside the workspace).
pub(super) fn plan_folds(
    messages: &[Message],
    entry_ids: &[Option<String>],
    tail_start: usize,
    workspace: &Path,
    policies: &[FoldPolicy],
) -> Vec<FoldPlan> {
    let mut plans: Vec<FoldPlan> = Vec::new();
    for policy in policies {
        match policy {
            FoldPolicy::SupersededReads => {
                superseded_read_plans(messages, entry_ids, tail_start, workspace, &mut plans);
            }
        }
    }
    // Stable by index, deduping any index a later policy also claimed (V1 has a
    // single policy, so this is a guard for the deferred multi-policy future).
    plans.sort_by_key(|plan| plan.index);
    plans.dedup_by_key(|plan| plan.index);
    plans
}

/// Detect superseded reads (latest-read-wins) and render their stubs.
///
/// A candidate is a SUCCESSFUL `read`/`ls` result (an error-classified result
/// has `ok != true` and no target, so it is never a candidate -- ADR-0040
/// unresolved failures never fold). It is superseded when a LATER successful
/// `read`/`ls`/`edit`/`write` result names the same workspace-relative path.
/// Only coverable (`Some`-id) results before `tail_start` are eligible.
fn superseded_read_plans(
    messages: &[Message],
    entry_ids: &[Option<String>],
    tail_start: usize,
    workspace: &Path,
    out: &mut Vec<FoldPlan>,
) {
    // Per-index workspace-relative target of every successful path-bearing
    // result, so "read again later" is a forward scan over the same vector.
    let targets: Vec<Option<(&str, String)>> = messages
        .iter()
        .map(|message| successful_target(message, workspace))
        .collect();

    for (i, candidate) in targets.iter().enumerate() {
        if i >= tail_start {
            break; // retained tail: never fold.
        }
        // Only a coverable (durable-id) message can carry a fold entry.
        let Some(entry_id) = entry_ids.get(i).and_then(Option::as_ref) else {
            continue;
        };
        // Candidate class: a successful read/ls result naming a path.
        let Some((name, path)) = candidate.as_ref() else {
            continue;
        };
        if !matches!(*name, "read" | "ls") {
            continue;
        }
        // Superseded when a later successful read/ls/edit/write touches the same
        // path (latest-read-wins). Only the EARLIER copy folds.
        let superseded = targets[i + 1..]
            .iter()
            .flatten()
            .any(|(later_name, later)| {
                matches!(*later_name, "read" | "ls" | "edit" | "write") && later == path
            });
        if !superseded {
            continue;
        }
        out.push(FoldPlan {
            index: i,
            entry_id: entry_id.clone(),
            stub: superseded_read_stub(name, path),
        });
    }
}

/// The workspace-relative target of a SUCCESSFUL path-bearing tool result, or
/// `None`. Reuses the ADR-0021 envelope + ADR-0044 `metadata.target` convention
/// and re-checks the path through the workspace-relative floor, matching
/// `derive_carry_paths`'s security boundary (no absolute/traversal leakage).
fn successful_target<'a>(message: &'a Message, workspace: &Path) -> Option<(&'a str, String)> {
    if message.role != Role::Tool {
        return None;
    }
    let name = message.tool_name.as_deref()?;
    if !matches!(name, "read" | "ls" | "edit" | "write") {
        return None;
    }
    let result = serde_json::from_str::<Value>(&message.content).ok()?;
    if result.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let target = result
        .get("metadata")
        .and_then(|metadata| metadata.get("target"))
        .and_then(Value::as_str)?;
    let rel = crate::tools::path::workspace_relative(workspace, target)?;
    Some((name, rel))
}

/// Render the deterministic superseded-read stub. It NAMES the workspace-
/// relative path so the fold is recoverable (re-read, or #373 recall), and is a
/// pure function of `(tool_name, path)` so the same fold reproduces byte-for-
/// byte across live and resumed rebuilds. Kept extensible: the deferred bash
/// policy will render its own stub variant carrying a handle id.
pub(super) fn superseded_read_stub(tool_name: &str, path: &str) -> String {
    format!(
        "[folded] The `{tool_name}` result for `{path}` was superseded by a later read of the \
         same path and folded to reclaim context (ADR-0048). Re-read `{path}` for its current \
         contents, or use the recall tool to fetch the original turn verbatim."
    )
}

#[cfg(test)]
#[path = "fold_tests.rs"]
mod fold_tests;
