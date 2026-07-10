//! Low-level live-run support promoted out of `compaction_live_bench.rs` so the
//! campaign harness and the legacy per-experiment live tests share ONE copy of
//! the recording wrapper, temp-dir, observers, and approval gates. Nothing here
//! is campaign-specific: it is the reusable plumbing every live run needs.
//!
//! Test-support only; the whole `live_harness` tree is `#[cfg(test)]` and never
//! reached by the committed suite (`cargo test --locked`) or CI.

use super::*;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// Matches the summarizer instruction prefix (`SUMMARY_PROMPT`, private to
/// `crate::wayland`) so the recording wrapper tags a summarization request
/// without depending on production internals.
pub(crate) const SUMMARY_INSTRUCTION_PREFIX: &str = "Summarize this coding session";

/// One provider round-trip's realized usage, tagged summarization vs normal.
/// `tag` is the first chars of the request's LAST message content, so a run can
/// select a specific turn's request by its prompt without relying on positional
/// order (a spurious model tool-call would shift positions).
#[derive(Clone)]
pub(crate) struct CapturedUsage {
    pub(crate) is_summary: bool,
    pub(crate) tag: String,
    pub(crate) started_at: Instant,
    pub(crate) usage: Option<ProviderUsage>,
    /// The pure estimator's token count of the EXACT messages sent on this
    /// request (`message_token_estimate` summed). Captured per request so the
    /// campaign row reports a genuine like-for-like `estimate_error`
    /// (provider-measured input minus estimator) instead of a per-turn
    /// measurement broadcast across the turn's requests (pilot-a finding 2).
    pub(crate) estimate_tokens: u64,
}

/// Wraps a real provider and records the `ProviderUsage` on every completed
/// turn, tagging summarization requests (last message is the summary
/// instruction). Test-only: `provider_summary` discards usage on the production
/// path, so this wrapper is how a summarization request's realized cache-hit
/// rate is captured WITHOUT touching production code.
pub(crate) struct RecordingProvider<P: ChatProvider> {
    pub(crate) inner: P,
    pub(crate) usages: Arc<Mutex<Vec<CapturedUsage>>>,
}

impl<P: ChatProvider> RecordingProvider<P> {
    pub(crate) fn new(inner: P) -> Self {
        Self {
            inner,
            usages: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A cheap clone of the shared usage log, so a caller can read the captured
    /// samples after the harness has consumed the provider.
    pub(crate) fn log(&self) -> Arc<Mutex<Vec<CapturedUsage>>> {
        self.usages.clone()
    }
}

impl<P: ChatProvider> ChatProvider for RecordingProvider<P> {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a Tools,
        cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        let is_summary = messages
            .last()
            .is_some_and(|m| m.content.starts_with(SUMMARY_INSTRUCTION_PREFIX));
        let tag = messages
            .last()
            .map(|m| m.content.chars().take(32).collect::<String>())
            .unwrap_or_default();
        // Estimator value for THIS request's exact payload, taken before the
        // provider answers, so the campaign row can diff it against the
        // provider's reported input tokens per request.
        let estimate_tokens = messages
            .iter()
            .map(crate::session::message_token_estimate)
            .fold(0u64, u64::saturating_add);
        let started_at = Instant::now();
        let usages = self.usages.clone();
        let stream = self.inner.respond_stream(messages, tools, cancel)?;
        let mapped = stream.map(move |item| {
            if let Ok(ProviderEvent::Completed(turn)) = &item {
                usages.lock().expect("usages lock").push(CapturedUsage {
                    is_summary,
                    tag: tag.clone(),
                    started_at,
                    usage: turn.usage.clone(),
                    estimate_tokens,
                });
            }
            item
        });
        Ok(Box::pin(mapped))
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }
}

/// A temp dir removed on drop (parallel-test safe).
pub(crate) struct TempDir {
    pub(crate) path: PathBuf,
}

impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        // Tags are built from ids like `S1::anthropic/claude-sonnet-4-6@low`;
        // sanitize to a single path component so `create_dir` never sees a
        // separator (a `/` in the tag made the parent-less nested path fail
        // with NotFound on the first live pilot run).
        let tag: String = tag
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let path = std::env::temp_dir().join(format!("iris-live-harness-{tag}-{nanos}"));
        std::fs::create_dir(&path).expect("create temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A no-op approval gate (text-only scenarios never reach `review`).
pub(crate) struct NoToolGate;

impl ApprovalGate for NoToolGate {
    fn review<'a>(
        &'a self,
        _call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        _ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::Deny) })
    }
}

/// Allows calls against a read-only registry. The harness advertises no
/// mutating tools, so this gate is reached only for real repository reads.
pub(crate) struct ReadOnlyGate;

impl ApprovalGate for ReadOnlyGate {
    fn review<'a>(
        &'a self,
        _call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        _ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::Allow) })
    }
}

/// An observer event stamped with the wall-clock instant it was delivered, so
/// timing gaps (e.g. compaction-to-next-request) can be measured after the run.
#[derive(Clone)]
pub(crate) struct TimedEvent {
    pub(crate) at: Instant,
    pub(crate) event: AgentEvent,
}

/// Records every `AgentEvent` with its arrival instant. Shared by the campaign
/// runner (for boundary/lifecycle rows) and the legacy live loop.
#[derive(Default)]
pub(crate) struct LiveLoopObserver {
    pub(crate) events: Mutex<Vec<TimedEvent>>,
}

impl AgentObserver for LiveLoopObserver {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        self.events
            .lock()
            .expect("live events lock")
            .push(TimedEvent {
                at: Instant::now(),
                event,
            });
        Ok(())
    }
}

/// A no-op observer: a run that reads usage from the recording provider, not
/// from events, does not need the timeline.
pub(crate) struct NoopObserver;

impl AgentObserver for NoopObserver {
    fn on_event(&self, _event: AgentEvent) -> Result<()> {
        Ok(())
    }
}

/// Drive one future to completion on a fresh current-thread runtime. The live
/// harness submits turns synchronously from a `#[test]` thread, so each turn
/// gets its own runtime rather than requiring a `#[tokio::test]` shell.
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(future)
}

#[cfg(test)]
mod tests {
    use super::TempDir;

    #[test]
    fn temp_dir_sanitizes_path_separators_and_symbols_in_tags() {
        // Regression: the first live pilot run panicked NotFound because a
        // cell id containing `/` produced a nested, parent-less temp path.
        let dir = TempDir::new("campaign-S1::anthropic/claude-sonnet-4-6@low#run1");
        assert!(dir.path.is_dir());
        let name = dir.path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with(
                "iris-live-harness-campaign-S1--anthropic-claude-sonnet-4-6-low-run1-"
            )
        );
    }
}
