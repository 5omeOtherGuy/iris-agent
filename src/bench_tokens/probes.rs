//! Per-tool micro-probes, layer 1: the DIRECT RENDER PROBE.
//!
//! Runs one tool by name over a workspace through the real dispatch, once with
//! `reduce_output = true` (arm A / shipped) and once `false` (arm B / baseline),
//! and compares the two rendered outputs. It proves two things a byte-only
//! bench cannot separate:
//!   1. the reduction is REAL   -- reduced output clears a token-reduction bar;
//!   2. the reduction is LOSSLESS for what matters -- every `needle` (the exact
//!      fact the paired live question depends on) survives verbatim.
//!
//! Layer 2 (the real-model probe: does a model still ANSWER correctly from the
//! reduced output?) is expressed as a `Workload` and driven through
//! `run_real_cell`, so it reuses the JSONL schema and behavior metrics. See
//! `workloads::probe_workloads`.
//!
//! Not every tool's advantage is the reduce flag (verified against the code,
//! not the design doc). Two render-probe axes (`ProbeAxis`):
//!   - `ReduceToggle`: reduce_output false vs true, args identical. `grep`
//!     (path grouping, issue-338), `find` (directory compaction, issue-340),
//!     `bash` (output filter, ADR-0037). `ls` does NOT respond (its `_reduce`
//!     is ignored, issue-339) so ls has no A/B probe and is absent here.
//!   - `ArgOverlay`: an always-on behavior toggled by a JSON arg, not the
//!     reduce flag. `read` skim (issue-337): baseline `skim:false` vs reduced
//!     `skim:true`, both at default reduce_output.
//!
//! `edit` (issue-341) is not a byte-reduction probe at all -- its advantage is
//! result-class correctness + the ADR-0038 conditional echo (exact success
//! stays terse; a tolerant match echoes the applied region). It has its own
//! deterministic probe (`edit_cases` / `run_edit_case`) asserting the outcome
//! CLASS + the on-disk effect, not a reduction ratio.
//!
//! Adding a tool is a data change: append a `ToolProbe` (or `EditCase`) row.

use std::cell::RefCell;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::nexus::{ClassifiedError, ToolEnv};
use crate::tools::bench_support::{assert_min_reduction, assert_survives_verbatim, reduction_pct};
use crate::tools::test_support::{TestDir, temp_dir};
use crate::tools::{ToolState, built_in_tools};

use super::fixtures::{build_find_tree, materialize};
use super::runner::block_on;

/// How a probe's workspace is produced.
pub(crate) enum ProbeWorkspace {
    /// A committed fixture dir under `bench_fixtures/tokens_per_task/`.
    Fixture(&'static str),
    /// A tree built programmatically at run time -- for inputs too large to
    /// commit (e.g. the >1000-file tree that trips find's compaction rail).
    Build(fn(&Path)),
}

/// What varies between a probe's baseline and reduced arm.
pub(crate) enum ProbeAxis {
    /// reduce_output false (baseline) vs true (reduced); args identical. For
    /// tools whose reduction is the default-on output filter.
    ReduceToggle,
    /// Both arms at default reduce_output; the reduced arm merges this object
    /// into the base args (shallow). For always-on behaviors a JSON arg toggles
    /// (e.g. read `{"skim": true}`), not the reduce flag.
    ArgOverlay(fn() -> Value),
}

/// One tool's render probe: fixed args over a workspace, compared across arms.
pub(crate) struct ToolProbe {
    /// Probe id (stable label for reports/JSONL).
    pub(crate) name: &'static str,
    /// Built-in tool to invoke (must resolve via `Tools::by_name`).
    pub(crate) tool: &'static str,
    /// How the probe's workspace is produced.
    pub(crate) workspace: ProbeWorkspace,
    /// What differs between the baseline and reduced arm.
    pub(crate) axis: ProbeAxis,
    /// Base tool arguments. For `ReduceToggle` they are identical across arms;
    /// for `ArgOverlay` they are the baseline and the reduced arm merges the
    /// overlay on top.
    pub(crate) args: fn() -> Value,
    /// Minimum token-reduction bar (percent), reduced vs baseline. A floor, not
    /// an exact figure (ADR-0036): the bar is the contract, not the measurement.
    pub(crate) min_reduction_pct: u32,
    /// Facts that MUST survive verbatim in the reduced output -- the exact
    /// values the paired live question asks for.
    pub(crate) needles: &'static [&'static str],
    /// Expensive to run (compiles a crate / heavy spawn), so it is excluded from
    /// the fast CI gate and run only via the opt-in slow render-probe test.
    pub(crate) slow: bool,
}

/// Outcome of one render probe: both rendered outputs and the measured ratio.
pub(crate) struct RenderProbe {
    pub(crate) baseline: String,
    pub(crate) reduced: String,
    pub(crate) reduction_pct: f64,
}

/// Materialize (or build) a probe's workspace into a fresh temp dir.
fn probe_workspace(probe: &ToolProbe) -> TestDir {
    match probe.workspace {
        ProbeWorkspace::Fixture(name) => materialize(name),
        ProbeWorkspace::Build(build) => {
            let dir = temp_dir();
            build(&dir.path);
            dir
        }
    }
}

/// Invoke one built-in tool by name over a prepared workspace with the given
/// reduce flag, and return the rendered tool-output text. Goes through the real
/// `Tool::execute` dispatch, so `reduce_output` drives the tool exactly as it
/// does in production.
fn render_over(workspace: &Path, tool: &str, args: &Value, reduce: bool) -> String {
    let tools = built_in_tools();
    let handle = tools
        .by_name(tool)
        .unwrap_or_else(|| panic!("unknown tool {tool}"));
    let state = RefCell::new(ToolState::new().with_reduce_output(reduce));
    let env = ToolEnv {
        workspace,
        state: &state,
        output_store: None,
        output_sink: None,
        mutation_guard: None,
        session_span: None,
    };
    let out = block_on(handle.execute(args, &env, CancellationToken::new()))
        .unwrap_or_else(|e| panic!("{tool} execute failed: {e}"));
    out.content
}

/// Run the render probe for one `ToolProbe` (both arms over one workspace).
pub(crate) fn run_render_probe(probe: &ToolProbe) -> RenderProbe {
    let workspace = probe_workspace(probe);
    let args = (probe.args)();
    let (baseline, reduced) = match &probe.axis {
        // Same args; the reduce flag is the only difference.
        ProbeAxis::ReduceToggle => (
            render_over(&workspace.path, probe.tool, &args, false),
            render_over(&workspace.path, probe.tool, &args, true),
        ),
        // Both arms at default reduce_output; the reduced arm merges the
        // overlay (e.g. read `skim:true`) into the base args.
        ProbeAxis::ArgOverlay(overlay) => {
            let mut reduced_args = args.clone();
            merge_object(&mut reduced_args, (overlay)());
            (
                render_over(&workspace.path, probe.tool, &args, true),
                render_over(&workspace.path, probe.tool, &reduced_args, true),
            )
        }
    };
    let reduction_pct = reduction_pct(&baseline, &reduced);
    RenderProbe {
        baseline,
        reduced,
        reduction_pct,
    }
}

/// Shallow-merge `overlay`'s object keys into `base` (both must be JSON
/// objects). Used to add the reduced arm's arg (e.g. `skim:true`) to the base.
fn merge_object(base: &mut Value, overlay: Value) {
    if let (Some(base), Some(overlay)) = (base.as_object_mut(), overlay.as_object()) {
        for (key, value) in overlay {
            base.insert(key.clone(), value.clone());
        }
    }
}

/// Assert a probe's render contract: the reduction clears its bar AND every
/// needle survives verbatim in the reduced output. Panics with the reduced text
/// on failure (the honest, debuggable form).
pub(crate) fn assert_render_contract(probe: &ToolProbe) -> RenderProbe {
    let result = run_render_probe(probe);
    assert_min_reduction(
        probe.name,
        &result.baseline,
        &result.reduced,
        probe.min_reduction_pct,
    );
    assert_survives_verbatim(probe.name, &result.reduced, probe.needles);
    result
}

/// The per-tool render-probe table (data-driven). Verified per tool against the
/// code: grep + find + bash respond to the reduce flag; ls does not (omitted).
pub(crate) fn tool_probes() -> Vec<ToolProbe> {
    vec![
        ToolProbe {
            name: "grep-deadline-repeated-matches",
            tool: "grep",
            workspace: ProbeWorkspace::Fixture("probe_grep"),
            axis: ProbeAxis::ReduceToggle,
            // Content mode, no context lines: isolate the path-dedup saving that
            // grep grouping (issue-338) delivers over the flat `path:line:` form.
            args: || json!({ "pattern": "deadline", "ignoreCase": true, "context": 0 }),
            min_reduction_pct: 20,
            needles: &["47231", "CHECKOUT_DEADLINE_MS"],
            slow: false,
        },
        ToolProbe {
            // find directory compaction (issue-340): with > 1000 matches the
            // default limit omits some, so the listing compacts; grouping shows
            // the same shown paths in fewer bytes (dir prefix shared once).
            name: "find-wide-tree-grouping",
            tool: "find",
            workspace: ProbeWorkspace::Build(build_find_tree),
            axis: ProbeAxis::ReduceToggle,
            args: || json!({ "pattern": "*.rs" }),
            min_reduction_pct: 20,
            needles: &["handler_zebra_target.rs"],
            slow: false,
        },
        ToolProbe {
            // read skim (issue-337, ADR-0036): an ALWAYS-ON behavior toggled by
            // the `skim` arg, NOT the reduce flag. A comment-heavy source skims
            // to its code signatures (whole-line comments + blank lines
            // stripped); every exported name/constant survives verbatim. The
            // never-worse guard means a thin file would fall back to full and
            // fail the bar -- this fixture is deliberately comment-dominated.
            name: "read-skim-comment-heavy",
            tool: "read",
            workspace: ProbeWorkspace::Fixture("probe_read"),
            axis: ProbeAxis::ArgOverlay(|| json!({ "skim": true })),
            args: || json!({ "path": "settlement.rs" }),
            min_reduction_pct: 20,
            // `due_ids` is a body-level local inside `sweep`; proving it
            // survives skim is what the paired live question depends on (a read
            // task, not a top-level symbol a grep answers trivially).
            needles: &[
                "CHECKOUT_DEADLINE_MS",
                "47231",
                "settlement_id",
                "PendingCharge",
                "due_ids",
            ],
            slow: false,
        },
        ToolProbe {
            // bash output filter (ADR-0037): a FAILING `cargo test` is
            // structured-filtered -- compile/run chatter collapses while the
            // failing test + assertion values are preserved. reduce=false forces
            // raw. Slow: it compiles the fixture crate, so it is opt-in, not in
            // the CI gate (the deterministic bash render corpus lives in
            // docs/benchmarks/adr-0037). Kept here to pair with the Phase 4 live
            // bash workload on the SAME fixture.
            name: "bash-cargo-test-failure",
            tool: "bash",
            workspace: ProbeWorkspace::Fixture("workload4_bash_diagnose"),
            axis: ProbeAxis::ReduceToggle,
            args: || json!({ "command": "cargo test" }),
            min_reduction_pct: 20,
            needles: &["ceiling_is_exact", "8191", "8192"],
            slow: true,
        },
    ]
}

// ---------------------------------------------------------------------------
// edit result-class probe (issue-341): assert the OUTCOME CLASS + disk effect.
//
// edit is not a byte-reduction probe. Its advantage is that it distinguishes
// five outcome classes with a stable machine token (ADR-0040) and keeps the
// common exact-success case terse while echoing a tolerant match (ADR-0038).
// Each case builds a tiny file, optionally reads it first (read-before-mutate),
// optionally mutates it out-of-band, runs one edit, and reports the class +
// whether disk changed. Deterministic; no provider.
// ---------------------------------------------------------------------------

/// One edit result-class case.
pub(crate) struct EditCase {
    /// Case id / stable label.
    pub(crate) name: &'static str,
    /// Initial contents written to `target.rs` in a fresh temp dir.
    pub(crate) initial: &'static str,
    /// `old_string` for the edit.
    pub(crate) old: &'static str,
    /// `new_string` for the edit.
    pub(crate) new: &'static str,
    /// Read the file first (satisfies read-before-mutate). `false` drives the
    /// `stale-file` unread case.
    pub(crate) pre_read: bool,
    /// Optional out-of-band disk mutation applied after the read, before the
    /// edit -- drives the `stale-file` modified case.
    pub(crate) mutate: Option<fn(&Path)>,
    /// Expected stable outcome class token (ADR-0040).
    pub(crate) expect_class: &'static str,
    /// Expected success (`Ok` vs classified `Err`).
    pub(crate) expect_ok: bool,
    /// Expected on-disk effect: did the file bytes change?
    pub(crate) expect_disk_changed: bool,
}

/// Result of running one `EditCase`.
pub(crate) struct EditOutcome {
    pub(crate) class: String,
    pub(crate) ok: bool,
    pub(crate) disk_changed: bool,
    /// Rendered success output length (0 on error) -- lets the caller prove an
    /// exact success stays terser than a tolerant success (ADR-0038 echo).
    pub(crate) output_len: usize,
}

/// Drive one edit case through the real read+edit dispatch on a shared
/// `ToolState` (so read-before-mutate tracking carries across the two calls),
/// and report the observed outcome class + disk effect.
pub(crate) fn run_edit_case(case: &EditCase) -> EditOutcome {
    let dir = temp_dir();
    let file = dir.path.join("target.rs");
    fs::write(&file, case.initial).expect("write edit fixture");
    let before = fs::read(&file).expect("read edit fixture");

    let tools = built_in_tools();
    let read_tool = tools.by_name("read").expect("read tool");
    let edit_tool = tools.by_name("edit").expect("edit tool");
    let state = RefCell::new(ToolState::new());
    let env = ToolEnv {
        workspace: &dir.path,
        state: &state,
        output_store: None,
        output_sink: None,
        mutation_guard: None,
        session_span: None,
    };

    if case.pre_read {
        // Full read (skim:false) satisfies read-before-mutate.
        let _ = block_on(read_tool.execute(
            &json!({ "path": "target.rs" }),
            &env,
            CancellationToken::new(),
        ));
    }
    if let Some(mutate) = case.mutate {
        mutate(&file);
    }

    let args = json!({
        "file_path": "target.rs",
        "old_string": case.old,
        "new_string": case.new,
    });
    let result = block_on(edit_tool.execute(&args, &env, CancellationToken::new()));
    let after = fs::read(&file).expect("re-read edit fixture");
    let disk_changed = before != after;

    match result {
        Ok(out) => EditOutcome {
            class: out
                .metadata
                .get("edit_outcome")
                .and_then(|v| v.as_str())
                .unwrap_or("exact")
                .to_string(),
            ok: true,
            disk_changed,
            output_len: out.content.len(),
        },
        Err(error) => EditOutcome {
            class: error
                .downcast_ref::<ClassifiedError>()
                .map(|c| c.class().to_string())
                .unwrap_or_else(|| "unclassified".to_string()),
            ok: false,
            disk_changed,
            output_len: 0,
        },
    }
}

/// Assert one edit case's contract: observed class, success flag, and on-disk
/// effect all match the case's expectations. Returns the outcome so the caller
/// can make cross-case assertions (e.g. exact terser than tolerant).
pub(crate) fn assert_edit_case(case: &EditCase) -> EditOutcome {
    let outcome = run_edit_case(case);
    assert_eq!(
        outcome.class, case.expect_class,
        "[{}] outcome class",
        case.name
    );
    assert_eq!(outcome.ok, case.expect_ok, "[{}] ok", case.name);
    assert_eq!(
        outcome.disk_changed, case.expect_disk_changed,
        "[{}] disk changed",
        case.name
    );
    outcome
}

/// The five edit outcome classes (ADR-0038/0040), each with its exact on-disk
/// effect. Data-driven: adding a class is a row.
pub(crate) fn edit_cases() -> Vec<EditCase> {
    vec![
        EditCase {
            name: "exact",
            initial: "let deadline = 47231;\nlet retries = 4;\n",
            old: "47231",
            new: "50000",
            pre_read: true,
            mutate: None,
            expect_class: "exact",
            expect_ok: true,
            expect_disk_changed: true,
        },
        EditCase {
            // Exact byte match fails (curly quotes in the file vs ASCII quotes
            // in old_string) but the fuzzy fallback folds Unicode quotes and
            // matches -> conditional echo fires (ADR-0038).
            name: "tolerant",
            initial: "let label = \u{201c}ready\u{201d};\n",
            old: "let label = \"ready\";",
            new: "let label = \"done\";",
            pre_read: true,
            mutate: None,
            expect_class: "tolerant-match-fired",
            expect_ok: true,
            expect_disk_changed: true,
        },
        EditCase {
            name: "not-found",
            initial: "let deadline = 47231;\n",
            old: "this text is absent",
            new: "x",
            pre_read: true,
            mutate: None,
            expect_class: "not-found",
            expect_ok: false,
            expect_disk_changed: false,
        },
        EditCase {
            // "= v;" occurs twice; without replace_all the match is ambiguous.
            name: "not-unique",
            initial: "let a = v;\nlet b = v;\n",
            old: "= v;",
            new: "= w;",
            pre_read: true,
            mutate: None,
            expect_class: "not-unique",
            expect_ok: false,
            expect_disk_changed: false,
        },
        EditCase {
            // No prior read -> read-before-mutate rejects the edit as stale.
            name: "stale-unread",
            initial: "let deadline = 47231;\n",
            old: "47231",
            new: "50000",
            pre_read: false,
            mutate: None,
            expect_class: "stale-file",
            expect_ok: false,
            expect_disk_changed: false,
        },
    ]
}
