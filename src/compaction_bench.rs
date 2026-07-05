//! Compaction retention-needle benchmark scaffold (ADR-0045, issue #372,
//! "slice A").
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
//! Slice B (deferred, not built here; tracked in ADR-0045 / #372): the
//! `provider + structured carry` (ADR-0044) and `provider + carry +
//! microcompaction` (ADR-0048) arms, and the report dimensions -- compaction
//! generation (ADR-0047), covered-range size, and the two `ProviderUsage`
//! cache-economics measurements (summary-request cache-hit rate and
//! post-compaction cache-write amplification). This slice fixes the
//! retention-needle contract and the two base arms only.

use super::*;
use crate::session::SessionLog;
use crate::tools::ToolState;
use crate::tools::bench_support::{
    assert_min_reduction, assert_ratio_within, assert_survives_verbatim, est_ratio, est_tokens,
    report_header, report_row,
};
use crate::wayland::{Harness, SummarizerKind};
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
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
/// begins with the summarizer instruction) gets a deterministic handoff summary
/// that echoes every needle verbatim and is small enough to clear the shrink
/// guard. No live calls -- this is the CI-safe lane ADR-0045 requires.
struct CompactionFakeProvider {
    summary_calls: RefCell<usize>,
}

impl CompactionFakeProvider {
    fn new() -> Self {
        Self {
            summary_calls: RefCell::new(0),
        }
    }

    fn handoff_summary() -> String {
        // A structured handoff a takeover model could resume from, carrying the
        // exact identifiers. Kept short so `estimate_tokens(framed) <
        // original_tokens` (the wayland shrink guard) holds for the covered
        // range.
        format!(
            "Goal: land {NEEDLE_TASK}. State: edits started. Key facts: path {NEEDLE_PATH}, \
             symbol {NEEDLE_SYMBOL}, decision {NEEDLE_DECISION}. Next: finish the wiring."
        )
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
        let turn = if is_summary {
            *self.summary_calls.borrow_mut() += 1;
            AssistantTurn::text(&Self::handoff_summary())
        } else {
            AssistantTurn::text("ok")
        };
        Ok(Box::pin(futures::stream::once(async move {
            Ok(ProviderEvent::Completed(turn))
        })))
    }
}

/// Counts compactions applied and swallows every other event, so the scenario
/// can assert at least one auto-compaction fired through the production seam.
struct CompactionCounter {
    compactions: RefCell<usize>,
}

impl CompactionCounter {
    fn new() -> Self {
        Self {
            compactions: RefCell::new(0),
        }
    }
}

impl AgentObserver for CompactionCounter {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        if let AgentEvent::CompactionApplied { .. } = event {
            *self.compactions.borrow_mut() += 1;
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

    let agent = Agent::new(CompactionFakeProvider::new(), Tools::new(Vec::new()));
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
    // the start of turn 3.
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

    ArmResult {
        rebuilt_context,
        summary,
        retained_tail,
        covered_original,
        compactions: *counter.compactions.borrow(),
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
