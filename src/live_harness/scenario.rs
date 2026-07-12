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

/// The tool workspace a scenario drives its live turns against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceKind {
    /// The live repository root. The S-series compaction scenarios read real
    /// source files (`src/nexus.rs`, ...) so a live read adds genuine mid-turn
    /// mass; nothing is written into the repo.
    Repo,
    /// A fresh temp tree the scenario materializes before the session seeds. The
    /// T-series tool-efficiency scenarios build deterministic fixture trees
    /// (reusing `bench_tokens` fixtures) so a live model's tools operate on
    /// fixed inputs, comparable to the legacy tokens-per-task probes.
    Fixtures,
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
    /// Post-run success criteria the scenario asserts on its own rows. The
    /// default accepts any completed run; a scenario overrides this to fail a
    /// run that completed without a provider error but did not exercise its
    /// target behavior. A silently under-driving scenario is the bug this
    /// guards against, so it is surfaced as a Fail, never a green pass.
    fn verify_run(&self, rows: &[Row]) -> std::result::Result<(), String> {
        let _ = rows;
        Ok(())
    }
    /// The tool workspace this scenario drives against. Defaults to the live
    /// repository (the S-series posture); a scenario that needs a materialized
    /// fixture tree overrides this to [`WorkspaceKind::Fixtures`] and provides
    /// its files through [`Scenario::materialize`].
    fn workspace_kind(&self) -> WorkspaceKind {
        WorkspaceKind::Repo
    }
    /// Materialize any fixture files the scenario's turns reference into the
    /// tool `workspace`, before the session seeds. Called by the runner only
    /// for a [`WorkspaceKind::Fixtures`] scenario; the default no-op fits the
    /// repo-reading S-series.
    fn materialize(&self, workspace: &Path) -> Result<()> {
        let _ = workspace;
        Ok(())
    }
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
    /// Number of sequential tool-call round-trips the single turn drives. Each
    /// read closes a pair boundary MID-TURN, where the governor can compact
    /// (`compaction_governor::govern` runs only on `turn_continues` boundaries;
    /// post-turn it early-returns). The pilot-a single-round-trip shape reached
    /// tier=hard but never compacted -- this is the regression sentinel for #552
    /// AND for that under-drive.
    pub(crate) round_trips: usize,
    /// Filler repetitions in the pre-turn seed. Sized so the seed loads BELOW
    /// the start tier, so the start and hard crossings both happen mid-turn as
    /// the scripted reads land, not on load.
    pub(crate) seed_repeat: usize,
    /// Filler repetitions per scripted read result (several thousand tokens
    /// each): the mass one mid-turn round-trip adds toward the hard tier.
    pub(crate) result_repeat: usize,
    pub(crate) budget: u64,
}

/// Large, stable repository files the live turn reads one at a time. Each is
/// well over a thousand tokens even after the read tool's caps, so a live read
/// adds real mid-turn mass exactly as the scripted bodies model it in-gate.
pub(crate) const S1_LIVE_READ_TARGETS: [&str; 6] = [
    "src/nexus.rs",
    "src/session.rs",
    "src/config.rs",
    "src/cli.rs",
    "src/lib.rs",
    "src/mimir/providers/anthropic_messages.rs",
];

impl AggressiveFill {
    /// The default pilot cell: seed below start, six mid-turn round-trips that
    /// cross start then hard well before the last one, so a continuing
    /// hard-tier boundary lets #552 current-turn coverage fire. Six (not four)
    /// because pilot-a run 2 showed live provider-anchored measurement runs
    /// ~15% below the estimator arithmetic: with four reads the live run
    /// topped out between start and hard and compaction became a start-tier
    /// background race (1 of 2 runs lost it).
    pub(crate) fn pilot() -> Self {
        Self {
            round_trips: 6,
            seed_repeat: 900,
            result_repeat: 320,
            budget: 32_768,
        }
    }

    /// The synthetic start/hard thresholds this scenario is tuned against
    /// (budget x default fractions), used by its own shape assertions.
    pub(crate) fn start_threshold(&self) -> u64 {
        (self.budget as f64 * 0.72) as u64
    }

    pub(crate) fn hard_threshold(&self) -> u64 {
        (self.budget as f64 * 0.90) as u64
    }

    /// The pre-turn seed transcript, sized below the start tier.
    pub(crate) fn seed_messages(&self) -> Vec<Message> {
        vec![
            Message::user(&format!(
                "S1 aggressive-fill preamble; the runaway session is pre-loaded below the start tier so the start and hard crossings both happen mid-turn.\n{}",
                "pre-turn seed filler that pushes the runaway session toward the start tier. "
                    .repeat(self.seed_repeat)
            )),
            Message::assistant("Seed loaded; ready to read the buffers one at a time."),
        ]
    }

    /// The scripted body one mid-turn read returns (several thousand tokens).
    /// The in-gate fake-provider flow writes these as fixture files and the
    /// live turn reads real repository files of at least this mass. Multi-line
    /// (one moderate line per repeat) so the read tool renders it in full,
    /// under its 2000-line / 50KB caps and without per-line truncation.
    pub(crate) fn scripted_read_body(&self, i: usize) -> String {
        let mut body = format!("READ-RESULT-{i}\n");
        for line in 0..self.result_repeat {
            body.push_str(&format!(
                "line {line}: large scripted tool result body toward the hard tier.\n"
            ));
        }
        body
    }
}

impl Scenario for AggressiveFill {
    fn id(&self) -> &str {
        "S1"
    }

    fn seed(&self, _workspace: &Path) -> Result<Vec<Message>> {
        Ok(self.seed_messages())
    }

    fn turns(&self) -> Vec<String> {
        // ONE turn that forces several sequential tool-call round-trips: the
        // model must read each file on its own step, so every read closes a
        // pair boundary mid-turn (mirrors `auto_compaction_live_loop`'s
        // real-tool loop). Sequential, one read per reply -- not parallel --
        // so the governor sees a continuing boundary between each read.
        let list = S1_LIVE_READ_TARGETS
            .iter()
            .take(self.round_trips)
            .map(|target| format!("- {target}"))
            .collect::<Vec<_>>()
            .join("\n");
        vec![format!(
            "MANDATORY VERIFICATION TASK: you MUST read every file listed below with the \
             read tool before answering; summaries from memory or from file names are \
             invalid and fail the verification. Read them ONE AT A TIME (a single read \
             tool call per reply, wait for each result before the next). Do not stop \
             early, do not skip files, do not batch reads. Only after the final read, \
             give a one-line summary of each:\n{list}"
        )]
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

    /// S1's target behavior is MID-TURN compaction. A run that completed but
    /// observed fewer than three boundaries, or that never compacted, silently
    /// under-drove and is a Fail -- exactly the pilot-a defect being fixed.
    fn verify_run(&self, rows: &[Row]) -> std::result::Result<(), String> {
        // One row per provider request == one round-trip boundary.
        let boundaries = rows.len();
        let compacted = rows
            .iter()
            .any(|row| row.lifecycle.compaction_generation_applied.is_some());
        if boundaries < 3 {
            return Err(format!(
                "S1 produced no compaction: only {boundaries} boundaries (< 3 required); \
                 the turn did not drive enough mid-turn round-trips"
            ));
        }
        if !compacted {
            return Err("S1 produced no compaction".to_string());
        }
        Ok(())
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
    /// The small pilot cell (S4-small): few cycles at a small budget.
    pub(crate) fn small() -> Self {
        Self {
            cycles: 2,
            filler_repeat: 20,
            budget: 32_768,
        }
    }

    /// The full cache-churn cell (S4): more cycles at a larger budget, for the
    /// dedicated break-even campaigns once the small pilot has validated plumbing.
    pub(crate) fn full() -> Self {
        Self {
            cycles: 4,
            filler_repeat: 30,
            budget: 131_072,
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

/// Optional per-cell size-knob overrides. Each field, when set, replaces the
/// scenario's pilot default for that knob; unset fields keep the shipped pilot
/// size. A knob that a scenario does not carry is ignored by that scenario's
/// arm of [`build_scenario`] (documented per scenario). Config validation
/// bounds these values before they reach here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ScenarioKnobs {
    pub(crate) budget: Option<u64>,
    pub(crate) round_trips: Option<usize>,
    pub(crate) seed_repeat: Option<usize>,
    pub(crate) result_repeat: Option<usize>,
}

/// The one place scenario ids map to constructed scenarios. Adding a scenario is
/// a new arm here plus its generator; the runner and config loader stay
/// unchanged. Unknown ids return `None` so a campaign definition fails loudly
/// rather than silently skipping a cell.
///
/// Knob application per scenario (only meaningful knobs are consumed):
/// - S1 (aggressive-fill): round_trips, seed_repeat, result_repeat, budget.
/// - S2 (multi-turn grind): budget.
/// - S3 (fold-dominant): result_repeat, budget.
/// - S4-small (cache-churn): budget.
/// - T1-T4 (tool-efficiency): round_trips (as the repetition/round-trip knob),
///   budget. See `tool_scenarios.rs`.
pub(crate) fn build_scenario(id: &str, knobs: &ScenarioKnobs) -> Option<Box<dyn Scenario>> {
    if let Some(scenario) = build_tool_scenario(id, knobs) {
        return Some(scenario);
    }
    match id {
        "S1" => {
            let mut s = AggressiveFill::pilot();
            if let Some(v) = knobs.round_trips {
                s.round_trips = v;
            }
            if let Some(v) = knobs.seed_repeat {
                s.seed_repeat = v;
            }
            if let Some(v) = knobs.result_repeat {
                s.result_repeat = v;
            }
            if let Some(v) = knobs.budget {
                s.budget = v;
            }
            Some(Box::new(s))
        }
        "S2" => {
            let mut s = MultiTurnGrind::pilot();
            if let Some(v) = knobs.budget {
                s.budget = v;
            }
            Some(Box::new(s))
        }
        "S3" => {
            let mut s = FoldDominant::pilot();
            if let Some(v) = knobs.result_repeat {
                s.result_repeat = v;
            }
            if let Some(v) = knobs.budget {
                s.budget = v;
            }
            Some(Box::new(s))
        }
        "S4" => {
            let mut s = CacheChurn::full();
            if let Some(v) = knobs.budget {
                s.budget = v;
            }
            Some(Box::new(s))
        }
        "S4-small" => {
            let mut s = CacheChurn::small();
            if let Some(v) = knobs.budget {
                s.budget = v;
            }
            Some(Box::new(s))
        }
        _ => None,
    }
}

/// Resolve a scenario id to its pilot-sized instance (no knob overrides). A thin
/// wrapper over [`build_scenario`] kept for the runner's default path and tests.
pub(crate) fn pilot_scenario(id: &str) -> Option<Box<dyn Scenario>> {
    build_scenario(id, &ScenarioKnobs::default())
}

/// The scenario ids a campaign cell may name, listed in error messages so a bad
/// `scenario = "..."` in a config file names the accepted set. The S-series
/// measure compaction; the T-series (`tool_scenarios.rs`) measure tool-output
/// token efficiency through the same row schema and artifact layout.
pub(crate) const ACCEPTED_SCENARIO_IDS: [&str; 9] =
    ["S1", "S2", "S3", "S4", "S4-small", "T1", "T2", "T3", "T4"];

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
    fn s1_seed_loads_below_start_and_scripted_reads_cross_hard_within_planned_round_trips() {
        let s1 = AggressiveFill::pilot();
        // One turn (finding 1 keeps S1 a single turn), but it must drive several
        // sequential round-trips.
        assert_eq!(s1.turns().len(), 1, "S1 stays a single turn");
        assert!(s1.round_trips >= 3, "needs >= 3 mid-turn boundaries");

        // The seed loads BELOW the start tier, so the crossings are mid-turn.
        let seed = s1.seed(&ws()).expect("s1 seed");
        assert_eq!(
            count_role(&seed, Role::User),
            1,
            "single pre-turn user seed"
        );
        let seed_tokens = est_tokens(&seed);
        assert!(
            seed_tokens < s1.start_threshold(),
            "seed must load below start ({}): got {seed_tokens}",
            s1.start_threshold()
        );

        // Pure token arithmetic against the estimator: seed + k scripted read
        // results cross start then hard within the planned round-trips, and
        // hard is crossed BEFORE the final round-trip so a continuing hard-tier
        // boundary remains for the governor to compact on.
        let body_tokens = est_tokens(std::slice::from_ref(&Message::tool_result(
            "s1-probe",
            "read",
            &s1.scripted_read_body(0),
        )));
        let cumulative = |k: u64| seed_tokens + k * body_tokens;
        let k_start = (1..=s1.round_trips as u64)
            .find(|k| cumulative(*k) >= s1.start_threshold())
            .expect("must cross start within planned round-trips");
        let k_hard = (1..=s1.round_trips as u64)
            .find(|k| cumulative(*k) >= s1.hard_threshold())
            .expect("must cross hard within planned round-trips");
        assert!(k_start < k_hard, "start is crossed before hard");
        assert!(
            k_hard < s1.round_trips as u64,
            "hard must be crossed before the final round-trip so a continuing \
             hard-tier boundary remains (k_hard={k_hard}, round_trips={})",
            s1.round_trips
        );

        // Live divergence margin: pilot-a run 2 measured provider-anchored
        // context ~15% below this estimator arithmetic, which left the live
        // run short of hard and compaction hostage to the start-tier race.
        // Require hard crossed before the final round-trip even if the
        // provider counts 20% fewer tokens than the estimator.
        let discounted = |k: u64| (cumulative(k) as f64 * 0.80) as u64;
        let k_hard_discounted = (1..=s1.round_trips as u64)
            .find(|k| discounted(*k) >= s1.hard_threshold())
            .expect("must cross hard within planned round-trips at a 20% provider discount");
        assert!(
            k_hard_discounted < s1.round_trips as u64,
            "hard must be crossed pre-final even at a 20% provider discount \
             (k={k_hard_discounted}, round_trips={})",
            s1.round_trips
        );
    }

    #[test]
    fn s1_verify_run_fails_without_compaction_and_passes_with_one() {
        let s1 = AggressiveFill::pilot();

        // Under-drove: too few boundaries.
        let one = vec![sample_row_no_compaction(0)];
        assert!(
            s1.verify_run(&one)
                .unwrap_err()
                .contains("S1 produced no compaction")
        );

        // Enough boundaries, but no compaction lifecycle event anywhere.
        let none_compacted: Vec<Row> = (0..4).map(sample_row_no_compaction).collect();
        assert_eq!(
            s1.verify_run(&none_compacted).unwrap_err(),
            "S1 produced no compaction"
        );

        // Three boundaries with a compaction event on one row: success.
        let mut compacted = none_compacted;
        compacted[2].lifecycle.compaction_generation_applied = Some(1);
        compacted[2].lifecycle.origin = Some("subagent".to_string());
        assert!(s1.verify_run(&compacted).is_ok());
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

    fn sample_row_no_compaction(seq: u32) -> Row {
        let mut row = super::metrics::sample_row_for_tests(seq, RowKind::Turn);
        row.scenario = "S1".to_string();
        row.lifecycle = LifecycleDelta::default();
        row.tier = Tier::Hard;
        row
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
        assert_eq!(pilot_scenario("S4").expect("S4").id(), "S4");
        assert!(pilot_scenario("nope").is_none());
    }

    #[test]
    fn build_scenario_applies_knob_overrides_to_s1() {
        // No knobs: the pilot default.
        let default = build_scenario("S1", &ScenarioKnobs::default()).expect("S1");
        assert_eq!(default.budget(), AggressiveFill::pilot().budget);

        // Overrides replace the pilot size knob-for-knob.
        let knobs = ScenarioKnobs {
            budget: Some(65_536),
            round_trips: Some(9),
            seed_repeat: Some(1_000),
            result_repeat: Some(400),
        };
        let tuned = build_scenario("S1", &knobs).expect("S1");
        assert_eq!(tuned.budget(), 65_536);
        // Round-trips flow into the turn's read list length.
        assert_eq!(tuned.turns().len(), 1);
    }
}
