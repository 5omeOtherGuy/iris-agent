//! Scenarios: deterministic session-drivers a campaign cell runs. A scenario
//! seeds a transcript and/or drives turns, and declares the compaction posture
//! it needs to isolate its target behavior. The four synthetic scenarios below
//! are parameterized by size knobs so a cell can pick a small pilot shape or a
//! larger stress shape without new Rust.
//!
//! R1 (SWE-bench instance execution) and R2 (repo Q&A with recall probes) are
//! OUT OF SCOPE for this PR. The [`Scenario`] trait is intentionally shaped so
//! they slot in later: a real-world scenario overrides `seed`/`turns` to clone a
//! scratch repo and drive the issue text, and reports its outcome through the
//! same campaign row schema. Nothing here hard-codes "synthetic only".

use super::*;
use std::path::Path;

/// The compaction posture a scenario needs. S3 isolates microcompaction, so it
/// runs with auto-compaction off and folds on; the fill/grind/churn scenarios
/// exercise auto-compaction directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScenarioPosture {
    pub(crate) auto_compaction: bool,
    pub(crate) folds: bool,
}

/// A campaign-drivable scenario. Synthetic generators and (future) real-world
/// drivers both implement this so the runner is scenario-agnostic.
pub(crate) trait Scenario {
    /// Stable id used in row schemas and manifests (e.g. `S1`).
    fn id(&self) -> &str;
    /// The transcript to seed before driving turns. Empty for scenarios that
    /// build all state through turns.
    fn seed(&self, workspace: &Path) -> Result<Vec<Message>>;
    /// User prompts submitted sequentially after the seed loads.
    fn turns(&self) -> Vec<String>;
    /// The compaction posture that isolates this scenario's target behavior.
    fn posture(&self) -> ScenarioPosture;
    /// The synthetic token window this scenario is tuned against.
    fn budget(&self) -> u64;
}

/// Deterministic chars/4 token estimate, matching the seam's estimator so a
/// scenario can be tuned to cross a threshold reproducibly.
pub(crate) fn est_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(|m| m.content.len() as u64).sum::<u64>() / 4
}

/// A workspace-relative path used by the fold-dominant scenario; a later, larger
/// read of the same path supersedes the earlier one so folds can reclaim it.
const FOLD_PATH: &str = "crates/orbit/src/telemetry/buffer.rs";

/// Build an ADR-0021 `read` result envelope (ok + metadata.target) so the fold
/// engine's `successful_target` recognizes it.
fn read_result(call: &str, target: &str, body: &str) -> Message {
    Message::tool_result(
        call,
        "read",
        &serde_json::json!({
            "ok": true,
            "content": body,
            "metadata": { "target": target },
        })
        .to_string(),
    )
}

fn read_call(id: &str, target: &str) -> Message {
    Message::assistant_tool_call(&ToolCall {
        id: id.to_string(),
        name: "read".to_string(),
        arguments: serde_json::json!({ "path": target }),
        thought_signature: None,
    })
}

// ---------------------------------------------------------------------------
// S1 aggressive-fill: a single mega-turn with parallel large tool results (the
// runaway-session shape; regression sentinel for #552).
// ---------------------------------------------------------------------------

pub(crate) struct AggressiveFill {
    /// Number of parallel tool results in the single mega-turn.
    pub(crate) parallel_results: usize,
    /// Filler repetitions per tool result (the size knob).
    pub(crate) result_repeat: usize,
    pub(crate) budget: u64,
}

impl AggressiveFill {
    /// The default pilot cell: sized to cross the hard threshold on load.
    pub(crate) fn pilot() -> Self {
        Self {
            parallel_results: 4,
            result_repeat: 600,
            budget: 32_768,
        }
    }
}

impl Scenario for AggressiveFill {
    fn id(&self) -> &str {
        "S1"
    }

    fn seed(&self, _workspace: &Path) -> Result<Vec<Message>> {
        let mut seed = vec![Message::user(
            "One shot: read every telemetry buffer and report. This is a single large turn.",
        )];
        for i in 0..self.parallel_results {
            let target = format!("crates/orbit/src/telemetry/buffer_{i}.rs");
            let body = format!(
                "PARALLEL-RESULT-{i} :: {}",
                "large tool result body carried in the runaway mega-turn. "
                    .repeat(self.result_repeat)
            );
            let call = format!("s1-{i}");
            seed.push(read_call(&call, &target));
            seed.push(read_result(&call, &target, &body));
        }
        seed.push(Message::assistant("All buffers read."));
        Ok(seed)
    }

    fn turns(&self) -> Vec<String> {
        vec!["Summarize the buffers you just read in one short sentence.".to_string()]
    }

    fn posture(&self) -> ScenarioPosture {
        ScenarioPosture {
            auto_compaction: true,
            folds: true,
        }
    }

    fn budget(&self) -> u64 {
        self.budget
    }
}

// ---------------------------------------------------------------------------
// S2 multi-turn grind: n user turns of tool-heavy work; steady pressure that
// crosses start and hard over the run.
// ---------------------------------------------------------------------------

pub(crate) struct MultiTurnGrind {
    pub(crate) turns: usize,
    pub(crate) filler_repeat: usize,
    pub(crate) budget: u64,
}

impl MultiTurnGrind {
    pub(crate) fn pilot() -> Self {
        Self {
            turns: 8,
            filler_repeat: 30,
            budget: 32_768,
        }
    }
}

impl Scenario for MultiTurnGrind {
    fn id(&self) -> &str {
        "S2"
    }

    fn seed(&self, _workspace: &Path) -> Result<Vec<Message>> {
        Ok(vec![
            Message::user("We are grinding through the reconciliation wiring turn by turn."),
            Message::assistant("Understood; ready."),
        ])
    }

    fn turns(&self) -> Vec<String> {
        (0..self.turns)
            .map(|i| {
                format!(
                    "Turn {i}: read Cargo.toml, then continue the wiring. {}",
                    "Steady per-turn pressure toward the next compaction boundary. "
                        .repeat(self.filler_repeat)
                )
            })
            .collect()
    }

    fn posture(&self) -> ScenarioPosture {
        ScenarioPosture {
            auto_compaction: true,
            folds: true,
        }
    }

    fn budget(&self) -> u64 {
        self.budget
    }
}

// ---------------------------------------------------------------------------
// S3 fold-dominant: many large reads of the same target that folds should
// reclaim before compaction fires. Runs with auto-compaction OFF to isolate
// microcompaction.
// ---------------------------------------------------------------------------

pub(crate) struct FoldDominant {
    /// Number of superseded reads of the fold path (the size knob).
    pub(crate) reads: usize,
    pub(crate) result_repeat: usize,
    pub(crate) budget: u64,
}

impl FoldDominant {
    pub(crate) fn pilot() -> Self {
        Self {
            reads: 6,
            result_repeat: 40,
            budget: 131_072,
        }
    }
}

impl Scenario for FoldDominant {
    fn id(&self) -> &str {
        "S3"
    }

    fn seed(&self, _workspace: &Path) -> Result<Vec<Message>> {
        let mut seed = vec![Message::user(
            "Re-read the buffer repeatedly as it changes; each read supersedes the last.",
        )];
        for i in 0..self.reads {
            let body = format!(
                "FOLD-READ-{i} :: {}",
                "superseded buffer read detail that a later read replaces. "
                    .repeat(self.result_repeat)
            );
            let call = format!("s3-{i}");
            seed.push(read_call(&call, FOLD_PATH));
            seed.push(read_result(&call, FOLD_PATH, &body));
            seed.push(Message::assistant("Noted the latest buffer contents."));
        }
        Ok(seed)
    }

    fn turns(&self) -> Vec<String> {
        vec!["Confirm the latest buffer state in one short sentence.".to_string()]
    }

    fn posture(&self) -> ScenarioPosture {
        ScenarioPosture {
            auto_compaction: false,
            folds: true,
        }
    }

    fn budget(&self) -> u64 {
        self.budget
    }
}

// ---------------------------------------------------------------------------
// S4 cache-churn: alternating hot-prefix turns and forced compactions at
// varying depths -- the dedicated cache break-even scenario (goal 4/6).
// ---------------------------------------------------------------------------

pub(crate) struct CacheChurn {
    /// Number of hot/churn cycles (the size knob; "small" uses few).
    pub(crate) cycles: usize,
    pub(crate) filler_repeat: usize,
    pub(crate) budget: u64,
}

impl CacheChurn {
    /// The small pilot cell (S4-small).
    pub(crate) fn small() -> Self {
        Self {
            cycles: 2,
            filler_repeat: 20,
            budget: 32_768,
        }
    }
}

impl Scenario for CacheChurn {
    fn id(&self) -> &str {
        "S4"
    }

    fn seed(&self, _workspace: &Path) -> Result<Vec<Message>> {
        Ok(vec![
            Message::user("Alternating cache-churn: hot prefix, then a forced churn, repeating."),
            Message::assistant("Ready for the churn cycles."),
        ])
    }

    fn turns(&self) -> Vec<String> {
        let mut turns = Vec::new();
        for cycle in 0..self.cycles {
            turns.push(format!(
                "HOT {cycle}: reuse the warm prefix; read Cargo.toml and reply briefly."
            ));
            turns.push(format!(
                "CHURN {cycle}: force pressure with fresh material. {}",
                "Distinct churn filler that breaks the warm prefix on this turn. "
                    .repeat(self.filler_repeat)
            ));
        }
        turns
    }

    fn posture(&self) -> ScenarioPosture {
        ScenarioPosture {
            auto_compaction: true,
            folds: true,
        }
    }

    fn budget(&self) -> u64 {
        self.budget
    }
}

/// Resolve a scenario id to its pilot-sized instance. Unknown ids return `None`
/// so a campaign definition fails loudly rather than silently skipping a cell.
pub(crate) fn pilot_scenario(id: &str) -> Option<Box<dyn Scenario>> {
    match id {
        "S1" => Some(Box::new(AggressiveFill::pilot())),
        "S2" => Some(Box::new(MultiTurnGrind::pilot())),
        "S3" => Some(Box::new(FoldDominant::pilot())),
        "S4-small" => Some(Box::new(CacheChurn::small())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn ws() -> PathBuf {
        PathBuf::from("/tmp/scenario-shape")
    }

    fn count_role(messages: &[Message], role: Role) -> usize {
        messages.iter().filter(|m| m.role == role).count()
    }

    #[test]
    fn s1_is_a_single_mega_turn_with_parallel_results_crossing_hard() {
        let s1 = AggressiveFill::pilot();
        let seed = s1.seed(&ws()).expect("s1 seed");
        // Single mega-turn: exactly one user message.
        assert_eq!(count_role(&seed, Role::User), 1);
        // Parallel tool results: one call + one result per parallel branch.
        assert_eq!(
            count_role(&seed, Role::AssistantToolCall),
            s1.parallel_results
        );
        assert_eq!(count_role(&seed, Role::Tool), s1.parallel_results);
        assert!(s1.parallel_results >= 2, "parallel means at least two");
        // Crosses the hard threshold on load.
        let hard = (s1.budget() as f64 * 0.90) as u64;
        assert!(
            est_tokens(&seed) > hard,
            "s1 must cross hard ({}): got {}",
            hard,
            est_tokens(&seed)
        );
    }

    #[test]
    fn s3_is_fold_heavy_with_compaction_off() {
        let s3 = FoldDominant::pilot();
        assert!(!s3.posture().auto_compaction, "S3 runs with compaction off");
        assert!(s3.posture().folds, "S3 isolates folds");
        let seed = s3.seed(&ws()).expect("s3 seed");
        // Many reads of the SAME target so folds can reclaim superseded ones.
        let read_targets: Vec<String> = seed
            .iter()
            .filter(|m| m.role == Role::AssistantToolCall)
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(read_targets.len(), s3.reads);
        assert!(s3.reads >= 3, "fold-dominant needs many reads");
        let distinct: BTreeSet<&String> = read_targets.iter().collect();
        assert_eq!(distinct.len(), 1, "all reads target the same fold path");
    }

    #[test]
    fn s4_alternates_hot_prefix_and_churn_turns() {
        let s4 = CacheChurn::small();
        let turns = s4.turns();
        assert_eq!(turns.len(), s4.cycles * 2);
        for (i, turn) in turns.iter().enumerate() {
            if i % 2 == 0 {
                assert!(turn.starts_with("HOT "), "even turns reuse the hot prefix");
            } else {
                assert!(turn.starts_with("CHURN "), "odd turns force churn");
            }
        }
    }

    #[test]
    fn s2_grinds_the_configured_number_of_turns() {
        let s2 = MultiTurnGrind::pilot();
        assert_eq!(s2.turns().len(), s2.turns);
        assert!(s2.posture().auto_compaction);
    }

    #[test]
    fn pilot_scenario_resolves_known_ids_only() {
        assert_eq!(pilot_scenario("S1").expect("S1").id(), "S1");
        assert_eq!(pilot_scenario("S4-small").expect("S4-small").id(), "S4");
        assert!(pilot_scenario("nope").is_none());
    }
}
