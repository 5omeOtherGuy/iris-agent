// SPDX-License-Identifier: Apache-2.0
// Derived from codex-rs/tui/src/streaming/controller.rs and
// codex-rs/tui/src/streaming/commit_tick.rs (OpenAI Codex, Apache-2.0).
// Changes from upstream: reimplemented against Iris `TranscriptRow` rendering
// (via `crate::ui::tui::pane`) instead of Codex `HistoryCell`; the stable/tail
// split uses Iris's block-boundary scanner (`super::table_holdback`) rather than
// Codex's rendered-line table budget, because Iris renders markdown SoftBreak as
// a space; the multi-controller commit-tick orchestration is collapsed into the
// single agent-message controller Iris needs. The two-region (stable prefix +
// mutable tail) model and the adaptive paced drain are preserved.

//! Assistant-message stream controller: newline-gated collection, block-safe
//! incremental commit to scrollback, a single mutable active tail, and an
//! adaptive paced drain.

use std::time::Instant;

use crate::ui::tui::pane;
use crate::ui::tui::rows::TranscriptRow;

use super::chunking::{AdaptiveChunkingPolicy, DrainPlan, QueueSnapshot};
use super::collector::MarkdownStreamCollector;
use super::escapement::Escapement;
use super::table_holdback::safe_commit_end;

/// Cached rendered-line count for a stable source prefix, keyed by the prefix
/// boundary and render width so a dense stream does not re-render the prefix on
/// every delta.
struct StableCache {
    safe_end: usize,
    content_width: usize,
    count: usize,
}

/// Memoized full render (committed source plus the pending partial line) used
/// for both the tail preview and the paced commit slice.
struct RenderCache {
    buffered_len: usize,
    content_width: usize,
    rows: Vec<TranscriptRow>,
}

/// Streaming controller for one assistant message.
#[derive(Default)]
pub(crate) struct StreamController {
    /// Word-quantized drain buffer between raw delta arrival and the pipeline:
    /// held arrivals are released one beat at a time on the loop's tick grid so
    /// the visible tail advances by word-steps, never a raw burst. Bypassed in
    /// reduced-motion (`passthrough`), where arrival ingests immediately.
    escapement: Escapement,
    /// Reduced-motion pass-through: deltas ingest straight into the collector on
    /// arrival (arrival == display in the same frame), the escapement idle.
    passthrough: bool,
    /// Newline-gated raw-source accumulator (owns the pending partial line).
    collector: MarkdownStreamCollector,
    /// Complete (newline-terminated) source committed out of the collector.
    raw_source: String,
    /// Rendered lines safe to commit to scrollback (`render(prefix)` count).
    stable_target: usize,
    /// Rendered lines already emitted to scrollback.
    emitted: usize,
    /// A stream is in progress (between the first delta and finalize/reset).
    active: bool,
    /// Adaptive paced-drain policy.
    policy: AdaptiveChunkingPolicy,
    /// When the current backlog of stable-but-unemitted lines first appeared.
    oldest_pending_at: Option<Instant>,
    stable_cache: Option<StableCache>,
    render_cache: Option<RenderCache>,
}

impl StreamController {
    /// Whether a stream is currently in progress.
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    /// Whether there is any not-yet-committed content (escapement-held arrival,
    /// paced backlog, or a mutable tail), i.e. the loop should keep driving
    /// commit ticks. The escapement clause matters: held text must be beaten out
    /// even when the collector is momentarily empty, or a stream would stall.
    pub(crate) fn has_work(&self) -> bool {
        self.active
            && (self.emitted < self.stable_target
                || !self.collector.is_empty()
                || !self.escapement.is_empty())
    }

    /// Identity of the current tail render `(buffered_len, emitted)`; changes
    /// exactly when the tail preview changes, so callers can memoize its wrap.
    pub(crate) fn tail_signature(&self) -> (usize, usize) {
        (self.collector.buffered_len(), self.emitted)
    }

    /// Accept a streaming delta. `content_width` is the assistant content column
    /// used for markdown layout (see `Transcript::markdown_content_width`).
    ///
    /// The arrival is held in the [`Escapement`] and released in word-quantized
    /// beats by [`commit_tick`](Self::commit_tick), so the visible tail advances
    /// on the tick grid instead of on the network. Under reduced motion the
    /// escapement is bypassed and the delta ingests immediately (arrival ==
    /// display in the same frame), byte-identical to the pre-escapement path.
    pub(crate) fn push_delta(&mut self, delta: &str, content_width: usize) {
        self.active = true;
        if self.passthrough {
            self.ingest(delta, content_width);
        } else {
            self.escapement.push(delta);
        }
    }

    /// Feed drained text into the newline-gated collector, promoting completed
    /// lines into the stable source. Both the visible tail and the collector are
    /// fed from this ONE drained output, so committed lines and the tail share a
    /// single, consistent, paced timeline.
    fn ingest(&mut self, text: &str, content_width: usize) {
        self.collector.push_delta(text);
        if text.contains('\n')
            && let Some(chunk) = self.collector.commit_complete_source()
        {
            self.raw_source.push_str(&chunk);
            self.recompute_stable_target(content_width);
        }
    }

    /// Apply the reduced-motion posture. On entering pass-through, any text still
    /// held in the escapement flushes immediately (§2.2 flush trigger) so the
    /// switch never strands a paced backlog.
    pub(crate) fn set_reduced_motion(&mut self, reduced_motion: bool, content_width: usize) {
        self.passthrough = reduced_motion;
        if reduced_motion && let Some(text) = self.escapement.flush() {
            self.ingest(&text, content_width);
        }
    }

    /// Release everything the escapement is holding into the visible tail now
    /// (approval gate: the user must review against complete context). Does not
    /// finalize — the tail simply shows all arrived text at once.
    pub(crate) fn flush_escapement(&mut self, content_width: usize) {
        if let Some(text) = self.escapement.flush() {
            self.ingest(&text, content_width);
        }
    }

    /// Recompute how many rendered lines are safe to commit, and note when a
    /// fresh backlog appears so the paced drain can measure its age.
    fn recompute_stable_target(&mut self, content_width: usize) {
        let safe_end = safe_commit_end(&self.raw_source);
        self.stable_target = self.stable_prefix_count(safe_end, content_width);
        if self.emitted < self.stable_target && self.oldest_pending_at.is_none() {
            self.oldest_pending_at = Some(Instant::now());
        }
    }

    /// Rendered-line count of `raw_source[..safe_end]` at `content_width`.
    fn stable_prefix_count(&mut self, safe_end: usize, content_width: usize) -> usize {
        if let Some(cache) = &self.stable_cache
            && cache.safe_end == safe_end
            && cache.content_width == content_width
        {
            return cache.count;
        }
        let prefix = &self.raw_source[..safe_end.min(self.raw_source.len())];
        let count = if prefix.is_empty() {
            0
        } else {
            pane::assistant_rows(prefix, content_width).len()
        };
        self.stable_cache = Some(StableCache {
            safe_end,
            content_width,
            count,
        });
        count
    }

    /// Full render of committed source plus the pending partial line, memoized
    /// on `(buffered_len, content_width)`.
    fn ensure_render(&mut self, content_width: usize) -> &[TranscriptRow] {
        let buffered_len = self.collector.buffered_len();
        let fresh = self
            .render_cache
            .as_ref()
            .is_some_and(|c| c.buffered_len == buffered_len && c.content_width == content_width);
        if !fresh {
            // The pending partial line lives in the collector; render it with
            // the committed source so the tail shows in-progress typing.
            let mut source = self.raw_source.clone();
            source.push_str(self.collector.pending_partial());
            let rows = if source.is_empty() {
                Vec::new()
            } else {
                pane::assistant_rows(&source, content_width)
            };
            self.render_cache = Some(RenderCache {
                buffered_len,
                content_width,
                rows,
            });
        }
        &self.render_cache.as_ref().expect("render cache set").rows
    }

    /// The mutable active-tail rows (rendered after committed scrollback, never
    /// searchable or committed until finalize).
    pub(crate) fn tail_rows(&mut self, content_width: usize) -> Vec<TranscriptRow> {
        let emitted = self.emitted;
        let rows = self.ensure_render(content_width);
        rows.get(emitted..).map(<[_]>::to_vec).unwrap_or_default()
    }

    /// Advance the paced drain by one commit tick, returning the rendered lines
    /// that just entered scrollback (to be appended to the transcript).
    pub(crate) fn commit_tick(&mut self, now: Instant, content_width: usize) -> Vec<TranscriptRow> {
        // The escapement beats on this same tick (never a second timer): release
        // one word-quantum of held arrival into the pipeline so the visible tail
        // advances by word-steps. The collector sees this drained output at the
        // same moment the tail does — committed lines never lag the tail.
        if let Some(quantum) = self.escapement.beat(now) {
            self.ingest(&quantum, content_width);
        }
        // Width may have changed since the last delta; keep the target current.
        self.recompute_stable_target(content_width);
        let queued = self.stable_target.saturating_sub(self.emitted);
        if queued == 0 {
            self.policy.decide(QueueSnapshot::default(), now);
            self.oldest_pending_at = None;
            return Vec::new();
        }
        let snapshot = QueueSnapshot {
            queued_lines: queued,
            oldest_age: self
                .oldest_pending_at
                .map(|t| now.saturating_duration_since(t)),
        };
        let decision = self.policy.decide(snapshot, now);
        let want = match decision.drain_plan {
            DrainPlan::Single => 1,
            DrainPlan::Batch(n) => n,
        };
        let commit = want.min(queued);
        let start = self.emitted;
        let end = start + commit;
        let rows = self.ensure_render(content_width);
        let out = rows.get(start..end).map(<[_]>::to_vec).unwrap_or_default();
        self.emitted = end;
        if self.emitted >= self.stable_target {
            self.oldest_pending_at = None;
        } else {
            self.oldest_pending_at = Some(now);
        }
        out
    }

    /// Finalize the stream: drain the collector, render the complete source, and
    /// return every rendered line not yet emitted (committed exactly once). The
    /// controller is reset for the next stream.
    pub(crate) fn finalize(&mut self, content_width: usize) -> Vec<TranscriptRow> {
        // Flush any escapement-held arrival into the collector first: finalize
        // renders the WHOLE source, so pacing changes WHEN a line shows, never
        // WHAT the finished message is (byte-identical to the unpaced path).
        if let Some(text) = self.escapement.flush() {
            self.collector.push_delta(&text);
        }
        let remainder = self.collector.finalize_and_drain_source();
        self.raw_source.push_str(&remainder);
        let rows = if self.raw_source.is_empty() {
            Vec::new()
        } else {
            pane::assistant_rows(&self.raw_source, content_width)
        };
        let out = rows
            .get(self.emitted..)
            .map(<[_]>::to_vec)
            .unwrap_or_default();
        self.reset();
        out
    }

    /// Clear all state (cancellation, session reset, or after finalize). The
    /// reduced-motion posture is a screen preference, not stream state, so it
    /// persists across streams.
    pub(crate) fn reset(&mut self) {
        self.escapement.clear();
        self.collector.clear();
        self.raw_source.clear();
        self.stable_target = 0;
        self.emitted = 0;
        self.active = false;
        self.policy.reset();
        self.oldest_pending_at = None;
        self.stable_cache = None;
        self.render_cache = None;
    }
}
