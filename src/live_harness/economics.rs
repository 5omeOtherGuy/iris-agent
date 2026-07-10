//! Cache-economics pairing, promoted out of `compaction_live_bench.rs`. Pairs
//! each compaction apply with the provider requests immediately before and
//! after it, then sums the realized cache-write mass on each side so a run can
//! report "what did breaking the warm prefix actually cost" from real
//! `ProviderUsage`, never estimates.
//!
//! The write-visible lane (Anthropic) reports `cache_write_input_tokens`
//! directly. The write-blind lane (Codex) does not report writes at all, so its
//! only realizable proxy is fresh (non-cache-read) input: `input - cache_read`.
//! Which proxy applies is chosen by [`CacheMassModel`], keeping the pairing
//! logic lane-neutral.

use super::*;
use std::time::Instant;

/// How a lane's realized "new prefix" mass is read from `ProviderUsage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CacheMassModel {
    /// Provider reports cache writes (Anthropic): the write tokens ARE the mass.
    ReportedWrite,
    /// Provider is write-blind (Codex): approximate the new mass as the fresh
    /// (non-cache-read) input tokens.
    DerivedFreshInput,
}

impl CacheMassModel {
    /// Short label for the pairing metric this lane can measure.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::ReportedWrite => "reported-write",
            Self::DerivedFreshInput => "derived-fresh-input",
        }
    }

    fn mass(self, usage: &ProviderUsage) -> u64 {
        match self {
            Self::ReportedWrite => usage.cache_write_input_tokens,
            Self::DerivedFreshInput => usage
                .input_tokens
                .saturating_sub(usage.cache_read_input_tokens),
        }
    }
}

/// The realized reduction one compaction apply produced, reconstructed from the
/// event's estimates so both the covered-range and total-context ratios are
/// reported without trusting a single number.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ApplyReclamation {
    pub(crate) before: u64,
    pub(crate) after: u64,
    pub(crate) reclaimed: u64,
    pub(crate) covered_reduction_ratio: f64,
    pub(crate) total_reduction_ratio: f64,
}

/// Reconstruct the true pre-apply size and both reduction ratios from the
/// covered-range original estimate, the summary estimate, and the post-apply
/// context. A malformed triple (summary larger than original) clamps to zero
/// reclaimed rather than fabricating a negative reduction.
pub(crate) fn apply_reclamation(original: u64, summary: u64, after: u64) -> ApplyReclamation {
    let reclaimed = original.saturating_sub(summary);
    let before = after.saturating_add(reclaimed);
    ApplyReclamation {
        before,
        after,
        reclaimed,
        covered_reduction_ratio: if original == 0 {
            0.0
        } else {
            reclaimed as f64 / original as f64
        },
        total_reduction_ratio: if before == 0 {
            0.0
        } else {
            reclaimed as f64 / before as f64
        },
    }
}

/// The paired cache mass across every compaction apply in one run.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct ParentCacheEconomics {
    pub(crate) paired_applies: usize,
    pub(crate) baseline_mass: u64,
    pub(crate) post_mass: u64,
    pub(crate) ratio: Option<f64>,
}

/// Pair each apply with the nearest parent (non-summary) request before and
/// after it and sum the cache mass on each side. Applies with no request on one
/// side are skipped rather than fabricating a zero, so the ratio reflects only
/// genuinely paired boundaries.
pub(crate) fn parent_cache_economics(
    model: CacheMassModel,
    applies: &[(Instant, ApplyReclamation)],
    captured: &[CapturedUsage],
) -> ParentCacheEconomics {
    let parent = captured
        .iter()
        .filter(|sample| !sample.is_summary && sample.usage.is_some())
        .collect::<Vec<_>>();
    let mut metric = ParentCacheEconomics::default();
    for (applied_at, _) in applies {
        let before = parent
            .iter()
            .rev()
            .find(|sample| sample.started_at < *applied_at)
            .and_then(|sample| sample.usage.as_ref());
        let after = parent
            .iter()
            .find(|sample| sample.started_at >= *applied_at)
            .and_then(|sample| sample.usage.as_ref());
        let (Some(before), Some(after)) = (before, after) else {
            continue;
        };
        metric.paired_applies += 1;
        metric.baseline_mass = metric.baseline_mass.saturating_add(model.mass(before));
        metric.post_mass = metric.post_mass.saturating_add(model.mass(after));
    }
    metric.ratio =
        (metric.baseline_mass > 0).then_some(metric.post_mass as f64 / metric.baseline_mass as f64);
    metric
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn apply_reclamation_reconstructs_true_pre_apply_and_both_ratios() {
        let metric = apply_reclamation(8_000, 1_000, 17_000);
        assert_eq!(metric.before, 24_000);
        assert_eq!(metric.after, 17_000);
        assert_eq!(metric.reclaimed, 7_000);
        assert!((metric.covered_reduction_ratio - 0.875).abs() < f64::EPSILON);
        assert!((metric.total_reduction_ratio - 7.0 / 24.0).abs() < f64::EPSILON);

        let malformed = apply_reclamation(10, 20, 30);
        assert_eq!(malformed.reclaimed, 0);
        assert_eq!(malformed.before, malformed.after);
        assert_eq!(malformed.total_reduction_ratio, 0.0);
    }

    #[test]
    fn parent_cache_economics_pairs_requests_across_apply_without_fabricating_writes() {
        let at = Instant::now();
        let usage = |input, read, write| ProviderUsage {
            provider: "test".to_string(),
            model: "test".to_string(),
            input_tokens: input,
            output_tokens: 0,
            cache_read_input_tokens: read,
            cache_write_input_tokens: write,
            reasoning_output_tokens: 0,
            total_tokens: input,
            cache_creation: None,
        };
        let captured = vec![
            CapturedUsage {
                is_summary: false,
                tag: "before".to_string(),
                started_at: at - Duration::from_millis(1),
                usage: Some(usage(100, 70, 10)),
                estimate_tokens: 0,
            },
            CapturedUsage {
                is_summary: false,
                tag: "after".to_string(),
                started_at: at + Duration::from_millis(1),
                usage: Some(usage(140, 60, 20)),
                estimate_tokens: 0,
            },
        ];
        let applies = vec![(at, apply_reclamation(80, 20, 100))];

        let anthropic = parent_cache_economics(CacheMassModel::ReportedWrite, &applies, &captured);
        assert_eq!((anthropic.baseline_mass, anthropic.post_mass), (10, 20));
        assert_eq!(anthropic.ratio, Some(2.0));

        let codex = parent_cache_economics(CacheMassModel::DerivedFreshInput, &applies, &captured);
        assert_eq!((codex.baseline_mass, codex.post_mass), (30, 80));
        assert_eq!(codex.ratio, Some(8.0 / 3.0));
    }
}
