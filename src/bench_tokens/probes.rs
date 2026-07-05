//! Per-tool micro-probes, layer 1: the DIRECT RENDER PROBE.
//!
//! Runs one tool by name over a fixture through the real dispatch, once with
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
//! Adding a tool is a data change: append a `ToolProbe` row.

use std::cell::RefCell;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::nexus::ToolEnv;
use crate::tools::bench_support::{assert_min_reduction, assert_survives_verbatim, reduction_pct};
use crate::tools::{ToolState, built_in_tools};

use super::fixtures::materialize;
use super::runner::block_on;

/// One tool's render probe: fixed args over a fixture, compared across arms.
pub(crate) struct ToolProbe {
    /// Probe id (stable label for reports/JSONL).
    pub(crate) name: &'static str,
    /// Built-in tool to invoke (must resolve via `Tools::by_name`).
    pub(crate) tool: &'static str,
    /// Fixture directory under `bench_fixtures/tokens_per_task/`.
    pub(crate) fixture: &'static str,
    /// Tool arguments (identical across arms; only the reduce flag differs).
    pub(crate) args: fn() -> Value,
    /// Minimum token-reduction bar (percent), reduced vs baseline. A floor, not
    /// an exact figure (ADR-0036): the bar is the contract, not the measurement.
    pub(crate) min_reduction_pct: u32,
    /// Facts that MUST survive verbatim in the reduced output -- the exact
    /// values the paired live question asks for.
    pub(crate) needles: &'static [&'static str],
}

/// Outcome of one render probe: both rendered outputs and the measured ratio.
pub(crate) struct RenderProbe {
    pub(crate) baseline: String,
    pub(crate) reduced: String,
    pub(crate) reduction_pct: f64,
}

/// Invoke one built-in tool by name over a freshly materialized fixture with the
/// given reduce flag, and return the rendered tool-output text. Goes through the
/// real `Tool::execute` dispatch, so `reduce_output` drives the tool exactly as
/// it does in production.
pub(crate) fn render_tool(tool: &str, fixture: &str, args: &Value, reduce: bool) -> String {
    let workspace = materialize(fixture);
    let tools = built_in_tools();
    let handle = tools
        .by_name(tool)
        .unwrap_or_else(|| panic!("unknown tool {tool}"));
    let state = RefCell::new(ToolState::new().with_reduce_output(reduce));
    let env = ToolEnv {
        workspace: &workspace.path,
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

/// Run the render probe for one `ToolProbe` (both arms).
pub(crate) fn run_render_probe(probe: &ToolProbe) -> RenderProbe {
    let args = (probe.args)();
    let baseline = render_tool(probe.tool, probe.fixture, &args, false);
    let reduced = render_tool(probe.tool, probe.fixture, &args, true);
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

/// The per-tool render-probe table (data-driven). Phase 5: grep only; find/ls/
/// bash rows land next as this template is proven.
pub(crate) fn tool_probes() -> Vec<ToolProbe> {
    vec![ToolProbe {
        name: "grep-deadline-repeated-matches",
        tool: "grep",
        fixture: "probe_grep",
        // Content mode, no context lines: isolate the path-dedup saving that
        // grep grouping (issue-338) delivers over the flat `path:line:` form.
        args: || {
            json!({
                "pattern": "deadline",
                "ignoreCase": true,
                "context": 0
            })
        },
        // Floor below the measured saving; grouping dedups a long nested path
        // across many matches. The render-probe gate test measures the actual %.
        min_reduction_pct: 20,
        // The exact planted value the paired live question asks for.
        needles: &["47231", "CHECKOUT_DEADLINE_MS"],
    }]
}
