//! Randomized property tests for the auto-compaction spec (Part V.1).
//!
//! Two invariants the deterministic suite only anchors at hand-picked shapes:
//!
//! 1. Pair-splitting never occurs. Across randomized transcripts, keep targets,
//!    and coverable-prefix sizes, neither `valid_compaction_range`, the engine
//!    `plan()`, nor an applied `apply_summary` rewrite ever severs an
//!    assistant tool-call from its tool-result. The check is keyed on
//!    `tool_call_id`, an oracle independent of the role-adjacency logic the
//!    production guard uses, so a range that is role-valid but id-splitting
//!    would still fail.
//! 2. Live == resumed equivalence (ADR-0048). After a randomized sequence of
//!    appends, fold flushes, and compaction applies at varying keep targets,
//!    the in-memory message list is byte-identical (serialized) to the list
//!    rebuilt from the JSONL session log by `SessionStore::open` /
//!    `rebuild_with_compactions`.
//!
//! Generation uses a small documented SplitMix64 PRNG with fixed seeds rather
//! than `proptest` or `rand`:
//! - `proptest` would add a dev-dependency plus a custom paired-transcript
//!   strategy for two tests -- disproportionate integration cost.
//! - `rand` is already a dependency, but `StdRng`'s value sequence is
//!   explicitly not guaranteed stable across `rand` releases, so a recorded
//!   seed would stop reproducing the same exploration after a dependency bump.
//!
//! SplitMix64 (Steele/Lea; the JDK `SplittableRandom` mixer) is a well-known
//! generator that gives permanent bit-exact reproducibility from a fixed seed,
//! which is exactly what a regression-oriented property test wants.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use anyhow::Result;
use serde_json::json;

use super::{
    ApplyContext, CompactionEngine, CompactionSummary, context_tokens, fold_tail_start, summarize,
    valid_compaction_range,
};
use crate::config::{Settings, ToolResultCompactionPolicy};
use crate::nexus::{AgentEvent, AgentObserver, Message, Role, ToolCall};
use crate::session::{SessionLog, SessionStore, estimate_tokens};
use crate::tools::test_support::{root_of, temp_dir};

/// SplitMix64: a constant-increment counter fed through a fixed avalanche mix.
/// Deterministic and bit-stable across platforms and toolchains, so a seed
/// reproduces the exact same sequence forever.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, n)`; `n` must be non-zero.
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// Uniform in `[lo, hi)`; `lo < hi` required.
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo)
    }

    /// True with probability `pct/100`.
    fn chance(&mut self, pct: u64) -> bool {
        self.next_u64() % 100 < pct
    }
}

/// A no-op observer: these tests assert on message/log state, not events.
struct NoopObserver;

impl AgentObserver for NoopObserver {
    fn on_event(&self, _event: AgentEvent) -> Result<()> {
        Ok(())
    }
}

/// Fold policy that maximizes fold opportunities so the randomized sequences
/// actually exercise the fold path: recency guards off, semantic dedupe on
/// (the built-in default). Mirrors `fold_tests::policy`.
fn fold_policy() -> ToolResultCompactionPolicy {
    let mut policy = Settings {
        microcompaction: Some(true),
        ..Settings::default()
    }
    .tool_result_compaction()
    .expect("built-in tool-result compaction defaults are valid");
    policy.semantic_dedupe.protect_recent_tool_results = 0;
    policy.semantic_dedupe.protect_recent_tokens = 0;
    policy
}

/// Body of varying size so token estimates land across the interesting range
/// (tiny to a few hundred tokens), forcing both shallow and deep cuts.
fn filler(rng: &mut SplitMix64, word: &str) -> String {
    format!("{word} {}", format!("{word} ").repeat(rng.range(1, 70)))
}

/// One assistant tool round exactly as the loop persists it: a block of N
/// tool-call messages followed by the block of their N results (ADR shape:
/// all calls, then all results). Result envelopes are the ADR-0021 wire shape
/// so fold detection (successful `read`/`ls` targets) can fire.
fn push_tool_round(rng: &mut SplitMix64, messages: &mut Vec<Message>, call_seq: &mut u64) {
    let calls = rng.range(1, 4);
    // Draw from a small path pool so repeated read/ls targets trigger semantic
    // stale-read dedupe folds.
    const PATHS: [&str; 4] = ["a.rs", "b.rs", "src/lib.rs", "src/main.rs"];
    const TOOLS: [&str; 5] = ["read", "ls", "grep", "bash", "edit"];

    let mut round: Vec<(String, &str)> = Vec::with_capacity(calls);
    for _ in 0..calls {
        let id = format!("call_{:04}", *call_seq);
        *call_seq += 1;
        let tool = TOOLS[rng.below(TOOLS.len())];
        messages.push(Message::assistant_tool_call(&ToolCall {
            id: id.clone(),
            name: tool.to_string(),
            arguments: json!({ "path": PATHS[rng.below(PATHS.len())] }),
            thought_signature: None,
        }));
        round.push((id, tool));
    }
    for (id, tool) in round {
        let ok = rng.chance(85);
        let content = if matches!(tool, "read" | "ls") && ok {
            json!({
                "ok": true,
                "content": filler(rng, "line"),
                "metadata": { "target": PATHS[rng.below(PATHS.len())] }
            })
        } else {
            json!({ "ok": ok, "content": filler(rng, "out") })
        };
        messages.push(Message::tool_result(&id, tool, &content.to_string()));
    }
}

/// Append one coherent conversation fragment: a user turn, a full tool round
/// (calls + results together, never a dangling call), or an assistant text
/// message. Keeps every generated transcript a valid, pair-closed history.
fn push_fragment(rng: &mut SplitMix64, messages: &mut Vec<Message>, call_seq: &mut u64) {
    match rng.below(5) {
        0 | 1 => messages.push(Message::user(&filler(rng, "ask"))),
        2 | 3 => push_tool_round(rng, messages, call_seq),
        _ => messages.push(Message::assistant(&filler(rng, "reply"))),
    }
}

/// Generate a multi-turn transcript: user, then interleaved assistant text and
/// tool rounds, repeated. Always pair-closed.
fn generate_transcript(rng: &mut SplitMix64) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut call_seq = 0u64;
    let turns = rng.range(2, 8);
    for _ in 0..turns {
        messages.push(Message::user(&filler(rng, "task")));
        let rounds = rng.below(3);
        for _ in 0..rounds {
            push_tool_round(rng, &mut messages, &mut call_seq);
            if rng.chance(40) {
                messages.push(Message::assistant(&filler(rng, "note")));
            }
        }
        messages.push(Message::assistant(&filler(rng, "done")));
    }
    messages
}

/// Independent oracle: does covering `[start, end)` sever any tool-call from
/// its result? Keyed on `tool_call_id`, so it is not a restatement of the
/// role-adjacency logic in `valid_compaction_range` / `plan`.
fn splits_pair_by_id(messages: &[Message], start: usize, end: usize) -> bool {
    let mut call_idx: HashMap<&str, usize> = HashMap::new();
    let mut result_idx: HashMap<&str, usize> = HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        if let Some(id) = m.tool_call_id.as_deref() {
            match m.role {
                Role::AssistantToolCall => {
                    call_idx.insert(id, i);
                }
                Role::Tool => {
                    result_idx.insert(id, i);
                }
                _ => {}
            }
        }
    }
    let covered = |i: usize| start <= i && i < end;
    call_idx.iter().any(|(id, &ci)| {
        result_idx
            .get(id)
            .is_some_and(|&ri| covered(ci) != covered(ri))
    })
}

/// After a rewrite, every tool-call id must still have its result id present
/// and vice versa: a split pair leaves exactly one orphaned half.
fn tool_pairs_balanced(messages: &[Message]) -> bool {
    let mut calls: HashSet<&str> = HashSet::new();
    let mut results: HashSet<&str> = HashSet::new();
    for m in messages {
        if let Some(id) = m.tool_call_id.as_deref() {
            match m.role {
                Role::AssistantToolCall => {
                    calls.insert(id);
                }
                Role::Tool => {
                    results.insert(id);
                }
                _ => {}
            }
        }
    }
    calls == results
}

const BUDGET: u64 = 131_072;

/// Property 1: no plan, guard, or applied rewrite ever splits a tool-call /
/// tool-result pair, across randomized transcripts, keep targets, and
/// coverable-prefix sizes.
#[test]
fn compaction_never_splits_tool_call_result_pairs() {
    const SEEDS: u64 = 400;
    let mut planned = 0u64;
    let mut applied = 0u64;
    let mut fold_targets = 0u64;

    for seed in 0..SEEDS {
        let mut rng = SplitMix64::new(0xC0FFEE ^ seed.wrapping_mul(0x1000_0001));
        let messages = generate_transcript(&mut rng);
        let len = messages.len();

        let sessions = temp_dir();
        let ws = temp_dir();
        let workspace = root_of(&ws);
        let log = SessionLog::create_in(&sessions.path, &workspace).unwrap();
        let mut engine = CompactionEngine::new(
            Some(log),
            0,
            Vec::new(),
            Some(BUDGET),
            Arc::new(AtomicBool::new(false)),
        );
        // Persist the whole transcript: entry_ids become all-Some and
        // parallel, persisted == len.
        engine.persist_messages(&messages);
        let full = engine.persisted;

        // Guard oracle: any range the production guard accepts must not split a
        // pair, over random endpoints (independent of `plan`).
        for _ in 0..16 {
            let a = rng.below(len + 1);
            let b = rng.below(len + 1);
            let (start, end) = (a.min(b), a.max(b));
            if valid_compaction_range(&messages, start, end) {
                assert!(
                    !splits_pair_by_id(&messages, start, end),
                    "seed {seed}: valid_compaction_range accepted a splitting range {start}..{end}"
                );
            }
        }

        // Read-only plan checks across shallow/deep keep targets and varying
        // coverable-prefix sizes (undurable-tail shapes).
        let total = context_tokens(&messages);
        let keep_targets = [0, total / 8, total / 4, total / 2, total, total * 2];
        for &keep in &keep_targets {
            for &persisted in &[full, full / 2, full.saturating_sub(1), 0] {
                engine.persisted = persisted;
                if let Some(plan) = engine.plan(&messages, keep) {
                    planned += 1;
                    assert!(
                        valid_compaction_range(&messages, plan.start, plan.end),
                        "seed {seed}: plan produced an invalid range {}..{}",
                        plan.start,
                        plan.end
                    );
                    assert!(
                        !splits_pair_by_id(&messages, plan.start, plan.end),
                        "seed {seed}: plan {}..{} splits a tool-call/result pair",
                        plan.start,
                        plan.end
                    );
                    assert!(
                        (plan.start..plan.end).all(|i| engine.entry_ids[i].is_some()),
                        "seed {seed}: plan covers a non-coverable id"
                    );
                }
            }
        }
        engine.persisted = full;

        // Fold planning never targets anything but a tool result, so a fold
        // (content-only rewrite) can never split a pair.
        let policy = fold_policy();
        let tail_start = fold_tail_start(&messages, total / 4);
        for plan in super::fold::plan_folds(
            &messages,
            &engine.entry_ids,
            tail_start,
            &workspace,
            &policy,
        ) {
            fold_targets += 1;
            assert_eq!(
                messages[plan.index].role,
                Role::Tool,
                "seed {seed}: fold targeted a non-tool-result message"
            );
        }

        // Applied rewrite: compact the deepest coverable range and confirm the
        // resulting list has no orphaned tool halves.
        if let Some(plan) = engine.plan(&messages, 0) {
            let summary = CompactionSummary::excerpts(summarize(&messages[plan.start..plan.end]));
            let observer = NoopObserver;
            let cx = ApplyContext {
                workspace: &workspace,
                output_store: None,
                task_state: None,
                observer: &observer,
            };
            if let Some((_, rewritten)) =
                engine.apply_summary(&messages, plan, summary, cx).unwrap()
            {
                applied += 1;
                assert!(
                    tool_pairs_balanced(&rewritten),
                    "seed {seed}: applied compaction orphaned a tool half"
                );
            }
        }
    }

    // Guard against a generator that silently stopped exercising the space.
    assert!(planned > 0, "no plans were produced across {SEEDS} seeds");
    assert!(
        applied > 0,
        "no compactions were applied across {SEEDS} seeds"
    );
    assert!(
        fold_targets > 0,
        "no folds were planned across {SEEDS} seeds"
    );
}

/// Property 2: after a randomized sequence of appends, fold flushes, and
/// compaction applies, the live in-memory context is byte-identical to the
/// context rebuilt from the JSONL log (ADR-0048 live == resumed).
#[test]
fn live_context_equals_resume_rebuild_under_random_ops() {
    const SEEDS: u64 = 220;
    let mut compactions = 0u64;
    let mut folds = 0u64;

    for seed in 0..SEEDS {
        let mut rng = SplitMix64::new(0xA11CE ^ seed.wrapping_mul(0x9E37_79B1));
        let sessions = temp_dir();
        let ws = temp_dir();
        let workspace = root_of(&ws);
        let log = SessionLog::create_in(&sessions.path, &workspace).unwrap();
        let path = log.path().to_path_buf();
        let mut engine = CompactionEngine::new(
            Some(log),
            0,
            Vec::new(),
            Some(BUDGET),
            Arc::new(AtomicBool::new(false)),
        );
        let policy = fold_policy();
        let observer = NoopObserver;

        let mut messages: Vec<Message> = Vec::new();
        let mut call_seq = 0u64;
        // Seed with at least one turn so early folds/compactions have material.
        push_fragment(&mut rng, &mut messages, &mut call_seq);
        engine.persist_messages(&messages);

        let ops = rng.range(12, 26);
        for _ in 0..ops {
            match rng.below(10) {
                // Append (weighted): grow the transcript.
                0..=4 => {
                    push_fragment(&mut rng, &mut messages, &mut call_seq);
                    engine.persist_messages(&messages);
                }
                // Fold flush: durable fold entry + in-memory stub, mirroring
                // `Harness::flush_folds` (which is what the live path runs).
                5 | 6 => {
                    engine.persist_messages(&messages);
                    let total = context_tokens(&messages);
                    // Vary the protected tail so folds land inside and outside
                    // covered ranges across the run.
                    let keep = if rng.chance(50) { 0 } else { total / 3 };
                    let tail_start = fold_tail_start(&messages, keep);
                    let plans = super::fold::plan_folds(
                        &messages,
                        &engine.entry_ids,
                        tail_start,
                        &workspace,
                        &policy,
                    );
                    for plan in plans {
                        let tokens = estimate_tokens(&plan.stub);
                        engine
                            .session
                            .as_mut()
                            .unwrap()
                            .append_fold(&plan.entry_id, &plan.stub, Some(tokens), "C")
                            .unwrap();
                        messages[plan.index].content = plan.stub.clone();
                        folds += 1;
                    }
                }
                // Compaction apply at a shallow or deep keep target.
                _ => {
                    engine.persist_messages(&messages);
                    let total = context_tokens(&messages);
                    let keep = match rng.below(4) {
                        0 => 0,
                        1 => total / 8,
                        2 => total / 2,
                        _ => total,
                    };
                    if let Some(plan) = engine.plan(&messages, keep) {
                        let summary =
                            CompactionSummary::excerpts(summarize(&messages[plan.start..plan.end]));
                        let cx = ApplyContext {
                            workspace: &workspace,
                            output_store: None,
                            task_state: None,
                            observer: &observer,
                        };
                        if let Some((_, rewritten)) =
                            engine.apply_summary(&messages, plan, summary, cx).unwrap()
                        {
                            messages = rewritten;
                            compactions += 1;
                        }
                    }
                }
            }
        }
        engine.persist_messages(&messages);

        // Rebuild from the JSONL log and compare byte-exact serialized forms.
        let store = SessionStore::with_root(sessions.path.clone());
        let meta = store
            .list()
            .unwrap()
            .into_iter()
            .find(|m| m.path == path)
            .expect("freshly written session must be listable");
        let stored = store.open(&meta).unwrap();

        let live: Vec<String> = messages
            .iter()
            .map(|m| serde_json::to_string(m).unwrap())
            .collect();
        let rebuilt: Vec<String> = stored
            .messages
            .iter()
            .map(|m| serde_json::to_string(m).unwrap())
            .collect();
        assert_eq!(
            live, rebuilt,
            "seed {seed}: live context and resume rebuild diverged"
        );
    }

    assert!(compactions > 0, "no compactions ran across {SEEDS} seeds");
    assert!(folds > 0, "no folds ran across {SEEDS} seeds");
}
