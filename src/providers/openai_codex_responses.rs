use std::env;
use std::io::{BufRead, BufReader};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{
    AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER, USER_AGENT,
};
use serde_json::{Value, json};

use crate::auth::openai_codex::{AccessToken, OpenAiCodexTokenStore};
use crate::errors::AuthError;
use crate::nexus::{AssistantTurn, ChatProvider, Message, Role, ToolCall, TurnSink};
use crate::telemetry;

const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const DEFAULT_MODEL: &str = "gpt-5.5";

// Transport resilience for Codex requests. Transient failures (network, 429,
// 5xx) are retried with exponential backoff plus jitter; a single auth
// rejection (401/403) triggers one forced token refresh before retrying.
const MAX_TRANSIENT_RETRIES: u32 = 3;
const BASE_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(8);

#[derive(Debug, Clone)]
pub(crate) struct OpenAiCodexResponsesProvider {
    client: Client,
    config: OpenAiCodexResponsesConfig,
    tokens: OpenAiCodexTokenStore,
}

impl OpenAiCodexResponsesProvider {
    /// Build the provider, resolving model and base URL from the optional
    /// settings values. The provider stays decoupled from the app-level config
    /// type by taking only the strings it needs.
    pub(crate) fn new(model: Option<&str>, base_url: Option<&str>) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(120))
                .build()?,
            config: OpenAiCodexResponsesConfig::resolve(model, base_url),
            tokens: OpenAiCodexTokenStore::from_env()?,
        })
    }
}

impl ChatProvider for OpenAiCodexResponsesProvider {
    fn respond(&self, messages: &[Message], sink: &mut dyn TurnSink) -> Result<AssistantTurn> {
        let span = tracing::info_span!("codex_roundtrip", model = %self.config.model);
        let _guard = span.enter();

        let request = build_codex_request(&self.config.model, messages);
        let url = resolve_codex_url(&self.config.base_url)?;

        run_retry_loop(
            |force_refresh| {
                if force_refresh {
                    self.tokens.force_refresh(&self.client)
                } else {
                    self.tokens.access_token(&self.client)
                }
            },
            |token| {
                let started = Instant::now();
                let attempt = self.send_once(url.clone(), token, &request, sink);
                tracing::debug!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "codex attempt complete"
                );
                attempt
            },
            sleep,
        )
    }
}

/// Drive the transient-retry / one-shot-reauth state machine.
///
/// Pure of HTTP and timing concerns so it can be unit-tested with scripted
/// closures: `get_token(force_refresh)` obtains a token (cached, or forcibly
/// refreshed after an auth rejection), `send` performs one attempt, and `sleep`
/// applies a backoff delay. Termination is guaranteed: reauth fires at most
/// once, transient retries are bounded by `MAX_TRANSIENT_RETRIES` and are not
/// reset by a reauth, and every other branch returns.
fn run_retry_loop(
    mut get_token: impl FnMut(bool) -> Result<AccessToken>,
    mut send: impl FnMut(&AccessToken) -> Attempt,
    mut sleep: impl FnMut(Duration),
) -> Result<AssistantTurn> {
    let mut transient_retries: u32 = 0;
    let mut reauth_used = false;
    let mut force_refresh = false;

    loop {
        let token = match get_token(force_refresh) {
            Ok(token) => token,
            Err(error) => {
                tracing::error!(error = %format!("{error:#}"), "failed to obtain access token");
                return Err(AuthError::new("authentication failed").into());
            }
        };
        force_refresh = false;
        tracing::debug!(token = %telemetry::redact_secret(&token.bearer), "using access token");

        match send(&token) {
            Attempt::Done(turn) => return Ok(turn),
            Attempt::Reauth(error) => {
                if reauth_used {
                    tracing::error!(error = %format!("{error:#}"), "codex auth rejected after refresh");
                    return Err(AuthError::new("authentication failed").into());
                }
                reauth_used = true;
                force_refresh = true;
                tracing::warn!(error = %format!("{error:#}"), "codex auth rejected; refreshing token and retrying");
                continue;
            }
            Attempt::Retry(error, retry_after) => {
                if transient_retries >= MAX_TRANSIENT_RETRIES {
                    tracing::error!(error = %format!("{error:#}"), retries = transient_retries, "codex transient error; retries exhausted");
                    return Err(error);
                }
                transient_retries += 1;
                let delay = backoff_delay(transient_retries, retry_after, BASE_BACKOFF);
                tracing::warn!(
                    error = %format!("{error:#}"),
                    attempt = transient_retries,
                    delay_ms = delay.as_millis() as u64,
                    "codex transient error; retrying"
                );
                sleep(delay);
                continue;
            }
            Attempt::Fatal(error) => {
                tracing::error!(error = %format!("{error:#}"), "codex request failed");
                return Err(error);
            }
        }
    }
}

/// Outcome of a single HTTP attempt, classified for the retry loop.
enum Attempt {
    Done(AssistantTurn),
    /// Auth rejected (401/403): force one token refresh, then retry.
    Reauth(anyhow::Error),
    /// Transient (network/429/5xx): retry with backoff; carries any server hint.
    Retry(anyhow::Error, Option<Duration>),
    /// Non-retryable (4xx other, malformed response): give up now.
    Fatal(anyhow::Error),
}

impl OpenAiCodexResponsesProvider {
    fn send_once(
        &self,
        url: Url,
        token: &AccessToken,
        request: &Value,
        sink: &mut dyn TurnSink,
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
            return match parse_response_stream_reader(BufReader::new(response), sink) {
                Ok(turn) => Attempt::Done(turn),
                Err(error) => Attempt::Fatal(error),
            };
        }

        let retry_after = parse_retry_after(response.headers());
        let body = response.text().unwrap_or_default();
        let error = match telemetry::sanitize_external_body(&body) {
            Some(detail) => anyhow!("Codex request failed ({status}): {detail}"),
            None => anyhow!("Codex request failed ({status})"),
        };
        match classify_http_status(status.as_u16()) {
            HttpClass::Reauth => Attempt::Reauth(error),
            HttpClass::Retry => Attempt::Retry(error, retry_after),
            HttpClass::Fatal => Attempt::Fatal(error),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpClass {
    Reauth,
    Retry,
    Fatal,
}

/// Classify an HTTP status into a retry policy class.
fn classify_http_status(status: u16) -> HttpClass {
    match status {
        401 | 403 => HttpClass::Reauth,
        408 | 425 | 429 => HttpClass::Retry,
        500..=599 => HttpClass::Retry,
        _ => HttpClass::Fatal,
    }
}

/// Compute the delay before the next transient retry.
///
/// Honors a server `Retry-After` hint when present (bounded against
/// pathological values); otherwise exponential backoff from `base` doubling
/// per retry, clamped to `MAX_BACKOFF`. Either way, up to 250ms of jitter is
/// added so concurrent requests hitting the same rate-limit window do not
/// retry in lockstep.
fn backoff_delay(retry: u32, retry_after: Option<Duration>, base: Duration) -> Duration {
    let jitter = Duration::from_millis(rand::random::<u64>() % 250);
    if let Some(after) = retry_after {
        return after.min(MAX_BACKOFF.saturating_mul(4)) + jitter;
    }
    let shift = retry.saturating_sub(1).min(10);
    let exp = base
        .checked_mul(1u32 << shift)
        .unwrap_or(MAX_BACKOFF)
        .min(MAX_BACKOFF);
    exp + jitter
}

/// Parse an integer-seconds `Retry-After` header. The HTTP-date form is
/// uncommon for 429s and is intentionally ignored.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let seconds: u64 = headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(seconds))
}

#[derive(Debug, Clone)]
struct OpenAiCodexResponsesConfig {
    model: String,
    base_url: String,
}

impl OpenAiCodexResponsesConfig {
    /// Resolve each value with precedence `env > settings file > built-in
    /// default`, so explicit runtime input always wins over persisted config.
    fn resolve(model: Option<&str>, base_url: Option<&str>) -> Self {
        Self {
            model: resolve_setting(non_empty_env("IRIS_MODEL"), model, DEFAULT_MODEL),
            base_url: resolve_setting(
                non_empty_env("IRIS_CODEX_BASE_URL"),
                base_url,
                DEFAULT_BASE_URL,
            ),
        }
    }
}

/// Three-layer precedence helper: env override, then settings value, then the
/// built-in default. Blank/whitespace-only settings values are ignored so an
/// empty `"defaultModel": ""` falls back to the default instead of sending an
/// invalid request. Pure so the precedence is unit-tested without env state.
fn resolve_setting(env_value: Option<String>, setting: Option<&str>, default: &str) -> String {
    env_value
        .or_else(|| {
            setting
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| default.to_string())
}

fn build_codex_request(model: &str, messages: &[Message]) -> Value {
    // The Codex adapter owns conversion between Nexus messages and Responses wire JSON.
    let input: Vec<Value> = messages.iter().map(codex_input_item).collect();

    json!({
        "model": model,
        "store": false,
        "stream": true,
        "instructions": "You are Iris, a helpful terminal coding assistant. You have file and shell tools: read, bash, edit, write, grep, find, and ls. Use them to inspect and modify the current workspace.",
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

#[cfg(test)]
fn parse_response_stream(body: &str) -> Result<AssistantTurn> {
    let mut sink = NoopSink;
    parse_response_stream_reader(BufReader::new(body.as_bytes()), &mut sink)
}

fn parse_response_stream_reader(
    reader: impl BufRead,
    sink: &mut dyn TurnSink,
) -> Result<AssistantTurn> {
    let mut parser = ResponseStreamParser::default();
    let mut event = String::new();

    for line in reader.lines() {
        let line = line.context("failed to read Codex stream response")?;
        if line.trim_end_matches('\r').is_empty() {
            parser.ingest_event(&event, sink)?;
            event.clear();
        } else {
            event.push_str(&line);
            event.push('\n');
        }
    }
    if !event.is_empty() {
        parser.ingest_event(&event, sink)?;
    }

    parser.finish()
}

#[derive(Default)]
struct ResponseStreamParser {
    text: String,
    tool_calls: Vec<ToolCall>,
    completed_response: Option<Value>,
    saw_completed: bool,
}

impl ResponseStreamParser {
    fn ingest_event(&mut self, event: &str, sink: &mut dyn TurnSink) -> Result<()> {
        let data = event_data(event);
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }

        let value: Value = serde_json::from_str(&data).context("failed to parse Codex SSE data")?;
        match value.get("type").and_then(Value::as_str) {
            Some("response.output_text.delta") => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                    self.text.push_str(delta);
                    sink.on_text_delta(delta);
                }
            }
            Some("response.output_item.done") => {
                if let Some(item) = value.get("item") {
                    if self.text.is_empty() {
                        self.text.push_str(&extract_output_text(item));
                    }
                    if let Some(call) = extract_tool_call(item) {
                        self.tool_calls.push(call);
                    }
                }
            }
            Some("response.completed") => {
                self.saw_completed = true;
                self.completed_response = value.get("response").cloned();
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
            bail!("Codex stream closed before response.completed");
        }
        if let Some(response) = self.completed_response.as_ref() {
            let completed_turn = extract_assistant_turn(response);
            if self.text.is_empty() {
                self.text
                    .push_str(completed_turn.text.as_deref().unwrap_or_default());
            }
            if self.tool_calls.is_empty() {
                self.tool_calls = completed_turn.tool_calls;
            }
        }
        if self.text.is_empty() && self.tool_calls.is_empty() {
            bail!("Codex response did not include assistant text or tool calls");
        }
        Ok(AssistantTurn {
            text: (!self.text.is_empty()).then_some(self.text),
            tool_calls: self.tool_calls,
        })
    }
}

#[cfg(test)]
struct NoopSink;

#[cfg(test)]
impl TurnSink for NoopSink {
    fn on_text_delta(&mut self, _delta: &str) {}
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
#[path = "openai_codex_responses_tests.rs"]
mod tests;
