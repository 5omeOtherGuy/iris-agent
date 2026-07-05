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

use super::*;
use crate::mimir::auth::anthropic::claude_code_credentials_available;
use crate::mimir::providers::anthropic_messages::AnthropicProvider;
use crate::mimir::retry::RetryPolicy;
use crate::mimir::selection::{ContextManagement, PromptCacheRetention};
use crate::session::{SessionLog, SessionStore};
use crate::tools::ToolState;
use crate::wayland::{Harness, SummarizerKind};
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// A Claude Code subscription model id (see `anthropic_models.rs`).
const LIVE_MODEL: &str = "claude-sonnet-4-6";
/// Minimal system prompt; the provider prepends the required Claude Code
/// identity block itself.
const LIVE_SYSTEM_PROMPT: &str = "You are a coding assistant. Keep answers short.";
/// Matches the summarizer instruction prefix (`SUMMARY_PROMPT`, private to
/// `crate::wayland`) so the recording wrapper tags a summarization request.
const SUMMARY_INSTRUCTION_PREFIX: &str = "Summarize this coding session";

/// One provider round-trip's realized usage, tagged summarization vs normal.
/// `tag` is the first bytes of the request's LAST message content, so a run
/// can select a specific turn's request by its prompt without relying on
/// positional order (a spurious model tool-call would shift positions).
#[derive(Clone)]
struct CapturedUsage {
    is_summary: bool,
    tag: String,
    usage: Option<ProviderUsage>,
}

/// Wraps a real provider and records the `ProviderUsage` on every completed
/// turn, tagging summarization requests (last message is the summary
/// instruction). Test-only: `provider_summary` discards usage on the production
/// path, so this wrapper is how the summarization request's realized cache-hit
/// rate is captured WITHOUT touching production code.
struct RecordingProvider<P: ChatProvider> {
    inner: P,
    usages: Arc<Mutex<Vec<CapturedUsage>>>,
}

impl<P: ChatProvider> ChatProvider for RecordingProvider<P> {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a Tools,
        cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        let is_summary = messages
            .last()
            .is_some_and(|m| m.content.starts_with(SUMMARY_INSTRUCTION_PREFIX));
        let tag = messages
            .last()
            .map(|m| m.content.chars().take(32).collect::<String>())
            .unwrap_or_default();
        let usages = self.usages.clone();
        let stream = self.inner.respond_stream(messages, tools, cancel)?;
        let mapped = stream.map(move |item| {
            if let Ok(ProviderEvent::Completed(turn)) = &item {
                usages.lock().expect("usages lock").push(CapturedUsage {
                    is_summary,
                    tag: tag.clone(),
                    usage: turn.usage.clone(),
                });
            }
            item
        });
        Ok(Box::pin(mapped))
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }
}

/// A temp dir removed on drop (parallel-test safe).
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("iris-live-bench-{tag}-{nanos}"));
        std::fs::create_dir(&path).expect("create temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A no-op approval gate (the scenario is text-only, so `review` is unreachable).
struct NoToolGate;

impl ApprovalGate for NoToolGate {
    fn review<'a>(
        &'a self,
        _call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        _ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::Deny) })
    }
}

/// A no-op observer: the live harness reads usage from the recording provider,
/// not from events.
struct NoopObserver;

impl AgentObserver for NoopObserver {
    fn on_event(&self, _event: AgentEvent) -> Result<()> {
        Ok(())
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(future)
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
