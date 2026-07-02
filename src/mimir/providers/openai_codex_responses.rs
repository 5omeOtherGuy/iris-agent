#[cfg(test)]
use std::io::BufRead;
use std::io::BufReader;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::transport::{
    Attempt, HttpClass, StreamReadError, TurnSink, classify_http_status_retryable,
    for_each_sse_event, retry_after_hint, run_with_retry, spawn_stream,
};
use crate::mimir::auth::openai_codex::{AccessToken, OpenAiCodexTokenStore};
use crate::mimir::selection::{PromptCacheRetention, ReasoningEffort};
use crate::nexus::{
    AssistantTurn, ChatProvider, Message, ModelOrigin, ProviderStream, ProviderUsage,
    ReasoningBlock, Role, ToolCall, Tools,
};

// Transport resilience for Codex requests. Transient failures (network, 429,
// 5xx) are retried with exponential backoff plus jitter; a single auth
// rejection (401/403) triggers one forced token refresh before retrying. The
// retry budget and backoff shape come from the shared
// [`RetryPolicy`](crate::mimir::retry::RetryPolicy), the single definition for
// every provider adapter.
const PROVIDER_ID: &str = "openai-codex";
const API_ID: &str = "openai-codex-responses";
const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

#[derive(Debug, Clone)]
pub(crate) struct OpenAiCodexResponsesProvider {
    client: Client,
    model: String,
    base_url: String,
    reasoning: Option<ReasoningEffort>,
    system_prompt: String,
    prompt_cache_key: String,
    cache_retention: PromptCacheRetention,
    cache_prefix: Arc<Mutex<super::PromptCachePrefix>>,
    tokens: OpenAiCodexTokenStore,
    retry_policy: crate::mimir::retry::RetryPolicy,
}

impl OpenAiCodexResponsesProvider {
    /// Build the provider from the resolved model/base-url/reasoning selection.
    /// Selection precedence (`IRIS_MODEL`/`IRIS_CODEX_BASE_URL` -> settings ->
    /// default) now lives in `mimir::selection`, so the adapter just receives the
    /// resolved strings plus the optional reasoning level. `system_prompt` is the
    /// harness-assembled instruction string; the provider only forwards it into
    /// the request envelope.
    pub(crate) fn new(
        model: &str,
        base_url: &str,
        reasoning: Option<ReasoningEffort>,
        system_prompt: &str,
        prompt_cache_key: &str,
        cache_retention: PromptCacheRetention,
        retry_policy: crate::mimir::retry::RetryPolicy,
    ) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()?,
            model: model.to_string(),
            base_url: base_url.to_string(),
            reasoning,
            system_prompt: system_prompt.to_string(),
            prompt_cache_key: prompt_cache_key.to_string(),
            cache_retention,
            cache_prefix: Arc::new(Mutex::new(super::PromptCachePrefix::default())),
            tokens: OpenAiCodexTokenStore::from_env()?,
            retry_policy,
        })
    }
}

impl ChatProvider for OpenAiCodexResponsesProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a Tools,
        cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        // Build the request eagerly so setup errors (e.g. a bad base URL) surface
        // synchronously and nothing borrowed from `self`/`messages`/`tools` is
        // captured by the blocking task.
        // Prompt-break diagnostic: warn only when Iris can prove the cached
        // prefix changed since the previous turn (compaction/history edit or a
        // changed instruction/tool head), never on a cold-cache or first turn.
        if super::PromptCachePrefix::observe_locked(
            &self.cache_prefix,
            self.cache_retention.caching_enabled(),
            &self.system_prompt,
            tools,
            messages,
        ) {
            tracing::warn!(
                provider = PROVIDER_ID,
                model = %self.model,
                "prompt cache prefix changed since the previous request; the cached prefix will not be reused this turn"
            );
        }
        let request = build_codex_request(
            &self.model,
            &self.system_prompt,
            messages,
            tools,
            self.reasoning,
            Some(&self.prompt_cache_key),
            // store:false, so Iris never supplies previous_response_id in prod.
            None,
            self.cache_retention,
        );
        let url = resolve_codex_url(&self.base_url)?;
        let provider = self.clone();
        let cancel = cancel.clone();
        Ok(spawn_stream(
            move |sink, cancel| provider.run_blocking(url, &request, sink, cancel),
            cancel,
        ))
    }
}

impl OpenAiCodexResponsesProvider {
    /// Drive the blocking retry/reauth state machine and SSE parse on a
    /// `spawn_blocking` thread, forwarding text deltas through `sink` and
    /// returning the assembled turn.
    fn run_blocking(
        &self,
        url: Url,
        request: &Value,
        sink: &mut dyn TurnSink,
        cancel: &CancellationToken,
    ) -> Result<AssistantTurn> {
        let span = tracing::info_span!("codex_roundtrip", model = %self.model);
        let _guard = span.enter();

        run_with_retry(
            PROVIDER_ID,
            &self.retry_policy,
            cancel,
            |force_refresh| {
                if force_refresh {
                    self.tokens.force_refresh(&self.client)
                } else {
                    self.tokens.access_token(&self.client)
                }
            },
            |token| self.send_once(url.clone(), token, request, sink, cancel),
        )
    }
}

impl OpenAiCodexResponsesProvider {
    fn send_once(
        &self,
        url: Url,
        token: &AccessToken,
        request: &Value,
        sink: &mut dyn TurnSink,
        cancel: &CancellationToken,
    ) -> Attempt {
        let headers = match codex_headers(token) {
            Ok(headers) => headers,
            Err(error) => return Attempt::Fatal(error),
        };
        let response = match self.client.post(url).headers(headers).json(request).send() {
            Ok(response) => response,
            Err(error) => {
                return Attempt::Retry(
                    anyhow::Error::new(error).context("failed to send Codex request"),
                    None,
                );
            }
        };

        let status = response.status();
        if status.is_success() {
            let mut parser = ResponseStreamParser::new(&self.model);
            if let Err(error) = for_each_sse_event(BufReader::new(response), cancel, |data| {
                parser.ingest_event(data, sink)
            }) {
                if !cancel.is_cancelled()
                    && protocol_anomaly_retryable(&error, parser.emitted_visible_text)
                {
                    return Attempt::Retry(error, None);
                }
                return Attempt::Fatal(error);
            }
            let emitted_visible_text = parser.emitted_visible_text;
            return match parser.finish() {
                Ok(turn) => {
                    if let Some(usage) = &turn.usage {
                        self.record_usage(usage);
                    }
                    Attempt::Done(Box::new(turn))
                }
                Err(error) => {
                    if protocol_anomaly_retryable(&error, emitted_visible_text) {
                        Attempt::Retry(error, None)
                    } else {
                        Attempt::Fatal(error)
                    }
                }
            };
        }

        let retry_after = retry_after_hint(response.headers());
        let body = response.text().unwrap_or_default();
        let diag = CodexDiagnostics {
            status: status.as_u16(),
            error_type: extract_error_field(&body, "type"),
            error_code: extract_error_field(&body, "code"),
            model: self.model.clone(),
            endpoint: "/codex/responses",
            last_event_type: None,
        };
        let error = anyhow!("Codex request failed [{diag}]");
        match classify_http_status_retryable(status.as_u16()) {
            HttpClass::Reauth => Attempt::Reauth(error),
            HttpClass::Retry => Attempt::Retry(error, retry_after),
            HttpClass::Fatal => Attempt::Fatal(error),
        }
    }

    fn record_usage(&self, usage: &ProviderUsage) {
        // Surface usage and the two distinct cache facts the diagnostics must
        // separate: whether Iris SENT a cacheable request (cache setting enabled)
        // vs whether the provider REPORTED a cache hit (cache_read > 0).
        tracing::info!(
            provider = %usage.provider,
            model = %usage.model,
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            cache_read_input_tokens = usage.cache_read_input_tokens,
            cache_write_input_tokens = usage.cache_write_input_tokens,
            reasoning_output_tokens = usage.reasoning_output_tokens,
            total_tokens = usage.total_tokens,
            cacheable_request_sent = self.cache_retention.caching_enabled(),
            cache_hit = usage.cache_read_input_tokens > 0,
            "provider token usage"
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn build_codex_request(
    model: &str,
    instructions: &str,
    messages: &[Message],
    tools: &Tools,
    reasoning: Option<ReasoningEffort>,
    prompt_cache_key: Option<&str>,
    previous_response_id: Option<&str>,
    cache_retention: PromptCacheRetention,
) -> Value {
    // The Codex adapter owns conversion between Nexus messages and Responses wire JSON.
    let origin = openai_origin(model);
    let input: Vec<Value> = messages
        .iter()
        .filter_map(|message| codex_input_item(message, &origin))
        .collect();

    let mut body = json!({
        "model": model,
        "store": false,
        "stream": true,
        "instructions": instructions,
        "input": input,
        "tools": tool_declarations(tools),
        "text": { "verbosity": "low" },
    });
    if cache_retention.caching_enabled() {
        if let Some(key) = prompt_cache_key.and_then(clamp_openai_prompt_cache_key) {
            body["prompt_cache_key"] = json!(key);
        }
        // Long retention opts into the 24h prompt-cache lifetime (pi-mono
        // `getPromptCacheRetention`); short/none leave the default in-memory
        // (~minutes) lifetime, so no field is sent.
        if cache_retention == PromptCacheRetention::Long {
            body["prompt_cache_retention"] = json!("24h");
        }
    }
    // `previous_response_id` requires server-side response storage (store:true).
    // Iris sends store:false, so production never supplies one; the field is
    // emitted only when a caller explicitly provides it (documented shape), so a
    // stored-response deployment can opt in later without changing the builder.
    if let Some(previous) = previous_response_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        body["previous_response_id"] = json!(previous);
    }
    // Reasoning is inserted only when a preference is set. Encrypted reasoning
    // is requested whenever reasoning is active or same-origin continuity is
    // replayed, matching Responses' include contract without adopting Codex's
    // websocket/session machinery.
    if let Some(reasoning) = codex_reasoning(reasoning) {
        body["reasoning"] = reasoning;
        body["include"] = json!(["reasoning.encrypted_content"]);
    } else if input
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("reasoning"))
    {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    body
}

/// Map a normalized reasoning level to the Codex Responses `reasoning` object,
/// or `None` to omit it. Verified shape: `reasoning.effort` accepts
/// `minimal..xhigh` (pi-mono `openai-codex-responses.ts`,
/// `openai-responses.ts`). `Off` maps to omitted because gpt-5.5 cannot disable
/// reasoning (`thinkingLevelMap.off == null`), so there is no disable field to
/// send.
fn codex_reasoning(reasoning: Option<ReasoningEffort>) -> Option<Value> {
    let effort = match reasoning? {
        ReasoningEffort::Off => return None,
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::XHigh => "xhigh",
    };
    Some(json!({ "effort": effort }))
}

/// Build the Codex `tools` declaration array from the injected tool set: one
/// `{type, name, description, parameters}` entry per tool, in declaration order.
/// Mirrors how pi builds provider declarations from `tool.name/description/
/// parameters` (see anthropic.ts / amazon-bedrock.ts).
fn tool_declarations(tools: &Tools) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name(),
                "description": tool.description(),
                "parameters": tool.parameters(),
            })
        })
        .collect()
}

fn codex_input_item(message: &Message, current_origin: &ModelOrigin) -> Option<Value> {
    let item = match message.role {
        Role::User | Role::Assistant => json!({
            "type": "message",
            "role": message.role.as_str(),
            "content": [{ "type": message_content_type(message.role), "text": message.content }],
        }),
        Role::AssistantToolCall => json!({
            "type": "function_call",
            "call_id": message.tool_call_id.as_deref().unwrap_or_default(),
            "name": message.tool_name.as_deref().unwrap_or_default(),
            "arguments": message.content,
        }),
        Role::Tool => json!({
            "type": "function_call_output",
            "call_id": message.tool_call_id.as_deref().unwrap_or_default(),
            "output": message.content,
        }),
        Role::AssistantReasoning => {
            if message.origin.as_ref() != Some(current_origin) {
                return None;
            }
            let encrypted = message.continuity.as_deref()?.trim();
            if encrypted.is_empty() {
                return None;
            }
            json!({
                "type": "reasoning",
                "encrypted_content": encrypted,
                "summary": [],
            })
        }
    };
    Some(item)
}

fn clamp_openai_prompt_cache_key(key: &str) -> Option<String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(
        trimmed
            .chars()
            .take(OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH)
            .collect(),
    )
}

fn openai_origin(model: &str) -> ModelOrigin {
    ModelOrigin::new(PROVIDER_ID, API_ID, model)
}

fn message_content_type(role: Role) -> &'static str {
    match role {
        Role::User => "input_text",
        Role::Assistant => "output_text",
        Role::AssistantReasoning | Role::AssistantToolCall | Role::Tool => {
            unreachable!("non-text messages are not text messages")
        }
    }
}

fn codex_headers(token: &AccessToken) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", token.bearer))?,
    );
    headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_str(&token.account_id)?,
    );
    headers.insert("originator", HeaderValue::from_static("iris"));
    headers.insert(USER_AGENT, HeaderValue::from_static("iris-agent"));
    headers.insert(
        "OpenAI-Beta",
        HeaderValue::from_static("responses=experimental"),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

fn resolve_codex_url(base_url: &str) -> Result<Url> {
    let mut url =
        Url::parse(base_url).with_context(|| format!("invalid Codex base URL: {base_url}"))?;
    let path = url.path().trim_end_matches('/');
    let next_path = if path.ends_with("/codex/responses") {
        path.to_string()
    } else if path.ends_with("/codex") {
        format!("{path}/responses")
    } else if path.is_empty() {
        "/codex/responses".to_string()
    } else {
        format!("{path}/codex/responses")
    };
    url.set_path(&next_path);
    Ok(url)
}

#[cfg(test)]
fn parse_response_json(value: Value) -> Result<AssistantTurn> {
    let turn = extract_assistant_turn(&value, "gpt-test");
    if turn.text.as_deref().unwrap_or_default().is_empty() && turn.tool_calls.is_empty() {
        bail!("Codex response did not include assistant text or tool calls");
    }
    Ok(turn)
}

#[cfg(test)]
fn parse_response_stream(body: &str) -> Result<AssistantTurn> {
    let mut sink = NoopSink;
    parse_response_stream_reader(
        BufReader::new(body.as_bytes()),
        &mut sink,
        &CancellationToken::new(),
        "gpt-test",
    )
}

#[cfg(test)]
fn parse_response_stream_reader(
    reader: impl BufRead,
    sink: &mut dyn TurnSink,
    cancel: &CancellationToken,
    model: &str,
) -> Result<AssistantTurn> {
    let mut parser = ResponseStreamParser::new(model);
    for_each_sse_event(reader, cancel, |data| parser.ingest_event(data, sink))?;
    parser.finish()
}

struct ResponseStreamParser {
    origin: ModelOrigin,
    text: String,
    reasoning: Vec<ReasoningBlock>,
    tool_calls: Vec<ToolCall>,
    completed_response: Option<Value>,
    response_id: Option<String>,
    saw_completed: bool,
    emitted_visible_text: bool,
    last_event_type: Option<String>,
}

impl ResponseStreamParser {
    fn new(model: &str) -> Self {
        Self {
            origin: openai_origin(model),
            text: String::new(),
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            completed_response: None,
            response_id: None,
            saw_completed: false,
            emitted_visible_text: false,
            last_event_type: None,
        }
    }

    fn ingest_event(&mut self, event: &str, sink: &mut dyn TurnSink) -> Result<()> {
        if event.is_empty() || event == "[DONE]" {
            return Ok(());
        }

        let value: Value = serde_json::from_str(event).map_err(|_| {
            anyhow::Error::new(CodexStreamProtocolAnomaly::invalid_json(
                self.last_event_type.clone(),
            ))
        })?;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string);
        self.last_event_type = event_type.clone();
        match event_type.as_deref() {
            Some("response.output_text.delta") => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.text.push_str(delta);
                    sink.on_text_delta(delta)?;
                    self.emitted_visible_text = true;
                }
            }
            Some("response.created") => {
                self.response_id = value
                    .get("response")
                    .and_then(|response| response.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            Some("response.output_item.done") => {
                if let Some(item) = value.get("item") {
                    if self.text.is_empty() {
                        self.text.push_str(&extract_output_text(item));
                    }
                    if let Some(block) = extract_reasoning_block(item, &self.origin) {
                        self.reasoning.push(block);
                    }
                    if let Some(call) = extract_tool_call(item) {
                        self.tool_calls.push(call);
                    }
                }
            }
            Some("response.completed") => {
                self.saw_completed = true;
                self.completed_response = value.get("response").cloned();
                if let Some(id) = self
                    .completed_response
                    .as_ref()
                    .and_then(|response| response.get("id"))
                    .and_then(Value::as_str)
                {
                    self.response_id = Some(id.to_string());
                }
            }
            Some("response.failed") => bail!("Codex response failed: {}", response_error(&value)),
            Some("response.incomplete") => {
                bail!("Codex response incomplete: {}", incomplete_reason(&value))
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(mut self) -> Result<AssistantTurn> {
        if !self.saw_completed {
            return Err(CodexStreamProtocolAnomaly::closed_before_completed(
                self.last_event_type.clone(),
            )
            .into());
        }
        let mut usage = None;
        if let Some(response) = self.completed_response.as_ref() {
            let completed_turn = extract_assistant_turn(response, &self.origin.model);
            if self.text.is_empty() {
                self.text
                    .push_str(completed_turn.text.as_deref().unwrap_or_default());
            }
            if self.reasoning.is_empty() {
                self.reasoning = completed_turn.reasoning;
            }
            if self.tool_calls.is_empty() {
                self.tool_calls = completed_turn.tool_calls;
            }
            if self.response_id.is_none() {
                self.response_id = completed_turn.response_id;
            }
            usage = completed_turn.usage;
        }
        if self.text.is_empty() && self.tool_calls.is_empty() {
            bail!("Codex response did not include assistant text or tool calls");
        }
        Ok(AssistantTurn {
            text: (!self.text.is_empty()).then_some(self.text),
            reasoning: self.reasoning,
            tool_calls: self.tool_calls,
            response_id: self.response_id,
            usage,
            completion_reason: None,
        })
    }
}

#[cfg(test)]
struct NoopSink;

#[cfg(test)]
impl TurnSink for NoopSink {
    fn on_text_delta(&mut self, _delta: &str) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct CodexDiagnostics {
    status: u16,
    error_type: Option<String>,
    error_code: Option<String>,
    model: String,
    endpoint: &'static str,
    last_event_type: Option<String>,
}

impl std::fmt::Display for CodexDiagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "status={} endpoint={} model={}",
            self.status, self.endpoint, self.model
        )?;
        if let Some(kind) = self.error_type.as_deref().and_then(safe_error_field) {
            write!(f, " error_type={kind}")?;
        }
        if let Some(code) = self.error_code.as_deref().and_then(safe_error_field) {
            write!(f, " error_code={code}")?;
        }
        if let Some(event) = self.last_event_type.as_deref().and_then(safe_error_field) {
            write!(f, " last_event={event}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct CodexStreamProtocolAnomaly {
    kind: &'static str,
    last_event_type: Option<String>,
}

impl CodexStreamProtocolAnomaly {
    fn invalid_json(last_event_type: Option<String>) -> Self {
        Self {
            kind: "invalid_json",
            last_event_type,
        }
    }

    fn closed_before_completed(last_event_type: Option<String>) -> Self {
        Self {
            kind: "closed_before_response_completed",
            last_event_type,
        }
    }
}

impl std::fmt::Display for CodexStreamProtocolAnomaly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Codex stream protocol anomaly kind={}", self.kind)?;
        if let Some(event) = self.last_event_type.as_deref().and_then(safe_error_field) {
            write!(f, " last_event={event}")?;
        }
        Ok(())
    }
}

impl std::error::Error for CodexStreamProtocolAnomaly {}

fn protocol_anomaly_retryable(error: &anyhow::Error, emitted_visible_text: bool) -> bool {
    !emitted_visible_text
        && (error.downcast_ref::<CodexStreamProtocolAnomaly>().is_some()
            || error.downcast_ref::<StreamReadError>().is_some())
}

fn extract_error_field(body: &str, field: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    value
        .get("error")
        .or_else(|| {
            value
                .get("response")
                .and_then(|response| response.get("error"))
        })
        .and_then(|error| error.get(field))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn safe_error_field(value: &str) -> Option<&str> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    let sensitive = ["secret", "token", "password", "api_key", "prompt", "sk-"];
    (!value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        && !sensitive.iter().any(|fragment| lower.contains(fragment)))
    .then_some(value)
}

fn response_error(value: &Value) -> String {
    let error = value
        .get("response")
        .and_then(|response| response.get("error"));
    let mut fields = Vec::new();
    if let Some(kind) = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .and_then(safe_error_field)
    {
        fields.push(format!("type={kind}"));
    }
    if let Some(code) = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .and_then(safe_error_field)
    {
        fields.push(format!("code={code}"));
    }
    if fields.is_empty() {
        "response.failed event received".to_string()
    } else {
        fields.join(" ")
    }
}

fn incomplete_reason(value: &Value) -> String {
    value
        .get("response")
        .and_then(|response| response.get("incomplete_details"))
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn extract_assistant_turn(value: &Value, model: &str) -> AssistantTurn {
    let origin = openai_origin(model);
    let text = extract_output_text(value);
    let reasoning = extract_reasoning_blocks(value, &origin);
    let tool_calls = extract_tool_calls(value);
    AssistantTurn {
        text: (!text.is_empty()).then_some(text),
        reasoning,
        tool_calls,
        response_id: value.get("id").and_then(Value::as_str).map(str::to_string),
        usage: extract_openai_usage(value, model),
        completion_reason: None,
    }
}

fn extract_reasoning_blocks(value: &Value, origin: &ModelOrigin) -> Vec<ReasoningBlock> {
    let mut blocks = Vec::new();
    if let Some(block) = extract_reasoning_block(value, origin) {
        blocks.push(block);
    }
    if let Some(items) = value.get("output").and_then(Value::as_array) {
        blocks.extend(
            items
                .iter()
                .filter_map(|item| extract_reasoning_block(item, origin)),
        );
    }
    blocks
}

fn extract_reasoning_block(value: &Value, origin: &ModelOrigin) -> Option<ReasoningBlock> {
    (value.get("type").and_then(Value::as_str) == Some("reasoning")).then(|| {
        let text = extract_reasoning_text(value);
        let encrypted = value
            .get("encrypted_content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty());
        // OpenAI `encrypted_content` is opaque continuity for replay, not a
        // redaction marker. When the provider also sends a summary/content block,
        // surface that text; when it sends encrypted-only reasoning, Nexus stores
        // the continuity row but emits no TUI reasoning block because the text is
        // empty and `redacted` is false.
        ReasoningBlock::new(&text, encrypted, false, origin.clone())
    })
}

fn extract_reasoning_text(value: &Value) -> String {
    let mut groups = Vec::new();
    for key in ["summary", "content"] {
        let mut group = String::new();
        if let Some(parts) = value.get(key).and_then(Value::as_array) {
            for part in parts {
                if let Some(part_text) = part.get("text").and_then(Value::as_str) {
                    group.push_str(part_text);
                }
            }
        }
        if !group.is_empty() {
            groups.push(group);
        }
    }
    groups.join("\n")
}

fn extract_openai_usage(value: &Value, model: &str) -> Option<ProviderUsage> {
    let usage = value.get("usage")?;
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read_input_tokens = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_output_tokens = usage
        .get("output_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
    Some(ProviderUsage {
        provider: PROVIDER_ID.to_string(),
        model: model.to_string(),
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens: 0,
        reasoning_output_tokens,
        total_tokens,
        // OpenAI Responses does not break cache creation down by tier.
        cache_creation: None,
    })
}

fn extract_tool_calls(value: &Value) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    if let Some(call) = extract_tool_call(value) {
        calls.push(call);
    }
    if let Some(items) = value.get("output").and_then(Value::as_array) {
        calls.extend(items.iter().filter_map(extract_tool_call));
    }
    calls
}

fn extract_tool_call(value: &Value) -> Option<ToolCall> {
    (value.get("type").and_then(Value::as_str) == Some("function_call")).then(|| ToolCall {
        thought_signature: None,
        id: value
            .get("call_id")
            .or_else(|| value.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        arguments: value
            .get("arguments")
            .and_then(parse_arguments)
            .unwrap_or_else(|| json!({})),
    })
}

fn parse_arguments(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => serde_json::from_str(text).ok(),
        object @ Value::Object(_) => Some(object.clone()),
        _ => None,
    }
}

fn extract_output_text(value: &Value) -> String {
    if let Some(text) = value.get("output_text").and_then(Value::as_str) {
        return text.to_string();
    }

    let mut output = String::new();
    if let Some(items) = value.get("output").and_then(Value::as_array) {
        for item in items {
            output.push_str(&extract_output_text(item));
        }
    }
    if let Some(content) = value.get("content").and_then(Value::as_array) {
        for part in content {
            let part_type = part.get("type").and_then(Value::as_str);
            if matches!(part_type, Some("output_text" | "text"))
                && let Some(text) = part.get("text").and_then(Value::as_str)
            {
                output.push_str(text);
            }
        }
    }
    output
}

#[cfg(test)]
#[path = "openai_codex_responses_tests.rs"]
mod tests;
