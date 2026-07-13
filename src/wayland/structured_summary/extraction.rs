//! Response-side extraction of a `CompactionSummary` (issue #475, ADR-0061).
//!
//! Both provider adapters (`mimir::providers::openai_codex_responses`,
//! `mimir::providers::anthropic_messages`) already parse their own
//! provider-specific response/SSE shape into the shared, provider-neutral
//! [`crate::nexus::AssistantTurn`]/[`crate::nexus::ToolCall`] types. This
//! module owns only the last, provider-neutral step: pulling the structured
//! JSON payload out of that shared shape (native path: the turn's visible
//! text; forced-tool fallback path: the single `emit_compaction_summary` tool
//! call) and validating it with [`super::validate::parse_compaction_summary_value`],
//! the same slice-1 validator every path reuses.
//!
//! No network, no provider-specific wire knowledge, no OAuth/auth material,
//! and no session-log mutation lives here.

use crate::nexus::{AssistantTurn, ToolCall};
use serde_json::Value;

use super::schema::CompactionSummary;
use super::validate::{SummaryValidationError, parse_compaction_summary_value};

/// The forced-virtual-tool name (issue #475). Iris never registers or
/// executes this tool through normal tool approval/execution policy; it
/// exists only as a schema-transport wrapper inside the summary request
/// builders and this extraction path.
pub(crate) const VIRTUAL_TOOL_NAME: &str = "emit_compaction_summary";

/// Why a provider turn could not be turned into a validated
/// `CompactionSummary`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SummaryExtractionError {
    /// Native path: the turn carried no non-empty visible text to parse as
    /// the structured-output JSON payload.
    EmptyNativeText,
    /// The recovered JSON failed slice-1 validation (malformed, missing/
    /// unknown/wrong-typed fields, or an all-empty summary).
    Invalid(SummaryValidationError),
    /// Forced-tool path: zero `emit_compaction_summary` calls were present.
    NoToolCall,
    /// Forced-tool path: more than one `emit_compaction_summary` call was
    /// present (the count is recorded for diagnostics).
    MultipleToolCalls(usize),
    /// Forced-tool path: a tool call other than `emit_compaction_summary` was
    /// present alongside it. Names are recorded for diagnostics; the payload
    /// is never inspected further once an unexpected call is seen.
    UnexpectedToolCalls(Vec<String>),
}

impl std::fmt::Display for SummaryExtractionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyNativeText => {
                write!(f, "native structured-output turn had no visible text")
            }
            Self::Invalid(error) => write!(f, "{error}"),
            Self::NoToolCall => write!(
                f,
                "forced-tool turn did not include an `{VIRTUAL_TOOL_NAME}` call"
            ),
            Self::MultipleToolCalls(count) => write!(
                f,
                "forced-tool turn included {count} `{VIRTUAL_TOOL_NAME}` calls; expected exactly one"
            ),
            Self::UnexpectedToolCalls(names) => write!(
                f,
                "forced-tool turn included unexpected tool call(s): {}",
                names.join(", ")
            ),
        }
    }
}

impl std::error::Error for SummaryExtractionError {}

/// Extract a validated `CompactionSummary` from a native structured-output
/// turn: the turn's visible text is the raw JSON payload (OpenAI Codex
/// Responses' `text.format` / Anthropic Messages' `output_config.format`
/// both surface the payload as ordinary assistant text on their respective
/// adapters' `AssistantTurn.text`).
pub(crate) fn extract_native_summary(
    turn: &AssistantTurn,
) -> Result<CompactionSummary, SummaryExtractionError> {
    let text = turn.text.as_deref().unwrap_or_default().trim();
    if text.is_empty() {
        return Err(SummaryExtractionError::EmptyNativeText);
    }
    let value: Value = serde_json::from_str(text).map_err(|error| {
        SummaryExtractionError::Invalid(SummaryValidationError::MalformedJson(error.to_string()))
    })?;
    parse_compaction_summary_value(&value).map_err(SummaryExtractionError::Invalid)
}

/// Extract a validated `CompactionSummary` from a forced-virtual-tool turn.
/// Requires exactly one `emit_compaction_summary` call and rejects any other
/// tool call alongside it -- see [`extract_forced_tool_summary_from_calls`]
/// for the exact rules.
pub(crate) fn extract_forced_tool_summary(
    turn: &AssistantTurn,
) -> Result<CompactionSummary, SummaryExtractionError> {
    extract_forced_tool_summary_from_calls(&turn.tool_calls)
}

/// As [`extract_forced_tool_summary`], starting from an already-extracted
/// tool-call slice. Rejects:
/// - any tool call whose name is not `emit_compaction_summary` (an "extra"
///   call the forced `tool_choice` should never have allowed through), and
/// - zero or more than one `emit_compaction_summary` call.
///
/// Exactly one `emit_compaction_summary` call, and nothing else, is the only
/// accepted shape.
pub(crate) fn extract_forced_tool_summary_from_calls(
    calls: &[ToolCall],
) -> Result<CompactionSummary, SummaryExtractionError> {
    let unexpected: Vec<String> = calls
        .iter()
        .map(|call| call.name.clone())
        .filter(|name| name != VIRTUAL_TOOL_NAME)
        .collect();
    if !unexpected.is_empty() {
        return Err(SummaryExtractionError::UnexpectedToolCalls(unexpected));
    }
    let matching: Vec<&ToolCall> = calls
        .iter()
        .filter(|call| call.name == VIRTUAL_TOOL_NAME)
        .collect();
    match matching.len() {
        0 => Err(SummaryExtractionError::NoToolCall),
        1 => parse_compaction_summary_value(&matching[0].arguments)
            .map_err(SummaryExtractionError::Invalid),
        count => Err(SummaryExtractionError::MultipleToolCalls(count)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good_summary_json() -> Value {
        json!({
            "goal": "Ship #475 structured summaries",
            "state": ["renderer written"],
            "decisions": ["native first, forced-tool fallback second"],
            "key_facts": ["src/wayland/structured_summary/ holds the new modules"],
            "next_steps": ["wire provider request plumbing"],
            "preserved_identifiers": ["DEPLOY-KEY-AB12CD34"]
        })
    }

    fn tool_call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments,
            thought_signature: None,
        }
    }

    /// Issue #475: the forced virtual tool is a schema transport only. It
    /// must never appear in the tool registry, so it can never reach normal
    /// Iris tool approval/execution policy -- it exists only inside the
    /// provider summary request builders and this extraction path. Sweep
    /// every registry configuration (bash-tool-mode on/off, model-compaction
    /// tool on/off; web tools have fixed unrelated names and are covered by
    /// the registry's own tests).
    #[test]
    fn virtual_tool_is_never_registered_in_the_tool_registry() {
        for bash_tool_mode in [false, true] {
            for model_compaction_tool in [false, true] {
                let tools = crate::tools::built_in_tools_for(bash_tool_mode, model_compaction_tool);
                assert!(
                    tools.iter().all(|tool| tool.name() != VIRTUAL_TOOL_NAME),
                    "`{VIRTUAL_TOOL_NAME}` must never be a model-visible tool \
                     (bash_tool_mode={bash_tool_mode}, model_compaction_tool={model_compaction_tool})"
                );
                // `by_name` resolves over the FULL registry (the resolution
                // path tool execution and approval use), independent of the
                // model-visible plan -- the stronger guarantee.
                assert!(
                    tools.by_name(VIRTUAL_TOOL_NAME).is_none(),
                    "`{VIRTUAL_TOOL_NAME}` must never resolve for execution/approval \
                     (bash_tool_mode={bash_tool_mode}, model_compaction_tool={model_compaction_tool})"
                );
            }
        }
    }

    #[test]
    fn native_ok_parses_visible_text_as_the_summary() {
        let turn = AssistantTurn {
            text: Some(good_summary_json().to_string()),
            ..AssistantTurn::default()
        };
        let summary = extract_native_summary(&turn).expect("valid native summary");
        assert_eq!(summary.goal, "Ship #475 structured summaries");
    }

    #[test]
    fn native_rejects_empty_text() {
        let turn = AssistantTurn::default();
        assert_eq!(
            extract_native_summary(&turn).unwrap_err(),
            SummaryExtractionError::EmptyNativeText
        );
    }

    #[test]
    fn native_rejects_malformed_json() {
        let turn = AssistantTurn {
            text: Some("{not valid json".to_string()),
            ..AssistantTurn::default()
        };
        assert!(matches!(
            extract_native_summary(&turn).unwrap_err(),
            SummaryExtractionError::Invalid(SummaryValidationError::MalformedJson(_))
        ));
    }

    #[test]
    fn native_rejects_a_validator_failure_same_as_the_direct_parser() {
        let mut invalid = good_summary_json();
        invalid.as_object_mut().unwrap().remove("goal");
        let turn = AssistantTurn {
            text: Some(invalid.to_string()),
            ..AssistantTurn::default()
        };
        assert_eq!(
            extract_native_summary(&turn).unwrap_err(),
            SummaryExtractionError::Invalid(SummaryValidationError::MissingField("goal"))
        );
    }

    #[test]
    fn forced_tool_ok_extracts_the_single_emit_call() {
        let turn = AssistantTurn {
            tool_calls: vec![tool_call(VIRTUAL_TOOL_NAME, good_summary_json())],
            ..AssistantTurn::default()
        };
        let summary = extract_forced_tool_summary(&turn).expect("valid forced-tool summary");
        assert_eq!(summary.goal, "Ship #475 structured summaries");
    }

    #[test]
    fn forced_tool_rejects_zero_calls() {
        let turn = AssistantTurn::default();
        assert_eq!(
            extract_forced_tool_summary(&turn).unwrap_err(),
            SummaryExtractionError::NoToolCall
        );
    }

    #[test]
    fn forced_tool_rejects_multiple_emit_calls() {
        let turn = AssistantTurn {
            tool_calls: vec![
                tool_call(VIRTUAL_TOOL_NAME, good_summary_json()),
                tool_call(VIRTUAL_TOOL_NAME, good_summary_json()),
            ],
            ..AssistantTurn::default()
        };
        assert_eq!(
            extract_forced_tool_summary(&turn).unwrap_err(),
            SummaryExtractionError::MultipleToolCalls(2)
        );
    }

    #[test]
    fn forced_tool_rejects_an_extra_unrelated_tool_call() {
        let turn = AssistantTurn {
            tool_calls: vec![
                tool_call(VIRTUAL_TOOL_NAME, good_summary_json()),
                tool_call("read", json!({ "path": "src/main.rs" })),
            ],
            ..AssistantTurn::default()
        };
        assert_eq!(
            extract_forced_tool_summary(&turn).unwrap_err(),
            SummaryExtractionError::UnexpectedToolCalls(vec!["read".to_string()])
        );
    }

    #[test]
    fn forced_tool_rejects_invalid_payload_in_the_matching_call() {
        let mut invalid = good_summary_json();
        invalid["state"] = json!("not-an-array");
        let turn = AssistantTurn {
            tool_calls: vec![tool_call(VIRTUAL_TOOL_NAME, invalid)],
            ..AssistantTurn::default()
        };
        assert!(matches!(
            extract_forced_tool_summary(&turn).unwrap_err(),
            SummaryExtractionError::Invalid(SummaryValidationError::WrongType(_))
        ));
    }
}
