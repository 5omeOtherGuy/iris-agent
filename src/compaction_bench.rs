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
use crate::session::{SessionLog, SessionStore};
use crate::tools::ToolState;
use crate::tools::bench_support::{
    assert_min_reduction, assert_min_reduction_tokens, assert_ratio_within,
    assert_survives_verbatim, est_ratio, est_tokens, report_header, report_row, report_row_tokens,
};
use crate::wayland::{Harness, SummarizerKind};
use serde_json::json;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
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
        let is_summary = messages
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
            self.summary_calls.fetch_add(1, Ordering::Relaxed);
            // Covered range = every message before the final summary
            // instruction the seam appends.
            let covered = &messages[..messages.len() - 1];
            AssistantTurn::text(&self.derive_handoff(covered))
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
        SummarizerKind::Provider => {
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
            SummarizerKind::Provider => "compaction/provider",
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

    let counter = CompactionCounter::new();
    let gate = NoToolGate;
    let token = CancellationToken::new();
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
