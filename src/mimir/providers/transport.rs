//! Shared provider transport: the `spawn_blocking` + channel streaming glue, a
//! one-shot reauth control flow, HTTP retry classification, and an SSE event
//! splitter. Used by the streaming providers (Anthropic, Antigravity) so each
//! adapter only owns its wire format (request build, message mapping, SSE event
//! parsing) and not the executor/cancellation plumbing.
//!
//! The OpenAI Codex adapter predates this module and keeps its own (richer,
//! backoff-capable) copy; the newer providers share this leaner version.
//!
//! ponytail: one reauth, no transient backoff. The Codex adapter has the full
//! exponential-backoff loop; promote this to the shared version too if 429/5xx
//! flakiness on Anthropic/Antigravity proves it pays for itself.

use std::io::BufRead;

use anyhow::{Result, anyhow, bail};
use futures::channel::mpsc;
use tokio_util::sync::CancellationToken;

use crate::nexus::{AssistantTurn, ProviderEvent, ProviderStream};

/// Provider-internal seam for incremental assistant text. The streamed SSE
/// parser pushes deltas here; the live provider forwards them onto the
/// [`ProviderStream`] channel, while tests use a recording/no-op sink.
pub(super) trait TurnSink {
    /// Forward one text delta. Returns `Err` when the consumer dropped the
    /// stream (cancellation) so the SSE read loop stops early instead of
    /// draining the rest of the response on a leaked blocking thread.
    fn on_text_delta(&mut self, delta: &str) -> Result<()>;
}

/// [`TurnSink`] that forwards each text delta onto the provider's event channel.
pub(super) struct ChannelSink {
    tx: mpsc::UnboundedSender<Result<ProviderEvent>>,
}

impl TurnSink for ChannelSink {
    fn on_text_delta(&mut self, delta: &str) -> Result<()> {
        self.tx
            .unbounded_send(Ok(ProviderEvent::TextDelta(delta.to_string())))
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
    tokio::task::spawn_blocking(move || {
        let mut sink = ChannelSink { tx: tx.clone() };
        let terminal = match run(&mut sink, &cancel) {
            Ok(turn) => Ok(ProviderEvent::Completed(turn)),
            Err(error) => Err(error),
        };
        let _ = tx.unbounded_send(terminal);
    });
    Box::pin(rx)
}

/// Outcome of a single HTTP attempt, classified for [`run_with_reauth`].
pub(super) enum Attempt {
    Done(AssistantTurn),
    /// Auth rejected (401/403): force one token refresh, then retry once.
    Reauth(anyhow::Error),
    /// Anything else non-retryable here: surface immediately.
    Fatal(anyhow::Error),
}

/// HTTP status retry classification shared by the providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HttpClass {
    Reauth,
    Fatal,
}

/// Classify an HTTP status into a (reauth-or-fatal) policy class. Transient
/// (429/5xx) is treated as fatal here -- the leaner transport does not retry
/// them (see the module-level ponytail note).
pub(super) fn classify_http_status(status: u16) -> HttpClass {
    match status {
        401 | 403 => HttpClass::Reauth,
        _ => HttpClass::Fatal,
    }
}

/// Drive the one-shot reauth control flow: attempt with a cached token; on an
/// auth rejection force exactly one token refresh and retry once; otherwise
/// return. `get_token(force_refresh)` obtains a token (cached, or forcibly
/// refreshed); `send` performs one attempt. Termination is guaranteed: reauth
/// fires at most once.
pub(super) fn run_with_reauth<T>(
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
            crate::errors::AuthError::new("authentication failed")
        })?;
        if cancel.is_cancelled() {
            bail!("turn cancelled");
        }
        match send(&token) {
            Attempt::Done(turn) => return Ok(turn),
            Attempt::Reauth(error) => {
                if reauth_used {
                    tracing::error!(error = %format!("{error:#}"), "auth rejected after refresh");
                    return Err(crate::errors::AuthError::new("authentication failed").into());
                }
                if cancel.is_cancelled() {
                    bail!("turn cancelled");
                }
                reauth_used = true;
                force_refresh = true;
                tracing::warn!(error = %format!("{error:#}"), "auth rejected; refreshing token and retrying");
            }
            Attempt::Fatal(error) => return Err(error),
        }
    }
}

/// Iterate SSE events from a blocking reader. Events are separated by a blank
/// line; for each event the joined `data:` payload is passed to `on_event`
/// (terminal `[DONE]` and empty payloads are skipped by the caller). `cancel`
/// is checked between events so a cancelled turn stops draining promptly (an
/// idle socket read still blocks until the next byte or the client timeout --
/// blocking reqwest cannot be force-aborted mid-read).
pub(super) fn for_each_sse_event(
    reader: impl BufRead,
    cancel: &CancellationToken,
    mut on_event: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let mut event = String::new();
    for line in reader.lines() {
        if cancel.is_cancelled() {
            bail!("provider stream cancelled");
        }
        let line = line.map_err(|error| anyhow!("failed to read provider stream: {error}"))?;
        if line.trim_end_matches('\r').is_empty() {
            let data = event_data(&event);
            if !data.is_empty() {
                on_event(&data)?;
            }
            event.clear();
        } else {
            event.push_str(&line);
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
                    Attempt::Done(AssistantTurn {
                        text: Some("ok".to_string()),
                        reasoning: Vec::new(),
                        tool_calls: Vec::new(),
                        response_id: None,
                        usage: None,
                    })
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
            &cancel,
            |_force| Ok(()),
            |&()| Attempt::Reauth(anyhow!("401")),
        );
        let error = result.unwrap_err();
        assert!(error.downcast_ref::<crate::errors::AuthError>().is_some());
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
}
