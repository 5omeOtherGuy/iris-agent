//! Per-frame render timing for the `/debug` snapshot.
//!
//! [`TuiUi::draw`](super::super::tui::TuiUi::draw) records two phases of every
//! real frame: `compose` (building the frame's lines/cells from [`Screen`]
//! state) and `flush` (writing the bytes to the terminal). The flush is the
//! part a criterion microbench cannot see, so this in-loop counter is the
//! authoritative answer to "is a frame slow, and which half is slow". Samples
//! live in a bounded ring so memory is fixed regardless of session length;
//! percentiles are computed only when `/debug` asks, never per frame.
//!
//! [`Screen`]: super::screen::Screen

use std::collections::VecDeque;
use std::time::Duration;

/// Recent frames retained for percentile math. At ~60fps this is roughly the
/// last ~8s of active rendering -- enough to characterize a burst (stream,
/// scroll, resize storm) without unbounded growth.
const CAPACITY: usize = 512;

/// One frame's split timing.
#[derive(Clone, Copy)]
struct Sample {
    compose: Duration,
    flush: Duration,
}

impl Sample {
    fn total(self) -> Duration {
        self.compose + self.flush
    }
}

/// Bounded ring of recent per-frame timings. Recording is O(1) (one push, one
/// possible eviction); summarizing is O(n log n) and happens only on `/debug`.
pub(crate) struct FrameStats {
    samples: VecDeque<Sample>,
}

impl FrameStats {
    pub(crate) fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(CAPACITY),
        }
    }

    /// Record one frame's compose + flush durations, evicting the oldest sample
    /// once the ring is full so retention stays bounded to [`CAPACITY`].
    pub(crate) fn record(&mut self, compose: Duration, flush: Duration) {
        if self.samples.len() == CAPACITY {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample { compose, flush });
    }

    /// Summarize the retained samples, or `None` when no frame has been drawn.
    pub(crate) fn summary(&self) -> Option<FrameSummary> {
        if self.samples.is_empty() {
            return None;
        }
        Some(FrameSummary {
            count: self.samples.len(),
            total: Percentiles::compute(self.samples.iter().map(|s| s.total())),
            compose: Percentiles::compute(self.samples.iter().map(|s| s.compose)),
            flush: Percentiles::compute(self.samples.iter().map(|s| s.flush)),
        })
    }
}

/// p50/p99/max of a set of durations (nearest-rank).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Percentiles {
    p50: Duration,
    p99: Duration,
    max: Duration,
}

impl Percentiles {
    fn compute(durations: impl Iterator<Item = Duration>) -> Self {
        let mut sorted: Vec<Duration> = durations.collect();
        sorted.sort_unstable();
        Self {
            p50: nearest_rank(&sorted, 0.50),
            p99: nearest_rank(&sorted, 0.99),
            max: sorted.last().copied().unwrap_or(Duration::ZERO),
        }
    }
}

/// Nearest-rank percentile of a pre-sorted slice. `q` in `0.0..=1.0`; empty
/// input yields zero. Rank = `ceil(q * n)` clamped to `[1, n]`; index is
/// `rank - 1`. Chosen over interpolation because frame timings are compared as
/// concrete observed values, not smoothed estimates.
fn nearest_rank(sorted: &[Duration], q: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    let idx = rank.clamp(1, sorted.len()) - 1;
    sorted[idx]
}

/// A rendered summary of the retained frame timings for the `/debug` snapshot.
pub(crate) struct FrameSummary {
    count: usize,
    total: Percentiles,
    compose: Percentiles,
    flush: Percentiles,
}

impl FrameSummary {
    /// Human-readable lines for the `/debug` frame-timing section. Values are
    /// milliseconds (3 decimals) so they read directly against the ~16ms
    /// coalescing budget; the `compose` vs `flush` split separates pure render
    /// cost from terminal-write cost.
    pub(crate) fn lines(&self) -> Vec<String> {
        vec![
            format!(
                "Frames sampled: {} (ring holds last {CAPACITY})",
                self.count
            ),
            format!("  total   {}", fmt_row(self.total)),
            format!("  compose {}", fmt_row(self.compose)),
            format!("  flush   {}", fmt_row(self.flush)),
        ]
    }
}

fn fmt_row(p: Percentiles) -> String {
    format!(
        "p50={} p99={} max={}",
        fmt_ms(p.p50),
        fmt_ms(p.p99),
        fmt_ms(p.max)
    )
}

fn fmt_ms(d: Duration) -> String {
    format!("{:.3}ms", d.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn summary_is_none_until_a_frame_is_recorded() {
        assert!(FrameStats::new().summary().is_none());
    }

    #[test]
    fn total_is_compose_plus_flush() {
        let mut stats = FrameStats::new();
        stats.record(ms(3), ms(5));
        let summary = stats.summary().expect("one sample");
        assert_eq!(summary.total.p50, ms(8));
        assert_eq!(summary.compose.p50, ms(3));
        assert_eq!(summary.flush.p50, ms(5));
    }

    #[test]
    fn nearest_rank_picks_observed_values() {
        // 100 samples with flush = 1..=100 ms. Nearest-rank: p50 -> 50th
        // (index 49 -> 50ms), p99 -> 99th (index 98 -> 99ms), max -> 100ms.
        let mut stats = FrameStats::new();
        for n in 1..=100 {
            stats.record(Duration::ZERO, ms(n));
        }
        let flush = stats.summary().expect("samples").flush;
        assert_eq!(flush.p50, ms(50), "p50");
        assert_eq!(flush.p99, ms(99), "p99");
        assert_eq!(flush.max, ms(100), "max");
    }

    #[test]
    fn ring_evicts_oldest_beyond_capacity() {
        let mut stats = FrameStats::new();
        // Fill the ring with 1ms frames, then overwrite all but one with 2ms.
        for _ in 0..CAPACITY {
            stats.record(Duration::ZERO, ms(1));
        }
        for _ in 0..(CAPACITY - 1) {
            stats.record(Duration::ZERO, ms(2));
        }
        let summary = stats.summary().expect("samples");
        assert_eq!(summary.count, CAPACITY, "retention bounded to CAPACITY");
        // 511x 2ms + 1x 1ms: median is 2ms, max is 2ms, min-tail 1ms survives.
        assert_eq!(summary.flush.p50, ms(2), "oldest 1ms frames evicted");
        assert_eq!(summary.flush.max, ms(2));
    }

    #[test]
    fn lines_split_compose_and_flush_in_milliseconds() {
        let mut stats = FrameStats::new();
        stats.record(Duration::from_micros(120), Duration::from_micros(2500));
        let lines = stats.summary().expect("sample").lines();
        assert!(lines[0].contains("Frames sampled: 1"), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("compose") && l.contains("0.120ms")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("flush") && l.contains("2.500ms")),
            "{lines:?}"
        );
    }
}
