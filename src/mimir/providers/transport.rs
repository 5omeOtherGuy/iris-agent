//! Shared provider transport: the `spawn_blocking` + channel streaming glue, a
//! one-shot reauth control flow, HTTP retry classification, and an SSE event
//! splitter. Used by the streaming providers so each adapter only owns its wire
//! format (request build, message mapping, SSE event parsing) and not the
//! executor/cancellation plumbing.
//!
//! Two control flows are offered: `run_with_reauth` (one-shot reauth only, used
//! by Antigravity) and `run_with_retry` (bounded exponential backoff for
//! transient network/429/5xx/stream anomalies plus the one-shot reauth, used by
//! Anthropic). The transient budget is bounded by the shared
//! [`RetryPolicy`](crate::mimir::retry::RetryPolicy) and is not reset by the
//! reauth.

use std::io::{BufRead, Error as IoError};
use std::sync::OnceLock;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use futures::StreamExt;
use futures::channel::mpsc;
use reqwest::blocking::Client;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use tokio_util::sync::CancellationToken;

use crate::mimir::retry::RetryPolicy;
use crate::nexus::{AssistantTurn, ProviderEvent, ProviderStream};

/// How long to wait for a TCP connect + TLS handshake before classifying the
/// attempt as transient (the retry loop then backs off and retries).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Total-request backstop. Generous enough that no legitimate turn hits it
/// (the hardest single requests run ~15 minutes; provider SDKs default to a
/// 10-minute total timeout), but finite so a provider that accepts the
/// request and then stalls with a healthy TCP connection cannot pin the
/// `spawn_blocking` thread and pooled connection forever -- a cancelled turn
/// cannot wake a blocking socket read, and TCP keepalive only detects dead
/// peers, not silent ones. Replaces the old 120s timeout, which killed
/// legitimate long streams.
const TOTAL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// Provider-event idle timeout. The total timeout above bounds the whole
/// blocking request, but without an idle timeout a provider can accept an SSE
/// request and then leave the UI spinning with no bytes until the 30-minute
/// backstop. Streaming providers send SSE events/pings while alive, so this is
/// an inactivity detector rather than a cap on long legitimate turns.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Keep pooled connections around across turns. Idle gaps between turns (the
/// user reading/typing, long tool runs) routinely exceed reqwest's 90s default,
/// which forced a fresh TCP+TLS handshake on the next turn's first token.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// TCP keepalive cadence: first probe after this idle period, then one probe
/// per [`TCP_KEEPALIVE_INTERVAL`], giving up after [`TCP_KEEPALIVE_RETRIES`]
/// unanswered probes. Keeps NATs/load-balancers from silently dropping the
/// idle pooled connection between turns and detects a dead socket (~60s)
/// instead of blocking a stream read indefinitely.
const TCP_KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const TCP_KEEPALIVE_RETRIES: u32 = 3;

/// Process-wide HTTP client shared by every provider adapter (and their OAuth
/// token refreshes). One client means one connection pool: switching models or
/// providers mid-session reuses warm connections instead of re-handshaking,
/// and a token refresh rides the same pool as the chat request.
///
/// Tuned for streaming SSE responsiveness: ALPN-negotiated HTTP/2 with an
/// adaptive flow-control window (no stalls on fast token streams), TCP
/// keepalive probes (warm, validated connection between turns), TCP_NODELAY
/// (no Nagle delay on request writes), and a total request timeout that is a
/// backstop rather than a cap on legitimate turns. Long turns (extended
/// thinking, large outputs) legitimately stream for many minutes; the old
/// per-provider 120s whole-request timeout killed any stream that outlived
/// it. Hang detection comes from the connect timeout, the TCP keepalive
/// probes (dead peer), and [`TOTAL_REQUEST_TIMEOUT`] (silent-but-alive peer);
/// a cancelled turn stops consuming the stream immediately either way.
pub(crate) fn shared_client() -> Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            Client::builder()
                .timeout(TOTAL_REQUEST_TIMEOUT)
                .connect_timeout(CONNECT_TIMEOUT)
                .pool_idle_timeout(POOL_IDLE_TIMEOUT)
                .tcp_nodelay(true)
                .tcp_keepalive(TCP_KEEPALIVE_IDLE)
                .tcp_keepalive_interval(TCP_KEEPALIVE_INTERVAL)
                .tcp_keepalive_retries(TCP_KEEPALIVE_RETRIES)
                .http2_adaptive_window(true)
                .build()
                // Static configuration over a compiled-in TLS backend: this
                // cannot fail at runtime for environment-specific reasons.
                .expect("failed to build shared HTTP client")
        })
        .clone()
}

/// Provider-internal seam for incremental assistant text. The streamed SSE
/// parser pushes deltas here; the live provider forwards them onto the
/// [`ProviderStream`] channel, while tests use a recording/no-op sink.
pub(super) trait TurnSink {
    /// Forward provider activity that does not yet have user-visible text. Used
    /// to keep idle detection from timing out live streams that are currently
    /// sending buffered reasoning/tool-call-input frames.
    fn on_activity(&mut self) -> Result<()> {
        Ok(())
    }

    /// Forward one text delta. Returns `Err` when the consumer dropped the
    /// stream (cancellation) so the SSE read loop stops early instead of
    /// draining the rest of the response on a leaked blocking thread.
    fn on_text_delta(&mut self, delta: &str) -> Result<()>;

    /// Forward one reasoning-*summary* delta (display-only; never raw
    /// chain-of-thought or encrypted content). Default no-op so providers and
    /// test sinks that do not stream reasoning are unaffected.
    fn on_reasoning_delta(&mut self, _delta: &str) -> Result<()> {
        Ok(())
    }

    /// Forward a boundary between two reasoning-summary parts (a blank line in
    /// the live trace). Default no-op.
    fn on_reasoning_section_break(&mut self) -> Result<()> {
        Ok(())
    }
}

/// [`TurnSink`] that forwards each text delta onto the provider's event channel.
pub(super) struct ChannelSink {
    tx: mpsc::UnboundedSender<Result<ProviderEvent>>,
}

impl TurnSink for ChannelSink {
    fn on_activity(&mut self) -> Result<()> {
        self.tx
            .unbounded_send(Ok(ProviderEvent::Activity))
            .map_err(|_| anyhow!("response stream dropped by consumer"))
    }

    fn on_text_delta(&mut self, delta: &str) -> Result<()> {
        self.tx
            .unbounded_send(Ok(ProviderEvent::TextDelta(delta.to_string())))
            .map_err(|_| anyhow!("response stream dropped by consumer"))
    }

    fn on_reasoning_delta(&mut self, delta: &str) -> Result<()> {
        self.tx
            .unbounded_send(Ok(ProviderEvent::ReasoningDelta(delta.to_string())))
            .map_err(|_| anyhow!("response stream dropped by consumer"))
    }

    fn on_reasoning_section_break(&mut self) -> Result<()> {
        self.tx
            .unbounded_send(Ok(ProviderEvent::ReasoningSectionBreak))
            .map_err(|_| anyhow!("response stream dropped by consumer"))
    }
}

/// Spawn the blocking request + SSE parse on the runtime's blocking pool,
/// streaming text deltas through the channel and ending with exactly one
/// terminal item (the assembled turn, or an error). `run` must be `'static +
/// Send`: capture an owned request `Value` and a cloned provider, never a borrow
/// of `self`/`messages`/`tools`.
pub(super) fn spawn_stream(
    run: impl FnOnce(&mut ChannelSink, &CancellationToken) -> Result<AssistantTurn> + Send + 'static,
    cancel: CancellationToken,
) -> ProviderStream<'static> {
    let (tx, rx) = mpsc::unbounded::<Result<ProviderEvent>>();
    let stream_cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
        let mut sink = ChannelSink { tx: tx.clone() };
        let terminal = match run(&mut sink, &cancel) {
            Ok(turn) => Ok(ProviderEvent::Completed(turn)),
            Err(error) => Err(error),
        };
        let _ = tx.unbounded_send(terminal);
    });
    Box::pin(futures::stream::unfold(
        (rx, stream_cancel),
        |(mut rx, cancel)| async move {
            match tokio::time::timeout(STREAM_IDLE_TIMEOUT, rx.next()).await {
                Ok(Some(item)) => Some((item, (rx, cancel))),
                Ok(None) => None,
                Err(_) => {
                    cancel.cancel();
                    Some((
                        Err(anyhow!(
                            "provider stream produced no events for {}s",
                            STREAM_IDLE_TIMEOUT.as_secs()
                        )),
                        (rx, cancel),
                    ))
                }
            }
        },
    ))
}

/// Outcome of a single HTTP attempt, classified for [`run_with_reauth`] /
/// [`run_with_retry`].
pub(super) enum Attempt {
    Done(Box<AssistantTurn>),
    /// Auth rejected (401/403): force one token refresh, then retry once.
    Reauth(anyhow::Error),
    /// Transient failure (network / 429 / 5xx / recoverable stream anomaly):
    /// retry with bounded backoff. Carries any server `Retry-After` hint. Only
    /// honored by [`run_with_retry`]; [`run_with_reauth`] treats it as fatal.
    Retry(anyhow::Error, Option<Duration>),
    /// Anything else non-retryable here: surface immediately.
    Fatal(anyhow::Error),
}

/// HTTP status retry classification shared by the providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HttpClass {
    Reauth,
    /// Transient (network/429/5xx): retry with backoff.
    Retry,
    Fatal,
}

/// Classify an HTTP status into a (reauth-or-fatal) policy class. Transient
/// (429/5xx) is treated as fatal here -- the reauth-only callers (Antigravity)
/// do not retry them. Callers that want bounded transient retry use
/// [`classify_http_status_retryable`] with [`run_with_retry`].
pub(super) fn classify_http_status(status: u16) -> HttpClass {
    match status {
        401 | 403 => HttpClass::Reauth,
        _ => HttpClass::Fatal,
    }
}

/// Classify an HTTP status into a reauth/retry/fatal policy class for callers
/// driving [`run_with_retry`]. 408/425/429 and 5xx are transient (retryable);
/// 401/403 trigger one-shot reauth; everything else is fatal. Mirrors the Codex
/// adapter's classification.
pub(super) fn classify_http_status_retryable(status: u16) -> HttpClass {
    match status {
        401 | 403 => HttpClass::Reauth,
        408 | 425 | 429 => HttpClass::Retry,
        500..=599 => HttpClass::Retry,
        _ => HttpClass::Fatal,
    }
}

/// Drive the one-shot reauth control flow: attempt with a cached token; on an
/// auth rejection force exactly one token refresh and retry once; otherwise
/// return. `get_token(force_refresh)` obtains a token (cached, or forcibly
/// refreshed); `send` performs one attempt. Termination is guaranteed: reauth
/// fires at most once.
pub(super) fn run_with_reauth<T>(
    provider: &str,
    cancel: &CancellationToken,
    mut get_token: impl FnMut(bool) -> Result<T>,
    mut send: impl FnMut(&T) -> Attempt,
) -> Result<AssistantTurn> {
    let mut force_refresh = false;
    let mut reauth_used = false;
    loop {
        if cancel.is_cancelled() {
            bail!("turn cancelled");
        }
        let token = get_token(force_refresh).map_err(|error| {
            tracing::error!(error = %format!("{error:#}"), "failed to obtain access token");
            auth_error(provider, &error)
        })?;
        // Unlike `retry_loop`, this loop deliberately does NOT reset
        // `force_refresh` after acquisition: its only loop-back (the `Reauth`
        // arm) sets `force_refresh = true` and `reauth_used = true`, so a refresh
        // happens at most once and the flag is never stale on a later read. A
        // reset here would be dead code (clippy `unused_assignments`).
        if cancel.is_cancelled() {
            bail!("turn cancelled");
        }
        match send(&token) {
            Attempt::Done(turn) => return Ok(*turn),
            Attempt::Reauth(error) => {
                if reauth_used {
                    tracing::error!(error = %format!("{error:#}"), "auth rejected after refresh");
                    return Err(auth_error(provider, &error).into());
                }
                if cancel.is_cancelled() {
                    bail!("turn cancelled");
                }
                reauth_used = true;
                force_refresh = true;
                tracing::warn!(error = %format!("{error:#}"), "auth rejected; refreshing token and retrying");
            }
            // The reauth-only loop does not retry transient failures; surface
            // them. Antigravity never constructs `Retry`, so this is unreachable
            // for it and only exists for exhaustiveness.
            Attempt::Retry(error, _) => return Err(error),
            Attempt::Fatal(error) => return Err(error),
        }
    }
}

/// Drive a bounded transient-retry control flow with a one-shot reauth, for
/// callers that classify transient HTTP / network / recoverable stream anomalies
/// as [`Attempt::Retry`]. `get_token(force_refresh)` obtains a token (cached, or
/// forcibly refreshed after an auth rejection); `send` performs one attempt.
///
/// Termination is guaranteed: reauth fires at most once, transient retries are
/// bounded by `policy.max_retries`, the reauth does NOT reset the transient
/// budget, and every other branch returns. Backoff sleeps are sliced so a
/// cancelled turn stops promptly instead of waiting out the full delay.
pub(super) fn run_with_retry<T>(
    provider: &str,
    policy: &RetryPolicy,
    cancel: &CancellationToken,
    get_token: impl FnMut(bool) -> Result<T>,
    send: impl FnMut(&T) -> Attempt,
) -> Result<AssistantTurn> {
    retry_loop(
        provider,
        policy,
        get_token,
        send,
        // Sleep in slices so a turn-level cancel interrupts retry backoff.
        |delay| sleep_cancellable(delay, cancel),
        || cancel.is_cancelled(),
    )
}

/// Pure retry/reauth state machine, free of timing and the cancellation token so
/// it can be unit-tested with scripted closures. `sleep` applies a backoff delay;
/// `is_cancelled` reports turn cancellation (checked before each attempt and
/// after each backoff sleep).
fn retry_loop<T>(
    provider: &str,
    policy: &RetryPolicy,
    mut get_token: impl FnMut(bool) -> Result<T>,
    mut send: impl FnMut(&T) -> Attempt,
    mut sleep: impl FnMut(Duration),
    is_cancelled: impl Fn() -> bool,
) -> Result<AssistantTurn> {
    let mut transient_retries: u32 = 0;
    let mut reauth_used = false;
    let mut force_refresh = false;
    loop {
        if is_cancelled() {
            bail!("turn cancelled");
        }
        let token = get_token(force_refresh).map_err(|error| {
            tracing::error!(error = %format!("{error:#}"), "failed to obtain access token");
            auth_error(provider, &error)
        })?;
        // Reset immediately after acquisition: a later transient retry must not
        // keep force-refreshing the token.
        force_refresh = false;
        if is_cancelled() {
            bail!("turn cancelled");
        }
        match send(&token) {
            Attempt::Done(turn) => return Ok(*turn),
            Attempt::Reauth(error) => {
                if reauth_used {
                    tracing::error!(error = %format!("{error:#}"), "auth rejected after refresh");
                    return Err(auth_error(provider, &error).into());
                }
                if is_cancelled() {
                    bail!("turn cancelled");
                }
                reauth_used = true;
                force_refresh = true;
                tracing::warn!(error = %format!("{error:#}"), "auth rejected; refreshing token and retrying");
            }
            Attempt::Retry(error, retry_after) => {
                // The transient budget is shared across network/429/5xx/stream
                // anomalies and is NOT reset by the one-shot reauth.
                if transient_retries >= policy.max_retries {
                    tracing::error!(error = %format!("{error:#}"), retries = transient_retries, "transient error; retries exhausted");
                    return Err(error);
                }
                transient_retries += 1;
                let delay = policy.backoff_delay(transient_retries, retry_after);
                tracing::warn!(
                    error = %format!("{error:#}"),
                    attempt = transient_retries,
                    delay_ms = delay.as_millis() as u64,
                    "transient error; retrying"
                );
                sleep(delay);
            }
            Attempt::Fatal(error) => return Err(error),
        }
    }
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

/// Surface a server `Retry-After` hint from response headers (integer seconds).
/// Public seam so provider adapters can attach the hint to [`Attempt::Retry`].
pub(super) fn retry_after_hint(headers: &HeaderMap) -> Option<Duration> {
    parse_retry_after(headers)
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

/// Build an [`AuthError`](crate::errors::AuthError) that preserves the safe cause
/// chain (Mimir's auth errors already drop raw token/response bodies and Keychain
/// output) and records the failing provider so the CLI/TUI can render the right
/// re-login hint. The runtime stays free of the CLI command string itself (a
/// Tier-4 detail); the generic message is used only when no cause text exists.
fn auth_error(provider: &str, cause: &anyhow::Error) -> crate::errors::AuthError {
    let detail = format!("{cause:#}");
    let detail = detail.trim();
    let message = if detail.is_empty() {
        format!("{provider} authentication failed")
    } else {
        format!("{provider} authentication failed: {detail}")
    };
    crate::errors::AuthError::for_provider(provider, message)
}

/// Status-200 SSE body read failure. Carries only the local IO error string,
/// never response bytes.
#[derive(Debug)]
pub(super) struct StreamReadError {
    source: IoError,
}

impl std::fmt::Display for StreamReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to read provider stream: {}", self.source)
    }
}

impl std::error::Error for StreamReadError {}

/// Iterate SSE events from a blocking reader. Events are separated by a blank
/// line; for each event the joined `data:` payload is passed to `on_event`
/// (terminal `[DONE]` and empty payloads are skipped by the caller). `cancel`
/// is checked between lines so a cancelled turn stops draining promptly (an
/// idle socket read still blocks until the next byte or the client read
/// timeout -- blocking reqwest cannot be force-aborted mid-read). Reads into
/// two reused buffers so a high-frequency token stream does not allocate a
/// fresh `String` per line.
pub(super) fn for_each_sse_event(
    mut reader: impl BufRead,
    cancel: &CancellationToken,
    mut on_event: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let mut event = String::new();
    let mut line = String::new();
    loop {
        if cancel.is_cancelled() {
            bail!("provider stream cancelled");
        }
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|source| anyhow::Error::new(StreamReadError { source }))?;
        if read == 0 {
            break;
        }
        let content = line.trim_end_matches(['\n', '\r']);
        if content.is_empty() {
            let data = event_data(&event);
            if !data.is_empty() {
                on_event(&data)?;
            }
            event.clear();
        } else {
            event.push_str(content);
            event.push('\n');
        }
    }
    let data = event_data(&event);
    if !data.is_empty() {
        on_event(&data)?;
    }
    Ok(())
}

/// Join the `data:` lines of one SSE event into a single payload string.
fn event_data(event: &str) -> String {
    event
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reauth_fires_once_then_succeeds() {
        let mut tokens = Vec::new();
        let mut sends = 0u32;
        let cancel = CancellationToken::new();
        let turn = run_with_reauth(
            "test",
            &cancel,
            |force| {
                tokens.push(force);
                Ok(())
            },
            |&()| {
                sends += 1;
                if sends == 1 {
                    Attempt::Reauth(anyhow!("401"))
                } else {
                    Attempt::Done(Box::new(AssistantTurn {
                        text: Some("ok".to_string()),
                        reasoning: Vec::new(),
                        tool_calls: Vec::new(),
                        response_id: None,
                        usage: None,
                        completion_reason: None,
                    }))
                }
            },
        )
        .expect("reauth retry should succeed");
        assert_eq!(turn.text.as_deref(), Some("ok"));
        assert_eq!(tokens, vec![false, true], "second token is force-refreshed");
        assert_eq!(sends, 2);
    }

    #[test]
    fn cancelled_before_token_load_exits_without_getting_token() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut token_loads = 0;
        let result = run_with_reauth(
            "test",
            &cancel,
            |_force| {
                token_loads += 1;
                Ok(())
            },
            |&()| Attempt::Fatal(anyhow!("should not send")),
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(token_loads, 0);
    }

    #[test]
    fn reauth_twice_is_auth_error() {
        let cancel = CancellationToken::new();
        let result = run_with_reauth(
            "test",
            &cancel,
            |_force| Ok(()),
            |&()| Attempt::Reauth(anyhow!("401")),
        );
        let error = result.unwrap_err();
        assert!(error.downcast_ref::<crate::errors::AuthError>().is_some());
    }

    #[test]
    fn auth_error_preserves_provider_and_safe_cause() {
        let cancel = CancellationToken::new();
        let result = run_with_reauth(
            "anthropic",
            &cancel,
            |_force| Ok(()),
            |&()| Attempt::Reauth(anyhow!("HTTP 401 token rejected")),
        );
        let error = result.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("anthropic"), "provider named: {message}");
        assert!(
            message.contains("HTTP 401 token rejected"),
            "safe cause preserved: {message}"
        );
        // The CLI command string is a Tier-4 detail and must NOT be baked into
        // the runtime error; the failing provider is carried structurally.
        assert!(
            !message.contains("iris login"),
            "no CLI command in runtime: {message}"
        );
        let auth = error
            .downcast_ref::<crate::errors::AuthError>()
            .expect("auth error");
        assert_eq!(auth.provider(), Some("anthropic"));
    }

    #[test]
    fn auth_error_includes_token_load_cause_and_login_hint() {
        let cancel = CancellationToken::new();
        let result = run_with_reauth::<()>(
            "antigravity",
            &cancel,
            |_force| Err(anyhow!("Claude Code credentials could not be read")),
            |&()| {
                Attempt::Done(Box::new(AssistantTurn {
                    text: Some("unused".to_string()),
                    reasoning: Vec::new(),
                    tool_calls: Vec::new(),
                    response_id: None,
                    usage: None,
                    completion_reason: None,
                }))
            },
        );
        let error = result.unwrap_err();
        let message = error.to_string();
        assert!(message.contains("antigravity"), "{message}");
        assert!(message.contains("could not be read"), "{message}");
        assert!(
            !message.contains("iris login"),
            "no CLI command in runtime: {message}"
        );
        assert_eq!(
            error
                .downcast_ref::<crate::errors::AuthError>()
                .and_then(crate::errors::AuthError::provider),
            Some("antigravity")
        );
    }

    #[test]
    fn splits_sse_events_and_joins_data_lines() {
        let body = "data: {\"a\":1}\n\ndata: line1\ndata: line2\n\n";
        let mut seen = Vec::new();
        for_each_sse_event(body.as_bytes(), &CancellationToken::new(), |data| {
            seen.push(data.to_string());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            seen,
            vec!["{\"a\":1}".to_string(), "line1\nline2".to_string()]
        );
    }

    #[test]
    fn classifies_auth_statuses_as_reauth() {
        assert_eq!(classify_http_status(401), HttpClass::Reauth);
        assert_eq!(classify_http_status(403), HttpClass::Reauth);
        assert_eq!(classify_http_status(500), HttpClass::Fatal);
    }

    #[test]
    fn retryable_classifier_marks_transient_statuses() {
        assert_eq!(classify_http_status_retryable(401), HttpClass::Reauth);
        assert_eq!(classify_http_status_retryable(403), HttpClass::Reauth);
        assert_eq!(classify_http_status_retryable(408), HttpClass::Retry);
        assert_eq!(classify_http_status_retryable(425), HttpClass::Retry);
        assert_eq!(classify_http_status_retryable(429), HttpClass::Retry);
        assert_eq!(classify_http_status_retryable(500), HttpClass::Retry);
        assert_eq!(classify_http_status_retryable(503), HttpClass::Retry);
        assert_eq!(classify_http_status_retryable(404), HttpClass::Fatal);
        assert_eq!(classify_http_status_retryable(400), HttpClass::Fatal);
    }

    fn done_turn(text: &str) -> Attempt {
        Attempt::Done(Box::new(AssistantTurn::text(text)))
    }

    #[test]
    fn retry_loop_retries_transient_then_succeeds() {
        let mut sends = 0u32;
        let mut slept = Vec::new();
        let turn = retry_loop(
            "test",
            &RetryPolicy::default(),
            |_force| Ok(()),
            |&()| {
                sends += 1;
                if sends <= 2 {
                    Attempt::Retry(anyhow!("503"), None)
                } else {
                    done_turn("ok")
                }
            },
            |delay| slept.push(delay),
            || false,
        )
        .expect("retry then success");
        assert_eq!(turn.text.as_deref(), Some("ok"));
        assert_eq!(sends, 3);
        assert_eq!(slept.len(), 2, "slept once per transient retry");
    }

    #[test]
    fn retry_loop_exhausts_budget_and_returns_last_error() {
        let max = RetryPolicy::default().max_retries;
        let mut sends = 0u32;
        let mut slept = 0u32;
        let result = retry_loop(
            "test",
            &RetryPolicy::default(),
            |_force| Ok(()),
            |&()| {
                sends += 1;
                Attempt::Retry(anyhow!("persistent 500 protocol anomaly"), None)
            },
            |_delay| slept += 1,
            || false,
        );
        let error = result.unwrap_err();
        assert!(error.to_string().contains("protocol anomaly"), "{error}");
        // `max` retries (with sleeps) then one more attempt exhausts the budget.
        assert_eq!(sends, max + 1);
        assert_eq!(slept, max);
    }

    #[test]
    fn reauth_does_not_reset_the_transient_budget() {
        // One reauth interleaved with transient retries must not grant extra
        // transient attempts: total sends stay bounded.
        let max = RetryPolicy::default().max_retries;
        let mut sends = 0u32;
        let mut tokens = Vec::new();
        let result = retry_loop(
            "test",
            &RetryPolicy::default(),
            |force| {
                tokens.push(force);
                Ok(())
            },
            |&()| {
                sends += 1;
                match sends {
                    1 => Attempt::Reauth(anyhow!("401")),
                    _ => Attempt::Retry(anyhow!("503"), None),
                }
            },
            |_delay| {},
            || false,
        );
        assert!(result.is_err());
        // 1 reauth attempt + (`max` retries) + 1 exhausting attempt.
        assert_eq!(sends, max + 2);
        assert!(!tokens[0], "first token is cached");
        assert!(tokens[1], "reauth forces one refresh");
    }

    #[test]
    fn retry_loop_stops_when_cancelled_during_backoff() {
        // The first attempt is transient; cancellation flips before the next
        // loop iteration, so the loop bails instead of retrying forever.
        let cancelled = std::cell::Cell::new(false);
        let mut sends = 0u32;
        let result = retry_loop(
            "test",
            &RetryPolicy::default(),
            |_force| Ok(()),
            |&()| {
                sends += 1;
                Attempt::Retry(anyhow!("503"), None)
            },
            |_delay| cancelled.set(true),
            || cancelled.get(),
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(sends, 1, "no further attempts after cancellation");
    }

    #[test]
    fn sleep_cancellable_returns_promptly_when_already_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let start = Instant::now();
        sleep_cancellable(Duration::from_secs(30), &cancel);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    // Backoff growth/clamping/`Retry-After` behavior is covered by
    // `mimir::retry::tests` since the computation moved to the shared
    // `RetryPolicy`.

    #[test]
    fn parse_retry_after_reads_integer_seconds_only() {
        let mut headers = HeaderMap::new();
        assert!(parse_retry_after(&headers).is_none());
        headers.insert(RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(7)));
        headers.insert(
            RETRY_AFTER,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        assert!(parse_retry_after(&headers).is_none(), "http-date ignored");
    }
}
