//! Headless `--print` mode (Tier 3). Runs one agent turn-sequence with no
//! interactive terminal: the final assistant answer goes to stdout, approval is
//! resolved non-interactively (deny by default, allow with `--approve`), and
//! piped stdin is merged into the prompt. The model loop, event stream, and
//! approval contract are Nexus-owned; this module only supplies the CLI-side
//! adapters (an observer that captures the final answer and a gate that never
//! prompts) plus the pure argument/stdin merging helpers.
//!
//! The observer also folds provider usage (`ProviderTurnCompleted`) and tool
//! lifecycle (`ToolLifecycle`) into a [`crate::metrics::TokenFlows`] so a
//! headless run can emit an end-of-run [`UsageReport`] for benchmarking and
//! diagnostics. Emission is opt-in (the `IRIS_USAGE_JSON` env var names a file
//! path); stdout still carries only the final answer.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use serde::Serialize;

use crate::metrics::TokenFlows;
use crate::nexus::{
    AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, CompletionReason,
    ReviewContext, ToolCall, ToolEventState, Tools,
};

/// A parsed `-p`/`--print` invocation: the prompt argument plus whether gated
/// tools are auto-approved (`--approve`).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PrintInvocation {
    pub(crate) prompt: String,
    pub(crate) approve: bool,
}

/// Parse a print-mode invocation from the raw arguments (already stripped of
/// `argv[0]`). Returns `None` when this is not a `-p`/`--print` run so the
/// caller falls through to the other command dispatch. A print run needs
/// exactly one `-p`/`--print` flag and exactly one prompt argument; both it and
/// `--approve` may appear in any position. Anything else (extra positionals,
/// missing prompt, repeated flags) makes it invalid (returns `None`, which
/// surfaces as a usage error when a print flag was present).
pub(crate) fn parse_print_args(args: &[String]) -> Option<PrintInvocation> {
    let mut print = false;
    let mut approve = false;
    let mut prompt: Option<String> = None;
    for arg in args {
        if arg == "-p" || arg == "--print" {
            if print {
                return None;
            }
            print = true;
        } else if arg == "--approve" {
            if approve {
                return None;
            }
            approve = true;
        } else if prompt.is_none() {
            prompt = Some(arg.clone());
        } else {
            return None;
        }
    }
    if !print {
        return None;
    }
    prompt.map(|prompt| PrintInvocation { prompt, approve })
}

/// Merge piped stdin into the prompt. When there is nothing piped (a TTY, or an
/// empty/whitespace-only pipe) the prompt is used verbatim; otherwise the piped
/// content follows the prompt after a blank-line delimiter so the model sees the
/// instruction first, then the material. Pure so the merge is unit-tested
/// without touching the real stdin.
pub(crate) fn merge_prompt(prompt: &str, piped: Option<&str>) -> String {
    match piped {
        Some(content) if !content.trim().is_empty() => {
            format!("{prompt}\n\n{}", content.trim_end())
        }
        _ => prompt.to_string(),
    }
}

/// Read piped stdin for print mode, or `None` when stdin is a terminal (nothing
/// to merge) or the pipe is empty. Blocking read to EOF; the caller merges the
/// result into the prompt.
pub(crate) fn read_piped_stdin() -> Result<Option<String>> {
    if io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut buffer = String::new();
    io::stdin().lock().read_to_string(&mut buffer)?;
    if buffer.is_empty() {
        Ok(None)
    } else {
        Ok(Some(buffer))
    }
}

/// Observer that captures the final assistant answer for stdout and drops all
/// intermediate output. In headless mode nothing but the final answer may reach
/// stdout, so tool activity, reasoning, and provider metadata are suppressed;
/// the last non-empty assistant text of the turn-sequence is the answer.
///
/// Alongside the answer it accumulates measured provider usage and a tool-use
/// histogram for the opt-in [`UsageReport`] (see [`Self::usage_report`]). This
/// accumulation is metadata only -- it never influences what reaches stdout.
#[derive(Default)]
pub(crate) struct PrintObserver {
    final_text: RefCell<String>,
    /// Completed provider rounds in event order. The raw usage retained beside
    /// each serialized turn lets `usage_report` derive every v1 total.
    turns: RefCell<Vec<TurnSample>>,
    /// Tool invocations in start order. Terminal lifecycle events fill timing;
    /// output-handle events enrich the same record before it is serialized.
    tools: RefCell<Vec<UsageTool>>,
    active_tools: RefCell<HashMap<(String, String), ActiveTool>>,
    base: UsageBase,
    /// Latest provider/model seen on a completed turn (the run's active cell).
    provider: RefCell<String>,
    model: RefCell<String>,
    /// When set, the report is (re)written here after every completed provider
    /// turn, so a run that is killed mid-task (e.g. a benchmark agent-timeout)
    /// still leaves the latest token accounting on disk. `None` disables the
    /// sink entirely (stdout stays the only output).
    usage_path: Option<PathBuf>,
}

struct TurnSample {
    turn_id: String,
    turn: UsageTurn,
    usage: Option<crate::nexus::ProviderUsage>,
}

struct ActiveTool {
    started: Instant,
    index: usize,
}

impl PrintObserver {
    /// Build an observer with the model-visible system-prompt/tool base measured
    /// before the tool registry moves into the agent.
    pub(crate) fn with_base(usage_path: Option<PathBuf>, base: UsageBase) -> Self {
        Self {
            base,
            usage_path,
            ..Self::default()
        }
    }

    /// The captured final assistant text (empty when the turn produced no text).
    pub(crate) fn final_text(self) -> String {
        self.final_text.into_inner()
    }

    /// Write the current [`UsageReport`] to the configured sink, if any. Best
    /// effort: a sink failure is logged, never propagated, so it cannot change
    /// the run outcome. A no-op when no `usage_path` was configured.
    pub(crate) fn flush_usage(&self) {
        let Some(path) = &self.usage_path else {
            return;
        };
        let report = self.usage_report();
        if let Err(error) = write_usage_report(&report, path) {
            tracing::warn!(
                error = %format!("{error:#}"),
                path = %path.display(),
                "headless usage report not written"
            );
        }
    }

    /// Snapshot the accumulated usage as a serializable report. Borrows only,
    /// so it can be called before [`Self::final_text`] consumes the observer.
    pub(crate) fn usage_report(&self) -> UsageReport {
        let samples = self.turns.borrow();
        let turns = samples.iter().map(|sample| sample.turn.clone()).collect();
        let mut flows = TokenFlows::default();
        for sample in samples.iter() {
            if let Some(usage) = &sample.usage {
                flows.observe(usage);
            }
        }
        let tools = self.tools.borrow().clone();
        let mut tool_calls_by_name = BTreeMap::new();
        for tool in &tools {
            *tool_calls_by_name.entry(tool.name.clone()).or_insert(0) += 1;
        }
        let totals = UsageTotals {
            provider_turns: flows.provider_turns,
            input_tokens: flows.input_tokens,
            output_tokens: flows.output_tokens,
            reasoning_output_tokens: flows.reasoning_output_tokens,
            cache_read_input_tokens: flows.cache_read_input_tokens,
            cache_write_input_tokens: flows.cache_write_input_tokens,
            cache_creation_5m_input_tokens: flows.cache_creation_5m_input_tokens,
            cache_creation_1h_input_tokens: flows.cache_creation_1h_input_tokens,
            cache_creation_reported: flows.cache_creation_reported,
            latest_total_tokens: flows.latest_total_tokens,
            tool_calls: tools.len() as u64,
            tool_calls_by_name,
        };
        UsageReport {
            provider: self.provider.borrow().clone(),
            model: self.model.borrow().clone(),
            base: self.base.clone(),
            turns,
            tools,
            totals: totals.clone(),
            legacy_totals: totals,
        }
    }
}

impl AgentObserver for PrintObserver {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        match event {
            AgentEvent::AssistantText(text) | AgentEvent::AssistantTextEnd(text)
                if !text.is_empty() =>
            {
                *self.final_text.borrow_mut() = text;
            }
            AgentEvent::ProviderTurnCompleted {
                turn_id,
                usage,
                completion_reason,
                timing,
                ..
            } => {
                if let Some(usage) = &usage {
                    self.provider.replace(usage.provider.clone());
                    self.model.replace(usage.model.clone());
                }
                let mut turns = self.turns.borrow_mut();
                let i = turns.len() as u32;
                let (fresh, cache_read, cache_write, output, reasoning, input_full) = usage
                    .as_ref()
                    .map(|usage| {
                        (
                            usage
                                .input_tokens
                                .saturating_sub(usage.cache_read_input_tokens)
                                .saturating_sub(usage.cache_write_input_tokens),
                            usage.cache_read_input_tokens,
                            usage.cache_write_input_tokens,
                            usage.output_tokens,
                            usage.reasoning_output_tokens,
                            usage.input_tokens,
                        )
                    })
                    .unwrap_or_default();
                turns.push(TurnSample {
                    turn_id,
                    turn: UsageTurn {
                        i,
                        fresh,
                        cache_read,
                        cache_write,
                        output,
                        reasoning,
                        input_full,
                        duration_ms: duration_ms(timing.duration),
                        ttft_ms: timing.time_to_first_output.map(duration_ms),
                        stop_reason: completion_reason.map(completion_reason_name),
                    },
                    usage,
                });
                drop(turns);
                // Persist incrementally so a mid-task kill (benchmark timeout)
                // still leaves this turn's cumulative accounting.
                self.flush_usage();
            }
            AgentEvent::ToolLifecycle {
                provider_turn_id,
                call_id,
                name,
                state: ToolEventState::Started,
            } => {
                let turn = self
                    .turns
                    .borrow()
                    .iter()
                    .position(|sample| sample.turn_id == provider_turn_id)
                    .unwrap_or(0) as u32;
                let mut tools = self.tools.borrow_mut();
                let index = tools.len();
                tools.push(UsageTool {
                    i: index as u32,
                    turn,
                    name,
                    call_id: call_id.clone(),
                    duration_ms: 0,
                    offloaded: false,
                    handle: None,
                    handle_bytes: None,
                    handle_lines: None,
                });
                self.active_tools.borrow_mut().insert(
                    (provider_turn_id, call_id),
                    ActiveTool {
                        started: Instant::now(),
                        index,
                    },
                );
            }
            AgentEvent::OutputHandleStored {
                provider_turn_id,
                call_id,
                handle_id,
                bytes,
                lines,
            } => {
                if let Some(index) = self
                    .active_tools
                    .borrow()
                    .get(&(provider_turn_id, call_id))
                    .map(|active| active.index)
                    && let Some(tool) = self.tools.borrow_mut().get_mut(index)
                {
                    tool.offloaded = true;
                    tool.handle = Some(handle_id);
                    tool.handle_bytes = Some(bytes as u64);
                    tool.handle_lines = Some(lines as u64);
                }
            }
            AgentEvent::ToolLifecycle {
                provider_turn_id,
                call_id,
                state:
                    ToolEventState::Succeeded | ToolEventState::Errored | ToolEventState::Cancelled,
                ..
            } => {
                if let Some(active) = self
                    .active_tools
                    .borrow_mut()
                    .remove(&(provider_turn_id, call_id))
                    && let Some(tool) = self.tools.borrow_mut().get_mut(active.index)
                {
                    tool.duration_ms = duration_ms(active.started.elapsed());
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn duration_ms(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn completion_reason_name(reason: CompletionReason) -> &'static str {
    match reason {
        CompletionReason::EndTurn => "end_turn",
        CompletionReason::ToolUse => "tool_use",
        CompletionReason::MaxOutputTokens => "max_output_tokens",
        CompletionReason::ContextWindowExceeded => "context_window_exceeded",
        CompletionReason::StopSequence => "stop_sequence",
        CompletionReason::Paused => "paused",
        CompletionReason::Refusal => "refusal",
        CompletionReason::Other => "other",
    }
}

/// Estimated fixed provider context sent before conversation messages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct UsageBase {
    pub(crate) system_prompt_tokens: u64,
    pub(crate) tools: Vec<UsageBaseTool>,
    pub(crate) tools_total_tokens: u64,
    pub(crate) base_total_tokens: u64,
}

impl UsageBase {
    pub(crate) fn estimate(system_prompt: &str, tools: &Tools) -> Self {
        let system_prompt_tokens = estimated_tokens(system_prompt);
        let tools = tools
            .iter()
            .map(|tool| {
                let declaration = serde_json::json!({
                    "name": tool.name(),
                    "description": tool.description(),
                    "input_schema": tool.parameters(),
                });
                let declaration = serde_json::to_string(&declaration)
                    .expect("serializing a JSON value cannot fail");
                UsageBaseTool {
                    name: tool.name().to_string(),
                    schema_tokens: estimated_tokens(&declaration),
                }
            })
            .collect::<Vec<_>>();
        let tools_total_tokens = tools.iter().map(|tool| tool.schema_tokens).sum::<u64>();
        Self {
            system_prompt_tokens,
            tools,
            tools_total_tokens,
            base_total_tokens: system_prompt_tokens.saturating_add(tools_total_tokens),
        }
    }
}

fn estimated_tokens(text: &str) -> u64 {
    u64::try_from(crate::tools::est_tokens(text)).unwrap_or(u64::MAX)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UsageBaseTool {
    pub(crate) name: String,
    pub(crate) schema_tokens: u64,
}

/// Provider-measured token and timing flows for one completed round trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UsageTurn {
    pub(crate) i: u32,
    pub(crate) fresh: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_write: u64,
    pub(crate) output: u64,
    pub(crate) reasoning: u64,
    pub(crate) input_full: u64,
    pub(crate) duration_ms: u64,
    pub(crate) ttft_ms: Option<u64>,
    pub(crate) stop_reason: Option<&'static str>,
}

/// One tool invocation, enriched by terminal lifecycle and output-handle events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UsageTool {
    pub(crate) i: u32,
    pub(crate) turn: u32,
    pub(crate) name: String,
    pub(crate) call_id: String,
    pub(crate) duration_ms: u64,
    pub(crate) offloaded: bool,
    pub(crate) handle: Option<String>,
    pub(crate) handle_bytes: Option<u64>,
    pub(crate) handle_lines: Option<u64>,
}

/// The cumulative v1 accounting, now rebuilt from the timeline on every flush.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UsageTotals {
    pub(crate) provider_turns: u32,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) reasoning_output_tokens: u64,
    pub(crate) cache_read_input_tokens: u64,
    pub(crate) cache_write_input_tokens: u64,
    pub(crate) cache_creation_5m_input_tokens: u64,
    pub(crate) cache_creation_1h_input_tokens: u64,
    /// `false` when the provider never reported a cache-creation breakdown, so
    /// a reader distinguishes "zero writes" from "writes not attributed".
    pub(crate) cache_creation_reported: bool,
    pub(crate) latest_total_tokens: Option<u64>,
    pub(crate) tool_calls: u64,
    pub(crate) tool_calls_by_name: BTreeMap<String, u64>,
}

/// Headless token/cache/tool timeline emitted when `IRIS_USAGE_JSON` names a
/// path. `totals` preserves v1 accounting; the flattened copy keeps every v1
/// top-level JSON key readable by existing consumers while v2 adds structure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UsageReport {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) base: UsageBase,
    pub(crate) turns: Vec<UsageTurn>,
    pub(crate) tools: Vec<UsageTool>,
    pub(crate) totals: UsageTotals,
    #[serde(flatten)]
    legacy_totals: UsageTotals,
}

/// Write a [`UsageReport`] as pretty JSON to `path`. Best effort: the caller
/// logs a failure rather than failing the run, so a diagnostics-sink problem
/// (unwritable path, full disk) never changes the agent's exit outcome.
pub(crate) fn write_usage_report(report: &UsageReport, path: &Path) -> Result<()> {
    let json = serde_json::to_string_pretty(report)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Non-interactive approval gate. Print mode must never prompt (that would hang
/// a pipe/CI run), so every gated call is resolved without input: deny by
/// default, or allow once when `--approve` was passed. Nexus still enforces the
/// decision and re-checks cancellation after review.
pub(crate) struct PrintApprovalGate {
    approve: bool,
}

impl PrintApprovalGate {
    pub(crate) fn new(approve: bool) -> Self {
        Self { approve }
    }

    /// The fixed decision this gate returns for any gated call.
    fn decision(&self) -> ApprovalDecision {
        if self.approve {
            ApprovalDecision::Allow
        } else {
            ApprovalDecision::Deny
        }
    }
}

impl ApprovalGate for PrintApprovalGate {
    fn review<'a>(
        &'a self,
        _call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
        _ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        let decision = self.decision();
        Box::pin(async move { Ok(decision) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parse_print_args_reads_prompt_for_both_flags() {
        for flag in ["-p", "--print"] {
            let parsed = parse_print_args(&args(&[flag, "hello"])).expect("parsed");
            assert_eq!(
                parsed,
                PrintInvocation {
                    prompt: "hello".to_string(),
                    approve: false,
                }
            );
        }
    }

    #[test]
    fn parse_print_args_reads_approve_before_or_after_prompt() {
        let after = parse_print_args(&args(&["-p", "hello", "--approve"])).expect("parsed");
        let before = parse_print_args(&args(&["-p", "--approve", "hello"])).expect("parsed");
        let expected = PrintInvocation {
            prompt: "hello".to_string(),
            approve: true,
        };
        assert_eq!(after, expected);
        assert_eq!(before, expected);
    }

    #[test]
    fn parse_print_args_accepts_flags_in_any_position() {
        let expected = PrintInvocation {
            prompt: "hello".to_string(),
            approve: true,
        };
        for order in [
            ["--approve", "-p", "hello"],
            ["--approve", "hello", "--print"],
            ["hello", "--print", "--approve"],
        ] {
            assert_eq!(parse_print_args(&args(&order)).expect("parsed"), expected);
        }
        // Repeated flags are invalid.
        assert!(parse_print_args(&args(&["-p", "--print", "hello"])).is_none());
        assert!(parse_print_args(&args(&["-p", "--approve", "--approve", "x"])).is_none());
    }

    #[test]
    fn parse_print_args_ignores_non_print_and_invalid_invocations() {
        // Not a print invocation.
        assert!(parse_print_args(&args(&[])).is_none());
        assert!(parse_print_args(&args(&["--plain"])).is_none());
        assert!(parse_print_args(&args(&["resume", "abc"])).is_none());
        // Print flag but no prompt, or too many positional args -> invalid.
        assert!(parse_print_args(&args(&["-p"])).is_none());
        assert!(parse_print_args(&args(&["-p", "--approve"])).is_none());
        assert!(parse_print_args(&args(&["-p", "one", "two"])).is_none());
    }

    #[test]
    fn merge_prompt_uses_prompt_verbatim_without_piped_content() {
        assert_eq!(merge_prompt("explain", None), "explain");
        assert_eq!(merge_prompt("explain", Some("")), "explain");
        assert_eq!(merge_prompt("explain", Some("   \n  ")), "explain");
    }

    #[test]
    fn merge_prompt_appends_piped_content_after_a_blank_line() {
        assert_eq!(
            merge_prompt("explain this failure", Some("line1\nline2\n")),
            "explain this failure\n\nline1\nline2"
        );
    }

    #[test]
    fn print_observer_captures_last_non_empty_assistant_text() {
        let observer = PrintObserver::default();
        observer
            .on_event(AgentEvent::AssistantText("preamble".to_string()))
            .unwrap();
        observer
            .on_event(AgentEvent::AssistantTextEnd("final answer".to_string()))
            .unwrap();
        // A trailing empty end (no text produced) must not clobber the answer.
        observer
            .on_event(AgentEvent::AssistantTextEnd(String::new()))
            .unwrap();
        assert_eq!(observer.final_text(), "final answer");
    }

    #[test]
    fn print_observer_suppresses_non_assistant_events() {
        let observer = PrintObserver::default();
        observer
            .on_event(AgentEvent::AssistantReasoning {
                text: "thinking".to_string(),
                redacted: false,
            })
            .unwrap();
        observer.on_event(AgentEvent::TurnComplete).unwrap();
        assert_eq!(observer.final_text(), "");
    }

    fn provider_usage(
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> crate::nexus::ProviderUsage {
        crate::nexus::ProviderUsage {
            provider: "anthropic".to_string(),
            model: "claude-sonnet-5".to_string(),
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: cache_read,
            cache_write_input_tokens: cache_write,
            reasoning_output_tokens: 0,
            total_tokens: input + output,
            cache_creation: Some(crate::nexus::CacheCreation {
                ephemeral_5m_input_tokens: cache_write,
                ephemeral_1h_input_tokens: 0,
            }),
        }
    }

    fn turn_completed(usage: Option<crate::nexus::ProviderUsage>) -> AgentEvent {
        AgentEvent::ProviderTurnCompleted {
            turn_id: "t1".to_string(),
            response_id: None,
            usage,
            completion_reason: None,
            timing: crate::nexus::ProviderTurnTiming::sample(),
        }
    }

    fn tool_started(name: &str) -> AgentEvent {
        AgentEvent::ToolLifecycle {
            provider_turn_id: "t1".to_string(),
            call_id: format!("call_{name}"),
            name: name.to_string(),
            state: ToolEventState::Started,
        }
    }

    #[test]
    fn usage_report_builds_turn_tool_and_base_timelines() {
        let tools = crate::tools::built_in_tools();
        let base = UsageBase::estimate("12345", &tools);
        assert_eq!(base.system_prompt_tokens, 2);
        let bash = tools.by_name("bash").expect("bash tool");
        let bash_declaration = serde_json::to_string(&json!({
            "name": bash.name(),
            "description": bash.description(),
            "input_schema": bash.parameters(),
        }))
        .unwrap();
        assert_eq!(
            base.tools
                .iter()
                .find(|tool| tool.name == "bash")
                .unwrap()
                .schema_tokens,
            estimated_tokens(&bash_declaration)
        );
        assert_eq!(
            base.tools_total_tokens,
            base.tools
                .iter()
                .map(|tool| tool.schema_tokens)
                .sum::<u64>()
        );
        assert_eq!(
            base.base_total_tokens,
            base.system_prompt_tokens + base.tools_total_tokens
        );

        let observer = PrintObserver::with_base(None, base.clone());
        observer
            .on_event(AgentEvent::ProviderTurnCompleted {
                turn_id: "t1".to_string(),
                response_id: None,
                usage: Some(provider_usage(1000, 200, 600, 300)),
                completion_reason: Some(crate::nexus::CompletionReason::ToolUse),
                timing: crate::nexus::ProviderTurnTiming {
                    duration: std::time::Duration::from_millis(1250),
                    time_to_first_output: Some(std::time::Duration::from_millis(250)),
                },
            })
            .unwrap();
        observer.on_event(tool_started("bash")).unwrap();
        observer
            .on_event(AgentEvent::OutputHandleStored {
                provider_turn_id: "t1".to_string(),
                call_id: "call_bash".to_string(),
                handle_id: "out_abc".to_string(),
                bytes: 4096,
                lines: 80,
            })
            .unwrap();
        observer
            .on_event(AgentEvent::ToolLifecycle {
                provider_turn_id: "t1".to_string(),
                call_id: "call_bash".to_string(),
                name: "bash".to_string(),
                state: ToolEventState::Succeeded,
            })
            .unwrap();

        let report = observer.usage_report();
        assert_eq!(report.base, base);
        assert_eq!(report.turns.len(), 1);
        assert_eq!(
            report.turns[0],
            UsageTurn {
                i: 0,
                fresh: 100,
                cache_read: 600,
                cache_write: 300,
                output: 200,
                reasoning: 0,
                input_full: 1000,
                duration_ms: 1250,
                ttft_ms: Some(250),
                stop_reason: Some("tool_use"),
            }
        );
        assert_eq!(report.tools.len(), 1);
        let tool = &report.tools[0];
        assert_eq!(tool.i, 0);
        assert_eq!(tool.turn, 0);
        assert_eq!(tool.name, "bash");
        assert_eq!(tool.call_id, "call_bash");
        assert!(tool.duration_ms < 1000);
        assert!(tool.offloaded);
        assert_eq!(tool.handle.as_deref(), Some("out_abc"));
        assert_eq!(tool.handle_bytes, Some(4096));
        assert_eq!(tool.handle_lines, Some(80));
    }

    #[test]
    fn usage_totals_are_derived_from_turns_and_preserve_v1_fields() {
        let observer = PrintObserver::default();
        observer
            .on_event(turn_completed(Some(provider_usage(1000, 200, 600, 300))))
            .unwrap();
        observer
            .on_event(turn_completed(Some(provider_usage(1500, 400, 1200, 100))))
            .unwrap();

        let report = observer.usage_report();
        assert_eq!(
            report.totals.input_tokens,
            report.turns.iter().map(|turn| turn.input_full).sum::<u64>()
        );
        assert_eq!(
            report.totals.output_tokens,
            report.turns.iter().map(|turn| turn.output).sum::<u64>()
        );
        assert_eq!(report.legacy_totals, report.totals);

        let json = serde_json::to_value(report).unwrap();
        assert_eq!(json["totals"]["input_tokens"], 2500);
        assert_eq!(json["input_tokens"], 2500);
    }

    #[test]
    fn usage_report_sums_flows_across_provider_turns() {
        let observer = PrintObserver::default();
        observer
            .on_event(turn_completed(Some(provider_usage(1000, 200, 800, 150))))
            .unwrap();
        observer
            .on_event(turn_completed(Some(provider_usage(1500, 300, 1400, 0))))
            .unwrap();
        let report = observer.usage_report();
        assert_eq!(report.provider, "anthropic");
        assert_eq!(report.model, "claude-sonnet-5");
        assert_eq!(report.totals.provider_turns, 2);
        assert_eq!(report.totals.input_tokens, 2500);
        assert_eq!(report.totals.output_tokens, 500);
        // cache read/write are subsets of input, summed across turns.
        assert_eq!(report.totals.cache_read_input_tokens, 2200);
        assert_eq!(report.totals.cache_write_input_tokens, 150);
        assert_eq!(report.totals.cache_creation_5m_input_tokens, 150);
        assert!(report.totals.cache_creation_reported);
        // total_tokens is a level, replaced by the latest turn (1500 + 300).
        assert_eq!(report.totals.latest_total_tokens, Some(1800));
    }

    #[test]
    fn usage_report_counts_tool_uses_by_name_from_started_events() {
        let observer = PrintObserver::default();
        observer.on_event(tool_started("bash")).unwrap();
        observer.on_event(tool_started("bash")).unwrap();
        observer.on_event(tool_started("read")).unwrap();
        // Non-`Started` lifecycle states do not count a fresh invocation.
        observer
            .on_event(AgentEvent::ToolLifecycle {
                provider_turn_id: "t1".to_string(),
                call_id: "call_bash".to_string(),
                name: "bash".to_string(),
                state: ToolEventState::Succeeded,
            })
            .unwrap();
        let report = observer.usage_report();
        assert_eq!(report.totals.tool_calls, 3);
        assert_eq!(report.totals.tool_calls_by_name.get("bash"), Some(&2));
        assert_eq!(report.totals.tool_calls_by_name.get("read"), Some(&1));
    }

    #[test]
    fn usage_report_preserves_turns_without_usage_and_tool_associations() {
        let observer = PrintObserver::default();
        observer
            .on_event(AgentEvent::ProviderTurnCompleted {
                turn_id: "t0".to_string(),
                response_id: None,
                usage: None,
                completion_reason: Some(crate::nexus::CompletionReason::EndTurn),
                timing: crate::nexus::ProviderTurnTiming {
                    duration: std::time::Duration::from_millis(900),
                    time_to_first_output: None,
                },
            })
            .unwrap();
        observer
            .on_event(turn_completed(Some(provider_usage(100, 20, 0, 0))))
            .unwrap();
        observer.on_event(tool_started("bash")).unwrap();

        let report = observer.usage_report();
        assert_eq!(
            report.turns[0],
            UsageTurn {
                i: 0,
                fresh: 0,
                cache_read: 0,
                cache_write: 0,
                output: 0,
                reasoning: 0,
                input_full: 0,
                duration_ms: 900,
                ttft_ms: None,
                stop_reason: Some("end_turn"),
            }
        );
        assert_eq!(report.turns[1].i, 1);
        assert_eq!(report.tools[0].turn, 1);
        assert_eq!(report.totals.provider_turns, 1);
    }

    #[test]
    fn configured_sink_writes_incrementally_after_each_provider_turn() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "iris-usage-incremental-{}-{seq}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let observer = PrintObserver::with_base(Some(path.clone()), UsageBase::default());
        // First completed turn writes the file even though the run has not
        // ended (simulating the state a mid-task kill would leave behind).
        observer
            .on_event(turn_completed(Some(provider_usage(1000, 200, 0, 1000))))
            .unwrap();
        let after_one: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("written after turn 1"))
                .unwrap();
        assert_eq!(after_one["input_tokens"], 1000);
        assert_eq!(after_one["totals"]["input_tokens"], 1000);
        assert_eq!(after_one["provider_turns"], 1);
        assert_eq!(after_one["turns"].as_array().unwrap().len(), 1);

        // A second turn rewrites with the accumulated totals.
        observer
            .on_event(turn_completed(Some(provider_usage(500, 50, 0, 0))))
            .unwrap();
        let after_two: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after_two["input_tokens"], 1500);
        assert_eq!(after_two["provider_turns"], 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn no_sink_configured_writes_nothing() {
        // Default observer has no usage_path; flush and turns must not panic or
        // write anywhere.
        let observer = PrintObserver::default();
        observer
            .on_event(turn_completed(Some(provider_usage(1000, 200, 0, 0))))
            .unwrap();
        observer.flush_usage();
        // usage_report still reflects accumulation for in-process callers.
        assert_eq!(observer.usage_report().totals.input_tokens, 1000);
    }

    #[test]
    fn write_usage_report_roundtrips_as_json() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);

        let observer = PrintObserver::default();
        observer
            .on_event(turn_completed(Some(provider_usage(1000, 200, 800, 150))))
            .unwrap();
        observer.on_event(tool_started("bash")).unwrap();
        let report = observer.usage_report();

        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "iris-usage-report-{}-{seq}.json",
            std::process::id()
        ));
        write_usage_report(&report, &path).expect("write report");
        let text = std::fs::read_to_string(&path).expect("read report");
        let _ = std::fs::remove_file(&path);

        let parsed: serde_json::Value = serde_json::from_str(&text).expect("valid json");
        assert_eq!(parsed["input_tokens"], 1000);
        assert_eq!(parsed["cache_read_input_tokens"], 800);
        assert_eq!(parsed["cache_write_input_tokens"], 150);
        assert_eq!(parsed["tool_calls"], 1);
        assert_eq!(parsed["tool_calls_by_name"]["bash"], 1);
    }

    fn tool_call() -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
            name: "bash".to_string(),
            arguments: json!({ "command": "rm -rf /" }),
        }
    }

    #[test]
    fn approval_gate_denies_by_default() {
        let gate = PrintApprovalGate::new(false);
        let decision = futures::executor::block_on(gate.review(
            &tool_call(),
            false,
            false,
            ReviewContext::default(),
        ))
        .unwrap();
        assert_eq!(decision, ApprovalDecision::Deny);
    }

    #[test]
    fn approval_gate_allows_with_approve_flag() {
        let gate = PrintApprovalGate::new(true);
        let decision = futures::executor::block_on(gate.review(
            &tool_call(),
            true,
            true,
            ReviewContext::default(),
        ))
        .unwrap();
        assert_eq!(decision, ApprovalDecision::Allow);
    }
}
