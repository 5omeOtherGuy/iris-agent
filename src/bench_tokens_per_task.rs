//! End-to-end tokens-per-completed-task benchmark harness (issue #210,
//! Milestone 2). Proves that Iris's default-on tool-output reductions
//! (grep grouping #338, find grouping #340) lower the prompt tokens spent to
//! COMPLETE a realistic task, without lowering task success.
//!
//! Two paths share one driver ([`run_arm`]):
//! - **Replay (CI, deterministic, no cost):** a [`ScriptedProvider`] replays a
//!   fixed, successful tool-call script per workload. The real built-in tools
//!   run over committed fixtures, so tool OUTPUTS are real; only the assistant's
//!   tool-call CHOICES are scripted. Prompt tokens are an estimated proxy over
//!   the transcript the provider is sent each turn (`bench_support::est_tokens`,
//!   4 bytes/token) -- a ratio, never presented as exact tokens. Asserts, per
//!   workload: (a) the mechanical success check passes in both arms, (b) arm A
//!   (defaults) < arm B (baseline) in proxy tokens by a margin, (c) zero
//!   approval prompts.
//! - **Headline (opt-in, real provider, costs money):** the `#[ignore]`d
//!   [`tokens_per_task_headline`] test runs the real provider N>=3 times per
//!   cell and reads REAL usage records; gated behind `IRIS_BENCH_REAL=1` so CI
//!   never spends money. See `docs/BENCHMARK_PLAN.md`.
//!
//! Both paths run under the ADR-0032 auto preset with a zero-prompt gate,
//! identical across arms; the safety floors stay active and the agent never
//! calls `bash` (auto-bash is deferred), so a workload that would prompt under
//! auto is a harness/workload bug, caught by the zero-prompt assertion.

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{
    Agent, AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ApprovalMode,
    AssistantTurn, ChatProvider, CompletionReason, Message, ProviderEvent, ProviderStream,
    ReviewContext, ToolCall, ToolEnv, ToolEventState, Tools,
};
use crate::tools::test_support::{TestDir, temp_dir};
use crate::tools::{ToolState, bench_support, built_in_tools};

// ---------------------------------------------------------------------------
// Arms
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    /// Iris defaults: bash filter, grep grouping, find grouping active.
    Defaults,
    /// Baseline: default-on reductions disabled (the benchmark-only switch).
    Baseline,
}

impl Arm {
    /// Whether tool output reductions are active for this arm.
    fn reduce(self) -> bool {
        matches!(self, Arm::Defaults)
    }

    fn label(self) -> &'static str {
        match self {
            Arm::Defaults => "A (defaults)",
            Arm::Baseline => "B (baseline)",
        }
    }
}

// ---------------------------------------------------------------------------
// Scripted replay provider
// ---------------------------------------------------------------------------

/// A provider that replays a fixed sequence of assistant turns regardless of
/// input, recording the messages it is sent each turn so the harness can
/// estimate the transcript size (the arm token proxy). The tool calls in the
/// script are chosen so the real tools run over the fixtures; the outputs are
/// real and differ between arms because the reductions differ.
struct ScriptedProvider {
    turns: RefCell<std::collections::VecDeque<AssistantTurn>>,
    seen: RefCell<Vec<Vec<Message>>>,
}

impl ScriptedProvider {
    fn new(turns: Vec<AssistantTurn>) -> Self {
        Self {
            turns: RefCell::new(turns.into_iter().collect()),
            seen: RefCell::new(Vec::new()),
        }
    }

    /// Cumulative estimated input tokens: sum over every provider call of the
    /// estimated tokens of the transcript it was sent. Mirrors how a real
    /// provider bills input (the growing transcript, re-sent each turn). Same
    /// estimator both arms; only the ratio is meaningful.
    fn cumulative_input_proxy(&self) -> usize {
        self.seen
            .borrow()
            .iter()
            .map(|messages| transcript_proxy_tokens(messages))
            .sum()
    }

    /// Estimated tokens of the final (largest) transcript the provider saw --
    /// the accumulated context after every tool result landed.
    fn final_context_proxy(&self) -> usize {
        self.seen
            .borrow()
            .last()
            .map(|messages| transcript_proxy_tokens(messages))
            .unwrap_or(0)
    }

    /// The full text of the final transcript the provider saw -- every message
    /// content, including the tool RESULTS. Used to assert that the reduced
    /// (arm A) tool output still surfaced the facts the task needed (the
    /// end-to-end "without quality loss" contract), not just that a scripted
    /// answer mentioned them.
    fn final_transcript_text(&self) -> String {
        self.seen
            .borrow()
            .last()
            .map(|messages| {
                messages
                    .iter()
                    .map(|message| message.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
}

/// Estimated tokens of a transcript, summed over message content (where the
/// tool outputs -- and thus the arm difference -- live).
fn transcript_proxy_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| bench_support::est_tokens(&message.content))
        .sum()
}

impl ChatProvider for ScriptedProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        self.seen.borrow_mut().push(messages.to_vec());
        let turn = self
            .turns
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| AssistantTurn::text("(script exhausted)"));
        let event = Ok(ProviderEvent::Completed(turn));
        Ok(Box::pin(futures::stream::once(async move { event })))
    }
}

// ---------------------------------------------------------------------------
// Observer + zero-prompt gate
// ---------------------------------------------------------------------------

/// Rich per-run instrumentation. Beyond the headline input-token total, it
/// captures the per-turn token trajectory, output/reasoning/cache tokens, a
/// per-tool call histogram (re-running grep or re-reading a file shows up here
/// -- the signal for "the reduced output was harder to interpret"), and handle
/// offloads. All of it is logged as JSONL so no observation is thrown away.
#[derive(Default)]
struct BenchObserver {
    final_text: RefCell<String>,
    usage_input_tokens: Cell<u64>,
    output_tokens: Cell<u64>,
    reasoning_tokens: Cell<u64>,
    cache_read: Cell<u64>,
    total_tokens: Cell<u64>,
    provider_turns: Cell<u32>,
    /// Successful tool executions keyed by tool name (grep/read/edit/find/ls).
    tool_counts: RefCell<std::collections::BTreeMap<String, u32>>,
    /// Count of large outputs offloaded behind a handle (more likely in arm B).
    handles_stored: Cell<u32>,
    /// Per provider turn: (input_tokens, output_tokens), in order.
    per_turn: RefCell<Vec<(u64, u64)>>,
}

impl BenchObserver {
    fn final_text(&self) -> String {
        self.final_text.borrow().clone()
    }
}

impl AgentObserver for BenchObserver {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        match event {
            AgentEvent::AssistantText(text) | AgentEvent::AssistantTextEnd(text)
                if !text.is_empty() =>
            {
                *self.final_text.borrow_mut() = text;
            }
            AgentEvent::ProviderTurnCompleted { usage, .. } => {
                self.provider_turns.set(self.provider_turns.get() + 1);
                let (mut inp, mut out) = (0u64, 0u64);
                if let Some(usage) = usage {
                    inp = usage.input_tokens;
                    out = usage.output_tokens;
                    self.usage_input_tokens
                        .set(self.usage_input_tokens.get() + usage.input_tokens);
                    self.output_tokens
                        .set(self.output_tokens.get() + usage.output_tokens);
                    self.reasoning_tokens
                        .set(self.reasoning_tokens.get() + usage.reasoning_output_tokens);
                    self.cache_read
                        .set(self.cache_read.get() + usage.cache_read_input_tokens);
                    self.total_tokens
                        .set(self.total_tokens.get() + usage.total_tokens);
                }
                self.per_turn.borrow_mut().push((inp, out));
            }
            // Count each SUCCESSFUL tool execution by name -- extra grep/read
            // calls are the fingerprint of the model re-looking because a
            // reduced output was harder to interpret.
            AgentEvent::ToolLifecycle {
                name,
                state: ToolEventState::Succeeded,
                ..
            } => {
                *self.tool_counts.borrow_mut().entry(name).or_insert(0) += 1;
            }
            AgentEvent::OutputHandleStored { .. } => {
                self.handles_stored.set(self.handles_stored.get() + 1);
            }
            _ => {}
        }
        Ok(())
    }
}

/// Approval gate that must never be consulted: under the auto preset with only
/// auto-approvable tools (read/grep/find + clean in-workspace edit), no call
/// reaches the gate. If it is consulted, the run is invalid (a prompt occurred);
/// it records the fact and denies so the run cannot silently proceed.
#[derive(Default)]
struct ZeroPromptGate {
    consulted: Cell<bool>,
}

impl ApprovalGate for ZeroPromptGate {
    fn review<'a>(
        &'a self,
        _call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        _ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        self.consulted.set(true);
        Box::pin(async move { Ok(ApprovalDecision::Deny) })
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The committed fixtures root (`src/bench_fixtures/tokens_per_task/`).
fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bench_fixtures/tokens_per_task")
}

/// Copy a fixture tree into a fresh temp workspace, stripping the `.txt`
/// suffix every committed fixture file carries (so fmt/clippy/typos never treat
/// them as live sources). Returns the temp dir (auto-cleaned on drop).
fn materialize(fixture: &str) -> TestDir {
    let dir = temp_dir();
    copy_stripping_txt(&fixtures_root().join(fixture), &dir.path);
    dir
}

fn copy_stripping_txt(src: &Path, dst: &Path) {
    for entry in fs::read_dir(src).expect("fixture dir readable") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type().expect("file type").is_dir() {
            let sub = dst.join(&name);
            fs::create_dir_all(&sub).expect("create fixture subdir");
            copy_stripping_txt(&entry.path(), &sub);
        } else {
            let target = name.strip_suffix(".txt").unwrap_or(&name);
            let bytes = fs::read(entry.path()).expect("read fixture file");
            fs::write(dst.join(target), bytes).expect("write materialized fixture");
        }
    }
}

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------

/// The result of a workload's mechanical success check.
struct Outcome {
    success: bool,
    detail: String,
}

struct Workload {
    name: &'static str,
    fixture: &'static str,
    prompt: &'static str,
    /// The scripted tool-call sequence the replay provider replays (real path
    /// ignores it; the real model chooses its own calls).
    script: fn() -> Vec<AssistantTurn>,
    /// Mechanical success check run OUTSIDE the agent turn (harness-side), so
    /// the agent never needs `bash`.
    check: fn(&Path, &str) -> Outcome,
    /// Facts the tool outputs MUST surface verbatim for the task to be solvable
    /// from context. Asserted present in the transcript the agent saw in BOTH
    /// arms, so a reduction that dropped an actionable fact fails the run even
    /// though the scripted answer would still mention it.
    needles: &'static [&'static str],
}

fn workloads() -> Vec<Workload> {
    vec![
        Workload {
            name: "fix-failing-test",
            fixture: "workload1_fix_test",
            prompt: "The test `parse_len_counts_all_tokens` fails. Find and fix the bug in \
                     parse_len using read/grep/find and edit only. Do not run any shell \
                     commands; the test will be run for you.",
            script: script_fix_test,
            check: check_fix_test,
            // The grep across files must surface the buggy symbol and the read
            // must surface the buggy expression the fix targets.
            needles: &["parse_len", "split_whitespace().count() - 1"],
        },
        Workload {
            name: "multi-file-search-and-edit",
            fixture: "workload2_rename",
            prompt: "Rename the identifier MAX_RETRIES to MAX_ATTEMPTS everywhere it appears \
                     in this tree (code and docs). Use grep/find to locate every occurrence \
                     and edit to change them. Do not run any shell commands.",
            script: script_rename,
            check: check_rename,
            // The grep must surface the identifier being renamed.
            needles: &["MAX_RETRIES"],
        },
        Workload {
            name: "investigate-large-log",
            fixture: "workload3_log_triage",
            prompt: "One test failed with a token-budget ceiling assertion. Search the logs/ \
                     directory to find which test failed and the exact left/right values it \
                     reported. Answer in one sentence. Do not run any shell commands.",
            script: script_log_triage,
            check: check_log_triage,
            // The reduced grep/read output must still carry the planted fact
            // (test name + both drift values), or the task is not solvable from
            // context in arm A.
            needles: &["ceiling_is_exact", "8192", "8191"],
        },
    ]
}

// -- scripted tool-call sequences -------------------------------------------

fn call_turn(id: &str, name: &str, arguments: Value) -> AssistantTurn {
    AssistantTurn {
        text: None,
        reasoning: Vec::new(),
        tool_calls: vec![ToolCall {
            id: id.to_string(),
            thought_signature: None,
            name: name.to_string(),
            arguments,
        }],
        response_id: None,
        usage: None,
        completion_reason: Some(CompletionReason::ToolUse),
    }
}

fn answer_turn(text: &str) -> AssistantTurn {
    AssistantTurn::text(text)
}

fn script_fix_test() -> Vec<AssistantTurn> {
    vec![
        call_turn("c1", "grep", json!({ "pattern": "parse_len" })),
        call_turn("c2", "read", json!({ "path": "parser.rs" })),
        call_turn(
            "c3",
            "edit",
            json!({
                "file_path": "parser.rs",
                "old_string": "s.split_whitespace().count() - 1",
                "new_string": "s.split_whitespace().count()",
            }),
        ),
        answer_turn(
            "Fixed the off-by-one in parser::parse_len -- removed the trailing `- 1` so it counts every whitespace-separated token.",
        ),
    ]
}

fn script_rename() -> Vec<AssistantTurn> {
    let files = [
        "config/retry.rs",
        "net/client.rs",
        "net/pool.rs",
        "worker/runner.rs",
        "docs/notes.md",
    ];
    let mut turns = vec![call_turn("g", "grep", json!({ "pattern": "MAX_RETRIES" }))];
    for (idx, file) in files.iter().enumerate() {
        turns.push(call_turn(
            &format!("r{idx}"),
            "read",
            json!({ "path": file }),
        ));
        turns.push(call_turn(
            &format!("e{idx}"),
            "edit",
            json!({
                "file_path": file,
                "old_string": "MAX_RETRIES",
                "new_string": "MAX_ATTEMPTS",
                "replace_all": true,
            }),
        ));
    }
    turns.push(answer_turn(
        "Renamed MAX_RETRIES to MAX_ATTEMPTS across config/retry.rs, net/client.rs, net/pool.rs, worker/runner.rs, and docs/notes.md.",
    ));
    turns
}

fn script_log_triage() -> Vec<AssistantTurn> {
    vec![
        call_turn(
            "g",
            "grep",
            json!({ "pattern": "assertion", "path": "logs" }),
        ),
        call_turn("r", "read", json!({ "path": "logs/shard-03.log" })),
        answer_turn(
            "The failing test is caps::tests::ceiling_is_exact (logs/shard-03.log): the token \
             budget ceiling drifted by one -- it reported left: 8192, right: 8191.",
        ),
    ]
}

// -- mechanical success checks ----------------------------------------------

/// Workload 1: the test goes green. Compiles the fixture crate with
/// `rustc --test` and runs it; success = every test passes (exit 0).
fn check_fix_test(workspace: &Path, _final_text: &str) -> Outcome {
    let bin = workspace.join("wl1_test_bin");
    let compile = Command::new("rustc")
        .args(["--test", "--edition", "2021", "-A", "warnings", "-o"])
        .arg(&bin)
        .arg(workspace.join("lib.rs"))
        .output();
    let compile = match compile {
        Ok(output) => output,
        Err(error) => {
            return Outcome {
                success: false,
                detail: format!("rustc not runnable: {error}"),
            };
        }
    };
    if !compile.status.success() {
        return Outcome {
            success: false,
            detail: format!(
                "fixture did not compile: {}",
                String::from_utf8_lossy(&compile.stderr).trim()
            ),
        };
    }
    match Command::new(&bin).output() {
        Ok(run) if run.status.success() => Outcome {
            success: true,
            detail: "cargo/rustc test binary exited 0 (all tests passed)".to_string(),
        },
        Ok(run) => Outcome {
            success: false,
            detail: format!(
                "test binary failed: {}",
                String::from_utf8_lossy(&run.stdout).trim()
            ),
        },
        Err(error) => Outcome {
            success: false,
            detail: format!("test binary not runnable: {error}"),
        },
    }
}

/// Workload 2: the expected diff is applied. No file may still contain the old
/// identifier, and every source that had it now has the new one.
fn check_rename(workspace: &Path, _final_text: &str) -> Outcome {
    let mut stray = Vec::new();
    let mut renamed = 0usize;
    for path in walk_files(workspace) {
        let content = fs::read_to_string(&path).unwrap_or_default();
        let rel = path.strip_prefix(workspace).unwrap_or(&path).display();
        if content.contains("MAX_RETRIES") {
            stray.push(rel.to_string());
        }
        if content.contains("MAX_ATTEMPTS") {
            renamed += 1;
        }
    }
    if stray.is_empty() && renamed >= 5 {
        Outcome {
            success: true,
            detail: format!("all occurrences renamed across {renamed} files, none left"),
        }
    } else {
        Outcome {
            success: false,
            detail: format!("renamed {renamed} files; stray MAX_RETRIES in {stray:?}"),
        }
    }
}

/// Workload 3: the planted fact is found. The answer must carry both the
/// planted left/right values (unique to shard-03), so a generic answer fails.
fn check_log_triage(_workspace: &Path, final_text: &str) -> Outcome {
    let has_left = final_text.contains("8192");
    let has_right = final_text.contains("8191");
    if has_left && has_right {
        Outcome {
            success: true,
            detail: "answer carries the planted left/right values (8192/8191)".to_string(),
        }
    } else {
        Outcome {
            success: false,
            detail: format!("answer missing planted values (8192={has_left}, 8191={has_right})"),
        }
    }
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Driver + metrics
// ---------------------------------------------------------------------------

/// Metrics from one workload x arm run.
struct RunMetrics {
    arm: Arm,
    /// Estimated cumulative input tokens (replay proxy) OR real provider input
    /// tokens (headline). See the field the caller reads.
    cumulative_proxy: usize,
    final_context_proxy: usize,
    provider_turns: u32,
    approvals_consulted: bool,
    /// The final transcript text the agent saw (replay path only; empty for the
    /// real path). Used for the needle-survival assertion.
    transcript: String,
    outcome: Outcome,
}

/// Drive one workload x arm with the scripted replay provider under the auto
/// preset + zero-prompt gate. The fixture is materialized fresh so edits never
/// touch the committed copy.
fn run_replay_arm(workload: &Workload, arm: Arm) -> RunMetrics {
    let workspace = materialize(workload.fixture);
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

/// Drive one workload x arm against the REAL provider under the auto preset +
/// zero-prompt gate. The model chooses its own tool calls (no script); the
/// workspace is a fresh materialized fixture and prompt tokens come from real
/// provider usage records. Costs money; only the opt-in headline test calls it.
/// Benchmark reasoning effort, held IDENTICAL across arms (it is a confounder).
/// `IRIS_BENCH_REASONING` overrides; default `low` -- the agreed cost-conscious
/// setting (reasoning tokens are output-side and add cost/variance without
/// sharpening the input-reduction signal this benchmark measures).
fn bench_reasoning() -> Option<crate::mimir::selection::ReasoningEffort> {
    let raw = std::env::var("IRIS_BENCH_REASONING").unwrap_or_else(|_| "low".to_string());
    if raw.trim().eq_ignore_ascii_case("none") {
        return None;
    }
    Some(
        crate::mimir::selection::ReasoningEffort::parse(raw.trim())
            .expect("valid IRIS_BENCH_REASONING"),
    )
}

/// Build a `ModelSelection` for a `provider:model` spec, overriding provider,
/// model, base URL, and reasoning on top of a config-resolved base (so cache /
/// retry / context-management defaults are inherited). Used by the smoke and
/// headline paths to drive an explicit model matrix.
fn selection_for_spec(
    cwd: &Path,
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
struct RealRunRecord {
    arm: Arm,
    outcome: Outcome,
    turns: u32,
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    cache_read: u64,
    total_tokens: u64,
    tool_counts: std::collections::BTreeMap<String, u32>,
    handles_stored: u32,
    per_turn: Vec<(u64, u64)>,
    approvals_consulted: bool,
}

impl RealRunRecord {
    fn tool_calls_total(&self) -> u32 {
        self.tool_counts.values().sum()
    }
    /// Mean input tokens per provider turn -- the factor the reduction lever
    /// actually moves, isolated from the (noisy, model-chosen) turn count.
    fn tokens_per_turn(&self) -> f64 {
        if self.turns == 0 {
            0.0
        } else {
            self.input_tokens as f64 / self.turns as f64
        }
    }
}

/// JSONL run-log path (override with `IRIS_BENCH_LOG`). One line per real run,
/// with every field captured -- the durable record for offline statistics.
fn bench_log_path() -> String {
    std::env::var("IRIS_BENCH_LOG")
        .unwrap_or_else(|_| "target/tokens-per-task-runs.jsonl".to_string())
}

fn bench_log_reset() {
    let _ = std::fs::write(bench_log_path(), "");
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

/// Run one real-provider cell for an explicit selection, capturing rich
/// per-run/per-turn data and appending it to the JSONL log. Fallible: a backend
/// rejection (bad model id, unsupported thinking level, auth) is returned as
/// `Err(message)` so the smoke can report per-model reachability instead of
/// aborting the whole matrix.
fn run_real_cell(
    model: &str,
    workload: &Workload,
    arm: Arm,
    run: usize,
    selection: &crate::mimir::selection::ModelSelection,
) -> std::result::Result<RealRunRecord, String> {
    let workspace = materialize(workload.fixture);
    let cwd = workspace.path.clone();
    let tools = built_in_tools();
    let system_prompt = crate::wayland::system_prompt::assemble(&cwd, &tools);
    let settings = crate::config::Settings::load(&cwd).map_err(|e| e.to_string())?;
    let session_id = crate::session::new_session_id();
    let provider = crate::build_provider(selection, &system_prompt, &session_id)
        .map_err(|e| format!("build provider: {e}"))?;
    let mut agent = Agent::new(provider, built_in_tools())
        .with_max_tool_roundtrips(settings.max_tool_roundtrips());
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

    let outcome = (workload.check)(&cwd, &observer.final_text());
    let record = RealRunRecord {
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
    };
    bench_log_append(&json!({
        "model": model,
        "workload": workload.name,
        "arm": record.arm.label(),
        "reduce_output": arm.reduce(),
        "run": run,
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

fn model_specs() -> Vec<String> {
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
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(future)
}

// ---------------------------------------------------------------------------
// Replay tests (CI, deterministic)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod replay {
    use super::*;

    /// The margin (in estimated tokens) arm A must beat arm B by, so the win is
    /// not a rounding artifact of the estimator.
    const MIN_MARGIN_TOKENS: usize = 32;

    fn assert_workload(workload: &Workload) -> (RunMetrics, RunMetrics) {
        let arm_a = run_replay_arm(workload, Arm::Defaults);
        let arm_b = run_replay_arm(workload, Arm::Baseline);

        // (c) zero approval prompts in either arm.
        assert!(
            !arm_a.approvals_consulted && !arm_b.approvals_consulted,
            "[{}] approval gate was consulted -- a prompt occurred under auto (run invalid)",
            workload.name
        );
        // (a) success in both arms (both apply the identical fix/answer)...
        assert!(
            arm_a.outcome.success,
            "[{}] arm A failed: {}",
            workload.name, arm_a.outcome.detail
        );
        assert!(
            arm_b.outcome.success,
            "[{}] arm B failed: {}",
            workload.name, arm_b.outcome.detail
        );
        // ...and the reduced (arm A) tool output must still carry every
        // actionable fact the task needs, verbatim -- so success is tied to
        // output fidelity, not just to a scripted answer. Checked in both arms.
        for needle in workload.needles {
            assert!(
                arm_a.transcript.contains(needle),
                "[{}] arm A (reduced) transcript dropped needle {needle:?}",
                workload.name
            );
            assert!(
                arm_b.transcript.contains(needle),
                "[{}] arm B transcript dropped needle {needle:?}",
                workload.name
            );
        }
        // (b) arm A spends fewer prompt tokens than arm B, by a margin.
        assert!(
            arm_a.cumulative_proxy + MIN_MARGIN_TOKENS <= arm_b.cumulative_proxy,
            "[{}] arm A ({}) must beat arm B ({}) by >= {} proxy tokens",
            workload.name,
            arm_a.cumulative_proxy,
            arm_b.cumulative_proxy,
            MIN_MARGIN_TOKENS
        );
        (arm_a, arm_b)
    }

    #[test]
    fn fix_failing_test_arm_a_wins_and_both_succeed() {
        assert_workload(&workloads()[0]);
    }

    #[test]
    fn multi_file_rename_arm_a_wins_and_both_succeed() {
        assert_workload(&workloads()[1]);
    }

    #[test]
    fn investigate_large_log_arm_a_wins_and_both_succeed() {
        assert_workload(&workloads()[2]);
    }

    /// Opt-in real-provider headline (issue #210 DoD #5). Costs money, so it is
    /// `#[ignore]`d AND additionally gated on `IRIS_BENCH_REAL=1`; CI and a plain
    /// `cargo test` never spend money even with `--ignored`. Prints the per-cell
    /// table (workload x arm x run) with REAL usage-record input tokens. N runs
    /// per cell via `IRIS_BENCH_N` (default 3). Run:
    ///   IRIS_BENCH_REAL=1 cargo test --bin iris tokens_per_task_headline \
    ///     -- --ignored --nocapture
    #[test]
    #[ignore = "real-provider run: costs money; set IRIS_BENCH_REAL=1 to run"]
    fn tokens_per_task_headline() {
        if std::env::var("IRIS_BENCH_REAL").ok().as_deref() != Some("1") {
            eprintln!(
                "skipping real-provider headline: set IRIS_BENCH_REAL=1 (this run costs money)"
            );
            return;
        }
        let n: usize = std::env::var("IRIS_BENCH_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        let specs = model_specs();
        let reasoning = bench_reasoning();
        let cwd = std::env::current_dir().expect("cwd");
        bench_log_reset();
        println!(
            "headline: models={} reasoning={:?} N={} workloads={} log={}",
            specs.join(", "),
            reasoning,
            n,
            workloads().len(),
            bench_log_path()
        );
        println!(
            "| model | workload | arm | run | success | turns | in tok | out tok | tok/turn | tool calls | handles | approvals | note |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
        for spec in &specs {
            let selection = match selection_for_spec(&cwd, spec, reasoning) {
                Ok(sel) => sel,
                Err(e) => {
                    println!("| {spec} | - | - | - | - | - | - | - | - | - | - | select: {e} |");
                    continue;
                }
            };
            for workload in workloads() {
                // Baseline first, then defaults -- same order for every cell.
                for arm in [Arm::Baseline, Arm::Defaults] {
                    for run in 0..n {
                        match run_real_cell(spec, &workload, arm, run + 1, &selection) {
                            Ok(m) => println!(
                                "| {} | {} | {} | {} | {} | {} | {} | {} | {:.0} | {} | {} | {} | |",
                                spec,
                                workload.name,
                                m.arm.label(),
                                run + 1,
                                m.outcome.success,
                                m.turns,
                                m.input_tokens,
                                m.output_tokens,
                                m.tokens_per_turn(),
                                m.tool_calls_total(),
                                m.handles_stored,
                                m.approvals_consulted,
                            ),
                            Err(e) => println!(
                                "| {} | {} | {} | {} | - | - | - | - | - | - | - | - | {} |",
                                spec,
                                workload.name,
                                arm.label(),
                                run + 1,
                                e
                            ),
                        }
                    }
                }
            }
        }
    }

    /// Opt-in real-provider SMOKE (cheapest gate before the headline matrix).
    /// One read-only workload (log triage) x both arms x N=1 per model, over
    /// the `model_specs()` matrix, with reasoning forced by `bench_reasoning()`
    /// (default low). Reports per-model REACHABILITY (a backend reject is a
    /// recorded row, not a panic), so we learn which model ids the current
    /// OAuth actually serves -- and whether Haiku accepts a thinking level --
    /// for a handful of calls before committing to N=3. Costs money; `#[ignore]`d
    /// and gated on `IRIS_BENCH_REAL=1`. Run:
    ///   IRIS_BENCH_REAL=1 cargo test --bin iris tokens_per_task_smoke \
    ///     -- --ignored --nocapture
    #[test]
    #[ignore = "real-provider smoke: costs a few calls; set IRIS_BENCH_REAL=1 to run"]
    fn tokens_per_task_smoke() {
        if std::env::var("IRIS_BENCH_REAL").ok().as_deref() != Some("1") {
            eprintln!("skipping real-provider smoke: set IRIS_BENCH_REAL=1 (this run costs money)");
            return;
        }
        let specs = model_specs();
        let reasoning = bench_reasoning();
        let workload = &workloads()[2]; // log triage: 3 turns, read/grep only.
        let cwd = std::env::current_dir().expect("cwd");
        bench_log_reset();
        println!(
            "smoke: workload={} reasoning={:?} models={} log={}",
            workload.name,
            reasoning,
            specs.join(", "),
            bench_log_path()
        );
        println!(
            "| model | arm | reachable | success | turns | in tok | out tok | tool calls | approvals | note |"
        );
        println!("|---|---|---|---|---|---|---|---|---|---|");
        let mut reachable_with_approval = false;
        for spec in &specs {
            let selection = match selection_for_spec(&cwd, spec, reasoning) {
                Ok(sel) => sel,
                Err(e) => {
                    println!("| {spec} | - | no | - | - | - | - | - | select: {e} |");
                    continue;
                }
            };
            for arm in [Arm::Baseline, Arm::Defaults] {
                match run_real_cell(spec, workload, arm, 1, &selection) {
                    Ok(m) => {
                        if m.approvals_consulted {
                            reachable_with_approval = true;
                        }
                        println!(
                            "| {} | {} | yes | {} | {} | {} | {} | {} | {} | |",
                            spec,
                            m.arm.label(),
                            m.outcome.success,
                            m.turns,
                            m.input_tokens,
                            m.output_tokens,
                            m.tool_calls_total(),
                            m.approvals_consulted,
                        );
                    }
                    Err(e) => {
                        println!(
                            "| {} | {} | no | - | - | - | - | - | {} |",
                            spec,
                            arm.label(),
                            e
                        );
                    }
                }
            }
        }
        // A reachable run that consulted the approval gate means the workload
        // would prompt under auto for that model -- the run is invalid and the
        // workload/prompt must be fixed before the headline matrix.
        assert!(
            !reachable_with_approval,
            "a reachable smoke run consulted the approval gate (a prompt occurred under auto); \
             fix the workload prompt before running the headline"
        );
    }

    #[test]
    fn tokens_per_task_replay_report() {
        // Prints the deterministic replay table committed to
        // docs/benchmarks/tokens-per-task.md (run with --nocapture).
        println!(
            "| workload | arm | success | turns | cumulative proxy tokens | final context proxy | reduction |"
        );
        println!("|---|---|---|---|---|---|---|");
        for workload in workloads() {
            let a = run_replay_arm(&workload, Arm::Defaults);
            let b = run_replay_arm(&workload, Arm::Baseline);
            let reduction = if b.cumulative_proxy == 0 {
                0.0
            } else {
                100.0 * (1.0 - a.cumulative_proxy as f64 / b.cumulative_proxy as f64)
            };
            for m in [&b, &a] {
                println!(
                    "| {} | {} | {} | {} | {} | {} | {} |",
                    workload.name,
                    m.arm.label(),
                    m.outcome.success,
                    m.provider_turns,
                    m.cumulative_proxy,
                    m.final_context_proxy,
                    if m.arm == Arm::Defaults {
                        format!("{reduction:.1}%")
                    } else {
                        "-".to_string()
                    },
                );
            }
        }
    }
}
