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
//! Not every tool's reduction is arm-toggled (verified against the code, not
//! the design doc): `grep` (path grouping, issue-338), `find` (directory
//! compaction, issue-340) and `bash` (output filter, ADR-0037) respond to the
//! reduce flag; `ls` does NOT -- its `_reduce` is ignored (issue-339 is not
//! started), so ls has no A/B render probe and is deliberately absent here.
//!
//! Adding a tool is a data change: append a `ToolProbe` row.

use std::cell::RefCell;
use std::path::Path;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::nexus::ToolEnv;
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

/// One tool's render probe: fixed args over a workspace, compared across arms.
pub(crate) struct ToolProbe {
    /// Probe id (stable label for reports/JSONL).
    pub(crate) name: &'static str,
    /// Built-in tool to invoke (must resolve via `Tools::by_name`).
    pub(crate) tool: &'static str,
    /// How the probe's workspace is produced.
    pub(crate) workspace: ProbeWorkspace,
    /// Tool arguments (identical across arms; only the reduce flag differs).
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
    let baseline = render_over(&workspace.path, probe.tool, &args, false);
    let reduced = render_over(&workspace.path, probe.tool, &args, true);
    let reduction_pct = reduction_pct(&baseline, &reduced);
    RenderProbe {
        baseline,
        reduced,
        reduction_pct,
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
            args: || json!({ "pattern": "*.rs" }),
            min_reduction_pct: 20,
            needles: &["handler_zebra_target.rs"],
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
            args: || json!({ "command": "cargo test" }),
            min_reduction_pct: 20,
            needles: &["ceiling_is_exact", "8191", "8192"],
            slow: true,
        },
    ]
}
