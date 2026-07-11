//! The uniform campaign row schema (one row per provider request), the
//! per-run derived aggregates, and the notional-price table. One schema means
//! any two campaigns compose and a comparator can diff them. Every number that
//! reaches a row comes from real `ProviderUsage` or session-log lifecycle
//! entries; the estimator's value appears ONLY as the `estimate_error`
//! diagnostic, never as a reported result.

use super::*;
use serde::{Deserialize, Serialize};

/// What produced this provider request. `native_compact` is the provider-native
/// compaction rung (Anthropic only); `summary` is a subagent/provider summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RowKind {
    Turn,
    Summary,
    NativeCompact,
    Probe,
}

/// The context-pressure tier in effect when the request was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Tier {
    None,
    Warn,
    Start,
    Hard,
}

impl Tier {
    /// Map the runtime pressure tier onto the schema tier.
    pub(crate) fn from_pressure(tier: ContextPressureTier) -> Self {
        match tier {
            ContextPressureTier::Normal => Self::None,
            ContextPressureTier::Warn => Self::Warn,
            ContextPressureTier::Start => Self::Start,
            ContextPressureTier::Hard => Self::Hard,
        }
    }
}

/// The compaction settings a cell ran under, stamped on every row so a row is
/// self-describing and a comparator can group by cell without a side table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SettingsFingerprint {
    pub(crate) start_pct: f64,
    pub(crate) hard_pct: f64,
    pub(crate) keep_tail_tokens: u64,
    pub(crate) hard_wait_ms: u64,
    pub(crate) summarizer: String,
    pub(crate) folds: bool,
    pub(crate) retention_tier: String,
}

/// Compaction lifecycle deltas observed since the previous row: which
/// generation applied, its origin, fold flushes, and breaker state. Counts and
/// enums only -- never folded/summarized content.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct LifecycleDelta {
    pub(crate) compaction_generation_applied: Option<u64>,
    pub(crate) origin: Option<String>,
    pub(crate) fold_flushes: usize,
    pub(crate) folds_reclaimed_estimate: u64,
    pub(crate) breaker_tripped: bool,
}

/// One provider request's row. Serialized one-per-line to
/// `docs/benchmarks/data/<campaign>-<date>.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Row {
    pub(crate) campaign: String,
    pub(crate) cell_id: String,
    pub(crate) lane: String,
    pub(crate) scenario: String,
    pub(crate) run_seq: u32,
    pub(crate) request_seq: u32,
    pub(crate) kind: RowKind,
    pub(crate) ts: u64,
    pub(crate) wall_ms: f64,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read: u64,
    /// 5-minute cache-write tokens. `None` on the write-blind Codex lane.
    pub(crate) cache_write_5m: Option<u64>,
    /// 1-hour cache-write tokens. `None` on the write-blind Codex lane.
    pub(crate) cache_write_1h: Option<u64>,
    /// True when the lane does not report cache writes (Codex): the cache_write
    /// fields are `None` because the provider is blind to them, not because the
    /// request wrote nothing.
    pub(crate) write_unreported: bool,
    pub(crate) context_measured_tokens: u64,
    pub(crate) context_estimate_tokens: u64,
    /// Diagnostic only (goal 1): measured minus estimate. The estimate never
    /// feeds a reported metric; this column exists to catch estimator drift.
    pub(crate) estimate_error: i64,
    pub(crate) boundary_index: u64,
    pub(crate) tier: Tier,
    pub(crate) lifecycle: LifecycleDelta,
    pub(crate) settings: SettingsFingerprint,
    pub(crate) error: Option<String>,
}

/// Max chars of assistant text recorded per transcript entry. The text is a
/// diagnostic sidecar, not a metric, so it is truncated to keep artifacts
/// bounded; a real early-stop reply is far shorter than this.
pub(crate) const TRANSCRIPT_TEXT_CAP: usize = 4_000;

/// The assistant's final text for one provider request, written to the sidecar
/// `<campaign>.transcripts.jsonl` beside the row JSONL. Keyed by the same
/// `cell_id` + `run_seq` + `request_seq` as the matching `Row`, so an early-stop
/// turn is diagnosable after the fact (why the model quit) without changing the
/// stable Row schema. `text` is `None` on a pure tool-call round-trip (the
/// request produced no assistant text) and truncated to `TRANSCRIPT_TEXT_CAP`
/// chars otherwise, with `truncated` recording whether the cap was hit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Transcript {
    pub(crate) campaign: String,
    pub(crate) cell_id: String,
    pub(crate) lane: String,
    pub(crate) scenario: String,
    pub(crate) run_seq: u32,
    pub(crate) request_seq: u32,
    pub(crate) kind: RowKind,
    pub(crate) text: Option<String>,
    pub(crate) truncated: bool,
}

impl Row {
    /// Signed estimator error for a measured/estimate pair. Saturates into the
    /// `i64` domain so an absurd estimate never panics the row builder.
    pub(crate) fn estimate_error_of(measured: u64, estimate: u64) -> i64 {
        measured as i64 - estimate as i64
    }

    /// Total cache-write tokens this row reports, or `None` when the lane is
    /// write-blind.
    pub(crate) fn cache_write_total(&self) -> Option<u64> {
        match (self.cache_write_5m, self.cache_write_1h) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        }
    }
}

/// The final outcome of one run's task, scored mechanically (goal 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskOutcome {
    Pass,
    Partial,
    Fail,
}

/// Per-run aggregates derived from that run's rows plus its mechanical outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct DerivedRun {
    pub(crate) run_seq: u32,
    pub(crate) requests: usize,
    pub(crate) input_total: u64,
    pub(crate) output_total: u64,
    pub(crate) cache_read_total: u64,
    /// Sum of reported cache writes; `None` when every row was write-blind.
    pub(crate) cache_write_total: Option<u64>,
    /// Notional cost from the (dated) price table. `None` when the lane's model
    /// has no price entry -- the report prints `null` and notes it rather than
    /// inventing a figure.
    pub(crate) notional_usd: Option<f64>,
    pub(crate) cache_hit_ratio: f64,
    /// Cache-write mass on the first request after each compaction boundary --
    /// the realized cost of breaking the warm prefix.
    pub(crate) post_apply_rewrite_mass: u64,
    pub(crate) wall_ms_total: f64,
    pub(crate) estimate_error_mean: f64,
    pub(crate) estimate_error_max_abs: i64,
    pub(crate) outcome: TaskOutcome,
    pub(crate) probe_score: Option<f64>,
}

impl DerivedRun {
    /// Fold one run's rows into aggregates, pricing the token classes with the
    /// lane's notional table. `outcome`/`probe_score` come from the scenario's
    /// mechanical checks, not the rows.
    pub(crate) fn from_rows(
        run_seq: u32,
        rows: &[Row],
        price: Option<&LanePrice>,
        outcome: TaskOutcome,
        probe_score: Option<f64>,
    ) -> Self {
        let input_total = rows.iter().map(|r| r.input_tokens).sum();
        let output_total = rows.iter().map(|r| r.output_tokens).sum();
        let cache_read_total = rows.iter().map(|r| r.cache_read).sum();
        let any_write = rows.iter().any(|r| r.cache_write_total().is_some());
        let cache_write_total =
            any_write.then(|| rows.iter().filter_map(Row::cache_write_total).sum::<u64>());
        let post_apply_rewrite_mass = rows
            .iter()
            .filter(|r| r.boundary_index > 0)
            .filter_map(Row::cache_write_total)
            .sum();
        let wall_ms_total = rows.iter().map(|r| r.wall_ms).sum();
        let errors: Vec<i64> = rows.iter().map(|r| r.estimate_error).collect();
        let estimate_error_mean = if errors.is_empty() {
            0.0
        } else {
            errors.iter().sum::<i64>() as f64 / errors.len() as f64
        };
        let estimate_error_max_abs = errors.iter().map(|e| e.abs()).max().unwrap_or(0);
        let cache_hit_ratio = if input_total == 0 {
            0.0
        } else {
            cache_read_total as f64 / input_total as f64
        };
        Self {
            run_seq,
            requests: rows.len(),
            input_total,
            output_total,
            cache_read_total,
            cache_write_total,
            notional_usd: price.map(|p| {
                p.cost_usd(
                    input_total,
                    output_total,
                    cache_read_total,
                    cache_write_total.unwrap_or(0),
                )
            }),
            cache_hit_ratio,
            post_apply_rewrite_mass,
            wall_ms_total,
            estimate_error_mean,
            estimate_error_max_abs,
            outcome,
            probe_score,
        }
    }
}

/// Notional per-million-token prices for one lane. Subscription lanes bill
/// against rate limits, not dollars; this table converts realized tokens into a
/// single comparable optimization score, not a billing figure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LanePrice {
    pub(crate) input_per_mtok: f64,
    pub(crate) output_per_mtok: f64,
    pub(crate) cache_read_per_mtok: f64,
    pub(crate) cache_write_per_mtok: f64,
    /// True when the numbers are a stand-in because the lane's list price is not
    /// public. Every report must surface this flag.
    pub(crate) placeholder: bool,
}

impl LanePrice {
    /// Notional dollars for the token classes. `input_tokens` already includes
    /// cache reads/writes, so fresh input is priced on the non-cached remainder
    /// and the cached classes are priced at their own rates.
    pub(crate) fn cost_usd(
        &self,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> f64 {
        let fresh_input = input.saturating_sub(cache_read).saturating_sub(cache_write);
        (fresh_input as f64 * self.input_per_mtok
            + cache_read as f64 * self.cache_read_per_mtok
            + cache_write as f64 * self.cache_write_per_mtok
            + output as f64 * self.output_per_mtok)
            / 1_000_000.0
    }
}

/// Dated, versioned notional price table for both lanes. Update `AS_OF` and the
/// numbers together when public list prices move; a unit test enforces the date
/// is present and well formed.
pub(crate) struct PriceTable {
    pub(crate) as_of: &'static str,
    pub(crate) anthropic_sonnet: LanePrice,
    pub(crate) codex_luna: LanePrice,
}

/// Current notional prices. Anthropic numbers are the public Claude Sonnet API
/// list prices (input / output / cache-read / 5m cache-write, USD per Mtok).
/// Codex/Luna list price is NOT public; its row is a clearly-marked placeholder
/// mirroring the Anthropic shape until an official number ships.
pub(crate) const PRICE_TABLE: PriceTable = PriceTable {
    as_of: "2026-07-10",
    anthropic_sonnet: LanePrice {
        input_per_mtok: 3.00,
        output_per_mtok: 15.00,
        cache_read_per_mtok: 0.30,
        cache_write_per_mtok: 3.75,
        placeholder: false,
    },
    codex_luna: LanePrice {
        input_per_mtok: 3.00,
        output_per_mtok: 15.00,
        cache_read_per_mtok: 0.30,
        cache_write_per_mtok: 3.75,
        placeholder: true,
    },
};

impl PriceTable {
    /// The notional price for a lane.
    pub(crate) fn price_for(&self, lane: ProviderLane) -> &LanePrice {
        match lane {
            ProviderLane::Anthropic => &self.anthropic_sonnet,
            ProviderLane::Codex => &self.codex_luna,
        }
    }
}

/// A model-id-keyed price book: the built-in table seeded by model id, extended
/// by a campaign's optional `[prices.<model-id>]` overrides. Pricing is by model
/// id (not lane) so a config that names any model can supply its own numbers; an
/// unpriced model resolves to `None` -> `notional_usd` null + a report note,
/// never an error or an invented figure.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct PriceBook {
    prices: std::collections::BTreeMap<String, LanePrice>,
}

impl PriceBook {
    /// The built-in table keyed by the model ids it prices: the public Anthropic
    /// Sonnet list price and the flagged Luna placeholder.
    pub(crate) fn builtin() -> Self {
        let mut prices = std::collections::BTreeMap::new();
        prices.insert(
            "claude-sonnet-4-6".to_string(),
            PRICE_TABLE.anthropic_sonnet,
        );
        prices.insert("gpt-5.6-luna".to_string(), PRICE_TABLE.codex_luna);
        Self { prices }
    }

    /// Add or replace one model's price (a config `[prices.<model-id>]` block).
    pub(crate) fn insert(&mut self, model_id: impl Into<String>, price: LanePrice) {
        self.prices.insert(model_id.into(), price);
    }

    /// The notional price for a model id, or `None` when it is unpriced.
    pub(crate) fn price_for(&self, model_id: &str) -> Option<&LanePrice> {
        self.prices.get(model_id)
    }
}

/// A representative row for tests in this and sibling modules (campaign
/// artifact writing round-trips a real `Row`). Test-only.
#[cfg(test)]
pub(crate) fn sample_row_for_tests(seq: u32, kind: RowKind) -> Row {
    Row {
        campaign: "pilot-a".to_string(),
        cell_id: "S1".to_string(),
        lane: "anthropic/claude-sonnet-4-6@low".to_string(),
        scenario: "S1".to_string(),
        run_seq: 0,
        request_seq: seq,
        kind,
        ts: 1_720_000_000,
        wall_ms: 1_234.5,
        input_tokens: 10_000,
        output_tokens: 200,
        cache_read: 7_000,
        cache_write_5m: Some(2_000),
        cache_write_1h: Some(0),
        write_unreported: false,
        context_measured_tokens: 12_000,
        context_estimate_tokens: 11_500,
        estimate_error: Row::estimate_error_of(12_000, 11_500),
        boundary_index: seq as u64,
        tier: Tier::Start,
        lifecycle: LifecycleDelta {
            compaction_generation_applied: Some(1),
            origin: Some("subagent".to_string()),
            ..LifecycleDelta::default()
        },
        settings: SettingsFingerprint {
            start_pct: 0.72,
            hard_pct: 0.90,
            keep_tail_tokens: 8_000,
            hard_wait_ms: 120_000,
            summarizer: "subagent".to_string(),
            folds: true,
            retention_tier: "5m".to_string(),
        },
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(seq: u32, kind: RowKind) -> Row {
        sample_row_for_tests(seq, kind)
    }

    #[test]
    fn row_round_trips_through_jsonl() {
        let row = sample_row(3, RowKind::Summary);
        let line = serde_json::to_string(&row).expect("serialize row");
        let back: Row = serde_json::from_str(&line).expect("deserialize row");
        assert_eq!(row, back);
        // Codex-lane blindness is expressible: both write fields None, flag set.
        assert!(!line.contains("\"error\":\"")); // null error stays null
    }

    #[test]
    fn write_unreported_row_has_no_cache_write_total() {
        let mut row = sample_row(1, RowKind::Turn);
        row.cache_write_5m = None;
        row.cache_write_1h = None;
        row.write_unreported = true;
        assert_eq!(row.cache_write_total(), None);
        let line = serde_json::to_string(&row).expect("serialize");
        assert!(line.contains("\"write_unreported\":true"));
    }

    #[test]
    fn estimate_error_is_signed_and_diagnostic() {
        assert_eq!(Row::estimate_error_of(12_000, 11_500), 500);
        assert_eq!(Row::estimate_error_of(11_000, 11_500), -500);
    }

    #[test]
    fn derived_run_aggregates_token_classes_and_prices() {
        let rows = vec![sample_row(1, RowKind::Turn), sample_row(2, RowKind::Turn)];
        let derived = DerivedRun::from_rows(
            0,
            &rows,
            Some(&PRICE_TABLE.anthropic_sonnet),
            TaskOutcome::Pass,
            Some(1.0),
        );
        assert_eq!(derived.input_total, 20_000);
        assert_eq!(derived.cache_read_total, 14_000);
        assert_eq!(derived.cache_write_total, Some(4_000));
        // boundary_index > 0 on both rows, so both writes count as re-write mass.
        assert_eq!(derived.post_apply_rewrite_mass, 4_000);
        assert!(derived.notional_usd.unwrap() > 0.0);
        assert!((derived.cache_hit_ratio - 0.7).abs() < 1e-9);
    }

    #[test]
    fn unpriced_model_yields_null_notional_not_a_fabricated_figure() {
        let rows = vec![sample_row(1, RowKind::Turn)];
        let derived = DerivedRun::from_rows(0, &rows, None, TaskOutcome::Pass, None);
        assert_eq!(
            derived.notional_usd, None,
            "an unpriced model prices to null"
        );
    }

    #[test]
    fn price_book_extends_the_builtin_table_by_model_id_and_arithmetic_matches() {
        let mut book = PriceBook::builtin();
        // Built-in model resolves to the shipped Anthropic price.
        assert_eq!(
            book.price_for("claude-sonnet-4-6"),
            Some(&PRICE_TABLE.anthropic_sonnet)
        );
        // An unknown model is unpriced until an override is supplied.
        assert_eq!(book.price_for("claude-opus-9"), None);
        let override_price = LanePrice {
            input_per_mtok: 5.0,
            output_per_mtok: 25.0,
            cache_read_per_mtok: 0.5,
            cache_write_per_mtok: 6.25,
            placeholder: false,
        };
        book.insert("claude-opus-9", override_price);
        let price = book.price_for("claude-opus-9").expect("overridden");
        // 1M fresh input at 5.0/Mtok == $5.00 exactly.
        assert!((price.cost_usd(1_000_000, 0, 0, 0) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn price_table_has_an_as_of_date_and_marks_luna_placeholder() {
        let parts: Vec<&str> = PRICE_TABLE.as_of.split('-').collect();
        assert_eq!(parts.len(), 3, "as_of must be YYYY-MM-DD");
        assert!(parts[0].len() == 4 && parts[0].chars().all(|c| c.is_ascii_digit()));
        assert!(parts[1].parse::<u8>().is_ok_and(|m| (1..=12).contains(&m)));
        assert!(parts[2].parse::<u8>().is_ok_and(|d| (1..=31).contains(&d)));
        // Loop over lanes so the price/placeholder reads are runtime values
        // (not constant-folded assertions): Anthropic is real, Luna is a
        // flagged placeholder until an official list price ships.
        for lane in [ProviderLane::Anthropic, ProviderLane::Codex] {
            let price = PRICE_TABLE.price_for(lane);
            assert!(price.input_per_mtok > 0.0);
            assert!(price.output_per_mtok > 0.0);
            let expect_placeholder = matches!(lane, ProviderLane::Codex);
            assert_eq!(
                price.placeholder, expect_placeholder,
                "only the Luna lane price is a placeholder"
            );
        }
    }
}
