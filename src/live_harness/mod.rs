//! Compaction live-measurement campaign harness (design:
//! `docs/... compaction-live-harness`). Generalizes the one-test-per-experiment
//! live bench (`compaction_live_bench.rs`) into a lane x scenario x settings x n
//! matrix runner with a uniform JSONL row schema, resumable campaign manifests,
//! and md+jsonl artifacts under `docs/benchmarks/data/`.
//!
//! Double-gated exactly like the legacy live bench: every live entry point is
//! `#[ignore]` AND guarded by `IRIS_BENCH_LIVE=1`, so `cargo test --locked` (the
//! gate) and CI never issue a provider call. The whole tree is `#[cfg(test)]`.
//!
//! Layout:
//! - `support`  -- recording provider, temp dir, observers, gates (promoted).
//! - `verdict`  -- flaky-exclusion verdict machinery (promoted, issue #545).
//! - `economics`-- cache-economics pairing across applies (promoted).
//! - `lanes`    -- lane descriptors + generic provider construction + parsing.
//! - `metrics`  -- row schema, derived aggregates, model-keyed price book.
//! - `scenario` -- Scenario trait, the synthetic S-series generators, id registry.
//! - `tool_scenarios` -- the T-series tool-efficiency scenarios (reuse
//!   `bench_tokens` fixtures/probes through the same Row schema).
//! - `config`   -- config-file campaign schema, parse, and validation (goal 1).
//! - `probes`   -- recall-probe bank + mechanical scoring (goal 7).
//! - `campaign` -- specs, matrix expansion, manifest, artifact writing (pure).
//! - `runner`   -- live execution turning a planned run into rows (live only).

use super::*;

mod campaign;
mod config;
mod economics;
mod lanes;
mod metrics;
mod probes;
mod runner;
mod scenario;
mod support;
mod tool_scenarios;
mod verdict;

// Hub re-exports so every submodule reaches the promoted helpers and sibling
// types through a single `use super::*`. `probes` is intentionally not
// re-exported: its bank is consumed only by its own tests until R2 lands.
pub(crate) use self::campaign::*;
pub(crate) use self::config::*;
pub(crate) use self::economics::*;
pub(crate) use self::lanes::*;
pub(crate) use self::metrics::*;
pub(crate) use self::runner::*;
pub(crate) use self::scenario::*;
pub(crate) use self::support::*;
pub(crate) use self::tool_scenarios::*;
pub(crate) use self::verdict::*;

/// The single live campaign entry point. Double-gated: `#[ignore]` keeps it out
/// of `cargo test`, and the `IRIS_BENCH_LIVE=1` guard makes even
/// `cargo test -- --ignored` a clean no-op unless the operator opts in.
///
/// A campaign is selected by exactly ONE of two mutually exclusive env vars:
/// - `IRIS_BENCH_CAMPAIGN=<name>` -- a built-in campaign (e.g. `pilot-a`).
/// - `IRIS_BENCH_CAMPAIGN_FILE=<path>` -- a config-file campaign (goal 1), so
///   any operator can run any model without editing Rust.
///
/// Setting both, or neither, is a clear error rather than a silent default. It
/// never fabricates numbers: an unavailable lane or a run error is recorded and
/// skipped, not invented.
#[test]
#[ignore = "live provider campaign; set IRIS_BENCH_LIVE=1 and IRIS_BENCH_CAMPAIGN(_FILE) to run"]
fn live_campaign() {
    if std::env::var("IRIS_BENCH_LIVE").ok().as_deref() != Some("1") {
        eprintln!("live_campaign: skipped (set IRIS_BENCH_LIVE=1 to run)");
        return;
    }
    // Install the global tracing subscriber (idempotent `try_init`) so the
    // provider-internal `iris::usage_raw` diagnostics surface when the operator
    // sets `RUST_LOG=iris::usage_raw=debug`. No-op without the env directive.
    // See the Diagnostics section of docs/benchmarks/HARNESS.md.
    crate::telemetry::init();
    let spec = match select_campaign() {
        Ok(spec) => spec,
        Err(error) => {
            eprintln!("live_campaign: {error:#}");
            return;
        }
    };
    if let Err(error) = run_campaign(&spec) {
        eprintln!("live_campaign {} failed: {error:#}", spec.name);
    }
}

/// Resolve the selected campaign from the two mutually exclusive selectors.
/// Enforcing exactly-one here (not in the `#[test]` body) keeps the rule
/// unit-testable without any live traffic.
fn select_campaign() -> Result<CampaignSpec> {
    let by_name = std::env::var("IRIS_BENCH_CAMPAIGN").ok();
    let by_file = std::env::var("IRIS_BENCH_CAMPAIGN_FILE").ok();
    match (by_name.as_deref(), by_file.as_deref()) {
        (Some(_), Some(_)) => Err(anyhow::anyhow!(
            "set exactly one of IRIS_BENCH_CAMPAIGN or IRIS_BENCH_CAMPAIGN_FILE, not both"
        )),
        (None, None) => Err(anyhow::anyhow!(
            "set exactly one of IRIS_BENCH_CAMPAIGN=<name> (e.g. pilot-a) or \
             IRIS_BENCH_CAMPAIGN_FILE=<path> to select a campaign"
        )),
        (Some(name), None) => campaign_by_name(name)
            .ok_or_else(|| anyhow::anyhow!("unknown campaign {name:?}; known built-in: pilot-a")),
        (None, Some(path)) => load_campaign_file(std::path::Path::new(path)),
    }
}
