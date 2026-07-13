//! Env-gated LIVE capability probe for #475 (typed structured-output compaction
//! summaries). It sends ONE minimal request per OAuth lane -- OpenAI Codex
//! Responses and Anthropic Messages -- with the #475 canonical
//! `CompactionSummary` schema as provider-native structured output, then (only
//! if native is rejected) retries that lane's forced-virtual-tool fallback. It
//! records ground truth: whether each lane actually honours the structured-
//! output request shape #475 assumes.
//!
//! This makes REAL API calls and is DOUBLE-gated exactly like
//! `compaction_live_bench`: `#[ignore]` keeps `cargo test` (the gate) from
//! running it, and an `IRIS_BENCH_LIVE=1` env guard makes even
//! `cargo test -- --ignored` a no-op unless the operator opts in. Credentials
//! come from the existing OAuth token stores (`OpenAiCodexTokenStore`,
//! `AnthropicTokenStore`); this module never hand-rolls auth. On any auth/infra
//! failure the probe records the verbatim error rather than asserting a
//! capability verdict.
//!
//! The heavy lifting (auth headers, endpoint resolution, SSE assembly) lives in
//! `#[cfg(test)]` `probe_compaction_summary` methods on each provider so the
//! real header/URL/token code is reused verbatim; this module owns only the
//! canonical schema, the shared `CompactionSummary` validator, and the two
//! ignored test entry points.

#![cfg(test)]

use serde_json::{Value, json};

/// The Codex OAuth lane's cheap model, matching the live bench.
pub(crate) const PROBE_MODEL_CODEX: &str = "gpt-5.4-mini";
/// The Anthropic OAuth lane's cheap model, matching the live bench.
pub(crate) const PROBE_MODEL_ANTHROPIC: &str = "claude-haiku-4-5";

pub(crate) const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api";
pub(crate) const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";

/// The forced-virtual-tool name (#475). Iris never executes it; it is a schema
/// transport only.
pub(crate) const VIRTUAL_TOOL_NAME: &str = "emit_compaction_summary";

/// A short dedicated summarizer instruction, used as the summary system prompt.
pub(crate) const SUMMARY_INSTRUCTION: &str = "You compact a coding session. Return only a \
    structured compaction summary matching the provided schema: goal, state, decisions, \
    key_facts, next_steps. Preserve exact identifiers, file paths, and constraints.";

/// Which request shape the probe sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeMode {
    /// Provider-native structured output (`text.format` / `output_config.format`).
    Native,
    /// Forced single virtual tool with the same schema.
    ForcedTool,
}

impl ProbeMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::ForcedTool => "forced-tool",
        }
    }
}

/// The #475 canonical `CompactionSummary` JSON Schema, restricted to the shared
/// provider-safe subset: root object only, all fields required,
/// `additionalProperties: false`, no `$ref`/`oneOf`/`anyOf`/regex/bounds.
pub(crate) fn canonical_compaction_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["goal", "state", "decisions", "key_facts", "next_steps"],
        "properties": {
            "goal": { "type": "string" },
            "state": { "type": "array", "items": { "type": "string" } },
            "decisions": { "type": "array", "items": { "type": "string" } },
            "key_facts": { "type": "array", "items": { "type": "string" } },
            "next_steps": { "type": "array", "items": { "type": "string" } }
        }
    })
}

/// A minimal 5-line toy transcript in the #475 compact-renderer shape, including
/// a credential-shaped needle the user asked to preserve (audit F17).
pub(crate) fn toy_transcript() -> String {
    "Summarize this coding session into the structured compaction summary schema.\n\
     F range=msg_1..msg_5 carry_path=src/telemetry/sink.rs\n\
     U Implement issue #475 typed compaction summaries; keep the token DEPLOY-KEY-AB12CD34 I asked to preserve.\n\
     A Agreed: native structured output first, forced-virtual-tool fallback second, deterministic excerpts last.\n\
     TC read path=src/mimir/providers/anthropic_messages.rs\n\
     TR ok preview=\"output_config.format json_schema path\""
        .to_string()
}

/// A single lane/mode probe result.
#[derive(Debug, Clone)]
pub(crate) struct ProbeOutcome {
    pub(crate) lane: String,
    pub(crate) model: String,
    pub(crate) mode: ProbeMode,
    pub(crate) status: u16,
    pub(crate) success: bool,
    pub(crate) error_type: Option<String>,
    pub(crate) error_message: Option<String>,
    /// The extracted structured summary JSON, if the request succeeded and a
    /// JSON object could be recovered from the response.
    pub(crate) summary: Option<Value>,
    /// Whether `summary` parses as a valid canonical `CompactionSummary`.
    pub(crate) schema_valid: bool,
    /// Verbatim (truncated) response body for the record on rejection.
    pub(crate) body_excerpt: String,
}

impl ProbeOutcome {
    pub(crate) fn rejected(
        lane: impl Into<String>,
        model: impl Into<String>,
        mode: ProbeMode,
        status: u16,
        error_type: Option<String>,
        error_message: Option<String>,
        body: &str,
    ) -> Self {
        Self {
            lane: lane.into(),
            model: model.into(),
            mode,
            status,
            success: false,
            error_type,
            error_message,
            summary: None,
            schema_valid: false,
            body_excerpt: truncate(body, 800),
        }
    }

    pub(crate) fn succeeded(
        lane: impl Into<String>,
        model: impl Into<String>,
        mode: ProbeMode,
        status: u16,
        summary: Option<Value>,
    ) -> Self {
        let schema_valid = summary
            .as_ref()
            .is_some_and(|value| validate_summary(value).is_ok());
        Self {
            lane: lane.into(),
            model: model.into(),
            mode,
            status,
            success: true,
            error_type: None,
            error_message: None,
            summary,
            schema_valid,
            body_excerpt: String::new(),
        }
    }

    /// One-line record for the console and the issue comment.
    pub(crate) fn record_line(&self) -> String {
        if self.success {
            format!(
                "lane={} model={} mode={} status={} schema_valid={} keys={}",
                self.lane,
                self.model,
                self.mode.label(),
                self.status,
                self.schema_valid,
                self.summary
                    .as_ref()
                    .and_then(Value::as_object)
                    .map(|map| map.keys().cloned().collect::<Vec<_>>().join(","))
                    .unwrap_or_else(|| "<none>".to_string()),
            )
        } else {
            format!(
                "lane={} model={} mode={} status={} error_type={} message={:?}",
                self.lane,
                self.model,
                self.mode.label(),
                self.status,
                self.error_type.as_deref().unwrap_or("unknown"),
                self.error_message.as_deref().unwrap_or(&self.body_excerpt),
            )
        }
    }
}

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= max {
        return trimmed.to_string();
    }
    let mut end = max;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &trimmed[..end])
}

/// Parse a value into the canonical `CompactionSummary` shape and reject
/// malformed/missing/unknown/wrong-typed fields. `Ok` means schema-valid.
pub(crate) fn validate_summary(value: &Value) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| "summary is not a JSON object".to_string())?;
    const REQUIRED: [&str; 5] = ["goal", "state", "decisions", "key_facts", "next_steps"];
    for key in object.keys() {
        if !REQUIRED.contains(&key.as_str()) {
            return Err(format!("unknown field: {key}"));
        }
    }
    for key in REQUIRED {
        if !object.contains_key(key) {
            return Err(format!("missing field: {key}"));
        }
    }
    if !object["goal"].is_string() {
        return Err("goal is not a string".to_string());
    }
    for key in ["state", "decisions", "key_facts", "next_steps"] {
        let array = object[key]
            .as_array()
            .ok_or_else(|| format!("{key} is not an array"))?;
        if array.iter().any(|item| !item.is_string()) {
            return Err(format!("{key} contains a non-string item"));
        }
    }
    Ok(())
}

/// Assemble the model-emitted JSON from a raw Anthropic Messages SSE body:
/// concatenated `text_delta` text (native structured output) and concatenated
/// `input_json_delta` partial JSON (forced-tool input). Also surfaces a
/// mid-stream `error` event if the server sent one after a 200.
pub(crate) fn collect_anthropic_sse(body: &str) -> AnthropicSseParts {
    let mut text = String::new();
    let mut tool_json = String::new();
    let mut stream_error: Option<Value> = None;
    for line in body.lines() {
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(data.trim()) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str) {
            Some("error") => stream_error = event.get("error").cloned().or(Some(event)),
            Some("content_block_delta") => {
                if let Some(delta) = event.get("delta") {
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(chunk) = delta.get("text").and_then(Value::as_str) {
                                text.push_str(chunk);
                            }
                        }
                        Some("input_json_delta") => {
                            if let Some(chunk) = delta.get("partial_json").and_then(Value::as_str) {
                                tool_json.push_str(chunk);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    AnthropicSseParts {
        text,
        tool_json,
        stream_error,
    }
}

pub(crate) struct AnthropicSseParts {
    pub(crate) text: String,
    pub(crate) tool_json: String,
    pub(crate) stream_error: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validator_accepts_canonical_shape_and_rejects_deviations() {
        let good = json!({
            "goal": "Ship #475 structured summaries",
            "state": ["probe written"],
            "decisions": ["native first, forced-tool fallback"],
            "key_facts": ["DEPLOY-KEY-AB12CD34"],
            "next_steps": ["author ADR"]
        });
        assert!(validate_summary(&good).is_ok());
        assert!(
            validate_summary(&json!({ "goal": "x" })).is_err(),
            "missing fields"
        );
        let mut extra = good.clone();
        extra["unexpected"] = json!(true);
        assert!(validate_summary(&extra).is_err(), "unknown field");
        let mut wrong = good.clone();
        wrong["state"] = json!("not-an-array");
        assert!(validate_summary(&wrong).is_err(), "wrong type");
        let mut nonstring = good;
        nonstring["decisions"] = json!([1, 2]);
        assert!(
            validate_summary(&nonstring).is_err(),
            "non-string array item"
        );
    }

    #[test]
    fn schema_is_provider_safe_subset() {
        let schema = canonical_compaction_schema();
        let text = serde_json::to_string(&schema).expect("schema serializes");
        for forbidden in [
            "$ref", "oneOf", "anyOf", "allOf", "pattern", "minimum", "maximum",
        ] {
            assert!(!text.contains(forbidden), "schema must omit {forbidden}");
        }
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["required"],
            json!(["goal", "state", "decisions", "key_facts", "next_steps"])
        );
    }

    #[test]
    fn anthropic_sse_collects_text_and_tool_json() {
        let body = "event: content_block_delta\n\
             data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"{\\\"goal\\\":\"}}\n\n\
             data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"goal\\\"\"}}\n\n";
        let parts = collect_anthropic_sse(body);
        assert_eq!(parts.text, "{\"goal\":");
        assert_eq!(parts.tool_json, "{\"goal\"");
        assert!(parts.stream_error.is_none());
    }

    /// LIVE: OpenAI Codex Responses OAuth lane. Native structured output first;
    /// forced-tool fallback only if native is rejected.
    #[test]
    #[ignore = "live Codex API call; set IRIS_BENCH_LIVE=1 to run"]
    fn probe_structured_output_codex() {
        if !live_enabled("probe_structured_output_codex") {
            return;
        }
        run_lane(Lane::Codex);
    }

    /// LIVE: Anthropic Messages OAuth lane. Native structured output first;
    /// forced-tool fallback only if native is rejected.
    #[test]
    #[ignore = "live Anthropic API call; set IRIS_BENCH_LIVE=1 to run"]
    fn probe_structured_output_anthropic() {
        if !live_enabled("probe_structured_output_anthropic") {
            return;
        }
        if !crate::mimir::auth::anthropic::claude_code_credentials_available() {
            eprintln!(
                "probe_structured_output_anthropic: no Claude Code credentials discovered \
                 (expected ~/.claude/.credentials.json); recording as infra-skip, not a rejection"
            );
            return;
        }
        run_lane(Lane::Anthropic);
    }

    #[derive(Clone, Copy)]
    enum Lane {
        Codex,
        Anthropic,
    }

    fn live_enabled(name: &str) -> bool {
        if std::env::var("IRIS_BENCH_LIVE").ok().as_deref() != Some("1") {
            eprintln!("{name}: skipped (set IRIS_BENCH_LIVE=1 to run)");
            return false;
        }
        true
    }

    fn probe(lane: Lane, mode: ProbeMode) -> anyhow::Result<ProbeOutcome> {
        use crate::mimir::retry::RetryPolicy;
        use crate::mimir::selection::{CodexTransport, ContextManagement, PromptCacheRetention};
        use tokio_util::sync::CancellationToken;
        let cancel = CancellationToken::new();
        match lane {
            Lane::Codex => {
                let provider =
                    crate::mimir::providers::openai_codex_responses::OpenAiCodexResponsesProvider::new(
                        PROBE_MODEL_CODEX,
                        CODEX_BASE_URL,
                        None,
                        SUMMARY_INSTRUCTION,
                        "iris-structured-summary-probe",
                        PromptCacheRetention::DEFAULT,
                        RetryPolicy::default(),
                        CodexTransport::Sse,
                    )?;
                provider.probe_compaction_summary(mode, &cancel)
            }
            Lane::Anthropic => {
                let provider = crate::mimir::providers::anthropic_messages::AnthropicProvider::new(
                    PROBE_MODEL_ANTHROPIC,
                    ANTHROPIC_BASE_URL,
                    None,
                    SUMMARY_INSTRUCTION,
                    PromptCacheRetention::DEFAULT,
                    ContextManagement::default(),
                    RetryPolicy::default(),
                )?;
                provider.probe_compaction_summary(mode, &cancel)
            }
        }
    }

    /// Native first; forced-tool fallback only when native is rejected. Prints a
    /// compact record for the issue comment and asserts the lane produced a
    /// schema-valid summary through at least one path (unless the only failures
    /// were infra, which are printed and tolerated).
    fn run_lane(lane: Lane) {
        let native = match probe(lane, ProbeMode::Native) {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("PROBE INFRA FAILURE (native): {error:#}");
                return;
            }
        };
        println!("PROBE {}", native.record_line());

        if native.success && native.schema_valid {
            return;
        }

        // Native did not yield a schema-valid summary. Probe the forced-tool
        // fallback and record whether THAT works.
        let fallback = match probe(lane, ProbeMode::ForcedTool) {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("PROBE INFRA FAILURE (forced-tool): {error:#}");
                return;
            }
        };
        println!("PROBE {}", fallback.record_line());

        assert!(
            native.success || fallback.success,
            "both probe paths failed for the lane; native=[{}] forced-tool=[{}]",
            native.record_line(),
            fallback.record_line(),
        );
        assert!(
            native.schema_valid || fallback.schema_valid,
            "no path returned a schema-valid CompactionSummary; native=[{}] forced-tool=[{}]",
            native.record_line(),
            fallback.record_line(),
        );
    }
}
