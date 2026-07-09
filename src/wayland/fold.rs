//! Local tool-result compaction planner (ADR-0048/0051).
//!
//! Detection is pure and separate from durable flushing. The planner composes
//! semantic stale-read dedupe (class C) and older-result clearing (class B),
//! applies shared recency guards first, and emits at most one fold per durable
//! tool result. Originals remain in the session JSONL and are recoverable by
//! `tool_call_id`.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::config::{ToolClearingBackend, ToolClearingMode, ToolResultCompactionPolicy};
use crate::nexus::{Message, Role};
use crate::session::estimate_tokens;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FoldReason {
    SemanticDedupe,
    ToolClearing,
}

/// One planned fold. Pairing fields are copied into the deterministic stub so
/// the model has an exact transcript-recovery instruction after the body is
/// replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FoldPlan {
    pub(super) index: usize,
    pub(super) entry_id: String,
    pub(super) tool_call_id: String,
    pub(super) tool_name: String,
    pub(super) reasons: Vec<FoldReason>,
    pub(super) stub: String,
}

impl FoldPlan {
    pub(super) fn has_reason(&self, reason: FoldReason) -> bool {
        self.reasons.contains(&reason)
    }
}

/// Plan local folds over durable messages only. `tail_start` is the token-based
/// recent boundary computed by the harness; `protect_recent_tool_results` adds
/// a count guard across all tool results. Either guard protects a message from
/// both reducers.
pub(super) fn plan_folds(
    messages: &[Message],
    entry_ids: &[Option<String>],
    tail_start: usize,
    workspace: &Path,
    policy: &ToolResultCompactionPolicy,
) -> Vec<FoldPlan> {
    if !policy.enabled {
        return Vec::new();
    }
    let protected_results = protected_result_indices(
        messages,
        policy.semantic_dedupe.protect_recent_tool_results as usize,
    );
    let is_protected =
        |index: usize| index >= tail_start || protected_results.binary_search(&index).is_ok();

    let mut reasons: BTreeMap<usize, Vec<FoldReason>> = BTreeMap::new();
    let targets: Vec<Option<SuccessfulTarget<'_>>> = messages
        .iter()
        .map(|message| successful_target(message, workspace))
        .collect();

    if policy.semantic_dedupe.enabled {
        plan_semantic_dedupe(
            messages,
            entry_ids,
            &targets,
            &is_protected,
            policy.semantic_dedupe.retain_per_path as usize,
            &mut reasons,
        );
    }
    if policy.tool_clearing.enabled
        && matches!(
            policy.tool_clearing.backend,
            ToolClearingBackend::Local | ToolClearingBackend::Auto
        )
    {
        plan_tool_clearing(messages, entry_ids, &is_protected, policy, &mut reasons);
    }

    reasons
        .into_iter()
        .filter_map(|(index, reasons)| {
            let message = messages.get(index)?;
            let entry_id = entry_ids.get(index)?.as_ref()?.clone();
            let tool_call_id = message.tool_call_id.as_ref()?.clone();
            let tool_name = message.tool_name.as_ref()?.clone();
            let path = targets
                .get(index)
                .and_then(Option::as_ref)
                .map(|target| target.path.as_str());
            let stub = fold_stub(&tool_name, &entry_id, &tool_call_id, path, &reasons);
            Some(FoldPlan {
                index,
                entry_id,
                tool_call_id,
                tool_name,
                reasons,
                stub,
            })
        })
        .collect()
}

fn protected_result_indices(messages: &[Message], keep: usize) -> Vec<usize> {
    if keep == 0 {
        return Vec::new();
    }
    let mut indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == Role::Tool).then_some(index))
        .collect();
    let drain = indices.len().saturating_sub(keep);
    indices.drain(..drain);
    indices
}

fn plan_semantic_dedupe(
    messages: &[Message],
    entry_ids: &[Option<String>],
    targets: &[Option<SuccessfulTarget<'_>>],
    is_protected: &impl Fn(usize) -> bool,
    retain_per_path: usize,
    out: &mut BTreeMap<usize, Vec<FoldReason>>,
) {
    for (index, target) in targets.iter().enumerate() {
        if is_protected(index) || !is_durable_tool_result(messages, entry_ids, index) {
            continue;
        }
        let Some(target) = target else { continue };
        if !matches!(target.name, "read" | "ls") {
            continue;
        }
        let later = targets[index + 1..].iter().flatten().filter(|later| {
            later.path == target.path && matches!(later.name, "read" | "ls" | "edit" | "write")
        });
        let mut later_reads = 0usize;
        let mut later_mutation = false;
        for later in later {
            match later.name {
                "read" | "ls" => later_reads += 1,
                "edit" | "write" => later_mutation = true,
                _ => {}
            }
        }
        if later_mutation || later_reads >= retain_per_path {
            add_reason(out, index, FoldReason::SemanticDedupe);
        }
    }
}

fn plan_tool_clearing(
    messages: &[Message],
    entry_ids: &[Option<String>],
    is_protected: &impl Fn(usize) -> bool,
    policy: &ToolResultCompactionPolicy,
    out: &mut BTreeMap<usize, Vec<FoldReason>>,
) {
    let clearing = &policy.tool_clearing;
    let mut eligible: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            if is_protected(index) || !is_durable_tool_result(messages, entry_ids, index) {
                return None;
            }
            let name = message.tool_name.as_deref()?;
            if !clearing_candidate(name, policy) {
                return None;
            }
            if !clearing.include_failures && !successful_result(message) {
                return None;
            }
            Some(index)
        })
        .collect();

    let keep = clearing.keep_recent_tool_uses as usize;
    eligible.truncate(eligible.len().saturating_sub(keep));
    let clearing_tokens = eligible.iter().fold(0u64, |total, index| {
        total.saturating_add(estimate_tokens(&messages[*index].content))
    });
    if clearing_tokens < clearing.clear_at_least_tokens {
        return;
    }
    for index in eligible {
        add_reason(out, index, FoldReason::ToolClearing);
    }
}

fn clearing_candidate(name: &str, policy: &ToolResultCompactionPolicy) -> bool {
    let clearing = &policy.tool_clearing;
    if clearing
        .excluded_tools
        .iter()
        .any(|excluded| excluded == name)
    {
        return false;
    }
    let mode_allows = match clearing.mode {
        ToolClearingMode::Replayable => matches!(name, "read" | "ls" | "grep" | "find"),
        ToolClearingMode::Selected => clearing
            .eligible_tools
            .iter()
            .any(|eligible| eligible == name),
        ToolClearingMode::AllRecoverable => true,
    };
    mode_allows
        && (clearing.eligible_tools.is_empty()
            || clearing.mode == ToolClearingMode::Selected
            || clearing
                .eligible_tools
                .iter()
                .any(|eligible| eligible == name))
}

fn is_durable_tool_result(
    messages: &[Message],
    entry_ids: &[Option<String>],
    index: usize,
) -> bool {
    messages.get(index).is_some_and(|message| {
        message.role == Role::Tool
            && message
                .tool_call_id
                .as_deref()
                .is_some_and(|id| !id.is_empty())
            && message
                .tool_name
                .as_deref()
                .is_some_and(|name| !name.is_empty())
            && entry_ids.get(index).and_then(Option::as_ref).is_some()
    })
}

fn successful_result(message: &Message) -> bool {
    serde_json::from_str::<Value>(&message.content)
        .ok()
        .and_then(|value| value.get("ok").and_then(Value::as_bool))
        == Some(true)
}

struct SuccessfulTarget<'a> {
    name: &'a str,
    path: String,
}

fn successful_target<'a>(message: &'a Message, workspace: &Path) -> Option<SuccessfulTarget<'a>> {
    if message.role != Role::Tool || !successful_result(message) {
        return None;
    }
    let name = message.tool_name.as_deref()?;
    if !matches!(name, "read" | "ls" | "edit" | "write") {
        return None;
    }
    let result = serde_json::from_str::<Value>(&message.content).ok()?;
    let target = result
        .get("metadata")
        .and_then(|metadata| metadata.get("target"))
        .and_then(Value::as_str)?;
    let path = crate::tools::path::workspace_relative(workspace, target)?;
    Some(SuccessfulTarget { name, path })
}

fn add_reason(out: &mut BTreeMap<usize, Vec<FoldReason>>, index: usize, reason: FoldReason) {
    let reasons = out.entry(index).or_default();
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

fn fold_stub(
    tool_name: &str,
    entry_id: &str,
    tool_call_id: &str,
    path: Option<&str>,
    reasons: &[FoldReason],
) -> String {
    let reason = reasons
        .iter()
        .map(|reason| match reason {
            FoldReason::SemanticDedupe => "semantic stale-read dedupe",
            FoldReason::ToolClearing => "local age/count tool-result clearing",
        })
        .collect::<Vec<_>>()
        .join(" + ");
    let call_id =
        serde_json::to_string(tool_call_id).unwrap_or_else(|_| format!("\"{tool_call_id}\""));
    let target = path
        .map(|path| {
            let path = serde_json::to_string(path).unwrap_or_else(|_| "\"(invalid)\"".to_string());
            format!(" for workspace path {path}")
        })
        .unwrap_or_default();
    format!(
        "[folded] The original `{tool_name}` tool-result body{target} was folded by {reason}. \
         Durable entry id: `{entry_id}`. tool_call_id: {call_id}. Recover the original \
         assistant tool call and tool result exactly with the recall tool: \
         recall(tool_call_id={call_id})."
    )
}

#[cfg(test)]
#[path = "fold_tests.rs"]
mod fold_tests;
