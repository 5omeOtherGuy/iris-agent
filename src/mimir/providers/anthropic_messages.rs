//! Anthropic Messages provider (Claude Code subscription / OAuth lane).
//! Mirrors `openai_codex_responses.rs`: the request is built eagerly, then a
//! blocking reqwest + SSE parse runs through the shared `transport` channel +
//! one-shot reauth glue, with each SSE event assembled into an `AssistantTurn`.
//!
//! ponytail: only the Claude Code subscription OAuth lane (Bearer token, no
//! x-api-key, no thinking replay, no transient backoff). Add the API-key lane
//! or extended-thinking replay only if a real need shows up.

use std::io::BufReader;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::{Value, json};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

use super::transport::{
    Attempt, HttpClass, TurnSink, classify_http_status, for_each_sse_event, run_with_reauth,
    spawn_stream,
};
use crate::mimir::auth::anthropic::AnthropicTokenStore;
use crate::mimir::selection::ReasoningEffort;
use crate::nexus::{
    AssistantTurn, ChatProvider, Message, ModelOrigin, ProviderStream, ReasoningBlock, Role,
    ToolCall, Tools,
};

/// Base output-token allowance. When extended thinking is enabled, the thinking
/// budget is added on top (Anthropic requires `max_tokens > budget_tokens`), so
/// the model keeps roughly this much room for its final answer.
const MAX_TOKENS: u32 = 8192;
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA: &str = "oauth-2025-04-20,claude-code-20250219";
const PROVIDER_ID: &str = "anthropic";
const API_ID: &str = "anthropic-messages";

/// First system block required on the OAuth lane: omitting it gets the request
/// rejected as not coming from the Claude Code client.
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

#[derive(Debug, Clone)]
pub(crate) struct AnthropicProvider {
    client: Client,
    model: String,
    base_url: String,
    reasoning: Option<ReasoningEffort>,
    system_prompt: String,
    tokens: AnthropicTokenStore,
}

impl AnthropicProvider {
    /// Build from the resolved model/base-url/reasoning selection (precedence is
    /// owned by `mimir::selection`). `system_prompt` is the harness-assembled
    /// instruction string; the provider prepends the required Claude Code
    /// identity block and forwards the rest.
    pub(crate) fn new(
        model: &str,
        base_url: &str,
        reasoning: Option<ReasoningEffort>,
        system_prompt: &str,
    ) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()?,
            model: model.to_string(),
            base_url: base_url.to_string(),
            reasoning,
            system_prompt: system_prompt.to_string(),
            tokens: AnthropicTokenStore::from_env()?,
        })
    }
}

impl ChatProvider for AnthropicProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a Tools,
        cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        // Build the request eagerly so nothing borrowed from `self`/`messages`/
        // `tools` is captured by the blocking task (only an owned `Value` is).
        let request = build_anthropic_request(
            &self.model,
            &self.system_prompt,
            messages,
            tools,
            self.reasoning,
        );
        let provider = self.clone();
        let cancel = cancel.clone();
        Ok(spawn_stream(
            move |sink, cancel| {
                run_with_reauth(
                    cancel,
                    |force| {
                        if force {
                            provider.tokens.force_refresh(&provider.client)
                        } else {
                            provider.tokens.access_token(&provider.client)
                        }
                    },
                    |token| provider.send_once(token, &request, sink, cancel),
                )
            },
            cancel,
        ))
    }
}

impl AnthropicProvider {
    fn send_once(
        &self,
        token: &str,
        request: &Value,
        sink: &mut dyn TurnSink,
        cancel: &CancellationToken,
    ) -> Attempt {
        let headers = match anthropic_headers(token) {
            Ok(headers) => headers,
            Err(error) => return Attempt::Fatal(error),
        };
        let url = format!("{}/v1/messages", self.base_url);
        let response = match self.client.post(&url).headers(headers).json(request).send() {
            Ok(response) => response,
            Err(error) => {
                return Attempt::Fatal(
                    anyhow::Error::new(error).context("failed to send Anthropic request"),
                );
            }
        };

        let status = response.status();
        if status.is_success() {
            let mut parser = AnthropicStreamParser::new(anthropic_origin(&self.model));
            if let Err(error) = for_each_sse_event(BufReader::new(response), cancel, |data| {
                parser.ingest_event(data, sink)
            }) {
                return Attempt::Fatal(error);
            }
            return match parser.finish() {
                Ok(turn) => Attempt::Done(turn),
                Err(error) => Attempt::Fatal(error),
            };
        }

        let body = response.text().unwrap_or_default();
        let error = match crate::telemetry::sanitize_external_body(&body) {
            Some(detail) => anyhow!("Anthropic request failed ({status}): {detail}"),
            None => anyhow!("Anthropic request failed ({status})"),
        };
        match classify_http_status(status.as_u16()) {
            HttpClass::Reauth => Attempt::Reauth(error),
            HttpClass::Fatal => Attempt::Fatal(error),
        }
    }
}

fn anthropic_headers(token: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(
        "anthropic-version",
        HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    headers.insert("anthropic-beta", HeaderValue::from_static(ANTHROPIC_BETA));
    headers.insert(
        "anthropic-dangerous-direct-browser-access",
        HeaderValue::from_static("true"),
    );
    headers.insert("x-app", HeaderValue::from_static("cli"));
    headers.insert(USER_AGENT, HeaderValue::from_static("iris-agent"));
    Ok(headers)
}

fn build_anthropic_request(
    model: &str,
    system_prompt: &str,
    messages: &[Message],
    tools: &Tools,
    reasoning: Option<ReasoningEffort>,
) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": MAX_TOKENS,
        "stream": true,
        "system": [
            { "type": "text", "text": CLAUDE_CODE_IDENTITY },
            { "type": "text", "text": system_prompt },
        ],
        "messages": build_messages(messages, &anthropic_origin(model)),
    });
    // Extended thinking is added only when a preference is set, so the default
    // (None) body is byte-identical to today's request (max_tokens unchanged, no
    // `thinking` field). Adaptive-thinking models take an effort level via
    // `output_config`; older models take a token budget that also raises
    // `max_tokens`.
    match anthropic_thinking(model, reasoning) {
        None => {}
        Some(ThinkingPlan::Adaptive { effort }) => {
            body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
            body["output_config"] = json!({ "effort": effort });
        }
        Some(ThinkingPlan::Budget { budget_tokens }) => {
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget_tokens });
            body["max_tokens"] = json!(MAX_TOKENS + budget_tokens);
        }
        Some(ThinkingPlan::Disabled) => {
            body["thinking"] = json!({ "type": "disabled" });
        }
    }
    let declarations = tool_declarations(tools);
    if !declarations.is_empty() {
        body["tools"] = Value::Array(declarations);
    }
    body
}

/// How a reasoning level is encoded for a given Anthropic model.
enum ThinkingPlan {
    /// Adaptive-thinking models (Opus 4.6/4.7/4.8, Sonnet 4.6): the model decides
    /// how much to think and the caller passes an effort level via
    /// `output_config.effort`. `max_tokens` is left at the base value.
    Adaptive { effort: &'static str },
    /// Older models: a fixed thinking token budget; `max_tokens` is raised to
    /// `base + budget` so `max_tokens > budget_tokens`.
    Budget { budget_tokens: u32 },
    /// Explicit `Off` on an adaptive model: disable thinking on the wire
    /// (`thinking: { type: "disabled" }`). Adaptive models can otherwise default
    /// thinking on, so omitting the field would not turn it off. `max_tokens`
    /// stays at the base value.
    Disabled,
}

/// Anthropic models that use adaptive thinking instead of budget-based thinking.
/// Tracks pi-mono `compat.forceAdaptiveThinking` (models.generated.ts); extend
/// as new adaptive models ship. Unknown/older ids fall back to budget-based.
fn uses_adaptive_thinking(model: &str) -> bool {
    matches!(
        model,
        "claude-sonnet-4-6" | "claude-opus-4-6" | "claude-opus-4-7" | "claude-opus-4-8"
    )
}

/// Map a normalized reasoning level to this model's thinking encoding. `None`
/// reasoning omits thinking entirely (today's byte-identical default). Verified
/// shapes (pi-mono `streamSimpleAnthropic` / `buildAnthropicParams`):
/// - adaptive: `thinking: { type: "adaptive", display: "summarized" }` +
///   `output_config: { effort }` (effort `low|medium|high|xhigh|max`).
/// - budget: `thinking: { type: "enabled", budget_tokens: N }` with
///   `max_tokens > budget_tokens` (`adjustMaxTokensForThinking`).
/// - explicit `Off`: `thinking: { type: "disabled" }` on adaptive models (which
///   can default thinking on); omitted on older models (absence already = off,
///   matching pi-mono's `thinkingLevelMap?.off !== null` guard).
///
/// Levels are capability-gated upstream (`model_capabilities::validate`/`clamp`),
/// so `xhigh` only reaches a model that accepts it.
//
// ponytail: the request shape is verified, but this OAuth/claude-code lane does
// not replay prior `thinking` blocks across tool turns, which Anthropic requires
// when thinking is enabled with tool use. Multi-turn thinking replay is a
// follow-up (the no-thinking default path is unaffected).
fn anthropic_thinking(model: &str, reasoning: Option<ReasoningEffort>) -> Option<ThinkingPlan> {
    let level = reasoning?;
    let adaptive = uses_adaptive_thinking(model);
    if level == ReasoningEffort::Off {
        // Explicit off: disable on adaptive models (which can default thinking
        // on); omit on older models where absence already means off.
        return adaptive.then_some(ThinkingPlan::Disabled);
    }
    if adaptive {
        let effort = match level {
            ReasoningEffort::Minimal | ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            // Opus 4.6 names its top effort "max"; 4.7/4.8 use "xhigh".
            ReasoningEffort::XHigh if model == "claude-opus-4-6" => "max",
            ReasoningEffort::XHigh => "xhigh",
            ReasoningEffort::Off => unreachable!("off returns above"),
        };
        return Some(ThinkingPlan::Adaptive { effort });
    }
    let budget_tokens: u32 = match level {
        ReasoningEffort::Minimal => 1024,
        ReasoningEffort::Low => 2048,
        ReasoningEffort::Medium => 8192,
        // xhigh shares the high budget on budget-based models.
        ReasoningEffort::High | ReasoningEffort::XHigh => 16384,
        ReasoningEffort::Off => unreachable!("off returns above"),
    };
    Some(ThinkingPlan::Budget { budget_tokens })
}

fn tool_declarations(tools: &Tools) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name(),
                "description": tool.description(),
                "input_schema": tool.parameters(),
            })
        })
        .collect()
}

/// Map Nexus messages onto Anthropic wire messages. The Messages API requires
/// strict user/assistant alternation, so every Nexus message becomes a content
/// block appended to the previous wire message when the role matches.
fn build_messages(messages: &[Message], current_origin: &ModelOrigin) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for message in messages {
        let mapped = match message.role {
            Role::User => Some(("user", json!({ "type": "text", "text": message.content }))),
            Role::Assistant => Some((
                "assistant",
                json!({ "type": "text", "text": message.content }),
            )),
            Role::AssistantReasoning => {
                reasoning_block(message, current_origin).map(|block| ("assistant", block))
            }
            Role::AssistantToolCall => Some((
                "assistant",
                json!({
                    "type": "tool_use",
                    "id": message.tool_call_id.as_deref().unwrap_or_default(),
                    "name": message.tool_name.as_deref().unwrap_or_default(),
                    "input": serde_json::from_str::<Value>(&message.content).unwrap_or_else(|_| json!({})),
                }),
            )),
            Role::Tool => Some((
                "user",
                json!({
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.as_deref().unwrap_or_default(),
                    "content": message.content,
                    "is_error": false,
                }),
            )),
        };
        if let Some((role, block)) = mapped {
            push_block(&mut out, role, block);
        }
    }
    out
}

fn anthropic_origin(model: &str) -> ModelOrigin {
    ModelOrigin::new(PROVIDER_ID, API_ID, model)
}

fn reasoning_block(message: &Message, current_origin: &ModelOrigin) -> Option<Value> {
    let same_origin = message.origin.as_ref() == Some(current_origin);
    if message.redacted {
        return same_origin.then(|| {
            message
                .continuity
                .as_ref()
                .map(|data| json!({ "type": "redacted_thinking", "data": data }))
        })?;
    }
    if same_origin && let Some(signature) = &message.continuity {
        return Some(json!({
            "type": "thinking",
            "thinking": message.content,
            "signature": signature,
        }));
    }
    (!message.content.is_empty()).then(|| json!({ "type": "text", "text": message.content }))
}

fn push_block(out: &mut Vec<Value>, role: &str, block: Value) {
    if let Some(last) = out.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.push(block);
        return;
    }
    out.push(json!({ "role": role, "content": [block] }));
}

/// Incremental SSE assembler. Text deltas accumulate into one buffer; each
/// `tool_use` content block is tracked by its stream index until its
/// `content_block_stop` finalizes it into a [`ToolCall`] (encounter order).
struct AnthropicStreamParser {
    origin: ModelOrigin,
    text: String,
    open_tools: HashMap<u64, ToolBlock>,
    tool_calls: Vec<ToolCall>,
    open_reasoning: HashMap<u64, ReasoningBlock>,
    reasoning: Vec<ReasoningBlock>,
    message_stopped: bool,
}

struct ToolBlock {
    id: String,
    name: String,
    partial_json: String,
    inline_input: Option<Value>,
}

impl AnthropicStreamParser {
    fn new(origin: ModelOrigin) -> Self {
        Self {
            origin,
            text: String::new(),
            open_tools: HashMap::new(),
            tool_calls: Vec::new(),
            open_reasoning: HashMap::new(),
            reasoning: Vec::new(),
            message_stopped: false,
        }
    }

    fn ingest_event(&mut self, data: &str, sink: &mut dyn TurnSink) -> Result<()> {
        if data == "[DONE]" {
            return Ok(());
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|e| anyhow!("failed to parse Anthropic SSE: {e}"))?;
        match value.get("type").and_then(Value::as_str) {
            Some("content_block_start") => {
                let index = block_index(&value);
                if let Some(block) = value.get("content_block") {
                    match block.get("type").and_then(Value::as_str) {
                        Some("thinking") => {
                            self.open_reasoning.insert(
                                index,
                                ReasoningBlock::new(
                                    &str_field(block, "thinking"),
                                    None,
                                    false,
                                    self.origin.clone(),
                                ),
                            );
                        }
                        Some("redacted_thinking") => {
                            let data = str_field(block, "data");
                            self.open_reasoning.insert(
                                index,
                                ReasoningBlock::new("", Some(&data), true, self.origin.clone()),
                            );
                        }
                        Some("tool_use") => {
                            let inline = block
                                .get("input")
                                .filter(
                                    |input| !matches!(input, Value::Object(map) if map.is_empty()),
                                )
                                .cloned();
                            self.open_tools.insert(
                                index,
                                ToolBlock {
                                    id: str_field(block, "id"),
                                    name: str_field(block, "name"),
                                    partial_json: String::new(),
                                    inline_input: inline,
                                },
                            );
                        }
                        _ => {}
                    }
                }
            }
            Some("content_block_delta") => {
                let index = block_index(&value);
                if let Some(delta) = value.get("delta") {
                    match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => {
                            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                self.text.push_str(text);
                                sink.on_text_delta(text)?;
                            }
                        }
                        Some("input_json_delta") => {
                            if let (Some(block), Some(partial)) = (
                                self.open_tools.get_mut(&index),
                                delta.get("partial_json").and_then(Value::as_str),
                            ) {
                                block.partial_json.push_str(partial);
                            }
                        }
                        Some("thinking_delta") => {
                            if let (Some(block), Some(thinking)) = (
                                self.open_reasoning.get_mut(&index),
                                delta.get("thinking").and_then(Value::as_str),
                            ) {
                                block.text.push_str(thinking);
                            }
                        }
                        Some("signature_delta") => {
                            if let (Some(block), Some(signature)) = (
                                self.open_reasoning.get_mut(&index),
                                delta.get("signature").and_then(Value::as_str),
                            ) {
                                block
                                    .continuity
                                    .get_or_insert_with(String::new)
                                    .push_str(signature);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some("content_block_stop") => {
                let index = block_index(&value);
                if let Some(block) = self.open_tools.remove(&index) {
                    self.tool_calls.push(finalize_tool(block)?);
                } else if let Some(block) = self.open_reasoning.remove(&index) {
                    self.reasoning.push(block);
                }
            }
            Some("message_stop") => {
                self.message_stopped = true;
            }
            Some("error") => {
                let message = value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Anthropic stream error");
                return Err(anyhow!("{message}"));
            }
            // message_start / message_delta carry no payload we assemble here
            // on the MVP lane.
            _ => {}
        }
        Ok(())
    }

    fn finish(self) -> Result<AssistantTurn> {
        if !self.message_stopped {
            return Err(anyhow!("Anthropic stream ended before message_stop"));
        }
        if !self.open_tools.is_empty() || !self.open_reasoning.is_empty() {
            return Err(anyhow!("Anthropic stream ended before content_block_stop"));
        }
        if self.text.is_empty() && self.tool_calls.is_empty() && self.reasoning.is_empty() {
            return Err(anyhow!(
                "Anthropic response did not include assistant text, reasoning, or tool calls"
            ));
        }
        Ok(AssistantTurn {
            text: (!self.text.is_empty()).then_some(self.text),
            reasoning: self.reasoning,
            tool_calls: self.tool_calls,
        })
    }
}

fn finalize_tool(block: ToolBlock) -> Result<ToolCall> {
    if block.id.is_empty() || block.name.is_empty() {
        return Err(anyhow!("Anthropic tool_use missing id or name"));
    }
    let arguments = match block.inline_input {
        Some(input) => input,
        None if block.partial_json.is_empty() => json!({}),
        None => serde_json::from_str(&block.partial_json)
            .context("Anthropic tool_use input JSON was incomplete or invalid")?,
    };
    Ok(ToolCall {
        id: block.id,
        name: block.name,
        arguments,
    })
}

fn block_index(value: &Value) -> u64 {
    value.get("index").and_then(Value::as_u64).unwrap_or(0)
}

fn str_field(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
fn parse_anthropic_sse(body: &str) -> Result<AssistantTurn> {
    parse_anthropic_sse_for_model(body, "m")
}

#[cfg(test)]
fn parse_anthropic_sse_for_model(body: &str, model: &str) -> Result<AssistantTurn> {
    struct NoopSink;
    impl TurnSink for NoopSink {
        fn on_text_delta(&mut self, _delta: &str) -> Result<()> {
            Ok(())
        }
    }
    let mut parser = AnthropicStreamParser::new(anthropic_origin(model));
    let mut sink = NoopSink;
    for_each_sse_event(body.as_bytes(), &CancellationToken::new(), |data| {
        parser.ingest_event(data, &mut sink)
    })?;
    parser.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::{Message, ModelOrigin, Tools};

    #[test]
    fn text_deltas_assemble_into_turn() {
        let body = "\
event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello \"}}

event: content_block_delta
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"world\"}}

event: message_stop
data: {\"type\":\"message_stop\"}

";
        let turn = parse_anthropic_sse(body).expect("stream should parse");
        assert_eq!(turn.text.as_deref(), Some("Hello world"));
        assert!(turn.tool_calls.is_empty());
    }

    #[test]
    fn tool_use_with_input_json_delta_parses_arguments() {
        let body = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.rs\\\"}\"}}

data: {\"type\":\"content_block_stop\",\"index\":0}

data: {\"type\":\"message_stop\"}

";
        let turn = parse_anthropic_sse(body).expect("stream should parse");
        assert_eq!(turn.tool_calls.len(), 1);
        let call = &turn.tool_calls[0];
        assert_eq!(call.id, "toolu_1");
        assert_eq!(call.name, "read");
        assert_eq!(call.arguments, json!({ "path": "a.rs" }));
    }

    #[test]
    fn missing_message_stop_is_error() {
        let body = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}

";
        let error = parse_anthropic_sse(body).unwrap_err().to_string();
        assert!(error.contains("message_stop"), "got: {error}");
    }

    #[test]
    fn incomplete_tool_json_is_error() {
        let body = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}

data: {\"type\":\"content_block_stop\",\"index\":0}

data: {\"type\":\"message_stop\"}

";
        let error = parse_anthropic_sse(body).unwrap_err().to_string();
        assert!(error.contains("input JSON"), "got: {error}");
    }

    #[test]
    fn error_event_is_error() {
        let body = "\
data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}

";
        let error = parse_anthropic_sse(body).unwrap_err().to_string();
        assert!(error.contains("overloaded"), "got: {error}");
    }

    #[test]
    fn request_has_identity_block_and_maps_tool_result() {
        let messages = vec![
            Message::user("hi"),
            Message {
                role: Role::Tool,
                content: "result body".to_string(),
                tool_call_id: Some("toolu_1".to_string()),
                tool_name: Some("read".to_string()),
                continuity: None,
                redacted: false,
                origin: None,
            },
        ];
        let request =
            build_anthropic_request("m", "IRIS PROMPT", &messages, &Tools::new(Vec::new()), None);

        let system = request["system"].as_array().expect("system is array");
        assert_eq!(system[0]["text"], json!(CLAUDE_CODE_IDENTITY));
        assert_eq!(system[1]["text"], json!("IRIS PROMPT"));

        let msgs = request["messages"].as_array().expect("messages array");
        let tool_msg = msgs.last().expect("tool result message");
        assert_eq!(tool_msg["role"], json!("user"));
        let blocks = tool_msg["content"].as_array().unwrap();
        let block = blocks
            .iter()
            .find(|block| block["type"] == json!("tool_result"))
            .expect("tool_result block");
        assert_eq!(block["type"], json!("tool_result"));
        assert_eq!(block["tool_use_id"], json!("toolu_1"));
        assert_eq!(block["content"], json!("result body"));
        assert_eq!(block["is_error"], json!(false));

        assert!(request.get("tools").is_none(), "empty tools omitted");
    }

    #[test]
    fn reasoning_none_keeps_todays_body_some_adds_thinking() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());

        // None: no `thinking`, max_tokens unchanged.
        let none = build_anthropic_request("m", "P", &messages, &tools, None);
        assert!(none.get("thinking").is_none(), "None omits thinking");
        assert_eq!(none["max_tokens"], json!(MAX_TOKENS));

        // High: budget-based extended thinking, max_tokens raised above budget.
        let high =
            build_anthropic_request("m", "P", &messages, &tools, Some(ReasoningEffort::High));
        assert_eq!(
            high["thinking"],
            json!({ "type": "enabled", "budget_tokens": 16384 })
        );
        assert_eq!(high["max_tokens"], json!(MAX_TOKENS + 16384));
        assert!(
            high["max_tokens"].as_u64().unwrap() > 16384,
            "max_tokens must exceed the thinking budget"
        );

        // Off omits thinking entirely (today's wire).
        let off = build_anthropic_request("m", "P", &messages, &tools, Some(ReasoningEffort::Off));
        assert!(off.get("thinking").is_none(), "Off omits thinking");
    }

    #[test]
    fn adaptive_models_use_effort_output_config_not_budget() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());

        // Sonnet 4.6 is adaptive: effort via output_config, adaptive thinking,
        // and max_tokens left at the base (no budget bump, no budget_tokens).
        let body = build_anthropic_request(
            "claude-sonnet-4-6",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::High),
        );
        assert_eq!(
            body["thinking"],
            json!({ "type": "adaptive", "display": "summarized" })
        );
        assert_eq!(body["output_config"], json!({ "effort": "high" }));
        assert_eq!(
            body["max_tokens"],
            json!(MAX_TOKENS),
            "adaptive keeps base max_tokens"
        );
        assert!(body["thinking"].get("budget_tokens").is_none());

        // Opus 4.8 accepts xhigh -> effort "xhigh".
        let opus = build_anthropic_request(
            "claude-opus-4-8",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::XHigh),
        );
        assert_eq!(opus["output_config"], json!({ "effort": "xhigh" }));
        assert_eq!(opus["thinking"]["type"], json!("adaptive"));

        // Opus 4.6 names its top effort "max".
        let opus6 = build_anthropic_request(
            "claude-opus-4-6",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::XHigh),
        );
        assert_eq!(opus6["output_config"], json!({ "effort": "max" }));

        // Adaptive model with no preference is still byte-identical to today.
        let none = build_anthropic_request("claude-sonnet-4-6", "P", &messages, &tools, None);
        assert!(none.get("thinking").is_none());
        assert!(none.get("output_config").is_none());
        assert_eq!(none["max_tokens"], json!(MAX_TOKENS));
    }

    #[test]
    fn explicit_off_disables_thinking_on_adaptive_models_only() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());

        // Adaptive model + explicit Off: disable on the wire, since omitting would
        // let the model default thinking on. No effort, base max_tokens.
        let adaptive_off = build_anthropic_request(
            "claude-sonnet-4-6",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::Off),
        );
        assert_eq!(adaptive_off["thinking"], json!({ "type": "disabled" }));
        assert!(adaptive_off.get("output_config").is_none());
        assert_eq!(adaptive_off["max_tokens"], json!(MAX_TOKENS));

        // None (no preference) stays byte-identical: no thinking field at all.
        let adaptive_none =
            build_anthropic_request("claude-sonnet-4-6", "P", &messages, &tools, None);
        assert!(adaptive_none.get("thinking").is_none());

        // Older/budget model + Off: omit thinking (absence already means off).
        let budget_off =
            build_anthropic_request("m", "P", &messages, &tools, Some(ReasoningEffort::Off));
        assert!(budget_off.get("thinking").is_none());
    }

    #[test]
    fn user_text_after_tool_result_coalesces_into_one_user_message() {
        let messages = vec![
            Message {
                role: Role::Tool,
                content: "result body".to_string(),
                tool_call_id: Some("toolu_1".to_string()),
                tool_name: Some("read".to_string()),
                continuity: None,
                redacted: false,
                origin: None,
            },
            Message::user("next prompt"),
        ];

        let msgs = build_messages(&messages, &anthropic_origin("m"));

        assert_eq!(msgs.len(), 1, "same-role user blocks coalesce");
        assert_eq!(msgs[0]["role"], json!("user"));
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], json!("tool_result"));
        assert_eq!(content[1], json!({ "type": "text", "text": "next prompt" }));
    }

    #[test]
    fn thinking_and_redacted_sse_blocks_capture_reasoning() {
        let body = "\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-6\"}}

data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"raw \"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" bytes\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-a\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-b\"}}

data: {\"type\":\"content_block_stop\",\"index\":0}

data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque-redacted\"}}

data: {\"type\":\"content_block_stop\",\"index\":1}

data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\"}}

data: {\"type\":\"content_block_stop\",\"index\":2}

data: {\"type\":\"message_stop\"}

";
        let turn =
            parse_anthropic_sse_for_model(body, "claude-sonnet-4-6").expect("stream should parse");

        assert_eq!(turn.reasoning.len(), 2);
        assert_eq!(turn.reasoning[0].text, "raw  bytes");
        assert_eq!(turn.reasoning[0].continuity.as_deref(), Some("sig-asig-b"));
        assert!(!turn.reasoning[0].redacted);
        assert_eq!(turn.reasoning[0].origin.model, "claude-sonnet-4-6");
        assert_eq!(turn.reasoning[1].text, "");
        assert_eq!(
            turn.reasoning[1].continuity.as_deref(),
            Some("opaque-redacted")
        );
        assert!(turn.reasoning[1].redacted);
        assert_eq!(turn.tool_calls.len(), 1);
    }

    #[test]
    fn reasoning_replay_is_same_origin_gated_and_byte_exact() {
        let same = ModelOrigin::new("anthropic", "anthropic-messages", "claude-sonnet-4-6");
        let other = ModelOrigin::new("anthropic", "anthropic-messages", "claude-opus-4-6");
        let messages = vec![
            Message::user("go"),
            // Empty visible thinking must still replay when signed.
            Message::assistant_reasoning("", "sig-empty", false, same.clone()),
            Message::assistant_reasoning(
                " foreign  thinking ",
                "sig-foreign",
                false,
                other.clone(),
            ),
            Message::assistant_reasoning("", "opaque-same", true, same),
            Message::assistant_reasoning("", "opaque-foreign", true, other),
            Message::assistant("answer"),
        ];

        let request = build_anthropic_request(
            "claude-sonnet-4-6",
            "P",
            &messages,
            &Tools::new(Vec::new()),
            None,
        );
        let assistant = &request["messages"].as_array().unwrap()[1];
        let blocks = assistant["content"].as_array().unwrap();

        assert_eq!(
            blocks[0],
            json!({ "type": "thinking", "thinking": "", "signature": "sig-empty" })
        );
        assert_eq!(
            blocks[1],
            json!({ "type": "text", "text": " foreign  thinking " })
        );
        assert_eq!(
            blocks[2],
            json!({ "type": "redacted_thinking", "data": "opaque-same" })
        );
        assert_eq!(blocks[3], json!({ "type": "text", "text": "answer" }));
        assert_eq!(blocks.len(), 4, "foreign redacted thinking is dropped");
    }

    #[test]
    fn assistant_text_and_tool_calls_coalesce_into_one_message() {
        // One model turn: text + two tool calls, then their two results. The
        // Messages API rejects consecutive same-role messages, so this must map
        // to exactly assistant{text,tool_use,tool_use} then user{result,result}.
        let messages = vec![
            Message::user("go"),
            Message::assistant("working"),
            Message {
                role: Role::AssistantToolCall,
                content: "{\"path\":\"a\"}".to_string(),
                tool_call_id: Some("toolu_1".to_string()),
                tool_name: Some("read".to_string()),
                continuity: None,
                redacted: false,
                origin: None,
            },
            Message {
                role: Role::AssistantToolCall,
                content: "{\"path\":\"b\"}".to_string(),
                tool_call_id: Some("toolu_2".to_string()),
                tool_name: Some("read".to_string()),
                continuity: None,
                redacted: false,
                origin: None,
            },
            Message {
                role: Role::Tool,
                content: "A".to_string(),
                tool_call_id: Some("toolu_1".to_string()),
                tool_name: Some("read".to_string()),
                continuity: None,
                redacted: false,
                origin: None,
            },
            Message {
                role: Role::Tool,
                content: "B".to_string(),
                tool_call_id: Some("toolu_2".to_string()),
                tool_name: Some("read".to_string()),
                continuity: None,
                redacted: false,
                origin: None,
            },
        ];
        let msgs = build_messages(&messages, &anthropic_origin("m"));
        let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(
            roles,
            vec!["user", "assistant", "user"],
            "strict alternation"
        );
        let assistant = &msgs[1]["content"];
        assert_eq!(assistant.as_array().unwrap().len(), 3, "text + 2 tool_use");
        assert_eq!(assistant[0]["type"], json!("text"));
        assert_eq!(assistant[1]["type"], json!("tool_use"));
        assert_eq!(assistant[1]["input"], json!({ "path": "a" }));
        assert_eq!(assistant[2]["id"], json!("toolu_2"));
        let results = msgs[2]["content"].as_array().unwrap();
        assert_eq!(results.len(), 2, "both tool results in one user message");
        assert_eq!(results[0]["tool_use_id"], json!("toolu_1"));
    }
}
