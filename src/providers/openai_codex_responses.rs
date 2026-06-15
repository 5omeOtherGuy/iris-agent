use std::env;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::{Value, json};

use crate::auth::openai_codex::{AccessToken, OpenAiCodexTokenStore};
use crate::nexus::{AssistantTurn, ChatProvider, Message, Role, ToolCall};

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const DEFAULT_MODEL: &str = "gpt-5.5";

#[derive(Debug, Clone)]
pub(crate) struct OpenAiCodexResponsesProvider {
    client: Client,
    config: OpenAiCodexResponsesConfig,
    tokens: OpenAiCodexTokenStore,
}

impl OpenAiCodexResponsesProvider {
    pub(crate) fn from_env() -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()?,
            config: OpenAiCodexResponsesConfig::from_env(),
            tokens: OpenAiCodexTokenStore::from_env()?,
        })
    }
}

impl ChatProvider for OpenAiCodexResponsesProvider {
    fn respond(&self, messages: &[Message]) -> Result<AssistantTurn> {
        let token = self.tokens.access_token(&self.client)?;
        let request = build_codex_request(&self.config.model, messages);
        let response = self
            .client
            .post(resolve_codex_url(&self.config.base_url)?)
            .headers(codex_headers(&token)?)
            .json(&request)
            .send()
            .context("failed to send Codex request")?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            bail!("Codex request failed ({status}): {body}");
        }

        parse_response_stream(
            &response
                .text()
                .context("failed to read Codex stream response")?,
        )
    }
}

#[derive(Debug, Clone)]
struct OpenAiCodexResponsesConfig {
    model: String,
    base_url: String,
}

impl OpenAiCodexResponsesConfig {
    fn from_env() -> Self {
        let model = non_empty_env("IRIS_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let base_url =
            non_empty_env("IRIS_CODEX_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Self { model, base_url }
    }
}

fn build_codex_request(model: &str, messages: &[Message]) -> Value {
    // The Codex adapter owns conversion between Nexus messages and Responses wire JSON.
    let input: Vec<Value> = messages.iter().map(codex_input_item).collect();

    json!({
        "model": model,
        "store": false,
        "stream": true,
        "instructions": "You are Iris, a helpful terminal coding assistant. You have file and shell tools: read, bash, edit, write, grep, find, ls, and hashline_edit. Use them to inspect and modify the current workspace.",
        "input": input,
        "tools": crate::tools::tool_definitions(),
        "text": { "verbosity": "low" },
    })
}

fn codex_input_item(message: &Message) -> Value {
    match message.role {
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
    }
}

fn message_content_type(role: Role) -> &'static str {
    match role {
        Role::User => "input_text",
        Role::Assistant => "output_text",
        Role::AssistantToolCall | Role::Tool => unreachable!("tool messages are not text messages"),
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

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
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
    let turn = extract_assistant_turn(&value);
    if turn.text.as_deref().unwrap_or_default().is_empty() && turn.tool_calls.is_empty() {
        bail!("Codex response did not include assistant text or tool calls");
    }
    Ok(turn)
}

fn parse_response_stream(body: &str) -> Result<AssistantTurn> {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut completed_response = None;
    let mut saw_completed = false;

    for event in body.split("\n\n") {
        let data = event_data(event);
        if data.is_empty() || data == "[DONE]" {
            continue;
        }

        let value: Value = serde_json::from_str(&data).context("failed to parse Codex SSE data")?;
        match value.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    text.push_str(delta);
                }
            }
            Some("response.output_item.done") => {
                if let Some(item) = value.get("item") {
                    if text.is_empty() {
                        text.push_str(&extract_output_text(item));
                    }
                    if let Some(call) = extract_tool_call(item) {
                        tool_calls.push(call);
                    }
                }
            }
            Some("response.completed") => {
                saw_completed = true;
                completed_response = value.get("response").cloned();
            }
            Some("response.failed") => bail!("Codex response failed: {}", response_error(&value)),
            Some("response.incomplete") => {
                bail!("Codex response incomplete: {}", incomplete_reason(&value))
            }
            _ => {}
        }
    }

    if !saw_completed {
        bail!("Codex stream closed before response.completed");
    }
    if let Some(response) = completed_response.as_ref() {
        let completed_turn = extract_assistant_turn(response);
        if text.is_empty() {
            text.push_str(completed_turn.text.as_deref().unwrap_or_default());
        }
        if tool_calls.is_empty() {
            tool_calls = completed_turn.tool_calls;
        }
    }
    if text.is_empty() && tool_calls.is_empty() {
        bail!("Codex response did not include assistant text or tool calls");
    }
    Ok(AssistantTurn {
        text: (!text.is_empty()).then_some(text),
        tool_calls,
    })
}

fn event_data(event: &str) -> String {
    event
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n")
}

fn response_error(value: &Value) -> String {
    value
        .get("response")
        .and_then(|response| response.get("error"))
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("response.failed event received")
        .to_string()
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

fn extract_assistant_turn(value: &Value) -> AssistantTurn {
    let text = extract_output_text(value);
    let tool_calls = extract_tool_calls(value);
    AssistantTurn {
        text: (!text.is_empty()).then_some(text),
        tool_calls,
    }
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
mod tests {
    use super::*;

    #[test]
    fn resolves_codex_responses_url() -> Result<()> {
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api")?.as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api/codex")?.as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            resolve_codex_url("https://chatgpt.com/backend-api/codex/responses")?.as_str(),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_codex_base_url() {
        assert!(resolve_codex_url("not a url").is_err());
    }

    #[test]
    fn builds_codex_request_from_conversation() {
        let request = build_codex_request(
            "gpt-test",
            &[Message::user("hello"), Message::assistant("hi")],
        );
        assert_eq!(request["model"], "gpt-test");
        assert_eq!(request["stream"], true);
        assert_eq!(request["input"].as_array().unwrap().len(), 2);
        assert_eq!(request["input"][0]["role"], "user");
        assert_eq!(request["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(request["input"][0]["content"][0]["text"], "hello");
        assert_eq!(request["input"][1]["role"], "assistant");
        assert_eq!(request["input"][1]["content"][0]["type"], "output_text");
        assert_eq!(request["tools"][0]["name"], "read");
    }

    #[test]
    fn builds_codex_request_from_tool_messages() {
        let request = build_codex_request(
            "gpt-test",
            &[
                Message {
                    role: Role::AssistantToolCall,
                    content: json!({ "path": "src/main.rs" }).to_string(),
                    tool_call_id: Some("call_1".to_string()),
                    tool_name: Some("read".to_string()),
                },
                Message {
                    role: Role::Tool,
                    content: json!({ "ok": true, "content": "file text" }).to_string(),
                    tool_call_id: Some("call_1".to_string()),
                    tool_name: Some("read".to_string()),
                },
            ],
        );

        assert_eq!(request["input"][0]["type"], "function_call");
        assert_eq!(request["input"][0]["call_id"], "call_1");
        assert_eq!(request["input"][0]["name"], "read");
        assert_eq!(request["input"][1]["type"], "function_call_output");
        assert_eq!(request["input"][1]["call_id"], "call_1");
        assert!(
            request["input"][1]["output"]
                .as_str()
                .unwrap()
                .contains("file text")
        );
    }

    #[test]
    fn parses_streamed_output_text_delta_events() -> Result<()> {
        let stream = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        );

        assert_eq!(
            parse_response_stream(stream)?.text.as_deref(),
            Some("Hello")
        );
        Ok(())
    }

    #[test]
    fn parses_streamed_output_item_done_events() -> Result<()> {
        let stream = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        );

        assert_eq!(
            parse_response_stream(stream)?.text.as_deref(),
            Some("Hello")
        );
        Ok(())
    }

    #[test]
    fn parses_streamed_tool_call() -> Result<()> {
        let stream = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"src/main.rs\\\"}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        );

        let turn = parse_response_stream(stream)?;

        assert!(turn.text.is_none());
        assert_eq!(turn.tool_calls[0].id, "call_1");
        assert_eq!(turn.tool_calls[0].name, "read");
        assert_eq!(turn.tool_calls[0].arguments["path"], "src/main.rs");
        Ok(())
    }

    #[test]
    fn parses_responses_output_text() -> Result<()> {
        let response = json!({
            "output": [{
                "type": "message",
                "content": [{ "type": "output_text", "text": "Hello" }]
            }]
        });
        assert_eq!(
            parse_response_json(response)?.text.as_deref(),
            Some("Hello")
        );
        Ok(())
    }

    #[test]
    fn parses_responses_tool_call() -> Result<()> {
        let response = json!({
            "output": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "read",
                "arguments": { "path": "src/main.rs" }
            }]
        });

        let turn = parse_response_json(response)?;

        assert!(turn.text.is_none());
        assert_eq!(turn.tool_calls[0].id, "call_1");
        assert_eq!(turn.tool_calls[0].name, "read");
        assert_eq!(turn.tool_calls[0].arguments["path"], "src/main.rs");
        Ok(())
    }

    #[test]
    fn rejects_response_without_text() {
        let response = json!({ "output": [{ "type": "message", "content": [] }] });
        let error = parse_response_json(response).unwrap_err().to_string();
        assert!(error.contains("did not include assistant text or tool calls"));
    }
}
