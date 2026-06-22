use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{
    AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER, USER_AGENT,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::errors::AuthError;
use crate::mimir::auth::openai_codex::{AccessToken, OpenAiCodexTokenStore};
use crate::mimir::selection::{PromptCacheRetention, ReasoningEffort};
use crate::nexus::{
    AssistantTurn, ChatProvider, Message, ModelOrigin, ProviderEvent, ProviderStream,
    ProviderUsage, ReasoningBlock, Role, ToolCall, Tools,
};
use crate::telemetry;
use futures::channel::mpsc;

/// Provider-internal seam for incremental assistant text. The streamed SSE
/// parser pushes deltas here; the live provider forwards them onto the
/// [`ProviderStream`] channel, while tests use a no-op sink. Not part of the
/// Nexus contract: `ChatProvider` is async-streaming, so deltas reach the loop
/// as `ProviderEvent::TextDelta` rather than through a sink argument.
trait TurnSink {
    /// Forward one text delta. Returns `Err` when the consumer has dropped the
    /// stream (cancellation): the SSE read loop then stops early instead of
    /// draining the rest of the response, mirroring Codex's dropped-stream
    /// cancellation.
    fn on_text_delta(&mut self, delta: &str) -> Result<()>;
}

/// [`TurnSink`] that forwards each text delta onto the provider's event channel.
/// `unbounded_send` is synchronous, so it is safe to call from the blocking
/// request thread.
struct ChannelSink {
    tx: mpsc::UnboundedSender<Result<ProviderEvent>>,
}

impl TurnSink for ChannelSink {
    fn on_text_delta(&mut self, delta: &str) -> Result<()> {
        // A send error means the consumer dropped the stream (cancellation):
        // surface it so the SSE read loop breaks immediately rather than
        // downloading and discarding the rest of the response on a leaked thread.
        self.tx
            .unbounded_send(Ok(ProviderEvent::TextDelta(delta.to_string())))
            .map_err(|_| anyhow!("response stream dropped by consumer"))
    }
}

// Transport resilience for Codex requests. Transient failures (network, 429,
// 5xx) are retried with exponential backoff plus jitter; a single auth
// rejection (401/403) triggers one forced token refresh before retrying.
const MAX_TRANSIENT_RETRIES: u32 = 3;
const BASE_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(8);
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
        let (tx, rx) = mpsc::unbounded::<Result<ProviderEvent>>();
        // The blocking reqwest+SSE work runs off the loop's executor; it streams
        // text deltas through the channel and ends with one terminal item.
        // Mirrors Codex's `map_response_events` (spawn + channel), minus the
        // unused transport/telemetry machinery. The turn token is checked
        // cooperatively (before each attempt, across retry backoff, and between
        // SSE lines) so a cancelled turn stops promptly instead of draining the
        // whole response on a leaked thread.
        tokio::task::spawn_blocking(move || {
            let mut sink = ChannelSink { tx: tx.clone() };
            let terminal = match provider.run_blocking(url, &request, &mut sink, &cancel) {
                Ok(turn) => Ok(ProviderEvent::Completed(turn)),
                Err(error) => Err(error),
            };
            let _ = tx.unbounded_send(terminal);
        });
        Ok(Box::pin(rx))
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
                let attempt = self.send_once(url.clone(), token, request, sink, cancel);
                tracing::debug!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "codex attempt complete"
                );
                attempt
            },
            // Sleep in slices so a turn-level Ctrl-C interrupts retry backoff;
            // the loop's cancellation check then ends the attempt without
            // firing another request.
            |delay| sleep_cancellable(delay, cancel),
            || cancel.is_cancelled(),
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
    is_cancelled: impl Fn() -> bool,
) -> Result<AssistantTurn> {
    let mut transient_retries: u32 = 0;
    let mut reauth_used = false;
    let mut force_refresh = false;

    loop {
        // Checked before every attempt and after each backoff sleep: a cancelled
        // turn stops here rather than issuing or retrying a request.
        if is_cancelled() {
            return Err(anyhow!("Codex request cancelled"));
        }
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
            return match parse_response_stream_reader(
                BufReader::new(response),
                sink,
                cancel,
                &self.model,
            ) {
                Ok(turn) => {
                    if let Some(usage) = &turn.usage {
                        self.record_usage(usage);
                    }
                    Attempt::Done(turn)
                }
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

    fn record_usage(&self, usage: &ProviderUsage) {
        // Surface usage and the two distinct cache facts the diagnostics must
        // separate: whether Iris SENT a cacheable request (request-side opt-in)
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

/// Sleep up to `delay`, but in slices so `cancel` is observed promptly; returns
/// early once cancelled.
fn sleep_cancellable(delay: Duration, cancel: &CancellationToken) {
    const SLICE: Duration = Duration::from_millis(100);
    let deadline = Instant::now() + delay;
    while Instant::now() < deadline {
        if cancel.is_cancelled() {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        sleep(remaining.min(SLICE));
    }
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

fn parse_response_stream_reader(
    reader: impl BufRead,
    sink: &mut dyn TurnSink,
    cancel: &CancellationToken,
    model: &str,
) -> Result<AssistantTurn> {
    let mut parser = ResponseStreamParser::new(model);
    let mut event = String::new();

    for line in reader.lines() {
        // Between lines: a cancelled turn stops draining an actively streaming
        // response promptly (an idle socket read still blocks until the next
        // byte or the client timeout -- blocking reqwest cannot be force-aborted
        // mid-read).
        if cancel.is_cancelled() {
            bail!("Codex stream cancelled");
        }
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

struct ResponseStreamParser {
    origin: ModelOrigin,
    text: String,
    reasoning: Vec<ReasoningBlock>,
    tool_calls: Vec<ToolCall>,
    completed_response: Option<Value>,
    response_id: Option<String>,
    saw_completed: bool,
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
        }
    }

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
                    sink.on_text_delta(delta)?;
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
            bail!("Codex stream closed before response.completed");
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
        ReasoningBlock::new(&text, encrypted, encrypted.is_some(), origin.clone())
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
