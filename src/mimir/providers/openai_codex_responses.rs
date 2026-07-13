use std::collections::HashSet;
use std::future::Future;
use std::io::BufRead;
use std::io::BufReader;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures::{SinkExt, StreamExt};
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::{Map, Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue as WsHeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use super::transport::{
    Attempt, ChannelSink, HttpClass, StreamReadError, TurnSink, classify_http_status_retryable,
    for_each_sse_event, retry_after_hint, run_with_retry, spawn_async_stream, spawn_stream,
};
use crate::mimir::auth::openai_codex::{AccessToken, OpenAiCodexTokenStore};
use crate::mimir::selection::{CodexTransport, PromptCacheRetention, ReasoningEffort};
use crate::nexus::{
    AssistantTurn, ChatProvider, Message, ModelOrigin, ProviderCompactionCapability,
    ProviderCompactionFuture, ProviderCompactionOutput, ProviderStream, ProviderUsage,
    ReasoningBlock, Role, StructuredSummaryCapability, StructuredSummaryError,
    StructuredSummaryFuture, StructuredSummaryMode, ToolCall, Tools,
};

// Transport resilience for Codex requests. Transient failures (network, 429,
// 5xx) are retried with exponential backoff plus jitter; a single auth
// rejection (401/403) triggers one forced token refresh before retrying. The
// retry budget and backoff shape come from the shared
// [`RetryPolicy`](crate::mimir::retry::RetryPolicy), the single definition for
// every provider adapter.
const PROVIDER_ID: &str = "openai-codex";
const API_ID: &str = "openai-codex-responses";
static NATIVE_COMPACTION_UNSUPPORTED_MODELS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const WS_SEND_TIMEOUT: Duration = Duration::from_secs(10);
const WS_MAX_AGE: Duration = Duration::from_secs(55 * 60);
const WS_IDLE_TTL: Duration = Duration::from_secs(5 * 60);
type CodexWs = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WsFallback {
    RetryWebSocket,
    RetryFullWebSocket,
    ForceRefresh,
    FallbackSse,
    Fatal,
}

#[derive(Default)]
struct CodexWsState {
    socket: Option<ReusableCodexWs>,
    disabled_for_session: bool,
    continuation: Option<CodexContinuation>,
}

struct ReusableCodexWs {
    stream: CodexWs,
    opened_at: Instant,
    last_used: Instant,
}

#[derive(Debug, Clone)]
struct CodexContinuation {
    last_full_body: Value,
    last_response_id: String,
    last_response_items: Vec<Value>,
}

impl std::fmt::Debug for CodexWsState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexWsState")
            .field("socket", &self.socket.is_some())
            .field("disabled_for_session", &self.disabled_for_session)
            .field("continuation", &self.continuation)
            .finish()
    }
}

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
    codex_transport: CodexTransport,
    ws_state: Arc<tokio::sync::Mutex<CodexWsState>>,
}

impl OpenAiCodexResponsesProvider {
    /// Build the provider from the resolved model/base-url/reasoning selection.
    /// Selection precedence (`IRIS_MODEL`/`IRIS_CODEX_BASE_URL` -> settings ->
    /// default) now lives in `mimir::selection`, so the adapter just receives the
    /// resolved strings plus the optional reasoning level. `system_prompt` is the
    /// harness-assembled instruction string; the provider only forwards it into
    /// the request envelope.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        model: &str,
        base_url: &str,
        reasoning: Option<ReasoningEffort>,
        system_prompt: &str,
        prompt_cache_key: &str,
        cache_retention: PromptCacheRetention,
        retry_policy: crate::mimir::retry::RetryPolicy,
        codex_transport: CodexTransport,
    ) -> Result<Self> {
        Ok(Self {
            // Shared process-wide client: warm pooled connections (HTTP/2 +
            // keep-alive) survive across turns and model switches, so a turn
            // does not pay a fresh TLS handshake after an idle gap.
            client: super::transport::shared_client(),
            model: model.to_string(),
            base_url: base_url.to_string(),
            reasoning,
            system_prompt: system_prompt.to_string(),
            prompt_cache_key: prompt_cache_key.to_string(),
            cache_retention,
            cache_prefix: Arc::new(Mutex::new(super::PromptCachePrefix::default())),
            tokens: OpenAiCodexTokenStore::from_env()?,
            retry_policy,
            codex_transport,
            ws_state: Arc::new(tokio::sync::Mutex::new(CodexWsState::default())),
        })
    }

    #[cfg(test)]
    pub(crate) fn probe_v2_compaction(
        &self,
        messages: &[Message],
        cancel: &CancellationToken,
    ) -> Result<Value> {
        if !openai_native_probe_enabled(
            std::env::var("IRIS_OPENAI_NATIVE_COMPACTION_PROBE")
                .ok()
                .as_deref(),
        ) {
            bail!("OpenAI native compaction probe is disabled");
        }
        let request = build_codex_native_compaction_request(
            &self.model,
            &self.system_prompt,
            messages,
            self.cache_retention,
        );
        let token = self.tokens.access_token(&self.client)?;
        let response = self
            .client
            .post(resolve_codex_url(&self.base_url)?)
            .headers(codex_headers(&token)?)
            .json(&request)
            .send()
            .context("failed to send Codex native compaction probe")?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            let error_type = extract_error_field(&body, "type")
                .or_else(|| extract_error_field(&body, "code"))
                .unwrap_or_else(|| "unknown_error".to_string());
            bail!(
                "Codex native compaction probe failed [status={} endpoint=/codex/responses model={} error_type={error_type}]",
                status.as_u16(),
                self.model
            );
        }
        parse_codex_compaction_probe_reader(BufReader::new(response), &self.model, cancel)
            .map(|output| output.block)
    }

    /// Build a compaction-summary request over this provider's native
    /// structured-output transport (issue #475 / ADR-0061): `text.format`
    /// json_schema strict, `tools: []`, `text.verbosity:"low"`, and
    /// deliberately no top-level `max_output_tokens` (ADR-0061's live probe:
    /// the `/codex/responses` OAuth lane 400s on it with
    /// `Unsupported parameter: max_output_tokens`, unlike the OpenAI platform
    /// Responses API #475's literal JSON was modeled on). Token bounding
    /// relies on verbosity plus the model's own output cap instead. Pure
    /// request-building only: callers still send it through the existing
    /// `OpenAiCodexTokenStore`/`codex_headers`/`resolve_codex_url` path
    /// themselves.
    pub(crate) fn build_summary_request(&self, messages: &[Message]) -> Value {
        build_codex_summary_request(
            &self.model,
            &self.system_prompt,
            messages,
            self.cache_retention,
        )
    }

    /// Build the forced single virtual-tool (`emit_compaction_summary`)
    /// fallback summary request (issue #475), used only when the native path
    /// above is rejected as unsupported for this lane/model/auth kind. Iris
    /// never registers or executes this tool through normal tool
    /// approval/execution policy: it exists only inside this request builder
    /// and the matching `wayland::structured_summary` extraction path.
    pub(crate) fn build_summary_fallback_request(&self, messages: &[Message]) -> Value {
        build_codex_summary_fallback_request(
            &self.model,
            &self.system_prompt,
            messages,
            self.cache_retention,
        )
    }

    /// Send one structured-output compaction-summary request (issue #475) and
    /// return the resulting `AssistantTurn`, or a typed
    /// [`StructuredSummaryError`] the `wayland::compaction` fallback ladder
    /// dispatches on: `Unsupported` (a deterministic 400 that is not a
    /// context-overflow body -- the caller retries once with
    /// [`StructuredSummaryMode::ForcedTool`]), `Cancelled` (no further
    /// fallback), or `Other` (the caller falls back to deterministic
    /// excerpts). Mirrors [`Self::compact_context_blocking`]'s auth/retry loop
    /// exactly, reusing the same token store, reauth-once behavior, and
    /// [`classify_codex_native_failure`] classifier (its "400 and not a
    /// context-overflow body" rule is a generic transport signal, not
    /// specific to the native-compaction-trigger request shape).
    fn run_structured_summary_blocking(
        &self,
        messages: &[Message],
        mode: StructuredSummaryMode,
        cancel: &CancellationToken,
    ) -> std::result::Result<AssistantTurn, StructuredSummaryError> {
        let request = match mode {
            StructuredSummaryMode::Native => self.build_summary_request(messages),
            StructuredSummaryMode::ForcedTool => self.build_summary_fallback_request(messages),
        };
        let url = resolve_codex_url(&self.base_url).map_err(StructuredSummaryError::Other)?;
        let mut force_refresh = false;
        let mut reauth_used = false;
        let mut transient_retries = 0u32;
        loop {
            if cancel.is_cancelled() {
                return Err(StructuredSummaryError::Cancelled);
            }
            let token = if force_refresh {
                self.tokens.force_refresh(&self.client)
            } else {
                self.tokens.access_token(&self.client)
            }
            .map_err(StructuredSummaryError::Other)?;
            force_refresh = false;
            match self.send_structured_summary_once(url.clone(), &token, &request, cancel) {
                StructuredSummaryAttempt::Done(turn) => return Ok(turn),
                StructuredSummaryAttempt::Unsupported(error) => {
                    // Safe metadata only (status/error_type/model/endpoint;
                    // never the raw body or credentials -- see the message
                    // built in `send_structured_summary_once`).
                    tracing::debug!(
                        error = %format!("{error:#}"),
                        "structured-output compaction summary rejected as unsupported"
                    );
                    return Err(StructuredSummaryError::Unsupported);
                }
                StructuredSummaryAttempt::Reauth(error) if !reauth_used => {
                    reauth_used = true;
                    force_refresh = true;
                    tracing::warn!(
                        error = %format!("{error:#}"),
                        "structured-output compaction summary auth rejected; refreshing once"
                    );
                }
                StructuredSummaryAttempt::Retry(error, retry_after)
                    if transient_retries < self.retry_policy.max_retries =>
                {
                    transient_retries += 1;
                    let delay = self
                        .retry_policy
                        .backoff_delay(transient_retries, retry_after);
                    tracing::warn!(
                        error = %format!("{error:#}"),
                        attempt = transient_retries,
                        delay_ms = delay.as_millis() as u64,
                        "structured-output compaction summary transient error; retrying"
                    );
                    sleep_codex_native_retry(delay, cancel);
                }
                StructuredSummaryAttempt::Reauth(error)
                | StructuredSummaryAttempt::Retry(error, _)
                | StructuredSummaryAttempt::Fatal(error) => {
                    return Err(StructuredSummaryError::Other(error));
                }
            }
        }
    }

    fn send_structured_summary_once(
        &self,
        url: Url,
        token: &AccessToken,
        request: &Value,
        cancel: &CancellationToken,
    ) -> StructuredSummaryAttempt {
        let headers = match codex_headers(token) {
            Ok(headers) => headers,
            Err(error) => return StructuredSummaryAttempt::Fatal(error),
        };
        let response = match self.client.post(url).headers(headers).json(request).send() {
            Ok(response) => response,
            Err(error) => {
                return StructuredSummaryAttempt::Retry(
                    anyhow::Error::new(error)
                        .context("failed to send Codex structured-summary request"),
                    None,
                );
            }
        };
        let status = response.status();
        if status.is_success() {
            let mut parser = ResponseStreamParser::new(&self.model);
            let mut sink = DiscardTextSink;
            if let Err(error) = for_each_sse_event(BufReader::new(response), cancel, |data| {
                sink.on_activity()?;
                parser.ingest_event(data, &mut sink)
            }) {
                if !cancel.is_cancelled()
                    && protocol_anomaly_retryable(&error, parser.emitted_visible_output())
                {
                    return StructuredSummaryAttempt::Retry(error, None);
                }
                return StructuredSummaryAttempt::Fatal(error);
            }
            let emitted_visible_output = parser.emitted_visible_output();
            return match parser.finish() {
                Ok(turn) => {
                    if let Some(usage) = &turn.usage {
                        self.record_usage(usage);
                    }
                    StructuredSummaryAttempt::Done(turn)
                }
                Err(error) => {
                    if protocol_anomaly_retryable(&error, emitted_visible_output) {
                        StructuredSummaryAttempt::Retry(error, None)
                    } else {
                        StructuredSummaryAttempt::Fatal(error)
                    }
                }
            };
        }
        let retry_after = retry_after_hint(response.headers());
        let body = response.text().unwrap_or_default();
        let error_type = extract_error_field(&body, "type")
            .or_else(|| extract_error_field(&body, "code"))
            .unwrap_or_else(|| "unknown_error".to_string());
        let error = anyhow!(
            "Codex structured-summary request failed [status={} endpoint=/codex/responses model={} error_type={error_type}]",
            status.as_u16(),
            self.model
        );
        match classify_codex_native_failure(status.as_u16(), &body) {
            CodexNativeFailure::Unsupported => StructuredSummaryAttempt::Unsupported(error),
            CodexNativeFailure::Reauth => StructuredSummaryAttempt::Reauth(error),
            CodexNativeFailure::Retry => StructuredSummaryAttempt::Retry(error, retry_after),
            CodexNativeFailure::Fatal => StructuredSummaryAttempt::Fatal(error),
        }
    }

    /// LIVE capability probe for #475: send one structured-output summary
    /// request over the real Codex OAuth lane and report whether the lane
    /// honoured it. Reuses the production token store, headers, endpoint,
    /// request builders (above), and SSE parser so the wire request matches
    /// what the compaction summarizer would send. `ProbeMode::Native` sets
    /// `text.format` json_schema; `ProbeMode::ForcedTool` sends the single
    /// forced `emit_compaction_summary` tool. Never executes any tool.
    #[cfg(test)]
    pub(crate) fn probe_compaction_summary(
        &self,
        mode: crate::structured_summary_probe::ProbeMode,
        cancel: &CancellationToken,
    ) -> Result<crate::structured_summary_probe::ProbeOutcome> {
        use crate::structured_summary_probe::{
            ProbeMode, ProbeOutcome, VIRTUAL_TOOL_NAME, toy_transcript,
        };
        let lane = format!("openai-codex/{}", self.model);
        let messages = vec![Message::user(&toy_transcript())];
        // NOTE: the ChatGPT backend-api `/codex/responses` lane rejects
        // `max_output_tokens` (`400 Unsupported parameter`), unlike the OpenAI
        // platform Responses API that #475 modeled. `build_summary_request`
        // follows production and omits it (see its doc comment).
        let request = match mode {
            ProbeMode::Native => self.build_summary_request(&messages),
            ProbeMode::ForcedTool => self.build_summary_fallback_request(&messages),
        };

        let token = self.tokens.access_token(&self.client)?;
        let response = self
            .client
            .post(resolve_codex_url(&self.base_url)?)
            .headers(codex_headers(&token)?)
            .json(&request)
            .send()
            .context("failed to send Codex structured-summary probe")?;
        let status = response.status();
        let body = response.text().unwrap_or_default();
        if !status.is_success() {
            let error_type =
                extract_error_field(&body, "type").or_else(|| extract_error_field(&body, "code"));
            let error_message = extract_error_field(&body, "message");
            return Ok(ProbeOutcome::rejected(
                lane,
                &self.model,
                mode,
                status.as_u16(),
                error_type,
                error_message,
                &body,
            ));
        }
        let turn = parse_response_stream(&body)?;
        let summary = match mode {
            ProbeMode::Native => turn
                .text
                .as_deref()
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .and_then(|text| serde_json::from_str::<Value>(text).ok()),
            ProbeMode::ForcedTool => turn
                .tool_calls
                .iter()
                .find(|call| call.name == VIRTUAL_TOOL_NAME)
                .map(|call| call.arguments.clone()),
        };
        let _ = cancel;
        Ok(ProbeOutcome::succeeded(
            lane,
            &self.model,
            mode,
            status.as_u16(),
            summary,
        ))
    }

    fn compact_context_blocking(
        &self,
        messages: &[Message],
        instructions: &str,
        cancel: &CancellationToken,
    ) -> Result<ProviderCompactionOutput> {
        if cancel.is_cancelled() {
            bail!("provider-native compaction cancelled");
        }
        let request = build_codex_native_compaction_request(
            &self.model,
            &self.system_prompt,
            messages,
            self.cache_retention,
        );
        let native = self.run_native_compaction_request(&request, cancel)?;
        if let Some(usage) = &native.usage {
            self.record_usage(usage);
        }

        // Codex compaction items are intentionally opaque and carry no portable
        // text. Iris therefore pays for a second inference call so cross-model
        // resume retains the provider-independent summary required by ADR-0056.
        // The summary directive must be the final user turn: when it only rides
        // in the system instructions and the covered transcript ends on an
        // unanswered user message, the model answers that message instead of
        // summarizing.
        let summary_messages = native_compaction_summary_messages(messages, instructions);
        let summary_request = build_codex_request(
            &self.model,
            &self.system_prompt,
            &summary_messages,
            &Tools::new(Vec::new()),
            self.reasoning,
            Some(&self.prompt_cache_key),
            None,
            self.cache_retention,
        );
        let mut sink = DiscardTextSink;
        let turn = self.run_blocking(
            resolve_codex_url(&self.base_url)?,
            &summary_request,
            &mut sink,
            cancel,
        )?;
        let summary = turn
            .text
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| anyhow!("Codex native compaction returned empty portable text"))?;
        let usage = merge_openai_compaction_usage(native.usage, turn.usage);
        Ok(ProviderCompactionOutput {
            summary,
            provider_blocks: vec![native.block],
            usage,
        })
    }

    fn run_native_compaction_request(
        &self,
        request: &Value,
        cancel: &CancellationToken,
    ) -> Result<CodexNativeCompaction> {
        let url = resolve_codex_url(&self.base_url)?;
        let mut force_refresh = false;
        let mut reauth_used = false;
        let mut transient_retries = 0u32;
        loop {
            if cancel.is_cancelled() {
                bail!("provider-native compaction cancelled");
            }
            let token = if force_refresh {
                self.tokens.force_refresh(&self.client)
            } else {
                self.tokens.access_token(&self.client)
            }?;
            force_refresh = false;
            match self.send_native_compaction_once(url.clone(), &token, request, cancel) {
                CodexNativeCompactionAttempt::Done(output) => return Ok(output),
                CodexNativeCompactionAttempt::Unsupported(error) => {
                    NATIVE_COMPACTION_UNSUPPORTED_MODELS
                        .get_or_init(|| Mutex::new(HashSet::new()))
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .insert(self.model.clone());
                    return Err(error);
                }
                CodexNativeCompactionAttempt::Reauth(error) if !reauth_used => {
                    reauth_used = true;
                    force_refresh = true;
                    tracing::warn!(
                        error = %format!("{error:#}"),
                        "Codex native compaction auth rejected; refreshing once"
                    );
                }
                CodexNativeCompactionAttempt::Retry(error, retry_after)
                    if transient_retries < self.retry_policy.max_retries =>
                {
                    transient_retries += 1;
                    let delay = self
                        .retry_policy
                        .backoff_delay(transient_retries, retry_after);
                    tracing::warn!(
                        error = %format!("{error:#}"),
                        attempt = transient_retries,
                        delay_ms = delay.as_millis() as u64,
                        "Codex native compaction transient error; retrying"
                    );
                    sleep_codex_native_retry(delay, cancel);
                }
                CodexNativeCompactionAttempt::Reauth(error)
                | CodexNativeCompactionAttempt::Retry(error, _)
                | CodexNativeCompactionAttempt::Fatal(error) => return Err(error),
            }
        }
    }

    fn send_native_compaction_once(
        &self,
        url: Url,
        token: &AccessToken,
        request: &Value,
        cancel: &CancellationToken,
    ) -> CodexNativeCompactionAttempt {
        let headers = match codex_headers(token) {
            Ok(headers) => headers,
            Err(error) => return CodexNativeCompactionAttempt::Fatal(error),
        };
        let response = match self.client.post(url).headers(headers).json(request).send() {
            Ok(response) => response,
            Err(error) => {
                return CodexNativeCompactionAttempt::Retry(
                    anyhow::Error::new(error)
                        .context("failed to send Codex native compaction request"),
                    None,
                );
            }
        };
        let status = response.status();
        if status.is_success() {
            return match parse_codex_compaction_probe_reader(
                BufReader::new(response),
                &self.model,
                cancel,
            ) {
                Ok(output) => CodexNativeCompactionAttempt::Done(output),
                Err(error) => CodexNativeCompactionAttempt::Retry(error, None),
            };
        }
        let retry_after = retry_after_hint(response.headers());
        let body = response.text().unwrap_or_default();
        let error_type = extract_error_field(&body, "type")
            .or_else(|| extract_error_field(&body, "code"))
            .unwrap_or_else(|| "unknown_error".to_string());
        let error = anyhow!(
            "Codex native compaction failed [status={} endpoint=/codex/responses model={} error_type={error_type}]",
            status.as_u16(),
            self.model
        );
        match classify_codex_native_failure(status.as_u16(), &body) {
            CodexNativeFailure::Unsupported => CodexNativeCompactionAttempt::Unsupported(error),
            CodexNativeFailure::Reauth => CodexNativeCompactionAttempt::Reauth(error),
            CodexNativeFailure::Retry => CodexNativeCompactionAttempt::Retry(error, retry_after),
            CodexNativeFailure::Fatal => CodexNativeCompactionAttempt::Fatal(error),
        }
    }
}

struct CodexNativeCompaction {
    block: Value,
    usage: Option<ProviderUsage>,
}

enum CodexNativeCompactionAttempt {
    Done(CodexNativeCompaction),
    Unsupported(anyhow::Error),
    Reauth(anyhow::Error),
    Retry(anyhow::Error, Option<Duration>),
    Fatal(anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexNativeFailure {
    Unsupported,
    Reauth,
    Retry,
    Fatal,
}

/// One structured-output compaction-summary send attempt (issue #475).
/// Mirrors [`CodexNativeCompactionAttempt`] but carries a full `AssistantTurn`
/// on success (the summary payload rides in ordinary text/tool-call fields,
/// not an opaque compaction block).
enum StructuredSummaryAttempt {
    Done(AssistantTurn),
    Unsupported(anyhow::Error),
    Reauth(anyhow::Error),
    Retry(anyhow::Error, Option<Duration>),
    Fatal(anyhow::Error),
}

fn classify_codex_native_failure(status: u16, body: &str) -> CodexNativeFailure {
    if status == 400 && !super::is_context_overflow_response(status, body) {
        return CodexNativeFailure::Unsupported;
    }
    match classify_http_status_retryable(status) {
        HttpClass::Reauth => CodexNativeFailure::Reauth,
        HttpClass::Retry => CodexNativeFailure::Retry,
        HttpClass::Fatal => CodexNativeFailure::Fatal,
    }
}

fn sleep_codex_native_retry(delay: Duration, cancel: &CancellationToken) {
    let slice = Duration::from_millis(50);
    let started = Instant::now();
    while !cancel.is_cancelled() && started.elapsed() < delay {
        std::thread::sleep(slice.min(delay.saturating_sub(started.elapsed())));
    }
}

fn merge_openai_compaction_usage(
    native: Option<ProviderUsage>,
    summary: Option<ProviderUsage>,
) -> Option<ProviderUsage> {
    match (native, summary) {
        (Some(mut total), Some(summary)) => {
            total.input_tokens = total.input_tokens.saturating_add(summary.input_tokens);
            total.output_tokens = total.output_tokens.saturating_add(summary.output_tokens);
            total.cache_read_input_tokens = total
                .cache_read_input_tokens
                .saturating_add(summary.cache_read_input_tokens);
            total.cache_write_input_tokens = total
                .cache_write_input_tokens
                .saturating_add(summary.cache_write_input_tokens);
            total.reasoning_output_tokens = total
                .reasoning_output_tokens
                .saturating_add(summary.reasoning_output_tokens);
            total.total_tokens = total.total_tokens.saturating_add(summary.total_tokens);
            Some(total)
        }
        (native, summary) => native.or(summary),
    }
}

struct DiscardTextSink;

impl TurnSink for DiscardTextSink {
    fn on_text_delta(&mut self, _delta: &str) -> Result<()> {
        Ok(())
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
        let request_for_stream = request.clone();
        let url = resolve_codex_url(&self.base_url)?;
        let provider = self.clone();
        let cancel = cancel.clone();
        if self.codex_transport == CodexTransport::Sse {
            return Ok(spawn_stream(
                move |sink, cancel| provider.run_blocking(url, &request_for_stream, sink, cancel),
                cancel,
            ));
        }
        Ok(spawn_async_stream(
            move |sink, cancel| async move { provider.run_auto(url, request, sink, cancel).await },
            cancel,
        ))
    }

    fn compaction_capability(&self, _input_tokens: u64) -> ProviderCompactionCapability {
        // Unlike Anthropic's compact beta, Codex advertises no provider-side
        // minimum input floor. Wayland's model-aware start/hard ladder decides
        // when the request is economical; Mimir only caches proven rejection.
        codex_native_compaction_capability(&self.model)
    }

    fn compact_context<'a>(
        &'a self,
        messages: &'a [Message],
        instructions: &'a str,
        cancel: &'a CancellationToken,
    ) -> ProviderCompactionFuture<'a> {
        Box::pin(async move { self.compact_context_blocking(messages, instructions, cancel) })
    }

    fn structured_summary_capability(&self) -> StructuredSummaryCapability {
        // ADR-0061's live probe: the Codex OAuth lane honours native
        // structured output on the first request for the probed model. No
        // per-model unsupported cache (unlike `compaction_capability` above)
        // -- the fallback ladder already retries once with the forced tool
        // per job, and #475 does not require cross-job memoization.
        StructuredSummaryCapability::Native
    }

    fn run_structured_summary<'a>(
        &'a self,
        messages: &'a [Message],
        mode: StructuredSummaryMode,
        cancel: &'a CancellationToken,
    ) -> StructuredSummaryFuture<'a> {
        Box::pin(async move { self.run_structured_summary_blocking(messages, mode, cancel) })
    }
}

fn codex_native_compaction_capability(model: &str) -> ProviderCompactionCapability {
    let unsupported = NATIVE_COMPACTION_UNSUPPORTED_MODELS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .contains(model);
    if unsupported {
        ProviderCompactionCapability::None
    } else {
        ProviderCompactionCapability::OpaqueBlocks
    }
}

impl OpenAiCodexResponsesProvider {
    async fn run_auto(
        &self,
        url: Url,
        full_request: Value,
        mut sink: ChannelSink,
        cancel: CancellationToken,
    ) -> Result<AssistantTurn> {
        if self.ws_state.lock().await.disabled_for_session {
            return self
                .run_blocking_off_thread(url, full_request, sink, cancel)
                .await;
        }
        let ws_url = resolve_codex_ws_url_from_resolved(&url)?;
        let mut tried_limit_refresh = false;
        let mut tried_previous_full = false;
        let mut tried_reauth = false;
        let mut force_full = false;
        loop {
            let token = self.codex_token_off_thread(tried_reauth).await?;
            match self
                .run_ws_once(
                    ws_url.clone(),
                    &full_request,
                    &token,
                    &mut sink,
                    &cancel,
                    force_full,
                )
                .await
            {
                Ok(turn) => return Ok(turn),
                Err((policy, error)) => match policy {
                    WsFallback::RetryWebSocket if !tried_limit_refresh => {
                        tried_limit_refresh = true;
                        self.drop_ws_socket().await;
                    }
                    WsFallback::RetryFullWebSocket if !tried_previous_full => {
                        tried_previous_full = true;
                        force_full = true;
                        self.clear_continuation().await;
                    }
                    WsFallback::ForceRefresh if !tried_reauth => {
                        tried_reauth = true;
                        self.drop_ws_socket().await;
                    }
                    WsFallback::FallbackSse => {
                        self.disable_ws_for_session().await;
                        return self
                            .run_blocking_off_thread(url, full_request, sink, cancel)
                            .await;
                    }
                    _ => return Err(error),
                },
            }
        }
    }

    async fn codex_token_off_thread(&self, force_refresh: bool) -> Result<AccessToken> {
        let tokens = self.tokens.clone();
        let client = self.client.clone();
        tokio::task::spawn_blocking(move || {
            if force_refresh {
                tokens.force_refresh(&client)
            } else {
                tokens.access_token(&client)
            }
        })
        .await
        .context("Codex token task failed")?
    }

    async fn run_blocking_off_thread(
        &self,
        url: Url,
        request: Value,
        sink: ChannelSink,
        cancel: CancellationToken,
    ) -> Result<AssistantTurn> {
        let provider = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut sink = sink;
            provider.run_blocking(url, &request, &mut sink, &cancel)
        })
        .await
        .context("Codex SSE fallback task failed")?
    }

    async fn run_ws_once(
        &self,
        ws_url: Url,
        full_request: &Value,
        token: &AccessToken,
        sink: &mut dyn TurnSink,
        cancel: &CancellationToken,
        force_full: bool,
    ) -> std::result::Result<AssistantTurn, (WsFallback, anyhow::Error)> {
        let frame = {
            let state = self.ws_state.lock().await;
            build_ws_create_frame(full_request, state.continuation.as_ref(), force_full)
        };
        let mut reusable = match self.take_ws_socket().await {
            Some(socket) => socket,
            None => ReusableCodexWs {
                stream: await_ws_setup(
                    "connect",
                    WS_CONNECT_TIMEOUT,
                    cancel,
                    false,
                    connect_codex_ws(ws_url, token, &self.prompt_cache_key),
                )
                .await?,
                opened_at: Instant::now(),
                last_used: Instant::now(),
            },
        };
        let text =
            serde_json::to_string(&frame).map_err(|error| (WsFallback::Fatal, error.into()))?;
        await_ws_setup("send", WS_SEND_TIMEOUT, cancel, false, async {
            reusable
                .stream
                .send(WsMessage::Text(text.into()))
                .await
                .map_err(|error| {
                    anyhow!(
                        "Codex WebSocket send failed: {}",
                        safe_transport_error(&error)
                    )
                })
        })
        .await?;

        let mut parser = ResponseStreamParser::new(&self.model);
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => {
                    self.clear_continuation().await;
                    return Err((WsFallback::Fatal, anyhow!("Codex WebSocket request cancelled")));
                }
                message = reusable.stream.next() => message,
            };
            let Some(message) = next else {
                let error: anyhow::Error = CodexStreamProtocolAnomaly::closed_before_completed(
                    parser.last_event_type.clone(),
                )
                .into();
                return Err((
                    classify_ws_error(&error, parser.emitted_visible_output()),
                    error,
                ));
            };
            match message {
                Ok(WsMessage::Text(text)) => {
                    sink.on_activity()
                        .map_err(|error| (WsFallback::Fatal, error))?;
                    if let Err(error) = parser.ingest_event(&text, sink) {
                        let policy = classify_ws_error(&error, parser.emitted_visible_output());
                        return Err((policy, error));
                    }
                }
                Ok(WsMessage::Binary(bytes)) => {
                    sink.on_activity()
                        .map_err(|error| (WsFallback::Fatal, error))?;
                    let text = String::from_utf8(bytes.to_vec()).map_err(|_| {
                        let error: anyhow::Error = CodexStreamProtocolAnomaly::invalid_json(
                            parser.last_event_type.clone(),
                        )
                        .into();
                        (
                            classify_ws_error(&error, parser.emitted_visible_output()),
                            error,
                        )
                    })?;
                    if let Err(error) = parser.ingest_event(&text, sink) {
                        let policy = classify_ws_error(&error, parser.emitted_visible_output());
                        return Err((policy, error));
                    }
                }
                Ok(WsMessage::Ping(payload)) => {
                    sink.on_activity()
                        .map_err(|error| (WsFallback::Fatal, error))?;
                    await_ws_setup(
                        "pong",
                        WS_SEND_TIMEOUT,
                        cancel,
                        parser.emitted_visible_output(),
                        async {
                            reusable
                                .stream
                                .send(WsMessage::Pong(payload))
                                .await
                                .map_err(|error| {
                                    anyhow!(
                                        "Codex WebSocket pong failed: {}",
                                        safe_transport_error(&error)
                                    )
                                })
                        },
                    )
                    .await?;
                }
                Ok(WsMessage::Pong(_)) => {
                    sink.on_activity()
                        .map_err(|error| (WsFallback::Fatal, error))?;
                }
                Ok(WsMessage::Close(_)) => {
                    let error: anyhow::Error = CodexStreamProtocolAnomaly::closed_before_completed(
                        parser.last_event_type.clone(),
                    )
                    .into();
                    return Err((
                        classify_ws_error(&error, parser.emitted_visible_output()),
                        error,
                    ));
                }
                Ok(WsMessage::Frame(_)) => {}
                Err(error) => {
                    let error = anyhow!(
                        "Codex WebSocket read failed: {}",
                        safe_transport_error(&error)
                    );
                    let policy = classify_ws_error(&error, parser.emitted_visible_output());
                    return Err((policy, error));
                }
            }
            if parser.saw_completed {
                break;
            }
        }
        let completed_response = parser.completed_response.clone();
        let emitted_visible = parser.emitted_visible_output();
        let turn = parser
            .finish()
            .map_err(|error| (classify_ws_error(&error, emitted_visible), error))?;
        if let Some(usage) = &turn.usage {
            self.record_usage(usage);
        }
        if let Some(response) = completed_response.as_ref()
            && let Some(id) = turn.response_id.as_deref()
        {
            self.update_continuation(full_request, response, id).await;
        } else {
            self.clear_continuation().await;
        }
        reusable.last_used = Instant::now();
        self.put_ws_socket(reusable).await;
        Ok(turn)
    }

    async fn take_ws_socket(&self) -> Option<ReusableCodexWs> {
        let mut state = self.ws_state.lock().await;
        let reusable = state.socket.take()?;
        let now = Instant::now();
        (now.duration_since(reusable.opened_at) < WS_MAX_AGE
            && now.duration_since(reusable.last_used) < WS_IDLE_TTL)
            .then_some(reusable)
    }

    async fn put_ws_socket(&self, reusable: ReusableCodexWs) {
        self.ws_state.lock().await.socket = Some(reusable);
    }

    async fn drop_ws_socket(&self) {
        self.ws_state.lock().await.socket = None;
    }

    async fn clear_continuation(&self) {
        self.ws_state.lock().await.continuation = None;
    }

    async fn disable_ws_for_session(&self) {
        let mut state = self.ws_state.lock().await;
        state.disabled_for_session = true;
        state.socket = None;
        state.continuation = None;
    }

    async fn update_continuation(&self, full_request: &Value, response: &Value, id: &str) {
        let Some(items) = normalize_response_items_for_continuation(response) else {
            self.clear_continuation().await;
            return;
        };
        self.ws_state.lock().await.continuation = Some(CodexContinuation {
            last_full_body: ws_body_from_full_request(full_request),
            last_response_id: id.to_string(),
            last_response_items: items,
        });
    }

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
            sink,
            |force_refresh| {
                if force_refresh {
                    self.tokens.force_refresh(&self.client)
                } else {
                    self.tokens.access_token(&self.client)
                }
            },
            |token, sink| self.send_once(url.clone(), token, request, sink, cancel),
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
                sink.on_activity()?;
                parser.ingest_event(data, sink)
            }) {
                if !cancel.is_cancelled()
                    && protocol_anomaly_retryable(&error, parser.emitted_visible_output())
                {
                    return Attempt::Retry(error, None);
                }
                return Attempt::Fatal(error);
            }
            let emitted_visible_output = parser.emitted_visible_output();
            return match parser.finish() {
                Ok(turn) => {
                    if let Some(usage) = &turn.usage {
                        self.record_usage(usage);
                    }
                    Attempt::Done(Box::new(turn))
                }
                Err(error) => {
                    if protocol_anomaly_retryable(&error, emitted_visible_output) {
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
        let error = super::classified_http_error(
            status.as_u16(),
            &body,
            format!("Codex request failed [{diag}]"),
        );
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
        if let Some(key) = prompt_cache_key.and_then(super::clamp_openai_prompt_cache_key) {
            body["prompt_cache_key"] = json!(key);
        }
        // Long retention opts into the 24h prompt-cache lifetime (pi-mono
        // `getPromptCacheRetention`); short/none leave the default in-memory
        // (~minutes) lifetime, so no field is sent.
        if cache_retention == PromptCacheRetention::Long {
            body["prompt_cache_retention"] = json!("24h");
        }
    }
    // `previous_response_id` normally requires server-side storage. The
    // WebSocket path is the exception: OpenAI's WebSocket Mode guide permits it
    // with `store:false` while the referenced response remains in the active
    // connection-local cache. HTTP/SSE production requests still pass `None` and
    // send full context.
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
    if let Some(reasoning) = codex_reasoning(model, reasoning) {
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

/// Build a compaction-summary request using OpenAI's native structured-
/// output transport (issue #475 / ADR-0061): `text.format` json_schema
/// strict, `tools: []`, `text.verbosity:"low"`, and deliberately no top-level
/// `max_output_tokens` -- see [`OpenAiCodexResponsesProvider::build_summary_request`]'s
/// doc comment for why. Built on [`build_codex_request`] so OAuth-lane
/// request shaping (store/stream, instructions, input, prompt-cache key,
/// reasoning) is unchanged; only `tools`/`text` are overridden.
fn build_codex_summary_request(
    model: &str,
    instructions: &str,
    messages: &[Message],
    cache_retention: PromptCacheRetention,
) -> Value {
    let mut request = build_codex_request(
        model,
        instructions,
        messages,
        &Tools::new(Vec::new()),
        None,
        None,
        None,
        cache_retention,
    );
    request["tools"] = json!([]);
    request["text"] = json!({
        "verbosity": "low",
        "format": {
            "type": "json_schema",
            "name": "compaction_summary",
            "strict": true,
            "schema": crate::wayland::structured_summary::canonical_compaction_schema(),
        }
    });
    request
}

/// Build the forced single virtual-tool (`emit_compaction_summary`) fallback
/// summary request (issue #475): the compatibility fallback used only when
/// the native path above is rejected as unsupported for the active
/// lane/model/auth kind. Built on [`build_codex_request`] the same way as
/// [`build_codex_summary_request`]; only `tools`/`tool_choice` are
/// overridden. Iris never registers or executes this tool through normal
/// tool approval/execution policy -- it exists only inside this request
/// builder and the matching `wayland::structured_summary` extraction path.
fn build_codex_summary_fallback_request(
    model: &str,
    instructions: &str,
    messages: &[Message],
    cache_retention: PromptCacheRetention,
) -> Value {
    let mut request = build_codex_request(
        model,
        instructions,
        messages,
        &Tools::new(Vec::new()),
        None,
        None,
        None,
        cache_retention,
    );
    request["tools"] = json!([{
        "type": "function",
        "name": crate::wayland::structured_summary::VIRTUAL_TOOL_NAME,
        "description": "Return the compaction summary.",
        "parameters": crate::wayland::structured_summary::canonical_compaction_schema(),
        "strict": true,
    }]);
    request["tool_choice"] = json!({
        "type": "function",
        "name": crate::wayland::structured_summary::VIRTUAL_TOOL_NAME,
    });
    request
}

#[cfg(test)]
fn openai_native_probe_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| matches!(value.trim(), "1" | "true" | "on"))
}

fn build_codex_native_compaction_request(
    model: &str,
    instructions: &str,
    messages: &[Message],
    cache_retention: PromptCacheRetention,
) -> Value {
    let mut body = build_codex_request(
        model,
        instructions,
        messages,
        &Tools::new(Vec::new()),
        None,
        None,
        None,
        cache_retention,
    );
    body["input"]
        .as_array_mut()
        .expect("request input is an array")
        .push(json!({ "type": "compaction_trigger" }));
    body
}

/// Covered transcript plus the summary directive as the final user turn. The
/// directive must not ride only in the system instructions: when the covered
/// range ends on an unanswered user message, the model answers it instead of
/// summarizing.
fn native_compaction_summary_messages(messages: &[Message], instructions: &str) -> Vec<Message> {
    let mut out = messages.to_vec();
    out.push(Message::user(&native_compaction_summary_instructions(
        instructions,
    )));
    out
}

fn native_compaction_summary_instructions(instructions: &str) -> String {
    let instructions = instructions.trim();
    let base = "Write a self-contained handoff summary of the supplied transcript. Preserve the goal, completed work, decisions, constraints, exact identifiers and paths, failures, and next steps. Respond with summary text only and do not call tools.";
    if instructions.is_empty() {
        base.to_string()
    } else {
        format!("{base} Additional focus: {instructions}")
    }
}

fn extract_codex_compaction_block(response: &Value, model: &str) -> Option<Value> {
    let item = response
        .get("output")
        .and_then(Value::as_array)?
        .iter()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))?;
    let encrypted = item.get("encrypted_content").and_then(Value::as_str)?;
    Some(json!({
        "adapter": API_ID,
        "model": model,
        "block": { "type": "compaction", "encrypted_content": encrypted }
    }))
}

fn parse_codex_compaction_probe_reader(
    reader: impl BufRead,
    model: &str,
    cancel: &CancellationToken,
) -> Result<CodexNativeCompaction> {
    let mut blocks = Vec::new();
    let mut usage = None;
    for_each_sse_event(reader, cancel, |data| {
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let event: Value = serde_json::from_str(data)
            .map_err(|_| anyhow!("Codex native compaction probe returned invalid SSE JSON"))?;
        if event.get("type").and_then(Value::as_str) == Some("response.output_item.done")
            && let Some(item) = event.get("item")
            && let Some(block) =
                extract_codex_compaction_block(&json!({ "output": [item.clone()] }), model)
            && !blocks.contains(&block)
        {
            blocks.push(block);
        }
        if matches!(
            event.get("type").and_then(Value::as_str),
            Some("response.completed" | "response.done")
        ) && let Some(response) = event.get("response")
        {
            if let Some(block) = extract_codex_compaction_block(response, model)
                && !blocks.contains(&block)
            {
                blocks.push(block);
            }
            usage = extract_openai_usage(response, model);
        }
        Ok(())
    })?;
    match blocks.len() {
        1 => Ok(CodexNativeCompaction {
            block: blocks.pop().expect("one checked block"),
            usage,
        }),
        count => bail!(
            "Codex native compaction probe returned {count} opaque blocks; expected exactly one"
        ),
    }
}

/// Render the model capability's typed OpenAI Responses reasoning shape.
fn codex_reasoning(model: &str, reasoning: Option<ReasoningEffort>) -> Option<Value> {
    let crate::mimir::model_capabilities::ReasoningWire::OpenAiResponses { effort, summary } =
        crate::mimir::model_capabilities::wire_config(
            crate::mimir::selection::ProviderId::OpenAiCodex,
            model,
            reasoning?,
        )?
    else {
        return None;
    };
    Some(json!({ "effort": effort, "summary": summary }))
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
        Role::Developer | Role::User | Role::Assistant => json!({
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

fn openai_origin(model: &str) -> ModelOrigin {
    ModelOrigin::new(PROVIDER_ID, API_ID, model)
}

fn message_content_type(role: Role) -> &'static str {
    match role {
        Role::Developer => "input_text",
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

fn resolve_codex_ws_url_from_resolved(http_url: &Url) -> Result<Url> {
    let mut url = http_url.clone();
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => bail!("unsupported Codex WebSocket base URL scheme: {other}"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow!("failed to derive Codex WebSocket URL"))?;
    Ok(url)
}

#[cfg(test)]
fn resolve_codex_ws_url(base_url: &str) -> Result<Url> {
    resolve_codex_ws_url_from_resolved(&resolve_codex_url(base_url)?)
}

fn ws_body_from_full_request(full_request: &Value) -> Value {
    let mut body = full_request.clone();
    if let Some(object) = body.as_object_mut() {
        object.remove("stream");
        object.remove("background");
    }
    body
}

fn build_ws_create_frame(
    full_request: &Value,
    continuation: Option<&CodexContinuation>,
    force_full: bool,
) -> Value {
    let mut body = ws_body_from_full_request(full_request);
    if !force_full
        && let Some(continuation) = continuation
        && let Some(delta) = continuation_delta(&body, continuation)
        && let Some(object) = body.as_object_mut()
    {
        object.insert("input".to_string(), Value::Array(delta));
        object.insert(
            "previous_response_id".to_string(),
            Value::String(continuation.last_response_id.clone()),
        );
    }
    let mut frame = Map::new();
    frame.insert(
        "type".to_string(),
        Value::String("response.create".to_string()),
    );
    if let Some(object) = body.as_object() {
        frame.extend(object.clone());
    }
    Value::Object(frame)
}

fn continuation_delta(
    current_body: &Value,
    continuation: &CodexContinuation,
) -> Option<Vec<Value>> {
    if !same_continuation_shape(current_body, &continuation.last_full_body) {
        return None;
    }
    let current = current_body.get("input")?.as_array()?;
    let previous = continuation.last_full_body.get("input")?.as_array()?;
    let mut expected = previous.clone();
    expected.extend(continuation.last_response_items.clone());
    current
        .starts_with(&expected)
        .then(|| current[expected.len()..].to_vec())
}

fn same_continuation_shape(current: &Value, previous: &Value) -> bool {
    fn without_input_and_previous(value: &Value) -> Value {
        let mut value = value.clone();
        if let Some(object) = value.as_object_mut() {
            object.remove("input");
            object.remove("previous_response_id");
        }
        value
    }
    without_input_and_previous(current) == without_input_and_previous(previous)
}

fn normalize_response_items_for_continuation(response: &Value) -> Option<Vec<Value>> {
    let output = response.get("output")?.as_array()?;
    let mut items = Vec::new();
    for item in output {
        if let Some(item) = normalize_response_item_for_continuation(item)? {
            items.push(item);
        }
    }
    Some(items)
}

fn normalize_response_item_for_continuation(item: &Value) -> Option<Option<Value>> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => normalize_message_for_continuation(item).map(Some),
        Some("function_call") => normalize_function_call_for_continuation(item).map(Some),
        Some("reasoning") => Some(normalize_reasoning_for_continuation(item)),
        Some("function_call_output") => None,
        Some(_) | None => None,
    }
}

fn normalize_message_for_continuation(item: &Value) -> Option<Value> {
    let role = item
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("assistant");
    if role != "assistant" {
        return None;
    }
    let text = extract_output_text(item);
    (!text.is_empty()).then(|| {
        json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": text }],
        })
    })
}

fn normalize_function_call_for_continuation(item: &Value) -> Option<Value> {
    (item.get("type").and_then(Value::as_str) == Some("function_call")).then(|| {
        let arguments = item
            .get("arguments")
            .and_then(parse_arguments)
            .unwrap_or_else(|| json!({}))
            .to_string();
        json!({
            "type": "function_call",
            "call_id": item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default(),
            "name": item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            "arguments": arguments,
        })
    })
}

fn normalize_reasoning_for_continuation(item: &Value) -> Option<Value> {
    let encrypted = item
        .get("encrypted_content")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Some(json!({
        "type": "reasoning",
        "encrypted_content": encrypted,
        "summary": [],
    }))
}

async fn await_ws_setup<T, Fut>(
    operation: &'static str,
    timeout: Duration,
    cancel: &CancellationToken,
    emitted_visible_output: bool,
    future: Fut,
) -> std::result::Result<T, (WsFallback, anyhow::Error)>
where
    Fut: Future<Output = Result<T>>,
{
    let result = tokio::select! {
        _ = cancel.cancelled() => {
            return Err((WsFallback::Fatal, anyhow!("Codex WebSocket request cancelled")));
        }
        result = tokio::time::timeout(timeout, future) => result,
    };
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => {
            let policy = classify_ws_error(&error, emitted_visible_output);
            Err((policy, error))
        }
        Err(_) => Err((
            if emitted_visible_output {
                WsFallback::Fatal
            } else {
                WsFallback::FallbackSse
            },
            anyhow!(
                "Codex WebSocket {operation} timed out after {}s",
                timeout.as_secs()
            ),
        )),
    }
}

fn build_codex_ws_request(
    url: &Url,
    token: &AccessToken,
    session_id: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>> {
    let mut request = url.as_str().into_client_request()?;
    let headers = request.headers_mut();
    headers.insert(
        AUTHORIZATION.as_str(),
        WsHeaderValue::from_str(&format!("Bearer {}", token.bearer))?,
    );
    headers.insert(
        "chatgpt-account-id",
        WsHeaderValue::from_str(&token.account_id)?,
    );
    headers.insert("originator", WsHeaderValue::from_static("iris"));
    headers.insert(
        USER_AGENT.as_str(),
        WsHeaderValue::from_static("iris-agent"),
    );
    headers.insert(
        "OpenAI-Beta",
        WsHeaderValue::from_static("responses_websockets=2026-02-06"),
    );
    headers.insert("session-id", WsHeaderValue::from_str(session_id)?);
    headers.insert(
        "x-client-request-id",
        WsHeaderValue::from_str(&format!("iris-{session_id}"))?,
    );
    Ok(request)
}

async fn connect_codex_ws(url: Url, token: &AccessToken, session_id: &str) -> Result<CodexWs> {
    let request = build_codex_ws_request(&url, token, session_id)?;
    let (stream, _) = tokio_tungstenite::connect_async(request).await?;
    Ok(stream)
}

fn classify_ws_error(error: &anyhow::Error, emitted_visible_output: bool) -> WsFallback {
    if emitted_visible_output {
        return WsFallback::Fatal;
    }
    let text = error.to_string();
    if text.contains("previous_response_not_found") {
        WsFallback::RetryFullWebSocket
    } else if text.contains("websocket_connection_limit_reached") {
        WsFallback::RetryWebSocket
    } else if text.contains("401") || text.contains("403") {
        WsFallback::ForceRefresh
    } else {
        WsFallback::FallbackSse
    }
}

fn safe_transport_error(error: &impl std::fmt::Display) -> &'static str {
    let text = error.to_string();
    if text.contains("401") {
        "status=401"
    } else if text.contains("403") {
        "status=403"
    } else if text.contains("previous_response_not_found") {
        "code=previous_response_not_found"
    } else if text.contains("websocket_connection_limit_reached") {
        "code=websocket_connection_limit_reached"
    } else {
        "transport_error"
    }
}

#[cfg(test)]
fn ws_headers_for_test(
    token: &AccessToken,
    session_id: &str,
) -> Result<tokio_tungstenite::tungstenite::http::HeaderMap> {
    let url = Url::parse("wss://chatgpt.com/backend-api/codex/responses")?;
    Ok(build_codex_ws_request(&url, token, session_id)?
        .headers()
        .clone())
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
    for_each_sse_event(reader, cancel, |data| {
        sink.on_activity()?;
        parser.ingest_event(data, sink)
    })?;
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
    /// Whether a reasoning-summary delta was forwarded for display. Like
    /// `emitted_visible_text`, it disables silent retry of a mid-stream protocol
    /// anomaly: the user has already seen live reasoning, so a replay would
    /// duplicate visible output.
    emitted_visible_reasoning: bool,
    /// Whether a freeform tool-call input delta was forwarded for display
    /// (ADR-0039). Also disables silent retry: the user has seen a live tool-input
    /// preview, so replaying the stream would duplicate visible output.
    emitted_visible_tool_input: bool,
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
            emitted_visible_reasoning: false,
            emitted_visible_tool_input: false,
            last_event_type: None,
        }
    }

    /// Whether any visible output (assistant text, a live reasoning summary, or
    /// a freeform tool-input preview) was forwarded to the front-end. Once true,
    /// a mid-stream protocol anomaly is fatal rather than silently retried, to
    /// avoid duplicating shown output on replay.
    fn emitted_visible_output(&self) -> bool {
        self.emitted_visible_text
            || self.emitted_visible_reasoning
            || self.emitted_visible_tool_input
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
            // Live reasoning deltas. Forwarded display-only and never
            // accumulated into `self.text` or any stored reasoning: the
            // persisted reasoning block still comes from `output_item.done` /
            // `response.completed` so replay continuity remains unchanged.
            Some("response.reasoning_summary_text.delta") => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    sink.on_reasoning_delta(delta)?;
                    self.emitted_visible_reasoning = true;
                }
            }
            Some("response.reasoning_text.delta") => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    sink.on_raw_reasoning_delta(delta)?;
                    self.emitted_visible_reasoning = true;
                }
            }
            Some("response.reasoning_summary_part.added") => {
                // A new summary part begins: a blank line between parts. Only a
                // section break *after* visible reasoning is meaningful (the
                // first part.added opens the trace and renders nothing).
                if self.emitted_visible_reasoning {
                    sink.on_reasoning_section_break()?;
                }
            }
            // Live *freeform/custom* tool-call input fragments (ADR-0039).
            // Forwarded display-only and never parsed, accumulated, approved,
            // executed, or stored: the completed `function_call`/`custom_tool_call`
            // item at `response.output_item.done`/`response.completed` remains the
            // only source of executed arguments. JSON-argument tools emit
            // `response.function_call_arguments.delta`, which is deliberately NOT
            // handled -- those arguments stay buffered until completion.
            Some("response.custom_tool_call_input.delta") => {
                if let Some(delta) = value.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    let call_id = value
                        .get("item_id")
                        .or_else(|| value.get("call_id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    sink.on_tool_input_delta(call_id, delta)?;
                    self.emitted_visible_tool_input = true;
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
            Some("response.completed") | Some("response.done") => {
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
            Some("error") => bail!("Codex WebSocket error: {}", top_level_error(&value)),
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
            // The terminal `response.completed` envelope is authoritative for
            // reasoning summaries. `response.output_item.done` can arrive with an
            // interim summary shell (for example a title plus `<!-- -->`), while
            // the completed response carries the final human-readable summary.
            // Prefer the final envelope when present, but keep item-level
            // reasoning for streams that omit it from `response.completed`.
            if !completed_turn.reasoning.is_empty() {
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

/// A mid-stream protocol anomaly is only silently retryable when no visible
/// output (assistant text OR a live reasoning summary) has been shown yet;
/// otherwise a retry would duplicate output the user already saw.
fn protocol_anomaly_retryable(error: &anyhow::Error, emitted_visible_output: bool) -> bool {
    !emitted_visible_output
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
    format_error_fields(error, "response.failed event received")
}

fn top_level_error(value: &Value) -> String {
    format_error_fields(value.get("error"), "error event received")
}

fn format_error_fields(error: Option<&Value>, fallback: &str) -> String {
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
        fallback.to_string()
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

/// Extract the display-safe reasoning text: the human-readable `summary` only.
/// The raw `content` (chain-of-thought) is deliberately excluded from anything
/// shown or stored as visible text (ADR-0049). It is normally absent anyway
/// because Iris always requests encrypted reasoning, which is carried
/// separately as opaque continuity, never rendered.
fn extract_reasoning_text(value: &Value) -> String {
    let mut summary = String::new();
    if let Some(parts) = value.get("summary").and_then(Value::as_array) {
        for part in parts {
            if let Some(part_text) = part.get("text").and_then(Value::as_str) {
                summary.push_str(part_text);
            }
        }
    }
    summary
}

fn extract_openai_usage(value: &Value, model: &str) -> Option<ProviderUsage> {
    let usage = value.get("usage")?;
    // Diagnostics only: the verbatim `usage` object this endpoint sent, so a
    // live campaign can settle whether the codex lane surfaces
    // `cache_write_tokens` at all. Off unless RUST_LOG enables the
    // `iris::usage_raw` target; never a reported metric. See HARNESS.md.
    tracing::debug!(target: "iris::usage_raw", model, usage = %usage, "codex responses raw usage");
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
    // GPT-5.6+ reports prompt-cache writes; older families omit the field.
    let cache_write_input_tokens = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cache_write_tokens"))
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
        cache_write_input_tokens,
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
