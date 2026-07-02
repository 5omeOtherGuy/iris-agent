use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::errors::AuthError;
use crate::mimir::providers::transport::{
    Attempt, classify_http_status_retryable, retry_after_hint, run_with_retry, spawn_stream,
};
use crate::mimir::retry::RetryPolicy;
use crate::mimir::selection::{ProviderId, ReasoningEffort};
use crate::nexus::{
    AssistantTurn, ChatProvider, CompletionReason, Message, ModelOrigin, ProviderStream,
    ProviderUsage, ReasoningBlock, Role, ToolCall, Tools,
};

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
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()?,
            provider: config.provider,
            model: config.model.to_string(),
            base_url: config.base_url.to_string(),
            reasoning: config.reasoning,
            system_prompt: config.system_prompt.to_string(),
            api_key: config.api_key.filter(|key| !key.trim().is_empty()),
            supports_reasoning: config.supports_reasoning,
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
        let request = build_chat_request(
            &self.model,
            &self.system_prompt,
            messages,
            tools,
            self.reasoning,
            self.supports_reasoning,
        );
        let url = resolve_chat_url(&self.base_url)?;
        let provider = self.clone();
        let cancel = cancel.clone();
        Ok(spawn_stream(
            move |_sink, cancel| {
                run_with_retry(
                    provider.provider.as_str(),
                    &provider.retry_policy,
                    cancel,
                    |_| Ok(()),
                    |_| provider.send_once(url.clone(), &request, cancel),
                )
            },
            cancel,
        ))
    }
}

impl OpenAiCompatibleChatProvider {
    fn send_once(&self, url: Url, request: &Value, cancel: &CancellationToken) -> Attempt {
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
                return Attempt::Retry(
                    anyhow!("failed to send OpenAI-compatible request: {error}"),
                    None,
                );
            }
        };
        if cancel.is_cancelled() {
            return Attempt::Fatal(anyhow!("OpenAI-compatible request cancelled"));
        }
        let status = response.status();
        if !status.is_success() {
            let retry_after = retry_after_hint(response.headers());
            let _ = response.text();
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
            let error = anyhow!("OpenAI-compatible request failed ({status})");
            return match classify_http_status_retryable(status.as_u16()) {
                crate::mimir::providers::transport::HttpClass::Retry => {
                    Attempt::Retry(error, retry_after)
                }
                _ => Attempt::Fatal(error),
            };
        }
        let value: Value = match response.json() {
            Ok(value) => value,
            Err(error) => {
                return Attempt::Fatal(anyhow!(
                    "failed to parse OpenAI-compatible response: {error}"
                ));
            }
        };
        match parse_chat_response(&value, self.provider.as_str(), &self.model) {
            Ok(turn) => Attempt::Done(Box::new(turn)),
            Err(error) => Attempt::Fatal(error),
        }
    }
}

fn chat_headers(api_key: Option<&str>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
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
) -> Value {
    let mut body = json!({
        "model": model,
        "stream": false,
        "messages": build_chat_messages(system_prompt, messages),
    });
    let declarations = tool_declarations(tools);
    if !declarations.is_empty() {
        body["tools"] = Value::Array(declarations);
    }
    if supports_reasoning && let Some(level) = reasoning.and_then(openai_reasoning_effort) {
        body["reasoning_effort"] = json!(level);
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
            Role::User => out.push(json!({ "role": "user", "content": message.content })),
            Role::Assistant => push_assistant_content(&mut out, &message.content),
            Role::AssistantReasoning => push_assistant_content(&mut out, &message.content),
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

fn parse_chat_response(value: &Value, provider: &str, model: &str) -> Result<AssistantTurn> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| anyhow!("OpenAI-compatible response missing choices"))?;
    let message = choice
        .get("message")
        .ok_or_else(|| anyhow!("OpenAI-compatible response missing message"))?;
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let reasoning = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| {
            ReasoningBlock::new(
                text,
                None,
                false,
                ModelOrigin::new(provider, "chat-completions", model),
            )
        })
        .into_iter()
        .collect();
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .map(parse_tool_call)
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(AssistantTurn {
        text,
        reasoning,
        tool_calls,
        response_id: value.get("id").and_then(Value::as_str).map(str::to_string),
        usage: parse_usage(value, provider, model),
        completion_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(map_finish_reason),
    })
}

fn parse_tool_call(value: &Value) -> Result<ToolCall> {
    let function = value.get("function").unwrap_or(&Value::Null);
    let raw_arguments = function.get("arguments").unwrap_or(&Value::Null);
    let arguments = match raw_arguments {
        Value::String(text) if text.trim().is_empty() => json!({}),
        Value::String(text) => serde_json::from_str(text)
            .map_err(|_| anyhow!("OpenAI-compatible tool call arguments were invalid JSON"))?,
        Value::Object(_) => raw_arguments.clone(),
        _ => json!({}),
    };
    Ok(ToolCall {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        name: function
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        arguments,
        thought_signature: None,
    })
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
        );

        assert_eq!(request["model"], json!("gpt-test"));
        assert_eq!(request["stream"], json!(false));
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
    fn omits_reasoning_when_disabled_or_off() {
        let messages = [Message::user("hi")];
        let request = build_chat_request(
            "llama3.1",
            "P",
            &messages,
            &Tools::new(Vec::new()),
            Some(ReasoningEffort::High),
            false,
        );
        assert!(request.get("reasoning_effort").is_none());

        let request = build_chat_request(
            "llama3.1",
            "P",
            &messages,
            &Tools::new(Vec::new()),
            Some(ReasoningEffort::Off),
            true,
        );
        assert!(request.get("reasoning_effort").is_none());

        let request = build_chat_request(
            "gpt-test",
            "P",
            &messages,
            &Tools::new(Vec::new()),
            Some(ReasoningEffort::XHigh),
            true,
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

    #[test]
    fn parses_text_tool_calls_usage_and_completion_reason() {
        let response = json!({
            "id": "chatcmpl_1",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": "",
                    "reasoning_content": "thinking",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read", "arguments": "{\"path\":\"src/main.rs\"}" }
                    }]
                }
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15,
                "prompt_tokens_details": { "cached_tokens": 3 },
                "completion_tokens_details": { "reasoning_tokens": 2 }
            }
        });

        let turn = parse_chat_response(&response, "openai", "gpt-test").unwrap();

        assert_eq!(turn.response_id.as_deref(), Some("chatcmpl_1"));
        assert!(turn.text.is_none());
        assert_eq!(turn.reasoning.len(), 1);
        assert_eq!(turn.reasoning[0].text, "thinking");
        assert_eq!(turn.reasoning[0].origin.provider, "openai");
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "call_1");
        assert_eq!(
            turn.tool_calls[0].arguments,
            json!({ "path": "src/main.rs" })
        );
        assert_eq!(
            turn.completion_reason,
            Some(crate::nexus::CompletionReason::ToolUse)
        );
        let usage = turn.usage.unwrap();
        assert_eq!(usage.provider, "openai");
        assert_eq!(usage.model, "gpt-test");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        assert_eq!(usage.cache_read_input_tokens, 3);
        assert_eq!(usage.reasoning_output_tokens, 2);
        assert_eq!(usage.total_tokens, 15);
    }
}
