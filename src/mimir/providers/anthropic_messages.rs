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
use crate::mimir::anthropic_models::{self, ThinkingMode};
use crate::mimir::auth::anthropic::AnthropicTokenStore;
use crate::mimir::selection::ReasoningEffort;
use crate::nexus::{
    AssistantTurn, ChatProvider, Message, ModelOrigin, ProviderStream, ReasoningBlock, Role,
    ToolCall, Tools,
};

/// Base output-token allowance, treated as the visible-output ask
/// (`requested_output_tokens` in the manual-budget invariant). For manual-budget
/// thinking the budget is added on top and the sum is capped at the model's
/// output cap; for adaptive thinking the API allocates reasoning dynamically and
/// this stays the base `max_tokens`.
const MAX_TOKENS: u32 = 8192;
/// Default output cap for an unknown/non-subscription Anthropic id (conservative
/// 64k). Subscription models carry their real cap in `anthropic_models`.
const DEFAULT_OUTPUT_CAP: u32 = 64000;
/// Anthropic's extended-thinking floor: `budget_tokens` must be `>= 1024` and
/// `< max_tokens`, else the request 400s.
const ANTHROPIC_MIN_THINKING_BUDGET_TOKENS: u32 = 1024;
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Base betas every Claude Code OAuth request carries.
const BASE_ANTHROPIC_BETA: &str = "oauth-2025-04-20,claude-code-20250219";
/// Appended only for manual-budget thinking payloads (`thinking.type ==
/// "enabled"`); adaptive thinking implies interleaved thinking server-side and
/// no thinking does not need it.
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
/// Appended only when the payload carries a `fallbacks` array (Fable 5 refusal
/// fallback). The header date is authoritative as written; adopted from
/// minimalcc-pi `SERVER_SIDE_FALLBACK_BETA`.
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-06-01";
const PROVIDER_ID: &str = "anthropic";
const API_ID: &str = "anthropic-messages";
/// Endpoint path surfaced in failure diagnostics (never the full base URL).
const ENDPOINT_PATH: &str = "/v1/messages";

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
                // Remember the token we last handed out so a forced refresh
                // (after a 401) can tell the rejected token apart from one a
                // concurrent refresh already rotated in -- otherwise a coalesced
                // refresh could hand the rejected token straight back.
                let mut last_token: Option<String> = None;
                run_with_reauth(
                    cancel,
                    |force| {
                        let token = if force {
                            provider
                                .tokens
                                .force_refresh(&provider.client, last_token.as_deref())
                        } else {
                            provider.tokens.access_token(&provider.client)
                        }?;
                        last_token = Some(token.clone());
                        Ok(token)
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
        let headers = match anthropic_headers(token, request) {
            Ok(headers) => headers,
            Err(error) => return Attempt::Fatal(error),
        };
        // Read auth kind from the request headers we are about to send (the map
        // is moved into the request below).
        let auth_kind = auth_kind_label(&headers);
        let url = format!("{}{ENDPOINT_PATH}", self.base_url);
        let response = match self.client.post(&url).headers(headers).json(request).send() {
            Ok(response) => response,
            Err(error) => {
                return Attempt::Fatal(
                    anyhow::Error::new(error).context("failed to send Anthropic request"),
                );
            }
        };

        let status = response.status();
        // Request id comes off the response headers, before the body is read.
        let request_id = extract_request_id(response.headers());
        if status.is_success() {
            let mut parser = AnthropicStreamParser::new(anthropic_origin(&self.model));
            // Build a safe diagnostic tail from local state only -- never the
            // streamed body. `last_event_type` is whatever the parser last saw.
            let diag = |last_event_type: Option<String>| AnthropicDiagnostics {
                status: status.as_u16(),
                request_id: request_id.clone(),
                error_type: None,
                model: self.model.clone(),
                endpoint: ENDPOINT_PATH,
                auth_kind,
                last_event_type,
            };
            if let Err(error) = for_each_sse_event(BufReader::new(response), cancel, |data| {
                parser.ingest_event(data, sink)
            }) {
                let last = parser.last_event_type.clone();
                return Attempt::Fatal(anyhow!("{error} [{}]", diag(last)));
            }
            let last = parser.last_event_type.clone();
            return match parser.finish() {
                Ok(turn) => Attempt::Done(turn),
                Err(error) => Attempt::Fatal(anyhow!("{error} [{}]", diag(last))),
            };
        }

        // Non-success: surface only safe metadata. The raw body is read and
        // dropped; only the enumerated `error.type` is pulled out of it (never
        // the body text, which can carry prompts/paths/args).
        let body = response.text().unwrap_or_default();
        let diag = AnthropicDiagnostics {
            status: status.as_u16(),
            request_id,
            error_type: extract_error_type(&body),
            model: self.model.clone(),
            endpoint: ENDPOINT_PATH,
            auth_kind,
            last_event_type: None,
        };
        let error = anyhow!("Anthropic request failed [{diag}]");
        match classify_http_status(status.as_u16()) {
            HttpClass::Reauth => Attempt::Reauth(error),
            HttpClass::Fatal => Attempt::Fatal(error),
        }
    }
}

/// Safe, redacted diagnostics for an Anthropic request/stream failure. Every
/// field is local metadata that cannot carry a credential, prompt, tool
/// argument, file path, command string, raw request/response body, or SSE
/// frame. Adopted conceptually from minimalcc-pi's metadata-only stream
/// diagnostics, trimmed to the fields Iris can produce cheaply.
struct AnthropicDiagnostics {
    status: u16,
    request_id: Option<String>,
    /// Enumerated Anthropic error classification (e.g. `invalid_request_error`),
    /// never the free-text error message.
    error_type: Option<String>,
    model: String,
    endpoint: &'static str,
    auth_kind: &'static str,
    last_event_type: Option<String>,
}

/// Render the safe diagnostic tail as a stable space-separated `key=value`
/// string; absent optionals are skipped. Writes straight to the formatter (no
/// intermediate allocation) and lets call sites use `{diag}` in `anyhow!`.
impl std::fmt::Display for AnthropicDiagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "status={} endpoint={} model={} auth={}",
            self.status, self.endpoint, self.model, self.auth_kind
        )?;
        if let Some(id) = &self.request_id {
            write!(f, " request_id={id}")?;
        }
        if let Some(kind) = &self.error_type {
            write!(f, " error_type={kind}")?;
        }
        if let Some(event) = &self.last_event_type {
            write!(f, " last_event={event}")?;
        }
        Ok(())
    }
}

/// `oauth_bearer` when an `Authorization: Bearer ...` header is present, else
/// `none`. The OAuth lane always sends a bearer, but this reads the header so
/// the label tracks what was actually sent.
fn auth_kind_label(headers: &HeaderMap) -> &'static str {
    let is_bearer = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("Bearer "));
    if is_bearer { "oauth_bearer" } else { "none" }
}

/// Anthropic request id from response headers (`request-id`, falling back to
/// `anthropic-request-id`). Safe metadata: an opaque server-side correlation id.
fn extract_request_id(headers: &HeaderMap) -> Option<String> {
    ["request-id", "anthropic-request-id"]
        .iter()
        .find_map(|name| headers.get(*name))
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// Pull only the enumerated `error.type` out of an error body. Deliberately does
/// NOT use `telemetry::sanitize_external_body`, which would surface the whole
/// (key-redacted) body -- non-sensitive keys like `message` can still hold
/// prompts/paths/commands. Returns None for a non-JSON body or one without a
/// string error type.
fn extract_error_type(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    value
        .get("error")
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Build OAuth-lane Anthropic headers. The `anthropic-beta` set is driven by the
/// request payload shape (like minimalcc-pi `buildNativeMessagesRequest`): base
/// betas always, `interleaved-thinking` only for manual-budget thinking, and the
/// server-side fallback beta only when a `fallbacks` array is present. The
/// OAuth lane never sends `x-api-key` / `anthropic-api-key`.
fn anthropic_headers(token: &str, request: &Value) -> Result<HeaderMap> {
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
    headers.insert(
        "anthropic-beta",
        HeaderValue::from_str(&anthropic_beta(request))?,
    );
    headers.insert(
        "anthropic-dangerous-direct-browser-access",
        HeaderValue::from_static("true"),
    );
    headers.insert("x-app", HeaderValue::from_static("cli"));
    headers.insert(USER_AGENT, HeaderValue::from_static("iris-agent"));
    Ok(headers)
}

/// Build the `anthropic-beta` header value from the outgoing payload. Payload-
/// driven so header construction needs no model object: manual-budget thinking
/// is `thinking.type == "enabled"`, the refusal fallback is a non-empty
/// `fallbacks` array.
fn anthropic_beta(request: &Value) -> String {
    let mut betas = String::from(BASE_ANTHROPIC_BETA);
    let manual_thinking = request
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        == Some("enabled");
    if manual_thinking {
        betas.push(',');
        betas.push_str(INTERLEAVED_THINKING_BETA);
    }
    let has_fallbacks = request
        .get("fallbacks")
        .and_then(Value::as_array)
        .is_some_and(|fallbacks| !fallbacks.is_empty());
    if has_fallbacks {
        betas.push(',');
        betas.push_str(SERVER_SIDE_FALLBACK_BETA);
    }
    betas
}

fn build_anthropic_request(
    model: &str,
    system_prompt: &str,
    messages: &[Message],
    tools: &Tools,
    reasoning: Option<ReasoningEffort>,
) -> Value {
    let meta = anthropic_models::find(model);
    // Wire `model` uses the native id; the soft-cap alias `claude-opus-4-7-300k`
    // sends `claude-opus-4-7`. Everything else sends its own id.
    let native_id = meta.map(|m| m.native_id).unwrap_or(model);
    let thinking_mode = meta
        .map(|m| m.thinking)
        .unwrap_or(ThinkingMode::ManualBudget);
    let output_cap = meta.map(|m| m.output_cap).unwrap_or(DEFAULT_OUTPUT_CAP);

    let mut body = json!({
        "model": native_id,
        "max_tokens": MAX_TOKENS,
        "stream": true,
        "system": [
            { "type": "text", "text": CLAUDE_CODE_IDENTITY },
            { "type": "text", "text": system_prompt },
        ],
        // Origin (reasoning-replay continuity) is keyed on the UI model id, not
        // the native id, so a 300k-alias turn replays against the same selection.
        "messages": build_messages(messages, &anthropic_origin(model)),
    });

    // Thinking is added only when a level is set and is not `off`. Both `None`
    // (no preference) and explicit `Off` omit `thinking` entirely: minimalcc-pi
    // never sends `thinking: { type: "disabled" }` (it 400s on Fable 5), so
    // absence is the off signal. The default (None) body stays byte-identical to
    // today's request.
    if let Some(level) = reasoning.filter(|level| *level != ReasoningEffort::Off) {
        match thinking_mode {
            ThinkingMode::Adaptive => {
                body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
                body["output_config"] = json!({ "effort": adaptive_effort(level) });
            }
            ThinkingMode::ManualBudget => {
                let (max_tokens, budget_tokens) =
                    resolve_manual_thinking(MAX_TOKENS, manual_budget(level), output_cap);
                body["max_tokens"] = json!(max_tokens);
                if let Some(budget_tokens) = budget_tokens {
                    body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget_tokens });
                }
            }
        }
    }

    // Fable 5 refusal fallback: ask the API to retry a safety-classifier decline
    // on the fallback model server-side (one round trip). The matching
    // `server-side-fallback` beta is added from this payload in `anthropic_beta`.
    if let Some(fallback) = meta.and_then(|m| m.refusal_fallback) {
        body["fallbacks"] = json!([{ "model": fallback }]);
    }

    let declarations = tool_declarations(tools);
    if !declarations.is_empty() {
        body["tools"] = Value::Array(declarations);
    }
    body
}

/// Manual-budget thinking token budget for an iris reasoning level. Adopted
/// verbatim from minimalcc-pi `DEFAULT_THINKING_BUDGETS`. `Off` yields 0 (no
/// thinking); it is never reached because callers filter `Off` out first.
fn manual_budget(level: ReasoningEffort) -> u32 {
    match level {
        ReasoningEffort::Off => 0,
        ReasoningEffort::Minimal => 1024,
        ReasoningEffort::Low => 4096,
        ReasoningEffort::Medium => 10240,
        ReasoningEffort::High => 20480,
        ReasoningEffort::XHigh => 32768,
    }
}

/// Map an iris reasoning level one notch up Anthropic's `low|medium|high|xhigh|
/// max` effort scale for adaptive models, so iris `xhigh` reaches Anthropic's
/// top `max` and iris `minimal` reaches its lowest non-off `low`. Adopted from
/// minimalcc-pi `CLAUDE_SUBSCRIPTION_ADAPTIVE_OPUS_THINKING_LEVEL_MAP`.
fn adaptive_effort(level: ReasoningEffort) -> &'static str {
    match level {
        ReasoningEffort::Off => "low",
        ReasoningEffort::Minimal => "low",
        ReasoningEffort::Low => "medium",
        ReasoningEffort::Medium => "high",
        ReasoningEffort::High => "xhigh",
        ReasoningEffort::XHigh => "max",
    }
}

/// Resolve `(max_tokens, budget_tokens)` for manual-budget thinking under
/// Anthropic's invariants, adopted from minimalcc-pi `resolveManualThinkingPayload`:
/// - Anthropic `max_tokens` covers thinking + visible output and never exceeds
///   the model's `output_cap`;
/// - `budget_tokens` must satisfy `1024 <= budget_tokens < max_tokens`.
///
/// `requested_output` is the visible-output ask. `max_tokens` expands to cover
/// the thinking budget on top of that ask, capped at `output_cap`. When the cap
/// forces an otherwise-invalid payload the budget is reduced toward the 1024
/// floor; if no valid budget fits, `budget_tokens` is `None` (omit thinking).
fn resolve_manual_thinking(
    requested_output: u32,
    budget: u32,
    output_cap: u32,
) -> (u32, Option<u32>) {
    let clamped_output = requested_output.min(output_cap);
    if budget == 0 {
        return (clamped_output, None);
    }
    let max_tokens = clamped_output.saturating_add(budget).min(output_cap);
    if budget < max_tokens {
        return (max_tokens, Some(budget));
    }
    // The cap forced max_tokens <= budget. Reduce thinking so budget < max_tokens,
    // preserving as much output room as possible; omit thinking if none fits.
    let reduced = max_tokens.saturating_sub(clamped_output);
    if reduced >= ANTHROPIC_MIN_THINKING_BUDGET_TOKENS {
        (max_tokens, Some(reduced))
    } else {
        (clamped_output, None)
    }
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
    /// Type of the most recent SSE event seen, for safe failure diagnostics.
    last_event_type: Option<String>,
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
            last_event_type: None,
        }
    }

    fn ingest_event(&mut self, data: &str, sink: &mut dyn TurnSink) -> Result<()> {
        if data == "[DONE]" {
            return Ok(());
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|e| anyhow!("failed to parse Anthropic SSE: {e}"))?;
        let event_type = value.get("type").and_then(Value::as_str);
        if let Some(event_type) = event_type {
            self.last_event_type = Some(event_type.to_string());
        }
        match event_type {
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
                // Surface only the enumerated error type, never the free-text
                // `message` (which is part of the response body).
                let error_type = value
                    .get("error")
                    .and_then(|error| error.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                return Err(anyhow!("Anthropic stream error (error_type={error_type})"));
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
    fn error_event_reports_type_without_leaking_message() {
        // The free-text message holds prompt/path/command-shaped material; only
        // the enumerated error type may surface.
        let body = "\
data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"queue depth 9001 for prompt about /home/u/secret.rs running rm -rf /\"}}

";
        let error = parse_anthropic_sse(body).unwrap_err().to_string();
        assert!(error.contains("overloaded_error"), "type kept: {error}");
        for leak in ["queue depth", "/home/u/secret.rs", "rm -rf", "prompt about"] {
            assert!(!error.contains(leak), "leaked {leak}: {error}");
        }
    }

    #[test]
    fn diagnostics_display_emits_safe_metadata_only() {
        let diag = AnthropicDiagnostics {
            status: 403,
            request_id: Some("req_fake_123".to_string()),
            error_type: Some("authentication_error".to_string()),
            model: "claude-opus-4-8".to_string(),
            endpoint: ENDPOINT_PATH,
            auth_kind: "oauth_bearer",
            last_event_type: Some("message_start".to_string()),
        };
        let rendered = diag.to_string();
        assert!(rendered.contains("status=403"));
        assert!(rendered.contains("endpoint=/v1/messages"));
        assert!(rendered.contains("model=claude-opus-4-8"));
        assert!(rendered.contains("auth=oauth_bearer"));
        assert!(rendered.contains("request_id=req_fake_123"));
        assert!(rendered.contains("error_type=authentication_error"));
        assert!(rendered.contains("last_event=message_start"));
    }

    #[test]
    fn http_failure_diagnostics_drop_raw_body_and_every_secret() {
        // A hostile error body packed with multiple fake sensitive strings:
        // prompt text, a file path, a command, an access token, a refresh token,
        // and tool arguments. None may reach the rendered diagnostic.
        let body = r#"{
            "error": {"type":"invalid_request_error","message":"prompt was SECRET_PROMPT_TEXT about /home/u/secret.rs running rm -rf /"},
            "access_token":"sk-fake-LEAKTOKEN-123",
            "refresh_token":"refresh-fake-LEAK-456",
            "tool_args":{"path":"/home/u/secret.rs","command":"rm -rf /"}
        }"#;
        // Only the enumerated error type is pulled from the body.
        assert_eq!(
            extract_error_type(body).as_deref(),
            Some("invalid_request_error")
        );
        let diag = AnthropicDiagnostics {
            status: 400,
            request_id: Some("req_fake".to_string()),
            error_type: extract_error_type(body),
            model: "claude-sonnet-4-6".to_string(),
            endpoint: ENDPOINT_PATH,
            auth_kind: "oauth_bearer",
            last_event_type: None,
        };
        let rendered = diag.to_string();
        for leak in [
            "SECRET_PROMPT_TEXT",
            "/home/u/secret.rs",
            "rm -rf",
            "sk-fake-LEAKTOKEN-123",
            "refresh-fake-LEAK-456",
            "prompt was",
            "tool_args",
        ] {
            assert!(!rendered.contains(leak), "leaked {leak}: {rendered}");
        }
        // ...while the safe metadata survives.
        assert!(rendered.contains("status=400"));
        assert!(rendered.contains("error_type=invalid_request_error"));
        assert!(rendered.contains("model=claude-sonnet-4-6"));
    }

    #[test]
    fn extract_error_type_ignores_non_json_and_typeless_bodies() {
        assert_eq!(extract_error_type("plain text with sk-token123"), None);
        assert_eq!(extract_error_type(r#"{"error":{"message":"x"}}"#), None);
        assert_eq!(
            extract_error_type(r#"{"error":{"type":"rate_limit_error"}}"#).as_deref(),
            Some("rate_limit_error")
        );
    }

    #[test]
    fn extract_request_id_prefers_request_id_then_falls_back() {
        let mut headers = HeaderMap::new();
        assert!(extract_request_id(&headers).is_none());
        headers.insert(
            "anthropic-request-id",
            HeaderValue::from_static("anthropic-fallback"),
        );
        assert_eq!(
            extract_request_id(&headers).as_deref(),
            Some("anthropic-fallback")
        );
        headers.insert("request-id", HeaderValue::from_static("req_primary"));
        assert_eq!(extract_request_id(&headers).as_deref(), Some("req_primary"));
    }

    #[test]
    fn auth_kind_reflects_bearer_header() {
        let messages = [Message::user("hi")];
        let request = build_anthropic_request(
            "claude-opus-4-8",
            "P",
            &messages,
            &Tools::new(Vec::new()),
            None,
        );
        let headers = anthropic_headers("fake-oauth-token", &request).unwrap();
        assert_eq!(auth_kind_label(&headers), "oauth_bearer");
        assert_eq!(auth_kind_label(&HeaderMap::new()), "none");
    }

    #[test]
    fn stream_error_event_surfaces_last_event_type_in_diagnostics() {
        // Drive a real error frame through the parser, then confirm the diagnostic
        // tail built from parser state names the error event and no payload.
        let mut parser = AnthropicStreamParser::new(anthropic_origin("claude-opus-4-8"));
        struct NoopSink;
        impl TurnSink for NoopSink {
            fn on_text_delta(&mut self, _delta: &str) -> Result<()> {
                Ok(())
            }
        }
        let mut sink = NoopSink;
        let err = parser
            .ingest_event(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"/secret/path leak"}}"#,
                &mut sink,
            )
            .unwrap_err();
        let diag = AnthropicDiagnostics {
            status: 200,
            request_id: None,
            error_type: None,
            model: "claude-opus-4-8".to_string(),
            endpoint: ENDPOINT_PATH,
            auth_kind: "oauth_bearer",
            last_event_type: parser.last_event_type.clone(),
        };
        let wrapped = anyhow!("{err} [{diag}]").to_string();
        assert!(wrapped.contains("last_event=error"), "got: {wrapped}");
        assert!(wrapped.contains("error_type=overloaded_error"));
        assert!(!wrapped.contains("/secret/path"), "got: {wrapped}");
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
    fn manual_budget_model_thinking_uses_minimalcc_budgets_and_invariant() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());

        // Sonnet 4.6 is a manual-budget model (cap 64k). No reasoning -> no
        // thinking, base max_tokens (byte-identical default).
        let none = build_anthropic_request("claude-sonnet-4-6", "P", &messages, &tools, None);
        assert!(none.get("thinking").is_none(), "None omits thinking");
        assert!(none.get("output_config").is_none());
        assert_eq!(none["max_tokens"], json!(MAX_TOKENS));

        // Explicit Off also omits thinking (no `disabled` block, matching
        // minimalcc-pi).
        let off = build_anthropic_request(
            "claude-sonnet-4-6",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::Off),
        );
        assert!(off.get("thinking").is_none(), "Off omits thinking");
        assert_eq!(off["max_tokens"], json!(MAX_TOKENS));

        // High -> 20480 budget; max_tokens = min(8192 + 20480, 64000) = 28672.
        let high = build_anthropic_request(
            "claude-sonnet-4-6",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::High),
        );
        assert_eq!(
            high["thinking"],
            json!({ "type": "enabled", "budget_tokens": 20480 })
        );
        assert_eq!(high["max_tokens"], json!(MAX_TOKENS + 20480));
        assert!(
            high["thinking"]["budget_tokens"].as_u64().unwrap()
                < high["max_tokens"].as_u64().unwrap(),
            "budget_tokens must stay below max_tokens"
        );
        assert!(
            high.get("output_config").is_none(),
            "manual model has no effort"
        );

        // xhigh -> 32768 budget; max_tokens = 8192 + 32768 = 40960 (< 64k cap).
        let xhigh = build_anthropic_request(
            "claude-sonnet-4-6",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::XHigh),
        );
        assert_eq!(
            xhigh["thinking"],
            json!({ "type": "enabled", "budget_tokens": 32768 })
        );
        assert_eq!(xhigh["max_tokens"], json!(MAX_TOKENS + 32768));

        // The full minimalcc-pi budget map on a manual model.
        for (level, budget) in [
            (ReasoningEffort::Minimal, 1024u32),
            (ReasoningEffort::Low, 4096),
            (ReasoningEffort::Medium, 10240),
            (ReasoningEffort::High, 20480),
            (ReasoningEffort::XHigh, 32768),
        ] {
            let body =
                build_anthropic_request("claude-opus-4-6", "P", &messages, &tools, Some(level));
            assert_eq!(
                body["thinking"],
                json!({ "type": "enabled", "budget_tokens": budget }),
                "{level:?} -> {budget}"
            );
            assert!(body.get("output_config").is_none());
        }
    }

    #[test]
    fn adaptive_models_use_effort_output_config_not_budget() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());

        // Opus 4.8 is adaptive: effort via output_config, adaptive thinking, and
        // max_tokens left at the base (no budget bump, no budget_tokens).
        let body = build_anthropic_request(
            "claude-opus-4-8",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::High),
        );
        assert_eq!(
            body["thinking"],
            json!({ "type": "adaptive", "display": "summarized" })
        );
        assert_eq!(body["output_config"], json!({ "effort": "xhigh" }));
        assert_eq!(
            body["max_tokens"],
            json!(MAX_TOKENS),
            "adaptive keeps base max_tokens"
        );
        assert!(body["thinking"].get("budget_tokens").is_none());

        // The full iris -> Anthropic upshift on an adaptive model: each iris level
        // lands one notch up the low|medium|high|xhigh|max effort scale.
        for (level, expected) in [
            (ReasoningEffort::Minimal, "low"),
            (ReasoningEffort::Low, "medium"),
            (ReasoningEffort::Medium, "high"),
            (ReasoningEffort::High, "xhigh"),
            (ReasoningEffort::XHigh, "max"),
        ] {
            let req =
                build_anthropic_request("claude-opus-4-7", "P", &messages, &tools, Some(level));
            assert_eq!(
                req["output_config"],
                json!({ "effort": expected }),
                "{level:?} -> {expected}"
            );
            assert_eq!(req["thinking"]["type"], json!("adaptive"));
        }

        // Adaptive model with no preference / explicit Off both omit thinking.
        let none = build_anthropic_request("claude-opus-4-8", "P", &messages, &tools, None);
        assert!(none.get("thinking").is_none());
        assert!(none.get("output_config").is_none());
        assert_eq!(none["max_tokens"], json!(MAX_TOKENS));
        let off = build_anthropic_request(
            "claude-opus-4-8",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::Off),
        );
        assert!(off.get("thinking").is_none(), "adaptive Off omits thinking");
        assert!(off.get("output_config").is_none());
    }

    #[test]
    fn opus_4_7_300k_sends_native_opus_4_7() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());
        let body = build_anthropic_request(
            "claude-opus-4-7-300k",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::Medium),
        );
        assert_eq!(
            body["model"],
            json!("claude-opus-4-7"),
            "300k soft-cap alias sends the native opus-4-7 id"
        );
        // Still adaptive, like the base opus-4-7.
        assert_eq!(body["thinking"]["type"], json!("adaptive"));
        assert_eq!(body["output_config"], json!({ "effort": "high" }));
    }

    #[test]
    fn fable_5_adds_server_side_fallback_payload_and_other_models_do_not() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());

        // Fable 5 is adaptive and carries the Opus 4.8 refusal fallback.
        let fable = build_anthropic_request(
            "claude-fable-5",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::High),
        );
        assert_eq!(fable["model"], json!("claude-fable-5"));
        assert_eq!(fable["thinking"]["type"], json!("adaptive"));
        assert_eq!(fable["output_config"], json!({ "effort": "xhigh" }));
        assert_eq!(fable["fallbacks"], json!([{ "model": "claude-opus-4-8" }]));

        // Fallback travels even when reasoning is off (Fable is always-on
        // server-side; thinking is omitted, fallbacks stay).
        let fable_off = build_anthropic_request(
            "claude-fable-5",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::Off),
        );
        assert!(fable_off.get("thinking").is_none());
        assert_eq!(
            fable_off["fallbacks"],
            json!([{ "model": "claude-opus-4-8" }])
        );

        // No other model emits a fallbacks parameter.
        let opus = build_anthropic_request(
            "claude-opus-4-8",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::High),
        );
        assert!(opus.get("fallbacks").is_none());
    }

    #[test]
    fn resolve_manual_thinking_enforces_the_invariant() {
        // Happy path: budget fits below the cap-bounded max_tokens.
        assert_eq!(
            resolve_manual_thinking(8192, 20480, 64000),
            (28672, Some(20480))
        );
        // Zero budget -> no thinking, max_tokens is the clamped output ask.
        assert_eq!(resolve_manual_thinking(8192, 0, 64000), (8192, None));
        // Cap forces a reduced (still valid) budget: requested 1000, cap 3000,
        // budget 4096 -> max_tokens 3000, budget reduced to 2000 (< 3000).
        let (max_tokens, budget) = resolve_manual_thinking(1000, 4096, 3000);
        assert_eq!((max_tokens, budget), (3000, Some(2000)));
        assert!(budget.unwrap() < max_tokens, "reduced budget stays valid");
        assert!(
            budget.unwrap() >= ANTHROPIC_MIN_THINKING_BUDGET_TOKENS,
            "reduced budget honors the 1024 floor"
        );
        // Cap leaves no room for a valid budget (would be < 1024): omit thinking
        // and revert max_tokens to the clamped output ask.
        assert_eq!(resolve_manual_thinking(500, 4096, 1200), (500, None));
        // A production manual model never trips the reduce/omit path: even xhigh
        // (32768) on the 64k cap leaves budget < max_tokens.
        let (max_tokens, budget) = resolve_manual_thinking(MAX_TOKENS, 32768, 64000);
        assert!(budget.unwrap() < max_tokens);
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

    #[test]
    fn headers_carry_oauth_betas_and_never_an_api_key() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());
        let body = build_anthropic_request("claude-opus-4-8", "P", &messages, &tools, None);
        let headers = anthropic_headers("fake-oauth-token", &body).expect("headers");

        assert_eq!(
            headers.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer fake-oauth-token"
        );
        assert_eq!(
            headers.get("anthropic-version").unwrap().to_str().unwrap(),
            ANTHROPIC_VERSION
        );
        // OAuth lane: no API-key headers, ever.
        assert!(headers.get("x-api-key").is_none());
        assert!(headers.get("anthropic-api-key").is_none());
    }

    #[test]
    fn interleaved_thinking_beta_is_present_only_for_manual_budget_thinking() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());
        let beta_of = |body: &Value| anthropic_beta(body);

        // Manual-budget thinking (thinking.type == "enabled") -> interleaved beta.
        let manual = build_anthropic_request(
            "claude-sonnet-4-6",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::High),
        );
        let manual_beta = beta_of(&manual);
        assert!(manual_beta.contains(BASE_ANTHROPIC_BETA));
        assert!(
            manual_beta.contains(INTERLEAVED_THINKING_BETA),
            "manual thinking needs the interleaved beta: {manual_beta}"
        );
        assert!(!manual_beta.contains(SERVER_SIDE_FALLBACK_BETA));

        // Adaptive thinking implies interleaved server-side -> beta omitted.
        let adaptive = build_anthropic_request(
            "claude-opus-4-8",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::High),
        );
        assert!(!beta_of(&adaptive).contains(INTERLEAVED_THINKING_BETA));

        // No thinking -> base betas only.
        let plain = build_anthropic_request("claude-opus-4-8", "P", &messages, &tools, None);
        assert_eq!(beta_of(&plain), BASE_ANTHROPIC_BETA);
    }

    #[test]
    fn server_side_fallback_beta_is_present_only_for_fable_5() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());

        let fable = build_anthropic_request(
            "claude-fable-5",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::High),
        );
        let fable_beta = anthropic_beta(&fable);
        assert!(
            fable_beta.contains(SERVER_SIDE_FALLBACK_BETA),
            "{fable_beta}"
        );
        // Fable is adaptive: no interleaved beta.
        assert!(!fable_beta.contains(INTERLEAVED_THINKING_BETA));

        let opus = build_anthropic_request(
            "claude-opus-4-8",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::High),
        );
        assert!(!anthropic_beta(&opus).contains(SERVER_SIDE_FALLBACK_BETA));
    }
}
