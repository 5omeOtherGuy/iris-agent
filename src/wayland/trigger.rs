//! Provider-neutral context measurement and trigger-ladder arithmetic.

use super::*;

pub(super) const DEFAULT_SUMMARY_RESERVE: u64 = 8_192;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct TriggerThresholds {
    pub(crate) warn: f64,
    pub(crate) start: f64,
    pub(crate) hard: f64,
}

impl Default for TriggerThresholds {
    fn default() -> Self {
        Self {
            warn: crate::config::DEFAULT_COMPACTION_WARN,
            start: crate::config::DEFAULT_COMPACTION_START,
            hard: crate::config::DEFAULT_COMPACTION_HARD,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TriggerLadder {
    pub(crate) effective_window: u64,
    pub(crate) warn: u64,
    pub(crate) start: u64,
    pub(crate) hard: u64,
    pub(crate) keep_recent_tokens: u64,
    pub(crate) deterministic_only: bool,
}

impl TriggerLadder {
    pub(crate) fn resolve(
        effective_window: u64,
        thresholds: TriggerThresholds,
        summary_reserve: u64,
        keep_recent_tokens: u64,
    ) -> Self {
        let threshold = |fraction: f64, buffer_multiples: u64| {
            let fractional = ((effective_window as f64) * fraction).floor() as u64;
            let buffered =
                effective_window.saturating_sub(summary_reserve.saturating_mul(buffer_multiples));
            fractional.max(buffered)
        };
        Self {
            effective_window,
            warn: threshold(thresholds.warn, 6),
            start: threshold(thresholds.start, 4),
            hard: threshold(thresholds.hard, 2),
            keep_recent_tokens: keep_recent_tokens.min(effective_window / 4),
            deterministic_only: effective_window < summary_reserve.saturating_mul(4),
        }
    }

    pub(crate) fn tier(&self, measured: u64) -> ContextPressureTier {
        if measured >= self.hard {
            ContextPressureTier::Hard
        } else if measured >= self.start {
            ContextPressureTier::Start
        } else if measured >= self.warn {
            ContextPressureTier::Warn
        } else {
            ContextPressureTier::Normal
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UsageAnchor {
    pub(crate) total_tokens: u64,
    pub(crate) message_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextMeasurement {
    pub(crate) tokens: u64,
    pub(crate) source: ContextMeasurementSource,
}

pub(crate) fn measure_context(
    messages: &[Message],
    anchor: Option<UsageAnchor>,
    pending_tokens: u64,
) -> ContextMeasurement {
    match anchor.filter(|anchor| anchor.message_count <= messages.len()) {
        Some(anchor) => ContextMeasurement {
            tokens: anchor.total_tokens.saturating_add(
                context_tokens(&messages[anchor.message_count..]).saturating_add(pending_tokens),
            ),
            source: ContextMeasurementSource::ProviderReportedPlusLocal,
        },
        None => ContextMeasurement {
            tokens: context_tokens(messages).saturating_add(pending_tokens),
            source: ContextMeasurementSource::Estimated,
        },
    }
}

#[derive(Debug, Default)]
pub(crate) struct PressureTracker {
    previous: Option<ContextPressureTier>,
}

impl PressureTracker {
    pub(crate) fn crossing(
        &mut self,
        measured: u64,
        ladder: &TriggerLadder,
    ) -> Option<ContextPressureTier> {
        let current = ladder.tier(measured);
        let previous = self.previous;
        self.previous = Some(current);
        match previous {
            Some(previous) if previous != current => Some(current),
            None if current != ContextPressureTier::Normal => Some(current),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_resolves_across_tiny_and_large_windows() {
        let cases = [
            (8_000, (4_800, 5_760, 7_200), true, 2_000),
            (32_000, (19_200, 23_040, 28_800), true, 8_000),
            (131_072, (78_643, 94_371, 117_964), false, 8_000),
            (1_000_000, (600_000, 720_000, 900_000), false, 8_000),
        ];

        for (window, expected, deterministic_only, keep) in cases {
            let ladder = TriggerLadder::resolve(window, TriggerThresholds::default(), 8_192, 8_000);
            assert_eq!(
                (ladder.warn, ladder.start, ladder.hard),
                expected,
                "window={window}"
            );
            assert_eq!(
                ladder.deterministic_only, deterministic_only,
                "window={window}"
            );
            assert_eq!(ladder.keep_recent_tokens, keep, "window={window}");
        }
    }

    #[test]
    fn hybrid_measurement_adds_only_messages_after_server_usage() {
        let messages = vec![
            Message::user(&"a".repeat(40)),
            Message::assistant(&"b".repeat(40)),
            Message::tool_result("call_1", "read", &"c".repeat(40)),
        ];
        let measured = measure_context(
            &messages,
            Some(UsageAnchor {
                total_tokens: 100,
                message_count: 2,
            }),
            0,
        );
        assert_eq!(measured.tokens, 110);
        assert_eq!(
            measured.source,
            ContextMeasurementSource::ProviderReportedPlusLocal
        );
    }

    #[test]
    fn usage_blind_and_post_apply_measurements_are_local_and_round_up() {
        let messages = vec![Message::user("12345")];
        let measured = measure_context(&messages, None, 0);
        assert_eq!(measured.tokens, 2);
        assert_eq!(measured.source, ContextMeasurementSource::Estimated);

        let stale_anchor = UsageAnchor {
            total_tokens: 90_000,
            message_count: 99,
        };
        let measured = measure_context(&messages, Some(stale_anchor), 0);
        assert_eq!(measured.tokens, 2, "a rewrite invalidates a stale anchor");
        assert_eq!(measured.source, ContextMeasurementSource::Estimated);
    }

    #[test]
    fn crossings_emit_once_per_direction() {
        let ladder = TriggerLadder::resolve(100_000, TriggerThresholds::default(), 8_192, 20_000);
        let mut tracker = PressureTracker::default();
        assert_eq!(tracker.crossing(1, &ladder), None);
        assert_eq!(
            tracker.crossing(ladder.warn, &ladder),
            Some(ContextPressureTier::Warn)
        );
        assert_eq!(tracker.crossing(ladder.warn + 1, &ladder), None);
        assert_eq!(
            tracker.crossing(ladder.start, &ladder),
            Some(ContextPressureTier::Start)
        );
        assert_eq!(tracker.crossing(ladder.start + 1, &ladder), None);
        assert_eq!(
            tracker.crossing(ladder.warn - 1, &ladder),
            Some(ContextPressureTier::Normal)
        );
        assert_eq!(tracker.crossing(ladder.warn - 2, &ladder), None);
    }
}
