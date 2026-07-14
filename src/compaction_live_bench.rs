//! Env-gated LIVE validation of the modeled compaction cache economics
//! (ADR-0045, issue #372, slice B). The deterministic modeled metric in
//! `compaction_bench.rs` ("modeled (prefix-divergence, estimated tokens)") is
//! anchored here against realized `ProviderUsage` cache splits from the
//! Anthropic Messages provider on the Claude Code subscription OAuth lane -- the
//! only lane that reports both cache reads AND writes (plus the 5m/1h tier
//! split), so it can validate both halves of the model.
//!
//! This harness makes REAL API calls and is DOUBLE-gated so the committed suite
//! and `scripts/gate.sh` never trigger it. First, `#[ignore]` keeps `cargo test`
//! (the gate's `cargo test --locked`) from running it. Second, an
//! `IRIS_BENCH_LIVE=1` env guard makes even `cargo test -- --ignored` return
//! immediately unless the operator opts in. Credentials are discovered with zero
//! setup at `~/.claude/.credentials.json` via
//! `claude_code_credentials_available()`. On any auth/infra failure the harness
//! records the verbatim error and returns; it never fabricates numbers.
//!
//! Flow: seed a near-budget session JSONL -> resume on the Anthropic Claude Code
//! OAuth lane -> drive one compaction (a real summarization request) plus one
//! follow-up turn -> capture `ProviderUsage` from (a) the summarization request
//! (realized cache-HIT rate) and (b) the first post-compaction request (realized
//! cache-WRITE mass vs the pre-compaction baseline).

use super::live_harness::{
    ApplyReclamation, CacheMassModel, CapturedUsage, LIVE_EXCLUSION_BUDGET, LiveLoopObserver,
    LiveSessionGates, LiveSessionOutcome, NoToolGate, NoopObserver, ParentCacheEconomics,
    ReadOnlyGate, RecordingProvider, TempDir, TimedEvent, apply_reclamation, block_on,
    classify_live_gates, live_run_verdict, parent_cache_economics,
};
use super::*;
use crate::config::CompactionTriggerConfig;
use crate::mimir::auth::anthropic::claude_code_credentials_available;
use crate::mimir::providers::anthropic_messages::AnthropicProvider;
use crate::mimir::retry::RetryPolicy;
use crate::mimir::selection::{
    CodexTransport, ContextManagement, PromptCacheRetention, ReasoningEffort,
};
use crate::session::{SessionLog, SessionStore};
use crate::tools::{ToolState, built_in_tools};
use crate::wayland::{CompactionWorkerConfig, Harness, SummarizerKind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// A Claude Code subscription model id (see `anthropic_models.rs`).
const LIVE_MODEL: &str = "claude-sonnet-4-6";
/// Minimal system prompt; the provider prepends the required Claude Code
/// identity block itself.
const LIVE_SYSTEM_PROMPT: &str = "You are a coding assistant. Keep answers short.";
const AUTO_LIVE_WORKER_MODEL: &str = "claude-opus-4-6";
struct InducedOverflowProvider {
    inner: Box<dyn ChatProvider>,
    inject_next: AtomicBool,
    forwarded: Arc<AtomicUsize>,
}

impl ChatProvider for InducedOverflowProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a Tools,
        cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        if self.inject_next.swap(false, Ordering::SeqCst) {
            let turn = AssistantTurn {
                completion_reason: Some(CompletionReason::ContextWindowExceeded),
                ..AssistantTurn::default()
            };
            return Ok(Box::pin(futures::stream::once(async move {
                Ok(ProviderEvent::Completed(turn))
            })));
        }
        self.forwarded.fetch_add(1, Ordering::SeqCst);
        self.inner.respond_stream(messages, tools, cancel)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }
}

#[derive(Debug, Clone, Copy)]
enum LiveLoopLane {
    AnthropicHaiku,
    CodexMini,
}

impl LiveLoopLane {
    fn label(self) -> &'static str {
        match self {
            Self::AnthropicHaiku => "anthropic/claude-haiku-4-5",
            Self::CodexMini => "openai-codex/gpt-5.4-mini",
        }
    }

    fn build_provider(self, cache_key: &str) -> Result<Box<dyn ChatProvider>> {
        self.build_provider_with_system(cache_key, LIVE_SYSTEM_PROMPT)
    }

    fn build_summary_provider(self, cache_key: &str) -> Result<Box<dyn ChatProvider>> {
        self.build_provider_with_system(cache_key, crate::wayland::SUMMARY_SYSTEM_PROMPT)
    }

    fn build_provider_with_system(
        self,
        cache_key: &str,
        system_prompt: &str,
    ) -> Result<Box<dyn ChatProvider>> {
        match self {
            Self::AnthropicHaiku => Ok(Box::new(AnthropicProvider::new(
                "claude-haiku-4-5",
                "https://api.anthropic.com",
                None,
                system_prompt,
                PromptCacheRetention::DEFAULT,
                ContextManagement::default(),
                RetryPolicy::default(),
            )?)),
            Self::CodexMini => Ok(Box::new(
                crate::mimir::providers::openai_codex_responses::OpenAiCodexResponsesProvider::new(
                    "gpt-5.4-mini",
                    "https://chatgpt.com/backend-api",
                    None,
                    system_prompt,
                    cache_key,
                    PromptCacheRetention::DEFAULT,
                    RetryPolicy::default(),
                    CodexTransport::Auto,
                    Some(std::time::Duration::from_secs(300)),
                )?,
            )),
        }
    }

    fn cache_mass_model(self) -> CacheMassModel {
        match self {
            Self::AnthropicHaiku => CacheMassModel::ReportedWrite,
            Self::CodexMini => CacheMassModel::DerivedFreshInput,
        }
    }

    fn cache_metric(self) -> &'static str {
        self.cache_mass_model().label()
    }
}

fn build_live_compaction_worker() -> Result<Box<dyn ChatProvider>> {
    Ok(Box::new(AnthropicProvider::new(
        AUTO_LIVE_WORKER_MODEL,
        "https://api.anthropic.com",
        Some(ReasoningEffort::Medium),
        LIVE_SYSTEM_PROMPT,
        PromptCacheRetention::DEFAULT,
        ContextManagement::default(),
        RetryPolicy::default(),
    )?))
}

/// Near-budget synthetic history: a large needle-bearing opener plus several
/// follow-up turns, sized so a small budget forces compaction on the first turn.
fn live_seed() -> Vec<Message> {
    let opener = format!(
        "TASK-LIVE-8291 target crates/orbit/src/telemetry/sink.rs fn reconcile_ledger. {}",
        "Context on the ledger reconciliation work and its constraints, carried so the takeover \
         model can resume the wiring without re-reading the originals. "
            .repeat(30)
    );
    let mut seed = vec![
        Message::user(&opener),
        Message::assistant("Understood; starting."),
    ];
    for i in 0..6 {
        seed.push(Message::user(&format!(
            "Step {i}: continue the reconciliation wiring. {}",
            "Additional constraint detail for this step that fills the covered range. ".repeat(6)
        )));
        seed.push(Message::assistant("ok"));
    }
    seed
}

// Four summary reserves: the smallest synthetic window that keeps model-backed
// background work enabled. Parent traffic stays on the two cheap protocol lanes;
// every summary worker is Opus 4.6 with medium thinking by operator requirement.
const AUTO_LIVE_BUDGET: u64 = 32_768;
const AUTO_LIVE_CARRY_HEADER: &str = "[files touched or read in the compacted range]";

#[derive(Debug, Clone, Copy)]
struct AutoLivePolicy {
    warn: f64,
    start: f64,
    hard: f64,
    keep_recent_tokens: u64,
}

fn auto_live_policy() -> AutoLivePolicy {
    let float = |name: &str, default: f64| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .unwrap_or(default)
    };
    let integer = |name: &str, default: u64| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(default)
    };
    let policy = AutoLivePolicy {
        warn: float(
            "IRIS_AUTO_COMPACTION_WARN",
            crate::config::DEFAULT_COMPACTION_WARN,
        ),
        start: float(
            "IRIS_AUTO_COMPACTION_START",
            crate::config::DEFAULT_COMPACTION_START,
        ),
        hard: float(
            "IRIS_AUTO_COMPACTION_HARD",
            crate::config::DEFAULT_COMPACTION_HARD,
        ),
        keep_recent_tokens: integer(
            "IRIS_AUTO_COMPACTION_KEEP_TOKENS",
            crate::config::DEFAULT_COMPACTION_KEEP_RECENT_TOKENS,
        ),
    };
    assert!(
        0.0 < policy.warn
            && policy.warn < policy.start
            && policy.start < policy.hard
            && policy.hard < 1.0,
        "live policy must satisfy 0 < warn < start < hard < 1"
    );
    policy
}

/// Password-like credential needle (audit F17 / ADR-0061), session-scoped
/// like `NEEDLE-{session}7f3a9` above so sessions stay isolated. Planted as an
/// explicitly user-sourced ask -- "remember the deploy password is X -- I'll
/// need it after compaction" -- rather than as ordinary project context, so
/// this needle actually exercises the retention class the audit found
/// invisible: every other needle in this bench is innocuous-shaped, so a
/// summarizer whose injection-defense framing scrubs credential-shaped facts
/// would pass every existing gate while silently dropping this one.
fn live_credential(session: usize) -> String {
    format!("korium-9741-{session:02x}")
}

fn auto_live_seed(session: usize, workspace: &std::path::Path) -> Result<Vec<Message>> {
    let needle = format!("NEEDLE-{session:02x}7f3a9: the flag is --enable-zeta");
    let credential = live_credential(session);
    let cargo = std::fs::read_to_string(workspace.join("Cargo.toml"))?;
    let call_id = format!("auto-live-seed-read-{session}");
    let mut seed = vec![
        Message::user(&format!(
            "Remember this exact project fact for later: {needle}. Also remember the deploy \
             password is {credential} -- I'll need it after compaction. We are auditing \
             auto-compaction."
        )),
        Message::assistant_tool_call(&ToolCall {
            id: call_id.clone(),
            name: "read".to_string(),
            arguments: serde_json::json!({ "path": "Cargo.toml" }),
            thought_signature: None,
        }),
        Message::tool_result(
            &call_id,
            "read",
            &serde_json::json!({
                "ok": true,
                "content": cargo,
                "metadata": { "target": "Cargo.toml" },
            })
            .to_string(),
        ),
        Message::assistant("The needle and manifest are recorded."),
    ];
    for turn in 0..12 {
        seed.push(Message::user(&format!(
            "Seed filler {turn}: {}",
            "Auto-compaction must preserve durable ranges, tool pairs, recall markers, and exact resume reconstruction. "
                .repeat(60)
        )));
        seed.push(Message::assistant(
            "Acknowledged. The constraints remain part of the session state.",
        ));
    }
    Ok(seed)
}

fn native_live_seed() -> Vec<Message> {
    let needle = "NATIVE-NEEDLE-7f3a9: the flag is --enable-zeta";
    vec![
        Message::user(&format!(
            "Remember this exact fact: {needle}. {}",
            "Native compaction must preserve this coding-session state, exact identifiers, decisions, and next steps. "
                .repeat(2_350)
        )),
        Message::assistant("The native-compaction needle is recorded."),
        Message::user("Retain the latest exchange verbatim."),
        Message::assistant("Retained."),
    ]
}

fn context_bytes(messages: &[Message]) -> Vec<u8> {
    serde_json::to_vec(messages).expect("messages serialize")
}

fn auto_live_session_count() -> usize {
    std::env::var("IRIS_AUTO_COMPACTION_SESSIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|count| *count > 0)
        .unwrap_or(10)
}

fn compaction_json_entries(path: &std::path::Path) -> Result<Vec<serde_json::Value>> {
    Ok(std::fs::read_to_string(path)?
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry.get("type").and_then(serde_json::Value::as_str) == Some("compaction"))
        .collect())
}

fn live_loop_enabled(test_name: &str) -> bool {
    if std::env::var("IRIS_BENCH_LIVE").ok().as_deref() != Some("1") {
        eprintln!("{test_name}: skipped (set IRIS_BENCH_LIVE=1 to run)");
        return false;
    }
    true
}

/// Repeated live auto-compaction protocol. Every row is one scripted session;
/// errors are printed verbatim and excluded rather than converted into made-up
/// metrics. G1 measures compaction-event-to-next-provider-start gaps inside a
/// continuing turn, excluding boundaries whose current pressure tier is hard.
fn auto_compaction_live_loop(lane: LiveLoopLane, session_count: usize) {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let policy = auto_live_policy();
    println!(
        "\n== LIVE auto-compaction loop ==\nlane={} sessions={} budget={} tokens policy={:.2}/{:.2}/{:.2} keep={}",
        lane.label(),
        session_count,
        AUTO_LIVE_BUDGET,
        policy.warn,
        policy.start,
        policy.hard,
        policy.keep_recent_tokens,
    );
    println!(
        "lane | session | compactions | G1 blocked | G2 max-after/start/pass | shallowest reclaim pre->post/reclaimed/covered%/total% | G3 needle/credential/marker/carry | G4 exact | G5 metadata | worker cache hit | parent cache pre->post/ratio/kind/pairs | real read | error"
    );

    let mut budget_used = 0usize;
    let mut outcomes = Vec::new();
    let mut gate_failures = Vec::new();
    for session in 0..session_count {
        let result = run_auto_compaction_live_session(lane, session, &workspace, policy);
        match result {
            Ok(row) => {
                let outcome = classify_live_gates(row.gates());
                // A G1-only flake is excluded only while the shared one-per-run
                // budget is still free; otherwise it counts as a gate failure.
                let excluded_g1_flake = outcome == LiveSessionOutcome::G1TimingFlake
                    && budget_used < LIVE_EXCLUSION_BUDGET;
                if excluded_g1_flake {
                    budget_used += 1;
                } else if outcome != LiveSessionOutcome::Pass {
                    gate_failures.push(format!(
                        "session {session:02}: compactions={} G1={:.1}ms/{} G2={} G3={}/{}/{}/{} G4={} G5={} read={}",
                        row.compactions,
                        row.g1_blocked_ms,
                        row.g1_non_blocking,
                        row.context_effective,
                        row.needle_answered,
                        row.credential_answered,
                        row.recall_marker,
                        row.carry_block,
                        row.resume_exact,
                        row.measured_entries,
                        row.real_read,
                    ));
                }
                outcomes.push(outcome);
                let detail = if excluded_g1_flake {
                    "excluded: G1 timing flake"
                } else {
                    row.metadata_detail.as_deref().unwrap_or("-")
                };
                println!(
                    "{} | {session:02} | {} | {:.1}ms/{} | {}/{}/{} | {}->{}/{}/{:.1}%/{:.1}% | {}/{}/{}/{} | {} | {} | {} | {}->{}/{}/{}/{} | {} | {}",
                    lane.label(),
                    row.compactions,
                    row.g1_blocked_ms,
                    row.g1_non_blocking,
                    row.max_context_after_apply,
                    row.start_threshold,
                    row.context_effective,
                    row.shallowest_reclaim.before,
                    row.shallowest_reclaim.after,
                    row.shallowest_reclaim.reclaimed,
                    row.shallowest_reclaim.covered_reduction_ratio * 100.0,
                    row.shallowest_reclaim.total_reduction_ratio * 100.0,
                    row.needle_answered,
                    row.credential_answered,
                    row.recall_marker,
                    row.carry_block,
                    row.resume_exact,
                    row.measured_entries,
                    row.summary_cache_hit_rate
                        .map(|rate| format!("{rate:.3}"))
                        .unwrap_or_else(|| "unknown".to_string()),
                    row.parent_cache.baseline_mass,
                    row.parent_cache.post_mass,
                    row.parent_cache
                        .ratio
                        .map(|rate| format!("{rate:.3}"))
                        .unwrap_or_else(|| "unknown".to_string()),
                    lane.cache_metric(),
                    row.parent_cache.paired_applies,
                    row.real_read,
                    detail,
                );
            }
            Err(error) => {
                budget_used += 1;
                outcomes.push(LiveSessionOutcome::ErrorExclusion);
                println!(
                    "{} | {session:02} | excluded | -/- | -/- | -/-/-/-/- | -/-/-/- | - | - | - | -->-/-/-/0 | - | {error:#}",
                    lane.label()
                );
            }
        }
    }
    let verdict = live_run_verdict(&outcomes);
    assert!(
        verdict.passed,
        "{} live protocol failed: exclusions={}; {}",
        lane.label(),
        verdict.exclusions,
        gate_failures.join("; ")
    );
}

struct AutoLiveRow {
    compactions: usize,
    g1_blocked_ms: f64,
    g1_non_blocking: bool,
    max_context_after_apply: u64,
    start_threshold: u64,
    context_effective: bool,
    shallowest_reclaim: ApplyReclamation,
    needle_answered: bool,
    credential_answered: bool,
    recall_marker: bool,
    carry_block: bool,
    resume_exact: bool,
    measured_entries: bool,
    summary_cache_hit_rate: Option<f64>,
    parent_cache: ParentCacheEconomics,
    real_read: bool,
    metadata_detail: Option<String>,
}

impl AutoLiveRow {
    /// Extract the boolean gate results this row asserts on.
    fn gates(&self) -> LiveSessionGates {
        LiveSessionGates {
            two_compactions: self.compactions >= 2,
            g1_non_blocking: self.g1_non_blocking,
            context_effective: self.context_effective,
            needle_answered: self.needle_answered,
            credential_answered: self.credential_answered,
            recall_marker: self.recall_marker,
            carry_block: self.carry_block,
            resume_exact: self.resume_exact,
            measured_entries: self.measured_entries,
            real_read: self.real_read,
        }
    }
}

fn run_auto_compaction_live_session(
    lane: LiveLoopLane,
    session: usize,
    workspace: &std::path::Path,
    policy: AutoLivePolicy,
) -> Result<AutoLiveRow> {
    let root = TempDir::new(&format!("auto-loop-{session}"));
    let mut log = SessionLog::create_in(&root.path, workspace)?;
    for message in auto_live_seed(session, workspace)? {
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

    let parent_key = format!("iris-auto-loop-{}-{session}-parent", lane.label());
    let parent_usages = Arc::new(Mutex::new(Vec::new()));
    let provider = RecordingProvider {
        inner: lane.build_provider(&parent_key)?,
        usages: parent_usages.clone(),
    };
    let agent = Agent::resumed(provider, built_in_tools().into_read_only(), stored.messages);
    let mut harness = Harness::resumed(
        agent,
        workspace.to_path_buf(),
        ToolState::new(),
        Some(log),
        entry_ids,
        Some(AUTO_LIVE_BUDGET),
    );
    harness.set_compaction_trigger(
        AUTO_LIVE_BUDGET.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: policy.warn,
            start: policy.start,
            hard: policy.hard,
            keep_recent_tokens: policy.keep_recent_tokens,
            hard_wait_ms: 10_000,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    harness.set_summarizer(SummarizerKind::Subagent);
    harness.set_compaction_summarizer_factory(Arc::new(build_live_compaction_worker));

    let observer = LiveLoopObserver::default();
    let gate = ReadOnlyGate;
    let token = CancellationToken::new();
    let mut turn_timeline = Vec::new();
    // Keep supplying real pair-closed read boundaries until two applies land.
    // A fixed turn count alone is insufficient when the second Opus worker is
    // still running: parent lanes grow at different rates and worker latency is
    // independent of parent latency. The cap keeps a broken trigger bounded.
    for turn in 0..14 {
        let prompt = format!(
            "Use the read tool on Cargo.toml, then reply in at most two short sentences. Turn {turn}. {}",
            "Keep this filler distinct so the real provider context grows toward the next compaction boundary. "
                .repeat(70)
        );
        let started = Instant::now();
        block_on(harness.submit_turn(&prompt, &observer, &gate, &token))?;
        turn_timeline.push((started, Instant::now()));
        let applied = observer
            .events
            .lock()
            .expect("live events lock")
            .iter()
            .filter(|timed| matches!(timed.event, AgentEvent::CompactionApplied { .. }))
            .count();
        if applied >= 2 {
            break;
        }
        if harness
            .context_diagnostics()
            .is_some_and(|diagnostics| diagnostics.background_running)
        {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }

    // A second job may start at the final filler's post-turn boundary. Give the
    // fixed Opus worker one bounded final interval before the needle probe
    // supplies the next safe apply boundary; the user's turn is not blocked by
    // this worker wait.
    let applied_before_probe = observer
        .events
        .lock()
        .expect("live events lock")
        .iter()
        .filter(|timed| matches!(timed.event, AgentEvent::CompactionApplied { .. }))
        .count();
    if applied_before_probe < 2
        && harness
            .context_diagnostics()
            .is_some_and(|diagnostics| diagnostics.background_running)
    {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    let credential = live_credential(session);
    let probe = format!(
        "What was the flag for NEEDLE-{session:02x}7f3a9, and what deploy password did I ask you \
         to remember? If either is behind a compaction reference, use recall. Reply with both \
         exactly."
    );
    let started = Instant::now();
    block_on(harness.submit_turn(&probe, &observer, &gate, &token))?;
    turn_timeline.push((started, Instant::now()));

    let live_messages = harness.messages().to_vec();
    let live_bytes = context_bytes(&live_messages);
    let rebuilt = store.open(&meta)?.messages;
    let resume_exact = live_bytes == context_bytes(&rebuilt);
    let context_text = live_messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let last_assistant_reply = live_messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant);
    let needle_answered =
        last_assistant_reply.is_some_and(|message| message.content.contains("--enable-zeta"));
    // Audit F17/F21: the credential-shaped needle is scored the same way as
    // the innocuous flag needle above (whole-text, on the model's final
    // reply) -- this lane's summarizer (`SummarizerKind::Subagent`) replays
    // the full transcript rather than the issue #475 structured-output route,
    // so it produces free-form model text with no guaranteed section
    // structure for field-wise scoring to key on. The field-wise scorer
    // (`tools::bench_support::assert_survives_fieldwise`) engages instead
    // wherever a genuine structured durable summary is under test, e.g. the
    // deterministic `wayland::background_compaction_tests` structured-summary
    // fallback-ladder coverage.
    let credential_answered =
        last_assistant_reply.is_some_and(|message| message.content.contains(&credential));
    let events = observer.events.lock().expect("live events lock");
    let real_read = events.iter().any(|timed| {
        matches!(
            &timed.event,
            AgentEvent::ToolResult { call, .. } if call.name == "read"
        )
    });
    let applies = events
        .iter()
        .filter_map(|timed| match &timed.event {
            AgentEvent::CompactionApplied {
                context_tokens_after_apply,
                original_tokens_estimate,
                summary_tokens_estimate,
                ..
            } => Some((
                timed.at,
                apply_reclamation(
                    *original_tokens_estimate,
                    *summary_tokens_estimate,
                    *context_tokens_after_apply,
                ),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    let _event_timeline_alignment = applies.iter().all(|(at, _)| {
        turn_timeline
            .iter()
            .any(|(started, ended)| started <= at && at <= ended)
    });
    let max_context_after_apply = applies
        .iter()
        .map(|(_, reclaim)| reclaim.after)
        .max()
        .unwrap_or(0);
    let shallowest_reclaim = applies
        .iter()
        .map(|(_, reclaim)| *reclaim)
        .min_by(|a, b| a.total_reduction_ratio.total_cmp(&b.total_reduction_ratio))
        .unwrap_or_else(|| apply_reclamation(0, 0, 0));
    let g1_blocked_ms = max_non_hard_compaction_block_ms(&events, &turn_timeline);
    let g1_non_blocking = g1_blocked_ms < 200.0;
    let context_effective = !applies.is_empty()
        && applies.iter().all(|(_, reclaim)| {
            reclaim.after <= (AUTO_LIVE_BUDGET as f64 * policy.start).floor() as u64
        });
    drop(events);

    let entries = compaction_json_entries(&path)?;
    let measured_entries = !entries.is_empty()
        && entries.iter().all(|entry| {
            let origin = entry
                .get("origin")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let usage = entry.get("workerUsage");
            match origin {
                "excerpts" => usage.is_some_and(serde_json::Value::is_null),
                "subagent" | "provider" => {
                    usage.is_some_and(|usage| !usage.is_null())
                        && usage
                            .and_then(|usage| usage.get("provider"))
                            .and_then(serde_json::Value::as_str)
                            == Some("anthropic")
                        && usage
                            .and_then(|usage| usage.get("model"))
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|model| model.starts_with(AUTO_LIVE_WORKER_MODEL))
                }
                _ => false,
            }
        });
    let metadata_detail = (!measured_entries).then(|| {
        entries
            .iter()
            .map(|entry| {
                let usage = entry.get("workerUsage");
                format!(
                    "origin={} worker={}/{}",
                    entry
                        .get("origin")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("null"),
                    usage
                        .and_then(|usage| usage.get("provider"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("null"),
                    usage
                        .and_then(|usage| usage.get("model"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("null")
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    });
    let (worker_input, worker_cache_read) = entries.iter().fold((0u64, 0u64), |acc, entry| {
        let usage = entry.get("workerUsage");
        (
            acc.0
                + usage
                    .and_then(|value| value.get("inputTokens"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            acc.1
                + usage
                    .and_then(|value| value.get("cacheReadInputTokens"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
        )
    });
    let summary_cache_hit_rate =
        (worker_input > 0).then_some(worker_cache_read as f64 / worker_input as f64);
    let parent_cache = parent_cache_economics(
        lane.cache_mass_model(),
        &applies,
        &parent_usages.lock().expect("parent usages lock"),
    );
    Ok(AutoLiveRow {
        compactions: applies.len(),
        g1_blocked_ms,
        g1_non_blocking,
        max_context_after_apply,
        start_threshold: (AUTO_LIVE_BUDGET as f64 * policy.start).floor() as u64,
        context_effective,
        shallowest_reclaim,
        needle_answered,
        credential_answered,
        recall_marker: context_text.matches("recall(handle=\"").count() >= applies.len(),
        carry_block: context_text.contains(AUTO_LIVE_CARRY_HEADER),
        resume_exact,
        measured_entries,
        summary_cache_hit_rate,
        parent_cache,
        real_read,
        metadata_detail,
    })
}

fn max_non_hard_compaction_block_ms(
    events: &[TimedEvent],
    turn_timeline: &[(Instant, Instant)],
) -> f64 {
    let mut pressure = ContextPressureTier::Normal;
    let mut worst = 0.0f64;
    for (index, timed) in events.iter().enumerate() {
        if let AgentEvent::ContextPressure { tier, .. } = &timed.event {
            pressure = *tier;
            continue;
        }
        let compaction_event = matches!(timed.event, AgentEvent::CompactionApplied { .. })
            || matches!(timed.event, AgentEvent::CompactionLifecycle { .. });
        if !compaction_event || pressure == ContextPressureTier::Hard {
            continue;
        }
        let Some((_, turn_end)) = turn_timeline
            .iter()
            .find(|(turn_start, turn_end)| *turn_start <= timed.at && timed.at <= *turn_end)
        else {
            continue;
        };
        let Some(next_request) = events[index + 1..].iter().find(|candidate| {
            candidate.at <= *turn_end
                && matches!(candidate.event, AgentEvent::ProviderTurnStarted { .. })
        }) else {
            // Post-turn lifecycle events do not block a continuing main loop.
            continue;
        };
        worst = worst.max(next_request.at.duration_since(timed.at).as_secs_f64() * 1_000.0);
    }
    worst
}

fn reactive_overflow_live(lane: LiveLoopLane) -> Result<()> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = TempDir::new("reactive-overflow");
    let mut log = SessionLog::create_in(&root.path, &workspace)?;
    for message in auto_live_seed(0xfe, &workspace)? {
        log.append(&message)?;
    }
    let path = log.path().to_path_buf();
    drop(log);

    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()?
        .into_iter()
        .find(|meta| meta.path == path)
        .ok_or_else(|| anyhow::anyhow!("reactive seed session was not listed"))?;
    let stored = store.open(&meta)?;
    let entry_ids = stored.entry_ids.clone();
    let log = SessionLog::resume(&path)?;
    let forwarded = Arc::new(AtomicUsize::new(0));
    let provider = InducedOverflowProvider {
        inner: lane.build_provider("iris-reactive-overflow-parent")?,
        inject_next: AtomicBool::new(true),
        forwarded: forwarded.clone(),
    };
    let agent = Agent::resumed(provider, built_in_tools().into_read_only(), stored.messages);
    let window = 131_072;
    let mut harness = Harness::resumed(
        agent,
        workspace,
        ToolState::new(),
        Some(log),
        entry_ids,
        Some(window),
    );
    harness.set_compaction_trigger(
        window.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.85,
            keep_recent_tokens: 1_000,
            hard_wait_ms: 10_000,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    harness.set_summarizer(SummarizerKind::Subagent);
    harness.set_compaction_summarizer_factory(Arc::new(build_live_compaction_worker));

    let observer = LiveLoopObserver::default();
    block_on(harness.submit_turn(
        "Use the read tool on Cargo.toml, then reply with one short sentence confirming recovery.",
        &observer,
        &ReadOnlyGate,
        &CancellationToken::new(),
    ))?;

    let events = observer.events.lock().expect("reactive events lock");
    let provider_starts = events
        .iter()
        .filter(|timed| matches!(timed.event, AgentEvent::ProviderTurnStarted { .. }))
        .count();
    let real_read = events.iter().any(
        |timed| matches!(&timed.event, AgentEvent::ToolResult { call, .. } if call.name == "read"),
    );
    let applied = events.iter().any(|timed| {
        matches!(
            timed.event,
            AgentEvent::CompactionApplied {
                origin: CompactionOrigin::Excerpts,
                ..
            }
        )
    });
    drop(events);
    let resume_exact =
        context_bytes(harness.messages()) == context_bytes(&store.open(&meta)?.messages);
    let entries = compaction_json_entries(&path)?;
    let measured = entries.iter().any(|entry| {
        entry.get("origin").and_then(serde_json::Value::as_str) == Some("excerpts")
            && entry
                .get("workerUsage")
                .is_some_and(serde_json::Value::is_null)
    });
    println!(
        "REACTIVE LIVE lane={} induced=1 forwarded={} provider_starts={} applied={} resume_exact={} G5={} read={}",
        lane.label(),
        forwarded.load(Ordering::SeqCst),
        provider_starts,
        applied,
        resume_exact,
        measured,
        real_read
    );
    assert!(forwarded.load(Ordering::SeqCst) >= 1);
    assert!(provider_starts >= 2, "overflow must trigger one resend");
    assert!(applied && resume_exact && measured && real_read);
    Ok(())
}

#[test]
fn g1_timing_counts_continuing_non_hard_gaps_and_excludes_hard_tier() {
    let base = Instant::now();
    let events = vec![
        TimedEvent {
            at: base,
            event: AgentEvent::ContextPressure {
                tier: ContextPressureTier::Start,
                measured: 70,
                effective_window: 100,
                source: ContextMeasurementSource::Estimated,
            },
        },
        TimedEvent {
            at: base + std::time::Duration::from_millis(1),
            event: AgentEvent::CompactionLifecycle {
                job_id: "job".to_string(),
                state: CompactionLifecycleState::Running,
                covered_messages: 2,
                original_tokens_estimate: 50,
                origin: CompactionOrigin::Subagent,
                worker_usage: None,
                trigger_tier: Some(ContextPressureTier::Start),
                message: None,
            },
        },
        TimedEvent {
            at: base + std::time::Duration::from_millis(51),
            event: AgentEvent::ProviderTurnStarted {
                turn_id: "turn_1".to_string(),
            },
        },
        TimedEvent {
            at: base + std::time::Duration::from_millis(60),
            event: AgentEvent::ContextPressure {
                tier: ContextPressureTier::Hard,
                measured: 90,
                effective_window: 100,
                source: ContextMeasurementSource::Estimated,
            },
        },
        TimedEvent {
            at: base + std::time::Duration::from_millis(61),
            event: AgentEvent::CompactionLifecycle {
                job_id: "job".to_string(),
                state: CompactionLifecycleState::Cancelled,
                covered_messages: 2,
                original_tokens_estimate: 50,
                origin: CompactionOrigin::Subagent,
                worker_usage: None,
                trigger_tier: Some(ContextPressureTier::Hard),
                message: None,
            },
        },
        TimedEvent {
            at: base + std::time::Duration::from_millis(561),
            event: AgentEvent::ProviderTurnStarted {
                turn_id: "turn_2".to_string(),
            },
        },
    ];
    let timelines = [(base, base + std::time::Duration::from_secs(1))];
    assert_eq!(max_non_hard_compaction_block_ms(&events, &timelines), 50.0);
}

#[test]
#[ignore = "live Anthropic API calls; set IRIS_BENCH_LIVE=1 to run"]
fn auto_compaction_live_loop_anthropic() {
    if !live_loop_enabled("auto_compaction_live_loop_anthropic") {
        return;
    }
    if !claude_code_credentials_available() {
        eprintln!(
            "LIVE RUN FAILED: no Claude Code credentials discovered (claude_code_credentials_available() == false); expected ~/.claude/.credentials.json"
        );
        return;
    }
    auto_compaction_live_loop(LiveLoopLane::AnthropicHaiku, auto_live_session_count());
}

#[test]
#[ignore = "live Codex API calls; set IRIS_BENCH_LIVE=1 to run"]
fn auto_compaction_live_loop_codex() {
    if !live_loop_enabled("auto_compaction_live_loop_codex") {
        return;
    }
    auto_compaction_live_loop(LiveLoopLane::CodexMini, auto_live_session_count());
}

#[test]
#[ignore = "live Anthropic provider-summary lifecycle; set IRIS_BENCH_LIVE=1 to run"]
fn auto_compaction_provider_live_anthropic() -> Result<()> {
    if !live_loop_enabled("auto_compaction_provider_live_anthropic") {
        return Ok(());
    }
    if !claude_code_credentials_available() {
        anyhow::bail!("no Claude Code credentials discovered");
    }
    auto_compaction_live(LiveLoopLane::AnthropicHaiku, false, "provider")
}

#[test]
#[ignore = "live Codex provider-summary lifecycle; set IRIS_BENCH_LIVE=1 to run"]
fn auto_compaction_provider_live_codex() -> Result<()> {
    if !live_loop_enabled("auto_compaction_provider_live_codex") {
        return Ok(());
    }
    auto_compaction_live(LiveLoopLane::CodexMini, false, "provider")
}

#[test]
#[ignore = "live Anthropic native-auto fallback lifecycle; set IRIS_BENCH_LIVE=1 to run"]
fn auto_compaction_native_auto_falls_back_anthropic() -> Result<()> {
    if !live_loop_enabled("auto_compaction_native_auto_falls_back_anthropic") {
        return Ok(());
    }
    if !claude_code_credentials_available() {
        anyhow::bail!("no Claude Code credentials discovered");
    }
    auto_compaction_live(LiveLoopLane::AnthropicHaiku, true, "provider")
}

#[test]
#[ignore = "live Codex native-compaction lifecycle; set IRIS_BENCH_LIVE=1 to run"]
fn auto_compaction_native_live_codex() -> Result<()> {
    if !live_loop_enabled("auto_compaction_native_live_codex") {
        return Ok(());
    }
    auto_compaction_live(LiveLoopLane::CodexMini, true, "providerNative")
}

fn auto_compaction_live(
    lane: LiveLoopLane,
    provider_native: bool,
    expected_origin: &str,
) -> Result<()> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = TempDir::new("native-loop");
    let mut log = SessionLog::create_in(&root.path, &workspace)?;
    for message in native_live_seed() {
        log.append(&message)?;
    }
    let path = log.path().to_path_buf();
    drop(log);
    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()?
        .into_iter()
        .find(|meta| meta.path == path)
        .ok_or_else(|| anyhow::anyhow!("native seed session was not listed"))?;
    let stored = store.open(&meta)?;
    let entry_ids = stored.entry_ids.clone();
    let log = SessionLog::resume(&path)?;
    let provider = lane.build_provider("native-parent")?;
    let agent = Agent::resumed(provider, built_in_tools().into_read_only(), stored.messages);
    let window = 80_000;
    let mut harness = Harness::resumed(
        agent,
        workspace,
        ToolState::new(),
        Some(log),
        entry_ids,
        Some(window),
    );
    harness.set_compaction_trigger(
        window.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.55,
            start: 0.65,
            hard: 0.90,
            keep_recent_tokens: 1_000,
            hard_wait_ms: 10_000,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    harness.set_summarizer(SummarizerKind::Provider);
    harness.set_compaction_worker(CompactionWorkerConfig {
        instructions:
            "Preserve exact flags, identifiers, decisions, current state, and next steps."
                .to_string(),
        ..CompactionWorkerConfig::default()
    });
    harness.set_compaction_summarizer_factory(Arc::new(move || {
        lane.build_summary_provider("provider-summary")
    }));
    harness.set_provider_native(provider_native);
    harness.set_provider_compaction_factory(Arc::new(move || {
        lane.build_provider("native-compaction")
    }));
    let observer = LiveLoopObserver::default();
    let token = CancellationToken::new();
    block_on(harness.submit_turn(
        "Use the read tool on Cargo.toml, then reply briefly. You must use the tool.",
        &observer,
        &ReadOnlyGate,
        &token,
    ))?;
    if harness
        .context_diagnostics()
        .is_some_and(|diagnostics| diagnostics.background_running)
    {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
    block_on(harness.submit_turn(
        "What was the exact flag for NATIVE-NEEDLE-7f3a9? Reply with the flag exactly.",
        &observer,
        &ReadOnlyGate,
        &token,
    ))?;

    let entries = compaction_json_entries(&path)?;
    let applied = entries.iter().find(|entry| {
        entry.get("origin").and_then(serde_json::Value::as_str) == Some(expected_origin)
    });
    let resume_exact =
        context_bytes(harness.messages()) == context_bytes(&store.open(&meta)?.messages);
    let needle = harness
        .messages()
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant)
        .is_some_and(|message| message.content.contains("--enable-zeta"));
    let lifecycle_failures = observer
        .events
        .lock()
        .expect("native events lock")
        .iter()
        .filter_map(|timed| match &timed.event {
            AgentEvent::CompactionLifecycle {
                state: CompactionLifecycleState::Failed,
                message: Some(message),
                ..
            } => Some(message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    println!(
        "COMPACTION LIVE lane={} path={} applied={} blocks={} usage={} needle={} resume_exact={} failures={} entries={}",
        lane.label(),
        expected_origin,
        applied.is_some(),
        applied
            .and_then(|entry| entry.get("providerBlocks"))
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len),
        applied
            .and_then(|entry| entry.get("workerUsage"))
            .is_some_and(|usage| !usage.is_null()),
        needle,
        resume_exact,
        serde_json::to_string(&lifecycle_failures)
            .unwrap_or_else(|_| "<unserializable>".to_string()),
        serde_json::to_string(&entries).unwrap_or_else(|_| "<unserializable>".to_string()),
    );
    assert!(
        lifecycle_failures.is_empty(),
        "compaction lifecycle failures: {lifecycle_failures:?}"
    );
    assert!(
        applied.is_some(),
        "{expected_origin} compaction entry was not applied"
    );
    if expected_origin == "providerNative" {
        assert!(
            applied
                .and_then(|entry| entry.get("providerBlocks"))
                .and_then(serde_json::Value::as_array)
                .is_some_and(|blocks| blocks.len() == 1)
        );
    }
    assert!(
        applied
            .and_then(|entry| entry.get("workerUsage"))
            .is_some_and(|usage| !usage.is_null())
    );
    assert!(needle && resume_exact);
    Ok(())
}

#[test]
#[ignore = "live Codex native-compaction capability probe; set both live probe env flags"]
fn auto_compaction_native_probe_codex() -> Result<()> {
    if !live_loop_enabled("auto_compaction_native_probe_codex") {
        return Ok(());
    }
    if std::env::var("IRIS_OPENAI_NATIVE_COMPACTION_PROBE")
        .ok()
        .as_deref()
        != Some("1")
    {
        eprintln!(
            "auto_compaction_native_probe_codex: skipped (set IRIS_OPENAI_NATIVE_COMPACTION_PROBE=1 to authorize the probe)"
        );
        return Ok(());
    }
    let provider =
        crate::mimir::providers::openai_codex_responses::OpenAiCodexResponsesProvider::new(
            "gpt-5.4-mini",
            "https://chatgpt.com/backend-api",
            None,
            LIVE_SYSTEM_PROMPT,
            "auto-compaction-native-probe",
            PromptCacheRetention::DEFAULT,
            RetryPolicy::default(),
            CodexTransport::Sse,
            Some(std::time::Duration::from_secs(300)),
        )?;
    let block = provider.probe_v2_compaction(&native_live_seed(), &CancellationToken::new())?;
    println!(
        "OPENAI NATIVE PROBE lane={} adapter={} model={} blocks=1 portable_text=false",
        LiveLoopLane::CodexMini.label(),
        block
            .get("adapter")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<missing>"),
        block
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("<missing>"),
    );
    assert_eq!(
        block.get("adapter").and_then(serde_json::Value::as_str),
        Some("openai-codex-responses")
    );
    Ok(())
}

#[test]
#[ignore = "live Anthropic API calls; set IRIS_BENCH_LIVE=1 to run"]
fn auto_compaction_reactive_overflow_anthropic() -> Result<()> {
    if !live_loop_enabled("auto_compaction_reactive_overflow_anthropic") {
        return Ok(());
    }
    if !claude_code_credentials_available() {
        anyhow::bail!("no Claude Code credentials discovered");
    }
    reactive_overflow_live(LiveLoopLane::AnthropicHaiku)
}

#[test]
#[ignore = "live Codex API calls; set IRIS_BENCH_LIVE=1 to run"]
fn auto_compaction_reactive_overflow_codex() -> Result<()> {
    if !live_loop_enabled("auto_compaction_reactive_overflow_codex") {
        return Ok(());
    }
    reactive_overflow_live(LiveLoopLane::CodexMini)
}

#[test]
#[ignore = "live Anthropic API call; set IRIS_BENCH_LIVE=1 to run"]
fn compaction_cache_economics_live_anthropic() {
    // Second gate: even `cargo test -- --ignored` is a no-op without opt-in.
    if std::env::var("IRIS_BENCH_LIVE").ok().as_deref() != Some("1") {
        eprintln!(
            "compaction_cache_economics_live_anthropic: skipped (set IRIS_BENCH_LIVE=1 to run)"
        );
        return;
    }
    if !claude_code_credentials_available() {
        eprintln!(
            "LIVE RUN FAILED: no Claude Code credentials discovered \
             (claude_code_credentials_available() == false); expected ~/.claude/.credentials.json"
        );
        return;
    }

    let provider = match AnthropicProvider::new(
        LIVE_MODEL,
        "https://api.anthropic.com",
        None,
        LIVE_SYSTEM_PROMPT,
        PromptCacheRetention::DEFAULT,
        ContextManagement::default(),
        RetryPolicy::default(),
    ) {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("LIVE RUN FAILED: AnthropicProvider::new error: {error:#}");
            return;
        }
    };

    // Seed a near-budget transcript and resume through the store, exactly as the
    // startup path does, so the loaded prefix carries its durable entry ids and
    // stays compactable.
    let root = TempDir::new("root");
    let workspace = TempDir::new("ws");
    let mut log = SessionLog::create_in(&root.path, &workspace.path).expect("create session log");
    for message in live_seed() {
        log.append(&message).expect("append seed message");
    }
    let path = log.path().to_path_buf();
    drop(log);

    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .expect("list sessions")
        .into_iter()
        .find(|m| m.path == path)
        .expect("seeded session is listed");
    let stored = store.open(&meta).expect("open seeded session");
    let entry_ids = stored.entry_ids.clone();
    let log = SessionLog::resume(&path).expect("resume session log");

    let usages = Arc::new(Mutex::new(Vec::new()));
    let recording = RecordingProvider {
        inner: provider,
        usages: usages.clone(),
    };
    // Budget ABOVE the resumed seed estimate so turn 1 sends the seed WITHOUT
    // compacting -- warming the provider's prompt cache over the covered prefix.
    // Turn 1's large prompt then pushes the context over budget, so the turn-2
    // compaction covers the now-cached seed prefix and its summarization request
    // can realize a cache HIT (cache_read > 0). The chars/4 estimate the seam
    // gates on is deterministic, so the budget is tuned against it with headroom.
    let seed_estimate = stored
        .messages
        .iter()
        .map(|m| m.content.len())
        .sum::<usize>() as u64
        / 4;
    let budget = seed_estimate + 500;
    eprintln!("live: resumed seed estimate ~{seed_estimate} tokens; budget {budget}");
    let agent = Agent::resumed(recording, Tools::new(Vec::new()), stored.messages);
    let mut harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        entry_ids,
        Some(budget),
    );
    harness.set_summarizer(SummarizerKind::Provider);

    let obs = NoopObserver;
    let gate = NoToolGate;
    let token = CancellationToken::new();

    // A large first prompt that, added on turn 1, pushes the context over budget
    // so the turn-2 compaction covers the warm (already-sent) seed prefix.
    let warming_prompt = format!(
        "Continue the reconciliation wiring. {}",
        "Provide a detailed status update on the sink and buffer wiring and the remaining \
         constraints so the record is complete before proceeding. "
            .repeat(40)
    );
    // Turn 1 warms the cache (no compaction); turn 2 drives the compaction
    // (summarization request over the warm prefix) plus the first post-compaction
    // turn; turn 3 is the follow-up turn.
    for prompt in [
        warming_prompt.as_str(),
        "Proceed with the next small wiring step and report back briefly.",
        "Follow-up: summarize the current state in one short line.",
    ] {
        if let Err(error) = block_on(harness.submit_turn(prompt, &obs, &gate, &token)) {
            eprintln!("LIVE RUN FAILED: submit_turn error: {error:#}");
            return;
        }
    }

    let captured = usages.lock().expect("usages lock").clone();
    let date = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();
    println!("\n== LIVE cache economics (Anthropic Claude Code OAuth) ==");
    println!(
        "lane: Anthropic Messages / Claude Code OAuth; model: {LIVE_MODEL}; unix_date: {date}"
    );
    let summary = captured
        .iter()
        .find(|c| c.is_summary)
        .and_then(|c| c.usage.clone());
    match summary {
        Some(u) => {
            let hit = if u.input_tokens > 0 {
                u.cache_read_input_tokens as f64 / u.input_tokens as f64
            } else {
                0.0
            };
            println!(
                "summarization request: input_tokens={}, cache_read={}, cache_write={}, \
                 realized cache-HIT rate={:.2}",
                u.input_tokens, u.cache_read_input_tokens, u.cache_write_input_tokens, hit,
            );
        }
        None => println!("summarization request: no usage captured"),
    }
    // Ordering invariant: `captured` is in capture (send) order. Turn 1 sends a
    // pre-compaction WARMING request (non-summary) BEFORE the turn-2
    // summarization request, so the first non-summary sample in the whole run is
    // that warming request, NOT a post-compaction request. Select the first
    // non-summary usage that comes strictly AFTER the summarization request in
    // capture order; that is the genuine first post-compaction request.
    let summary_index = captured.iter().position(|c| c.is_summary);
    let post = summary_index.and_then(|idx| {
        captured
            .iter()
            .skip(idx + 1)
            .filter(|c| !c.is_summary)
            .find_map(|c| c.usage.clone())
    });
    match post {
        Some(u) => {
            let (m5, h1) = u
                .cache_creation
                .as_ref()
                .map(|c| (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens))
                .unwrap_or((0, 0));
            println!(
                "first post-compaction request: input_tokens={}, cache_read={}, \
                 cache_write={} (5m={m5}, 1h={h1})",
                u.input_tokens, u.cache_read_input_tokens, u.cache_write_input_tokens,
            );
        }
        None => println!("first post-compaction request: no usage captured"),
    }
}

// --- Fold-flush cost, realized (issue #400). ---

/// The superseded-read path in the live fold seed (mirrors the modeled
/// `FOLD_PATH` scenario in `compaction_bench.rs`).
const LIVE_FOLD_PATH: &str = "crates/orbit/src/telemetry/buffer.rs";

/// A successful ADR-0021 `read` result envelope, as the fold engine's
/// `successful_target` expects (ok + `metadata.target`).
fn live_read_result(call: &str, body: &str) -> Message {
    Message::tool_result(
        call,
        "read",
        &serde_json::json!({
            "ok": true,
            "content": body,
            "metadata": { "target": LIVE_FOLD_PATH },
        })
        .to_string(),
    )
}

/// An assistant `read` tool call for `LIVE_FOLD_PATH`. The Anthropic Messages
/// API validates tool_use/tool_result pairing, so unlike the fake lane the
/// live seed must carry the calls the results answer.
fn live_read_call(id: &str) -> Message {
    Message::assistant_tool_call(&ToolCall {
        id: id.to_string(),
        name: "read".to_string(),
        arguments: serde_json::json!({ "path": LIVE_FOLD_PATH }),
        thought_signature: None,
    })
}

/// Fold-cost seed: an early needle-bearing `read` of `LIVE_FOLD_PATH` that a
/// later, much larger read of the same path supersedes. The superseding read
/// alone exceeds the 2000-token protected fold tail, so the earlier read sits
/// before `fold_tail_start` and is foldable. Total ~2900 estimated tokens --
/// comfortably above Anthropic's minimum cacheable prefix, so turn 1 writes a
/// prefix turn 2 can realize reads against.
fn fold_live_seed() -> Vec<Message> {
    let fold_body = format!(
        "LIVE-FOLD-DETAIL-4417 :: {}",
        "spent buffer read detail for the reconciliation work. ".repeat(40)
    );
    let superseding = "current buffer contents after the ledger reconciliation pass. ".repeat(140);
    vec![
        Message::user("start: we are reconciling the ledger sink; read the buffer first."),
        live_read_call("lf-1"),
        live_read_result("lf-1", &fold_body),
        Message::assistant("Noted the buffer contents."),
        Message::user("the buffer changed; read it again before continuing"),
        live_read_call("lf-2"),
        live_read_result("lf-2", &superseding),
        Message::assistant("Done; the latest buffer contents are loaded."),
    ]
}

/// One live fold-cost run: seed, resume, drive two turns, return the captured
/// usages, the fold count, and the seed estimate. `micro` toggles
/// microcompaction; everything else is byte-identical between runs.
fn run_fold_cost_live(
    micro: bool,
    warming_prompt: &str,
    steady_prompt: &str,
) -> Option<(Vec<CapturedUsage>, usize, u64)> {
    let provider = match AnthropicProvider::new(
        LIVE_MODEL,
        "https://api.anthropic.com",
        None,
        LIVE_SYSTEM_PROMPT,
        PromptCacheRetention::DEFAULT,
        ContextManagement::default(),
        RetryPolicy::default(),
    ) {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("LIVE RUN FAILED: AnthropicProvider::new error: {error:#}");
            return None;
        }
    };

    let root = TempDir::new("fold-root");
    let workspace = TempDir::new("fold-ws");
    let mut log = SessionLog::create_in(&root.path, &workspace.path).expect("create session log");
    for message in fold_live_seed() {
        log.append(&message).expect("append seed message");
    }
    let path = log.path().to_path_buf();
    drop(log);

    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .expect("list sessions")
        .into_iter()
        .find(|m| m.path == path)
        .expect("seeded session is listed");
    let stored = store.open(&meta).expect("open seeded session");
    let entry_ids = stored.entry_ids.clone();
    let log = SessionLog::resume(&path).expect("resume session log");

    let usages = Arc::new(Mutex::new(Vec::new()));
    let recording = RecordingProvider {
        inner: provider,
        usages: usages.clone(),
    };
    // The flush must land at the TURN-2 boundary so turn 1 warms the ORIGINAL
    // (unfolded) prefix and turn 2's request shows the realized break. With
    // budget = 2 * (seed + 150): the micro-watermark (budget/2 = seed + 150)
    // sits ABOVE the bare seed (no flush at turn 1) and BELOW seed + turn-1
    // exchange (flush at turn 2). Compaction never fires (total stays far
    // under budget).
    let seed_estimate = stored
        .messages
        .iter()
        .map(|m| m.content.len())
        .sum::<usize>() as u64
        / 4;
    let budget = 2 * (seed_estimate + 150);
    let agent = Agent::resumed(recording, Tools::new(Vec::new()), stored.messages);
    let mut harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        entry_ids,
        Some(budget),
    );
    harness.set_summarizer(SummarizerKind::Provider);
    harness.set_microcompaction(micro);

    let obs = NoopObserver;
    let gate = NoToolGate;
    let token = CancellationToken::new();
    for prompt in [warming_prompt, steady_prompt] {
        if let Err(error) = block_on(harness.submit_turn(prompt, &obs, &gate, &token)) {
            eprintln!("LIVE RUN FAILED: submit_turn error: {error:#}");
            return None;
        }
    }

    let folds = super::compaction_bench::fold_count(&path);
    let captured = usages.lock().expect("usages lock").clone();
    Some((captured, folds, seed_estimate))
}

#[test]
#[ignore = "live Anthropic API calls; set IRIS_BENCH_LIVE=1 to run"]
fn fold_flush_cost_live_anthropic() {
    if std::env::var("IRIS_BENCH_LIVE").ok().as_deref() != Some("1") {
        eprintln!("fold_flush_cost_live_anthropic: skipped (set IRIS_BENCH_LIVE=1 to run)");
        return;
    }
    if !claude_code_credentials_available() {
        eprintln!(
            "LIVE RUN FAILED: no Claude Code credentials discovered \
             (claude_code_credentials_available() == false); expected ~/.claude/.credentials.json"
        );
        return;
    }

    // Turn-1 prompt is large enough (~300 estimated tokens) to push the total
    // past the micro-watermark; both prompts forbid tool use so the request
    // stream stays two normal turns per run.
    let warming_prompt = format!(
        "Do not use any tools. Reply with one short sentence acknowledging the state. {}",
        "The reconciliation status must be recorded before the next wiring step proceeds. "
            .repeat(15)
    );
    let steady_prompt = "Do not use any tools. In one short sentence: is the buffer state current?";

    let date = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();
    println!("\n== LIVE fold-flush cost (Anthropic Claude Code OAuth) ==");
    println!(
        "lane: Anthropic Messages / Claude Code OAuth; model: {LIVE_MODEL}; unix_date: {date}"
    );

    let Some((ctrl, ctrl_folds, seed_est)) =
        run_fold_cost_live(false, &warming_prompt, steady_prompt)
    else {
        return;
    };
    let Some((arm, arm_folds, _)) = run_fold_cost_live(true, &warming_prompt, steady_prompt) else {
        return;
    };
    println!("seed estimate ~{seed_est} tokens; folds: control={ctrl_folds}, arm={arm_folds}");
    if arm_folds == 0 {
        println!(
            "LIVE RUN INCONCLUSIVE: the arm wrote no fold (watermark not crossed at turn 2); \
             no realized fold cost to report."
        );
        return;
    }

    // Select each run's turn-2 request by prompt tag, not position.
    let steady_tag = |c: &CapturedUsage| c.tag.starts_with("Do not use any tools. In one sho");
    let pick = |run: &[CapturedUsage]| -> Option<ProviderUsage> {
        run.iter()
            .find(|c| steady_tag(c))
            .and_then(|c| c.usage.clone())
    };
    let report = |label: &str, u: &Option<ProviderUsage>| match u {
        Some(u) => {
            let (m5, h1) = u
                .cache_creation
                .as_ref()
                .map(|c| (c.ephemeral_5m_input_tokens, c.ephemeral_1h_input_tokens))
                .unwrap_or((0, 0));
            println!(
                "{label}: input_tokens={}, cache_read={}, cache_write={} (5m={m5}, 1h={h1})",
                u.input_tokens, u.cache_read_input_tokens, u.cache_write_input_tokens,
            );
        }
        None => println!("{label}: no usage captured"),
    };
    let ctrl_t2 = pick(&ctrl);
    let arm_t2 = pick(&arm);
    report("control turn-2 (no fold)", &ctrl_t2);
    report("arm turn-2 (post-flush)  ", &arm_t2);
    if let (Some(c), Some(a)) = (&ctrl_t2, &arm_t2) {
        println!(
            "realized marginal fold cost: cache_write {} - {} = {} tokens; \
             cache_read drop: {} - {} = {} tokens",
            a.cache_write_input_tokens,
            c.cache_write_input_tokens,
            a.cache_write_input_tokens as i64 - c.cache_write_input_tokens as i64,
            c.cache_read_input_tokens,
            a.cache_read_input_tokens,
            c.cache_read_input_tokens as i64 - a.cache_read_input_tokens as i64,
        );
    }
}

// --- Phase 2 (issue #400 M5): inferred-cold (Class B) live pair. ---
//
// One run per lane, both double-gated. Each drives the SAME shape: turn 1
// warms the prefix, a real idle wait past the provider TTL follows, and turn
// 2 (whose boundary infers cold and flushes the pending fold when
// microcompaction is on) captures the realized usage. Anthropic reports the
// write side (realized write delta); the write-blind Codex lane reports the
// read side (realized cached_tokens drop). Default idle: 390 s (past the
// Anthropic 5 m tier and inside the documented OpenAI 5-10 min eviction
// window); override with IRIS_BENCH_IDLE_SECS for a shorter smoke run -- the
// actual wait is printed with the numbers either way.

fn live_idle_wait() -> std::time::Duration {
    let secs = std::env::var("IRIS_BENCH_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(390);
    std::time::Duration::from_secs(secs)
}

/// Drive one inferred-cold live run against `provider`: turn 1 warm, real
/// idle wait, turn 2 post-gap. The cold threshold is set just below the wait
/// so the Class-B trigger fires exactly at the turn-2 boundary when `micro`
/// is on. Returns the captured usages and the fold count.
fn run_inferred_cold_live<P: ChatProvider>(
    provider: P,
    micro: bool,
    idle: std::time::Duration,
) -> Option<(Vec<CapturedUsage>, usize)> {
    let root = TempDir::new("cold-root");
    let workspace = TempDir::new("cold-ws");
    let mut log = SessionLog::create_in(&root.path, &workspace.path).expect("create session log");
    for message in fold_live_seed() {
        log.append(&message).expect("append seed message");
    }
    let path = log.path().to_path_buf();
    drop(log);

    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .expect("list sessions")
        .into_iter()
        .find(|m| m.path == path)
        .expect("seeded session is listed");
    let stored = store.open(&meta).expect("open seeded session");
    let entry_ids = stored.entry_ids.clone();
    let log = SessionLog::resume(&path).expect("resume session log");

    let usages = Arc::new(Mutex::new(Vec::new()));
    let recording = RecordingProvider {
        inner: provider,
        usages: usages.clone(),
    };
    // Budget far above the running total: neither compaction nor the
    // watermark can fire; only the inferred-cold trigger releases the fold.
    let seed_estimate = stored
        .messages
        .iter()
        .map(|m| m.content.len())
        .sum::<usize>() as u64
        / 4;
    let budget = seed_estimate * 8;
    let agent = Agent::resumed(recording, Tools::new(Vec::new()), stored.messages);
    let mut harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        entry_ids,
        Some(budget),
    );
    harness.set_summarizer(SummarizerKind::Provider);
    harness.set_microcompaction(micro);

    let obs = NoopObserver;
    let gate = NoToolGate;
    let token = CancellationToken::new();
    // Turn 1: warm the prefix (no cold threshold installed yet, and the
    // resume-time A4 check sees a seconds-old transcript -> holds).
    if let Err(error) = block_on(harness.submit_turn(
        "Do not use any tools. Reply with one short sentence acknowledging the buffer state.",
        &obs,
        &gate,
        &token,
    )) {
        eprintln!("LIVE RUN FAILED: submit_turn (warm) error: {error:#}");
        return None;
    }
    // The real idle gap, with the profile threshold just inside it.
    harness.set_cache_profile(crate::wayland::CacheProfile {
        cold_after: Some(idle.saturating_sub(std::time::Duration::from_secs(10))),
        ..Default::default()
    });
    std::thread::sleep(idle);
    if let Err(error) = block_on(harness.submit_turn(
        "Do not use any tools. In one short sentence: is the buffer state current?",
        &obs,
        &gate,
        &token,
    )) {
        eprintln!("LIVE RUN FAILED: submit_turn (post-gap) error: {error:#}");
        return None;
    }

    let folds = super::compaction_bench::fold_count(&path);
    let captured = usages.lock().expect("usages lock").clone();
    Some((captured, folds))
}

/// Post-gap (turn-2) usage: the request tagged with the steady prompt.
fn post_gap_usage(run: &[CapturedUsage]) -> Option<ProviderUsage> {
    run.iter()
        .find(|c| c.tag.starts_with("Do not use any tools. In one sho"))
        .and_then(|c| c.usage.clone())
}

#[test]
#[ignore = "live Anthropic API calls with a real idle wait; set IRIS_BENCH_LIVE=1 to run"]
fn inferred_cold_flush_live_anthropic() {
    if std::env::var("IRIS_BENCH_LIVE").ok().as_deref() != Some("1") {
        eprintln!("inferred_cold_flush_live_anthropic: skipped (set IRIS_BENCH_LIVE=1 to run)");
        return;
    }
    if !claude_code_credentials_available() {
        eprintln!("LIVE RUN FAILED: no Claude Code credentials discovered");
        return;
    }
    let idle = live_idle_wait();
    let date = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();
    println!("\n== LIVE inferred-cold flush (Anthropic Claude Code OAuth) ==");
    println!(
        "lane: Anthropic Messages / Claude Code OAuth; model: {LIVE_MODEL}; \
         idle wait: {}s (5m-tier TTL is 300s); unix_date: {date}",
        idle.as_secs()
    );
    let build = || {
        AnthropicProvider::new(
            LIVE_MODEL,
            "https://api.anthropic.com",
            None,
            LIVE_SYSTEM_PROMPT,
            PromptCacheRetention::DEFAULT,
            ContextManagement::default(),
            RetryPolicy::default(),
        )
    };
    let control = match build() {
        Ok(p) => run_inferred_cold_live(p, false, idle),
        Err(error) => {
            eprintln!("LIVE RUN FAILED: AnthropicProvider::new error: {error:#}");
            return;
        }
    };
    let arm = match build() {
        Ok(p) => run_inferred_cold_live(p, true, idle),
        Err(error) => {
            eprintln!("LIVE RUN FAILED: AnthropicProvider::new error: {error:#}");
            return;
        }
    };
    let (Some((ctrl, ctrl_folds)), Some((arm, arm_folds))) = (control, arm) else {
        return;
    };
    println!("folds: control={ctrl_folds}, arm={arm_folds}");
    if arm_folds == 0 {
        println!("LIVE RUN INCONCLUSIVE: the arm wrote no fold at the post-gap boundary.");
        return;
    }
    let (ctrl_t2, arm_t2) = (post_gap_usage(&ctrl), post_gap_usage(&arm));
    let report = |label: &str, u: &Option<ProviderUsage>| match u {
        Some(u) => println!(
            "{label}: input_tokens={}, cache_read={}, cache_write={}",
            u.input_tokens, u.cache_read_input_tokens, u.cache_write_input_tokens
        ),
        None => println!("{label}: no usage captured"),
    };
    report("control post-gap (no fold)", &ctrl_t2);
    report("arm post-gap (B flush)    ", &arm_t2);
    if let (Some(c), Some(a)) = (&ctrl_t2, &arm_t2) {
        println!(
            "realized write delta (arm - control): {} tokens (past the TTL both re-write; \
             <= 0 means the fold only shrank the cold re-write)",
            a.cache_write_input_tokens as i64 - c.cache_write_input_tokens as i64
        );
    }
}

#[test]
#[ignore = "live Codex API calls with a real idle wait; set IRIS_BENCH_LIVE=1 to run"]
fn inferred_cold_flush_live_codex() {
    if std::env::var("IRIS_BENCH_LIVE").ok().as_deref() != Some("1") {
        eprintln!("inferred_cold_flush_live_codex: skipped (set IRIS_BENCH_LIVE=1 to run)");
        return;
    }
    let idle = live_idle_wait();
    let date = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs();
    println!("\n== LIVE inferred-cold flush (OpenAI Codex Responses) ==");
    println!(
        "lane: Codex Responses (write-blind: cache_write is hardcoded 0; the read side is \
         what this lane can measure); model: gpt-5.5; idle wait: {}s (documented in-memory \
         eviction 5-10 min); unix_date: {date}",
        idle.as_secs()
    );
    let build = |key: &str| {
        crate::mimir::providers::openai_codex_responses::OpenAiCodexResponsesProvider::new(
            "gpt-5.5",
            "https://chatgpt.com/backend-api",
            None,
            LIVE_SYSTEM_PROMPT,
            key,
            PromptCacheRetention::DEFAULT,
            RetryPolicy::default(),
            CodexTransport::Sse,
            Some(std::time::Duration::from_secs(300)),
        )
    };
    let control = match build("iris-bench-cold-ctrl") {
        Ok(p) => run_inferred_cold_live(p, false, idle),
        Err(error) => {
            eprintln!("LIVE RUN FAILED: OpenAiCodexResponsesProvider::new error: {error:#}");
            return;
        }
    };
    let arm = match build("iris-bench-cold-arm") {
        Ok(p) => run_inferred_cold_live(p, true, idle),
        Err(error) => {
            eprintln!("LIVE RUN FAILED: OpenAiCodexResponsesProvider::new error: {error:#}");
            return;
        }
    };
    let (Some((ctrl, ctrl_folds)), Some((arm, arm_folds))) = (control, arm) else {
        return;
    };
    println!("folds: control={ctrl_folds}, arm={arm_folds}");
    if arm_folds == 0 {
        println!("LIVE RUN INCONCLUSIVE: the arm wrote no fold at the post-gap boundary.");
        return;
    }
    // The write-blind lane measures the read side: past the eviction window
    // the post-gap request's cached_tokens collapse for BOTH runs (the drop
    // is the gap's doing, not the fold's), and the fold costs nothing extra.
    let warm = |run: &[CapturedUsage]| {
        run.iter()
            .find(|c| c.tag.starts_with("Do not use any tools. Reply with"))
            .and_then(|c| c.usage.clone())
    };
    let report = |label: &str, u: &Option<ProviderUsage>| match u {
        Some(u) => println!(
            "{label}: input_tokens={}, cached_tokens(read)={}",
            u.input_tokens, u.cache_read_input_tokens
        ),
        None => println!("{label}: no usage captured"),
    };
    report("control warm turn          ", &warm(&ctrl));
    report("control post-gap (no fold) ", &post_gap_usage(&ctrl));
    report("arm post-gap (B flush)     ", &post_gap_usage(&arm));
    if let (Some(c), Some(a)) = (&post_gap_usage(&ctrl), &post_gap_usage(&arm)) {
        println!(
            "realized cached-read delta (control - arm): {} tokens; realized input delta \
             (control - arm): {} tokens (the fold's residency saving on the cold re-bill)",
            c.cache_read_input_tokens as i64 - a.cache_read_input_tokens as i64,
            c.input_tokens as i64 - a.input_tokens as i64
        );
    }
}
