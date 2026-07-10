//! Campaign definitions, matrix expansion, resumable manifests, and artifact
//! writing. Everything here is PURE and deterministic (no provider, no live
//! traffic) so the matrix/resume/report logic is unit-tested in the gate. The
//! live execution that turns a planned run into rows lives in `runner.rs` and is
//! reached only from the double-gated entry point.

use super::*;
use crate::config::{
    CompactionTriggerConfig, DEFAULT_COMPACTION_HARD, DEFAULT_COMPACTION_KEEP_RECENT_TOKENS,
    DEFAULT_COMPACTION_START,
};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The compaction settings one cell runs under. A settings sweep (C2/C3) is a
/// list of these; the pilot uses one `defaults()` cell per scenario.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CellSettings {
    pub(crate) start: f64,
    pub(crate) hard: f64,
    pub(crate) keep_tail_tokens: u64,
    pub(crate) hard_wait_ms: u64,
    /// `excerpts` | `provider` | `subagent`.
    pub(crate) summarizer: String,
    /// Prompt-cache retention tier recorded in the fingerprint: `5m` | `1h`.
    pub(crate) retention_tier: String,
}

impl CellSettings {
    /// Compaction defaults (the pilot cell). Mirrors the config `DEFAULT_*`
    /// constants so a cell that changes nothing is provably the shipped posture.
    pub(crate) fn defaults() -> Self {
        Self {
            start: DEFAULT_COMPACTION_START,
            hard: DEFAULT_COMPACTION_HARD,
            keep_tail_tokens: DEFAULT_COMPACTION_KEEP_RECENT_TOKENS,
            hard_wait_ms: 10_000,
            summarizer: "subagent".to_string(),
            retention_tier: "5m".to_string(),
        }
    }

    /// A short, stable fingerprint used inside a cell id.
    pub(crate) fn short(&self) -> String {
        format!(
            "s{}-h{}-k{}-w{}-{}-{}",
            (self.start * 100.0).round() as u64,
            (self.hard * 100.0).round() as u64,
            self.keep_tail_tokens,
            self.hard_wait_ms,
            self.summarizer,
            self.retention_tier,
        )
    }

    /// The schema fingerprint stamped on every row, folding in the scenario's
    /// posture (folds on/off).
    pub(crate) fn fingerprint(&self, posture: ScenarioPosture) -> SettingsFingerprint {
        SettingsFingerprint {
            start_pct: self.start,
            hard_pct: self.hard,
            keep_tail_tokens: self.keep_tail_tokens,
            hard_wait_ms: self.hard_wait_ms,
            summarizer: self.summarizer.clone(),
            folds: posture.folds,
            retention_tier: self.retention_tier.clone(),
        }
    }

    /// The runtime trigger config for the real Harness surface. `enabled`
    /// follows the scenario posture (S3 runs with auto-compaction off).
    pub(crate) fn trigger_config(
        &self,
        posture: ScenarioPosture,
        budget: u64,
    ) -> CompactionTriggerConfig {
        let _ = budget;
        CompactionTriggerConfig {
            enabled: posture.auto_compaction,
            warn: (self.start - 0.10).max(0.05),
            start: self.start,
            hard: self.hard,
            keep_recent_tokens: self.keep_tail_tokens,
            hard_wait_ms: self.hard_wait_ms,
            max_consecutive_failures: 3,
            reactive: true,
        }
    }
}

/// One (scenario, settings) pair; combined with each lane and each run index to
/// form the matrix.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CellSpec {
    pub(crate) scenario_id: String,
    pub(crate) settings: CellSettings,
}

/// A full campaign: which lanes, which cells, and how many runs per cell.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CampaignSpec {
    pub(crate) name: String,
    pub(crate) lanes: Vec<LaneSpec>,
    pub(crate) cells: Vec<CellSpec>,
    pub(crate) runs: u32,
}

/// One fully-resolved unit of work: a lane, a scenario+settings cell, and a run
/// index. `cell_id` groups the `runs` repetitions; `key` is the manifest token.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlannedRun {
    pub(crate) lane: LaneSpec,
    pub(crate) scenario_id: String,
    pub(crate) settings: CellSettings,
    pub(crate) run_seq: u32,
    pub(crate) cell_id: String,
    pub(crate) key: String,
}

/// Stable cell id: scenario, lane, and settings fingerprint. Two campaigns that
/// share a cell produce the same id so a comparator can diff them.
pub(crate) fn cell_id(lane: &LaneSpec, scenario_id: &str, settings: &CellSettings) -> String {
    format!("{scenario_id}::{}::{}", lane.label(), settings.short())
}

/// The manifest key for one run of a cell.
fn run_key(cell_id: &str, run_seq: u32) -> String {
    format!("{cell_id}#run{run_seq}")
}

/// Expand a campaign into its ordered list of planned runs: lane x cell x run,
/// executed sequentially (rate-limit friendly).
pub(crate) fn expand(spec: &CampaignSpec) -> Vec<PlannedRun> {
    let mut runs = Vec::new();
    for lane in &spec.lanes {
        for cell in &spec.cells {
            let id = cell_id(lane, &cell.scenario_id, &cell.settings);
            for run_seq in 0..spec.runs {
                runs.push(PlannedRun {
                    lane: *lane,
                    scenario_id: cell.scenario_id.clone(),
                    settings: cell.settings.clone(),
                    run_seq,
                    cell_id: id.clone(),
                    key: run_key(&id, run_seq),
                });
            }
        }
    }
    runs
}

/// A resumable campaign manifest: the set of completed run keys, persisted one
/// per line so an interrupted campaign continues instead of restarting.
pub(crate) struct Manifest {
    path: PathBuf,
    completed: BTreeSet<String>,
}

impl Manifest {
    /// Load an existing manifest, or start an empty one at `path`.
    pub(crate) fn load(path: PathBuf) -> Result<Self> {
        let completed = match std::fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(err) => return Err(err.into()),
        };
        Ok(Self { path, completed })
    }

    /// True when this run key is already recorded complete.
    pub(crate) fn contains(&self, key: &str) -> bool {
        self.completed.contains(key)
    }

    /// Record a run key complete and persist immediately, so a crash after this
    /// call still resumes past the finished run.
    pub(crate) fn mark(&mut self, key: &str) -> Result<()> {
        if self.completed.insert(key.to_string()) {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            writeln!(file, "{key}")?;
        }
        Ok(())
    }

    /// The pending runs of a plan: those not already recorded complete.
    pub(crate) fn pending<'a>(&self, plan: &'a [PlannedRun]) -> Vec<&'a PlannedRun> {
        plan.iter().filter(|run| !self.contains(&run.key)).collect()
    }
}

/// Convert Unix seconds to a UTC `YYYY-MM-DD` date string. No date crate is a
/// dependency, so this uses Howard Hinnant's `civil_from_days` algorithm
/// (public-domain, exact for the proleptic Gregorian calendar) rather than a
/// hand-rolled month table. Deterministic and unit-tested.
pub(crate) fn date_utc(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    // Shift so the era starts 0000-03-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}")
}

/// The artifact paths for a campaign on a given date (`YYYY-MM-DD`).
pub(crate) struct Artifacts {
    pub(crate) jsonl: PathBuf,
    pub(crate) markdown: PathBuf,
    pub(crate) manifest: PathBuf,
}

impl Artifacts {
    pub(crate) fn new(dir: &Path, campaign: &str, date: &str) -> Self {
        let stem = format!("{campaign}-{date}");
        Self {
            jsonl: dir.join(format!("{stem}.jsonl")),
            markdown: dir.join(format!("{stem}.md")),
            manifest: dir.join(format!("{stem}.manifest")),
        }
    }
}

/// Append rows to the campaign JSONL, one compact object per line.
pub(crate) fn append_rows(path: &Path, rows: &[Row]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for row in rows {
        writeln!(file, "{}", serde_json::to_string(row)?)?;
    }
    Ok(())
}

/// Render the campaign `.md` report: verdict, per-cell headline numbers, and the
/// row-schema reference. Kept small; a comparator script does deeper diffs.
pub(crate) fn format_report(
    spec: &CampaignSpec,
    derived: &[(String, DerivedRun)],
    verdict: LiveRunVerdict,
    failures: &[(String, String)],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Campaign {} report\n\n", spec.name));
    out.push_str(&format!(
        "Verdict: {} (exclusions {} / budget {}). Notional prices: {} (as of {}).\n\n",
        if verdict.passed { "PASS" } else { "FAIL" },
        verdict.exclusions,
        LIVE_EXCLUSION_BUDGET,
        "see metrics.rs price table",
        PRICE_TABLE.as_of,
    ));
    out.push_str(
        "| cell | run | requests | input | output | cache_read | notional_usd | outcome |\n",
    );
    out.push_str("| --- | --- | --- | --- | --- | --- | --- | --- |\n");
    for (cell, run) in derived {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {:.4} | {:?} |\n",
            cell,
            run.run_seq,
            run.requests,
            run.input_total,
            run.output_total,
            run.cache_read_total,
            run.notional_usd,
            run.outcome,
        ));
    }
    if !failures.is_empty() {
        out.push_str("\n## Scenario failures\n\n");
        out.push_str(
            "A run that completed without a provider error but did not exercise \
             its target behavior is a hard failure, recorded verbatim here.\n\n",
        );
        for (run_key, reason) in failures {
            out.push_str(&format!("- `{run_key}`: {reason}\n"));
        }
    }
    out.push_str(
        "\nRow schema: one row per provider request; see the design doc \
         (docs/... compaction-live-harness) and `metrics.rs::Row`.\n",
    );
    out
}

/// The pilot-A campaign: anthropic-only, low effort, n=2, cells S1 + S3 +
/// S4-small at compaction defaults. Validates plumbing/schema/artifacts before
/// any real spend widens.
pub(crate) fn pilot_a() -> CampaignSpec {
    let defaults = CellSettings::defaults();
    CampaignSpec {
        name: "pilot-a".to_string(),
        lanes: vec![anthropic_sonnet(LaneEffort::Low)],
        cells: vec![
            CellSpec {
                scenario_id: "S1".to_string(),
                settings: defaults.clone(),
            },
            CellSpec {
                scenario_id: "S3".to_string(),
                settings: defaults.clone(),
            },
            CellSpec {
                scenario_id: "S4-small".to_string(),
                settings: defaults,
            },
        ],
        runs: 2,
    }
}

/// Resolve a campaign by its `IRIS_BENCH_CAMPAIGN` selector.
pub(crate) fn campaign_by_name(name: &str) -> Option<CampaignSpec> {
    match name {
        "pilot-a" => Some(pilot_a()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_expands_lane_by_cell_by_run_in_order() {
        let spec = pilot_a();
        let plan = expand(&spec);
        // 1 lane x 3 cells x 2 runs.
        assert_eq!(plan.len(), 6);
        // Runs of a cell are contiguous and ordered 0,1.
        assert_eq!(plan[0].scenario_id, "S1");
        assert_eq!((plan[0].run_seq, plan[1].run_seq), (0, 1));
        assert_eq!(plan[2].scenario_id, "S3");
        assert_eq!(plan[4].scenario_id, "S4-small");
        // Keys are unique.
        let keys: BTreeSet<&String> = plan.iter().map(|r| &r.key).collect();
        assert_eq!(keys.len(), plan.len());
    }

    #[test]
    fn manifest_resume_skips_completed_runs() {
        let dir = TempDir::new("manifest");
        let path = dir.path.join("c.manifest");
        let spec = pilot_a();
        let plan = expand(&spec);

        let mut manifest = Manifest::load(path.clone()).expect("load empty");
        assert_eq!(manifest.pending(&plan).len(), plan.len());

        // Complete the first three runs, then simulate a restart.
        for run in plan.iter().take(3) {
            manifest.mark(&run.key).expect("mark");
        }
        let resumed = Manifest::load(path).expect("reload");
        let pending = resumed.pending(&plan);
        assert_eq!(pending.len(), plan.len() - 3);
        assert_eq!(pending[0].key, plan[3].key);
        // Marking is idempotent across the restart.
        assert!(resumed.contains(&plan[0].key));
    }

    #[test]
    fn date_utc_matches_known_epochs() {
        assert_eq!(date_utc(0), "1970-01-01");
        // 2026-07-10T00:00:00Z == 1_783_641_600.
        assert_eq!(date_utc(1_783_641_600), "2026-07-10");
        // Leap day boundary: 2024-02-29.
        assert_eq!(date_utc(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn cell_id_is_stable_for_the_same_inputs() {
        let lane = anthropic_sonnet(LaneEffort::Low);
        let a = cell_id(&lane, "S1", &CellSettings::defaults());
        let b = cell_id(&lane, "S1", &CellSettings::defaults());
        assert_eq!(a, b);
        assert!(a.contains("S1"));
        assert!(a.contains("anthropic/claude-sonnet-4-6@low"));
    }

    #[test]
    fn s3_posture_disables_auto_compaction_in_trigger_config() {
        let posture = ScenarioPosture {
            auto_compaction: false,
            folds: true,
        };
        let config = CellSettings::defaults().trigger_config(posture, 32_768);
        assert!(!config.enabled, "S3 cell must run with auto-compaction off");
        assert!(config.reactive);
    }

    #[test]
    fn report_includes_verdict_and_rows() {
        let spec = pilot_a();
        let derived = vec![(
            "S1::anthropic/claude-sonnet-4-6@low::s72-h90-k8000-w10000-subagent-5m".to_string(),
            DerivedRun {
                run_seq: 0,
                requests: 3,
                input_total: 10_000,
                output_total: 200,
                cache_read_total: 7_000,
                cache_write_total: Some(2_000),
                notional_usd: 0.05,
                cache_hit_ratio: 0.7,
                post_apply_rewrite_mass: 2_000,
                wall_ms_total: 1_000.0,
                estimate_error_mean: 100.0,
                estimate_error_max_abs: 250,
                outcome: TaskOutcome::Pass,
                probe_score: Some(1.0),
            },
        )];
        let report = format_report(
            &spec,
            &derived,
            live_run_verdict(&[LiveSessionOutcome::Pass]),
            &[],
        );
        assert!(report.contains("Verdict: PASS"));
        assert!(report.contains("S1::anthropic"));
        assert!(report.contains("Row schema"));
        assert!(!report.contains("Scenario failures"));
    }

    #[test]
    fn report_surfaces_scenario_failures_verbatim() {
        let spec = pilot_a();
        let failures = vec![(
            "S1::anthropic/claude-sonnet-4-6@low::s72-h90-k8000-w10000-subagent-5m#run0"
                .to_string(),
            "S1 produced no compaction".to_string(),
        )];
        let report = format_report(
            &spec,
            &[],
            live_run_verdict(&[LiveSessionOutcome::HardFailure]),
            &failures,
        );
        assert!(report.contains("Verdict: FAIL"));
        assert!(report.contains("## Scenario failures"));
        assert!(report.contains("S1 produced no compaction"));
    }

    #[test]
    fn append_rows_writes_one_json_object_per_line() {
        let dir = TempDir::new("jsonl");
        let path = dir.path.join("c.jsonl");
        let row = super::metrics::sample_row_for_tests(1, RowKind::Turn);
        append_rows(&path, std::slice::from_ref(&row)).expect("append");
        append_rows(&path, std::slice::from_ref(&row)).expect("append again");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text.lines().count(), 2);
        let parsed: Row = serde_json::from_str(text.lines().next().unwrap()).expect("parse");
        assert_eq!(parsed, row);
    }
}
