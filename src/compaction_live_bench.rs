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
#[derive(Clone)]
struct CapturedUsage {
    is_summary: bool,
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
        let usages = self.usages.clone();
        let stream = self.inner.respond_stream(messages, tools, cancel)?;
        let mapped = stream.map(move |item| {
            if let Ok(ProviderEvent::Completed(turn)) = &item {
                usages.lock().expect("usages lock").push(CapturedUsage {
                    is_summary,
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
    let post = captured
        .iter()
        .filter(|c| !c.is_summary)
        .find_map(|c| c.usage.clone());
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
