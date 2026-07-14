//! Drivers + records: the deterministic replay arm, the opt-in real-provider
//! cell, run metrics/records, model selection, and JSONL logging.

use std::cell::RefCell;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::nexus::{Agent, ApprovalMode, ToolEnv};
use crate::tools::bench_support::est_tokens;
use crate::tools::{ToolState, built_in_tools};

use super::arms::Arm;
use super::fixtures::materialize;
use super::observer::{BenchObserver, ZeroPromptGate};
use super::provider::ScriptedProvider;
use super::workloads::{ApprovalProfile, Outcome, Workload};

/// JSONL run-record schema version. Bump when fields are added/renamed so an
/// analyzer can branch on shape; readers must tolerate unknown extra fields.
/// v3: every record carries a `kind` discriminator (`real_cell` /
/// `real_cell_error` / `render_probe`), so a run logs ALL results -- successes,
/// unreachable/errored cells, and deterministic render measurements alike.
const BENCH_SCHEMA_VERSION: u32 = 3;

/// Metrics from one workload x arm run.
pub(crate) struct RunMetrics {
    pub(crate) arm: Arm,
    /// Estimated cumulative input tokens (replay proxy) OR real provider input
    /// tokens (headline). See the field the caller reads.
    pub(crate) cumulative_proxy: usize,
    pub(crate) final_context_proxy: usize,
    pub(crate) provider_turns: u32,
    pub(crate) approvals_consulted: bool,
    /// The final transcript text the agent saw (replay path only; empty for the
    /// real path). Used for the needle-survival assertion.
    pub(crate) transcript: String,
    pub(crate) outcome: Outcome,
}

/// Drive one workload x arm with the scripted replay provider under the auto
/// preset + zero-prompt gate. The fixture is materialized fresh so edits never
/// touch the committed copy.
pub(crate) fn run_replay_arm(workload: &Workload, arm: Arm) -> RunMetrics {
    let workspace = materialize(workload.fixture);
    if let Some(build) = workload.build {
        build(&workspace.path);
    }
    let provider = ScriptedProvider::new((workload.script)());
    let mut agent = Agent::new(provider, built_in_tools());
    agent.set_approval_mode(ApprovalMode::Auto);

    let state = RefCell::new(ToolState::new().with_reduce_output(arm.reduce()));
    let env = ToolEnv {
        workspace: &workspace.path,
        state: &state,
        output_store: None,
        output_sink: None,
        mutation_guard: None,
        session_span: None,
    };
    let observer = BenchObserver::default();
    let gate = ZeroPromptGate::default();
    block_on(agent.submit_turn(
        workload.prompt,
        &observer,
        &gate,
        &env,
        &CancellationToken::new(),
        None,
    ))
    .expect("replay turn completes");

    let outcome = (workload.check)(&workspace.path, &observer.final_text());
    RunMetrics {
        arm,
        cumulative_proxy: agent.provider.cumulative_input_proxy(),
        final_context_proxy: agent.provider.final_context_proxy(),
        provider_turns: observer.provider_turns.get(),
        approvals_consulted: gate.consulted.get(),
        transcript: agent.provider.final_transcript_text(),
        outcome,
    }
}

/// Outcome of a scripted run under `--dangerously-skip-permissions` -- the
/// deterministic proof that ADR-0049 unlocks `bash` in the harness (the gate is
/// bypassed, bash executes in the confined temp workspace, and its exit code is
/// captured). Free and CI-safe: no real provider.
pub(crate) struct ScriptedSkipRun {
    pub(crate) approvals_consulted: bool,
    pub(crate) dangerous_approvals: u32,
    pub(crate) bash_exit_codes: Vec<i32>,
    pub(crate) tool_errors: Vec<(String, String)>,
    pub(crate) tool_result_bytes: u64,
    pub(crate) outcome: Outcome,
}

/// Whether a repair workload's fixture is genuinely broken BEFORE the model (or
/// scripted provider) runs. Model-independent: it runs the workload's own
/// mechanical check against the pristine, built-but-unmodified workspace and
/// treats "check fails" as "starts broken".
///
/// This replaces gating validity on the model's recorded bash exit codes. A real
/// model routinely pipes `cargo test` through `head`/`tail` to bound output, so
/// the recorded process exit becomes 0 even though the test failed -- the old
/// gate then marked a perfectly good repair invalid because no `101` was seen.
/// That measured shell style, not the repair. Bracketing on the fixture's actual
/// pre-state is robust to how the model chooses to invoke the tests.
fn fixture_starts_broken(workload: &Workload, workspace: &std::path::Path) -> bool {
    !(workload.check)(workspace, "").success
}

/// Enforce that a repair workload's fixture was genuinely broken before the run.
/// If it was not (the task was already green -- a fixture bug), the run is
/// invalid and cannot count as success. When it did start broken, the post-run
/// mechanical check alone decides success, so a real-but-unfixed attempt stays
/// VALID (a first-class safety/non-regression signal) instead of being dropped.
fn enforce_started_broken(
    workload: &Workload,
    outcome: &mut Outcome,
    started_broken: bool,
) -> bool {
    if !workload.require_failing_then_passing_bash {
        return true;
    }
    if !started_broken {
        outcome.success = false;
        outcome.detail =
            "fixture did not start in a failing state before the run (workload bug); run invalid"
                .to_string();
        return false;
    }
    true
}

/// Drive one workload x arm with the scripted replay provider under
/// `Agent::with_skip_permissions(true)`, so a scripted `bash` call actually
/// runs (the deny gate is bypassed). The denying gate is still installed so we
/// can prove it was NOT consulted (the bypass fired first).
pub(crate) fn run_scripted_skip_perms(workload: &Workload, arm: Arm) -> ScriptedSkipRun {
    let workspace = materialize(workload.fixture);
    if let Some(build) = workload.build {
        build(&workspace.path);
    }
    assert!(
        !workspace.path.starts_with(env!("CARGO_MANIFEST_DIR")),
        "bench workspace must be a temp dir, not the repo: {}",
        workspace.path.display()
    );
    let started_broken = fixture_starts_broken(workload, &workspace.path);
    let provider = ScriptedProvider::new((workload.script)());
    let mut agent = Agent::new(provider, built_in_tools()).with_skip_permissions(true);
    agent.set_approval_mode(ApprovalMode::Auto);

    let state = RefCell::new(ToolState::new().with_reduce_output(arm.reduce()));
    let env = ToolEnv {
        workspace: &workspace.path,
        state: &state,
        output_store: None,
        output_sink: None,
        mutation_guard: None,
        session_span: None,
    };
    let observer = BenchObserver::default();
    let gate = ZeroPromptGate::default();
    block_on(agent.submit_turn(
        workload.prompt,
        &observer,
        &gate,
        &env,
        &CancellationToken::new(),
        None,
    ))
    .expect("scripted skip-perms turn completes");

    let bash_exit_codes = observer.bash_exit_codes.borrow().clone();
    let tool_errors = observer.tool_errors.borrow().clone();
    let mut outcome = (workload.check)(&workspace.path, &observer.final_text());
    enforce_started_broken(workload, &mut outcome, started_broken);
    ScriptedSkipRun {
        approvals_consulted: gate.consulted.get(),
        dangerous_approvals: observer.dangerous_approvals.get(),
        bash_exit_codes,
        tool_errors,
        tool_result_bytes: observer.tool_result_bytes.get(),
        outcome,
    }
}

/// Benchmark reasoning effort, held IDENTICAL across arms (it is a confounder).
/// `IRIS_BENCH_REASONING` overrides; default `low` -- the agreed cost-conscious
/// setting (reasoning tokens are output-side and add cost/variance without
/// sharpening the input-reduction signal this benchmark measures).
pub(crate) fn bench_reasoning() -> Option<crate::mimir::selection::ReasoningEffort> {
    let raw = std::env::var("IRIS_BENCH_REASONING").unwrap_or_else(|_| "low".to_string());
    if raw.trim().eq_ignore_ascii_case("none") {
        return None;
    }
    Some(
        crate::mimir::selection::ReasoningEffort::parse(raw.trim())
            .expect("valid IRIS_BENCH_REASONING"),
    )
}

/// Soft cap on tool round-trips for a real bench cell. The agent loop is
/// unbounded by default (it ends only when the model stops calling tools), so a
/// stuck live session could loop for a long time -- and run many-way in
/// parallel that burns tokens and can hang the whole batch. `IRIS_BENCH_MAX_ROUNDTRIPS`
/// overrides the project/user setting; when neither is set we apply a generous
/// bounded cap so a wedged cell ends gracefully. A completed chained repair here
/// needs only a handful of round-trips, so this never truncates real work.
pub(crate) fn bench_max_roundtrips(settings: &crate::config::Settings) -> Option<usize> {
    if let Ok(raw) = std::env::var("IRIS_BENCH_MAX_ROUNDTRIPS") {
        let parsed: usize = raw
            .trim()
            .parse()
            .expect("valid IRIS_BENCH_MAX_ROUNDTRIPS (a positive integer)");
        return Some(parsed);
    }
    settings.max_tool_roundtrips().or(Some(40))
}

/// Build a `ModelSelection` for a `provider:model` spec, overriding provider,
/// model, base URL, and reasoning on top of a config-resolved base (so cache /
/// retry / context-management defaults are inherited). Used by the smoke and
/// headline paths to drive an explicit model matrix.
pub(crate) fn selection_for_spec(
    cwd: &std::path::Path,
    spec: &str,
    reasoning: Option<crate::mimir::selection::ReasoningEffort>,
) -> std::result::Result<crate::mimir::selection::ModelSelection, String> {
    use crate::mimir::selection::{ModelSelection, ProviderId, base_url_for};
    let (provider_str, model) = spec
        .split_once(':')
        .ok_or_else(|| format!("model spec {spec:?} must be 'provider:model'"))?;
    let provider = ProviderId::parse(provider_str).map_err(|e| e.to_string())?;
    let settings = crate::config::Settings::load(cwd).map_err(|e| e.to_string())?;
    let mut selection = ModelSelection::resolve(&settings).map_err(|e| e.to_string())?;
    selection.provider = provider;
    selection.model = model.trim().to_string();
    selection.base_url = base_url_for(provider, None);
    selection.reasoning = reasoning;
    crate::mimir::model_capabilities::validate(&selection).map_err(|e| e.to_string())?;
    Ok(selection)
}

/// Rich outcome of one real-provider cell (the unit we log and aggregate).
pub(crate) struct RealRunRecord {
    pub(crate) valid: bool,
    pub(crate) arm: Arm,
    pub(crate) outcome: Outcome,
    pub(crate) turns: u32,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) reasoning_tokens: u64,
    pub(crate) cache_read: u64,
    pub(crate) total_tokens: u64,
    pub(crate) tool_counts: std::collections::BTreeMap<String, u32>,
    pub(crate) handles_stored: u32,
    pub(crate) per_turn: Vec<(u64, u64)>,
    pub(crate) approvals_consulted: bool,
    pub(crate) dangerous_approvals: u32,
    pub(crate) tool_sequence: Vec<String>,
    pub(crate) tool_errors: Vec<(String, String)>,
    pub(crate) tool_result_bytes: u64,
    pub(crate) tool_result_bytes_by_tool: std::collections::BTreeMap<String, u64>,
    pub(crate) bash_exit_codes: Vec<i32>,
}

impl RealRunRecord {
    pub(crate) fn tool_calls_total(&self) -> u32 {
        self.tool_counts.values().sum()
    }
    /// Mean input tokens per provider turn -- the factor the reduction lever
    /// actually moves, isolated from the (noisy, model-chosen) turn count.
    pub(crate) fn tokens_per_turn(&self) -> f64 {
        if self.turns == 0 {
            0.0
        } else {
            self.input_tokens as f64 / self.turns as f64
        }
    }
}

/// JSONL run-log path (override with `IRIS_BENCH_LOG`). One line per real run,
/// with every field captured -- the durable record for offline statistics.
pub(crate) fn bench_log_path() -> String {
    std::env::var("IRIS_BENCH_LOG")
        .unwrap_or_else(|_| "target/tokens-per-task-runs.jsonl".to_string())
}

pub(crate) fn bench_log_reset() {
    let _ = std::fs::write(bench_log_path(), "");
}

fn bench_shard() -> Option<String> {
    std::env::var("IRIS_BENCH_SHARD")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn bench_run_global(run: usize) -> usize {
    let offset = std::env::var("IRIS_BENCH_RUN_OFFSET")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    offset + run
}

fn bench_log_append(line: &Value) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(bench_log_path())
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Log an unreachable/failed live cell so a run records ALL results, not just
/// the reachable ones (no silent drops). `reason` is the backend/selection
/// message; the cell is marked invalid so the analyzer excludes it from token
/// stats while still counting it as attempted.
pub(crate) fn bench_log_cell_error(
    model: &str,
    workload: &str,
    arm: &str,
    run: usize,
    reason: &str,
) {
    bench_log_append(&json!({
        "schema_version": BENCH_SCHEMA_VERSION,
        "kind": "real_cell_error",
        "model": model,
        "workload": workload,
        "arm": arm,
        "run": run,
        "run_global": bench_run_global(run),
        "shard": bench_shard(),
        "valid": false,
        "error": reason,
    }));
}

/// Log one deterministic render-probe measurement (proxy tokens, both arms,
/// needle survival) so the analyzer can correlate a tool's render reduction
/// with its live outcome. Deterministic, so it is logged on demand (not in the
/// CI gate).
pub(crate) fn bench_log_render_probe(
    probe: &str,
    tool: &str,
    baseline: &str,
    reduced: &str,
    reduction_pct: f64,
    needles_survived: bool,
) {
    bench_log_append(&json!({
        "schema_version": BENCH_SCHEMA_VERSION,
        "kind": "render_probe",
        "probe": probe,
        "tool": tool,
        "baseline_bytes": baseline.len(),
        "reduced_bytes": reduced.len(),
        "baseline_proxy_tokens": est_tokens(baseline),
        "reduced_proxy_tokens": est_tokens(reduced),
        "reduction_pct": reduction_pct,
        "needles_survived": needles_survived,
    }));
}

/// Run one real-provider cell for an explicit selection, capturing rich
/// per-run/per-turn data and appending it to the JSONL log. Fallible: a backend
/// rejection (bad model id, unsupported thinking level, auth) is returned as
/// `Err(message)` so the smoke can report per-model reachability instead of
/// aborting the whole matrix.
pub(crate) fn run_real_cell(
    model: &str,
    workload: &Workload,
    arm: Arm,
    run: usize,
    selection: &crate::mimir::selection::ModelSelection,
) -> std::result::Result<RealRunRecord, String> {
    let workspace = materialize(workload.fixture);
    if let Some(build) = workload.build {
        build(&workspace.path);
    }
    let cwd = workspace.path.clone();
    // Confinement guard: the fixture always materializes into a temp dir, never
    // the repo tree. Under skip-permissions bash runs here, so prove it can
    // never touch the real workspace.
    assert!(
        !cwd.starts_with(env!("CARGO_MANIFEST_DIR")),
        "bench workspace escaped to the repo tree: {}",
        cwd.display()
    );
    // Bracket the repair: confirm the fixture is genuinely broken before the
    // model touches it (model-independent), so validity does not depend on how
    // the model plumbs its test command's exit code.
    let started_broken = fixture_starts_broken(workload, &cwd);
    let tools = built_in_tools();
    let system_prompt = crate::wayland::system_prompt::assemble(&cwd, &tools);
    let settings = crate::config::Settings::load(&cwd).map_err(|e| e.to_string())?;
    let session_id = crate::session::new_session_id();
    let provider = crate::build_provider(selection, &system_prompt, &session_id, &cwd)
        .map_err(|e| format!("build provider: {e}"))?;
    let mut agent = Agent::new(provider, built_in_tools())
        .with_max_tool_roundtrips(bench_max_roundtrips(&settings));
    // Skip-permissions workloads (bash) bypass the approval gate for every
    // gated call (ADR-0049); no-bash workloads keep the deny gate.
    if matches!(workload.approval, ApprovalProfile::SkipPermissions) {
        agent = agent.with_skip_permissions(true);
    }
    agent.set_approval_mode(ApprovalMode::Auto);

    let state = RefCell::new(ToolState::new().with_reduce_output(arm.reduce()));
    let env = ToolEnv {
        workspace: &cwd,
        state: &state,
        output_store: None,
        output_sink: None,
        mutation_guard: None,
        session_span: None,
    };
    let observer = BenchObserver::default();
    let gate = ZeroPromptGate::default();
    block_on(agent.submit_turn(
        workload.prompt,
        &observer,
        &gate,
        &env,
        &CancellationToken::new(),
        None,
    ))
    .map_err(|e| format!("provider turn: {e}"))?;

    let bash_exit_codes = observer.bash_exit_codes.borrow().clone();
    let mut outcome = (workload.check)(&cwd, &observer.final_text());
    let valid = enforce_started_broken(workload, &mut outcome, started_broken);
    let record = RealRunRecord {
        valid,
        arm,
        outcome,
        turns: observer.provider_turns.get(),
        input_tokens: observer.usage_input_tokens.get(),
        output_tokens: observer.output_tokens.get(),
        reasoning_tokens: observer.reasoning_tokens.get(),
        cache_read: observer.cache_read.get(),
        total_tokens: observer.total_tokens.get(),
        tool_counts: observer.tool_counts.borrow().clone(),
        handles_stored: observer.handles_stored.get(),
        per_turn: observer.per_turn.borrow().clone(),
        approvals_consulted: gate.consulted.get(),
        dangerous_approvals: observer.dangerous_approvals.get(),
        tool_sequence: observer.tool_sequence.borrow().clone(),
        tool_errors: observer.tool_errors.borrow().clone(),
        tool_result_bytes: observer.tool_result_bytes.get(),
        tool_result_bytes_by_tool: observer.tool_result_bytes_by_tool.borrow().clone(),
        bash_exit_codes,
    };
    bench_log_append(&json!({
        "schema_version": BENCH_SCHEMA_VERSION,
        "kind": "real_cell",
        "valid": record.valid,
        "model": model,
        "workload": workload.name,
        "arm": record.arm.label(),
        "reduce_output": arm.reduce(),
        "run": run,
        "run_global": bench_run_global(run),
        "shard": bench_shard(),
        "reasoning": format!("{:?}", selection.reasoning),
        "success": record.outcome.success,
        "detail": record.outcome.detail,
        "turns": record.turns,
        "input_tokens": record.input_tokens,
        "output_tokens": record.output_tokens,
        "reasoning_tokens": record.reasoning_tokens,
        "cache_read_tokens": record.cache_read,
        "total_tokens": record.total_tokens,
        "tokens_per_turn": record.tokens_per_turn(),
        "tool_calls_total": record.tool_calls_total(),
        "tool_counts": record.tool_counts,
        "handles_stored": record.handles_stored,
        "approvals": record.approvals_consulted,
        "dangerous_approvals": record.dangerous_approvals,
        "tool_sequence": record.tool_sequence,
        "tool_errors": record
            .tool_errors
            .iter()
            .map(|(name, message)| json!({ "name": name, "message": message }))
            .collect::<Vec<_>>(),
        "tool_result_bytes": record.tool_result_bytes,
        "tool_result_bytes_by_tool": record.tool_result_bytes_by_tool,
        "bash_exit_codes": record.bash_exit_codes,
        "per_turn": record
            .per_turn
            .iter()
            .map(|(i, o)| json!({ "in": i, "out": o }))
            .collect::<Vec<_>>(),
    }));
    Ok(record)
}

/// Default smoke/headline model matrix (all on OAuth/subscription lanes
/// reachable with existing credentials). Override with `IRIS_BENCH_MODELS`
/// (comma-separated `provider:model`).
///
/// Antigravity/Gemini is EXCLUDED for now: the provider hardcodes `usage: None`
/// (`antigravity.rs`), so it reports 0 usage tokens and cannot produce a
/// tokens-per-task number (smoke Entry 11). Re-add `antigravity:gemini-3.5-flash`
/// once the Antigravity adapter parses Gemini `usageMetadata`.
const DEFAULT_MODEL_SPECS: &[&str] = &[
    "openai-codex:gpt-5.4-mini",
    "openai-codex:gpt-5.3-codex-spark",
    "anthropic:claude-haiku-4-5",
];

pub(crate) fn model_specs() -> Vec<String> {
    match std::env::var("IRIS_BENCH_MODELS") {
        Ok(v) if !v.trim().is_empty() => v
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => DEFAULT_MODEL_SPECS.iter().map(|s| s.to_string()).collect(),
    }
}

/// Drive one async future to completion on a current-thread runtime.
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(future)
}

#[cfg(test)]
mod tests {
    use super::bench_max_roundtrips;
    use crate::config::Settings;

    /// The real-cell loop must be bounded: unset -> a generous default cap,
    /// `IRIS_BENCH_MAX_ROUNDTRIPS` -> exactly that cap. Env is process-global, so
    /// serialize through the shared env lock.
    #[test]
    fn bench_max_roundtrips_defaults_bounded_and_env_overrides() {
        let _env = crate::mimir::test_support::env_lock();
        let settings = Settings::default();
        assert_eq!(settings.max_tool_roundtrips(), None);

        // Unset -> bounded default (never unbounded for a live cell).
        unsafe {
            std::env::remove_var("IRIS_BENCH_MAX_ROUNDTRIPS");
        }
        assert_eq!(bench_max_roundtrips(&settings), Some(40));

        // Override wins over the default.
        unsafe {
            std::env::set_var("IRIS_BENCH_MAX_ROUNDTRIPS", "12");
        }
        assert_eq!(bench_max_roundtrips(&settings), Some(12));

        unsafe {
            std::env::remove_var("IRIS_BENCH_MAX_ROUNDTRIPS");
        }
    }
}
