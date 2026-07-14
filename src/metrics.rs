//! Single authoritative home for token/usage arithmetic.
//!
//! Every accumulator, ratio, and rate the TUI or diagnostics surface is
//! computed here from measured [`ProviderUsage`] (and, where present, turn
//! timing) — never re-derived ad hoc at a render site. Pure data + math:
//! no provider names, no terminal/UI types, no I/O. Formatting (compact
//! counts, labels) stays in the UI; this module owns the numbers.

use crate::nexus::{ProviderTurnTiming, ProviderUsage};

/// Accumulated token *flows* across provider turns, plus the latest
/// conversation *level*.
///
/// Flow fields (input/output/cache/reasoning) are per-turn costs and are
/// saturating-summed; `latest_total_tokens` is a level — the provider's
/// conversation size after the most recent turn — so it is replaced, never
/// summed. One type serves every scope (per-task divider, whole-session
/// meter) so the two can never disagree on the arithmetic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TokenFlows {
    /// Completed provider turns observed (round-trips, not user turns).
    pub(crate) provider_turns: u32,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    /// Subset of `input_tokens` served from prompt cache; never add the two.
    pub(crate) cache_read_input_tokens: u64,
    pub(crate) cache_write_input_tokens: u64,
    /// Subset of `output_tokens` spent on hidden reasoning.
    pub(crate) reasoning_output_tokens: u64,
    /// Per-retention cache-write split; nonzero only when a provider reports
    /// the breakdown (`cache_creation_reported` distinguishes "zero" from
    /// "not reported" so displays never over-claim).
    pub(crate) cache_creation_5m_input_tokens: u64,
    pub(crate) cache_creation_1h_input_tokens: u64,
    pub(crate) cache_creation_reported: bool,
    /// Conversation size after the latest observed turn (provider-reported
    /// `total_tokens`). `None` until the first turn completes.
    pub(crate) latest_total_tokens: Option<u64>,
}

impl TokenFlows {
    /// Fold one completed provider turn's measured usage into the totals.
    pub(crate) fn observe(&mut self, usage: &ProviderUsage) {
        self.provider_turns = self.provider_turns.saturating_add(1);
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(usage.cache_read_input_tokens);
        self.cache_write_input_tokens = self
            .cache_write_input_tokens
            .saturating_add(usage.cache_write_input_tokens);
        self.reasoning_output_tokens = self
            .reasoning_output_tokens
            .saturating_add(usage.reasoning_output_tokens);
        if let Some(creation) = &usage.cache_creation {
            self.cache_creation_reported = true;
            self.cache_creation_5m_input_tokens = self
                .cache_creation_5m_input_tokens
                .saturating_add(creation.ephemeral_5m_input_tokens);
            self.cache_creation_1h_input_tokens = self
                .cache_creation_1h_input_tokens
                .saturating_add(creation.ephemeral_1h_input_tokens);
        }
        self.latest_total_tokens = Some(usage.total_tokens);
    }

    /// Whether any provider turn has been observed. Callers render nothing
    /// (rather than a fabricated zero row) while this is false.
    pub(crate) fn is_empty(&self) -> bool {
        self.provider_turns == 0
    }

    /// Share of sent tokens served from prompt cache, when input exists.
    pub(crate) fn cache_read_percent(&self) -> Option<u64> {
        ratio_percent(self.cache_read_input_tokens, self.input_tokens)
    }
}

/// Single-turn flows, mostly for tests and single-usage render paths.
impl From<&ProviderUsage> for TokenFlows {
    fn from(usage: &ProviderUsage) -> Self {
        let mut flows = Self::default();
        flows.observe(usage);
        flows
    }
}

/// Accumulated provider-turn timing across a scope (task or session).
///
/// `generation` sums each turn's measured generation window — first output to
/// terminal event when the turn streamed, otherwise the whole round-trip — so
/// `tokens_per_second(output, generation)` is an output rate over provider
/// time only, never inflated by tool execution between round-trips. TTFT is
/// averaged only over turns that actually streamed (no fabricated samples).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TimingStats {
    pub(crate) generation: std::time::Duration,
    pub(crate) ttft_total: std::time::Duration,
    pub(crate) ttft_samples: u32,
}

impl TimingStats {
    pub(crate) fn observe(&mut self, timing: &ProviderTurnTiming) {
        let generation = match timing.time_to_first_output {
            Some(ttft) => {
                self.ttft_total = self.ttft_total.saturating_add(ttft);
                self.ttft_samples = self.ttft_samples.saturating_add(1);
                timing.duration.saturating_sub(ttft)
            }
            None => timing.duration,
        };
        self.generation = self.generation.saturating_add(generation);
    }

    /// Mean time-to-first-output over the turns that streamed, or `None`
    /// when no turn produced a TTFT sample.
    pub(crate) fn avg_ttft(&self) -> Option<std::time::Duration> {
        (self.ttft_samples > 0).then(|| self.ttft_total / self.ttft_samples)
    }
}

/// Integer percentage of `part` in `whole`, rounded half-up, UNCAPPED: a
/// fullness ratio may honestly exceed 100% (an overflowing context), and
/// clamping it would hide exactly the condition worth showing. `None` when
/// `whole` is zero. Use [`ratio_percent`] for shares of a whole, where >100%
/// is impossible by definition and capping is defensive.
pub(crate) fn percent_of(part: u64, whole: u64) -> Option<u64> {
    if whole == 0 {
        return None;
    }
    // Widened to u128: `part * 100` can overflow u64 for large valid counts.
    let percent = (u128::from(part) * 100 + u128::from(whole) / 2) / u128::from(whole);
    Some(u64::try_from(percent).unwrap_or(u64::MAX))
}

/// Integer percentage of `part` in `whole`, rounded half-up and capped at
/// 100. `None` when `whole` is zero: a ratio of nothing is not 0%, it is
/// unknowable, and callers must hide it rather than claim it.
pub(crate) fn ratio_percent(part: u64, whole: u64) -> Option<u64> {
    percent_of(part, whole).map(|percent| percent.min(100))
}

/// Signed fractional percentage of `delta` in `whole` (e.g. a context-growth
/// delta against the window). `None` when `whole` is zero. Callers format the
/// float; this owns the arithmetic.
pub(crate) fn signed_percent_of(delta: i64, whole: u64) -> Option<f64> {
    (whole > 0).then(|| delta as f64 / whole as f64 * 100.0)
}

/// Output rate over a measured generation window. `None` when the window is
/// zero (a rate over no time is undefined, not infinite).
pub(crate) fn tokens_per_second(
    output_tokens: u64,
    generation: std::time::Duration,
) -> Option<f64> {
    if generation.is_zero() {
        return None;
    }
    Some(output_tokens as f64 / generation.as_secs_f64())
}

/// Provider/model context facts resolved by Mimir. Values stay numeric and
/// provider-neutral so runtime tiers never branch on provider or model ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextWindowFacts {
    pub(crate) raw: u64,
    pub(crate) displayed: u64,
    pub(crate) model_max_output_tokens: u64,
    pub(crate) output_reserve: u64,
    pub(crate) summary_reserve: u64,
    pub(crate) hard_compaction_threshold: u64,
    pub(crate) official_cli: bool,
    pub(crate) configured_endpoint: bool,
}

/// One resolved context policy shared by enforcement, diagnostics, `/context`,
/// and the session meter. Display capacity, preparation, and hard application
/// are separate values because official CLIs do not use one overloaded window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedContextBudget {
    pub(crate) window: Option<ContextWindowFacts>,
    pub(crate) clamp: Option<u64>,
    pub(crate) displayed_context_window: u64,
    pub(crate) warning_threshold: u64,
    pub(crate) preparation_threshold: u64,
    pub(crate) hard_compaction_threshold: u64,
}

impl ResolvedContextBudget {
    pub(crate) fn resolve(
        window: Option<ContextWindowFacts>,
        clamp: Option<u64>,
        fallback: u64,
    ) -> Self {
        let provider_display = window.map_or(fallback, |window| window.displayed);
        let provider_hard = window.map_or(fallback, |window| window.hard_compaction_threshold);
        let displayed_context_window =
            clamp.map_or(provider_display, |cap| cap.min(provider_display));
        let hard_cap = clamp.map_or(provider_hard, |cap| cap.min(provider_hard));
        Self {
            window,
            clamp,
            displayed_context_window,
            warning_threshold: 0,
            preparation_threshold: 0,
            hard_compaction_threshold: hard_cap,
        }
        .with_thresholds(
            crate::config::DEFAULT_COMPACTION_WARN,
            crate::config::DEFAULT_COMPACTION_START,
            crate::config::DEFAULT_COMPACTION_HARD,
        )
    }

    pub(crate) fn with_thresholds(mut self, warn: f64, start: f64, hard: f64) -> Self {
        let fraction = |value: f64| ((self.displayed_context_window as f64) * value).floor() as u64;
        if self.window.is_none() {
            self.hard_compaction_threshold = fraction(hard);
        }
        self.preparation_threshold = fraction(start).min(self.hard_compaction_threshold);
        self.warning_threshold = fraction(warn).min(self.preparation_threshold);
        self
    }

    pub(crate) fn with_hard_threshold_fraction(mut self, hard: f64) -> Self {
        let configured = ((self.displayed_context_window as f64) * hard).floor() as u64;
        self.hard_compaction_threshold = configured.min(self.displayed_context_window);
        self.preparation_threshold = self
            .preparation_threshold
            .min(self.hard_compaction_threshold);
        self.warning_threshold = self.warning_threshold.min(self.preparation_threshold);
        self
    }

    pub(crate) fn clamped(&self) -> bool {
        match (self.clamp, self.window) {
            (Some(clamp), Some(window)) => {
                clamp < window.displayed || clamp < window.hard_compaction_threshold
            }
            _ => false,
        }
    }
}

/// A direct numeric test/bench policy has no model authority, so all trigger
/// thresholds remain the configured fractions of that displayed capacity.
impl From<u64> for ResolvedContextBudget {
    fn from(displayed_context_window: u64) -> Self {
        Self::resolve(
            None,
            Some(displayed_context_window),
            displayed_context_window,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::CacheCreation;
    use std::time::Duration;

    fn usage(input: u64, output: u64, cache_read: u64, total: u64) -> ProviderUsage {
        ProviderUsage {
            provider: "test".to_string(),
            model: "test-model".to_string(),
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: cache_read,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: total,
            cache_creation: None,
        }
    }

    fn window(raw: u64, displayed: u64, hard: u64) -> ContextWindowFacts {
        ContextWindowFacts {
            raw,
            displayed,
            model_max_output_tokens: 64_000,
            output_reserve: 20_000,
            summary_reserve: raw.saturating_sub(20_000).saturating_sub(hard),
            hard_compaction_threshold: hard,
            official_cli: true,
            configured_endpoint: false,
        }
    }

    #[test]
    fn flows_sum_and_total_is_latest_level() {
        let mut flows = TokenFlows::default();
        flows.observe(&usage(1_000, 100, 800, 1_100));
        flows.observe(&usage(2_000, 200, 1_900, 3_300));
        assert_eq!(flows.provider_turns, 2);
        assert_eq!(flows.input_tokens, 3_000);
        assert_eq!(flows.output_tokens, 300);
        assert_eq!(flows.cache_read_input_tokens, 2_700);
        // Level, not flow: latest wins, never summed.
        assert_eq!(flows.latest_total_tokens, Some(3_300));
        assert!(!flows.is_empty());
    }

    #[test]
    fn flows_track_cache_creation_split_only_when_reported() {
        let mut flows = TokenFlows::default();
        flows.observe(&usage(1_000, 100, 0, 1_100));
        assert!(!flows.cache_creation_reported);
        let mut with_split = usage(1_000, 100, 0, 2_200);
        with_split.cache_write_input_tokens = 700;
        with_split.cache_creation = Some(CacheCreation {
            ephemeral_5m_input_tokens: 500,
            ephemeral_1h_input_tokens: 200,
        });
        flows.observe(&with_split);
        assert!(flows.cache_creation_reported);
        assert_eq!(flows.cache_creation_5m_input_tokens, 500);
        assert_eq!(flows.cache_creation_1h_input_tokens, 200);
        assert_eq!(flows.cache_write_input_tokens, 700);
    }

    #[test]
    fn cache_read_percent_matches_legacy_half_up_rounding() {
        let mut flows = TokenFlows::default();
        assert_eq!(flows.cache_read_percent(), None);
        flows.observe(&usage(1_000, 0, 875, 1_000));
        assert_eq!(flows.cache_read_percent(), Some(88));
    }

    #[test]
    fn ratio_percent_half_up_capped_and_unknowable_when_whole_is_zero() {
        assert_eq!(ratio_percent(1, 0), None);
        assert_eq!(ratio_percent(0, 10), Some(0));
        assert_eq!(ratio_percent(1, 3), Some(33));
        assert_eq!(ratio_percent(1, 2), Some(50));
        assert_eq!(ratio_percent(875, 1_000), Some(88));
        // Part exceeding whole (defensive: cache math is provider-reported)
        // caps at 100 rather than claiming >100%.
        assert_eq!(ratio_percent(20, 10), Some(100));
        // Large valid inputs must not overflow the intermediate multiply.
        assert_eq!(ratio_percent(u64::MAX, u64::MAX), Some(100));
        assert_eq!(ratio_percent(u64::MAX / 2, u64::MAX), Some(50));
    }

    #[test]
    fn percent_of_is_uncapped_so_overflow_shows_as_over_100() {
        assert_eq!(percent_of(1, 0), None);
        assert_eq!(percent_of(105, 100), Some(105));
        assert_eq!(percent_of(1, 3), Some(33));
        // Signed growth: arithmetic here, formatting at the caller.
        assert_eq!(signed_percent_of(-500, 0), None);
        let pct = signed_percent_of(-500, 100_000).unwrap();
        assert!((pct - -0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn tokens_per_second_undefined_over_zero_time() {
        assert_eq!(tokens_per_second(100, Duration::ZERO), None);
        let rate = tokens_per_second(100, Duration::from_secs(4)).unwrap();
        assert!((rate - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn timing_stats_sum_generation_and_average_ttft_over_streamed_turns_only() {
        let mut stats = TimingStats::default();
        assert_eq!(stats.avg_ttft(), None);
        // Streamed turn: generation excludes the wait for the first token.
        stats.observe(&crate::nexus::ProviderTurnTiming {
            duration: Duration::from_millis(1_000),
            time_to_first_output: Some(Duration::from_millis(400)),
        });
        // Non-streaming turn: no TTFT sample; whole round-trip is generation.
        stats.observe(&crate::nexus::ProviderTurnTiming {
            duration: Duration::from_millis(2_000),
            time_to_first_output: None,
        });
        stats.observe(&crate::nexus::ProviderTurnTiming {
            duration: Duration::from_millis(1_000),
            time_to_first_output: Some(Duration::from_millis(200)),
        });
        assert_eq!(stats.generation, Duration::from_millis(600 + 2_000 + 800));
        // Average over the two streamed turns only.
        assert_eq!(stats.avg_ttft(), Some(Duration::from_millis(300)));
    }

    #[test]
    fn budget_resolution_keeps_display_preparation_and_hard_separate() {
        let model = window(372_000, 353_400, 334_800);
        let policy = ResolvedContextBudget::resolve(Some(model), None, 100_000);
        assert_eq!(policy.displayed_context_window, 353_400);
        assert_eq!(policy.warning_threshold, 212_040);
        assert_eq!(policy.preparation_threshold, 254_448);
        assert_eq!(policy.hard_compaction_threshold, 334_800);
        let start_only_override = policy.with_thresholds(0.60, 0.70, 0.90);
        assert_eq!(start_only_override.preparation_threshold, 247_379);
        assert_eq!(start_only_override.hard_compaction_threshold, 334_800);
        assert_eq!(
            policy
                .with_hard_threshold_fraction(0.95)
                .hard_compaction_threshold,
            335_730
        );

        let clamped = ResolvedContextBudget::resolve(Some(model), Some(235_808), 100_000);
        assert_eq!(clamped.displayed_context_window, 235_808);
        assert_eq!(clamped.hard_compaction_threshold, 235_808);
        assert!(clamped.clamped());

        let unknown = ResolvedContextBudget::resolve(None, None, 128_000);
        assert_eq!(unknown.displayed_context_window, 128_000);
        assert_eq!(unknown.hard_compaction_threshold, 115_200);

        let direct = ResolvedContextBudget::from(128_000).with_thresholds(0.60, 0.72, 0.95);
        assert_eq!(direct.hard_compaction_threshold, 121_600);
    }
}
