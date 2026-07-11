//! T-series: tool-efficiency scenarios (the tokens-per-task measurements,
//! migrated into the live-harness `Scenario` family). Where the S-series isolate
//! compaction behaviors, the T-series drive a live model through the built-in
//! tools against deterministic fixture trees and measure the per-request token
//! mass a tool result injects. They ride the SAME `Row` schema, artifact
//! folders, and campaign-file loader as the S-series, so the harness's
//! cache/estimate columns apply to tool-efficiency runs for free -- that is the
//! whole point of the migration (BENCHMARK_PLAN / issue #210 lineage).
//!
//! Fixtures and per-tool render/edit probes are REUSED from `bench_tokens`
//! (`fixtures.rs`, `probes.rs`) rather than rewritten: a T-series scenario
//! materializes the exact committed fixture a legacy probe measured, so a
//! T-series read/grep/find/edit sees the byte-for-byte same input and the
//! in-gate parity test can prove "same fixtures in, comparable token classes
//! out" against the legacy probe contract.
//!
//! Mapping to the legacy studies:
//! - **T1** read/skim token mass (issue-337): `probe_read` -> `read` full vs skim.
//! - **T2** search-output classes (issue-338/339/340): `probe_grep` + the wide
//!   find tree -> `grep` / `ls` / `find`.
//! - **T3** edit-result classes (issue-341): the five `edit` outcome classes.
//! - **T4** chained suite (the chained-all-four flow): read + grep + edit + shell
//!   chained across the four PR-seeded repair fixtures.
//!
//! Live-only, like the rest of `live_harness`: the deterministic turns/shape/
//! parity/verify_run tests run in the gate; a real model only runs behind
//! `IRIS_BENCH_LIVE=1`.

use super::*;
use crate::nexus::bench_tokens_per_task::fixtures;
use std::path::Path;

/// The large synthetic budget the T-series run under. Tool-efficiency measures
/// raw per-request tool mass, so the scenarios run with auto-compaction and
/// folds OFF (see [`tool_posture`]) and a headroom budget that keeps pressure
/// tiers out of the way; a T-series row's value is its token classes, not a
/// compaction boundary.
const TOOL_BUDGET: u64 = 131_072;

/// The T-series posture: compaction and folds OFF so a tool result's mass is
/// measured raw, not reclaimed by a fold or dropped by a compaction apply.
fn tool_posture() -> ScenarioPosture {
    ScenarioPosture {
        auto_compaction: false,
        folds: false,
    }
}

/// The documented minimum tool-output reduction class (percent) the reducing
/// T-series tools clear over their baseline render -- the same 20% floor the
/// legacy probes assert (ADR-0036: the bar is the contract, not the exact
/// figure).
pub(crate) const TOOL_MIN_REDUCTION_PCT: u32 = 20;

/// Fail a T-series run that did not actually exercise its tools. Rows are one
/// per provider request, and each tool round-trip is a request (the model emits
/// a tool call, the tool runs, the next request carries the result), so a run
/// that answered without running any tool produces a single row. A scenario that
/// silently under-drove its tools is a hard failure recorded verbatim, never a
/// green pass -- the same fail-loud rule the S-series use.
fn require_round_trips(id: &str, rows: &[Row], min: usize) -> std::result::Result<(), String> {
    let observed = rows.len();
    if observed < min {
        return Err(format!(
            "{id} exercised only {observed} tool round-trip(s) (< {min} required); \
             the model answered without running the target tools"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// T1 read/skim token mass (issue-337): full read vs skim over one comment-heavy
// source. Wraps the legacy `read-skim-comment-heavy` probe fixture.
// ---------------------------------------------------------------------------

pub(crate) struct ReadSkimMass {
    /// Repetitions of the full-read + skim-read pair (the round-trip knob,
    /// mapped from the config `round_trips`). Each repetition is two reads.
    pub(crate) reps: usize,
    pub(crate) budget: u64,
}

impl ReadSkimMass {
    /// The committed fixture this scenario materializes, and the legacy probe
    /// name whose contract the in-gate parity test replays.
    pub(crate) const FIXTURE: &'static str = "probe_read";
    pub(crate) const LEGACY_PROBE: &'static str = "read-skim-comment-heavy";
    /// The file the turns read; present at the workspace root after materialize.
    pub(crate) const TARGET: &'static str = "settlement.rs";

    pub(crate) fn pilot() -> Self {
        Self {
            reps: 1,
            budget: TOOL_BUDGET,
        }
    }

    fn min_round_trips(&self) -> usize {
        // Two reads per rep (full + skim).
        (self.reps * 2).max(2)
    }
}

impl Scenario for ReadSkimMass {
    fn id(&self) -> &str {
        "T1"
    }

    fn seed(&self, _workspace: &Path) -> Result<Vec<Message>> {
        Ok(vec![Message::user(
            "T1 read/skim token-mass probe: read the same source two ways and \
             compare how much of the file each read returns.",
        )])
    }

    fn turns(&self) -> Vec<String> {
        let mut turns = Vec::with_capacity(self.reps * 2);
        for i in 0..self.reps {
            turns.push(format!(
                "Rep {i}: read `{}` in FULL (skim off) and list every exported item.",
                Self::TARGET
            ));
            turns.push(format!(
                "Rep {i}: now read `{}` with `skim: true` and confirm the same exports \
                 plus `due_ids` still appear.",
                Self::TARGET
            ));
        }
        turns
    }

    fn posture(&self) -> ScenarioPosture {
        tool_posture()
    }

    fn budget(&self) -> u64 {
        self.budget
    }

    fn workspace_kind(&self) -> WorkspaceKind {
        WorkspaceKind::Fixtures
    }

    fn materialize(&self, workspace: &Path) -> Result<()> {
        fixtures::materialize_into(Self::FIXTURE, workspace);
        Ok(())
    }

    fn verify_run(&self, rows: &[Row]) -> std::result::Result<(), String> {
        require_round_trips(self.id(), rows, self.min_round_trips())
    }
}

// ---------------------------------------------------------------------------
// T2 search-output classes (issue-338/339/340): grep grouping, ls listing, and
// find directory compaction. Wraps the legacy grep + find probes.
// ---------------------------------------------------------------------------

pub(crate) struct SearchOutputMass {
    /// Repetitions of the grep + ls + find sweep (round-trip knob).
    pub(crate) reps: usize,
    pub(crate) budget: u64,
}

impl SearchOutputMass {
    pub(crate) const GREP_FIXTURE: &'static str = "probe_grep";
    pub(crate) const GREP_PROBE: &'static str = "grep-deadline-repeated-matches";
    pub(crate) const FIND_PROBE: &'static str = "find-wide-tree-grouping";
    /// A directory the wide find tree materializes; the `ls` turn lists it.
    pub(crate) const LS_DIR: &'static str = "services/aaa_target/gateway";

    pub(crate) fn pilot() -> Self {
        Self {
            reps: 1,
            budget: TOOL_BUDGET,
        }
    }

    fn min_round_trips(&self) -> usize {
        // grep + ls + find per rep.
        (self.reps * 3).max(3)
    }
}

impl Scenario for SearchOutputMass {
    fn id(&self) -> &str {
        "T2"
    }

    fn seed(&self, _workspace: &Path) -> Result<Vec<Message>> {
        Ok(vec![Message::user(
            "T2 search-output token-mass probe: run grep, ls, and find over a wide \
             tree and compare how compactly each renders its matches.",
        )])
    }

    fn turns(&self) -> Vec<String> {
        let mut turns = Vec::with_capacity(self.reps * 3);
        for i in 0..self.reps {
            turns.push(format!(
                "Rep {i}: grep for `deadline` (ignore case) across the workspace and \
                 report the matching constant."
            ));
            turns.push(format!(
                "Rep {i}: list the directory `{}` and count its files.",
                Self::LS_DIR
            ));
            turns.push(format!(
                "Rep {i}: find every `*.rs` file in the tree and note `handler_zebra_target.rs`."
            ));
        }
        turns
    }

    fn posture(&self) -> ScenarioPosture {
        tool_posture()
    }

    fn budget(&self) -> u64 {
        self.budget
    }

    fn workspace_kind(&self) -> WorkspaceKind {
        WorkspaceKind::Fixtures
    }

    fn materialize(&self, workspace: &Path) -> Result<()> {
        // The union of the two legacy search fixtures: the grep tree under
        // `crates/` and the wide find tree under `services/`. They do not
        // overlap, so grep-for-deadline and find-*.rs each hit their own tree.
        fixtures::materialize_into(Self::GREP_FIXTURE, workspace);
        fixtures::build_find_tree(workspace);
        Ok(())
    }

    fn verify_run(&self, rows: &[Row]) -> std::result::Result<(), String> {
        require_round_trips(self.id(), rows, self.min_round_trips())
    }
}

// ---------------------------------------------------------------------------
// T3 edit-result classes (issue-341): drive `edit` through its distinct outcome
// classes. Reuses the legacy `edit_cases()` initial contents as the fixture and
// the class contract as the parity anchor.
// ---------------------------------------------------------------------------

pub(crate) struct EditResultMass {
    /// Repetitions of the exact + tolerant edit pair (round-trip knob).
    pub(crate) reps: usize,
    pub(crate) budget: u64,
}

impl EditResultMass {
    pub(crate) const EXACT_FILE: &'static str = "edit_exact.rs";
    pub(crate) const TOLERANT_FILE: &'static str = "edit_tolerant.rs";
    /// Initial contents lifted from the legacy `exact` / `tolerant` edit cases,
    /// so the T3 fixture is the same input the legacy edit probe measured.
    const EXACT_INITIAL: &'static str = "let deadline = 47231;\nlet retries = 4;\n";
    const TOLERANT_INITIAL: &'static str = "let label = \u{201c}ready\u{201d};\n";

    pub(crate) fn pilot() -> Self {
        Self {
            reps: 1,
            budget: TOOL_BUDGET,
        }
    }

    fn min_round_trips(&self) -> usize {
        // A read-before-mutate + edit per file, exact and tolerant.
        (self.reps * 2).max(2)
    }
}

impl Scenario for EditResultMass {
    fn id(&self) -> &str {
        "T3"
    }

    fn seed(&self, _workspace: &Path) -> Result<Vec<Message>> {
        Ok(vec![Message::user(
            "T3 edit-result-class probe: apply an exact edit and a tolerant \
             (fuzzy-quote) edit and compare the result envelopes.",
        )])
    }

    fn turns(&self) -> Vec<String> {
        let mut turns = Vec::with_capacity(self.reps * 2);
        for i in 0..self.reps {
            turns.push(format!(
                "Rep {i}: read `{}`, then replace `47231` with `50000` (exact match).",
                Self::EXACT_FILE
            ));
            turns.push(format!(
                "Rep {i}: read `{}`, then change the label text from `ready` to `done` \
                 (the file uses curly quotes, so this is a tolerant match).",
                Self::TOLERANT_FILE
            ));
        }
        turns
    }

    fn posture(&self) -> ScenarioPosture {
        tool_posture()
    }

    fn budget(&self) -> u64 {
        self.budget
    }

    fn workspace_kind(&self) -> WorkspaceKind {
        WorkspaceKind::Fixtures
    }

    fn materialize(&self, workspace: &Path) -> Result<()> {
        std::fs::write(workspace.join(Self::EXACT_FILE), Self::EXACT_INITIAL)?;
        std::fs::write(workspace.join(Self::TOLERANT_FILE), Self::TOLERANT_INITIAL)?;
        Ok(())
    }

    fn verify_run(&self, rows: &[Row]) -> std::result::Result<(), String> {
        require_round_trips(self.id(), rows, self.min_round_trips())
    }
}

// ---------------------------------------------------------------------------
// T4 chained suite (chained-all-four): read + grep + edit + shell chained across
// the four PR-seeded repair fixtures. The closest analogue to the legacy
// chained-suite campaigns; its fail-loud rule is the strictest.
// ---------------------------------------------------------------------------

pub(crate) struct ChainedToolSuite {
    /// Minimum tool round-trips the chained flow must produce; a run below this
    /// skipped tools and is a hard failure (the round-trip knob doubles as the
    /// fail-loud floor).
    pub(crate) min_round_trips: usize,
    pub(crate) budget: u64,
}

impl ChainedToolSuite {
    /// A file the chained tree materializes; the read turn targets it.
    pub(crate) const READ_TARGET: &'static str = "bytes/src/buf_impl.rs";

    pub(crate) fn pilot() -> Self {
        Self {
            // read + grep + edit + shell.
            min_round_trips: 4,
            budget: TOOL_BUDGET,
        }
    }
}

impl Scenario for ChainedToolSuite {
    fn id(&self) -> &str {
        "T4"
    }

    fn seed(&self, _workspace: &Path) -> Result<Vec<Message>> {
        Ok(vec![Message::user(
            "T4 chained tool suite: inspect and repair across a small multi-project \
             tree using read, grep, edit, and a shell check in one session.",
        )])
    }

    fn turns(&self) -> Vec<String> {
        // ONE chained turn that forces the whole tool sequence, mirroring the
        // legacy chained-all-four flow: read to locate, grep to survey, edit to
        // repair, shell to check.
        vec![format!(
            "In one session: (1) read `{}` to find the sign-extension bug; \
             (2) grep for `as i32` across the tree to survey the sites; \
             (3) edit the file to fix the cast; \
             (4) run the shell command `ls bytes/src` to confirm the layout. \
             Do all four steps, each with its own tool call.",
            Self::READ_TARGET
        )]
    }

    fn posture(&self) -> ScenarioPosture {
        tool_posture()
    }

    fn budget(&self) -> u64 {
        self.budget
    }

    fn workspace_kind(&self) -> WorkspaceKind {
        WorkspaceKind::Fixtures
    }

    fn materialize(&self, workspace: &Path) -> Result<()> {
        // The four PR-seeded single-bug repair fixtures, each an independent
        // cargo/npm project under its own subdir (bytes/clap/nushell/dayjs).
        fixtures::build_chained_all_tree(workspace);
        Ok(())
    }

    fn verify_run(&self, rows: &[Row]) -> std::result::Result<(), String> {
        require_round_trips(self.id(), rows, self.min_round_trips)
    }
}

/// The T-series arm of the scenario registry. Kept beside the scenarios so
/// [`build_scenario`] delegates here for the tool-efficiency ids while the
/// S-series stay in `scenario.rs`. The `round_trips` knob is the repetition /
/// round-trip floor for the T-series; `budget` overrides the headroom budget.
pub(crate) fn build_tool_scenario(id: &str, knobs: &ScenarioKnobs) -> Option<Box<dyn Scenario>> {
    match id {
        "T1" => {
            let mut s = ReadSkimMass::pilot();
            if let Some(v) = knobs.round_trips {
                s.reps = v;
            }
            if let Some(v) = knobs.budget {
                s.budget = v;
            }
            Some(Box::new(s))
        }
        "T2" => {
            let mut s = SearchOutputMass::pilot();
            if let Some(v) = knobs.round_trips {
                s.reps = v;
            }
            if let Some(v) = knobs.budget {
                s.budget = v;
            }
            Some(Box::new(s))
        }
        "T3" => {
            let mut s = EditResultMass::pilot();
            if let Some(v) = knobs.round_trips {
                s.reps = v;
            }
            if let Some(v) = knobs.budget {
                s.budget = v;
            }
            Some(Box::new(s))
        }
        "T4" => {
            let mut s = ChainedToolSuite::pilot();
            if let Some(v) = knobs.round_trips {
                s.min_round_trips = v;
            }
            if let Some(v) = knobs.budget {
                s.budget = v;
            }
            Some(Box::new(s))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::bench_tokens_per_task::probes::{
        EditCase, ToolProbe, assert_edit_case, assert_render_contract, edit_cases, tool_probes,
    };

    /// Materialize a scenario's fixtures into a fresh temp workspace and return
    /// it (auto-cleaned on drop) so a test can assert the turns' paths exist.
    fn materialized(scenario: &dyn Scenario) -> TempDir {
        let ws = TempDir::new(&format!("tool-scenario-{}", scenario.id()));
        scenario
            .materialize(&ws.path)
            .expect("materialize fixtures");
        ws
    }

    /// Find a legacy render probe by name (the parity anchor for T1/T2).
    fn probe(name: &str) -> ToolProbe {
        tool_probes()
            .into_iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| panic!("legacy probe {name} not found"))
    }

    /// Find a legacy edit case by name (the parity anchor for T3).
    fn edit_case(name: &str) -> EditCase {
        edit_cases()
            .into_iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("legacy edit case {name} not found"))
    }

    #[test]
    fn t_series_ids_resolve_through_the_registry_and_are_fixture_backed() {
        for id in ["T1", "T2", "T3", "T4"] {
            let scenario = build_scenario(id, &ScenarioKnobs::default())
                .unwrap_or_else(|| panic!("registry must build {id}"));
            assert_eq!(scenario.id(), id);
            assert_eq!(
                scenario.workspace_kind(),
                WorkspaceKind::Fixtures,
                "{id} drives against a materialized fixture tree"
            );
            // Tool efficiency measures raw mass: compaction and folds are off.
            assert!(
                !scenario.posture().auto_compaction,
                "{id} runs compaction off"
            );
            assert!(!scenario.posture().folds, "{id} runs folds off");
            assert!(scenario.budget() >= 8_192, "{id} carries a headroom budget");
        }
        // The accepted set names the T-series so a bad id is a loud, listed error.
        for id in ["T1", "T2", "T3", "T4"] {
            assert!(ACCEPTED_SCENARIO_IDS.contains(&id), "{id} is accepted");
        }
    }

    #[test]
    fn t1_materializes_the_read_fixture_and_turns_reference_it() {
        let t1 = ReadSkimMass::pilot();
        let ws = materialized(&t1);
        assert!(
            ws.path.join(ReadSkimMass::TARGET).exists(),
            "settlement.rs must materialize at the workspace root"
        );
        // Both a full-read and a skim turn, referencing the real path.
        let turns = t1.turns();
        assert_eq!(turns.len(), 2);
        assert!(turns[0].contains(ReadSkimMass::TARGET) && turns[0].contains("FULL"));
        assert!(turns[1].contains("skim"));
    }

    #[test]
    fn t1_parity_the_legacy_read_skim_probe_still_clears_its_mass_class() {
        // Same fixture in (probe_read), comparable token class out: the reduced
        // (skim) render clears the documented 20% class and every needle
        // survives verbatim. This is the legacy issue-337 contract, unchanged.
        let result = assert_render_contract(&probe(ReadSkimMass::LEGACY_PROBE));
        assert!(result.reduction_pct >= f64::from(TOOL_MIN_REDUCTION_PCT));
    }

    #[test]
    fn t2_materializes_both_search_trees_and_turns_reference_them() {
        let t2 = SearchOutputMass::pilot();
        let ws = materialized(&t2);
        // The grep tree (crates/) and the wide find tree (services/) coexist.
        assert!(ws.path.join("crates").is_dir(), "grep fixture tree present");
        assert!(
            ws.path.join(SearchOutputMass::LS_DIR).is_dir(),
            "find tree ls target present"
        );
        assert!(
            ws.path
                .join("services/aaa_target/gateway/handler_zebra_target.rs")
                .exists(),
            "find needle file present"
        );
        let turns = t2.turns();
        assert_eq!(turns.len(), 3);
        assert!(turns[0].contains("grep"));
        assert!(turns[1].contains(SearchOutputMass::LS_DIR));
        assert!(turns[2].contains("find"));
    }

    #[test]
    fn t2_parity_the_legacy_grep_and_find_probes_still_clear_their_mass_classes() {
        for name in [SearchOutputMass::GREP_PROBE, SearchOutputMass::FIND_PROBE] {
            let p = probe(name);
            assert!(!p.slow, "T2 parity uses only in-gate (non-slow) probes");
            let result = assert_render_contract(&p);
            assert!(result.reduction_pct >= f64::from(TOOL_MIN_REDUCTION_PCT));
        }
    }

    #[test]
    fn t3_materializes_edit_fixtures_from_the_legacy_case_contents() {
        let t3 = EditResultMass::pilot();
        let ws = materialized(&t3);
        // The T3 fixture is the SAME initial content the legacy edit cases use.
        let exact = std::fs::read_to_string(ws.path.join(EditResultMass::EXACT_FILE)).unwrap();
        assert_eq!(exact, edit_case("exact").initial);
        let tolerant =
            std::fs::read_to_string(ws.path.join(EditResultMass::TOLERANT_FILE)).unwrap();
        assert_eq!(tolerant, edit_case("tolerant").initial);
        let turns = t3.turns();
        assert_eq!(turns.len(), 2);
        assert!(turns[0].contains("47231") && turns[0].contains("50000"));
        assert!(turns[1].contains("ready") && turns[1].contains("done"));
    }

    #[test]
    fn t3_parity_the_legacy_edit_classes_still_hold() {
        // Same edit inputs in, same outcome classes + disk effects out.
        for case in edit_cases() {
            assert_edit_case(&case);
        }
    }

    #[test]
    fn t4_materializes_the_chained_tree_and_turns_reference_a_real_file() {
        let t4 = ChainedToolSuite::pilot();
        let ws = materialized(&t4);
        for sub in ["bytes", "clap", "nushell", "dayjs"] {
            assert!(
                ws.path.join(sub).is_dir(),
                "chained subproject {sub} present"
            );
        }
        assert!(
            ws.path.join(ChainedToolSuite::READ_TARGET).exists(),
            "the read turn's target file must exist"
        );
        let turns = t4.turns();
        assert_eq!(turns.len(), 1, "T4 is one chained turn");
        for tool in ["read", "grep", "edit", "shell"] {
            assert!(turns[0].contains(tool), "chained turn drives {tool}");
        }
    }

    #[test]
    fn verify_run_is_red_when_tools_are_skipped_and_green_when_exercised() {
        // A single-request run (model answered without running any tool) is a
        // hard failure for every T scenario; enough round-trips passes.
        let one = vec![row("T1")];
        for id in ["T1", "T2", "T3", "T4"] {
            let scenario = build_scenario(id, &ScenarioKnobs::default()).unwrap();
            let err = scenario
                .verify_run(&one)
                .expect_err("a tools-skipped run must fail");
            assert!(err.contains(id), "failure names the scenario: {err}");
            assert!(
                err.contains("without running the target tools"),
                "failure explains the under-drive: {err}"
            );
        }

        // T4's floor is the strictest (>= 4 round-trips): 3 rows still fails, 4
        // passes.
        let t4 = build_scenario("T4", &ScenarioKnobs::default()).unwrap();
        let three: Vec<Row> = (0..3).map(|_| row("T4")).collect();
        assert!(
            t4.verify_run(&three).is_err(),
            "3 round-trips is below T4's floor"
        );
        let four: Vec<Row> = (0..4).map(|_| row("T4")).collect();
        assert!(
            t4.verify_run(&four).is_ok(),
            "4 round-trips clears T4's floor"
        );
    }

    #[test]
    fn round_trips_knob_raises_the_repetition_count_and_fail_loud_floor() {
        // The config `round_trips` knob scales the T-series repetitions (T1-T3)
        // and the T4 fail-loud floor.
        let knobs = ScenarioKnobs {
            round_trips: Some(3),
            ..ScenarioKnobs::default()
        };
        let t1 = build_scenario("T1", &knobs).unwrap();
        assert_eq!(t1.turns().len(), 6, "3 reps x (full + skim)");
        // Below 6 round-trips now fails T1.
        let five: Vec<Row> = (0..5).map(|_| row("T1")).collect();
        assert!(t1.verify_run(&five).is_err());

        let t4 = build_scenario("T4", &knobs).unwrap();
        let two: Vec<Row> = (0..2).map(|_| row("T4")).collect();
        assert!(t4.verify_run(&two).is_err(), "below the floor of 3");
        let three: Vec<Row> = (0..3).map(|_| row("T4")).collect();
        assert!(
            t4.verify_run(&three).is_ok(),
            "floor lowered to 3 clears at 3"
        );
    }

    fn row(scenario: &str) -> Row {
        let mut r = super::metrics::sample_row_for_tests(0, RowKind::Turn);
        r.scenario = scenario.to_string();
        r
    }
}
