//! Compaction retention-needle benchmark (ADR-0045, issue #372, slices A + B).
//!
//! ADR-0036 rule 5 made per-tool output reduction measurable; ADR-0045 extends
//! the same discipline to compaction: a long-horizon scenario that forces at
//! least one auto-compaction through the PRODUCTION seam
//! (`crate::wayland::Harness::maybe_auto_compact`), then asserts that
//! load-bearing facts present only in the covered (compacted-away) range
//! survive verbatim in the rebuilt context. Token cost is reported as a ratio
//! per summarizer arm (`provider` vs `excerpts`, ADR-0041) via
//! `bench_support::est_tokens`, with a minimum-reduction bar per arm that
//! fails the test on regression.
//!
//! Determinism: the scenario runs on the FAKE-PROVIDER lane (no live model
//! calls), with fixed-size prompts and a fixed budget, so the covered range,
//! the summaries, and every ratio are reproducible in CI.
//!
//! Slice A (above) fixes the retention-needle contract and the two base arms
//! (`provider` vs `excerpts`). Slice B now lives in this same file, starting at
//! the "Slice B" banner further down: the `provider + structured carry`
//! (ADR-0044) and `provider + carry + microcompaction` (ADR-0048) arms, the
//! report dimensions -- compaction generation (ADR-0047), covered-range size --
//! and the cache economics. Cache economics are MODELED deterministically here
//! ("modeled (prefix-divergence, estimated tokens)": the char-exact common
//! prefix of consecutive request payloads is the cache-READ mass, the divergent
//! suffix is the cache-WRITE mass); the live provider-reported `ProviderUsage`
//! cache splits that anchor the model are captured by the env-gated harness in
//! the sibling `compaction_live_bench` module (never run under the gate).

use super::*;
use crate::config::CompactionTriggerConfig;
use crate::session::{SessionLog, SessionStore};
use crate::tools::ToolState;
use crate::tools::bench_support::{
    assert_min_reduction, assert_min_reduction_tokens, assert_ratio_within,
    assert_survives_verbatim, est_ratio, est_tokens, report_header, report_row, report_row_tokens,
};
use crate::wayland::{CompactionWorkerConfig, CompactionWorkerInput, Harness, SummarizerKind};
use serde_json::json;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

// --- Retention needles: facts that exist ONLY in the covered range (u1). ---
// Each is a load-bearing detail a competent takeover would need: a task id, a
// file path, a symbol, and a prior decision. They live at the very start of the
// first prompt so the deterministic `excerpts` arm (160-char per-message
// truncation) also keeps them; the `provider` arm echoes them in its handoff.
const NEEDLE_TASK: &str = "TASK-8291";
const NEEDLE_PATH: &str = "crates/orbit/src/telemetry/sink.rs";
const NEEDLE_SYMBOL: &str = "reconcile_ledger";
const NEEDLE_DECISION: &str = "ULID-keys ADR-0044";
const NEEDLES: &[&str] = &[NEEDLE_TASK, NEEDLE_PATH, NEEDLE_SYMBOL, NEEDLE_DECISION];

// The prefix of the provider-backed summarizer's instruction
// (`SUMMARY_PROMPT`, private to `crate::wayland`). The fake provider keys on it
// to tell a summarization request from a normal turn request without reaching
// into private constants.
const SUMMARY_INSTRUCTION_PREFIX: &str = "Summarize this coding session";

// Context budget that triggers auto-compaction, and prompt sizes chosen so the
// first (large, needle-bearing) turn falls in the covered range while the
// smaller recent turns are retained. See `run_scenario` for the token math.
const BUDGET: u64 = 300;

/// A unique temp workspace/session root per call (parallel-test safe), removed
/// on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("iris-compaction-bench-{nanos}-{seq}"));
        std::fs::create_dir(&path).expect("create temp dir");
        Self { path }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Fake provider for the compaction lane. It answers two request shapes:
/// normal turns get a fixed short text; a summarization request (last message
/// begins with the summarizer instruction) gets a handoff summary DERIVED from
/// the covered range the production seam actually passed, asserting each needle
/// is present in that covered input before echoing it. No live calls -- this is
/// the CI-safe lane ADR-0045 requires.
struct CompactionFakeProvider {
    /// Shared with the scenario so it can prove the provider summarizer actually
    /// ran for the provider arm (vs. a silent fallback to excerpts).
    summary_calls: Arc<AtomicUsize>,
    /// Every request payload this fake received, in order, so the modeled cache
    /// economics (`model_cache_economics`) can diff consecutive pre-/post-
    /// compaction payloads. The provider is moved into the `Agent`, so the
    /// scenario keeps a clone of this handle to read the payloads back after the
    /// run instead of reaching into the moved provider.
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    /// Facts the fake asserts present in the covered range before echoing them
    /// in a short handoff. Non-empty for the text arms (the four load-bearing
    /// facts, retained THROUGH the summary); empty for the seeded carry arms,
    /// where retention is via the deterministic carry block (ADR-0044) and the
    /// recall reference, not the summary text -- so the fake echoes a generic
    /// short handoff and the production seam owns retention.
    echo_needles: Vec<&'static str>,
    summary_delay: Duration,
    summary_completed: Option<Arc<AtomicBool>>,
}

impl CompactionFakeProvider {
    fn new(
        summary_calls: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
        echo_needles: Vec<&'static str>,
    ) -> Self {
        Self {
            summary_calls,
            requests,
            echo_needles,
            summary_delay: Duration::ZERO,
            summary_completed: None,
        }
    }

    fn with_summary_delay_and_signal(
        summary_calls: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
        echo_needles: Vec<&'static str>,
        summary_delay: Duration,
        summary_completed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            summary_calls,
            requests,
            echo_needles,
            summary_delay,
            summary_completed: Some(summary_completed),
        }
    }

    /// Build the handoff by DERIVING it from the covered range the production
    /// seam passed to summarization (`provider_summary` sends the covered
    /// messages followed by the summary instruction, so `covered` is every
    /// message before that final instruction). Each `echo_needle` is asserted
    /// present in that covered input before it is echoed: a seam that passes the
    /// wrong covered range, drops the opener, or otherwise breaks retention
    /// fails here instead of the fake silently echoing hard-coded facts. Kept
    /// short so `estimate_tokens(framed) < original_tokens` (the wayland shrink
    /// guard) holds for the covered range.
    fn derive_handoff(&self, covered: &[Message]) -> String {
        let covered_text = covered
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for needle in &self.echo_needles {
            assert!(
                covered_text.contains(needle),
                "fake provider: needle {needle:?} absent from the covered range the seam passed \
                 to summarization; the summary cannot be derived from covered content, so \
                 retention through this arm is not actually proven"
            );
        }
        // Echo only facts confirmed present in the covered input above, as a
        // short structured handoff a takeover model could resume from. With no
        // echo needles (seeded carry arms) this is a generic short handoff.
        let mut out = String::from("Goal: resume the session. State: edits started. Key facts:");
        if self.echo_needles.is_empty() {
            out.push_str(" (see carry block and recall reference).");
        } else {
            for needle in &self.echo_needles {
                out.push(' ');
                out.push_str(needle);
                out.push(';');
            }
        }
        out.push_str(" Next: finish the wiring.");
        out
    }
}

impl ChatProvider for CompactionFakeProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        let investigator_summary = messages.last().is_some_and(|m| {
            m.content
                .starts_with("You are a read-only compaction summarizer")
        });
        let is_summary = investigator_summary
            || messages
                .last()
                .is_some_and(|m| m.content.starts_with(SUMMARY_INSTRUCTION_PREFIX));
        // Record the exact request payload this boundary carries so the modeled
        // cache economics can char-diff it against the previous request.
        self.requests
            .lock()
            .expect("requests capture lock")
            .push(CapturedRequest {
                is_summary,
                payload: serialize_request(messages),
            });
        let turn = if is_summary {
            if !self.summary_delay.is_zero() {
                std::thread::sleep(self.summary_delay);
            }
            self.summary_calls.fetch_add(1, Ordering::Relaxed);
            // Covered range = every message before the final summary
            // instruction the seam appends.
            let covered = if investigator_summary {
                messages
            } else {
                &messages[..messages.len() - 1]
            };
            let turn = AssistantTurn::text(&self.derive_handoff(covered));
            if let Some(completed) = &self.summary_completed {
                completed.store(true, Ordering::Release);
            }
            turn
        } else {
            AssistantTurn::text("ok")
        };
        Ok(Box::pin(futures::stream::once(async move {
            Ok(ProviderEvent::Completed(turn))
        })))
    }
}

// ===========================================================================
// Modeled cache economics (DoD item 1). The fake-provider lane reports no
// `ProviderUsage` cache splits, so cache read/write mass is MODELED, never
// presented as provider-reported. The model: a prompt cache serves the longest
// prefix shared with the previous request and re-bills (writes) the divergent
// suffix. So for each consecutive request pair straddling a compaction the
// char-EXACT common prefix is the expected cache-READ mass and the divergent
// suffix of the newer request is the expected cache-WRITE mass, both in
// `bench_support::est_tokens`. Labeled everywhere as
// "modeled (prefix-divergence, estimated tokens)". The env-gated live harness
// (`compaction_live_bench`) anchors this model against realized `ProviderUsage`.
// ===========================================================================

/// One request payload the fake provider received, tagged as a summarization
/// request (the covered range + summary instruction) or a normal turn request.
#[derive(Clone)]
struct CapturedRequest {
    is_summary: bool,
    payload: String,
}

/// Canonical, boundary-aware serialization of a request's messages: each
/// message is `role<US>content`, messages joined by a record separator, so the
/// char-exact common prefix of two payloads cannot span a message boundary by
/// accident. This is the string a prefix cache would key on (deterministic and
/// reproducible; the live lane keys on the provider's own serialization).
fn serialize_request(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| format!("{}\u{1f}{}", m.role.as_str(), m.content))
        .collect::<Vec<_>>()
        .join("\u{1e}")
}

/// Byte length of the char-exact common prefix of `a` and `b`. Sums whole
/// `char` widths so the returned length is always a UTF-8 boundary safe to slice
/// at.
fn common_prefix_bytes(a: &str, b: &str) -> usize {
    a.chars()
        .zip(b.chars())
        .take_while(|(ca, cb)| ca == cb)
        .map(|(ca, _)| ca.len_utf8())
        .sum()
}

/// One compaction boundary's modeled cache economics, in estimated tokens.
#[derive(Clone)]
struct CacheGen {
    /// 1-based compaction ordinal (one per summarization request observed).
    generation: u64,
    /// Expected cache-READ mass: the char-exact prefix the post-compaction
    /// request still shares with the pre-compaction request.
    read_tokens: usize,
    /// Expected cache-WRITE mass: the divergent suffix of the post-compaction
    /// request (the rewritten summary + retained tail + new turn).
    write_tokens: usize,
    /// Total estimated tokens of the pre-compaction request.
    pre_tokens: usize,
    /// Total estimated tokens of the post-compaction request
    /// (`read_tokens + write_tokens` by construction).
    post_tokens: usize,
}

/// Model the cache economics of a captured request stream. Each summarization
/// request marks a compaction boundary: the last normal request before it is
/// the pre-compaction payload, the first normal request after it is the
/// post-compaction payload. Their char-exact common prefix is the modeled
/// cache-READ mass; the divergent suffix of the post payload is the modeled
/// cache-WRITE mass. One `CacheGen` per generation, in order.
///
/// `seed_baseline` is the serialized resumed context for a seeded arm whose
/// FIRST turn already compacts (so no normal request precedes generation 1):
/// it is the pre-compaction payload the summary rewrites. `None` for the text
/// arms, where a real turn-1 normal request precedes the first compaction.
/// Only the PROVIDER-summarizer arms issue a summarization request, so only
/// they yield generations; the deterministic excerpts arm makes no provider
/// summary call and therefore has no provider-side summary-request cache
/// economics to model (stated in the report).
fn model_cache_economics(
    requests: &[CapturedRequest],
    seed_baseline: Option<&str>,
) -> Vec<CacheGen> {
    let mut gens = Vec::new();
    let mut last_normal: Option<String> = seed_baseline.map(str::to_string);
    let mut pending_pre: Option<String> = None;
    let mut awaiting_post = false;
    let mut generation = 0u64;
    for req in requests {
        if req.is_summary {
            generation += 1;
            pending_pre = last_normal.clone();
            awaiting_post = true;
        } else {
            if awaiting_post {
                if let Some(pre) = &pending_pre {
                    let post = &req.payload;
                    let split = common_prefix_bytes(pre, post);
                    gens.push(CacheGen {
                        generation,
                        read_tokens: est_tokens(&post[..split]),
                        write_tokens: est_tokens(&post[split..]),
                        pre_tokens: est_tokens(pre),
                        post_tokens: est_tokens(post),
                    });
                }
                awaiting_post = false;
            }
            last_normal = Some(req.payload.clone());
        }
    }
    gens
}

/// The label every modeled cache number carries so it is never mistaken for a
/// provider-reported split.
const MODELED_LABEL: &str = "modeled (prefix-divergence, estimated tokens)";

/// One `CompactionApplied` event, captured so slice-B dimensions (generation,
/// covered-range size, carry count, and the covered-range start id the cache
/// economics reason about) come straight from the production seam, not the
/// bench's own bookkeeping.
#[derive(Clone)]
struct CompactionRecord {
    generation: u64,
    covered_messages: usize,
    original_tokens: u64,
    summary_tokens: u64,
    context_tokens_after: u64,
    carried_paths: usize,
    covered_from: String,
}

/// Records every `CompactionApplied` event (and swallows the rest) so the
/// scenario can assert at least one auto-compaction fired through the production
/// seam and read the per-compaction dimensions the report needs.
struct CompactionCounter {
    records: RefCell<Vec<CompactionRecord>>,
}

impl CompactionCounter {
    fn new() -> Self {
        Self {
            records: RefCell::new(Vec::new()),
        }
    }

    fn count(&self) -> usize {
        self.records.borrow().len()
    }
}

impl AgentObserver for CompactionCounter {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        if let AgentEvent::CompactionApplied {
            covered_from,
            covered_messages,
            original_tokens_estimate,
            summary_tokens_estimate,
            context_tokens_after_apply,
            generation,
            carried_paths,
            ..
        } = event
        {
            self.records.borrow_mut().push(CompactionRecord {
                generation,
                covered_messages,
                original_tokens: original_tokens_estimate,
                summary_tokens: summary_tokens_estimate,
                context_tokens_after: context_tokens_after_apply,
                carried_paths,
                covered_from,
            });
        }
        Ok(())
    }
}

/// A gate that never needs to decide: the scenario is text-only (no tool
/// calls), so `review` is unreachable. Present only to satisfy the seam.
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

/// The result of one arm's run: the numbers the retention/ratio contract and
/// the report row are built from.
struct ArmResult {
    /// Concatenated content of the whole rebuilt context after compaction.
    rebuilt_context: String,
    /// The summary message the compaction inserted (the reduced form).
    summary: String,
    /// Concatenated content of the retained tail messages (everything that is
    /// not the summary), used to prove the needles do NOT linger outside the
    /// covered range.
    retained_tail: String,
    /// The original covered-range text (pre-compaction), the reduction baseline.
    covered_original: String,
    /// How many auto-compactions fired.
    compactions: usize,
    /// How many times the fake provider's summarization path ran. Zero on the
    /// excerpts arm; >= 1 on the provider arm proves it did not silently fall
    /// back to excerpts.
    summary_calls: usize,
    /// Every request payload the fake provider received, for modeled cache
    /// economics (`model_cache_economics`).
    requests: Vec<CapturedRequest>,
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(future)
}

/// Drive the long-horizon scenario through the production auto-compaction seam
/// for one summarizer arm and capture the retention/ratio inputs.
///
/// Token math (`estimate_tokens` = chars/4). The first prompt is large and
/// needle-bearing (~1600 chars, ~400 tokens); later prompts are small (~30
/// tokens); assistant replies are ~1 token. With `BUDGET = 300`, the first turn
/// alone puts the context over budget, so `maybe_auto_compact` fires once at
/// the start of turn 2 and covers the oldest coverable message -- the
/// needle-bearing first prompt (~400 tokens dwarfs the 225-token keep target,
/// so it cannot sit in the retained tail). The later small turns keep the
/// context under budget, so no second compaction fires.
fn run_scenario(summarizer: SummarizerKind) -> ArmResult {
    let workspace = TempDir::new();
    let root = TempDir::new();
    let log = SessionLog::create_in(&root.path, &workspace.path).expect("create session log");

    let summary_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::new(
        // Text arms echo the four load-bearing facts THROUGH the summary, so the
        // fake asserts them present in the covered range it received.
        CompactionFakeProvider::new(summary_calls.clone(), requests.clone(), NEEDLES.to_vec()),
        Tools::new(Vec::new()),
    );
    let mut harness = Harness::new(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        Some(BUDGET),
    );
    harness.set_summarizer(summarizer);

    // The needle-bearing opener: all needles packed into the first ~110 chars
    // so both arms retain them, then filler to make the covered range large
    // enough for a meaningful reduction bar.
    let opener = format!(
        "{NEEDLE_TASK} target {NEEDLE_PATH} fn {NEEDLE_SYMBOL} decision {NEEDLE_DECISION}. {}",
        "Context on the ledger reconciliation work and its constraints. ".repeat(24)
    );
    let covered_original = format!("{}: {opener}", Role::User.as_str());

    let counter = CompactionCounter::new();
    let gate = NoToolGate;
    let token = CancellationToken::new();

    // Three turns: the opener plus two smaller follow-ups. Compaction fires at
    // the start of turn 2 (the oversized opener persisted on turn 1 already
    // exceeds the budget, and `submit_turn` runs `maybe_auto_compact` before
    // each provider request).
    let prompts = [
        opener.as_str(),
        "Follow-up one: keep going on the wiring described above; \
         proceed with the next small step and report back briefly.",
        "Follow-up two: continue with the remaining wiring and \
         summarize the state so far in one short line.",
    ];
    for prompt in prompts {
        block_on(harness.submit_turn(prompt, &counter, &gate, &token)).expect("turn succeeds");
    }

    let messages = harness.agent.messages();
    let summary = messages
        .iter()
        .find(|m| {
            m.content.starts_with("[compacted summary")
                || m.content.starts_with("[auto-compacted summary")
        })
        .map(|m| m.content.clone())
        .expect("a compaction summary is present in the rebuilt context");
    let rebuilt_context = messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let retained_tail = messages
        .iter()
        .filter(|m| m.content != summary)
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let summary_calls = summary_calls.load(Ordering::Relaxed);

    // Arm integrity: the provider/excerpts comparison is only meaningful if
    // each arm actually used its own summarizer. Assert it here so every test
    // and the report row inherit the guard -- a provider arm that silently
    // falls back to excerpts (provider summary failed or failed to shrink)
    // would otherwise pass as an excerpts-vs-excerpts comparison.
    match summarizer {
        SummarizerKind::Provider | SummarizerKind::Subagent => {
            assert!(
                summary_calls >= 1,
                "provider arm did not invoke the provider summarizer (summary_calls=0); it \
                 silently fell back to excerpts, so provider/excerpts would not be a genuine \
                 provider-vs-excerpts comparison"
            );
            assert!(
                summary.starts_with("[compacted summary"),
                "provider arm produced an excerpts-shaped summary ({summary:?}); expected the \
                 provider marker, so the arm fell back to excerpts"
            );
        }
        SummarizerKind::Excerpts => {
            assert_eq!(
                summary_calls, 0,
                "excerpts arm unexpectedly invoked the provider summarizer ({summary_calls})"
            );
            assert!(
                summary.starts_with("[auto-compacted summary"),
                "excerpts arm produced a non-excerpts summary ({summary:?})"
            );
        }
    }

    ArmResult {
        rebuilt_context,
        summary,
        retained_tail,
        covered_original,
        compactions: counter.count(),
        summary_calls,
        requests: requests.lock().expect("requests capture lock").clone(),
    }
}

#[test]
fn scenario_forces_at_least_one_auto_compaction_on_both_arms() {
    for arm in [SummarizerKind::Excerpts, SummarizerKind::Provider] {
        let result = run_scenario(arm);
        assert!(
            result.compactions >= 1,
            "{arm:?}: expected >= 1 auto-compaction through the production seam, got {}",
            result.compactions
        );
    }
}

#[test]
fn needles_survive_verbatim_in_rebuilt_context_excerpts_arm() {
    let result = run_scenario(SummarizerKind::Excerpts);
    // Retention contract: every load-bearing fact from the covered range
    // survives verbatim in the rebuilt context (here, inside the summary).
    assert_survives_verbatim("compaction/excerpts", &result.rebuilt_context, NEEDLES);
    // And the facts existed ONLY in the covered range: none linger in the
    // retained tail, so retention is via the summary, not leftover context.
    for needle in NEEDLES {
        assert!(
            !result.retained_tail.contains(needle),
            "excerpts: needle {needle:?} leaked into the retained tail; the scenario no longer \
             proves retention through compaction"
        );
    }
}

#[test]
fn needles_survive_verbatim_in_rebuilt_context_provider_arm() {
    let result = run_scenario(SummarizerKind::Provider);
    assert_survives_verbatim("compaction/provider", &result.rebuilt_context, NEEDLES);
    for needle in NEEDLES {
        assert!(
            !result.retained_tail.contains(needle),
            "provider: needle {needle:?} leaked into the retained tail; the scenario no longer \
             proves retention through compaction"
        );
    }
}

#[test]
fn provider_arm_uses_the_provider_summarizer_not_the_excerpts_fallback() {
    // Guards the degenerate-arm failure (FINDING 2): if provider summarization
    // stops working but excerpts still carry the needles, the provider arm and
    // the provider/excerpts ratio must NOT quietly pass as excerpts-vs-excerpts.
    // The provider arm must have run the provider summarizer and produced the
    // provider-shaped marker; the excerpts arm must never call the provider.
    let provider = run_scenario(SummarizerKind::Provider);
    assert!(
        provider.summary_calls >= 1,
        "provider arm never invoked the provider summarizer; it fell back to excerpts"
    );
    assert!(
        provider.summary.starts_with("[compacted summary"),
        "provider arm did not produce the provider summary marker: {:?}",
        provider.summary
    );

    let excerpts = run_scenario(SummarizerKind::Excerpts);
    assert_eq!(
        excerpts.summary_calls, 0,
        "excerpts arm unexpectedly invoked the provider summarizer"
    );
    assert!(
        excerpts.summary.starts_with("[auto-compacted summary"),
        "excerpts arm did not produce the excerpts summary marker: {:?}",
        excerpts.summary
    );
}

#[test]
fn each_arm_clears_the_minimum_reduction_bar() {
    // Minimum-reduction bar per arm: the summary must shrink the covered range
    // by at least this much or the test fails. 60% matches the noisy-class bar
    // in the token-efficiency skill; both arms clear it comfortably today, and
    // a summarizer regression that stops compressing the covered range trips it.
    const MIN_REDUCTION_PCT: u32 = 60;
    for arm in [SummarizerKind::Excerpts, SummarizerKind::Provider] {
        let result = run_scenario(arm);
        let class = match arm {
            SummarizerKind::Excerpts => "compaction/excerpts",
            SummarizerKind::Provider | SummarizerKind::Subagent => "compaction/provider",
        };
        assert_min_reduction(
            class,
            &result.covered_original,
            &result.summary,
            MIN_REDUCTION_PCT,
        );
    }
}

#[test]
fn provider_arm_stays_within_a_bounded_ratio_of_excerpts() {
    // Cross-arm bound: the provider handoff must not balloon past the
    // deterministic excerpts floor. Both are already-reduced forms, so this is
    // a ceiling on provider/excerpts, not a raw-vs-reduced reduction. The 1.5x
    // ceiling is a regression guard, not a tight fit: it fails if the provider
    // summary grows well beyond the excerpts baseline.
    let excerpts = run_scenario(SummarizerKind::Excerpts);
    let provider = run_scenario(SummarizerKind::Provider);
    assert_ratio_within(
        "compaction/provider-vs-excerpts",
        &excerpts.summary,
        &provider.summary,
        1.5,
    );
}

/// Prints the committed report table under `docs/benchmarks/`. Not an
/// assertion; run with `--nocapture` to regenerate the snapshot. The contract
/// lives in the asserting tests above; this is the doc's source of numbers.
#[test]
fn compaction_retention_benchmark_report() {
    let excerpts = run_scenario(SummarizerKind::Excerpts);
    let provider = run_scenario(SummarizerKind::Provider);

    println!("\n{}", report_header());
    println!(
        "{}",
        report_row(
            "compaction/excerpts",
            &excerpts.covered_original,
            &excerpts.summary,
            "excerpts",
        )
    );
    println!(
        "{}",
        report_row(
            "compaction/provider",
            &provider.covered_original,
            &provider.summary,
            "provider",
        )
    );
    println!(
        "\nprovider/excerpts summary token ratio: {:.2} ({} vs {} est tokens)",
        est_ratio(&excerpts.summary, &provider.summary),
        est_tokens(&provider.summary),
        est_tokens(&excerpts.summary),
    );
    println!(
        "auto-compactions fired: excerpts={}, provider={}",
        excerpts.compactions, provider.compactions
    );
}

// ===========================================================================
// Slice B (ADR-0045, #372): the two remaining arms (`provider + carry`,
// `provider + carry + microcompaction`), the report dimensions (generation,
// covered-range size, cache economics), and the retained vs.
// recoverable-behind-reference split. Everything below runs on the same
// deterministic fake-provider lane and reads its numbers from the production
// seam's `CompactionApplied` events -- never fabricated.
// ===========================================================================

// --- Slice-B needles / paths. ---
// A path carried VERBATIM by the ADR-0044 carry (its read is never superseded,
// so microcompaction never folds it): the "retained" category.
const CARRY_PATH: &str = "crates/orbit/src/telemetry/sink.rs";
// A path whose earlier read is SUPERSEDED by a later read of the same path, so
// microcompaction folds the earlier one: the "recoverable-behind-reference"
// category. Its body carries a detail that exists ONLY in the folded read.
const FOLD_PATH: &str = "crates/orbit/src/telemetry/buffer.rs";
const FOLD_NEEDLE: &str = "FOLD-ONLY-DETAIL-4417";

/// True for either compaction summary marker (provider or excerpts).
fn is_summary_marker(content: &str) -> bool {
    content.starts_with("[compacted summary") || content.starts_with("[auto-compacted summary")
}

/// A successful `read` tool-result envelope (ADR-0021) naming a workspace-
/// relative `metadata.target`, so `derive_carry_paths` carries the path and the
/// fold engine can supersede it. Mirrors the shape the real read tool persists.
fn read_result(call: &str, target: &str, body: &str) -> Message {
    Message::tool_result(
        call,
        "read",
        &json!({
            "ok": true,
            "content": body,
            "metadata": { "target": target },
        })
        .to_string(),
    )
}

/// Count durable `fold` entries in a transcript (ADR-0048), so a seeded arm can
/// report how many spent reads microcompaction folded.
pub(super) fn fold_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .expect("read transcript")
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["type"] == "fold")
        .count()
}

/// The carry/microcompaction seed: two successful reads sit in the OLD
/// (coverable) prefix -- `CARRY_PATH`, never re-read, so it is carried verbatim
/// and never folded; and `FOLD_PATH`, whose earlier needle-bearing read a LATER
/// read of the same path supersedes. A large superseding read fills the
/// protected fold tail (`MICRO_FOLD_KEEP_TOKENS`) so the earlier `FOLD_PATH`
/// read sits before it and is foldable.
fn carry_seed() -> Vec<Message> {
    let carry_body = format!(
        "carry context for the sink. {}",
        "reconciliation detail. ".repeat(40)
    );
    let fold_body = format!("{FOLD_NEEDLE} :: {}", "spent read detail. ".repeat(40));
    // ~700 * 25 chars ~= 4375 tokens: dwarfs the 2000-token protected fold tail
    // so the earlier FOLD_PATH read is before `fold_tail_start` and folds.
    let superseding = "current buffer contents. ".repeat(700);
    vec![
        Message::user("start: read the sink before touching the buffer"),
        read_result("c-carry", CARRY_PATH, &carry_body),
        Message::assistant("ok"),
        Message::user("read the buffer"),
        read_result("c-fold-1", FOLD_PATH, &fold_body),
        Message::assistant("ok"),
        Message::user("read the buffer again"),
        read_result("c-fold-2", FOLD_PATH, &superseding),
        Message::assistant("done"),
    ]
}

/// A plain-text seed of `pairs` user/assistant turns, each user turn long enough
/// (> `MAX_EXCERPT_CHARS`) that the excerpts arm truncates it. Covering more
/// messages makes the excerpts summary grow (~160 chars/message) while the
/// provider handoff stays fixed, which is how the covered-range-SIZE dimension
/// separates the two arms.
fn text_seed(pairs: usize) -> Vec<Message> {
    let mut seed = Vec::with_capacity(pairs * 2);
    for i in 0..pairs {
        let body = format!(
            "Step {i}: {}",
            "context about the ledger reconciliation work and its constraints. ".repeat(4)
        );
        seed.push(Message::user(&body));
        seed.push(Message::assistant("ok"));
    }
    seed
}

/// The result of one seeded arm's run, read from the production seam.
struct SeededArm {
    /// Concatenated rebuilt context after the run.
    rebuilt_context: String,
    /// The (last) compaction summary body, or empty when no compaction fired
    /// (a micro-only run).
    summary_body: String,
    /// Every `CompactionApplied` event, in order (for the generation dimension).
    records: Vec<CompactionRecord>,
    /// Durable `fold` entries written (microcompaction, ADR-0048).
    folds: usize,
    /// Provider summarizer invocations (arm-integrity guard).
    summary_calls: usize,
    /// Context tokens remaining after the run: the mass the `keep_target`
    /// hysteresis rewrites to cache on the next compaction (cache-write proxy).
    post_context_tokens: u64,
    /// Every request payload the fake provider received, for modeled cache
    /// economics (`model_cache_economics`).
    requests: Vec<CapturedRequest>,
    /// The serialized resumed context: the pre-compaction baseline for a seeded
    /// arm whose first turn already compacts (see `model_cache_economics`).
    seed_baseline: String,
}

impl SeededArm {
    /// The first compaction's record (the four-arm ratio is measured on the
    /// session's first compaction, the warm-cache case).
    fn first(&self) -> &CompactionRecord {
        self.records.first().expect("at least one compaction fired")
    }
}

/// Seed a transcript, resume it through the production startup path, install the
/// chosen summarizer + microcompaction flag, and drive `prompts` forcing turns
/// so `maybe_microcompact` then `maybe_auto_compact` run at each turn boundary.
/// All numbers are read back from the seam's events and the durable transcript.
fn run_seeded(
    seed: &[Message],
    budget: u64,
    micro: bool,
    summarizer: SummarizerKind,
    echo: Vec<&'static str>,
    prompts: &[&str],
) -> SeededArm {
    run_seeded_with(
        seed,
        budget,
        micro,
        summarizer,
        echo,
        prompts,
        |_| {},
        false,
    )
}

/// [`run_seeded`] with two extra knobs for the cache-aware trigger arms
/// (issue #400 M2): `prepare` runs against the resumed harness before any
/// turn (install a cache profile, arm a selection switch), and
/// `manual_compact` drives one `compact_now` (the `/compact` seam, trigger
/// A6) before the driven prompts.
#[allow(clippy::too_many_arguments)]
fn run_seeded_with(
    seed: &[Message],
    budget: u64,
    micro: bool,
    summarizer: SummarizerKind,
    echo: Vec<&'static str>,
    prompts: &[&str],
    prepare: impl FnOnce(&mut Harness<CompactionFakeProvider>),
    manual_compact: bool,
) -> SeededArm {
    let root = TempDir::new();
    let workspace = TempDir::new();
    let mut log = SessionLog::create_in(&root.path, &workspace.path).expect("create session log");
    for message in seed {
        log.append(message).expect("append seed message");
    }
    let path = log.path().to_path_buf();
    drop(log);

    // Resume through the store so the loaded messages carry their durable entry
    // ids (the coverable prefix), exactly as the startup path does.
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

    let summary_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::resumed(
        CompactionFakeProvider::new(summary_calls.clone(), requests.clone(), echo),
        Tools::new(Vec::new()),
        stored.messages,
    );
    let mut harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        entry_ids,
        Some(budget),
    );
    harness.set_summarizer(summarizer);
    harness.set_microcompaction(micro);
    harness.set_microcompaction_watermark(budget / 2);
    prepare(&mut harness);

    let counter = CompactionCounter::new();
    let gate = NoToolGate;
    let token = CancellationToken::new();
    if manual_compact {
        block_on(harness.compact_now(&counter, &token)).expect("manual compaction succeeds");
    }
    for prompt in prompts {
        block_on(harness.submit_turn(prompt, &counter, &gate, &token)).expect("turn succeeds");
    }

    let folds = fold_count(&path);
    let messages = harness.agent.messages();
    let summary_body = messages
        .iter()
        .rev()
        .find(|m| is_summary_marker(&m.content))
        .map(|m| m.content.clone())
        .unwrap_or_default();
    let rebuilt_context = messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let post_context_tokens = harness.context_token_estimate();

    SeededArm {
        rebuilt_context,
        summary_body,
        records: counter.records.borrow().clone(),
        folds,
        summary_calls: summary_calls.load(Ordering::Relaxed),
        post_context_tokens,
        requests: requests.lock().expect("requests capture lock").clone(),
        seed_baseline: serialize_request(seed),
    }
}

/// The `provider + carry` arm: a tool-bearing covered range, microcompaction
/// OFF. The successful reads in the covered range make the ADR-0044 carry
/// non-empty, so their paths are RETAINED verbatim beside the summary.
fn run_carry_arm() -> SeededArm {
    run_seeded(
        &carry_seed(),
        300,
        false,
        SummarizerKind::Provider,
        Vec::new(),
        &["continue: proceed with the next small wiring step."],
    )
}

/// The `provider + carry + microcompaction` arm: the SAME covered range, but
/// microcompaction ON folds the superseded `FOLD_PATH` read to a stub BEFORE
/// compaction. The folded detail becomes RECOVERABLE-BEHIND-REFERENCE (the stub
/// names the path; the recall marker names the handle), never retained.
fn run_carry_micro_arm() -> SeededArm {
    run_seeded(
        &carry_seed(),
        300,
        true,
        SummarizerKind::Provider,
        Vec::new(),
        &["continue: proceed with the next small wiring step."],
    )
}

// --- The four arms measured (DoD item 1). ---

#[test]
fn four_arms_each_clear_the_minimum_reduction_bar() {
    // The noisy-class 60% bar from the token-efficiency skill; all four arms
    // clear it. Text arms measure the covered string directly; the seeded arms
    // read the seam's own before/after token estimates (the microcompaction arm
    // folds the covered range first, so there is no single verbatim string).
    const MIN_REDUCTION_PCT: u32 = 60;

    let excerpts = run_scenario(SummarizerKind::Excerpts);
    let provider = run_scenario(SummarizerKind::Provider);
    assert_min_reduction(
        "compaction/excerpts",
        &excerpts.covered_original,
        &excerpts.summary,
        MIN_REDUCTION_PCT,
    );
    assert_min_reduction(
        "compaction/provider",
        &provider.covered_original,
        &provider.summary,
        MIN_REDUCTION_PCT,
    );

    let carry = run_carry_arm();
    let micro = run_carry_micro_arm();
    assert_min_reduction_tokens(
        "compaction/provider+carry",
        carry.first().original_tokens,
        carry.first().summary_tokens,
        MIN_REDUCTION_PCT,
    );
    assert_min_reduction_tokens(
        "compaction/provider+carry+microcompaction",
        micro.first().original_tokens,
        micro.first().summary_tokens,
        MIN_REDUCTION_PCT,
    );
}

#[test]
fn seeded_arms_use_the_provider_summarizer_and_carry_paths() {
    // Arm integrity: both seeded arms must actually run the provider summarizer
    // (not fall back to excerpts) and produce a non-empty carry, or the arm is
    // not what its name claims.
    let carry = run_carry_arm();
    assert!(
        carry.summary_calls >= 1,
        "provider+carry arm did not invoke the provider summarizer"
    );
    assert!(
        carry.summary_body.starts_with("[compacted summary"),
        "provider+carry arm produced an excerpts-shaped summary: {:?}",
        carry.summary_body
    );
    assert_eq!(
        carry.folds, 0,
        "microcompaction is OFF for the provider+carry arm; no fold should be written"
    );
    assert!(
        carry.first().carried_paths >= 1,
        "provider+carry arm carried no paths; the carry retention it measures is absent"
    );

    let micro = run_carry_micro_arm();
    assert!(
        micro.summary_calls >= 1,
        "microcompaction arm did not invoke the provider summarizer"
    );
    assert_eq!(
        micro.folds, 1,
        "microcompaction arm should fold exactly the one superseded read"
    );
    assert!(
        micro.first().carried_paths >= 1,
        "microcompaction arm carried no paths; CARRY_PATH should still be carried"
    );
}

// --- Retained vs. recoverable-behind-reference (DoD item 3). ---

#[test]
fn carry_path_is_retained_verbatim_in_rebuilt_context() {
    // RETAINED category: a load-bearing path in the covered range survives
    // verbatim in the rebuilt context via the ADR-0044 carry block, on both
    // seeded arms.
    for arm in [run_carry_arm(), run_carry_micro_arm()] {
        assert_survives_verbatim(
            "compaction/carry-retained",
            &arm.rebuilt_context,
            &[CARRY_PATH],
        );
        assert!(
            arm.rebuilt_context
                .contains("[files touched or read in the compacted range]"),
            "the carry block header is missing; CARRY_PATH is not retained via the carry"
        );
    }
}

#[test]
fn folded_detail_is_recoverable_behind_a_reference_not_retained() {
    // RECOVERABLE-BEHIND-REFERENCE category: the microcompaction arm folds the
    // superseded read, so its detail is NOT in rebuilt context, but a named
    // reference that IS retained lets the model recover it -- the recall marker
    // (handle) that compaction added.
    let micro = run_carry_micro_arm();
    assert!(
        !micro.rebuilt_context.contains(FOLD_NEEDLE),
        "folded detail must not be retained verbatim in rebuilt context"
    );
    assert!(
        micro.rebuilt_context.contains("recall(handle="),
        "the recall reference (the named recovery path) must be retained verbatim"
    );
}

#[test]
fn microcompaction_fold_stub_names_the_recoverable_path() {
    // A fold-only view of recoverable-behind-reference: a HIGH budget so no
    // compaction fires, only the micro-watermark fold. The fold stub then
    // survives verbatim in rebuilt context, and it names the workspace-relative
    // path AND points at the recall tool -- the two recovery references. The
    // folded needle itself is gone.
    let micro = run_seeded(
        &carry_seed(),
        // Total context ~4.8k tokens: below this budget so NO compaction fires,
        // but above the micro-watermark (budget/2 ~ 3.5k) so the fold still runs.
        7_000,
        true,
        SummarizerKind::Provider,
        Vec::new(),
        &["note: continue reading the buffer as needed."],
    );
    assert_eq!(micro.folds, 1, "the superseded read should fold");
    assert!(
        micro.records.is_empty(),
        "no compaction should fire at this high budget (fold-only view)"
    );
    assert!(
        !micro.rebuilt_context.contains(FOLD_NEEDLE),
        "the folded read body must be gone from rebuilt context"
    );
    assert!(
        micro.rebuilt_context.contains("[folded]") && micro.rebuilt_context.contains(FOLD_PATH),
        "the fold stub must name the recoverable workspace-relative path"
    );
    assert!(
        micro.rebuilt_context.contains("recall tool"),
        "the fold stub must point at the recall recovery path"
    );
}

// --- Covered-range SIZE dimension (DoD item 2). ---

/// Run one text arm over a covered range of `pairs` turns and return its summary
/// string (empty if nothing compacted). Small ranges make provider ~= excerpts;
/// large ranges make excerpts grow while the provider handoff stays fixed.
fn text_size_summary(summarizer: SummarizerKind, pairs: usize) -> String {
    run_seeded(
        &text_seed(pairs),
        300,
        false,
        summarizer,
        Vec::new(),
        &["continue with the next step."],
    )
    .summary_body
}

#[test]
fn arms_separate_as_the_covered_range_grows() {
    // Slice-A note: a single-message covered range makes provider ~= excerpts
    // (both return one bounded form of comparable size). As the covered range
    // grows, the excerpts summary grows (~160 chars/msg) while the provider
    // handoff stays fixed, so excerpts/provider climbs well above 1. This is the
    // covered-range-SIZE dimension: the arms genuinely separate on size, not
    // just on the single-message shape slice A measured.
    let small_ratio = est_ratio(
        &run_scenario(SummarizerKind::Provider).summary,
        &run_scenario(SummarizerKind::Excerpts).summary,
    );
    let large_provider = text_size_summary(SummarizerKind::Provider, 10);
    let large_excerpts = text_size_summary(SummarizerKind::Excerpts, 10);
    assert!(
        !large_provider.is_empty() && !large_excerpts.is_empty(),
        "the 10-turn covered range must actually compact"
    );
    let large_ratio = est_ratio(&large_provider, &large_excerpts);
    assert!(
        large_ratio > small_ratio * 2.0,
        "expected the excerpts/provider ratio to climb with covered-range size \
         (single-message={small_ratio:.2}, ten-turn={large_ratio:.2})"
    );
}

// --- Compaction GENERATION dimension (DoD item 2). ---

/// Drive enough over-budget turns to force at least two compactions, so the
/// generation ordinal (ADR-0047) advances and the covered-range start id moves
/// past the prior summary.
fn run_multi_generation() -> SeededArm {
    // Plain-text turns (no huge retained read to pin the tail), so each large
    // forcing prompt refills the tail back over the budget and the next turn
    // compacts a later range -- advancing the generation ordinal cleanly.
    let big_prompt = concat!(
        "Large follow-up turn carrying enough new context that the retained tail climbs back ",
        "over the compaction budget and forces another compaction on a later range of messages, ",
        "advancing the generation ordinal. Details on the ledger reconciliation work, the sink ",
        "and buffer wiring, and the constraints that still matter for the takeover model here."
    );
    run_seeded(
        &text_seed(4),
        300,
        false,
        SummarizerKind::Provider,
        Vec::new(),
        &[big_prompt, big_prompt, big_prompt, big_prompt, big_prompt],
    )
}

#[test]
fn generation_ordinal_advances_across_compactions() {
    let run = run_multi_generation();
    assert!(
        run.records.len() >= 2,
        "expected at least two compactions to exercise the generation dimension, got {}",
        run.records.len()
    );
    // Generations are 1-based and strictly increasing in event order (ADR-0047).
    for (i, record) in run.records.iter().enumerate() {
        assert_eq!(
            record.generation,
            (i + 1) as u64,
            "generation ordinal must be the 1-based compaction count"
        );
    }
    // Cache-economics structural fact: the covered range moves past the prior
    // summary each generation, so only generation 1 starts at the live cached
    // prefix (the warm-cache case). The covered-from id therefore differs
    // between generation 1 and 2.
    assert_ne!(
        run.records[0].covered_from, run.records[1].covered_from,
        "generation 2 must cover a later range than generation 1"
    );
}

// --- Modeled cache economics (DoD item 1). ---

#[test]
fn modeled_cache_write_dominates_the_post_compaction_request() {
    // MODELED (prefix-divergence, estimated tokens): a compaction whose covered
    // range starts at the live cached prefix (generation 1, covered_from =
    // 00000000) rewrites that prefix, so the post-compaction request's char-exact
    // shared prefix with the pre-compaction request (modeled cache-READ) is
    // near-zero and the divergent suffix (modeled cache-WRITE) is the majority of
    // the request. Asserted per provider-summarizer arm. This is a MODEL, not a
    // provider-reported split; the live anchor lives in `compaction_live_bench`.
    let provider = run_scenario(SummarizerKind::Provider);
    let carry = run_carry_arm();
    let micro = run_carry_micro_arm();
    let arms: [(&str, Vec<CacheGen>); 3] = [
        ("provider", model_cache_economics(&provider.requests, None)),
        (
            "provider+carry",
            model_cache_economics(&carry.requests, Some(&carry.seed_baseline)),
        ),
        (
            "provider+carry+microcompaction",
            model_cache_economics(&micro.requests, Some(&micro.seed_baseline)),
        ),
    ];
    // A compaction always rewrites at least this much fresh mass at the modeled
    // cache-WRITE boundary; a summarizer that stopped rewriting the prefix would
    // trip it. Minimum, never an exact figure.
    const MIN_WRITE_TOKENS: usize = 50;
    for (name, gens) in &arms {
        let g1 = gens
            .first()
            .unwrap_or_else(|| panic!("{name}: no modeled generation-1 cache economics captured"));
        assert_eq!(
            g1.generation, 1,
            "{name}: first modeled generation must be 1"
        );
        // read + est_tokens split rounding: each side is div_ceil, so the pair
        // can exceed the whole request by at most one token.
        assert!(
            (g1.read_tokens + g1.write_tokens).abs_diff(g1.post_tokens) <= 1,
            "{name}: modeled cache-READ {} + cache-WRITE {} must reconstruct the {}-token post \
             request (+/- 1 for split rounding)",
            g1.read_tokens,
            g1.write_tokens,
            g1.post_tokens,
        );
        assert!(
            g1.write_tokens >= MIN_WRITE_TOKENS,
            "{name}: modeled cache-WRITE {} is below the {MIN_WRITE_TOKENS}-token bar",
            g1.write_tokens,
        );
        assert!(
            g1.write_tokens * 2 >= g1.post_tokens,
            "{name}: modeled cache-WRITE {} is not the majority of the {}-token post-compaction \
             request; a compaction starting at the live prefix should rewrite the majority",
            g1.write_tokens,
            g1.post_tokens,
        );
    }
}

#[test]
fn modeled_cache_read_warms_across_generations() {
    // MODELED (prefix-divergence, estimated tokens): after generation 1 collapses
    // the cache, later compactions retain a stable summary + tail prefix, so the
    // modeled cache-READ (char-exact shared prefix) accrues generation over
    // generation while the cache-WRITE stays bounded -- the warm-cache accrual the
    // model predicts across successive compactions.
    let run = run_multi_generation();
    let gens = model_cache_economics(&run.requests, Some(&run.seed_baseline));
    assert!(
        gens.len() >= 2,
        "expected at least two modeled generations, got {}",
        gens.len()
    );
    // Each later generation shares a strictly longer stable summary+tail prefix,
    // so adjacent cache-READ grows by at least this much. Observed deltas are
    // ~80 tokens (2 -> 84 -> 165 -> 247); a 40-token bar catches an intermediate
    // generation that stopped warming (delta collapses toward 0) without being
    // trivially true.
    const MIN_READ_GROWTH: usize = 40;
    // A compaction rewrites the summary + retained tail + new turn, which stays
    // roughly constant even as the post request grows (post climbs 405 -> 600),
    // so cache-WRITE stays bounded. Observed writes are ~350-405; a 450-token
    // cap trips if an intermediate generation stopped warming and rewrote the
    // whole (growing) prefix instead of just the divergent suffix.
    const MAX_WRITE_TOKENS: usize = 450;
    for (i, g) in gens.iter().enumerate() {
        assert_eq!(
            g.generation,
            (i + 1) as u64,
            "modeled generations must be the 1-based compaction ordinal"
        );
        // Reconstruction holds PER generation: READ + WRITE ~= post request.
        assert!(
            (g.read_tokens + g.write_tokens).abs_diff(g.post_tokens) <= 1,
            "generation {}: modeled READ {} + WRITE {} must reconstruct the {}-token post request",
            g.generation,
            g.read_tokens,
            g.write_tokens,
            g.post_tokens,
        );
        // Bounded write PER generation.
        assert!(
            g.write_tokens <= MAX_WRITE_TOKENS,
            "generation {}: modeled cache-WRITE {} exceeds the {MAX_WRITE_TOKENS}-token bound; \
             write should stay bounded as the post request grows, not rewrite the whole prefix",
            g.generation,
            g.write_tokens,
        );
    }
    // Warming is asserted across EACH ADJACENT generation pair, not just first vs
    // last: a regression in any intermediate generation trips the bar.
    for pair in gens.windows(2) {
        let (prev, next) = (&pair[0], &pair[1]);
        assert!(
            next.read_tokens >= prev.read_tokens + MIN_READ_GROWTH,
            "modeled cache-READ must grow by at least {MIN_READ_GROWTH} tokens from generation {} \
             ({} tokens) to generation {} ({} tokens) as later generations share a longer stable \
             summary+tail prefix",
            prev.generation,
            prev.read_tokens,
            next.generation,
            next.read_tokens,
        );
    }
}

// --- Background worker arms and boundary dimensions (slice 9). ---

const BACKGROUND_NEEDLE: &str = "BACKGROUND-V2-NEEDLE-41c9";

struct OneToolParent {
    calls: AtomicUsize,
}

impl ChatProvider for OneToolParent {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        // Keep issuing the delay tool across pair-closed Start-pressure
        // boundaries until the background summary has actually been applied,
        // then finish. The apply is observable because compaction replaces the
        // large (~90 KB) covered seed block with the short worker summary, so
        // the transcript the parent is handed collapses far below this
        // threshold once the apply lands. Draining until the summary is
        // genuinely applied - rather than after a fixed number of boundaries -
        // makes the mid-turn apply deterministic no matter how long a saturated
        // scheduler starves the worker OS thread between signaling summary
        // completion and delivering the result to the boundary channel (the two
        // steps run sequentially on the same worker thread, so no summarizer
        // signal can fire after the delivery). In the common case the summary
        // is drained at the first boundary and the parent finishes on its
        // second call exactly as before; the bound only guards against a
        // genuine no-apply regression looping forever. Both boundaries stay at
        // Start pressure, so the arm still measures the opportunistic mid-turn
        // apply.
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let awaiting_apply = messages.iter().map(|m| m.content.len()).sum::<usize>() > 20_000;
        let turn = if awaiting_apply && call < 16 {
            AssistantTurn {
                tool_calls: vec![ToolCall {
                    id: format!("bench-delay-{}", call + 1),
                    name: "bench_delay".to_string(),
                    arguments: json!({}),
                    thought_signature: None,
                }],
                ..AssistantTurn::default()
            }
        } else {
            AssistantTurn::text("done")
        };
        Ok(Box::pin(futures::stream::once(async move {
            Ok(ProviderEvent::Completed(turn))
        })))
    }
}

struct DelayTool {
    worker_completed: Arc<AtomicBool>,
}

impl Tool for DelayTool {
    fn name(&self) -> &str {
        "bench_delay"
    }

    fn description(&self) -> &str {
        "Wait briefly so a deterministic background benchmark can overlap work."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn execute<'a>(
        &'a self,
        _args: &'a serde_json::Value,
        _env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        Box::pin(async {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self.worker_completed.load(Ordering::Acquire) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // The provider sets `worker_completed` immediately before its result
            // reaches the background channel. Yield once more so a saturated
            // full-suite run can deliver that result before the next boundary.
            if self.worker_completed.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(ToolOutput::text("delay complete"))
        })
    }
}

#[derive(Default)]
struct TimingObserver {
    events: RefCell<Vec<(Instant, AgentEvent)>>,
}

impl AgentObserver for TimingObserver {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        self.events.borrow_mut().push((Instant::now(), event));
        Ok(())
    }
}

#[derive(Debug)]
struct BackgroundArmResult {
    blocked_ms: f64,
    original_tokens: u64,
    summary_tokens: u64,
    boundary: &'static str,
    trigger: Option<ContextPressureTier>,
    origin: CompactionOrigin,
    needle: bool,
}

fn background_seed() -> Vec<Message> {
    vec![
        Message::user(&format!(
            "{BACKGROUND_NEEDLE}. {}",
            "Large pair-closed transcript state with exact identifiers and decisions. "
                .repeat(1_350)
        )),
        Message::assistant("The background benchmark state is recorded."),
        Message::user(&format!(
            "Recent hot-tail state. {}",
            "Keep this current coding turn verbatim. ".repeat(60)
        )),
        Message::assistant("Retained."),
    ]
}

fn build_v2_benchmark_harness(
    input: CompactionWorkerInput,
) -> (Harness<OneToolParent>, TempDir, TempDir) {
    let root = TempDir::new();
    let workspace = TempDir::new();
    let mut log = SessionLog::create_in(&root.path, &workspace.path).expect("create session");
    for message in background_seed() {
        log.append(&message).expect("append seed");
    }
    let path = log.path().to_path_buf();
    drop(log);
    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .expect("list sessions")
        .into_iter()
        .find(|meta| meta.path == path)
        .expect("seed session listed");
    let stored = store.open(&meta).expect("open seed");
    let log = SessionLog::resume(&path).expect("resume seed");
    let worker_completed = Arc::new(AtomicBool::new(false));
    let agent = Agent::resumed(
        OneToolParent {
            calls: AtomicUsize::new(0),
        },
        Tools::new(vec![Box::new(DelayTool {
            worker_completed: worker_completed.clone(),
        })]),
        stored.messages,
    );
    let mut harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        stored.entry_ids,
        Some(32_768),
    );
    let initial_tokens = harness.context_token_estimate();
    harness.set_compaction_trigger(
        32_768.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: initial_tokens.saturating_sub(2) as f64 / 32_768.0,
            start: initial_tokens.saturating_sub(1) as f64 / 32_768.0,
            hard: initial_tokens.saturating_add(100) as f64 / 32_768.0,
            keep_recent_tokens: TUNED_POLICY.keep,
            hard_wait_ms: 10_000,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    harness.set_summarizer(SummarizerKind::Subagent);
    harness.set_compaction_worker(CompactionWorkerConfig {
        input,
        max_tool_roundtrips: 1,
        timeout: Duration::from_secs(2),
        instructions: String::new(),
    });
    harness.set_compaction_summarizer_factory(Arc::new(move || {
        let worker_completed = worker_completed.clone();
        Ok(Box::new(
            CompactionFakeProvider::with_summary_delay_and_signal(
                Arc::new(AtomicUsize::new(0)),
                Arc::new(Mutex::new(Vec::new())),
                vec![BACKGROUND_NEEDLE],
                Duration::from_millis(20),
                worker_completed,
            ),
        ))
    }));
    (harness, root, workspace)
}

fn run_background_arm(input: CompactionWorkerInput) -> BackgroundArmResult {
    let (mut harness, _root, _workspace) = build_v2_benchmark_harness(input);
    let observer = TimingObserver::default();
    block_on(harness.submit_turn(
        "Continue through the delayed tool once, then finish.",
        &observer,
        &NoToolGate,
        &CancellationToken::new(),
    ))
    .expect("background arm turn succeeds");
    let events = observer.events.borrow();
    let applied_index = events
        .iter()
        .position(|(_, event)| matches!(event, AgentEvent::CompactionApplied { .. }))
        .expect("background arm applies");
    let (applied_at, original_tokens, summary_tokens, origin) = match &events[applied_index] {
        (
            at,
            AgentEvent::CompactionApplied {
                original_tokens_estimate,
                summary_tokens_estimate,
                origin,
                ..
            },
        ) => (
            *at,
            *original_tokens_estimate,
            *summary_tokens_estimate,
            *origin,
        ),
        _ => unreachable!(),
    };
    let next_request = events[applied_index + 1..]
        .iter()
        .find(|(_, event)| matches!(event, AgentEvent::ProviderTurnStarted { .. }))
        .map(|(at, _)| *at)
        .expect("mid-turn apply precedes another provider request");
    let turn_complete = events
        .iter()
        .position(|(_, event)| matches!(event, AgentEvent::TurnComplete))
        .expect("turn completes");
    let provider_started_before_apply = events[..applied_index]
        .iter()
        .any(|(_, event)| matches!(event, AgentEvent::ProviderTurnStarted { .. }));
    let trigger = events.iter().find_map(|(_, event)| match event {
        AgentEvent::CompactionLifecycle {
            state: CompactionLifecycleState::Running,
            trigger_tier,
            ..
        } => *trigger_tier,
        _ => None,
    });
    BackgroundArmResult {
        blocked_ms: next_request.duration_since(applied_at).as_secs_f64() * 1_000.0,
        original_tokens,
        summary_tokens,
        boundary: if provider_started_before_apply && applied_index < turn_complete {
            "mid-turn"
        } else {
            "turn-edge"
        },
        trigger,
        origin,
        needle: harness
            .messages()
            .iter()
            .any(|message| message.content.contains(BACKGROUND_NEEDLE)),
    }
}

fn run_foreground_comparator() -> (f64, BackgroundArmResult) {
    let (mut harness, _root, _workspace) =
        build_v2_benchmark_harness(CompactionWorkerInput::Transcript);
    let observer = TimingObserver::default();
    let started = Instant::now();
    block_on(harness.compact_now(&observer, &CancellationToken::new()))
        .expect("foreground comparator compacts");
    let blocked_ms = started.elapsed().as_secs_f64() * 1_000.0;
    let events = observer.events.borrow();
    let (original_tokens, summary_tokens, origin) = events
        .iter()
        .find_map(|(_, event)| match event {
            AgentEvent::CompactionApplied {
                original_tokens_estimate,
                summary_tokens_estimate,
                origin,
                ..
            } => Some((*original_tokens_estimate, *summary_tokens_estimate, *origin)),
            _ => None,
        })
        .expect("foreground apply event");
    (
        blocked_ms,
        BackgroundArmResult {
            blocked_ms,
            original_tokens,
            summary_tokens,
            boundary: "turn-edge/manual-await",
            trigger: None,
            origin,
            needle: harness
                .messages()
                .iter()
                .any(|message| message.content.contains(BACKGROUND_NEEDLE)),
        },
    )
}

#[test]
fn background_worker_arms_preserve_needles_and_avoid_foreground_wait() {
    let transcript = run_background_arm(CompactionWorkerInput::Transcript);
    let investigator = run_background_arm(CompactionWorkerInput::Investigator);
    let (foreground_ms, foreground) = run_foreground_comparator();
    for (name, arm) in [
        ("background-transcript", &transcript),
        ("background-investigator", &investigator),
        ("foreground", &foreground),
    ] {
        assert!(arm.needle, "{name}: retention needle missing");
        assert_eq!(
            arm.origin,
            CompactionOrigin::Subagent,
            "{name}: worker fallback invalidated the arm"
        );
        assert!(
            arm.summary_tokens * 4 < arm.original_tokens,
            "{name}: covered range reduction below 75%"
        );
    }
    assert_eq!(transcript.boundary, "mid-turn");
    assert_eq!(investigator.boundary, "mid-turn");
    assert_eq!(transcript.trigger, Some(ContextPressureTier::Start));
    assert_eq!(investigator.trigger, Some(ContextPressureTier::Start));
    assert!(transcript.blocked_ms < 50.0, "{transcript:?}");
    assert!(investigator.blocked_ms < 50.0, "{investigator:?}");
    assert!(
        foreground_ms >= 15.0,
        "foreground comparator did not include the 20ms worker wait: {foreground_ms:.1}ms"
    );
}

#[test]
fn auto_compaction_v2_worker_arms_benchmark_report() {
    let transcript = run_background_arm(CompactionWorkerInput::Transcript);
    let investigator = run_background_arm(CompactionWorkerInput::Investigator);
    let (_, foreground) = run_foreground_comparator();
    println!(
        "\n== Worker arms (production seam; deterministic 20ms worker) ==\n\
         | arm | boundary | trigger | origin | main-loop blocked | covered reduction | needle |\n\
         |---|---|---|---|---:|---:|---:|"
    );
    for (name, arm) in [
        ("background-transcript", transcript),
        ("background-investigator", investigator),
        ("foreground/manual-await", foreground),
    ] {
        println!(
            "| {name} | {} | {} | {} | {:.1} ms | {:.1}% | {} |",
            arm.boundary,
            arm.trigger.map_or("manual", ContextPressureTier::as_str),
            arm.origin.as_str(),
            arm.blocked_ms,
            100.0 * (1.0 - arm.summary_tokens as f64 / arm.original_tokens as f64),
            arm.needle,
        );
    }
}

fn run_hard_dimension() -> BackgroundArmResult {
    let (mut harness, _root, _workspace) =
        build_v2_benchmark_harness(CompactionWorkerInput::Transcript);
    harness.set_compaction_trigger(
        32_768.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: 0.30,
            start: 0.40,
            hard: 0.50,
            keep_recent_tokens: 8_000,
            hard_wait_ms: 0,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    let observer = TimingObserver::default();
    block_on(harness.submit_turn(
        "Continue through the hard-pressure benchmark.",
        &observer,
        &NoToolGate,
        &CancellationToken::new(),
    ))
    .expect("hard dimension turn succeeds");
    let events = observer.events.borrow();
    let (original_tokens, summary_tokens, origin) = events
        .iter()
        .find_map(|(_, event)| match event {
            AgentEvent::CompactionApplied {
                original_tokens_estimate,
                summary_tokens_estimate,
                origin,
                ..
            } => Some((*original_tokens_estimate, *summary_tokens_estimate, *origin)),
            _ => None,
        })
        .expect("hard dimension applies");
    BackgroundArmResult {
        blocked_ms: 0.0,
        original_tokens,
        summary_tokens,
        boundary: "turn-edge",
        trigger: Some(ContextPressureTier::Hard),
        origin,
        needle: harness
            .messages()
            .iter()
            .any(|message| message.content.contains(BACKGROUND_NEEDLE)),
    }
}

struct OverflowOnceParent {
    calls: AtomicUsize,
}

impl ChatProvider for OverflowOnceParent {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        let turn = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            AssistantTurn {
                completion_reason: Some(CompletionReason::ContextWindowExceeded),
                ..AssistantTurn::default()
            }
        } else {
            AssistantTurn::text("recovered")
        };
        Ok(Box::pin(futures::stream::once(async move {
            Ok(ProviderEvent::Completed(turn))
        })))
    }
}

fn run_reactive_dimension() -> BackgroundArmResult {
    let root = TempDir::new();
    let workspace = TempDir::new();
    let mut log = SessionLog::create_in(&root.path, &workspace.path).expect("create reactive log");
    for message in [
        Message::user(&format!(
            "{BACKGROUND_NEEDLE}. {}",
            "Reactive seed context with exact state. ".repeat(850)
        )),
        Message::assistant("recorded"),
        Message::user("small retained tail"),
        Message::assistant("retained"),
    ] {
        log.append(&message).expect("append reactive seed");
    }
    let path = log.path().to_path_buf();
    drop(log);
    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .expect("list reactive session")
        .into_iter()
        .find(|meta| meta.path == path)
        .expect("reactive session listed");
    let stored = store.open(&meta).expect("open reactive seed");
    let log = SessionLog::resume(&path).expect("resume reactive seed");
    let agent = Agent::resumed(
        OverflowOnceParent {
            calls: AtomicUsize::new(0),
        },
        Tools::new(Vec::new()),
        stored.messages,
    );
    let mut harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        stored.entry_ids,
        Some(32_768),
    );
    harness.set_compaction_trigger(
        32_768.into(),
        CompactionTriggerConfig {
            enabled: true,
            warn: TUNED_POLICY.warn,
            start: TUNED_POLICY.start,
            hard: TUNED_POLICY.hard,
            keep_recent_tokens: TUNED_POLICY.keep,
            hard_wait_ms: 10_000,
            max_consecutive_failures: 3,
            reactive: true,
        },
    );
    harness.set_summarizer(SummarizerKind::Excerpts);
    let observer = TimingObserver::default();
    block_on(harness.submit_turn(
        "Exercise one classified overflow and recover.",
        &observer,
        &NoToolGate,
        &CancellationToken::new(),
    ))
    .expect("reactive dimension turn succeeds");
    let events = observer.events.borrow();
    let (original_tokens, summary_tokens, origin) = events
        .iter()
        .find_map(|(_, event)| match event {
            AgentEvent::CompactionApplied {
                original_tokens_estimate,
                summary_tokens_estimate,
                origin,
                ..
            } => Some((*original_tokens_estimate, *summary_tokens_estimate, *origin)),
            _ => None,
        })
        .expect("reactive dimension applies");
    BackgroundArmResult {
        blocked_ms: 0.0,
        original_tokens,
        summary_tokens,
        boundary: "reactive-resend",
        trigger: None,
        origin,
        needle: harness
            .messages()
            .iter()
            .any(|message| message.content.contains(BACKGROUND_NEEDLE)),
    }
}

#[test]
fn hard_and_reactive_dimensions_use_deterministic_parent_owned_apply() {
    let hard = run_hard_dimension();
    let reactive = run_reactive_dimension();
    assert_eq!(hard.trigger, Some(ContextPressureTier::Hard));
    assert_eq!(hard.boundary, "turn-edge");
    assert_eq!(reactive.boundary, "reactive-resend");
    for arm in [hard, reactive] {
        assert_eq!(arm.origin, CompactionOrigin::Excerpts);
        assert!(arm.needle);
        assert!(arm.summary_tokens < arm.original_tokens);
    }
}

#[test]
fn auto_compaction_v2_trigger_boundary_benchmark_report() {
    let start = run_background_arm(CompactionWorkerInput::Transcript);
    let hard = run_hard_dimension();
    let reactive = run_reactive_dimension();
    println!(
        "\n== Trigger and boundary dimensions ==\n\
         | trigger | boundary | origin | covered reduction | needle |\n\
         |---|---|---|---:|---:|"
    );
    for (trigger, arm) in [("start", start), ("hard", hard), ("reactive", reactive)] {
        println!(
            "| {trigger} | {} | {} | {:.1}% | {} |",
            arm.boundary,
            arm.origin.as_str(),
            100.0 * (1.0 - arm.summary_tokens as f64 / arm.original_tokens as f64),
            arm.needle,
        );
    }
}

// --- Focus-instruction retention needle (slice 9). ---

const FOCUS_NEEDLE: &str = "FOCUS-NEEDLE-a91d: preserve allocator_epoch=73";

struct FocusAwareProvider;

impl ChatProvider for FocusAwareProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        let request = messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let focused = messages
            .last()
            .is_some_and(|message| message.content.contains(FOCUS_NEEDLE));
        let summary = if focused && request.contains(FOCUS_NEEDLE) {
            format!("Goal: continue. Key facts: {FOCUS_NEEDLE}. Next: proceed.")
        } else {
            "Goal: continue. State: generic handoff. Next: proceed.".to_string()
        };
        Ok(Box::pin(futures::stream::once(async move {
            Ok(ProviderEvent::Completed(AssistantTurn::text(&summary)))
        })))
    }
}

fn run_focus_trial(focused: bool) -> bool {
    let root = TempDir::new();
    let workspace = TempDir::new();
    let mut log = SessionLog::create_in(&root.path, &workspace.path).expect("create focus log");
    for message in [
        Message::user(&format!(
            "{FOCUS_NEEDLE}. {}",
            "Older coding state that must be summarized deliberately. ".repeat(100)
        )),
        Message::assistant("recorded"),
        Message::user("recent hot tail"),
        Message::assistant("retained"),
    ] {
        log.append(&message).expect("append focus seed");
    }
    let path = log.path().to_path_buf();
    drop(log);
    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .expect("list focus session")
        .into_iter()
        .find(|meta| meta.path == path)
        .expect("focus session listed");
    let stored = store.open(&meta).expect("open focus session");
    let log = SessionLog::resume(&path).expect("resume focus session");
    let agent = Agent::resumed(FocusAwareProvider, Tools::new(Vec::new()), stored.messages);
    let mut harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(log),
        stored.entry_ids,
        Some(32_768),
    );
    harness.set_summarizer(SummarizerKind::Subagent);
    harness.set_compaction_worker(CompactionWorkerConfig {
        input: CompactionWorkerInput::Transcript,
        max_tool_roundtrips: 1,
        timeout: Duration::from_secs(2),
        instructions: if focused {
            format!("Prioritize this exact fact: {FOCUS_NEEDLE}")
        } else {
            "Preserve the general task state.".to_string()
        },
    });
    harness.set_compaction_summarizer_factory(Arc::new(|| Ok(Box::new(FocusAwareProvider))));
    block_on(harness.compact_now(&CompactionCounter::new(), &CancellationToken::new()))
        .expect("focus trial compacts");
    harness
        .messages()
        .iter()
        .any(|message| message.content.contains(FOCUS_NEEDLE))
}

fn focus_retention_rates() -> (usize, usize, usize) {
    let trials = 5;
    let control = (0..trials).filter(|_| run_focus_trial(false)).count();
    let focused = (0..trials).filter(|_| run_focus_trial(true)).count();
    (control, focused, trials)
}

#[test]
fn focus_instruction_improves_needle_retention_rate() {
    let (control, focused, trials) = focus_retention_rates();
    assert_eq!(control, 0, "control unexpectedly retained focus-only fact");
    assert_eq!(
        focused, trials,
        "focused arm must retain every planted fact"
    );
    assert!(focused > control);
}

#[test]
fn auto_compaction_v2_focus_benchmark_report() {
    let (control, focused, trials) = focus_retention_rates();
    println!(
        "\n== Focus instruction retention ==\n\
         | arm | retained | trials | rate |\n\
         |---|---:|---:|---:|\n\
         | control | {control} | {trials} | {:.0}% |\n\
         | focused | {focused} | {trials} | {:.0}% |",
        100.0 * control as f64 / trials as f64,
        100.0 * focused as f64 / trials as f64,
    );
}

// --- Trigger-v2 default tuning (slice 9). ---

const POLICY_NEEDLE: &str = "POLICY-NEEDLE-7f3a9 --enable-zeta";
const RECALL_LOOP_HIT: &str = "RECALL-LOOP-HIT-22b7 allocator_epoch=73";

#[derive(Clone, Copy)]
struct PolicyCandidate {
    name: &'static str,
    warn: f64,
    start: f64,
    hard: f64,
    keep: u64,
}

const CURRENT_POLICY: PolicyCandidate = PolicyCandidate {
    name: "current-0.55/0.65/0.85-keep20k",
    warn: 0.55,
    start: 0.65,
    hard: 0.85,
    keep: 20_000,
};

const TUNED_POLICY: PolicyCandidate = PolicyCandidate {
    name: "candidate-0.60/0.72/0.90-keep8k",
    warn: 0.60,
    start: 0.72,
    hard: 0.90,
    keep: 8_000,
};

fn policy_seed() -> Vec<Message> {
    let mut seed = (0..30)
        .flat_map(|turn| {
            let prefix = if turn == 0 {
                format!("{POLICY_NEEDLE}. ")
            } else {
                format!("Policy seed turn {turn}. ")
            };
            [
                Message::user(&format!(
                    "{prefix}{}",
                    "Pair-closed coding context with exact decisions, paths, and current state. "
                        .repeat(32)
                )),
                Message::assistant("recorded"),
            ]
        })
        .collect::<Vec<_>>();
    seed.extend([
        Message::assistant_tool_call(&ToolCall {
            id: "policy-recall-1".to_string(),
            name: "recall".to_string(),
            arguments: json!({ "pattern": "allocator_epoch" }),
            thought_signature: None,
        }),
        Message::tool_result(
            "policy-recall-1",
            "recall",
            &format!(
                "{RECALL_LOOP_HIT}. {}",
                "Specific recalled detail re-inflated for one decision. ".repeat(80)
            ),
        ),
        Message::assistant("The specific recalled hit is now understood."),
    ]);
    seed
}

fn run_policy_candidate(policy: PolicyCandidate) -> SeededArm {
    let prompts = (0..60)
        .map(|turn| {
            format!(
                "Policy growth turn {turn}. {}",
                "Continue the coding task while preserving exact identifiers and decisions. "
                    .repeat(48)
            )
        })
        .collect::<Vec<_>>();
    let prompt_refs = prompts.iter().map(String::as_str).collect::<Vec<_>>();
    run_seeded_with(
        &policy_seed(),
        32_768,
        false,
        SummarizerKind::Excerpts,
        Vec::new(),
        &prompt_refs,
        |harness| {
            harness.set_compaction_trigger(
                32_768.into(),
                CompactionTriggerConfig {
                    enabled: true,
                    warn: policy.warn,
                    start: policy.start,
                    hard: policy.hard,
                    keep_recent_tokens: policy.keep,
                    hard_wait_ms: 10_000,
                    max_consecutive_failures: 3,
                    reactive: true,
                },
            );
        },
        false,
    )
}

fn record_total_reduction(record: &CompactionRecord) -> f64 {
    let reclaimed = record.original_tokens.saturating_sub(record.summary_tokens);
    let before = record.context_tokens_after.saturating_add(reclaimed);
    if before == 0 {
        0.0
    } else {
        reclaimed as f64 / before as f64
    }
}

fn average_total_reduction(run: &SeededArm) -> f64 {
    run.records.iter().map(record_total_reduction).sum::<f64>() / run.records.len().max(1) as f64
}

#[test]
fn tuned_policy_reclaims_more_total_context_without_more_generations() {
    let current = run_policy_candidate(CURRENT_POLICY);
    let tuned = run_policy_candidate(TUNED_POLICY);
    assert!(
        tuned.records.len() >= 3,
        "long-horizon candidate must exercise at least three generations"
    );
    assert!(
        tuned.records.len() <= current.records.len(),
        "candidate generations {} must not exceed current {}",
        tuned.records.len(),
        current.records.len()
    );
    assert!(
        average_total_reduction(&tuned) > average_total_reduction(&current),
        "candidate average total reduction {:.3} must beat current {:.3}",
        average_total_reduction(&tuned),
        average_total_reduction(&current)
    );
    for (name, run) in [(CURRENT_POLICY.name, &current), (TUNED_POLICY.name, &tuned)] {
        assert!(run.rebuilt_context.contains(POLICY_NEEDLE), "{name}");
        assert!(
            run.rebuilt_context.contains(RECALL_LOOP_HIT),
            "{name}: the specific recall-loop hit must survive repeated compaction"
        );
        assert!(
            run.rebuilt_context.matches("recall(handle=\"").count() >= run.records.len(),
            "{name}: every generation must retain a recall marker"
        );
    }
}

#[test]
fn auto_compaction_v2_tuning_benchmark_report() {
    println!(
        "\n== Trigger-v2 tuning (production seam; estimated message tokens) ==\n\
         | policy | generations | avg total reduction | shallowest total reduction | max post/start | needle | recall markers |\n\
         |---|---:|---:|---:|---:|---:|---:|"
    );
    for policy in [CURRENT_POLICY, TUNED_POLICY] {
        let run = run_policy_candidate(policy);
        let shallowest = run
            .records
            .iter()
            .map(record_total_reduction)
            .fold(1.0f64, f64::min);
        let max_post = run
            .records
            .iter()
            .map(|record| record.context_tokens_after)
            .max()
            .unwrap_or(0);
        let start = (32_768.0 * policy.start).floor() as u64;
        println!(
            "| {} | {} | {:.1}% | {:.1}% | {}/{} ({:.1}%) | {} | {}/{} |",
            policy.name,
            run.records.len(),
            average_total_reduction(&run) * 100.0,
            shallowest * 100.0,
            max_post,
            start,
            max_post as f64 / start as f64 * 100.0,
            run.rebuilt_context.contains(POLICY_NEEDLE),
            run.rebuilt_context.matches("recall(handle=\"").count(),
            run.records.len(),
        );
    }
}

/// Prints the slice-B report tables under `docs/benchmarks/`. Not an assertion;
/// run with `--nocapture` to regenerate the snapshot. The contract lives in the
/// asserting tests above.
#[test]
fn compaction_slice_b_benchmark_report() {
    let excerpts = run_scenario(SummarizerKind::Excerpts);
    let provider = run_scenario(SummarizerKind::Provider);
    let carry = run_carry_arm();
    let micro = run_carry_micro_arm();

    println!("\n== Four arms (first compaction, warm-cache case) ==");
    println!("{}", report_header());
    println!(
        "{}",
        report_row(
            "compaction/excerpts",
            &excerpts.covered_original,
            &excerpts.summary,
            "excerpts",
        )
    );
    println!(
        "{}",
        report_row(
            "compaction/provider",
            &provider.covered_original,
            &provider.summary,
            "provider",
        )
    );
    println!(
        "{}",
        report_row_tokens(
            "compaction/provider+carry",
            carry.first().original_tokens,
            carry.first().summary_tokens,
            "provider+carry",
        )
    );
    println!(
        "{}",
        report_row_tokens(
            "compaction/provider+carry+microcompaction",
            micro.first().original_tokens,
            micro.first().summary_tokens,
            "provider+carry+microcompaction",
        )
    );
    println!(
        "\ncarried paths: provider+carry={}, microcompaction={}; folds: microcompaction={}",
        carry.first().carried_paths,
        micro.first().carried_paths,
        micro.folds,
    );

    println!("\n== Covered-range SIZE (excerpts/provider summary token ratio) ==");
    println!("| covered range | provider est tokens | excerpts est tokens | ratio |");
    println!("|---|---|---|---|");
    println!(
        "| single large message | {} | {} | {:.2} |",
        est_tokens(&provider.summary),
        est_tokens(&excerpts.summary),
        est_ratio(&provider.summary, &excerpts.summary),
    );
    for pairs in [6usize, 10] {
        let p = text_size_summary(SummarizerKind::Provider, pairs);
        let e = text_size_summary(SummarizerKind::Excerpts, pairs);
        println!(
            "| {pairs} text turns | {} | {} | {:.2} |",
            est_tokens(&p),
            est_tokens(&e),
            est_ratio(&p, &e),
        );
    }

    println!("\n== Compaction GENERATION (ADR-0047) ==");
    let generations = run_multi_generation();
    println!(
        "| generation | covered msgs | before est tok | after est tok | reduction | carried |"
    );
    println!("|---|---|---|---|---|---|");
    for record in &generations.records {
        println!(
            "| {} | {} | {} | {} | {:.0}% | {} |",
            record.generation,
            record.covered_messages,
            record.original_tokens,
            record.summary_tokens,
            100.0 * (1.0 - record.summary_tokens as f64 / record.original_tokens.max(1) as f64),
            record.carried_paths,
        );
    }

    println!("\n== Cache economics -- {MODELED_LABEL} ==");
    println!(
        "| arm | generation | pre req tok | post req tok | cache-READ (shared prefix) | cache-WRITE (divergent suffix) |"
    );
    println!("|---|---|---|---|---|---|");
    let modeled_arms: [(&str, Vec<CacheGen>); 4] = [
        ("provider", model_cache_economics(&provider.requests, None)),
        (
            "provider+carry",
            model_cache_economics(&carry.requests, Some(&carry.seed_baseline)),
        ),
        (
            "provider+carry+microcompaction",
            model_cache_economics(&micro.requests, Some(&micro.seed_baseline)),
        ),
        (
            "provider (multi-generation)",
            model_cache_economics(&generations.requests, Some(&generations.seed_baseline)),
        ),
    ];
    for (arm, gens) in &modeled_arms {
        for g in gens {
            println!(
                "| {arm} | {} | {} | {} | {} | {} |",
                g.generation, g.pre_tokens, g.post_tokens, g.read_tokens, g.write_tokens,
            );
        }
    }
    println!(
        "\nMODEL: a prefix cache serves the char-exact prefix a request still shares with the \
         previous request (cache-READ) and re-bills the divergent suffix (cache-WRITE). Compaction \
         rewrites the prefix, so the post-compaction cache-WRITE mass is the amplification the \
         summary buys back. These are {MODELED_LABEL}, NOT provider-reported."
    );
    println!(
        "NOTE: the deterministic excerpts arm makes no provider summarization call, so it issues \
         no summary request and has no provider-side summary-request cache economics to model; its \
         compaction is provider-invisible."
    );
    println!(
        "structural cross-check -- post-compaction retained-tail tokens (keep_target rewrite mass, \
         a coarse cache-write proxy): {}",
        micro.post_context_tokens,
    );
    println!(
        "generation 1 covered-from id: {} (starts at the live cached prefix); generation 2 \
         covered-from id: {} (starts AFTER the generation-1 summary -> a later range).",
        generations
            .records
            .first()
            .map(|r| r.covered_from.as_str())
            .unwrap_or("-"),
        generations
            .records
            .get(1)
            .map(|r| r.covered_from.as_str())
            .unwrap_or("-"),
    );

    println!("\n== Provider asymmetry (what a LIVE lane would report) ==");
    println!(
        "Anthropic Messages (Claude Code OAuth): reports cache_read_input_tokens AND \
         cache_write_input_tokens, plus the cache_creation 5m/1h tier split -- so both the modeled \
         cache-READ and cache-WRITE masses above have a directly realized counterpart."
    );
    println!(
        "Codex Responses: reports cache_read_input_tokens only; cache_write_input_tokens is \
         hardcoded 0 and cache_creation is None (openai_codex_responses.rs:854), a PROVIDER \
         limitation. Its cache-WRITE column is therefore a DERIVED fresh-input amplification: \
         input_tokens - cached_tokens on the first post-compaction request vs the pre-compaction \
         baseline. The modeled divergent-suffix mass is the deterministic stand-in for that derived \
         amplification."
    );
}

// --- Fold-flush cost (issue #400): what a fold-ONLY prefix break pays. ---
//
// The slice-B microcompaction arm folds inside a range compaction covers in
// the same boundary, so the fold's own cache break is masked by the
// compaction's. These arms isolate it: a budget high enough that compaction
// NEVER fires, so the micro-watermark flush is the only transcript rewrite,
// and its price and payback are measured alone.

/// Stated Anthropic 5-minute-tier pricing ratios, used ONLY to illustrate the
/// warm-cache break-even horizon in the report: cache writes bill at 1.25x
/// base input, cache reads at 0.10x. Published-pricing assumptions, not
/// measurements; every token mass they multiply is modeled.
const PRICE_WRITE_5M: f64 = 1.25;
const PRICE_READ: f64 = 0.10;

/// `(read, write)` est-token split of one request payload against its
/// predecessor: the char-exact shared prefix models the cache-READ mass, the
/// divergent suffix the cache-WRITE mass. Boundary-free counterpart of
/// `model_cache_economics` for runs with no summarization request.
fn diff_payloads(pre: &str, post: &str) -> (usize, usize) {
    let split = common_prefix_bytes(pre, post);
    (est_tokens(&post[..split]), est_tokens(&post[split..]))
}

/// Budget for a fold-ONLY run over `seed`: at 1.5x the seed estimate the
/// micro-watermark (budget/2 = 0.75x seed) sits below the seed, so the fold
/// pass flushes at the very first turn boundary, while the budget stays above
/// the running total so compaction never fires.
fn fold_only_budget(seed: &[Message]) -> u64 {
    let est: u64 = seed.iter().map(|m| est_tokens(&m.content) as u64).sum();
    est + est / 2
}

/// A fold-only arm (`micro` on) or its byte-identical control (`micro` off):
/// the carry seed under a compaction-proof budget, driven two turns so turn 1
/// carries the flush boundary and turn 2 measures the steady-state request.
fn run_fold_only(micro: bool) -> SeededArm {
    let seed = carry_seed();
    let budget = fold_only_budget(&seed);
    run_seeded(
        &seed,
        budget,
        micro,
        SummarizerKind::Provider,
        Vec::new(),
        &[
            "continue: proceed with the next small wiring step.",
            "then: confirm the buffer state briefly.",
        ],
    )
}

#[test]
fn fold_only_flush_folds_without_compacting() {
    // Integrity gate for the isolation: the arm folds exactly the superseded
    // read and neither run ever compacts or calls the summarizer, so every
    // byte of divergence measured below is the fold flush and nothing else.
    let arm = run_fold_only(true);
    let control = run_fold_only(false);
    assert_eq!(arm.folds, 1, "arm must fold exactly the superseded read");
    assert!(arm.records.is_empty(), "arm must not compact");
    assert_eq!(arm.summary_calls, 0, "arm must not call the summarizer");
    assert_eq!(control.folds, 0, "control must not fold");
    assert!(control.records.is_empty(), "control must not compact");
    assert_eq!(arm.requests.len(), 2, "one request per driven turn");
    assert_eq!(control.requests.len(), 2, "one request per driven turn");
    assert!(arm.requests.iter().all(|r| !r.is_summary));
}

#[test]
fn modeled_marginal_cost_of_a_fold_only_flush() {
    let arm = run_fold_only(true);
    let control = run_fold_only(false);
    // Turn 1 carries the flush boundary. Same seed baseline, same prompt: the
    // only difference between the two turn-1 payloads is the fold stub
    // replacing the superseded read mid-transcript.
    let (read_arm, write_arm) = diff_payloads(&arm.seed_baseline, &arm.requests[0].payload);
    let (read_ctrl, write_ctrl) =
        diff_payloads(&control.seed_baseline, &control.requests[0].payload);
    // Control is append-only: its divergent suffix is just the new user turn.
    assert!(
        write_ctrl < 100,
        "control turn-1 divergence must be the appended prompt only, got {write_ctrl}"
    );
    // The flush breaks the prefix at the folded read; everything after it
    // re-bills. The ~4400-token superseding read sits after the fold point, so
    // the marginal write dwarfs the fold's own saving.
    let marginal = write_arm.saturating_sub(write_ctrl);
    assert!(
        marginal >= 1_000,
        "fold-only flush must re-bill the suffix below the fold point \
         (expected >= 1000 modeled write tokens, got {marginal})"
    );
    assert!(
        read_arm < read_ctrl,
        "the arm's shared prefix must shrink to the fold point \
         (arm READ {read_arm} vs control READ {read_ctrl})"
    );
}

#[test]
fn fold_only_flush_shrinks_every_subsequent_request() {
    let arm = run_fold_only(true);
    let control = run_fold_only(false);
    // Steady state (turn 2): the arm's request is smaller by the folded body
    // minus the stub, on this and every subsequent request.
    let arm_t2 = est_tokens(&arm.requests[1].payload);
    let ctrl_t2 = est_tokens(&control.requests[1].payload);
    let saving = ctrl_t2.saturating_sub(arm_t2);
    assert!(
        saving >= 100,
        "fold must shrink the steady-state request by at least the folded body \
         minus the stub, got {saving}"
    );
    // The stub (the recovery affordance) survives verbatim in steady state.
    assert_survives_verbatim(
        "fold-only steady state",
        &arm.requests[1].payload,
        &["[folded]", FOLD_PATH],
    );
    // And the break is ONE-TIME: after the flush the arm is append-only again.
    let (_, arm_t1_to_t2_write) = diff_payloads(&arm.requests[0].payload, &arm.requests[1].payload);
    assert!(
        arm_t1_to_t2_write < 100,
        "post-flush turns must be append-only (divergence {arm_t1_to_t2_write})"
    );
}

#[test]
fn same_boundary_fold_flush_adds_no_marginal_write() {
    // Piggyback case (#400 trigger 1): when the flush lands on the same
    // boundary as a compaction, the compaction rewrites the prefix anyway, so
    // the fold adds no marginal cache-WRITE -- it can only shrink the rewrite.
    let carry = run_carry_arm();
    let micro = run_carry_micro_arm();
    let carry_gen = model_cache_economics(&carry.requests, Some(&carry.seed_baseline));
    let micro_gen = model_cache_economics(&micro.requests, Some(&micro.seed_baseline));
    let c = carry_gen.first().expect("carry arm generation 1");
    let m = micro_gen.first().expect("micro arm generation 1");
    assert!(
        m.write_tokens <= c.write_tokens,
        "same-boundary fold must not increase the post-compaction write \
         (micro {} > carry-only {})",
        m.write_tokens,
        c.write_tokens
    );
}

/// Prints the fold-flush cost tables for
/// `docs/benchmarks/issue-400-fold-flush-cost.md`. Regenerate with:
/// `cargo test fold_flush_cost_benchmark_report -- --nocapture`
#[test]
fn fold_flush_cost_benchmark_report() {
    let arm = run_fold_only(true);
    let control = run_fold_only(false);
    let (read_arm, write_arm) = diff_payloads(&arm.seed_baseline, &arm.requests[0].payload);
    let (read_ctrl, write_ctrl) =
        diff_payloads(&control.seed_baseline, &control.requests[0].payload);
    let arm_t2 = est_tokens(&arm.requests[1].payload);
    let ctrl_t2 = est_tokens(&control.requests[1].payload);
    let marginal = write_arm.saturating_sub(write_ctrl);
    let saving = ctrl_t2.saturating_sub(arm_t2);

    println!("\n== Fold-ONLY flush at the micro-watermark -- {MODELED_LABEL} ==");
    println!("| run | turn-1 cache-READ | turn-1 cache-WRITE | turn-2 request tok | folds |");
    println!("|---|---|---|---|---|");
    println!(
        "| control (micro off) | {read_ctrl} | {write_ctrl} | {ctrl_t2} | {} |",
        control.folds
    );
    println!(
        "| fold-only arm (micro on) | {read_arm} | {write_arm} | {arm_t2} | {} |",
        arm.folds
    );
    println!("\nmarginal flush cost (arm - control turn-1 WRITE): {marginal} modeled tokens");
    println!("steady-state saving per subsequent request: {saving} modeled tokens");
    let breakeven =
        (marginal as f64 * (PRICE_WRITE_5M - PRICE_READ)) / (saving as f64 * PRICE_READ);
    println!(
        "WARM-cache break-even under stated Anthropic 5m pricing ratios \
         (write {PRICE_WRITE_5M}x, read {PRICE_READ}x base input): ~{:.0} turns",
        breakeven.ceil()
    );
    println!(
        "COLD-cache case (idle past TTL, cold resume): the suffix re-bills regardless, so the \
         flush is free and the saving is immediate (#400 trigger 3)."
    );

    println!("\n== Same-boundary flush (piggyback on compaction, #400 trigger 1) ==");
    let carry = run_carry_arm();
    let micro = run_carry_micro_arm();
    let carry_gen = model_cache_economics(&carry.requests, Some(&carry.seed_baseline));
    let micro_gen = model_cache_economics(&micro.requests, Some(&micro.seed_baseline));
    println!("| arm | generation-1 post req tok | cache-WRITE (divergent suffix) |");
    println!("|---|---|---|");
    if let Some(c) = carry_gen.first() {
        println!(
            "| provider+carry (no folds) | {} | {} |",
            c.post_tokens, c.write_tokens
        );
    }
    if let Some(m) = micro_gen.first() {
        println!(
            "| provider+carry+microcompaction | {} | {} |",
            m.post_tokens, m.write_tokens
        );
    }
    println!(
        "\nSame boundary as a compaction, the fold's marginal cache-WRITE is zero or negative: \
         the compaction rewrites the prefix anyway and the fold only shrinks the rewrite."
    );
}

// ===========================================================================
// Cache-aware trigger arms (issue #400 M2): each Class A trigger's flush is
// measured against its own control on the same deterministic fake-provider
// lane. The claim per arm is "marginal cache-WRITE ~ 0": under a full break
// (A2 model switch), an expired cache (A4 cold resume), or an uncacheable
// prefix (A5 below the minimum), the request mass re-bills (or was never
// cached) regardless of folding -- the fold can only SHRINK what is billed.
// A6 rides a manual compaction exactly like A1 rides an automatic one.
// ===========================================================================

/// A compaction-proof budget: the watermark (budget/2 = 2x seed) stays above
/// the running total, so neither compaction (A1) nor the watermark backstop
/// (C) can fire and the arm's flush is attributable to its trigger alone.
fn break_arm_budget(seed: &[Message]) -> u64 {
    let est: u64 = seed.iter().map(|m| est_tokens(&m.content) as u64).sum();
    est.saturating_mul(4)
}

/// One Class-A break arm over the carry seed: `prepare` arms the trigger on
/// the resumed harness, two driven turns measure the flush boundary (turn 1)
/// and the steady state (turn 2).
fn run_break_arm(
    micro: bool,
    prepare: impl FnOnce(&mut Harness<CompactionFakeProvider>),
) -> SeededArm {
    let seed = carry_seed();
    let budget = break_arm_budget(&seed);
    run_seeded_with(
        &seed,
        budget,
        micro,
        SummarizerKind::Provider,
        Vec::new(),
        &[
            "continue: proceed with the next small wiring step.",
            "then: confirm the buffer state briefly.",
        ],
        prepare,
        false,
    )
}

/// `(arm, control, transcript fold tags)` for one break trigger: the arm and
/// control differ ONLY in the microcompaction flag; both receive the same
/// `prepare` so the break itself is identical.
fn break_pair(
    prepare_arm: impl FnOnce(&mut Harness<CompactionFakeProvider>),
    prepare_ctrl: impl FnOnce(&mut Harness<CompactionFakeProvider>),
) -> (SeededArm, SeededArm) {
    (
        run_break_arm(true, prepare_arm),
        run_break_arm(false, prepare_ctrl),
    )
}

/// Assert one break arm's economics: the flush happened (tagged as expected),
/// nothing compacted, and under the break's re-bill the folded turn-1 payload
/// is no larger than the control's -- marginal modeled write <= 0 -- while the
/// steady state (turn 2) is strictly smaller.
fn assert_break_arm_is_free(tag: &str, arm: &SeededArm, control: &SeededArm) {
    assert_eq!(arm.folds, 1, "{tag}: the arm folds the superseded read");
    assert_eq!(control.folds, 0, "{tag}: the control must not fold");
    assert!(arm.records.is_empty(), "{tag}: the arm must not compact");
    assert!(
        control.records.is_empty(),
        "{tag}: the control must not compact"
    );
    // Under the break, the entire request re-bills (or was never cached)
    // either way: the write mass IS the payload, and the fold only shrinks it.
    let arm_t1 = est_tokens(&arm.requests[0].payload);
    let ctrl_t1 = est_tokens(&control.requests[0].payload);
    assert!(
        arm_t1 <= ctrl_t1,
        "{tag}: folded break payload must not exceed the control's \
         (arm {arm_t1} vs control {ctrl_t1})"
    );
    // Steady state keeps the full per-turn saving.
    let arm_t2 = est_tokens(&arm.requests[1].payload);
    let ctrl_t2 = est_tokens(&control.requests[1].payload);
    assert!(
        arm_t2 < ctrl_t2,
        "{tag}: steady-state request must shrink (arm {arm_t2} vs control {ctrl_t2})"
    );
}

#[test]
fn selection_switch_flush_is_free_a2() {
    // A2: caches are model-scoped, so a model switch re-bills the whole
    // request with or without the fold.
    let arm_prepare = |h: &mut Harness<CompactionFakeProvider>| {
        h.note_active_selection("prov-a", "model-a", None);
        h.record_selection_event("prov-a", "model-b", None)
            .expect("selection event records");
    };
    let ctrl_prepare = |h: &mut Harness<CompactionFakeProvider>| {
        h.note_active_selection("prov-a", "model-a", None);
        h.record_selection_event("prov-a", "model-b", None)
            .expect("selection event records");
    };
    let (arm, control) = break_pair(arm_prepare, ctrl_prepare);
    assert_break_arm_is_free("A2", &arm, &control);
}

#[test]
fn reasoning_switch_flush_is_free_a3() {
    // A3: a reasoning change breaks at the message level; folds live in
    // messages, so the fold's rewrite is covered by the same break.
    let prepare = |h: &mut Harness<CompactionFakeProvider>| {
        h.note_active_selection("prov-a", "model-a", Some("medium"));
        h.record_selection_event("prov-a", "model-a", Some("high"))
            .expect("selection event records");
    };
    let prepare_ctrl = |h: &mut Harness<CompactionFakeProvider>| {
        h.note_active_selection("prov-a", "model-a", Some("medium"));
        h.record_selection_event("prov-a", "model-a", Some("high"))
            .expect("selection event records");
    };
    let (arm, control) = break_pair(prepare, prepare_ctrl);
    assert_break_arm_is_free("A3", &arm, &control);
}

#[test]
fn cold_resume_flush_is_free_a4() {
    // A4: the resumed transcript's last activity sits past the profile's
    // cold threshold, so the cache is expired and the first request re-bills
    // everything regardless. A tiny threshold plus a real wait keeps the arm
    // deterministic without fabricating timestamps.
    let cold = |h: &mut Harness<CompactionFakeProvider>| {
        h.set_cache_profile(crate::wayland::CacheProfile {
            cold_after: Some(std::time::Duration::from_millis(10)),
            ..Default::default()
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let (arm, control) = break_pair(cold, cold);
    assert_break_arm_is_free("A4", &arm, &control);
}

#[test]
fn below_minimum_prefix_flush_is_free_a5() {
    // A5: a prefix below the provider's minimum cacheable length is never
    // cached, so there is no cache to break -- the flush is free by
    // construction (marginal write 0) and the steady state still shrinks.
    let below_min = |h: &mut Harness<CompactionFakeProvider>| {
        h.set_cache_profile(crate::wayland::CacheProfile {
            min_cacheable_tokens: u64::MAX,
            ..Default::default()
        });
    };
    let (arm, control) = break_pair(below_min, below_min);
    assert_break_arm_is_free("A5", &arm, &control);
}

#[test]
fn manual_compact_flush_rides_the_compaction_a6() {
    // A6 mirrors A1: the manual compaction rewrites the prefix anyway, so the
    // fold adds no marginal write -- it can only shrink the rewrite. Driven
    // through the production `/compact` seam (`compact_now`).
    let run = |micro: bool| {
        let seed = carry_seed();
        let budget = break_arm_budget(&seed);
        run_seeded_with(
            &seed,
            budget,
            micro,
            SummarizerKind::Provider,
            Vec::new(),
            &["continue: proceed with the next small wiring step."],
            |_| {},
            true,
        )
    };
    let arm = run(true);
    let control = run(false);
    assert_eq!(
        arm.folds, 1,
        "A6: the pending fold rides the manual compact"
    );
    assert_eq!(arm.records.len(), 1, "A6: the manual compaction applied");
    assert_eq!(control.records.len(), 1);
    let arm_gen = model_cache_economics(&arm.requests, Some(&arm.seed_baseline));
    let ctrl_gen = model_cache_economics(&control.requests, Some(&control.seed_baseline));
    let a = arm_gen.first().expect("arm generation 1");
    let c = ctrl_gen.first().expect("control generation 1");
    assert!(
        a.write_tokens <= c.write_tokens,
        "A6: folding must not increase the post-compaction write \
         (arm {} vs control {})",
        a.write_tokens,
        c.write_tokens
    );
}

/// Prints the per-trigger flush-cost table appended to
/// `docs/benchmarks/issue-400-fold-flush-cost.md`. Regenerate with:
/// `cargo test trigger_class_flush_cost_benchmark_report -- --nocapture`
#[test]
fn trigger_class_flush_cost_benchmark_report() {
    struct Row {
        tag: &'static str,
        arm: SeededArm,
        control: SeededArm,
    }
    let mut rows = Vec::new();
    {
        let prepare = |h: &mut Harness<CompactionFakeProvider>| {
            h.note_active_selection("prov-a", "model-a", None);
            h.record_selection_event("prov-a", "model-b", None).unwrap();
        };
        let arm = run_break_arm(true, prepare);
        let control = run_break_arm(false, |h| {
            h.note_active_selection("prov-a", "model-a", None);
            h.record_selection_event("prov-a", "model-b", None).unwrap();
        });
        rows.push(Row {
            tag: "A2 model switch",
            arm,
            control,
        });
    }
    {
        let arm = run_break_arm(true, |h| {
            h.note_active_selection("p", "m", Some("medium"));
            h.record_selection_event("p", "m", Some("high")).unwrap();
        });
        let control = run_break_arm(false, |h| {
            h.note_active_selection("p", "m", Some("medium"));
            h.record_selection_event("p", "m", Some("high")).unwrap();
        });
        rows.push(Row {
            tag: "A3 reasoning switch",
            arm,
            control,
        });
    }
    {
        let cold = |h: &mut Harness<CompactionFakeProvider>| {
            h.set_cache_profile(crate::wayland::CacheProfile {
                cold_after: Some(std::time::Duration::from_millis(10)),
                ..Default::default()
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        rows.push(Row {
            tag: "A4 cold resume",
            arm: run_break_arm(true, cold),
            control: run_break_arm(false, cold),
        });
    }
    {
        let below = |h: &mut Harness<CompactionFakeProvider>| {
            h.set_cache_profile(crate::wayland::CacheProfile {
                min_cacheable_tokens: u64::MAX,
                ..Default::default()
            });
        };
        rows.push(Row {
            tag: "A5 below minimum",
            arm: run_break_arm(true, below),
            control: run_break_arm(false, below),
        });
    }

    println!("\n== Class-A break-trigger flushes -- {MODELED_LABEL} ==");
    println!(
        "| trigger | arm t1 payload | control t1 payload | marginal write | t2 arm | t2 control | steady saving |"
    );
    println!("|---|---|---|---|---|---|---|");
    for row in &rows {
        let arm_t1 = est_tokens(&row.arm.requests[0].payload);
        let ctrl_t1 = est_tokens(&row.control.requests[0].payload);
        let arm_t2 = est_tokens(&row.arm.requests[1].payload);
        let ctrl_t2 = est_tokens(&row.control.requests[1].payload);
        println!(
            "| {} | {arm_t1} | {ctrl_t1} | {} | {arm_t2} | {ctrl_t2} | {} |",
            row.tag,
            arm_t1 as i64 - ctrl_t1 as i64,
            ctrl_t2.saturating_sub(arm_t2),
        );
    }
    println!(
        "\nUnder each break the request mass re-bills (A2/A3: model/params scoped cache; A4: \
         expired TTL) or was never cached (A5: below the minimum cacheable prefix), with or \
         without the fold. The negative marginal is the fold's own shrink riding the break."
    );
    println!(
        "A6 (manual /compact) mirrors A1: see the same-boundary table above -- the fold only \
         shrinks the compaction's rewrite."
    );
}

// --- Phase 2 (issue #400 M5): inferred-cold (Class B) modeled arm. ---
//
// SIMULATION CAVEAT, stated honestly: the fake-provider lane has no real
// prompt cache and no TTL. The arm simulates the cold cache by the same
// modeling rule used everywhere in this file -- once the TTL is past, the
// provider re-bills the entire request, so the "cost" of a request after the
// idle gap is its full payload with or without the fold. What the arm proves
// deterministically is (a) the trigger fires on a real idle gap measured from
// the transcript's activity clock against the profile threshold, mid-session
// and without any pending break, and (b) the folded payload the cold cache
// re-bills is never larger than the unfolded one. The realized counterpart is
// the env-gated live pair below (`compaction_live_bench`).

/// One inferred-cold arm: turn 1 runs warm (neutral profile -- the resume-time
/// A4 check is consumed and holds), then a cold threshold far below a real
/// wall-clock wait is installed mid-session, so turn 2's boundary infers cold
/// and flushes (Class B); turn 3 measures the steady state.
fn run_inferred_cold_arm(micro: bool) -> SeededArm {
    let seed = carry_seed();
    let budget = break_arm_budget(&seed);
    let root = TempDir::new();
    let workspace = TempDir::new();
    let mut log = SessionLog::create_in(&root.path, &workspace.path).expect("create session log");
    for message in &seed {
        log.append(message).expect("append seed message");
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

    let summary_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let agent = Agent::resumed(
        CompactionFakeProvider::new(summary_calls.clone(), requests.clone(), Vec::new()),
        Tools::new(Vec::new()),
        stored.messages,
    );
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
    harness.set_microcompaction_watermark(budget / 2);

    let counter = CompactionCounter::new();
    let gate = NoToolGate;
    let token = CancellationToken::new();
    // Turn 1: warm (no cold threshold); the resume-time check is consumed.
    block_on(harness.submit_turn(
        "continue: proceed with the next small wiring step.",
        &counter,
        &gate,
        &token,
    ))
    .expect("turn succeeds");
    // Mid-session idle gap: a real wait against a threshold far below it.
    harness.set_cache_profile(crate::wayland::CacheProfile {
        cold_after: Some(std::time::Duration::from_millis(10)),
        ..Default::default()
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    for prompt in [
        "then: confirm the buffer state briefly.",
        "finally: report the wiring status in one line.",
    ] {
        block_on(harness.submit_turn(prompt, &counter, &gate, &token)).expect("turn succeeds");
    }

    let folds = fold_count(&path);
    let messages = harness.agent.messages();
    let rebuilt_context = messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let post_context_tokens = harness.context_token_estimate();
    SeededArm {
        rebuilt_context,
        summary_body: String::new(),
        records: counter.records.borrow().clone(),
        folds,
        summary_calls: summary_calls.load(Ordering::Relaxed),
        post_context_tokens,
        requests: requests.lock().expect("requests capture lock").clone(),
        seed_baseline: serialize_request(&seed),
    }
}

#[test]
fn inferred_cold_flush_fires_mid_session_and_is_free_under_a_cold_cache() {
    let arm = run_inferred_cold_arm(true);
    let control = run_inferred_cold_arm(false);
    // Integrity: the flush landed at the TURN-2 boundary (mid-session, after
    // the warm turn), not at turn 1; nothing compacted; no summarizer call.
    assert_eq!(arm.folds, 1, "the idle gap releases exactly one fold");
    assert_eq!(control.folds, 0);
    assert!(arm.records.is_empty() && control.records.is_empty());
    assert_eq!(arm.summary_calls, 0);
    assert_eq!(arm.requests.len(), 3, "three driven turns");
    // Turn 1 payloads are byte-identical (the arm held while warm).
    assert_eq!(
        est_tokens(&arm.requests[0].payload),
        est_tokens(&control.requests[0].payload),
        "no fold before the idle gap: the warm turn is identical"
    );
    // Under the expired cache, turn 2 re-bills its full payload either way;
    // the folded payload never exceeds the control's (marginal write <= 0).
    let arm_t2 = est_tokens(&arm.requests[1].payload);
    let ctrl_t2 = est_tokens(&control.requests[1].payload);
    assert!(
        arm_t2 <= ctrl_t2,
        "cold re-bill with the fold must not exceed the control ({arm_t2} vs {ctrl_t2})"
    );
    // Steady state (turn 3) keeps the full per-turn saving.
    let arm_t3 = est_tokens(&arm.requests[2].payload);
    let ctrl_t3 = est_tokens(&control.requests[2].payload);
    assert!(
        arm_t3 < ctrl_t3,
        "steady-state request must shrink ({arm_t3} vs {ctrl_t3})"
    );
}

/// Prints the Class-B row for `docs/benchmarks/issue-400-fold-flush-cost.md`.
/// Regenerate with:
/// `cargo test inferred_cold_flush_cost_benchmark_report -- --nocapture`
#[test]
fn inferred_cold_flush_cost_benchmark_report() {
    let arm = run_inferred_cold_arm(true);
    let control = run_inferred_cold_arm(false);
    let arm_t2 = est_tokens(&arm.requests[1].payload);
    let ctrl_t2 = est_tokens(&control.requests[1].payload);
    let arm_t3 = est_tokens(&arm.requests[2].payload);
    let ctrl_t3 = est_tokens(&control.requests[2].payload);
    println!("\n== Class-B inferred-cold flush -- {MODELED_LABEL} ==");
    println!(
        "| trigger | arm t2 payload (cold re-bill) | control t2 payload | marginal write | steady saving (t3) |"
    );
    println!("|---|---|---|---|---|");
    println!(
        "| B idle gap | {arm_t2} | {ctrl_t2} | {} | {} |",
        arm_t2 as i64 - ctrl_t2 as i64,
        ctrl_t3.saturating_sub(arm_t3),
    );
    println!(
        "\nSIMULATION: the fake lane has no real TTL; the cold cache is modeled as a full \
         re-bill of the post-gap request, which holds by definition once the provider TTL is \
         past. The trigger firing on the real idle gap (transcript activity clock vs profile \
         threshold, mid-session, no pending break) is what the arm proves deterministically; \
         realized deltas come from the env-gated live pair."
    );
    println!(
        "Wrong-inference cost (gap inferred cold but cache still warm): one warm flush, \
         measured at 4485 modeled / 2129 realized write tokens on this seed -- bounded, and \
         strictly less than what the watermark-only trigger paid on every flush."
    );
}
