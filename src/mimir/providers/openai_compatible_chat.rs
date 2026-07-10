use std::io::BufReader;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::errors::AuthError;
use crate::mimir::providers::transport::{
    Attempt, StreamReadError, TurnSink, classify_http_status_retryable, for_each_sse_event,
    retry_after_hint, run_with_retry, spawn_stream,
};
use crate::mimir::retry::RetryPolicy;
use crate::mimir::selection::{PromptCacheRetention, ProviderId, ReasoningEffort};
use crate::nexus::{
    AssistantTurn, ChatProvider, CompletionReason, Message, ModelOrigin, ProviderStream,
    ProviderUsage, ReasoningBlock, Role, ToolCall, Tools,
};

/// API id recorded on reasoning-block origins for this adapter.
const API_ID: &str = "chat-completions";

#[derive(Clone, Copy)]
struct ChatPromptCache<'a> {
    key: Option<&'a str>,
    retention: PromptCacheRetention,
}

#[derive(Clone)]
pub(crate) struct OpenAiCompatibleChatConfig<'a> {
    pub(crate) provider: ProviderId,
    pub(crate) model: &'a str,
    pub(crate) base_url: &'a str,
    pub(crate) reasoning: Option<ReasoningEffort>,
    pub(crate) system_prompt: &'a str,
    pub(crate) api_key: Option<String>,
    pub(crate) supports_reasoning: bool,
    pub(crate) api_key_required: bool,
    pub(crate) prompt_cache_key: Option<&'a str>,
    pub(crate) cache_retention: PromptCacheRetention,
    pub(crate) retry_policy: RetryPolicy,
}

impl std::fmt::Debug for OpenAiCompatibleChatConfig<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatibleChatConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("reasoning", &self.reasoning)
            .field("system_prompt", &self.system_prompt)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("supports_reasoning", &self.supports_reasoning)
            .field("api_key_required", &self.api_key_required)
            .field("prompt_cache_key", &self.prompt_cache_key)
            .field("cache_retention", &self.cache_retention)
            .field("retry_policy", &self.retry_policy)
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct OpenAiCompatibleChatProvider {
    client: Client,
    provider: ProviderId,
    model: String,
    base_url: String,
    reasoning: Option<ReasoningEffort>,
    system_prompt: String,
    api_key: Option<String>,
    supports_reasoning: bool,
    prompt_cache_key: Option<String>,
    cache_retention: PromptCacheRetention,
    cache_prefix: Arc<Mutex<super::PromptCachePrefix>>,
    retry_policy: RetryPolicy,
}

impl std::fmt::Debug for OpenAiCompatibleChatProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatibleChatProvider")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("reasoning", &self.reasoning)
            .field("system_prompt", &self.system_prompt)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("supports_reasoning", &self.supports_reasoning)
            .field("prompt_cache_key", &self.prompt_cache_key)
            .field("cache_retention", &self.cache_retention)
            .field("retry_policy", &self.retry_policy)
            .finish()
    }
}

impl OpenAiCompatibleChatProvider {
    pub(crate) fn new(config: OpenAiCompatibleChatConfig<'_>) -> Result<Self> {
        if config.api_key_required
            && config
                .api_key
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
        {
            return Err(AuthError::for_provider(
                config.provider.as_str(),
                format!("{} API key is required", config.provider.display_name()),
            )
            .into());
        }
        Ok(Self {
            // Shared process-wide client: warm pooled connections (HTTP/2 +
            // keep-alive) survive across turns and model switches, so a turn
            // does not pay a fresh TLS handshake after an idle gap.
            client: crate::mimir::providers::transport::shared_client(),
            provider: config.provider,
            model: config.model.to_string(),
            base_url: config.base_url.to_string(),
            reasoning: config.reasoning,
            system_prompt: config.system_prompt.to_string(),
            api_key: config.api_key.filter(|key| !key.trim().is_empty()),
            supports_reasoning: config.supports_reasoning,
            prompt_cache_key: config.prompt_cache_key.map(str::to_string),
            cache_retention: config.cache_retention,
            cache_prefix: Arc::new(Mutex::new(super::PromptCachePrefix::default())),
            retry_policy: config.retry_policy,
        })
    }
}

impl ChatProvider for OpenAiCompatibleChatProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a Tools,
        cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        if super::PromptCachePrefix::observe_locked(
            &self.cache_prefix,
            self.cache_retention.caching_enabled(),
            &self.system_prompt,
            tools,
            messages,
        ) {
            tracing::warn!(
                provider = self.provider.as_str(),
                model = %self.model,
                "prompt cache prefix changed since the previous request; the cached prefix will not be reused this turn"
            );
        }
        let request = build_chat_request(
            &self.model,
            &self.system_prompt,
            messages,
            tools,
            self.reasoning,
            self.supports_reasoning,
            ChatPromptCache {
                key: self.prompt_cache_key.as_deref(),
                retention: self.cache_retention,
            },
        );
        let url = resolve_chat_url(&self.base_url)?;
        let provider = self.clone();
        let cancel = cancel.clone();
        Ok(spawn_stream(
            move |sink, cancel| {
                run_with_retry(
                    provider.provider.as_str(),
                    &provider.retry_policy,
                    cancel,
                    |_| Ok(()),
                    |_| provider.send_once(url.clone(), &request, sink, cancel),
                )
            },
            cancel,
        ))
    }
}

impl OpenAiCompatibleChatProvider {
    fn send_once(
        &self,
        url: Url,
        request: &Value,
        sink: &mut dyn TurnSink,
        cancel: &CancellationToken,
    ) -> Attempt {
        if cancel.is_cancelled() {
            return Attempt::Fatal(anyhow!("OpenAI-compatible request cancelled"));
        }
        let headers = match chat_headers(self.api_key.as_deref()) {
            Ok(headers) => headers,
            Err(error) => return Attempt::Fatal(error),
        };
        let response = match self.client.post(url).headers(headers).json(request).send() {
            Ok(response) => response,
            Err(error) => {
                if cancel.is_cancelled() {
                    return Attempt::Fatal(anyhow!("OpenAI-compatible request cancelled"));
                }
                // A pre-stream send failure (DNS/TLS/connect/timeout) emitted
                // no output yet: retry with backoff.
                return Attempt::Retry(
                    anyhow!("failed to send OpenAI-compatible request: {error}"),
                    None,
                );
            }
        };
        let status = response.status();
        if status.is_success() {
            let mut parser = ChatStreamParser::new(self.provider.as_str(), &self.model);
            if let Err(error) = for_each_sse_event(BufReader::new(response), cancel, |data| {
                sink.on_activity()?;
                parser.ingest_event(data, sink)
            }) {
                // A mid-stream read failure is retryable only when this attempt
                // streamed no visible text, so a retry cannot duplicate output
                // the user already saw.
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
        if matches!(status.as_u16(), 401 | 403) {
            return Attempt::Fatal(
                AuthError::for_provider(
                    self.provider.as_str(),
                    format!(
                        "{} key was rejected ({status})",
                        self.provider.display_name()
                    ),
                )
                .into(),
            );
        }
        let error = super::classified_http_error(
            status.as_u16(),
            &body,
            format!("OpenAI-compatible request failed ({status})"),
        );
        match classify_http_status_retryable(status.as_u16()) {
            crate::mimir::providers::transport::HttpClass::Retry => {
                Attempt::Retry(error, retry_after)
            }
            _ => Attempt::Fatal(error),
        }
    }

    fn record_usage(&self, usage: &ProviderUsage) {
        tracing::info!(
            provider = %usage.provider,
            model = %usage.model,
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            cache_read_input_tokens = usage.cache_read_input_tokens,
            reasoning_output_tokens = usage.reasoning_output_tokens,
            total_tokens = usage.total_tokens,
            "provider token usage"
        );
    }
}

/// A status-200 chat-completions stream that ended in a structurally invalid
/// state: an unparsable SSE frame, or the socket closed before the terminal
/// `[DONE]` sentinel / `finish_reason`. Recoverable: the transport may retry
/// the whole turn when no visible text has streamed. Carries no streamed
/// content -- only the anomaly kind.
#[derive(Debug, Clone)]
struct ChatStreamProtocolAnomaly {
    kind: &'static str,
}

impl ChatStreamProtocolAnomaly {
    fn invalid_json() -> Self {
        Self {
            kind: "invalid_json",
        }
    }

    fn closed_before_done() -> Self {
        Self {
            kind: "closed_before_done",
        }
    }
}

impl std::fmt::Display for ChatStreamProtocolAnomaly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "OpenAI-compatible stream protocol anomaly kind={}",
            self.kind
        )
    }
}

impl std::error::Error for ChatStreamProtocolAnomaly {}

/// Whether a stream failure may be retried: only protocol anomalies and local
/// read errors, and only when the attempt streamed no visible text (a retry
/// must never duplicate already-shown output).
fn protocol_anomaly_retryable(error: &anyhow::Error, emitted_visible_text: bool) -> bool {
    !emitted_visible_text
        && (error.downcast_ref::<ChatStreamProtocolAnomaly>().is_some()
            || error.downcast_ref::<StreamReadError>().is_some())
}

/// Incremental Chat Completions SSE assembler. Text deltas are forwarded to
/// the sink as they arrive (live rendering); reasoning and tool-call argument
/// fragments are buffered and only surface on the terminal [`AssistantTurn`].
struct ChatStreamParser {
    provider: String,
    model: String,
    text: String,
    reasoning: String,
    tool_calls: Vec<StreamingToolCall>,
    response_id: Option<String>,
    usage: Option<ProviderUsage>,
    completion_reason: Option<CompletionReason>,
    /// Whether a non-empty content delta was forwarded to the sink this
    /// attempt; gates whether a malformed stream can be retried without
    /// duplicating user-visible output.
    emitted_visible_text: bool,
    saw_done: bool,
}

#[derive(Default)]
struct StreamingToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl ChatStreamParser {
    fn new(provider: &str, model: &str) -> Self {
        Self {
            provider: provider.to_string(),
            model: model.to_string(),
            text: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            response_id: None,
            usage: None,
            completion_reason: None,
            emitted_visible_text: false,
            saw_done: false,
        }
    }

    fn ingest_event(&mut self, data: &str, sink: &mut dyn TurnSink) -> Result<()> {
        if data == "[DONE]" {
            self.saw_done = true;
            return Ok(());
        }
        // Drop the serde error so no streamed bytes can reach logs through
        // this path; the fixed anomaly kind is sufficient for diagnostics.
        let value: Value = serde_json::from_str(data)
            .map_err(|_| anyhow::Error::new(ChatStreamProtocolAnomaly::invalid_json()))?;
        if self.response_id.is_none() {
            self.response_id = value.get("id").and_then(Value::as_str).map(str::to_string);
        }
        // With `stream_options.include_usage` the final chunk carries usage
        // (typically with an empty `choices` array).
        if value.get("usage").is_some_and(|usage| !usage.is_null()) {
            self.usage = parse_usage(&value, &self.provider, &self.model);
        }
        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return Ok(());
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str)
                && !text.is_empty()
            {
                self.text.push_str(text);
                sink.on_text_delta(text)?;
                self.emitted_visible_text = true;
            }
            if let Some(thinking) = delta.get("reasoning_content").and_then(Value::as_str) {
                self.reasoning.push_str(thinking);
            }
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    self.ingest_tool_call_delta(call);
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.completion_reason = Some(map_finish_reason(reason));
        }
        Ok(())
    }

    /// Accumulate one `delta.tool_calls[]` fragment. The stream identifies a
    /// call by its `index`; `id`/`name` arrive on the first fragment and the
    /// JSON `arguments` string streams across subsequent fragments.
    fn ingest_tool_call_delta(&mut self, call: &Value) {
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .map(|index| index as usize)
            .unwrap_or_else(|| self.tool_calls.len().saturating_sub(1));
        while self.tool_calls.len() <= index {
            self.tool_calls.push(StreamingToolCall::default());
        }
        let entry = &mut self.tool_calls[index];
        if let Some(id) = call.get("id").and_then(Value::as_str)
            && entry.id.is_empty()
        {
            entry.id.push_str(id);
        }
        if let Some(function) = call.get("function") {
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                entry.name.push_str(name);
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                entry.arguments.push_str(arguments);
            }
        }
    }

    fn finish(self) -> Result<AssistantTurn> {
        // A stream that ended without the `[DONE]` sentinel or a terminal
        // finish_reason was cut off: return the typed anomaly so the transport
        // can retry when nothing visible streamed.
        if !self.saw_done && self.completion_reason.is_none() {
            return Err(ChatStreamProtocolAnomaly::closed_before_done().into());
        }
        let tool_calls = self
            .tool_calls
            .into_iter()
            .map(finalize_stream_tool_call)
            .collect::<Result<Vec<_>>>()?;
        let reasoning: Vec<ReasoningBlock> = (!self.reasoning.is_empty())
            .then(|| {
                ReasoningBlock::new(
                    &self.reasoning,
                    None,
                    false,
                    ModelOrigin::new(&self.provider, API_ID, &self.model),
                )
            })
            .into_iter()
            .collect();
        if self.text.is_empty()
            && tool_calls.is_empty()
            && reasoning.is_empty()
            && self.completion_reason.is_none()
        {
            bail!("OpenAI-compatible response did not include assistant text or tool calls");
        }
        Ok(AssistantTurn {
            text: (!self.text.is_empty()).then_some(self.text),
            reasoning,
            tool_calls,
            response_id: self.response_id,
            usage: self.usage,
            completion_reason: self.completion_reason,
        })
    }
}

fn finalize_stream_tool_call(call: StreamingToolCall) -> Result<ToolCall> {
    let arguments = if call.arguments.trim().is_empty() {
        json!({})
    } else {
        // Drop the serde error so a malformed buffer cannot surface any of the
        // tool arguments; the fixed message is sufficient for diagnostics.
        serde_json::from_str(&call.arguments).map_err(|_| {
            anyhow!("OpenAI-compatible tool call arguments were incomplete or invalid JSON")
        })?
    };
    Ok(ToolCall {
        id: call.id,
        name: call.name,
        arguments,
        thought_signature: None,
    })
}

fn chat_headers(api_key: Option<&str>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(USER_AGENT, HeaderValue::from_static("iris-agent"));
    if let Some(key) = api_key.map(str::trim).filter(|key| !key.is_empty()) {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {key}"))?,
        );
    }
    Ok(headers)
}

fn resolve_chat_url(base_url: &str) -> Result<Url> {
    let mut url = Url::parse(base_url)
        .with_context(|| format!("invalid OpenAI-compatible base URL: {base_url}"))?;
    let path = url.path().trim_end_matches('/');
    let next_path = if path.ends_with("/chat/completions") {
        path.to_string()
    } else if path.is_empty() {
        "/chat/completions".to_string()
    } else {
        format!("{path}/chat/completions")
    };
    url.set_path(&next_path);
    Ok(url)
}

fn build_chat_request(
    model: &str,
    system_prompt: &str,
    messages: &[Message],
    tools: &Tools,
    reasoning: Option<ReasoningEffort>,
    supports_reasoning: bool,
    prompt_cache: ChatPromptCache<'_>,
) -> Value {
    let mut body = json!({
        "model": model,
        // Stream tokens as they are generated so the UI renders text live
        // instead of waiting for the whole completion. `include_usage` asks
        // for a final usage chunk (supported by OpenAI and the mainstream
        // OpenAI-compatible servers).
        "stream": true,
        "stream_options": { "include_usage": true },
        "messages": build_chat_messages(system_prompt, messages),
    });
    let declarations = tool_declarations(tools);
    if !declarations.is_empty() {
        body["tools"] = Value::Array(declarations);
    }
    if supports_reasoning && let Some(level) = reasoning.and_then(openai_reasoning_effort) {
        body["reasoning_effort"] = json!(level);
    }
    if prompt_cache.retention.caching_enabled() {
        if let Some(key) = prompt_cache
            .key
            .and_then(super::clamp_openai_prompt_cache_key)
        {
            body["prompt_cache_key"] = json!(key);
        }
        if prompt_cache.retention == PromptCacheRetention::Long {
            body["prompt_cache_retention"] = json!("24h");
        }
    }
    body
}

fn openai_reasoning_effort(level: ReasoningEffort) -> Option<&'static str> {
    match level {
        ReasoningEffort::Off => None,
        ReasoningEffort::Minimal | ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High | ReasoningEffort::XHigh => Some("high"),
    }
}

fn build_chat_messages(system_prompt: &str, messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    if !system_prompt.trim().is_empty() {
        out.push(json!({ "role": "system", "content": system_prompt }));
    }
    for message in messages {
        match message.role {
            Role::Developer => out.push(json!({ "role": "developer", "content": message.content })),
            Role::User => out.push(json!({ "role": "user", "content": message.content })),
            Role::Assistant => push_assistant_content(&mut out, &message.content),
            // Chat Completions has no reasoning replay channel; re-sending
            // reasoning as assistant content would re-bill the chain-of-thought
            // as input on every request (ADR-0041), so reasoning rows are
            // display/persistence-only on this lane.
            Role::AssistantReasoning => {}
            Role::AssistantToolCall => push_assistant_tool_call(&mut out, message),
            Role::Tool => out.push(json!({
                "role": "tool",
                "tool_call_id": message.tool_call_id.as_deref().unwrap_or_default(),
                "content": message.content,
            })),
        }
    }
    out
}

fn push_assistant_content(out: &mut Vec<Value>, content: &str) {
    if content.is_empty() {
        return;
    }
    let assistant = ensure_assistant_message(out);
    let current = assistant
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let next = if current.is_empty() {
        content.to_string()
    } else {
        format!("{current}\n{content}")
    };
    assistant.insert("content".to_string(), Value::String(next));
}

fn push_assistant_tool_call(out: &mut Vec<Value>, message: &Message) {
    let assistant = ensure_assistant_message(out);
    assistant.entry("content").or_insert(Value::Null);
    let call = json!({
        "id": message.tool_call_id.as_deref().unwrap_or_default(),
        "type": "function",
        "function": {
            "name": message.tool_name.as_deref().unwrap_or_default(),
            "arguments": message.content,
        }
    });
    match assistant.get_mut("tool_calls") {
        Some(Value::Array(calls)) => calls.push(call),
        _ => {
            assistant.insert("tool_calls".to_string(), Value::Array(vec![call]));
        }
    }
}

fn ensure_assistant_message(out: &mut Vec<Value>) -> &mut serde_json::Map<String, Value> {
    let needs_new = !matches!(
        out.last(),
        Some(Value::Object(object))
            if object.get("role").and_then(Value::as_str) == Some("assistant")
    );
    if needs_new {
        out.push(json!({ "role": "assistant", "content": Value::Null }));
    }
    out.last_mut()
        .and_then(Value::as_object_mut)
        .expect("assistant message is an object")
}

fn tool_declarations(tools: &Tools) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name(),
                    "description": tool.description(),
                    "parameters": tool.parameters(),
                }
            })
        })
        .collect()
}

fn parse_usage(value: &Value, provider: &str, model: &str) -> Option<ProviderUsage> {
    let usage = value.get("usage")?;
    let input_tokens = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
    let cache_read_input_tokens = usage
        .get("prompt_tokens_details")
        .or_else(|| usage.get("input_tokens_details"))
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let reasoning_output_tokens = usage
        .get("completion_tokens_details")
        .or_else(|| usage.get("output_tokens_details"))
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    Some(ProviderUsage {
        provider: provider.to_string(),
        model: model.to_string(),
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens: 0,
        reasoning_output_tokens,
        total_tokens,
        cache_creation: None,
    })
}

fn map_finish_reason(reason: &str) -> CompletionReason {
    match reason {
        "stop" => CompletionReason::EndTurn,
        "tool_calls" | "function_call" => CompletionReason::ToolUse,
        "length" => CompletionReason::MaxOutputTokens,
        "content_filter" => CompletionReason::Refusal,
        _ => CompletionReason::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mimir::selection::ReasoningEffort;
    use crate::nexus::{Message, Role, Tools};
    use anyhow::Result;
    use serde_json::json;

    fn tool_call(id: &str, name: &str, args: serde_json::Value) -> Message {
        Message {
            role: Role::AssistantToolCall,
            content: args.to_string(),
            tool_call_id: Some(id.to_string()),
            tool_name: Some(name.to_string()),
            continuity: None,
            provider_turn_id: None,
            redacted: false,
            origin: None,
        }
    }

    fn tool_result(id: &str, name: &str, content: &str) -> Message {
        Message {
            role: Role::Tool,
            content: content.to_string(),
            tool_call_id: Some(id.to_string()),
            tool_name: Some(name.to_string()),
            continuity: None,
            provider_turn_id: None,
            redacted: false,
            origin: None,
        }
    }

    #[test]
    fn builds_chat_completions_request_with_tools_and_reasoning_when_enabled() {
        let messages = vec![
            Message::user("inspect"),
            Message::assistant("working"),
            tool_call("call_1", "read", json!({ "path": "src/main.rs" })),
            tool_result("call_1", "read", "file text"),
        ];
        let request = build_chat_request(
            "gpt-test",
            "system prompt",
            &messages,
            &crate::tools::built_in_tools(),
            Some(ReasoningEffort::High),
            true,
            ChatPromptCache {
                key: None,
                retention: PromptCacheRetention::None,
            },
        );

        assert_eq!(request["model"], json!("gpt-test"));
        assert_eq!(request["stream"], json!(true));
        assert_eq!(request["stream_options"], json!({ "include_usage": true }));
        assert_eq!(request["reasoning_effort"], json!("high"));
        assert_eq!(
            request["messages"][0],
            json!({ "role": "system", "content": "system prompt" })
        );
        assert_eq!(
            request["messages"][1],
            json!({ "role": "user", "content": "inspect" })
        );
        assert_eq!(request["messages"][2]["role"], json!("assistant"));
        assert_eq!(request["messages"][2]["content"], json!("working"));
        assert_eq!(
            request["messages"][2]["tool_calls"][0]["id"],
            json!("call_1")
        );
        assert_eq!(
            request["messages"][2]["tool_calls"][0]["function"]["name"],
            json!("read")
        );
        assert_eq!(
            request["messages"][3],
            json!({ "role": "tool", "tool_call_id": "call_1", "content": "file text" })
        );
        assert_eq!(request["tools"][0]["type"], json!("function"));
        assert_eq!(request["tools"][0]["function"]["name"], json!("read"));
    }

    #[test]
    fn prompt_cache_fields_follow_retention_setting() {
        let messages = [Message::user("hi")];
        let short = build_chat_request(
            "gpt-test",
            "P",
            &messages,
            &Tools::new(Vec::new()),
            None,
            true,
            ChatPromptCache {
                key: Some(" session-1 "),
                retention: PromptCacheRetention::Short,
            },
        );
        assert_eq!(short["prompt_cache_key"], json!("session-1"));
        assert!(short.get("prompt_cache_retention").is_none());

        let long = build_chat_request(
            "gpt-test",
            "P",
            &messages,
            &Tools::new(Vec::new()),
            None,
            true,
            ChatPromptCache {
                key: Some("session-1"),
                retention: PromptCacheRetention::Long,
            },
        );
        assert_eq!(long["prompt_cache_key"], json!("session-1"));
        assert_eq!(long["prompt_cache_retention"], json!("24h"));

        let disabled = build_chat_request(
            "gpt-test",
            "P",
            &messages,
            &Tools::new(Vec::new()),
            None,
            true,
            ChatPromptCache {
                key: Some("session-1"),
                retention: PromptCacheRetention::None,
            },
        );
        assert!(disabled.get("prompt_cache_key").is_none());
        assert!(disabled.get("prompt_cache_retention").is_none());
    }

    #[test]
    fn prompt_cache_key_is_clamped_to_64_unicode_scalars() {
        let messages = [Message::user("hi")];
        let long_key = format!("{}tail", "å".repeat(70));
        let request = build_chat_request(
            "gpt-test",
            "P",
            &messages,
            &Tools::new(Vec::new()),
            None,
            true,
            ChatPromptCache {
                key: Some(&long_key),
                retention: PromptCacheRetention::Short,
            },
        );
        let key = request["prompt_cache_key"].as_str().expect("cache key");
        assert_eq!(key.chars().count(), 64);
        assert_eq!(key, "å".repeat(64));
    }

    #[test]
    fn reasoning_rows_are_omitted_from_chat_messages() {
        // Reasoning rows (own or foreign) never re-enter the request: Chat
        // Completions has no replay channel, so re-sending them as assistant
        // content would re-bill the chain-of-thought every turn (ADR-0041).
        let origin = crate::nexus::ModelOrigin::new("openai", API_ID, "gpt-test");
        let messages = vec![
            Message::user("go"),
            Message::assistant_reasoning_block(crate::nexus::ReasoningBlock::new(
                "internal deliberation",
                None,
                false,
                origin,
            )),
            Message::assistant("answer"),
        ];
        let out = build_chat_messages("P", &messages);
        assert_eq!(out.len(), 3, "system + user + one assistant message");
        assert_eq!(out[2]["role"], json!("assistant"));
        assert_eq!(out[2]["content"], json!("answer"));
        assert!(
            !out.iter()
                .any(|m| m.to_string().contains("internal deliberation")),
            "reasoning text must not appear anywhere in the request: {out:?}"
        );
    }

    #[test]
    fn omits_reasoning_when_disabled_or_off() {
        let messages = [Message::user("hi")];
        let request = build_chat_request(
            "llama3.1",
            "P",
            &messages,
            &Tools::new(Vec::new()),
            Some(ReasoningEffort::High),
            false,
            ChatPromptCache {
                key: None,
                retention: PromptCacheRetention::None,
            },
        );
        assert!(request.get("reasoning_effort").is_none());

        let request = build_chat_request(
            "llama3.1",
            "P",
            &messages,
            &Tools::new(Vec::new()),
            Some(ReasoningEffort::Off),
            true,
            ChatPromptCache {
                key: None,
                retention: PromptCacheRetention::None,
            },
        );
        assert!(request.get("reasoning_effort").is_none());

        let request = build_chat_request(
            "gpt-test",
            "P",
            &messages,
            &Tools::new(Vec::new()),
            Some(ReasoningEffort::XHigh),
            true,
            ChatPromptCache {
                key: None,
                retention: PromptCacheRetention::None,
            },
        );
        assert_eq!(request["reasoning_effort"], json!("high"));
    }

    #[test]
    fn resolves_chat_completions_url_without_double_appending_path() -> Result<()> {
        assert_eq!(
            resolve_chat_url("https://api.openai.com/v1")?.as_str(),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            resolve_chat_url("http://localhost:11434/v1/chat/completions")?.as_str(),
            "http://localhost:11434/v1/chat/completions"
        );
        assert!(resolve_chat_url("not a url").is_err());
        Ok(())
    }

    /// Recording sink so tests can assert what streamed live.
    struct RecordingSink(Vec<String>);

    impl TurnSink for RecordingSink {
        fn on_text_delta(&mut self, delta: &str) -> Result<()> {
            self.0.push(delta.to_string());
            Ok(())
        }
    }

    fn parse_stream(events: &[Value], done: bool) -> (Result<AssistantTurn>, Vec<String>) {
        let mut parser = ChatStreamParser::new("openai", "gpt-test");
        let mut sink = RecordingSink(Vec::new());
        for event in events {
            parser
                .ingest_event(&event.to_string(), &mut sink)
                .expect("event ingested");
        }
        if done {
            parser.ingest_event("[DONE]", &mut sink).expect("[DONE]");
        }
        (parser.finish(), sink.0)
    }

    #[test]
    fn streams_text_deltas_live_and_assembles_the_turn() {
        let events = [
            json!({ "id": "chatcmpl_1", "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "Hel" }, "finish_reason": null }] }),
            json!({ "id": "chatcmpl_1", "choices": [{ "index": 0, "delta": { "content": "lo" }, "finish_reason": null }] }),
            json!({ "id": "chatcmpl_1", "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
        ];
        let (turn, deltas) = parse_stream(&events, true);
        let turn = turn.expect("stream parsed");
        assert_eq!(deltas, vec!["Hel".to_string(), "lo".to_string()]);
        assert_eq!(turn.text.as_deref(), Some("Hello"));
        assert_eq!(turn.response_id.as_deref(), Some("chatcmpl_1"));
        assert_eq!(
            turn.completion_reason,
            Some(crate::nexus::CompletionReason::EndTurn)
        );
    }

    #[test]
    fn assembles_tool_calls_reasoning_and_usage_from_stream_chunks() {
        let events = [
            json!({ "id": "chatcmpl_2", "choices": [{ "index": 0, "delta": { "reasoning_content": "thin" }, "finish_reason": null }] }),
            json!({ "choices": [{ "index": 0, "delta": { "reasoning_content": "king" }, "finish_reason": null }] }),
            json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "id": "call_1", "type": "function", "function": { "name": "read", "arguments": "" } }] }, "finish_reason": null }] }),
            json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "{\"path\":" } }] }, "finish_reason": null }] }),
            json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "\"src/main.rs\"}" } }] }, "finish_reason": null }] }),
            json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
            json!({ "choices": [], "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "prompt_tokens_details": { "cached_tokens": 3 },
                "completion_tokens_details": { "reasoning_tokens": 2 }
            } }),
        ];
        let (turn, deltas) = parse_stream(&events, true);
        let turn = turn.expect("stream parsed");
        assert!(deltas.is_empty(), "tool/reasoning deltas are not visible");
        assert!(turn.text.is_none());
        assert_eq!(turn.reasoning.len(), 1);
        assert_eq!(turn.reasoning[0].text, "thinking");
        assert_eq!(turn.reasoning[0].origin.provider, "openai");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_1");
        assert_eq!(turn.tool_calls[0].name, "read");
        assert_eq!(
            turn.tool_calls[0].arguments,
            json!({ "path": "src/main.rs" })
        );
        assert_eq!(
            turn.completion_reason,
            Some(crate::nexus::CompletionReason::ToolUse)
        );
        let usage = turn.usage.expect("usage from final chunk");
        assert_eq!(usage.provider, "openai");
        assert_eq!(usage.model, "gpt-test");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.cache_read_input_tokens, 3);
        assert_eq!(usage.reasoning_output_tokens, 2);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn truncated_stream_is_a_retryable_protocol_anomaly() {
        // No [DONE], no finish_reason: the socket died mid-stream.
        let events = [
            json!({ "id": "chatcmpl_3", "choices": [{ "index": 0, "delta": { "content": "par" }, "finish_reason": null }] }),
        ];
        let (turn, deltas) = parse_stream(&events, false);
        let error = turn.expect_err("truncated stream must not produce a turn");
        assert!(
            error.downcast_ref::<ChatStreamProtocolAnomaly>().is_some(),
            "typed anomaly so the transport can classify: {error}"
        );
        // Visible text streamed, so the transport must NOT retry this attempt.
        assert!(!protocol_anomaly_retryable(&error, true));
        assert!(protocol_anomaly_retryable(&error, false));
        assert_eq!(deltas, vec!["par".to_string()]);
    }

    #[test]
    fn empty_completion_with_finish_reason_is_a_valid_turn() {
        let events = [
            json!({ "id": "chatcmpl_4", "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
        ];
        let (turn, _) = parse_stream(&events, true);
        let turn = turn.expect("empty completion with stop reason is legitimate");
        assert!(turn.text.is_none());
        assert!(turn.tool_calls.is_empty());
    }

    #[test]
    fn developer_context_keeps_its_chat_role() {
        let messages = build_chat_messages("system", &[Message::developer("skill catalog")]);

        assert_eq!(messages[0]["role"], json!("system"));
        assert_eq!(messages[1]["role"], json!("developer"));
        assert_eq!(messages[1]["content"], json!("skill catalog"));
    }
}
