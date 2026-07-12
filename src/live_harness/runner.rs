//! Live campaign execution: turn a planned run into rows against a real
//! provider, through the real Harness/CompactionSettings surfaces (no test
//! backdoors). This is the ONLY file that issues live traffic; it is reached
//! solely from the double-gated `live_campaign` entry point in `mod.rs`, never
//! from the gate. The row assembly is best-effort pilot fidelity: every number
//! comes from real `ProviderUsage` or observed lifecycle events, and the
//! context estimate is the pure estimator's count of each request's exact
//! payload (captured per request in `support::RecordingProvider`), so
//! `estimate_error` is a genuine per-request measured-vs-estimate delta rather
//! than a turn-end measurement broadcast across the turn's requests.

use super::*;
use crate::config::CompactionTriggerConfig;
use crate::session::{SessionLog, SessionStore};
use crate::tools::{ToolState, built_in_tools};
use crate::wayland::{Harness, SummarizerKind};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// The base directory campaign artifact folders live under. Each run writes into
/// `docs/benchmarks/campaigns/<name>/<date>/` beneath this (created on demand).
fn artifacts_base_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/benchmarks/campaigns")
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
    /// The assistant's final text per request, written to the transcript
    /// sidecar so an early-stop turn is diagnosable after the run.
    transcripts: Vec<Transcript>,
    outcome: TaskOutcome,
    probe_score: Option<f64>,
    /// The scenario's own failure reason when its post-run success criteria did
    /// not hold (e.g. "S1 produced no compaction"). `None` on a clean run. This
    /// is surfaced in the verdict and report so a scenario that silently
    /// under-drives its target behavior is a Fail, not a green pass.
    fail_reason: Option<String>,
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
    let scenario = build_scenario(&planned.scenario_id, &planned.knobs)
        .ok_or_else(|| anyhow::anyhow!("unknown scenario {}", planned.scenario_id))?;
    let posture = scenario.posture();
    let budget = scenario.budget();

    // The tool workspace: the S-series read the live repo; the T-series drive
    // against a fresh materialized fixture tree. `_fixture_ws` (a temp dir
    // removed on drop) is held for the whole run so the fixtures survive every
    // turn, then cleaned up when execute_run returns.
    let (workspace, _fixture_ws) = match scenario.workspace_kind() {
        WorkspaceKind::Repo => (PathBuf::from(env!("CARGO_MANIFEST_DIR")), None),
        WorkspaceKind::Fixtures => {
            let dir = TempDir::new(&format!("tool-ws-{}-{}", planned.cell_id, planned.run_seq));
            scenario.materialize(&dir.path)?;
            (dir.path.clone(), Some(dir))
        }
    };

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
        workspace,
        ToolState::new(),
        Some(log),
        entry_ids,
        Some(budget),
    );

    // Settings through the real surfaces (no backdoors).
    let trigger: CompactionTriggerConfig = planned.settings.trigger_config(posture, budget);
    harness.set_compaction_trigger(budget.into(), trigger);
    harness.set_summarizer(summarizer_kind(&planned.settings.summarizer));
    let worker_lane = planned.lane.clone();
    harness.set_compaction_summarizer_factory(Arc::new(move || {
        worker_lane.build_provider("iris-campaign-compaction-worker")
    }));
    harness.set_microcompaction(posture.folds);

    let observer = LiveLoopObserver::default();
    let gate = ReadOnlyGate;
    let token = CancellationToken::new();
    for prompt in scenario.turns() {
        block_on(harness.submit_turn(&prompt, &observer, &gate, &token))?;
    }

    let captured = usages.lock().expect("usages lock").clone();
    let events = observer.events.lock().expect("events lock");
    let fingerprint = planned.settings.fingerprint(posture);
    let rows = assemble_rows(spec, planned, &captured, &events, &fingerprint);
    let transcripts = assemble_transcripts(spec, planned, &captured);
    drop(events);

    // The run completed every turn without a provider error, but "completed" is
    // not "exercised its target behavior": a scenario declares post-run success
    // criteria (S1 must observe multiple boundaries and at least one compaction)
    // and fails the run when they do not hold, so an under-driving scenario
    // surfaces as a Fail instead of a silent green pass (pilot-a finding 1).
    let (outcome, fail_reason) = match scenario.verify_run(&rows) {
        Ok(()) => (TaskOutcome::Pass, None),
        Err(reason) => (TaskOutcome::Fail, Some(reason)),
    };

    // Probe scoring is scenario-specific (R2 supplies a bank); the synthetic
    // pilot scenarios carry no probe bank, so the score is absent rather than
    // fabricated.
    Ok(RunResult {
        rows,
        transcripts,
        outcome,
        probe_score: None,
        fail_reason,
    })
}

/// Build one transcript entry per captured request, carrying the assistant's
/// final text (already truncated to `TRANSCRIPT_TEXT_CAP` at capture). Keyed by
/// the same `cell_id` + `run_seq` + `request_seq` as the matching row, so a row
/// and its transcript join cleanly; the text is `None` on a pure tool-call
/// round-trip. This is the sidecar that makes a behavioral early-stop
/// diagnosable after the fact without changing the stable Row schema.
fn assemble_transcripts(
    spec: &CampaignSpec,
    planned: &PlannedRun,
    captured: &[CapturedUsage],
) -> Vec<Transcript> {
    captured
        .iter()
        .enumerate()
        .map(|(index, sample)| Transcript {
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
            text: sample.assistant_text.clone(),
            truncated: sample.assistant_text_truncated,
        })
        .collect()
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
        // Per-request estimator value sampled from the exact payload this
        // request sent (support::RecordingProvider), diffed against the same
        // request's provider-reported input below. A per-request like-for-like
        // delta -- not the turn-end measurement broadcast across every request
        // that produced the phantom ~-3.6k errors in pilot-a (finding 2).
        let estimate = sample.estimate_tokens;

        let usage = sample.usage.as_ref();
        let input_tokens = usage.map(|u| u.input_tokens).unwrap_or(0);
        // write_unreported is an HONEST per-row flag, not a per-lane constant
        // (goal 2). Since PR #557 the Codex adapter parses cache writes, so a
        // Codex row that reports a nonzero write is NOT blind. The Anthropic
        // lane reports the 5m/1h split via `cache_creation`; the Codex lane
        // reports a single flat write with no retention split, stored in the 5m
        // slot with 1h = 0. Residual ambiguity (documented in the schema): a
        // Codex row reporting a zero write cannot distinguish "wrote nothing"
        // from "the endpoint did not surface a write", so it is conservatively
        // flagged write_unreported = true.
        let (cache_write_5m, cache_write_1h, write_unreported) = match planned.lane.lane {
            ProviderLane::Anthropic => {
                let creation = usage.and_then(|u| u.cache_creation.as_ref());
                (
                    Some(creation.map(|c| c.ephemeral_5m_input_tokens).unwrap_or(0)),
                    Some(creation.map(|c| c.ephemeral_1h_input_tokens).unwrap_or(0)),
                    false,
                )
            }
            ProviderLane::Codex => {
                let reported_write = usage.map(|u| u.cache_write_input_tokens).unwrap_or(0);
                if reported_write > 0 {
                    (Some(reported_write), Some(0), false)
                } else {
                    (None, None, true)
                }
            }
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
            write_unreported,
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
    let date = date_utc(now_secs());
    // Per-campaign, per-date artifact folder (goal 3), created on demand.
    let dir = artifacts_base_dir().join(&spec.name).join(&date);
    std::fs::create_dir_all(&dir)?;
    let artifacts = Artifacts::new(&dir, &spec.name);
    let plan = expand(spec);
    let mut manifest = Manifest::load(artifacts.manifest.clone())?;

    let mut derived: Vec<(String, DerivedRun)> = Vec::new();
    let mut outcomes: Vec<LiveSessionOutcome> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();
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
                append_transcripts(&artifacts.transcripts, &result.transcripts)?;
                let price = spec.prices.price_for(&run.lane.model_id);
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
                // A scenario that ran clean but did not exercise its target
                // behavior is a hard failure, not a pass: it fails the verdict
                // and its reason is recorded in the report.
                match &result.fail_reason {
                    None => outcomes.push(LiveSessionOutcome::Pass),
                    Some(reason) => {
                        println!("campaign {}: run {} FAIL: {reason}", spec.name, run.key);
                        failures.push((run.key.clone(), reason.clone()));
                        outcomes.push(LiveSessionOutcome::HardFailure);
                    }
                }
                manifest.mark(&run.key)?;
            }
            Err(error) => {
                println!("campaign {}: run {} error: {error:#}", spec.name, run.key);
                outcomes.push(LiveSessionOutcome::ErrorExclusion);
            }
        }
    }

    let verdict = live_run_verdict_with_budget(&outcomes, spec.exclusion_budget);
    let report = format_report(spec, &derived, verdict, &failures);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::{AssistantTurn, ProviderEvent, ProviderUsage, ToolCall};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn spec_and_planned() -> (CampaignSpec, PlannedRun, SettingsFingerprint) {
        let spec = pilot_a();
        let planned = expand(&spec).into_iter().next().expect("a planned run");
        let fingerprint = planned.settings.fingerprint(ScenarioPosture {
            auto_compaction: true,
            folds: true,
        });
        (spec, planned, fingerprint)
    }

    fn usage(input: u64, output: u64) -> ProviderUsage {
        ProviderUsage {
            provider: "test".to_string(),
            model: "test".to_string(),
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: input + output,
            cache_creation: None,
        }
    }

    /// A provider that answers with one fixed assistant text turn (plus usage),
    /// so the recording wrapper's text capture/truncation is exercised without
    /// live traffic.
    struct FixedTextProvider {
        text: String,
    }

    impl ChatProvider for FixedTextProvider {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            let mut turn = AssistantTurn::text(&self.text);
            turn.usage = Some(usage(10, 5));
            Ok(Box::pin(futures::stream::once(async move {
                Ok(ProviderEvent::Completed(turn))
            })))
        }
    }

    /// Goal 1 (capture): the recording provider records the assistant's final
    /// text and truncates it at `TRANSCRIPT_TEXT_CAP`, setting the truncated
    /// flag. Deterministic -- a fake provider, no live traffic.
    #[test]
    fn recording_provider_captures_and_truncates_assistant_text_at_cap() {
        use futures::StreamExt;
        let long = "x".repeat(TRANSCRIPT_TEXT_CAP + 500);
        let provider = RecordingProvider::new(FixedTextProvider { text: long });
        let usages = provider.log();
        let tools = Tools::new(Vec::new());
        let token = CancellationToken::new();
        let messages = vec![Message::user("trigger the turn")];
        block_on(async {
            let mut stream = provider
                .respond_stream(&messages, &tools, &token)
                .expect("stream");
            while stream.next().await.is_some() {}
        });
        let captured = usages.lock().expect("usages").clone();
        assert_eq!(captured.len(), 1);
        let text = captured[0].assistant_text.as_ref().expect("text captured");
        assert_eq!(
            text.chars().count(),
            TRANSCRIPT_TEXT_CAP,
            "captured text truncated to the cap"
        );
        assert!(captured[0].assistant_text_truncated);
    }

    /// Goal 1 (assemble + write): the runner builds one transcript per request,
    /// keyed by cell_id + run_seq + request_seq to the matching row, with the
    /// final text preserved (truncation flag carried) and `None` on a pure
    /// tool-call round-trip. The sidecar is written one JSON object per line.
    /// Deterministic -- assemble/append only, no live traffic.
    #[test]
    fn assemble_transcripts_keys_by_request_and_writes_sidecar() {
        let (spec, planned, _fingerprint) = spec_and_planned();
        let at = Instant::now();
        let long_text: String = "y".repeat(TRANSCRIPT_TEXT_CAP);
        let captured = vec![
            // Pure tool-call round-trip: no assistant text.
            CapturedUsage {
                is_summary: false,
                tag: "read".to_string(),
                started_at: at,
                usage: Some(usage(2_100, 90)),
                estimate_tokens: 2_000,
                assistant_text: None,
                assistant_text_truncated: false,
            },
            // Turn-ending text (recorded as already truncated at capture).
            CapturedUsage {
                is_summary: false,
                tag: "reply".to_string(),
                started_at: at + Duration::from_millis(1),
                usage: Some(usage(6_100, 150)),
                estimate_tokens: 6_000,
                assistant_text: Some(long_text.clone()),
                assistant_text_truncated: true,
            },
        ];
        let transcripts = assemble_transcripts(&spec, &planned, &captured);
        assert_eq!(transcripts.len(), 2);
        // Keyed positionally to the rows: request_seq matches the index, and the
        // cell_id/run_seq/lane/scenario mirror the planned run.
        assert_eq!(transcripts[0].request_seq, 0);
        assert_eq!(transcripts[1].request_seq, 1);
        assert_eq!(transcripts[0].cell_id, planned.cell_id);
        assert_eq!(transcripts[0].run_seq, planned.run_seq);
        assert_eq!(transcripts[0].lane, planned.lane.label());
        // A pure tool-call round-trip records no assistant text.
        assert_eq!(transcripts[0].text, None);
        assert!(!transcripts[0].truncated);
        // The turn-ending text is preserved with its truncation flag.
        assert_eq!(transcripts[1].text.as_deref(), Some(long_text.as_str()));
        assert!(transcripts[1].truncated);

        // Written to the sidecar one JSON object per line, round-tripping.
        let dir = TempDir::new("transcripts-sidecar");
        let path = dir.path.join("c.transcripts.jsonl");
        append_transcripts(&path, &transcripts).expect("append");
        assert!(path.exists(), "sidecar file exists");
        let body = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "one object per line");
        let back: Transcript = serde_json::from_str(lines[1]).expect("deserialize");
        assert_eq!(back, transcripts[1]);
    }

    /// Finding 2: `context_estimate_tokens` is a per-request estimator value, so
    /// `estimate_error` is a genuine like-for-like delta. The pilot-a bug
    /// broadcast the turn-end measurement across every request, so the opening
    /// request of a multi-round-trip turn showed a phantom multi-thousand-token
    /// error against a context it never sent. Two requests in one turn with
    /// distinct payloads must keep distinct, correctly-signed errors.
    #[test]
    fn estimate_error_is_per_request_not_a_turn_end_broadcast() {
        let (spec, planned, fingerprint) = spec_and_planned();
        let at = Instant::now();
        let captured = vec![
            CapturedUsage {
                is_summary: false,
                tag: "open".to_string(),
                started_at: at,
                usage: Some(usage(2_100, 90)),
                estimate_tokens: 2_000,
                assistant_text: None,
                assistant_text_truncated: false,
            },
            CapturedUsage {
                is_summary: false,
                tag: "reply".to_string(),
                started_at: at + Duration::from_millis(1),
                usage: Some(usage(6_100, 150)),
                estimate_tokens: 6_000,
                assistant_text: None,
                assistant_text_truncated: false,
            },
        ];
        let rows = assemble_rows(&spec, &planned, &captured, &[], &fingerprint);

        // Each request keeps ITS OWN estimator value and provider input.
        assert_eq!(rows[0].context_estimate_tokens, 2_000);
        assert_eq!(rows[1].context_estimate_tokens, 6_000);
        assert_eq!(rows[0].context_measured_tokens, 2_100);
        assert_eq!(rows[1].context_measured_tokens, 6_100);
        // Like-for-like per request: measured - estimate, small and honest --
        // NOT the -3.6k phantom the turn-end broadcast produced.
        assert_eq!(rows[0].estimate_error, 100);
        assert_eq!(rows[1].estimate_error, 100);
    }

    /// Goal 2: `write_unreported` is a per-row honest flag. A Codex row that
    /// reports a nonzero cache write is NOT blind (write_unreported=false) and
    /// its write is preserved in the 5m slot; a Codex row reporting a zero write
    /// stays flagged because zero is ambiguous. The Anthropic lane is never
    /// write-blind. Deterministic (assemble_rows only, no live traffic).
    #[test]
    fn write_unreported_is_codex_lane_and_zero_reported_write_only() {
        let fingerprint = CellSettings::defaults().fingerprint(ScenarioPosture {
            auto_compaction: true,
            folds: true,
        });
        let spec = pilot_a();
        let codex = PlannedRun {
            lane: codex_luna(LaneEffort::Low),
            scenario_id: "S4-small".to_string(),
            settings: CellSettings::defaults(),
            knobs: ScenarioKnobs::default(),
            run_seq: 0,
            cell_id: "codex-cell".to_string(),
            key: "codex-cell#run0".to_string(),
        };
        let mut wrote = usage(5_000, 40);
        wrote.cache_write_input_tokens = 1_800;
        let at = Instant::now();
        let captured = vec![
            CapturedUsage {
                is_summary: false,
                tag: "zero-write".to_string(),
                started_at: at,
                usage: Some(usage(5_000, 40)),
                estimate_tokens: 5_000,
                assistant_text: None,
                assistant_text_truncated: false,
            },
            CapturedUsage {
                is_summary: false,
                tag: "reported-write".to_string(),
                started_at: at + Duration::from_millis(1),
                usage: Some(wrote),
                estimate_tokens: 5_000,
                assistant_text: None,
                assistant_text_truncated: false,
            },
        ];
        let rows = assemble_rows(&spec, &codex, &captured, &[], &fingerprint);
        // Zero reported write on the Codex lane: blind + ambiguous -> flagged.
        assert!(rows[0].write_unreported);
        assert_eq!(rows[0].cache_write_5m, None);
        // Nonzero reported write: NOT blind, write preserved in the 5m slot.
        assert!(!rows[1].write_unreported);
        assert_eq!(rows[1].cache_write_5m, Some(1_800));
        assert_eq!(rows[1].cache_write_1h, Some(0));
    }

    /// A fake provider that emits one `read` tool call per fixture across
    /// several mid-turn round-trips, then a final text turn. Its reported usage
    /// puts the anchored context above the hard tier, so the governor's #552
    /// current-turn coverage compacts MID-TURN at a continuing boundary --
    /// deterministic, no live traffic.
    struct ScriptedReadProvider {
        reads: Vec<String>,
        call: Arc<AtomicUsize>,
        anchor_tokens: u64,
    }

    impl ChatProvider for ScriptedReadProvider {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            let n = self.call.fetch_add(1, Ordering::SeqCst);
            let mut turn = if n < self.reads.len() {
                AssistantTurn {
                    tool_calls: vec![ToolCall {
                        id: format!("scripted-read-{n}"),
                        name: "read".to_string(),
                        arguments: serde_json::json!({ "path": self.reads[n] }),
                        thought_signature: None,
                    }],
                    ..AssistantTurn::default()
                }
            } else {
                AssistantTurn::text("Summaries: all files read.")
            };
            // Anchor the measured context above hard from the first completed
            // round-trip, so every continuing mid-turn boundary is hard.
            turn.usage = Some(usage(self.anchor_tokens, 40));
            Ok(Box::pin(futures::stream::once(async move {
                Ok(ProviderEvent::Completed(turn))
            })))
        }
    }

    /// Finding 1: S1 drives several tool-call round-trips inside ONE turn, so a
    /// mid-turn hard-tier boundary lets auto-compaction fire, and S1's own
    /// `verify_run` accepts the run. Exercised deterministically through the
    /// real Harness/governor with a scripted provider (no live traffic).
    #[test]
    fn s1_drives_multiple_boundaries_and_compacts_mid_turn() {
        let s1 = AggressiveFill::pilot();
        let workspace = TempDir::new("s1-flow-ws");
        // The scripted reads resolve against real fixture files carrying S1's
        // scripted bodies (several thousand tokens each).
        let mut reads = Vec::new();
        for i in 0..s1.round_trips {
            let name = format!("s1_fixture_{i}.txt");
            std::fs::write(workspace.path.join(&name), s1.scripted_read_body(i))
                .expect("write fixture");
            reads.push(name);
        }

        let root = TempDir::new("s1-flow-session");
        let mut log = SessionLog::create_in(&root.path, &workspace.path).expect("create log");
        for message in s1.seed_messages() {
            log.append(&message).expect("append seed");
        }
        let path = log.path().to_path_buf();
        drop(log);
        let store = SessionStore::with_root(root.path.clone());
        let meta = store
            .list()
            .expect("list")
            .into_iter()
            .find(|meta| meta.path == path)
            .expect("seeded session listed");
        let stored = store.open(&meta).expect("open");
        let entry_ids = stored.entry_ids.clone();
        let log = SessionLog::resume(&path).expect("resume");

        let provider = RecordingProvider::new(ScriptedReadProvider {
            reads,
            call: Arc::new(AtomicUsize::new(0)),
            // Comfortably above hard (0.90 x 32768 ~= 29.5k).
            anchor_tokens: 40_000,
        });
        let usages = provider.log();
        let agent = Agent::resumed(provider, built_in_tools().into_read_only(), stored.messages);
        let mut harness = Harness::resumed(
            agent,
            workspace.path.clone(),
            ToolState::new(),
            Some(log),
            entry_ids,
            Some(s1.budget()),
        );
        harness.set_compaction_trigger(
            s1.budget().into(),
            CompactionTriggerConfig {
                enabled: true,
                warn: 0.62,
                start: 0.72,
                hard: 0.90,
                keep_recent_tokens: 8_000,
                hard_wait_ms: 10,
                max_consecutive_failures: 3,
                reactive: true,
            },
        );

        let observer = LiveLoopObserver::default();
        let gate = ReadOnlyGate;
        let token = CancellationToken::new();
        let turns = s1.turns();
        block_on(harness.submit_turn(&turns[0], &observer, &gate, &token)).expect("turn");

        let events = observer.events.lock().expect("events");
        let applied = events
            .iter()
            .filter(|timed| matches!(timed.event, AgentEvent::CompactionApplied { .. }))
            .count();
        assert!(
            applied >= 1,
            "S1 must compact mid-turn; got {applied} applies"
        );
        drop(events);

        let captured = usages.lock().expect("usages").clone();
        assert!(
            captured.len() >= 3,
            "S1 must drive >= 3 round-trip boundaries; got {}",
            captured.len()
        );

        let (spec, planned, fingerprint) = spec_and_planned();
        let events = observer.events.lock().expect("events");
        let rows = assemble_rows(&spec, &planned, &captured, &events, &fingerprint);
        drop(events);
        assert!(
            s1.verify_run(&rows).is_ok(),
            "S1 verify_run must accept a run that compacted across >= 3 boundaries: {:?}",
            s1.verify_run(&rows)
        );
    }
}
