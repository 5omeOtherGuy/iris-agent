//! Config-file campaigns (goal 1): parse and validate a versioned TOML campaign
//! definition into the same `CampaignSpec` the built-in `pilot_a()` produces, so
//! any operator can run the harness against any model without editing Rust.
//!
//! This module owns the SCHEMA, PARSING, and VALIDATION only. Expansion,
//! manifests, and artifact writing stay in `campaign.rs`; lane construction
//! stays in `lanes.rs`; scenario construction stays in `scenario.rs`. Validation
//! is a system boundary: every rejection names the field, the offending value,
//! and the accepted range or set, and each rule has a unit test.
//!
//! Format choice: TOML, because `toml` (1.1) is already a first-party
//! dependency (it parses the vendored RTK filter data). No new crate is added.

use super::*;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// The only schema version this loader accepts. Bump together with a migration
/// when the shape changes; a mismatch is a named, actionable error.
pub(crate) const CAMPAIGN_SCHEMA_VERSION: u64 = 1;

/// Default runs-per-cell when a config omits `[campaign].runs`.
const DEFAULT_RUNS: u32 = 2;

// --- Validation ranges (system boundary). Every bound is named in an error. ---
const MIN_BUDGET: u64 = 8_192;
const MIN_ROUND_TRIPS: usize = 1;
const MAX_ROUND_TRIPS: usize = 16;
const MIN_START: f64 = 0.1;
const MAX_START: f64 = 0.95;
const MAX_HARD: f64 = 0.99;

/// The raw TOML shape, before validation. `deny_unknown_fields` turns a typo'd
/// key into a parse error that names the unknown field rather than silently
/// ignoring it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    schema: u64,
    campaign: RawCampaign,
    #[serde(default)]
    lanes: Vec<RawLane>,
    #[serde(default)]
    cells: Vec<RawCell>,
    #[serde(default)]
    prices: BTreeMap<String, RawPrice>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCampaign {
    name: String,
    #[serde(default)]
    runs: Option<u32>,
    #[serde(default)]
    exclusion_budget: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLane {
    provider: String,
    model: String,
    effort: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCell {
    scenario: String,
    // Scenario size-knob overrides (optional).
    #[serde(default)]
    budget: Option<u64>,
    #[serde(default)]
    round_trips: Option<usize>,
    #[serde(default)]
    seed_repeat: Option<usize>,
    #[serde(default)]
    result_repeat: Option<usize>,
    // Compaction settings overrides (optional).
    #[serde(default)]
    start: Option<f64>,
    #[serde(default)]
    hard: Option<f64>,
    #[serde(default)]
    keep_tail_tokens: Option<u64>,
    #[serde(default)]
    hard_wait_ms: Option<u64>,
    #[serde(default)]
    summarizer: Option<String>,
    #[serde(default)]
    retention: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPrice {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    // Advisory only: the report stamps the built-in table's `as_of`. Declared so
    // `deny_unknown_fields` accepts the key; parsed and intentionally not stored.
    #[serde(default)]
    #[allow(dead_code)]
    as_of: Option<String>,
}

/// Load and validate a campaign config file into a `CampaignSpec`.
pub(crate) fn load_campaign_file(path: &Path) -> Result<CampaignSpec> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| anyhow::anyhow!("reading campaign file {}: {err}", path.display()))?;
    campaign_from_toml(&text)
        .map_err(|err| anyhow::anyhow!("invalid campaign file {}: {err:#}", path.display()))
}

/// Parse and validate a campaign from TOML text. Kept file-IO-free so the
/// validation matrix is unit-tested in the gate.
pub(crate) fn campaign_from_toml(text: &str) -> Result<CampaignSpec> {
    let raw: RawConfig =
        toml::from_str(text).map_err(|err| anyhow::anyhow!("TOML parse error: {err}"))?;

    if raw.schema != CAMPAIGN_SCHEMA_VERSION {
        return Err(anyhow::anyhow!(
            "field `schema` value {} is not supported; this loader accepts schema = {}",
            raw.schema,
            CAMPAIGN_SCHEMA_VERSION
        ));
    }

    if raw.campaign.name.trim().is_empty() {
        return Err(anyhow::anyhow!(
            "field `campaign.name` must be a non-empty string"
        ));
    }
    let runs = raw.campaign.runs.unwrap_or(DEFAULT_RUNS);
    if runs == 0 {
        return Err(anyhow::anyhow!(
            "field `campaign.runs` value 0 is out of range; accepted range: runs >= 1"
        ));
    }
    let exclusion_budget = raw
        .campaign
        .exclusion_budget
        .unwrap_or(LIVE_EXCLUSION_BUDGET);

    if raw.lanes.is_empty() {
        return Err(anyhow::anyhow!(
            "at least one `[[lanes]]` entry is required"
        ));
    }
    let lanes = raw
        .lanes
        .iter()
        .map(validate_lane)
        .collect::<Result<Vec<_>>>()?;

    if raw.cells.is_empty() {
        return Err(anyhow::anyhow!(
            "at least one `[[cells]]` entry is required"
        ));
    }
    let cells = raw
        .cells
        .iter()
        .map(validate_cell)
        .collect::<Result<Vec<_>>>()?;

    let mut prices = PriceBook::builtin();
    for (model_id, raw_price) in &raw.prices {
        prices.insert(model_id.clone(), validate_price(model_id, raw_price)?);
    }

    Ok(CampaignSpec {
        name: raw.campaign.name,
        lanes,
        cells,
        runs,
        exclusion_budget,
        prices,
    })
}

/// Validate one `[[lanes]]` entry: provider and effort come from a closed set;
/// the model id is passed verbatim to the provider constructor.
fn validate_lane(raw: &RawLane) -> Result<LaneSpec> {
    let lane = ProviderLane::parse(&raw.provider)?;
    let effort = LaneEffort::parse(&raw.effort)?;
    if raw.model.trim().is_empty() {
        return Err(anyhow::anyhow!(
            "field `lanes.model` must be a non-empty model id string"
        ));
    }
    Ok(LaneSpec::new(lane, raw.model.clone(), effort))
}

/// Validate one `[[cells]]` entry: the scenario id from the registry's accepted
/// set, knob overrides against their ranges, and settings overrides layered on
/// the shipped `CellSettings::defaults()`.
fn validate_cell(raw: &RawCell) -> Result<CellSpec> {
    if !ACCEPTED_SCENARIO_IDS.contains(&raw.scenario.as_str()) {
        return Err(anyhow::anyhow!(
            "field `cells.scenario` value {:?} is not a known scenario; accepted scenarios: {}",
            raw.scenario,
            ACCEPTED_SCENARIO_IDS.join(", ")
        ));
    }

    // --- Knob overrides ---
    if let Some(budget) = raw.budget
        && budget < MIN_BUDGET
    {
        return Err(anyhow::anyhow!(
            "field `cells.budget` value {budget} is out of range; accepted range: budget >= {MIN_BUDGET}"
        ));
    }
    if let Some(round_trips) = raw.round_trips
        && !(MIN_ROUND_TRIPS..=MAX_ROUND_TRIPS).contains(&round_trips)
    {
        return Err(anyhow::anyhow!(
            "field `cells.round_trips` value {round_trips} is out of range; accepted range: {MIN_ROUND_TRIPS}..={MAX_ROUND_TRIPS}"
        ));
    }
    if let Some(seed_repeat) = raw.seed_repeat
        && seed_repeat == 0
    {
        return Err(anyhow::anyhow!(
            "field `cells.seed_repeat` value 0 is out of range; accepted range: seed_repeat >= 1"
        ));
    }
    if let Some(result_repeat) = raw.result_repeat
        && result_repeat == 0
    {
        return Err(anyhow::anyhow!(
            "field `cells.result_repeat` value 0 is out of range; accepted range: result_repeat >= 1"
        ));
    }
    let knobs = ScenarioKnobs {
        budget: raw.budget,
        round_trips: raw.round_trips,
        seed_repeat: raw.seed_repeat,
        result_repeat: raw.result_repeat,
    };

    // --- Settings overrides layered on the shipped defaults ---
    let mut settings = CellSettings::defaults();
    if let Some(start) = raw.start {
        if !(MIN_START..=MAX_START).contains(&start) {
            return Err(anyhow::anyhow!(
                "field `cells.start` value {start} is out of range; accepted range: {MIN_START}..={MAX_START}"
            ));
        }
        settings.start = start;
    }
    if let Some(hard) = raw.hard {
        if hard <= settings.start || hard > MAX_HARD {
            return Err(anyhow::anyhow!(
                "field `cells.hard` value {hard} is out of range; accepted range: start ({}) < hard <= {MAX_HARD}",
                settings.start
            ));
        }
        settings.hard = hard;
    } else if settings.hard <= settings.start {
        // A start override that crosses the default hard is a user error, named
        // rather than silently producing an invalid tier ordering.
        return Err(anyhow::anyhow!(
            "field `cells.start` value {} must stay below `hard` ({}); set `hard` too",
            settings.start,
            settings.hard
        ));
    }
    if let Some(keep) = raw.keep_tail_tokens {
        if keep == 0 {
            return Err(anyhow::anyhow!(
                "field `cells.keep_tail_tokens` value 0 is out of range; accepted range: keep_tail_tokens >= 1"
            ));
        }
        settings.keep_tail_tokens = keep;
    }
    if let Some(wait) = raw.hard_wait_ms {
        settings.hard_wait_ms = wait;
    }
    if let Some(summarizer) = &raw.summarizer {
        const ACCEPTED_SUMMARIZERS: [&str; 3] = ["subagent", "provider", "excerpts"];
        if !ACCEPTED_SUMMARIZERS.contains(&summarizer.as_str()) {
            return Err(anyhow::anyhow!(
                "field `cells.summarizer` value {summarizer:?} is not supported; accepted: {}",
                ACCEPTED_SUMMARIZERS.join(", ")
            ));
        }
        settings.summarizer = summarizer.clone();
    }
    if let Some(retention) = &raw.retention {
        const ACCEPTED_RETENTION: [&str; 2] = ["5m", "1h"];
        if !ACCEPTED_RETENTION.contains(&retention.as_str()) {
            return Err(anyhow::anyhow!(
                "field `cells.retention` value {retention:?} is not supported; accepted: {}",
                ACCEPTED_RETENTION.join(", ")
            ));
        }
        settings.retention_tier = retention.clone();
    }

    Ok(CellSpec {
        scenario_id: raw.scenario.clone(),
        settings,
        knobs,
    })
}

/// Validate one `[prices.<model-id>]` block into a `LanePrice`. Non-negative
/// per-mtok rates; `as_of` is advisory (the built-in table date still stamps
/// the report), so it is accepted but not required.
fn validate_price(model_id: &str, raw: &RawPrice) -> Result<LanePrice> {
    for (field, value) in [
        ("input", raw.input),
        ("output", raw.output),
        ("cache_read", raw.cache_read),
        ("cache_write", raw.cache_write),
    ] {
        if value < 0.0 || !value.is_finite() {
            return Err(anyhow::anyhow!(
                "field `prices.{model_id}.{field}` value {value} is out of range; accepted range: >= 0"
            ));
        }
    }
    Ok(LanePrice {
        input_per_mtok: raw.input,
        output_per_mtok: raw.output,
        cache_read_per_mtok: raw.cache_read,
        cache_write_per_mtok: raw.cache_write,
        // A config-supplied price is a real number the operator vouched for, not
        // a placeholder; report it as such.
        placeholder: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> &'static str {
        r#"
schema = 1
[campaign]
name = "t"
[[lanes]]
provider = "anthropic"
model = "claude-sonnet-4-6"
effort = "low"
[[cells]]
scenario = "S1"
"#
    }

    #[test]
    fn minimal_config_parses_with_defaults() {
        let spec = campaign_from_toml(minimal()).expect("parse");
        assert_eq!(spec.name, "t");
        assert_eq!(spec.runs, DEFAULT_RUNS);
        assert_eq!(spec.exclusion_budget, LIVE_EXCLUSION_BUDGET);
        assert_eq!(spec.lanes.len(), 1);
        assert_eq!(spec.cells.len(), 1);
        // Settings default to the shipped posture (post hard_wait fix).
        assert_eq!(spec.cells[0].settings, CellSettings::defaults());
        assert_eq!(spec.cells[0].knobs, ScenarioKnobs::default());
    }

    #[test]
    fn wrong_schema_version_names_field_and_accepted_value() {
        let text = minimal().replace("schema = 1", "schema = 2");
        let err = campaign_from_toml(&text).unwrap_err().to_string();
        assert!(err.contains("schema"), "{err}");
        assert!(err.contains('2') && err.contains("schema = 1"), "{err}");
    }

    #[test]
    fn unknown_provider_lists_supported_set() {
        let text = minimal().replace("provider = \"anthropic\"", "provider = \"gemini\"");
        let err = campaign_from_toml(&text).unwrap_err().to_string();
        assert!(err.contains("gemini"), "{err}");
        assert!(err.contains("anthropic") && err.contains("codex"), "{err}");
    }

    #[test]
    fn unknown_effort_and_scenario_are_named_errors() {
        let bad_effort = minimal().replace("effort = \"low\"", "effort = \"turbo\"");
        assert!(
            campaign_from_toml(&bad_effort)
                .unwrap_err()
                .to_string()
                .contains("turbo")
        );
        let bad_scenario = minimal().replace("scenario = \"S1\"", "scenario = \"S9\"");
        let err = campaign_from_toml(&bad_scenario).unwrap_err().to_string();
        assert!(err.contains("S9") && err.contains("S1"), "{err}");
    }

    #[test]
    fn empty_lanes_or_cells_are_rejected() {
        let no_lanes = r#"
schema = 1
[campaign]
name = "t"
[[cells]]
scenario = "S1"
"#;
        assert!(
            campaign_from_toml(no_lanes)
                .unwrap_err()
                .to_string()
                .contains("[[lanes]]")
        );
        let no_cells = r#"
schema = 1
[campaign]
name = "t"
[[lanes]]
provider = "anthropic"
model = "m"
effort = "low"
"#;
        assert!(
            campaign_from_toml(no_cells)
                .unwrap_err()
                .to_string()
                .contains("[[cells]]")
        );
    }

    #[test]
    fn budget_and_round_trip_ranges_are_enforced_with_named_bounds() {
        let small_budget = format!("{}\nbudget = 4096", minimal());
        let err = campaign_from_toml(&small_budget).unwrap_err().to_string();
        assert!(
            err.contains("cells.budget") && err.contains("8192"),
            "{err}"
        );

        let big_rt = format!("{}\nround_trips = 99", minimal());
        let err = campaign_from_toml(&big_rt).unwrap_err().to_string();
        assert!(
            err.contains("cells.round_trips") && err.contains("1..=16"),
            "{err}"
        );
    }

    #[test]
    fn start_and_hard_ordering_is_enforced() {
        // hard <= start is rejected, naming the ordering.
        let bad = format!("{}\nstart = 0.80\nhard = 0.70", minimal());
        let err = campaign_from_toml(&bad).unwrap_err().to_string();
        assert!(err.contains("cells.hard") && err.contains("start"), "{err}");

        // start above the range is rejected.
        let bad_start = format!("{}\nstart = 0.99\nhard = 0.995", minimal());
        let err = campaign_from_toml(&bad_start).unwrap_err().to_string();
        assert!(err.contains("cells.start"), "{err}");

        // A valid override is accepted and layered on the defaults.
        let ok = format!(
            "{}\nstart = 0.60\nhard = 0.85\nsummarizer = \"provider\"",
            minimal()
        );
        let spec = campaign_from_toml(&ok).expect("valid overrides");
        assert_eq!(spec.cells[0].settings.start, 0.60);
        assert_eq!(spec.cells[0].settings.hard, 0.85);
        assert_eq!(spec.cells[0].settings.summarizer, "provider");
    }

    #[test]
    fn summarizer_and_retention_sets_are_closed() {
        let bad_sum = format!("{}\nsummarizer = \"magic\"", minimal());
        assert!(
            campaign_from_toml(&bad_sum)
                .unwrap_err()
                .to_string()
                .contains("cells.summarizer")
        );
        let bad_ret = format!("{}\nretention = \"1d\"", minimal());
        assert!(
            campaign_from_toml(&bad_ret)
                .unwrap_err()
                .to_string()
                .contains("cells.retention")
        );
    }

    #[test]
    fn price_override_extends_the_book_and_is_not_a_placeholder() {
        let text = format!(
            "{}\n[prices.custom-model]\ninput = 2.0\noutput = 8.0\ncache_read = 0.2\ncache_write = 2.5\nas_of = \"2026-07-10\"",
            minimal()
        );
        let spec = campaign_from_toml(&text).expect("parse with price");
        let price = spec.prices.price_for("custom-model").expect("priced");
        assert!(!price.placeholder);
        // 1M fresh input at 2.0/Mtok == $2.00.
        assert!((price.cost_usd(1_000_000, 0, 0, 0) - 2.0).abs() < 1e-9);
        // Built-ins still resolve.
        assert!(spec.prices.price_for("claude-sonnet-4-6").is_some());
        // A negative rate is a named error.
        let bad = text.replace("input = 2.0", "input = -1.0");
        assert!(
            campaign_from_toml(&bad)
                .unwrap_err()
                .to_string()
                .contains("prices.custom-model.input")
        );
    }

    /// Parity: the committed `pilot-a.toml` expands to EXACTLY the built-in
    /// `pilot_a()` plan (post hard_wait_ms fix). This pins the config schema to
    /// the reference plan and doubles as executable schema documentation.
    #[test]
    fn committed_pilot_a_toml_expands_to_the_builtin_plan() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/benchmarks/campaigns/pilot-a.toml");
        let from_file = load_campaign_file(&path).expect("load committed pilot-a.toml");
        assert_eq!(
            from_file,
            pilot_a(),
            "docs/benchmarks/campaigns/pilot-a.toml must expand to the built-in pilot_a() plan"
        );
    }

    #[test]
    fn t_series_cells_parse_with_the_round_trips_and_budget_knobs() {
        // A T-series cell is a first-class scenario id; its `round_trips` knob
        // (the repetition / fail-loud floor) and `budget` validate on the same
        // ranges the S-series knobs do.
        let ok = minimal().replace(
            "scenario = \"S1\"",
            "scenario = \"T4\"\nround_trips = 6\nbudget = 65536",
        );
        let spec = campaign_from_toml(&ok).expect("T4 cell parses");
        assert_eq!(spec.cells[0].scenario_id, "T4");
        assert_eq!(spec.cells[0].knobs.round_trips, Some(6));
        assert_eq!(spec.cells[0].knobs.budget, Some(65_536));

        // Out-of-range T-series knobs are rejected with the same named bounds.
        let bad_rt = minimal().replace("scenario = \"S1\"", "scenario = \"T2\"\nround_trips = 99");
        let err = campaign_from_toml(&bad_rt).unwrap_err().to_string();
        assert!(
            err.contains("cells.round_trips") && err.contains("1..=16"),
            "{err}"
        );
        let bad_budget = minimal().replace("scenario = \"S1\"", "scenario = \"T1\"\nbudget = 4096");
        let err = campaign_from_toml(&bad_budget).unwrap_err().to_string();
        assert!(
            err.contains("cells.budget") && err.contains("8192"),
            "{err}"
        );
    }

    /// The committed `tool-suite.toml` example loads and expands to a valid
    /// T1-T4 tool-efficiency plan on one lane, so the file the docs point the
    /// operator at is proven runnable in the gate.
    #[test]
    fn committed_tool_suite_toml_expands_to_a_t_series_plan() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/benchmarks/campaigns/tool-suite.toml");
        let spec = load_campaign_file(&path).expect("load committed tool-suite.toml");
        assert_eq!(spec.name, "tool-suite");
        assert_eq!(spec.lanes.len(), 1);
        let ids: Vec<&str> = spec.cells.iter().map(|c| c.scenario_id.as_str()).collect();
        assert_eq!(ids, ["T1", "T2", "T3", "T4"]);
        // Every cell resolves to a fixture-backed T-series scenario.
        for cell in &spec.cells {
            let scenario = build_scenario(&cell.scenario_id, &cell.knobs)
                .unwrap_or_else(|| panic!("registry builds {}", cell.scenario_id));
            assert_eq!(scenario.workspace_kind(), WorkspaceKind::Fixtures);
        }
    }

    #[test]
    fn unknown_top_level_key_is_rejected_not_ignored() {
        let text = format!("{}\n[campaign]\nname = \"dup\"", minimal());
        // Duplicate table is a TOML parse error; a stray key likewise.
        assert!(campaign_from_toml(&text).is_err());
        let stray = minimal().replace("name = \"t\"", "name = \"t\"\nnope = 1");
        assert!(
            campaign_from_toml(&stray)
                .unwrap_err()
                .to_string()
                .contains("nope")
        );
    }
}
