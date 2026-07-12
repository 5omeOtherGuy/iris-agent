//! Anthropic Messages provider (Claude Code subscription / OAuth lane).
//! Mirrors `openai_codex_responses.rs`: the request is built eagerly, then a
//! blocking reqwest + SSE parse runs through the shared `transport` channel +
//! one-shot reauth glue, with each SSE event assembled into an `AssistantTurn`.
//!
//! ponytail: only the Claude Code subscription OAuth lane (Bearer token, no
//! x-api-key, no thinking replay). Malformed status-200 streams, 429s, 5xx, and
//! network errors get bounded transient backoff via the shared transport's
//! `run_with_retry`. Add the API-key lane or extended-thinking replay only if a
//! real need shows up.

use std::io::BufReader;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Result, anyhow};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use tokio_util::sync::CancellationToken;

use super::transport::{
    Attempt, HttpClass, TurnSink, classify_http_status_retryable, for_each_sse_event,
    retry_after_hint, run_with_retry, spawn_stream,
};
use crate::errors::AuthError;
use crate::mimir::anthropic_models::{self, ThinkingMode};
use crate::mimir::auth::anthropic::AnthropicTokenStore;
use crate::mimir::auth::api_key;
use crate::mimir::auth::storage::{AuthStore, CredentialKind};
use crate::mimir::selection::{
    ContextManagement, PromptCacheRetention, ProviderId, ReasoningEffort,
};
use crate::nexus::{
    AssistantTurn, CacheCreation, ChatProvider, CompletionReason, Message, ModelOrigin,
    ProviderCompactionCapability, ProviderCompactionFuture, ProviderCompactionOutput,
    ProviderStream, ProviderUsage, ReasoningBlock, Role, ToolCall, Tools,
};

/// Default output cap for an unknown/non-subscription Anthropic id (conservative
/// 64k). Subscription models carry their real cap in `anthropic_models`.
const DEFAULT_OUTPUT_CAP: u32 = 64000;
/// Anthropic's extended-thinking floor: `budget_tokens` must be `>= 1024` and
/// `< max_tokens`, else the request 400s.
const ANTHROPIC_MIN_THINKING_BUDGET_TOKENS: u32 = 1024;
const ANTHROPIC_VERSION: &str = "2023-06-01";
const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
/// Base betas every Claude Code OAuth request carries.
const BASE_ANTHROPIC_BETA: &str = "oauth-2025-04-20,claude-code-20250219";
/// Appended only for manual-budget thinking payloads (`thinking.type ==
/// "enabled"`); adaptive thinking implies interleaved thinking server-side and
/// no thinking does not need it.
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const EXTENDED_CACHE_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";
const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";
const COMPACTION_BETA: &str = "compact-2026-01-12";
const NATIVE_COMPACTION_MIN_INPUT_TOKENS: u64 = 50_000;
/// Appended only when the payload carries a `fallbacks` array (Fable 5 refusal
/// fallback). The header date is authoritative as written; adopted from
/// minimalcc-pi `SERVER_SIDE_FALLBACK_BETA`.
const SERVER_SIDE_FALLBACK_BETA: &str = "server-side-fallback-2026-06-01";
const PROVIDER_ID: &str = "anthropic";
const API_ID: &str = "anthropic-messages";
/// Endpoint path surfaced in failure diagnostics (never the full base URL).
const ENDPOINT_PATH: &str = "/v1/messages";
static SERVER_SIDE_FALLBACK_UNSUPPORTED: AtomicBool = AtomicBool::new(false);
static NATIVE_COMPACTION_UNSUPPORTED_MODELS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

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
    cache_retention: PromptCacheRetention,
    context_management: ContextManagement,
    cache_prefix: Arc<Mutex<super::PromptCachePrefix>>,
    auth: AnthropicAuthSource,
    retry_policy: crate::mimir::retry::RetryPolicy,
}

#[derive(Clone)]
enum AnthropicAuthSource {
    OAuth(AnthropicTokenStore),
    ApiKey(String),
}

#[derive(Clone, PartialEq, Eq)]
enum AnthropicAuth {
    OAuthBearer(String),
    ApiKey(String),
}

impl std::fmt::Debug for AnthropicAuthSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OAuth(store) => f.debug_tuple("OAuth").field(store).finish(),
            Self::ApiKey(_) => f.debug_tuple("ApiKey").field(&"<redacted>").finish(),
        }
    }
}

impl std::fmt::Debug for AnthropicAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OAuthBearer(_) => f.debug_tuple("OAuthBearer").field(&"<redacted>").finish(),
            Self::ApiKey(_) => f.debug_tuple("ApiKey").field(&"<redacted>").finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnthropicAuthKind {
    OAuth,
    ApiKey,
}

impl AnthropicAuth {
    fn kind(&self) -> AnthropicAuthKind {
        match self {
            AnthropicAuth::OAuthBearer(_) => AnthropicAuthKind::OAuth,
            AnthropicAuth::ApiKey(_) => AnthropicAuthKind::ApiKey,
        }
    }
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
        cache_retention: PromptCacheRetention,
        context_management: ContextManagement,
        retry_policy: crate::mimir::retry::RetryPolicy,
    ) -> Result<Self> {
        context_management.validate_supported()?;
        Ok(Self {
            // Shared process-wide client: warm pooled connections (HTTP/2 +
            // keep-alive) survive across turns and model switches, so a turn
            // does not pay a fresh TLS handshake after an idle gap.
            client: super::transport::shared_client(),
            model: model.to_string(),
            base_url: base_url.to_string(),
            reasoning,
            system_prompt: system_prompt.to_string(),
            cache_retention,
            context_management,
            cache_prefix: Arc::new(Mutex::new(super::PromptCachePrefix::default())),
            auth: resolve_anthropic_auth()?,
            retry_policy,
        })
    }
}

fn resolve_anthropic_auth() -> Result<AnthropicAuthSource> {
    let auth_store = AuthStore::from_env()?;
    if auth_store.credential_kind(PROVIDER_ID)? == Some(CredentialKind::ApiKey) {
        return Ok(AnthropicAuthSource::ApiKey(
            auth_store.api_key_credentials(PROVIDER_ID)?.key,
        ));
    }
    if auth_store.credential_kind(PROVIDER_ID)? != Some(CredentialKind::OAuth)
        && let Some(key) = api_key::api_key_for_provider(ProviderId::Anthropic, &auth_store)?
    {
        return Ok(AnthropicAuthSource::ApiKey(key));
    }
    Ok(AnthropicAuthSource::OAuth(AnthropicTokenStore::from_env()?))
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
        let auth_kind = match &self.auth {
            AnthropicAuthSource::OAuth(_) => AnthropicAuthKind::OAuth,
            AnthropicAuthSource::ApiKey(_) => AnthropicAuthKind::ApiKey,
        };
        let mut request = build_anthropic_request_for_auth(
            &self.model,
            &self.system_prompt,
            messages,
            tools,
            self.reasoning,
            AnthropicRequestConfig {
                cache_retention: self.cache_retention,
                context_management: &self.context_management,
                auth_kind,
            },
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
                run_with_retry(
                    "anthropic",
                    &provider.retry_policy,
                    cancel,
                    |force| match &provider.auth {
                        AnthropicAuthSource::OAuth(tokens) => {
                            let token = if force {
                                tokens.force_refresh(&provider.client, last_token.as_deref())
                            } else {
                                tokens.access_token(&provider.client)
                            }?;
                            last_token = Some(token.clone());
                            Ok(AnthropicAuth::OAuthBearer(token))
                        }
                        AnthropicAuthSource::ApiKey(key) => Ok(AnthropicAuth::ApiKey(key.clone())),
                    },
                    |auth| {
                        let attempt = provider.send_once(auth, &request, sink, cancel);
                        if let Attempt::Fatal(ref error) = attempt
                            && error
                                .downcast_ref::<ServerSideFallbackRejection>()
                                .is_some()
                            && remove_fallbacks(&mut request)
                        {
                            SERVER_SIDE_FALLBACK_UNSUPPORTED.store(true, Ordering::Relaxed);
                            tracing::warn!(
                                error = %format!("{error:#}"),
                                "server-side fallback rejected; retrying once without fallbacks and disabling fallback payloads for this process"
                            );
                            return provider.send_once(auth, &request, sink, cancel);
                        }
                        attempt
                    },
                )
            },
            cancel,
        ))
    }

    fn compaction_capability(&self, input_tokens: u64) -> ProviderCompactionCapability {
        let unsupported = NATIVE_COMPACTION_UNSUPPORTED_MODELS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains(&self.model);
        if !unsupported && input_tokens >= NATIVE_COMPACTION_MIN_INPUT_TOKENS {
            ProviderCompactionCapability::OpaqueBlocks
        } else {
            ProviderCompactionCapability::None
        }
    }

    fn compact_context<'a>(
        &'a self,
        messages: &'a [Message],
        instructions: &'a str,
        cancel: &'a CancellationToken,
    ) -> ProviderCompactionFuture<'a> {
        Box::pin(async move { self.compact_context_blocking(messages, instructions, cancel) })
    }
}

impl AnthropicProvider {
    fn send_once(
        &self,
        auth: &AnthropicAuth,
        request: &Value,
        sink: &mut dyn TurnSink,
        cancel: &CancellationToken,
    ) -> Attempt {
        let headers = match anthropic_headers_for_auth(auth, request) {
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
                // A pre-stream send failure (DNS/TLS/connect/timeout) is
                // transient and emitted no output yet: retry with backoff.
                return Attempt::Retry(
                    anyhow::Error::new(error).context("failed to send Anthropic request"),
                    None,
                );
            }
        };

        let status = response.status();
        // Request id comes off the response headers, before the body is read.
        let request_id = extract_request_id(response.headers());
        if status.is_success() {
            let mut parser = AnthropicStreamParser::new(
                anthropic_origin(&self.model),
                request_has_fallbacks(request),
            );
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
                sink.on_activity()?;
                parser.ingest_event(data, sink)
            }) {
                let last = parser.last_event_type.clone();
                let error = error.context(diag(last).to_string());
                // A mid-stream read failure (connection drop, timeout) is
                // transient. It is safe to retry on the same terms as a
                // protocol anomaly: only when this attempt streamed no visible
                // text, so a retry cannot duplicate user-visible output. If text
                // was already shown, or the turn was cancelled, surface it.
                if !cancel.is_cancelled() && !parser.emitted_visible_output() {
                    return Attempt::Retry(error, None);
                }
                return Attempt::Fatal(error);
            }
            let last = parser.last_event_type.clone();
            // Whether this attempt streamed any visible text to the consumer.
            // Assistant text AND non-redacted reasoning summaries are forwarded
            // live, so either gates whether a malformed stream can be safely
            // retried without duplicating user-visible output.
            let emitted_visible_output = parser.emitted_visible_output();
            return match parser.finish() {
                Ok(turn) => {
                    if let Some(usage) = &turn.usage {
                        self.record_usage(usage);
                    }
                    warn_on_truncation(&self.model, turn.completion_reason.as_ref());
                    Attempt::Done(Box::new(turn))
                }
                Err(error) => {
                    let retryable = protocol_anomaly_retryable(&error, emitted_visible_output);
                    // Attach safe diagnostics; downcast already happened above so
                    // wrapping does not lose the classification.
                    let error = error.context(diag(last).to_string());
                    if retryable {
                        Attempt::Retry(error, None)
                    } else {
                        Attempt::Fatal(error)
                    }
                }
            };
        }

        // Non-success: surface only safe metadata. The raw body is read and
        // dropped; only the enumerated `error.type` is pulled out of it (never
        // the body text, which can carry prompts/paths/args).
        let retry_after = retry_after_hint(response.headers());
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
        let fallback_rejected = request_has_fallbacks(request)
            && is_server_side_fallback_rejection(status.as_u16(), &body);
        let error = if fallback_rejected {
            anyhow::Error::new(ServerSideFallbackRejection {
                diagnostics: diag.to_string(),
            })
        } else {
            super::classified_http_error(
                status.as_u16(),
                &body,
                format!("Anthropic request failed [{diag}]"),
            )
        };
        match classify_http_status_retryable(status.as_u16()) {
            HttpClass::Reauth if auth.kind() == AnthropicAuthKind::OAuth => Attempt::Reauth(error),
            HttpClass::Reauth => Attempt::Fatal(
                AuthError::for_provider(
                    PROVIDER_ID,
                    format!("Anthropic API key was rejected (HTTP {})", status.as_u16()),
                )
                .into(),
            ),
            HttpClass::Retry => Attempt::Retry(error, retry_after),
            HttpClass::Fatal => Attempt::Fatal(error),
        }
    }

    fn record_usage(&self, usage: &ProviderUsage) {
        let cache_creation_5m = usage
            .cache_creation
            .as_ref()
            .map_or(0, |creation| creation.ephemeral_5m_input_tokens);
        let cache_creation_1h = usage
            .cache_creation
            .as_ref()
            .map_or(0, |creation| creation.ephemeral_1h_input_tokens);
        tracing::info!(
            provider = %usage.provider,
            model = %usage.model,
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            cache_read_input_tokens = usage.cache_read_input_tokens,
            cache_write_input_tokens = usage.cache_write_input_tokens,
            cache_creation_5m_input_tokens = cache_creation_5m,
            cache_creation_1h_input_tokens = cache_creation_1h,
            total_tokens = usage.total_tokens,
            cacheable_request_sent = self.cache_retention.caching_enabled(),
            cache_hit = usage.cache_read_input_tokens > 0,
            "provider token usage"
        );
    }

    fn compact_context_blocking(
        &self,
        messages: &[Message],
        instructions: &str,
        cancel: &CancellationToken,
    ) -> Result<ProviderCompactionOutput> {
        let auth_kind = match &self.auth {
            AnthropicAuthSource::OAuth(_) => AnthropicAuthKind::OAuth,
            AnthropicAuthSource::ApiKey(_) => AnthropicAuthKind::ApiKey,
        };
        let request = build_native_compaction_request(
            &self.model,
            &self.system_prompt,
            messages,
            instructions,
            auth_kind,
        );
        let mut last_token: Option<String> = None;
        let mut force_refresh = false;
        let mut reauth_used = false;
        let mut transient_retries = 0u32;
        loop {
            if cancel.is_cancelled() {
                anyhow::bail!("provider-native compaction cancelled");
            }
            let auth = match &self.auth {
                AnthropicAuthSource::OAuth(tokens) => {
                    let token = if force_refresh {
                        tokens.force_refresh(&self.client, last_token.as_deref())
                    } else {
                        tokens.access_token(&self.client)
                    }?;
                    last_token = Some(token.clone());
                    AnthropicAuth::OAuthBearer(token)
                }
                AnthropicAuthSource::ApiKey(key) => AnthropicAuth::ApiKey(key.clone()),
            };
            force_refresh = false;
            match self.send_native_compaction_once(&auth, &request, cancel) {
                NativeCompactionAttempt::Done(output) => {
                    if let Some(usage) = &output.usage {
                        self.record_usage(usage);
                    }
                    return Ok(output);
                }
                NativeCompactionAttempt::Unsupported(error) => {
                    NATIVE_COMPACTION_UNSUPPORTED_MODELS
                        .get_or_init(|| Mutex::new(HashSet::new()))
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .insert(self.model.clone());
                    return Err(error);
                }
                NativeCompactionAttempt::Reauth(error) if !reauth_used => {
                    reauth_used = true;
                    force_refresh = true;
                    tracing::warn!(
                        error = %format!("{error:#}"),
                        "provider-native compaction auth rejected; refreshing once"
                    );
                }
                NativeCompactionAttempt::Retry(error, retry_after)
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
                        "provider-native compaction transient error; retrying"
                    );
                    sleep_native_retry(delay, cancel);
                }
                NativeCompactionAttempt::Reauth(error)
                | NativeCompactionAttempt::Retry(error, _)
                | NativeCompactionAttempt::Fatal(error) => return Err(error),
            }
        }
    }

    fn send_native_compaction_once(
        &self,
        auth: &AnthropicAuth,
        request: &Value,
        cancel: &CancellationToken,
    ) -> NativeCompactionAttempt {
        let headers = match anthropic_headers_for_auth(auth, request) {
            Ok(headers) => headers,
            Err(error) => return NativeCompactionAttempt::Fatal(error),
        };
        let url = format!("{}{ENDPOINT_PATH}", self.base_url);
        let response = match self.client.post(&url).headers(headers).json(request).send() {
            Ok(response) => response,
            Err(error) => {
                return NativeCompactionAttempt::Retry(
                    anyhow::Error::new(error)
                        .context("failed to send Anthropic native compaction request"),
                    None,
                );
            }
        };
        let status = response.status();
        if status.is_success() {
            return match parse_native_compaction_reader(
                BufReader::new(response),
                &self.model,
                cancel,
            ) {
                Ok(output) => NativeCompactionAttempt::Done(output),
                Err(error) => NativeCompactionAttempt::Retry(error, None),
            };
        }
        let retry_after = retry_after_hint(response.headers());
        let body = response.text().unwrap_or_default();
        let error_type = extract_error_type(&body).unwrap_or_else(|| "unknown_error".to_string());
        let error = anyhow!(
            "Anthropic native compaction request failed (status={}, error_type={error_type})",
            status.as_u16()
        );
        if is_anthropic_native_unsupported(status.as_u16(), &body) {
            return NativeCompactionAttempt::Unsupported(error);
        }
        match classify_http_status_retryable(status.as_u16()) {
            HttpClass::Reauth => NativeCompactionAttempt::Reauth(error),
            HttpClass::Retry => NativeCompactionAttempt::Retry(error, retry_after),
            HttpClass::Fatal => NativeCompactionAttempt::Fatal(error),
        }
    }
}

fn is_anthropic_native_unsupported(status: u16, body: &str) -> bool {
    status == 400 && !super::is_context_overflow_response(status, body)
}

enum NativeCompactionAttempt {
    Done(ProviderCompactionOutput),
    Unsupported(anyhow::Error),
    Reauth(anyhow::Error),
    Retry(anyhow::Error, Option<std::time::Duration>),
    Fatal(anyhow::Error),
}

fn sleep_native_retry(delay: std::time::Duration, cancel: &CancellationToken) {
    let slice = std::time::Duration::from_millis(50);
    let started = std::time::Instant::now();
    while !cancel.is_cancelled() && started.elapsed() < delay {
        std::thread::sleep(slice.min(delay.saturating_sub(started.elapsed())));
    }
}

fn parse_native_compaction_reader(
    reader: impl std::io::BufRead,
    model: &str,
    cancel: &CancellationToken,
) -> Result<ProviderCompactionOutput> {
    let mut open_index = None;
    let mut summary = None;
    let mut compaction_block = None;
    let mut blocks = 0usize;
    let mut message_stopped = false;
    let mut stop_reason = None;
    let mut usage_value = None;
    for_each_sse_event(reader, cancel, |data| {
        let value: Value = serde_json::from_str(data)
            .map_err(|_| anyhow!("Anthropic native compaction SSE contained invalid JSON"))?;
        match value.get("type").and_then(Value::as_str) {
            Some("content_block_start")
                if value
                    .get("content_block")
                    .and_then(|block| block.get("type"))
                    .and_then(Value::as_str)
                    == Some("compaction") =>
            {
                blocks += 1;
                open_index = value.get("index").and_then(Value::as_u64);
                compaction_block = value.get("content_block").cloned();
                summary = compaction_block
                    .as_ref()
                    .and_then(|block| block.get("content"))
                    .and_then(Value::as_str)
                    .map(String::from);
            }
            Some("content_block_delta")
                if value
                    .get("delta")
                    .and_then(|delta| delta.get("type"))
                    .and_then(Value::as_str)
                    == Some("compaction_delta") =>
            {
                let index = value.get("index").and_then(Value::as_u64);
                if open_index != index {
                    anyhow::bail!("Anthropic native compaction delta had no open block");
                }
                if let Some(content) = value
                    .get("delta")
                    .and_then(|delta| delta.get("content"))
                    .and_then(Value::as_str)
                {
                    summary = Some(content.to_string());
                    if let Some(block) = compaction_block.as_mut() {
                        block["content"] = Value::String(content.to_string());
                    }
                }
            }
            Some("content_block_stop") => {
                if open_index == value.get("index").and_then(Value::as_u64) {
                    open_index = None;
                }
            }
            Some("message_delta") => {
                stop_reason = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                    .map(String::from);
                if let Some(usage) = value.get("usage") {
                    usage_value = Some(usage.clone());
                }
            }
            Some("message_stop") => message_stopped = true,
            Some("error") => {
                let error_type = value
                    .get("error")
                    .and_then(|error| error.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                anyhow::bail!("Anthropic native compaction stream error (error_type={error_type})");
            }
            _ => {}
        }
        Ok(())
    })?;
    if !message_stopped || open_index.is_some() || blocks != 1 {
        anyhow::bail!("Anthropic native compaction stream was incomplete");
    }
    if stop_reason.as_deref() != Some("compaction") {
        anyhow::bail!("Anthropic native compaction did not pause after compaction");
    }
    let summary = summary
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("Anthropic native compaction returned an empty compaction block"))?;
    let summary = summary.trim().to_string();
    let mut block = compaction_block
        .ok_or_else(|| anyhow!("Anthropic native compaction returned no compaction block"))?;
    block["content"] = Value::String(summary.clone());
    let provider_blocks = vec![json!({
        "adapter": API_ID,
        "model": model,
        "block": block
    })];
    let usage = usage_value
        .as_ref()
        .map(|usage| native_compaction_usage(model, usage));
    Ok(ProviderCompactionOutput {
        summary,
        provider_blocks,
        usage,
    })
}

fn native_compaction_usage(model: &str, usage: &Value) -> ProviderUsage {
    let iterations = usage.get("iterations").and_then(Value::as_array);
    let (input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens) =
        if let Some(iterations) = iterations {
            iterations.iter().fold((0, 0, 0, 0), |acc, item| {
                (
                    acc.0
                        + item
                            .get("input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    acc.1
                        + item
                            .get("output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    acc.2
                        + item
                            .get("cache_read_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    acc.3
                        + item
                            .get("cache_creation_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                )
            })
        } else {
            (
                usage
                    .get("input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                usage
                    .get("cache_read_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                usage
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            )
        };
    ProviderUsage {
        provider: PROVIDER_ID.to_string(),
        model: model.to_string(),
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens,
        reasoning_output_tokens: 0,
        total_tokens: input_tokens.saturating_add(output_tokens),
        cache_creation: None,
    }
}

#[cfg(test)]
fn parse_native_compaction_sse(body: &str, model: &str) -> Result<ProviderCompactionOutput> {
    parse_native_compaction_reader(body.as_bytes(), model, &CancellationToken::new())
}

/// Safe, redacted diagnostics for an Anthropic request/stream failure. Every
/// field is local metadata that cannot carry a credential, prompt, tool
/// argument, file path, command string, raw request/response body, or SSE
/// frame. Adopted conceptually from minimalcc-pi's metadata-only stream
/// diagnostics, trimmed to the fields Iris can produce cheaply.
#[derive(Debug)]
struct ServerSideFallbackRejection {
    diagnostics: String,
}

impl std::fmt::Display for ServerSideFallbackRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Anthropic request rejected server-side fallback payload [{}]",
            self.diagnostics
        )
    }
}

impl std::error::Error for ServerSideFallbackRejection {}

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

/// A status-200 Anthropic stream that ended in a structurally invalid state
/// (malformed SSE): either the terminal `message_stop` never arrived, or it
/// arrived while one or more content blocks were still open (no
/// `content_block_stop`). Recoverable: the transport may retry the whole turn.
///
/// Carries only block-shape metadata -- counts, open block indexes, and the
/// last event type. It never carries streamed content (text, tool input,
/// reasoning), prompts, paths, tool arguments, or auth material.
#[derive(Debug, Clone)]
struct StreamProtocolAnomaly {
    /// Whether the terminal `message_stop` event was observed before the stream
    /// ended. `false` means a truncated/incomplete stream.
    message_stop_seen: bool,
    /// Tool-use content blocks still open (no `content_block_stop`) at stream
    /// end.
    open_tool_blocks: usize,
    /// Thinking / redacted-thinking content blocks still open at stream end.
    open_reasoning_blocks: usize,
    /// Stream indexes of the still-open content blocks (safe correlation ids).
    open_block_indexes: Vec<u64>,
    /// Type of the most recent SSE event the parser saw.
    last_event_type: Option<String>,
}

impl std::fmt::Display for StreamProtocolAnomaly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Name the anomaly so the keyword (`message_stop` / `content_block_stop`)
        // is stable for callers and logs, then append safe block metadata.
        if !self.message_stop_seen {
            write!(f, "Anthropic stream ended before message_stop")?;
        } else {
            write!(f, "Anthropic stream ended before content_block_stop")?;
        }
        write!(
            f,
            " (message_stop_seen={} open_tool_blocks={} open_reasoning_blocks={}",
            self.message_stop_seen, self.open_tool_blocks, self.open_reasoning_blocks
        )?;
        if !self.open_block_indexes.is_empty() {
            let indexes = self
                .open_block_indexes
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            write!(f, " open_block_indexes={indexes}")?;
        }
        if let Some(event) = &self.last_event_type {
            // Use the same `last_event` key as `AnthropicDiagnostics` so logs are
            // searchable on one field across both error shapes.
            write!(f, " last_event={event}")?;
        }
        write!(f, ")")
    }
}

impl std::error::Error for StreamProtocolAnomaly {}

/// Whether a status-200 `finish()` error may be retried. Recoverable only when
/// the error is a [`StreamProtocolAnomaly`] AND this attempt streamed no visible
/// output to the consumer, so a retry cannot duplicate already-shown output.
/// Other finish failures (incomplete tool JSON, empty turn) and any anomaly that
/// arrived after visible output are non-retryable.
///
/// INVARIANT: the gate passed here must be `true` whenever this attempt has
/// pushed ANYTHING to the user-visible stream. Assistant `text_delta` and
/// non-redacted reasoning summaries (`thinking_delta`) are forwarded live and
/// each set their own flag (`emitted_visible_text` / `emitted_visible_reasoning`),
/// which `emitted_visible_output` ORs together. Tool input (`input_json_delta`)
/// and redacted thinking are buffered and only surface in the terminal
/// `AssistantTurn`, so they cannot duplicate on a retry. If tool-input deltas
/// ever become live UI events, they MUST also fold into `emitted_visible_output`,
/// or this retry policy must change -- otherwise a retry could replay
/// already-shown output.
fn protocol_anomaly_retryable(error: &anyhow::Error, emitted_visible_output: bool) -> bool {
    !emitted_visible_output && error.downcast_ref::<StreamProtocolAnomaly>().is_some()
}

/// Map an Anthropic `stop_reason` wire token onto the provider-neutral
/// [`CompletionReason`]. Unknown/future values map to `Other`; the raw
/// enumerated token is logged by the caller rather than stored.
fn map_stop_reason(reason: &str) -> CompletionReason {
    match reason {
        "end_turn" => CompletionReason::EndTurn,
        "tool_use" => CompletionReason::ToolUse,
        "max_tokens" => CompletionReason::MaxOutputTokens,
        "model_context_window_exceeded" => CompletionReason::ContextWindowExceeded,
        "stop_sequence" => CompletionReason::StopSequence,
        "pause_turn" => CompletionReason::Paused,
        "refusal" => CompletionReason::Refusal,
        _ => CompletionReason::Other,
    }
}

/// Surface truncation completion reasons so they are not silently dropped. The
/// reason itself rides on the turn's safe completion metadata; this adds an
/// operator-visible log line for the two truncation outcomes.
fn warn_on_truncation(model: &str, reason: Option<&CompletionReason>) {
    match reason {
        Some(CompletionReason::MaxOutputTokens) => {
            tracing::warn!(
                provider = PROVIDER_ID,
                model = %model,
                completion_reason = "max_output_tokens",
                "Anthropic turn truncated at the max output-token ceiling"
            );
        }
        Some(CompletionReason::ContextWindowExceeded) => {
            tracing::warn!(
                provider = PROVIDER_ID,
                model = %model,
                completion_reason = "context_window_exceeded",
                "Anthropic turn ended because the model context window was exceeded"
            );
        }
        _ => {}
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
    if is_bearer {
        return "oauth_bearer";
    }
    if headers.get("x-api-key").is_some() {
        return "api_key";
    }
    "none"
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
#[cfg(test)]
fn anthropic_headers(token: &str, request: &Value) -> Result<HeaderMap> {
    anthropic_headers_for_auth(&AnthropicAuth::OAuthBearer(token.to_string()), request)
}

fn anthropic_headers_for_auth(auth: &AnthropicAuth, request: &Value) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    match auth {
        AnthropicAuth::OAuthBearer(token) => {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))?,
            );
            headers.insert(
                "anthropic-dangerous-direct-browser-access",
                HeaderValue::from_static("true"),
            );
            headers.insert("x-app", HeaderValue::from_static("cli"));
        }
        AnthropicAuth::ApiKey(key) => {
            headers.insert("x-api-key", HeaderValue::from_str(key)?);
        }
    }
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(
        "anthropic-version",
        HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    if let Some(beta) = anthropic_beta_for_auth(request, auth.kind()) {
        headers.insert("anthropic-beta", HeaderValue::from_str(&beta)?);
    }
    headers.insert(USER_AGENT, HeaderValue::from_static("iris-agent"));
    Ok(headers)
}

/// Build the `anthropic-beta` header value from the outgoing payload. Payload-
/// driven so header construction needs no model object: manual-budget thinking
/// is `thinking.type == "enabled"`, the refusal fallback is a non-empty
/// `fallbacks` array.
#[cfg(test)]
fn anthropic_beta(request: &Value) -> String {
    anthropic_beta_for_auth(request, AnthropicAuthKind::OAuth).unwrap_or_default()
}

fn anthropic_beta_for_auth(request: &Value, auth_kind: AnthropicAuthKind) -> Option<String> {
    let mut betas = Vec::new();
    match auth_kind {
        AnthropicAuthKind::OAuth => {
            betas.extend(BASE_ANTHROPIC_BETA.split(',').map(str::to_string));
        }
        AnthropicAuthKind::ApiKey => betas.push(CLAUDE_CODE_BETA.to_string()),
    }
    let manual_thinking = request
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        == Some("enabled");
    if manual_thinking {
        betas.push(INTERLEAVED_THINKING_BETA.to_string());
    }
    let has_fallbacks = request
        .get("fallbacks")
        .and_then(Value::as_array)
        .is_some_and(|fallbacks| !fallbacks.is_empty());
    if has_fallbacks {
        betas.push(SERVER_SIDE_FALLBACK_BETA.to_string());
    }
    if request_contains_one_hour_cache(request) {
        betas.push(EXTENDED_CACHE_TTL_BETA.to_string());
    }
    if request_contains_native_compaction(request) {
        betas.push(COMPACTION_BETA.to_string());
    } else if request_contains_context_management(request) {
        betas.push(CONTEXT_MANAGEMENT_BETA.to_string());
    }
    (!betas.is_empty()).then(|| betas.join(","))
}

fn request_has_fallbacks(request: &Value) -> bool {
    request
        .get("fallbacks")
        .and_then(Value::as_array)
        .is_some_and(|fallbacks| !fallbacks.is_empty())
}

fn remove_fallbacks(request: &mut Value) -> bool {
    request
        .as_object_mut()
        .and_then(|object| object.remove("fallbacks"))
        .is_some()
}

fn is_server_side_fallback_rejection(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return false;
    };
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .is_some_and(|message| message.to_ascii_lowercase().contains("fallback"))
}

fn request_contains_context_management(request: &Value) -> bool {
    request
        .get("context_management")
        .and_then(|context| context.get("edits"))
        .and_then(Value::as_array)
        .is_some_and(|edits| !edits.is_empty())
}

fn request_contains_native_compaction(request: &Value) -> bool {
    request
        .get("context_management")
        .and_then(|context| context.get("edits"))
        .and_then(Value::as_array)
        .is_some_and(|edits| {
            edits
                .iter()
                .any(|edit| edit.get("type").and_then(Value::as_str) == Some("compact_20260112"))
        })
        || contains_compaction_block(request)
}

fn contains_compaction_block(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.get("type").and_then(Value::as_str) == Some("compaction")
                || map.values().any(contains_compaction_block)
        }
        Value::Array(items) => items.iter().any(contains_compaction_block),
        _ => false,
    }
}

fn request_contains_one_hour_cache(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            let is_one_hour_cache = map
                .get("cache_control")
                .and_then(|cache_control| cache_control.get("ttl"))
                .and_then(Value::as_str)
                == Some("1h");
            is_one_hour_cache || map.values().any(request_contains_one_hour_cache)
        }
        Value::Array(items) => items.iter().any(request_contains_one_hour_cache),
        _ => false,
    }
}

#[cfg(test)]
fn build_anthropic_request(
    model: &str,
    system_prompt: &str,
    messages: &[Message],
    tools: &Tools,
    reasoning: Option<ReasoningEffort>,
    cache_retention: PromptCacheRetention,
    context_management: &ContextManagement,
) -> Value {
    build_anthropic_request_for_auth(
        model,
        system_prompt,
        messages,
        tools,
        reasoning,
        AnthropicRequestConfig {
            cache_retention,
            context_management,
            auth_kind: AnthropicAuthKind::OAuth,
        },
    )
}

struct AnthropicRequestConfig<'a> {
    cache_retention: PromptCacheRetention,
    context_management: &'a ContextManagement,
    auth_kind: AnthropicAuthKind,
}

fn build_anthropic_request_for_auth(
    model: &str,
    system_prompt: &str,
    messages: &[Message],
    tools: &Tools,
    reasoning: Option<ReasoningEffort>,
    config: AnthropicRequestConfig<'_>,
) -> Value {
    let meta = anthropic_models::find(model);
    let thinking_mode = meta
        .map(|m| m.thinking)
        .unwrap_or(ThinkingMode::ManualBudget);
    let output_cap = meta.map(|m| m.output_cap).unwrap_or(DEFAULT_OUTPUT_CAP);

    // The `max_tokens` ceiling is the model's full output cap. A fixed 8192
    // base used to truncate large outputs, surfacing as a silently-dropped
    // `stop_reason=max_tokens`; unknown models fall back to the conservative
    // DEFAULT_OUTPUT_CAP.
    let system = match config.auth_kind {
        AnthropicAuthKind::OAuth => json!([
            { "type": "text", "text": CLAUDE_CODE_IDENTITY },
            { "type": "text", "text": system_prompt },
        ]),
        AnthropicAuthKind::ApiKey => json!([{ "type": "text", "text": system_prompt }]),
    };
    let mut body = json!({
        "model": model,
        "max_tokens": output_cap,
        "stream": true,
        "system": system,
        // Origin (reasoning-replay continuity) is keyed on the selected model id,
        // so a turn replays against the same selection.
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
                    resolve_manual_thinking(output_cap, manual_budget(level), output_cap);
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
    if let Some(fallback) = meta.and_then(|m| m.refusal_fallback)
        && !SERVER_SIDE_FALLBACK_UNSUPPORTED.load(Ordering::Relaxed)
    {
        // Use Anthropic's server-side safety fallback. Do not implement a
        // client-side Opus retry: the server-side path is the one Anthropic
        // fallback-credit reprices/refunds when Fable's safety classifier
        // declines and Opus 4.8 serves the turn.
        body["fallbacks"] = json!([{ "model": fallback }]);
    }

    let declarations = tool_declarations(tools);
    if !declarations.is_empty() {
        body["tools"] = Value::Array(declarations);
    }
    apply_anthropic_cache_control(&mut body, config.cache_retention);
    apply_context_management(&mut body, config.context_management);
    body
}

fn build_native_compaction_request(
    model: &str,
    system_prompt: &str,
    messages: &[Message],
    instructions: &str,
    auth_kind: AnthropicAuthKind,
) -> Value {
    let mut body = build_anthropic_request_for_auth(
        model,
        system_prompt,
        messages,
        &Tools::new(Vec::new()),
        None,
        AnthropicRequestConfig {
            cache_retention: PromptCacheRetention::None,
            context_management: &ContextManagement::default(),
            auth_kind,
        },
    );
    let instructions = native_compaction_instructions(instructions);
    body["context_management"] = json!({
        "edits": [{
            "type": "compact_20260112",
            "trigger": {
                "type": "input_tokens",
                "value": NATIVE_COMPACTION_MIN_INPUT_TOKENS,
            },
            "pause_after_compaction": true,
            "instructions": instructions,
        }]
    });
    body
}

fn native_compaction_instructions(instructions: &str) -> String {
    let instructions = instructions.trim();
    let guard = "Do not call tools while writing this summary; respond with text only.";
    if instructions.is_empty() {
        format!(
            "Summarize the transcript inside <summary></summary> tags. Include all information needed to continue the task. {guard}"
        )
    } else {
        format!("{instructions} {guard}")
    }
}

/// Manual-budget thinking token budget for an iris reasoning level. Adopted
/// verbatim from minimalcc-pi `DEFAULT_THINKING_BUDGETS`. `Off` yields 0 (no
/// thinking); it is never reached because callers filter `Off` out first.
fn apply_anthropic_cache_control(body: &mut Value, retention: PromptCacheRetention) {
    let Some(cache_control) = anthropic_cache_control(retention) else {
        return;
    };
    if let Some(system) = body.get_mut("system").and_then(Value::as_array_mut)
        && let Some(block) = system
            .iter_mut()
            .rev()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        && let Some(object) = block.as_object_mut()
    {
        object.insert("cache_control".to_string(), cache_control.clone());
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages.iter_mut().rev() {
            if message.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut)
                && let Some(block) = content.last_mut()
                && let Some(object) = block.as_object_mut()
            {
                object.insert("cache_control".to_string(), cache_control.clone());
                break;
            }
        }
    }
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut)
        && let Some(tool) = tools.last_mut().and_then(Value::as_object_mut)
    {
        tool.insert("cache_control".to_string(), cache_control);
    }
}

fn anthropic_cache_control(retention: PromptCacheRetention) -> Option<Value> {
    match retention {
        PromptCacheRetention::None => None,
        PromptCacheRetention::Short => Some(json!({ "type": "ephemeral" })),
        PromptCacheRetention::Long => Some(json!({ "type": "ephemeral", "ttl": "1h" })),
    }
}

fn apply_context_management(body: &mut Value, context_management: &ContextManagement) {
    if !context_management.is_enabled() {
        return;
    }
    let mut edits = Vec::new();
    if let Some(clear) = &context_management.clear_tool_uses {
        let mut edit = json!({ "type": "clear_tool_uses_20250919" });
        if let Some(value) = clear.trigger_input_tokens {
            edit["trigger"] = typed_value("input_tokens", value);
        }
        if let Some(value) = clear.keep_tool_uses {
            edit["keep"] = typed_value("tool_uses", value);
        }
        if let Some(value) = clear.clear_at_least_input_tokens {
            edit["clear_at_least"] = typed_value("input_tokens", value);
        }
        if let Some(exclude_tools) = &clear.exclude_tools
            && !exclude_tools.is_empty()
        {
            edit["exclude_tools"] = json!(exclude_tools);
        }
        if let Some(clear_tool_inputs) = clear.clear_tool_inputs {
            edit["clear_tool_inputs"] = json!(clear_tool_inputs);
        }
        edits.push(edit);
    }
    if let Some(clear) = &context_management.clear_thinking {
        let mut edit = json!({ "type": "clear_thinking_20251015" });
        if let Some(value) = clear.trigger_input_tokens {
            edit["trigger"] = typed_value("input_tokens", value);
        }
        if let Some(value) = clear.keep_thinking_turns {
            edit["keep"] = typed_value("thinking_turns", value);
        }
        edits.push(edit);
    }
    if !edits.is_empty() {
        body["context_management"] = json!({ "edits": edits });
    }
}

fn typed_value(kind: &str, value: u64) -> Value {
    json!({ "type": kind, "value": value })
}

fn manual_budget(level: ReasoningEffort) -> u32 {
    match level {
        ReasoningEffort::Off => 0,
        ReasoningEffort::Minimal => 1024,
        ReasoningEffort::Low => 4096,
        ReasoningEffort::Medium => 10240,
        ReasoningEffort::High => 20480,
        ReasoningEffort::XHigh => 32768,
        ReasoningEffort::Max => 32768,
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
        ReasoningEffort::Max => "max",
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
        if let Some(block) = matching_compaction_block(message, current_origin) {
            push_block(&mut out, "assistant", block);
            // Keep the provider-neutral body after the opaque block. It carries
            // deterministic recall/carry data the server-authored summary does
            // not know about and is the exact text another adapter receives.
            push_block(
                &mut out,
                "user",
                json!({ "type": "text", "text": message.content }),
            );
            continue;
        }
        let mapped = match message.role {
            // Anthropic has no interleaved developer-message role. Preserve the
            // lower-than-system contextual instruction as a user text block.
            Role::Developer => Some(("user", json!({ "type": "text", "text": message.content }))),
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

fn matching_compaction_block(message: &Message, current_origin: &ModelOrigin) -> Option<Value> {
    message.provider_blocks.iter().find_map(|envelope| {
        let same_adapter = envelope.get("adapter").and_then(Value::as_str) == Some(API_ID);
        let same_model =
            envelope.get("model").and_then(Value::as_str) == Some(current_origin.model.as_str());
        (same_adapter && same_model)
            .then(|| envelope.get("block").cloned())
            .flatten()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("compaction"))
    })
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
    // Foreign-origin reasoning is dropped, not downgraded to text: replaying
    // another model's chain-of-thought would re-bill it as input on every
    // later request while its answer/tool calls already carry the outcome
    // (ADR-0041). The row stays persisted and display-visible.
    if !same_origin {
        return None;
    }
    // Same-origin but signature-less visible reasoning cannot be replayed as a
    // `thinking` block (Anthropic requires the signature), so it degrades to
    // text to preserve same-model continuity.
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
    fallback_block_indexes: HashSet<u64>,
    response_id: Option<String>,
    usage: ProviderUsage,
    raw_input_tokens: u64,
    usage_seen: bool,
    message_stopped: bool,
    /// Whether a non-empty `text_delta` was forwarded to the sink this attempt.
    /// Assistant text and non-redacted reasoning summaries are streamed live;
    /// tool input and redacted thinking are buffered. This flag together with
    /// `emitted_visible_reasoning` gates whether a malformed stream can be
    /// retried without duplicating user-visible output.
    emitted_visible_text: bool,
    /// Whether a non-redacted reasoning (thinking) delta was forwarded live to
    /// the sink this attempt. Like `emitted_visible_text`, it disables silent
    /// retry: the user has already seen live reasoning, so a replay would
    /// duplicate visible output. Redacted thinking never counts here -- its text
    /// is never forwarded (ADR-0016).
    emitted_visible_reasoning: bool,
    /// Fable can switch to Opus mid-stream through a server-side fallback marker.
    /// Until the marker arrives (or the turn ends without one), pre-boundary text
    /// is buffered rather than streamed so a safety-refusal preface can be
    /// discarded cleanly.
    server_side_fallback_possible: bool,
    fallback_boundary_seen: bool,
    /// Provider-neutral completion reason from `message_delta.delta.stop_reason`.
    completion_reason: Option<CompletionReason>,
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
    fn new(origin: ModelOrigin, server_side_fallback_possible: bool) -> Self {
        let model = origin.model.clone();
        Self {
            origin,
            text: String::new(),
            open_tools: HashMap::new(),
            tool_calls: Vec::new(),
            open_reasoning: HashMap::new(),
            reasoning: Vec::new(),
            fallback_block_indexes: HashSet::new(),
            response_id: None,
            usage: ProviderUsage {
                provider: PROVIDER_ID.to_string(),
                model,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
                total_tokens: 0,
                cache_creation: None,
            },
            raw_input_tokens: 0,
            usage_seen: false,
            message_stopped: false,
            emitted_visible_text: false,
            emitted_visible_reasoning: false,
            server_side_fallback_possible,
            fallback_boundary_seen: false,
            completion_reason: None,
            last_event_type: None,
        }
    }

    /// Whether any visible output (assistant text or a live, non-redacted
    /// reasoning summary) was forwarded to the front-end this attempt. Once
    /// true, a mid-stream protocol anomaly is fatal rather than silently
    /// retried, so a replay cannot duplicate shown output.
    fn emitted_visible_output(&self) -> bool {
        self.emitted_visible_text || self.emitted_visible_reasoning
    }

    fn ingest_event(&mut self, data: &str, sink: &mut dyn TurnSink) -> Result<()> {
        if data == "[DONE]" {
            return Ok(());
        }
        // Drop the serde error: although deserializing to an untyped `Value`
        // yields only positional syntax errors today (never the input bytes),
        // returning a fixed message guarantees no streamed content can ever
        // reach logs through this path.
        let value: Value = serde_json::from_str(data)
            .map_err(|_| anyhow!("failed to parse Anthropic SSE frame"))?;
        let event_type = value.get("type").and_then(Value::as_str);
        if let Some(event_type) = event_type {
            self.last_event_type = Some(event_type.to_string());
        }
        match event_type {
            Some("message_start") => {
                self.response_id = value
                    .get("message")
                    .and_then(|message| message.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(model) = value
                    .get("message")
                    .and_then(|message| message.get("model"))
                    .and_then(Value::as_str)
                    .filter(|model| !model.is_empty())
                {
                    self.origin.model = model.to_string();
                    self.usage.model = model.to_string();
                }
                if let Some(usage) = value
                    .get("message")
                    .and_then(|message| message.get("usage"))
                {
                    self.merge_usage(usage);
                }
            }
            Some("content_block_start") => {
                let index = block_index(&value);
                if let Some(block) = value.get("content_block") {
                    match block.get("type").and_then(Value::as_str) {
                        Some("thinking") => {
                            // A new thinking block after reasoning was already
                            // shown this attempt: separate it with a section
                            // break (a blank line between paragraphs), mirroring
                            // the OpenAI adapter's summary-part breaks.
                            let past_boundary =
                                !self.server_side_fallback_possible || self.fallback_boundary_seen;
                            if past_boundary && self.emitted_visible_reasoning {
                                sink.on_reasoning_section_break()?;
                            }
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
                        Some("fallback") => {
                            self.fallback_block_indexes.insert(index);
                            self.fallback_boundary_seen = true;
                            if let Some(to_model) = block
                                .get("to")
                                .and_then(|to| to.get("model"))
                                .and_then(Value::as_str)
                                .filter(|model| !model.is_empty())
                            {
                                self.origin.model = to_model.to_string();
                                self.usage.model = to_model.to_string();
                            }
                            self.text.clear();
                            self.tool_calls.clear();
                            self.open_tools.clear();
                            self.reasoning.clear();
                            self.open_reasoning.clear();
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
                                if !self.server_side_fallback_possible
                                    || self.fallback_boundary_seen
                                {
                                    sink.on_text_delta(text)?;
                                    if !text.is_empty() {
                                        self.emitted_visible_text = true;
                                    }
                                }
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
                            // Same gate as `text_delta`: for a refusal-fallback
                            // model, withhold live output until the fallback
                            // boundary is seen, so reasoning a fallback would
                            // discard is never shown.
                            let past_boundary =
                                !self.server_side_fallback_possible || self.fallback_boundary_seen;
                            let forwarded = if let (Some(block), Some(thinking)) = (
                                self.open_reasoning.get_mut(&index),
                                delta.get("thinking").and_then(Value::as_str),
                            ) {
                                block.text.push_str(thinking);
                                // Forward the display-safe summary live, but
                                // never a redacted block's text (ADR-0016) and
                                // never an empty delta.
                                if past_boundary && !block.redacted && !thinking.is_empty() {
                                    sink.on_reasoning_delta(thinking)?;
                                    true
                                } else {
                                    false
                                }
                            } else {
                                false
                            };
                            if forwarded {
                                self.emitted_visible_reasoning = true;
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
                if self.fallback_block_indexes.remove(&index) {
                    return Ok(());
                }
                if let Some(block) = self.open_tools.remove(&index) {
                    self.tool_calls.push(finalize_tool(block)?);
                } else if let Some(block) = self.open_reasoning.remove(&index) {
                    self.reasoning.push(block);
                }
            }
            Some("message_delta") => {
                if let Some(reason) = value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                {
                    let mapped = map_stop_reason(reason);
                    if mapped == CompletionReason::Other {
                        // Forward-compat: a stop reason Iris does not model yet.
                        // The token is an enumerated wire value (never response
                        // content), safe to log for observability.
                        tracing::debug!(
                            provider = PROVIDER_ID,
                            stop_reason = reason,
                            "unmapped Anthropic stop_reason"
                        );
                    }
                    self.completion_reason = Some(mapped);
                }
                if let Some(usage) = value.get("usage") {
                    self.merge_usage(usage);
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

    fn merge_usage(&mut self, usage: &Value) {
        self.usage_seen = true;
        // Diagnostics only: the verbatim `usage` object this endpoint sent (a
        // message_start baseline then message_delta finals). Off unless RUST_LOG
        // enables the `iris::usage_raw` target; never a reported metric. See
        // HARNESS.md.
        tracing::debug!(
            target: "iris::usage_raw",
            model = %self.usage.model,
            usage = %usage,
            "anthropic messages raw usage"
        );
        if let Some(tokens) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.raw_input_tokens = tokens;
        }
        if let Some(tokens) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.usage.output_tokens = tokens;
        }
        if let Some(tokens) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.usage.cache_read_input_tokens = tokens;
        }
        if let Some(tokens) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.usage.cache_write_input_tokens = tokens;
        }
        if let Some(cache_creation) = usage.get("cache_creation") {
            let creation = self
                .usage
                .cache_creation
                .get_or_insert_with(CacheCreation::default);
            if let Some(tokens) = cache_creation
                .get("ephemeral_5m_input_tokens")
                .and_then(Value::as_u64)
            {
                creation.ephemeral_5m_input_tokens = tokens;
            }
            if let Some(tokens) = cache_creation
                .get("ephemeral_1h_input_tokens")
                .and_then(Value::as_u64)
            {
                creation.ephemeral_1h_input_tokens = tokens;
            }
            if self.usage.cache_write_input_tokens == 0 {
                self.usage.cache_write_input_tokens = creation
                    .ephemeral_5m_input_tokens
                    .saturating_add(creation.ephemeral_1h_input_tokens);
            }
        }
        self.usage.input_tokens = self
            .raw_input_tokens
            .saturating_add(self.usage.cache_read_input_tokens)
            .saturating_add(self.usage.cache_write_input_tokens);
        self.usage.total_tokens = self
            .usage
            .input_tokens
            .saturating_add(self.usage.output_tokens);
    }

    fn finish(self) -> Result<AssistantTurn> {
        // Malformed status-200 streams (no terminal `message_stop`, or
        // `message_stop` with content blocks still open) are recoverable
        // protocol anomalies: return the typed error so the transport can retry.
        if !self.message_stopped
            || !self.open_tools.is_empty()
            || !self.open_reasoning.is_empty()
            || !self.fallback_block_indexes.is_empty()
        {
            let mut open_block_indexes: Vec<u64> = self
                .open_tools
                .keys()
                .chain(self.open_reasoning.keys())
                .chain(self.fallback_block_indexes.iter())
                .copied()
                .collect();
            open_block_indexes.sort_unstable();
            return Err(anyhow::Error::new(StreamProtocolAnomaly {
                message_stop_seen: self.message_stopped,
                open_tool_blocks: self.open_tools.len(),
                open_reasoning_blocks: self.open_reasoning.len(),
                open_block_indexes,
                last_event_type: self.last_event_type,
            }));
        }
        if self.text.is_empty() && self.tool_calls.is_empty() && self.reasoning.is_empty() {
            // A terminal stream carrying a valid `stop_reason` but no content is
            // a legitimate empty completion (e.g. `end_turn` / `tool_use` with
            // nothing to add). Only a contentless stream with NO stop reason is
            // treated as a malformed/empty response.
            if self.completion_reason.is_none() {
                return Err(anyhow!(
                    "Anthropic response did not include assistant text, reasoning, or tool calls"
                ));
            }
        }
        Ok(AssistantTurn {
            text: (!self.text.is_empty()).then_some(self.text),
            reasoning: self.reasoning,
            tool_calls: self.tool_calls,
            response_id: self.response_id,
            usage: self.usage_seen.then_some(self.usage),
            completion_reason: self.completion_reason,
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
        // Drop the serde error so a malformed buffer cannot surface any of the
        // tool arguments; the fixed message is sufficient for diagnostics.
        None => serde_json::from_str(&block.partial_json)
            .map_err(|_| anyhow!("Anthropic tool_use input JSON was incomplete or invalid"))?,
    };
    Ok(ToolCall {
        id: block.id,
        thought_signature: None,
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
    let mut parser = AnthropicStreamParser::new(
        anthropic_origin(model),
        anthropic_models::find(model)
            .and_then(|model| model.refusal_fallback)
            .is_some(),
    );
    let mut sink = NoopSink;
    for_each_sse_event(body.as_bytes(), &CancellationToken::new(), |data| {
        parser.ingest_event(data, &mut sink)
    })?;
    parser.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mimir::selection::PromptCacheRetention;
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
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        let headers = anthropic_headers("fake-oauth-token", &request).unwrap();
        assert_eq!(auth_kind_label(&headers), "oauth_bearer");
        assert_eq!(auth_kind_label(&HeaderMap::new()), "none");
    }

    #[test]
    fn stream_error_event_surfaces_last_event_type_in_diagnostics() {
        // Drive a real error frame through the parser, then confirm the diagnostic
        // tail built from parser state names the error event and no payload.
        let mut parser = AnthropicStreamParser::new(anthropic_origin("claude-opus-4-8"), false);
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
                provider_turn_id: None,
                redacted: false,
                origin: None,
                provider_blocks: Vec::new(),
            },
        ];
        let request = build_anthropic_request(
            "m",
            "IRIS PROMPT",
            &messages,
            &Tools::new(Vec::new()),
            None,
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );

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
    fn cache_control_short_marks_system_prefix_final_user_and_last_tool_only() {
        let messages = [Message::user("cache me")];
        let tools = crate::tools::built_in_tools();
        let request = build_anthropic_request(
            "claude-sonnet-4-6",
            "IRIS PROMPT",
            &messages,
            &tools,
            None,
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );

        let expected = json!({ "type": "ephemeral" });
        let system = request["system"].as_array().expect("system array");
        assert!(
            system[..system.len() - 1]
                .iter()
                .all(|block| block.get("cache_control").is_none())
        );
        assert_eq!(system.last().unwrap()["cache_control"], expected);
        let message_blocks = request["messages"][0]["content"].as_array().unwrap();
        assert_eq!(message_blocks.last().unwrap()["cache_control"], expected);
        let tools = request["tools"].as_array().expect("tools array");
        assert_eq!(tools.last().unwrap()["cache_control"], expected);
        assert!(
            tools[..tools.len() - 1]
                .iter()
                .all(|tool| tool.get("cache_control").is_none())
        );
    }

    #[test]
    fn cache_control_none_omits_markers_and_long_uses_one_hour_ttl() {
        let messages = [Message::user("cache me")];
        let tools = crate::tools::built_in_tools();
        let none = build_anthropic_request(
            "claude-sonnet-4-6",
            "IRIS PROMPT",
            &messages,
            &tools,
            None,
            PromptCacheRetention::None,
            &ContextManagement::default(),
        );
        assert!(!json_contains_key(&none, "cache_control"));

        let long = build_anthropic_request(
            "claude-sonnet-4-6",
            "IRIS PROMPT",
            &messages,
            &tools,
            None,
            PromptCacheRetention::Long,
            &ContextManagement::default(),
        );
        let system = long["system"].as_array().expect("system array");
        assert_eq!(
            system.last().unwrap()["cache_control"],
            json!({ "type": "ephemeral", "ttl": "1h" })
        );
        assert!(anthropic_beta(&long).contains(EXTENDED_CACHE_TTL_BETA));
    }

    #[test]
    fn context_management_edits_and_betas_are_explicit_opt_ins() {
        let messages = [Message::user("manage history")];
        let tools = crate::tools::built_in_tools();
        let disabled = build_anthropic_request(
            "claude-sonnet-4-6",
            "IRIS PROMPT",
            &messages,
            &tools,
            None,
            PromptCacheRetention::None,
            &ContextManagement::default(),
        );
        assert!(disabled.get("context_management").is_none());
        assert!(!anthropic_beta(&disabled).contains(CONTEXT_MANAGEMENT_BETA));

        let context_management = ContextManagement {
            clear_tool_uses: Some(crate::mimir::selection::ClearToolUses {
                trigger_input_tokens: Some(30_000),
                keep_tool_uses: Some(4),
                clear_at_least_input_tokens: Some(10_000),
                exclude_tools: Some(vec!["recall".to_string(), "read_output".to_string()]),
                clear_tool_inputs: Some(false),
            }),
            clear_thinking: Some(crate::mimir::selection::ClearThinking {
                trigger_input_tokens: Some(80_000),
                keep_thinking_turns: Some(2),
            }),
            compact: None,
        };
        let enabled = build_anthropic_request(
            "claude-sonnet-4-6",
            "IRIS PROMPT",
            &messages,
            &tools,
            None,
            PromptCacheRetention::None,
            &context_management,
        );
        assert_eq!(
            enabled["context_management"],
            json!({
                "edits": [
                    {
                        "type": "clear_tool_uses_20250919",
                        "trigger": { "type": "input_tokens", "value": 30000 },
                        "keep": { "type": "tool_uses", "value": 4 },
                        "clear_at_least": { "type": "input_tokens", "value": 10000 },
                        "exclude_tools": ["recall", "read_output"],
                        "clear_tool_inputs": false
                    },
                    {
                        "type": "clear_thinking_20251015",
                        "trigger": { "type": "input_tokens", "value": 80000 },
                        "keep": { "type": "thinking_turns", "value": 2 }
                    }
                ]
            })
        );
        let beta = anthropic_beta(&enabled);
        assert!(beta.contains(CONTEXT_MANAGEMENT_BETA), "{beta}");
        assert!(!beta.contains("compact-2026-01-12"), "{beta}");
    }

    #[test]
    fn native_compaction_request_and_replay_use_the_beta_shape() {
        let covered = [Message::user("old work"), Message::assistant("result")];
        let request = build_native_compaction_request(
            "claude-opus-4-6",
            "IRIS PROMPT",
            &covered,
            "Preserve exact flags.",
            AnthropicAuthKind::OAuth,
        );
        assert_eq!(
            request["context_management"],
            json!({
                "edits": [{
                    "type": "compact_20260112",
                    "trigger": { "type": "input_tokens", "value": 50000 },
                    "pause_after_compaction": true,
                    "instructions": "Preserve exact flags. Do not call tools while writing this summary; respond with text only."
                }]
            })
        );
        assert!(request.get("tools").is_none());
        let beta = anthropic_beta_for_auth(&request, AnthropicAuthKind::OAuth).unwrap();
        assert!(beta.contains("compact-2026-01-12"), "{beta}");

        let block = json!({
            "adapter": "anthropic-messages",
            "model": "claude-opus-4-6",
            "block": { "type": "compaction", "content": "native summary" }
        });
        let messages = [Message::user("portable summary").with_provider_blocks(vec![block])];
        let replay = build_anthropic_request(
            "claude-opus-4-6",
            "IRIS PROMPT",
            &messages,
            &Tools::new(Vec::new()),
            None,
            PromptCacheRetention::None,
            &ContextManagement::default(),
        );
        assert_eq!(replay["messages"][0]["role"], "assistant");
        assert_eq!(
            replay["messages"][0]["content"][0],
            json!({ "type": "compaction", "content": "native summary" })
        );
        assert_eq!(replay["messages"][1]["role"], "user");
        assert_eq!(
            replay["messages"][1]["content"][0]["text"],
            "portable summary"
        );

        let cross_model = build_anthropic_request(
            "claude-sonnet-4-6",
            "IRIS PROMPT",
            &messages,
            &Tools::new(Vec::new()),
            None,
            PromptCacheRetention::None,
            &ContextManagement::default(),
        );
        assert_eq!(cross_model["messages"][0]["role"], "user");
        assert_eq!(
            cross_model["messages"][0]["content"][0]["text"],
            "portable summary"
        );
    }

    #[test]
    fn native_compaction_stream_captures_one_block_text_and_iteration_usage() {
        let body = "\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_compact\",\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"compaction\",\"content\":null,\"future_field\":{\"version\":7}}}\n\n
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"compaction_delta\",\"content\":\"native summary\"}}\n\n
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"compaction\"},\"usage\":{\"input_tokens\":120,\"output_tokens\":0,\"iterations\":[{\"type\":\"compaction\",\"input_tokens\":50000,\"output_tokens\":800}]}}\n\n
data: {\"type\":\"message_stop\"}\n\n";

        let output = parse_native_compaction_sse(body, "claude-opus-4-6").unwrap();
        assert_eq!(output.summary, "native summary");
        assert_eq!(output.provider_blocks.len(), 1);
        assert_eq!(
            output.provider_blocks[0]["block"]["future_field"]["version"],
            7
        );
        assert_eq!(output.usage.as_ref().unwrap().input_tokens, 50_000);
        assert_eq!(output.usage.as_ref().unwrap().output_tokens, 800);
        assert_eq!(output.usage.as_ref().unwrap().total_tokens, 50_800);
    }

    #[test]
    fn native_compaction_overflow_does_not_poison_the_unsupported_model_cache() {
        assert!(!is_anthropic_native_unsupported(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"prompt is too long"}}"#,
        ));
        assert!(is_anthropic_native_unsupported(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"compact is unsupported"}}"#,
        ));
    }

    fn json_contains_key(value: &Value, key: &str) -> bool {
        match value {
            Value::Object(map) => {
                map.contains_key(key) || map.values().any(|child| json_contains_key(child, key))
            }
            Value::Array(items) => items.iter().any(|child| json_contains_key(child, key)),
            _ => false,
        }
    }

    #[test]
    fn parses_usage_from_message_start_and_delta_without_breaking_text() {
        let body = "\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":50,\"output_tokens\":0,\"cache_read_input_tokens\":11,\"cache_creation_input_tokens\":22,\"cache_creation\":{\"ephemeral_5m_input_tokens\":12,\"ephemeral_1h_input_tokens\":10}}}}\n\n
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7,\"cache_read_input_tokens\":13}}\n\n
data: {\"type\":\"message_stop\"}\n\n";

        let turn = parse_anthropic_sse_for_model(body, "claude-sonnet-4-6").unwrap();

        assert_eq!(turn.text.as_deref(), Some("hello"));
        assert_eq!(turn.response_id.as_deref(), Some("msg_1"));
        let usage = turn.usage.expect("usage");
        assert_eq!(usage.input_tokens, 85);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.cache_read_input_tokens, 13);
        assert_eq!(usage.cache_write_input_tokens, 22);
        let creation = usage.cache_creation.expect("cache creation detail");
        assert_eq!(creation.ephemeral_5m_input_tokens, 12);
        assert_eq!(creation.ephemeral_1h_input_tokens, 10);
        assert_eq!(usage.total_tokens, 92);
    }

    #[test]
    fn manual_budget_model_thinking_uses_minimalcc_budgets_and_invariant() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());

        // Sonnet 4.6 is a manual-budget model (cap 64k). No reasoning -> no
        // thinking, base max_tokens (byte-identical default).
        let none = build_anthropic_request(
            "claude-sonnet-4-6",
            "P",
            &messages,
            &tools,
            None,
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        // Sonnet's output cap is the new base/ceiling for max_tokens.
        const SONNET_CAP: u32 = 64000;
        assert!(none.get("thinking").is_none(), "None omits thinking");
        assert!(none.get("output_config").is_none());
        assert_eq!(none["max_tokens"], json!(SONNET_CAP));

        // Explicit Off also omits thinking (no `disabled` block, matching
        // minimalcc-pi).
        let off = build_anthropic_request(
            "claude-sonnet-4-6",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::Off),
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert!(off.get("thinking").is_none(), "Off omits thinking");
        assert_eq!(off["max_tokens"], json!(SONNET_CAP));

        // High -> 20480 budget; max_tokens = min(64000 + 20480, 64000) = 64000.
        let high = build_anthropic_request(
            "claude-sonnet-4-6",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::High),
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert_eq!(
            high["thinking"],
            json!({ "type": "enabled", "budget_tokens": 20480 })
        );
        assert_eq!(high["max_tokens"], json!(SONNET_CAP));
        assert!(
            high["thinking"]["budget_tokens"].as_u64().unwrap()
                < high["max_tokens"].as_u64().unwrap(),
            "budget_tokens must stay below max_tokens"
        );
        assert!(
            high.get("output_config").is_none(),
            "manual model has no effort"
        );

        // xhigh -> 32768 budget; max_tokens = min(64000 + 32768, 64000) = 64000.
        let xhigh = build_anthropic_request(
            "claude-sonnet-4-6",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::XHigh),
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert_eq!(
            xhigh["thinking"],
            json!({ "type": "enabled", "budget_tokens": 32768 })
        );
        assert_eq!(xhigh["max_tokens"], json!(SONNET_CAP));

        // The full minimalcc-pi budget map on a manual model.
        for (level, budget) in [
            (ReasoningEffort::Minimal, 1024u32),
            (ReasoningEffort::Low, 4096),
            (ReasoningEffort::Medium, 10240),
            (ReasoningEffort::High, 20480),
            (ReasoningEffort::XHigh, 32768),
        ] {
            let body = build_anthropic_request(
                "claude-opus-4-6",
                "P",
                &messages,
                &tools,
                Some(level),
                PromptCacheRetention::Short,
                &ContextManagement::default(),
            );
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

        // Sonnet 5 is adaptive: effort via output_config, adaptive thinking, and
        // max_tokens left at the base (no budget bump, no budget_tokens).
        let sonnet = build_anthropic_request(
            "claude-sonnet-5",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::High),
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert_eq!(sonnet["model"], json!("claude-sonnet-5"));
        assert_eq!(
            sonnet["thinking"],
            json!({ "type": "adaptive", "display": "summarized" })
        );
        assert_eq!(sonnet["output_config"], json!({ "effort": "xhigh" }));
        const SONNET_5_CAP: u32 = 128000;
        assert_eq!(sonnet["max_tokens"], json!(SONNET_5_CAP));
        assert!(sonnet["thinking"].get("budget_tokens").is_none());

        // Opus 4.8 is adaptive too.
        let body = build_anthropic_request(
            "claude-opus-4-8",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::High),
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert_eq!(
            body["thinking"],
            json!({ "type": "adaptive", "display": "summarized" })
        );
        assert_eq!(body["output_config"], json!({ "effort": "xhigh" }));
        // Opus 4.8 output cap is the base/ceiling max_tokens for adaptive too.
        const OPUS_CAP: u32 = 128000;
        assert_eq!(
            body["max_tokens"],
            json!(OPUS_CAP),
            "adaptive max_tokens is the model output cap"
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
            let req = build_anthropic_request(
                "claude-opus-4-7",
                "P",
                &messages,
                &tools,
                Some(level),
                PromptCacheRetention::Short,
                &ContextManagement::default(),
            );
            assert_eq!(
                req["output_config"],
                json!({ "effort": expected }),
                "{level:?} -> {expected}"
            );
            assert_eq!(req["thinking"]["type"], json!("adaptive"));
        }

        // Adaptive model with no preference / explicit Off both omit thinking.
        let none = build_anthropic_request(
            "claude-opus-4-8",
            "P",
            &messages,
            &tools,
            None,
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert!(none.get("thinking").is_none());
        assert!(none.get("output_config").is_none());
        assert_eq!(none["max_tokens"], json!(OPUS_CAP));
        let off = build_anthropic_request(
            "claude-opus-4-8",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::Off),
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert!(off.get("thinking").is_none(), "adaptive Off omits thinking");
        assert!(off.get("output_config").is_none());
    }

    #[test]
    fn opus_4_7_sends_its_own_id() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());
        let body = build_anthropic_request(
            "claude-opus-4-7",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::Medium),
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert_eq!(
            body["model"],
            json!("claude-opus-4-7"),
            "the selected model id is sent verbatim"
        );
        // Adaptive thinking with medium -> high effort.
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
            PromptCacheRetention::Short,
            &ContextManagement::default(),
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
            PromptCacheRetention::Short,
            &ContextManagement::default(),
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
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert!(opus.get("fallbacks").is_none());
    }

    #[test]
    fn fallback_payload_helpers_detect_and_remove_fallbacks() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());
        let mut request = build_anthropic_request(
            "claude-fable-5",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::High),
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert!(request_has_fallbacks(&request));
        assert!(remove_fallbacks(&mut request));
        assert!(!request_has_fallbacks(&request));
    }

    #[test]
    fn fallback_rejection_detection_is_local_and_specific() {
        let body = r#"{"error":{"type":"invalid_request_error","message":"fallbacks: Extra inputs are not permitted"}}"#;
        assert!(is_server_side_fallback_rejection(400, body));
        assert!(!is_server_side_fallback_rejection(
            400,
            r#"{"error":{"message":"other"}}"#
        ));
        assert!(!is_server_side_fallback_rejection(500, body));
    }

    #[test]
    fn fallback_text_buffering_follows_actual_request_payload() {
        struct RecordingSink {
            deltas: Vec<String>,
        }
        impl TurnSink for RecordingSink {
            fn on_text_delta(&mut self, delta: &str) -> Result<()> {
                self.deltas.push(delta.to_string());
                Ok(())
            }
        }
        let mut parser = AnthropicStreamParser::new(anthropic_origin("claude-fable-5"), false);
        let mut sink = RecordingSink { deltas: Vec::new() };
        parser
            .ingest_event(
                r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-fable-5"}}"#,
                &mut sink,
            )
            .unwrap();
        parser
            .ingest_event(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
                &mut sink,
            )
            .unwrap();
        parser
            .ingest_event(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"live"}}"#,
                &mut sink,
            )
            .unwrap();
        assert_eq!(sink.deltas, ["live"]);
        assert!(parser.emitted_visible_text);
    }

    #[test]
    fn thinking_deltas_stream_live_and_persist_once() {
        // Anthropic extended-thinking summary deltas are forwarded live to the
        // reasoning rail AND the final block is still persisted exactly once.
        #[derive(Default)]
        struct RecordingSink {
            reasoning: Vec<String>,
            section_breaks: usize,
        }
        impl TurnSink for RecordingSink {
            fn on_text_delta(&mut self, _delta: &str) -> Result<()> {
                Ok(())
            }
            fn on_reasoning_delta(&mut self, delta: &str) -> Result<()> {
                self.reasoning.push(delta.to_string());
                Ok(())
            }
            fn on_reasoning_section_break(&mut self) -> Result<()> {
                self.section_breaks += 1;
                Ok(())
            }
        }
        let body = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Plan: \"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"inspect then edit\"}}

data: {\"type\":\"content_block_stop\",\"index\":0}

data: {\"type\":\"message_stop\"}

";
        let mut parser = AnthropicStreamParser::new(anthropic_origin("claude-opus-4-8"), false);
        let mut sink = RecordingSink::default();
        for_each_sse_event(body.as_bytes(), &CancellationToken::new(), |data| {
            parser.ingest_event(data, &mut sink)
        })
        .expect("stream parses");
        assert_eq!(sink.reasoning, ["Plan: ", "inspect then edit"]);
        assert_eq!(sink.section_breaks, 0);
        assert!(parser.emitted_visible_output());
        assert!(
            !parser.emitted_visible_text,
            "reasoning is not assistant text"
        );
        let turn = parser.finish().expect("finish");
        assert_eq!(turn.reasoning.len(), 1);
        assert_eq!(turn.reasoning[0].text, "Plan: inspect then edit");
        assert!(!turn.reasoning[0].redacted);
    }

    #[test]
    fn redacted_thinking_is_never_streamed_live() {
        // ADR-0016: a redacted thinking block's text is never forwarded live,
        // even if a (synthetic) thinking_delta targets its index.
        #[derive(Default)]
        struct RecordingSink {
            reasoning: Vec<String>,
        }
        impl TurnSink for RecordingSink {
            fn on_text_delta(&mut self, _delta: &str) -> Result<()> {
                Ok(())
            }
            fn on_reasoning_delta(&mut self, delta: &str) -> Result<()> {
                self.reasoning.push(delta.to_string());
                Ok(())
            }
        }
        let body = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"ENC\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"must not leak\"}}

data: {\"type\":\"content_block_stop\",\"index\":0}

data: {\"type\":\"message_stop\"}

";
        let mut parser = AnthropicStreamParser::new(anthropic_origin("claude-opus-4-8"), false);
        let mut sink = RecordingSink::default();
        for_each_sse_event(body.as_bytes(), &CancellationToken::new(), |data| {
            parser.ingest_event(data, &mut sink)
        })
        .expect("stream parses");
        assert!(
            sink.reasoning.is_empty(),
            "redacted reasoning is never streamed (ADR-0016)"
        );
        assert!(!parser.emitted_visible_reasoning);
        let turn = parser.finish().expect("finish");
        assert_eq!(turn.reasoning.len(), 1);
        assert!(turn.reasoning[0].redacted);
    }

    #[test]
    fn visible_reasoning_delta_disables_silent_retry() {
        // A shown reasoning summary is visible output, so a later truncated
        // stream (protocol anomaly) is fatal, not silently retried.
        let body = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reasoning shown\"}}

";
        // No content_block_stop / message_stop -> truncated (anomalous) stream.
        let parser = parser_after(body, "claude-opus-4-8");
        let emitted = parser.emitted_visible_output();
        assert!(emitted, "reasoning was shown");
        assert!(!parser.emitted_visible_text, "but not via assistant text");
        let err = parser.finish().unwrap_err();
        assert!(
            !protocol_anomaly_retryable(&err, emitted),
            "a protocol anomaly after visible reasoning must not be silently retried"
        );
    }

    #[test]
    fn pre_fallback_reasoning_is_withheld_until_the_boundary() {
        // A refusal-fallback model buffers reasoning like text until the
        // fallback boundary: pre-boundary thinking a fallback would discard is
        // never streamed; post-boundary reasoning streams normally.
        #[derive(Default)]
        struct RecordingSink {
            reasoning: Vec<String>,
        }
        impl TurnSink for RecordingSink {
            fn on_text_delta(&mut self, _delta: &str) -> Result<()> {
                Ok(())
            }
            fn on_reasoning_delta(&mut self, delta: &str) -> Result<()> {
                self.reasoning.push(delta.to_string());
                Ok(())
            }
        }
        let body = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"discarded preface\"}}

data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"fallback\",\"to\":{\"model\":\"claude-opus-4-8\"}}}

data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}

data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"real reasoning\"}}

data: {\"type\":\"content_block_stop\",\"index\":2}

data: {\"type\":\"message_stop\"}

";
        // `true` marks the model refusal-fallback-capable, so pre-boundary
        // output is buffered rather than streamed.
        let mut parser = AnthropicStreamParser::new(anthropic_origin("claude-fable-5"), true);
        let mut sink = RecordingSink::default();
        for_each_sse_event(body.as_bytes(), &CancellationToken::new(), |data| {
            parser.ingest_event(data, &mut sink)
        })
        .expect("stream parses");
        assert_eq!(
            sink.reasoning,
            ["real reasoning"],
            "only post-boundary reasoning streams; the discarded preface never does"
        );
        assert!(parser.emitted_visible_reasoning);
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
        let (max_tokens, budget) = resolve_manual_thinking(8192, 32768, 64000);
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
                provider_turn_id: None,
                redacted: false,
                origin: None,
                provider_blocks: Vec::new(),
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
    fn fallback_marker_drops_pre_boundary_text_reasoning_and_tool_calls() {
        let body = "\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_fallback\",\"model\":\"claude-fable-5\",\"usage\":{\"input_tokens\":1}}}\n\n
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"refusal preface\"}}\n\n
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"refused thought\"}}\n\n
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-fable\"}}\n\n
data: {\"type\":\"content_block_stop\",\"index\":1}\n\n
data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_pre\",\"name\":\"read\",\"input\":{\"path\":\"secret.rs\"}}}\n\n
data: {\"type\":\"content_block_stop\",\"index\":2}\n\n
data: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"fallback\",\"from\":{\"model\":\"claude-fable-5\"},\"to\":{\"model\":\"claude-opus-4-8\"}}}\n\n
data: {\"type\":\"content_block_stop\",\"index\":3}\n\n
data: {\"type\":\"content_block_start\",\"index\":4,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n
data: {\"type\":\"content_block_delta\",\"index\":4,\"delta\":{\"type\":\"text_delta\",\"text\":\"fallback answer\"}}\n\n
data: {\"type\":\"content_block_stop\",\"index\":4}\n\n
data: {\"type\":\"message_stop\"}\n\n
";
        let turn = parse_anthropic_sse_for_model(body, "claude-fable-5")
            .expect("fallback stream should parse");
        assert_eq!(turn.text.as_deref(), Some("fallback answer"));
        assert!(
            turn.reasoning.is_empty(),
            "pre-fallback reasoning must not replay"
        );
        assert!(
            turn.tool_calls.is_empty(),
            "pre-fallback tool calls must not execute"
        );
        assert_eq!(
            turn.usage.map(|usage| usage.model),
            Some("claude-opus-4-8".to_string())
        );
    }

    #[test]
    fn response_model_rekeys_reasoning_origin() {
        let body = "\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-opus-4-8\"}}\n\n
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"opus thought\"}}\n\n
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-opus\"}}\n\n
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n
data: {\"type\":\"message_stop\"}\n\n
";
        let turn =
            parse_anthropic_sse_for_model(body, "claude-fable-5").expect("stream should parse");
        assert_eq!(turn.reasoning.len(), 1);
        assert_eq!(turn.reasoning[0].origin.model, "claude-opus-4-8");
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
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        let assistant = &request["messages"].as_array().unwrap()[1];
        let blocks = assistant["content"].as_array().unwrap();

        assert_eq!(
            blocks[0],
            json!({ "type": "thinking", "thinking": "", "signature": "sig-empty" })
        );
        assert_eq!(
            blocks[1],
            json!({ "type": "redacted_thinking", "data": "opaque-same" })
        );
        assert_eq!(blocks[2], json!({ "type": "text", "text": "answer" }));
        assert_eq!(
            blocks.len(),
            3,
            "foreign visible and redacted thinking are both dropped (ADR-0041)"
        );
    }

    #[test]
    fn same_origin_signatureless_reasoning_degrades_to_text() {
        // A same-origin visible row without a signature cannot replay as a
        // `thinking` block, so it degrades to text; a foreign row never does.
        let same = ModelOrigin::new("anthropic", "anthropic-messages", "claude-sonnet-4-6");
        let messages = vec![
            Message::user("go"),
            Message::assistant_reasoning_block(crate::nexus::ReasoningBlock::new(
                "kept same-origin thought",
                None,
                false,
                same,
            )),
            Message::assistant("answer"),
        ];
        let msgs = build_messages(&messages, &anthropic_origin("claude-sonnet-4-6"));
        let blocks = msgs[1]["content"].as_array().unwrap();
        assert_eq!(
            blocks[0],
            json!({ "type": "text", "text": "kept same-origin thought" })
        );
        assert_eq!(blocks[1], json!({ "type": "text", "text": "answer" }));
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
                provider_turn_id: None,
                redacted: false,
                origin: None,
                provider_blocks: Vec::new(),
            },
            Message {
                role: Role::AssistantToolCall,
                content: "{\"path\":\"b\"}".to_string(),
                tool_call_id: Some("toolu_2".to_string()),
                tool_name: Some("read".to_string()),
                continuity: None,
                provider_turn_id: None,
                redacted: false,
                origin: None,
                provider_blocks: Vec::new(),
            },
            Message {
                role: Role::Tool,
                content: "A".to_string(),
                tool_call_id: Some("toolu_1".to_string()),
                tool_name: Some("read".to_string()),
                continuity: None,
                provider_turn_id: None,
                redacted: false,
                origin: None,
                provider_blocks: Vec::new(),
            },
            Message {
                role: Role::Tool,
                content: "B".to_string(),
                tool_call_id: Some("toolu_2".to_string()),
                tool_name: Some("read".to_string()),
                continuity: None,
                provider_turn_id: None,
                redacted: false,
                origin: None,
                provider_blocks: Vec::new(),
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
    fn api_key_lane_sends_x_api_key_and_omits_claude_code_identity() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());
        let body = build_anthropic_request_for_auth(
            "claude-sonnet-4-6",
            "P",
            &messages,
            &tools,
            None,
            AnthropicRequestConfig {
                cache_retention: PromptCacheRetention::Short,
                context_management: &ContextManagement::default(),
                auth_kind: AnthropicAuthKind::ApiKey,
            },
        );
        let system = body["system"].as_array().unwrap();
        assert_eq!(
            system.len(),
            1,
            "API-key requests do not need Claude Code identity"
        );
        assert_eq!(system[0]["text"], json!("P"));

        let headers =
            anthropic_headers_for_auth(&AnthropicAuth::ApiKey("sk-ant".to_string()), &body)
                .expect("headers");
        assert_eq!(
            headers.get("x-api-key").unwrap().to_str().unwrap(),
            "sk-ant"
        );
        assert!(headers.get(AUTHORIZATION).is_none());
        assert_eq!(auth_kind_label(&headers), "api_key");
        assert_eq!(
            headers.get("anthropic-version").unwrap().to_str().unwrap(),
            ANTHROPIC_VERSION
        );
        let beta = headers.get("anthropic-beta").unwrap().to_str().unwrap();
        assert!(
            beta.contains(CLAUDE_CODE_BETA),
            "API-key lane keeps Claude Code tool-format beta"
        );
        assert!(
            !beta.contains("oauth"),
            "API-key lane must not send OAuth-only beta"
        );
    }

    #[test]
    fn headers_carry_oauth_betas_and_never_an_api_key() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());
        let body = build_anthropic_request(
            "claude-opus-4-8",
            "P",
            &messages,
            &tools,
            None,
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
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
            PromptCacheRetention::Short,
            &ContextManagement::default(),
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
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert!(!beta_of(&adaptive).contains(INTERLEAVED_THINKING_BETA));

        // No thinking -> base betas only.
        let plain = build_anthropic_request(
            "claude-opus-4-8",
            "P",
            &messages,
            &tools,
            None,
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
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
            PromptCacheRetention::Short,
            &ContextManagement::default(),
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
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert!(!anthropic_beta(&opus).contains(SERVER_SIDE_FALLBACK_BETA));
    }

    /// Drive `body` through the parser and return it for inspection (open block
    /// state, emitted-text flag) before `finish()` consumes it.
    fn parser_after(body: &str, model: &str) -> AnthropicStreamParser {
        struct NoopSink;
        impl TurnSink for NoopSink {
            fn on_text_delta(&mut self, _delta: &str) -> Result<()> {
                Ok(())
            }
        }
        let mut parser = AnthropicStreamParser::new(
            anthropic_origin(model),
            anthropic_models::find(model)
                .and_then(|model| model.refusal_fallback)
                .is_some(),
        );
        let mut sink = NoopSink;
        for_each_sse_event(body.as_bytes(), &CancellationToken::new(), |data| {
            parser.ingest_event(data, &mut sink)
        })
        .expect("events ingest without error");
        parser
    }

    #[test]
    fn missing_content_block_stop_on_open_tool_use_is_recoverable_anomaly() {
        // message_stop arrives while a tool_use block is still open: malformed.
        let body = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}

data: {\"type\":\"message_stop\"}

";
        let parser = parser_after(body, "claude-opus-4-8");
        // Tool input is buffered, never streamed live -> safe to retry.
        assert!(!parser.emitted_visible_text);
        let err = parser.finish().unwrap_err();
        let anomaly = err
            .downcast_ref::<StreamProtocolAnomaly>()
            .expect("typed protocol anomaly");
        assert!(anomaly.message_stop_seen);
        assert_eq!(anomaly.open_tool_blocks, 1);
        assert_eq!(anomaly.open_reasoning_blocks, 0);
        assert_eq!(anomaly.open_block_indexes, vec![0]);
        assert_eq!(anomaly.last_event_type.as_deref(), Some("message_stop"));
        let rendered = err.to_string();
        assert!(rendered.contains("content_block_stop"), "{rendered}");
        // The malformed anomaly with no visible text is retryable.
        assert!(protocol_anomaly_retryable(&err, false));
        assert!(
            !protocol_anomaly_retryable(&err, true),
            "already-streamed text disables retry"
        );
    }

    #[test]
    fn missing_content_block_stop_on_open_thinking_and_redacted_is_recoverable_anomaly() {
        // Two open reasoning blocks (thinking + redacted_thinking) at message_stop.
        let body = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"hmm\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-a\"}}

data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque\"}}

data: {\"type\":\"message_stop\"}

";
        let parser = parser_after(body, "claude-opus-4-8");
        assert!(
            !parser.emitted_visible_text,
            "reasoning is not streamed live"
        );
        let err = parser.finish().unwrap_err();
        let anomaly = err
            .downcast_ref::<StreamProtocolAnomaly>()
            .expect("typed protocol anomaly");
        assert!(anomaly.message_stop_seen);
        assert_eq!(anomaly.open_tool_blocks, 0);
        assert_eq!(anomaly.open_reasoning_blocks, 2);
        assert_eq!(anomaly.open_block_indexes, vec![0, 1]);
        assert!(protocol_anomaly_retryable(&err, false));
    }

    #[test]
    fn missing_message_stop_is_recoverable_anomaly_with_safe_metadata() {
        // Stream truncated before the terminal message_stop.
        let body = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}

";
        let parser = parser_after(body, "claude-opus-4-8");
        // Visible text WAS streamed: the anomaly must NOT be retried.
        assert!(parser.emitted_visible_text);
        let err = parser.finish().unwrap_err();
        let anomaly = err
            .downcast_ref::<StreamProtocolAnomaly>()
            .expect("typed protocol anomaly");
        assert!(!anomaly.message_stop_seen);
        assert!(err.to_string().contains("message_stop"));
        assert!(
            !protocol_anomaly_retryable(&err, true),
            "text already streamed -> no retry"
        );
    }

    #[test]
    fn malformed_stream_after_visible_text_is_not_retryable() {
        // Visible text streams, then a tool_use block is left open at
        // message_stop. A retry would replay the already-shown text, so this
        // malformed stream must be classified non-retryable even though it is a
        // typed protocol anomaly.
        let body = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial answer\"}}

data: {\"type\":\"content_block_stop\",\"index\":0}

data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\"}}

data: {\"type\":\"message_stop\"}

";
        let parser = parser_after(body, "claude-opus-4-8");
        let emitted_visible_text = parser.emitted_visible_text;
        assert!(
            emitted_visible_text,
            "a text_delta was streamed to the consumer"
        );
        let err = parser.finish().unwrap_err();
        // It IS a typed protocol anomaly...
        assert!(
            err.downcast_ref::<StreamProtocolAnomaly>().is_some(),
            "open tool block at message_stop is a protocol anomaly"
        );
        // ...but having already shown text, it must NOT be retried.
        assert!(
            !protocol_anomaly_retryable(&err, emitted_visible_text),
            "anomaly after visible text must not be retryable"
        );
    }

    #[test]
    fn empty_turn_with_valid_stop_reason_completes() {
        // A terminal stream with a valid stop_reason but no content blocks is a
        // legitimate empty completion, not an error.
        for reason in [
            "end_turn",
            "tool_use",
            "stop_sequence",
            "pause_turn",
            "refusal",
        ] {
            let body = format!(
                "\
data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_1\"}}}}

data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{reason}\"}},\"usage\":{{\"output_tokens\":0}}}}

data: {{\"type\":\"message_stop\"}}

"
            );
            let turn = parse_anthropic_sse(&body)
                .unwrap_or_else(|e| panic!("empty {reason} should complete: {e}"));
            assert!(turn.text.is_none(), "{reason}: no text");
            assert!(turn.tool_calls.is_empty(), "{reason}: no tool calls");
            assert!(turn.reasoning.is_empty(), "{reason}: no reasoning");
            assert_eq!(turn.completion_reason, Some(map_stop_reason(reason)));
        }
    }

    #[test]
    fn empty_turn_without_stop_reason_is_error() {
        // No content AND no stop reason: still treated as a malformed/empty
        // response.
        let body = "\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}

data: {\"type\":\"message_stop\"}

";
        let error = parse_anthropic_sse(body).unwrap_err().to_string();
        assert!(error.contains("did not include assistant"), "got: {error}");
    }

    #[test]
    fn incomplete_tool_json_is_not_a_recoverable_anomaly() {
        // A CLOSED tool block with invalid JSON is a hard parse failure, not a
        // recoverable stream anomaly, and must never be retried/executed.
        let body = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\"}}

data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}

data: {\"type\":\"content_block_stop\",\"index\":0}

data: {\"type\":\"message_stop\"}

";
        let err = parse_anthropic_sse(body).unwrap_err();
        assert!(err.downcast_ref::<StreamProtocolAnomaly>().is_none());
        assert!(!protocol_anomaly_retryable(&err, false));
    }

    #[test]
    fn stop_reason_is_captured_and_mapped_provider_neutrally() {
        let turn_with_reason = |reason: &str| {
            let body = format!(
                "\
data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}

data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"hi\"}}}}

data: {{\"type\":\"content_block_stop\",\"index\":0}}

data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{reason}\"}},\"usage\":{{\"output_tokens\":3}}}}

data: {{\"type\":\"message_stop\"}}

"
            );
            parse_anthropic_sse(&body).unwrap().completion_reason
        };
        // max_tokens / model_context_window_exceeded are no longer dropped.
        assert_eq!(
            turn_with_reason("max_tokens"),
            Some(CompletionReason::MaxOutputTokens)
        );
        assert_eq!(
            turn_with_reason("model_context_window_exceeded"),
            Some(CompletionReason::ContextWindowExceeded)
        );
        assert_eq!(
            turn_with_reason("end_turn"),
            Some(CompletionReason::EndTurn)
        );
        assert_eq!(
            turn_with_reason("tool_use"),
            Some(CompletionReason::ToolUse)
        );
        assert_eq!(
            turn_with_reason("stop_sequence"),
            Some(CompletionReason::StopSequence)
        );
        assert_eq!(
            turn_with_reason("pause_turn"),
            Some(CompletionReason::Paused)
        );
        assert_eq!(turn_with_reason("refusal"), Some(CompletionReason::Refusal));
        assert_eq!(
            turn_with_reason("some_future_reason"),
            Some(CompletionReason::Other)
        );
    }

    #[test]
    fn map_stop_reason_covers_every_known_value() {
        assert_eq!(map_stop_reason("end_turn"), CompletionReason::EndTurn);
        assert_eq!(map_stop_reason("tool_use"), CompletionReason::ToolUse);
        assert_eq!(
            map_stop_reason("max_tokens"),
            CompletionReason::MaxOutputTokens
        );
        assert_eq!(
            map_stop_reason("model_context_window_exceeded"),
            CompletionReason::ContextWindowExceeded
        );
        assert_eq!(
            map_stop_reason("stop_sequence"),
            CompletionReason::StopSequence
        );
        assert_eq!(map_stop_reason("pause_turn"), CompletionReason::Paused);
        assert_eq!(map_stop_reason("refusal"), CompletionReason::Refusal);
        assert_eq!(map_stop_reason("weird"), CompletionReason::Other);
    }

    #[test]
    fn output_cap_drives_max_tokens_with_conservative_unknown_fallback() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());
        // Known subscription model -> its output cap (Opus 4.8 = 128k).
        let opus = build_anthropic_request(
            "claude-opus-4-8",
            "P",
            &messages,
            &tools,
            None,
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert_eq!(opus["max_tokens"], json!(128000));
        // Unknown model -> conservative DEFAULT_OUTPUT_CAP fallback (64k), never
        // the old fixed 8192.
        let unknown = build_anthropic_request(
            "claude-some-unreleased-model",
            "P",
            &messages,
            &tools,
            None,
            PromptCacheRetention::Short,
            &ContextManagement::default(),
        );
        assert_eq!(unknown["max_tokens"], json!(DEFAULT_OUTPUT_CAP));
    }

    #[test]
    fn protocol_anomaly_diagnostics_carry_no_streamed_content() {
        // The anomaly Display must include block-shape metadata only.
        let anomaly = StreamProtocolAnomaly {
            message_stop_seen: true,
            open_tool_blocks: 1,
            open_reasoning_blocks: 0,
            open_block_indexes: vec![2],
            last_event_type: Some("message_stop".to_string()),
        };
        let rendered = anomaly.to_string();
        assert!(rendered.contains("content_block_stop"));
        assert!(rendered.contains("open_tool_blocks=1"));
        assert!(rendered.contains("open_reasoning_blocks=0"));
        assert!(rendered.contains("open_block_indexes=2"));
        assert!(rendered.contains("last_event=message_stop"));
    }

    #[test]
    fn developer_context_maps_to_a_user_text_block() {
        let messages = build_messages(
            &[Message::developer("skill catalog"), Message::user("task")],
            &anthropic_origin("m"),
        );

        assert_eq!(messages.len(), 1, "consecutive user blocks coalesce");
        assert_eq!(messages[0]["role"], json!("user"));
        assert_eq!(messages[0]["content"][0]["text"], json!("skill catalog"));
        assert_eq!(messages[0]["content"][1]["text"], json!("task"));
    }
}
