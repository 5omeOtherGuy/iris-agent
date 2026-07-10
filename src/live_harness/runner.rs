//! Live campaign execution: turn a planned run into rows against a real
//! provider, through the real Harness/CompactionSettings surfaces (no test
//! backdoors). This is the ONLY file that issues live traffic; it is reached
//! solely from the double-gated `live_campaign` entry point in `mod.rs`, never
//! from the gate. The row assembly is best-effort pilot fidelity: every number
//! comes from real `ProviderUsage` or observed lifecycle events, and the
//! per-turn context estimate is sampled from `context_diagnostics()` so
//! `estimate_error` is a genuine measured-vs-estimate diagnostic.

use super::*;
use crate::config::CompactionTriggerConfig;
use crate::session::{SessionLog, SessionStore};
use crate::tools::{ToolState, built_in_tools};
use crate::wayland::{Harness, SummarizerKind};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// The docs/benchmarks/data directory this repo writes campaign artifacts to.
fn artifacts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/benchmarks/data")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}

/// One run's rows plus its mechanical outcome.
struct RunResult {
    rows: Vec<Row>,
    outcome: TaskOutcome,
    probe_score: Option<f64>,
}

/// Map a summarizer label to the real `SummarizerKind` surface.
fn summarizer_kind(label: &str) -> SummarizerKind {
    match label {
        "provider" => SummarizerKind::Provider,
        "excerpts" => SummarizerKind::Excerpts,
        _ => SummarizerKind::Subagent,
    }
}

/// Execute one planned run live and assemble its rows. Errors are surfaced to
/// the caller, which records them as an excluded run rather than fabricating
/// numbers.
fn execute_run(spec: &CampaignSpec, planned: &PlannedRun) -> Result<RunResult> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let scenario = pilot_scenario(&planned.scenario_id)
        .ok_or_else(|| anyhow::anyhow!("unknown scenario {}", planned.scenario_id))?;
    let posture = scenario.posture();
    let budget = scenario.budget();

    // Seed and resume exactly as startup does, so the loaded prefix is
    // compactable through the production seam.
    let root = TempDir::new(&format!("campaign-{}-{}", planned.cell_id, planned.run_seq));
    let mut log = SessionLog::create_in(&root.path, &workspace)?;
    for message in scenario.seed(&workspace)? {
        log.append(&message)?;
    }
    let path = log.path().to_path_buf();
    drop(log);
    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()?
        .into_iter()
        .find(|meta| meta.path == path)
        .ok_or_else(|| anyhow::anyhow!("seeded session was not listed"))?;
    let stored = store.open(&meta)?;
    let entry_ids = stored.entry_ids.clone();
    let log = SessionLog::resume(&path)?;

    let cache_key = format!("iris-campaign-{}-{}", planned.cell_id, planned.run_seq);
    let recording = RecordingProvider::new(planned.lane.build_provider(&cache_key)?);
    let usages = recording.log();
    let agent = Agent::resumed(
        recording,
        built_in_tools().into_read_only(),
        stored.messages,
    );
    let mut harness = Harness::resumed(
        agent,
        workspace.clone(),
        ToolState::new(),
        Some(log),
        entry_ids,
        Some(budget),
    );

    // Settings through the real surfaces (no backdoors).
    let trigger: CompactionTriggerConfig = planned.settings.trigger_config(posture, budget);
    harness.set_compaction_trigger(budget, trigger);
    harness.set_summarizer(summarizer_kind(&planned.settings.summarizer));
    let worker_lane = planned.lane;
    harness.set_compaction_summarizer_factory(Arc::new(move || {
        worker_lane.build_provider("iris-campaign-compaction-worker")
    }));
    harness.set_microcompaction(posture.folds);

    let observer = LiveLoopObserver::default();
    let gate = ReadOnlyGate;
    let token = CancellationToken::new();
    let mut turn_estimates: Vec<(Instant, u64)> = Vec::new();
    for prompt in scenario.turns() {
        block_on(harness.submit_turn(&prompt, &observer, &gate, &token))?;
        let estimate = harness
            .context_diagnostics()
            .map(|diagnostics| diagnostics.measured)
            .unwrap_or(0);
        turn_estimates.push((Instant::now(), estimate));
    }

    let captured = usages.lock().expect("usages lock").clone();
    let events = observer.events.lock().expect("events lock");
    let fingerprint = planned.settings.fingerprint(posture);
    let rows = assemble_rows(
        spec,
        planned,
        &captured,
        &events,
        &fingerprint,
        &turn_estimates,
    );
    drop(events);

    // Pilot outcome: the run completed every turn without error. Probe scoring
    // is scenario-specific (R2 supplies a bank); the synthetic pilot scenarios
    // carry no probe bank, so the score is absent rather than fabricated.
    Ok(RunResult {
        rows,
        outcome: TaskOutcome::Pass,
        probe_score: None,
    })
}

/// Assemble one row per captured provider request. Boundary/tier/lifecycle
/// context comes from the observed event timeline; token classes come from the
/// realized `ProviderUsage`.
fn assemble_rows(
    spec: &CampaignSpec,
    planned: &PlannedRun,
    captured: &[CapturedUsage],
    events: &[TimedEvent],
    fingerprint: &SettingsFingerprint,
    turn_estimates: &[(Instant, u64)],
) -> Vec<Row> {
    let applies: Vec<(Instant, u64, String)> = events
        .iter()
        .filter_map(|timed| match &timed.event {
            AgentEvent::CompactionApplied {
                generation, origin, ..
            } => Some((timed.at, *generation, origin.as_str().to_string())),
            _ => None,
        })
        .collect();
    let folds: Vec<(Instant, usize, u64)> = events
        .iter()
        .filter_map(|timed| match &timed.event {
            AgentEvent::FoldApplied {
                folds,
                reclaimed_tokens_estimate,
                ..
            } => Some((timed.at, *folds, *reclaimed_tokens_estimate)),
            _ => None,
        })
        .collect();
    let pressure: Vec<(Instant, Tier)> = events
        .iter()
        .filter_map(|timed| match &timed.event {
            AgentEvent::ContextPressure { tier, .. } => {
                Some((timed.at, Tier::from_pressure(*tier)))
            }
            _ => None,
        })
        .collect();

    let write_visible = planned.lane.supports_native_compaction();
    let mut rows = Vec::with_capacity(captured.len());
    let mut prev_at: Option<Instant> = None;
    for (index, sample) in captured.iter().enumerate() {
        let at = sample.started_at;
        let wall_ms = captured
            .get(index + 1)
            .map(|next| next.started_at.duration_since(at).as_secs_f64() * 1_000.0)
            .unwrap_or(0.0);
        let boundary_index = applies.iter().filter(|(a, _, _)| *a <= at).count() as u64;
        let tier = pressure
            .iter()
            .rev()
            .find(|(a, _)| *a <= at)
            .map(|(_, t)| *t)
            .unwrap_or(Tier::None);
        let window_start = prev_at.unwrap_or_else(|| at - std::time::Duration::from_nanos(1));
        let applied = applies
            .iter()
            .find(|(a, _, _)| *a > window_start && *a <= at);
        let fold_flushes: usize = folds
            .iter()
            .filter(|(a, _, _)| *a > window_start && *a <= at)
            .map(|(_, count, _)| count)
            .sum();
        let folds_reclaimed: u64 = folds
            .iter()
            .filter(|(a, _, _)| *a > window_start && *a <= at)
            .map(|(_, _, reclaimed)| reclaimed)
            .sum();
        let estimate = turn_estimates
            .iter()
            .find(|(end, _)| *end >= at)
            .map(|(_, estimate)| *estimate)
            .or_else(|| turn_estimates.last().map(|(_, estimate)| *estimate))
            .unwrap_or(0);

        let usage = sample.usage.as_ref();
        let input_tokens = usage.map(|u| u.input_tokens).unwrap_or(0);
        let (cache_write_5m, cache_write_1h) = if write_visible {
            let creation = usage.and_then(|u| u.cache_creation.as_ref());
            (
                Some(creation.map(|c| c.ephemeral_5m_input_tokens).unwrap_or(0)),
                Some(creation.map(|c| c.ephemeral_1h_input_tokens).unwrap_or(0)),
            )
        } else {
            (None, None)
        };
        rows.push(Row {
            campaign: spec.name.clone(),
            cell_id: planned.cell_id.clone(),
            lane: planned.lane.label(),
            scenario: planned.scenario_id.clone(),
            run_seq: planned.run_seq,
            request_seq: index as u32,
            kind: if sample.is_summary {
                RowKind::Summary
            } else {
                RowKind::Turn
            },
            ts: now_secs(),
            wall_ms,
            input_tokens,
            output_tokens: usage.map(|u| u.output_tokens).unwrap_or(0),
            cache_read: usage.map(|u| u.cache_read_input_tokens).unwrap_or(0),
            cache_write_5m,
            cache_write_1h,
            write_unreported: !write_visible,
            context_measured_tokens: input_tokens,
            context_estimate_tokens: estimate,
            estimate_error: Row::estimate_error_of(input_tokens, estimate),
            boundary_index,
            tier,
            lifecycle: LifecycleDelta {
                compaction_generation_applied: applied.map(|(_, generation, _)| *generation),
                origin: applied.map(|(_, _, origin)| origin.clone()),
                fold_flushes,
                folds_reclaimed_estimate: folds_reclaimed,
                breaker_tripped: false,
            },
            settings: fingerprint.clone(),
            error: usage.is_none().then(|| "no usage captured".to_string()),
        });
        prev_at = Some(at);
    }
    rows
}

/// Run a full campaign sequentially, resuming past any completed runs recorded
/// in the manifest, writing rows to JSONL as they land and a `.md` report at the
/// end. Rate-limit friendly: one run at a time, manifest persisted per run.
pub(crate) fn run_campaign(spec: &CampaignSpec) -> Result<()> {
    let dir = artifacts_dir();
    std::fs::create_dir_all(&dir)?;
    let date = date_utc(now_secs());
    let artifacts = Artifacts::new(&dir, &spec.name, &date);
    let plan = expand(spec);
    let mut manifest = Manifest::load(artifacts.manifest.clone())?;

    let mut derived: Vec<(String, DerivedRun)> = Vec::new();
    let mut outcomes: Vec<LiveSessionOutcome> = Vec::new();
    for run in &plan {
        if manifest.contains(&run.key) {
            println!("campaign {}: skip completed {}", spec.name, run.key);
            continue;
        }
        if !run.lane.available() {
            println!(
                "campaign {}: lane {} unavailable (no credentials); skipping {}",
                spec.name,
                run.lane.label(),
                run.key
            );
            continue;
        }
        match execute_run(spec, run) {
            Ok(result) => {
                append_rows(&artifacts.jsonl, &result.rows)?;
                let price = PRICE_TABLE.price_for(run.lane.lane);
                derived.push((
                    run.cell_id.clone(),
                    DerivedRun::from_rows(
                        run.run_seq,
                        &result.rows,
                        price,
                        result.outcome,
                        result.probe_score,
                    ),
                ));
                outcomes.push(LiveSessionOutcome::Pass);
                manifest.mark(&run.key)?;
            }
            Err(error) => {
                println!("campaign {}: run {} error: {error:#}", spec.name, run.key);
                outcomes.push(LiveSessionOutcome::ErrorExclusion);
            }
        }
    }

    let verdict = live_run_verdict(&outcomes);
    let report = format_report(spec, &derived, verdict);
    std::fs::write(&artifacts.markdown, report)?;
    println!(
        "campaign {} done: {} rows-cells, verdict {} (exclusions {}); artifacts {}",
        spec.name,
        derived.len(),
        if verdict.passed { "PASS" } else { "FAIL" },
        verdict.exclusions,
        artifacts.jsonl.display(),
    );
    Ok(())
}
