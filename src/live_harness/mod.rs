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
//! - `lanes`    -- lane descriptors + provider construction + availability.
//! - `metrics`  -- row schema, derived aggregates, notional price table.
//! - `scenario` -- Scenario trait + the four synthetic generators (S1-S4).
//! - `probes`   -- recall-probe bank + mechanical scoring (goal 7).
//! - `campaign` -- specs, matrix expansion, manifest, artifact writing (pure).
//! - `runner`   -- live execution turning a planned run into rows (live only).

use super::*;

mod campaign;
mod economics;
mod lanes;
mod metrics;
mod probes;
mod runner;
mod scenario;
mod support;
mod verdict;

// Hub re-exports so every submodule reaches the promoted helpers and sibling
// types through a single `use super::*`. `probes` is intentionally not
// re-exported: its bank is consumed only by its own tests until R2 lands.
pub(crate) use self::campaign::*;
pub(crate) use self::economics::*;
pub(crate) use self::lanes::*;
pub(crate) use self::metrics::*;
pub(crate) use self::runner::*;
pub(crate) use self::scenario::*;
pub(crate) use self::support::*;
pub(crate) use self::verdict::*;

/// The single live campaign entry point. Selected by `IRIS_BENCH_CAMPAIGN`
/// (e.g. `pilot-a`) and double-gated: `#[ignore]` keeps it out of `cargo test`,
/// and the `IRIS_BENCH_LIVE=1` guard makes even `cargo test -- --ignored` a
/// clean no-op unless the operator opts in. It never fabricates numbers: an
/// unavailable lane or a run error is recorded and skipped, not invented.
#[test]
#[ignore = "live provider campaign; set IRIS_BENCH_LIVE=1 and IRIS_BENCH_CAMPAIGN=<name> to run"]
fn live_campaign() {
    if std::env::var("IRIS_BENCH_LIVE").ok().as_deref() != Some("1") {
        eprintln!("live_campaign: skipped (set IRIS_BENCH_LIVE=1 to run)");
        return;
    }
    let name = match std::env::var("IRIS_BENCH_CAMPAIGN") {
        Ok(name) => name,
        Err(_) => {
            eprintln!("live_campaign: set IRIS_BENCH_CAMPAIGN=<name> (e.g. pilot-a) to select one");
            return;
        }
    };
    let Some(spec) = campaign_by_name(&name) else {
        eprintln!("live_campaign: unknown campaign {name:?}; known: pilot-a");
        return;
    };
    if let Err(error) = run_campaign(&spec) {
        eprintln!("live_campaign {name} failed: {error:#}");
    }
}
