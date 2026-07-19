use std::cell::RefCell;

use anyhow::Result;

use crate::nexus::{
    AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, CompactionOrigin,
    InteractionFuture, InteractionOutcome, ProviderUsage, ReviewContext, ToolCall,
    VerificationOutcome,
};

/// Maximum characters of failing verification output carried into a notice, so
/// a large build/test log is truncated (tail-first: the failure is usually at
/// the end) rather than flooding the transcript.
const MAX_VERIFICATION_OUTPUT_CHARS: usize = 2000;

/// Honest one-line (plus output on failure) summary of a verification outcome
/// for the notice channel (issue #265). Never claims pass on failure; preserves
/// the failing output, truncated to [`MAX_VERIFICATION_OUTPUT_CHARS`].
fn verification_notice(outcome: &VerificationOutcome) -> String {
    match outcome {
        VerificationOutcome::Passed { attempts } => {
            if *attempts <= 1 {
                "verification passed".to_string()
            } else {
                format!("verification passed after {attempts} attempts")
            }
        }
        VerificationOutcome::Failed {
            attempts,
            exit_code,
            last_output,
        } => {
            let code = match exit_code {
                Some(code) => format!(" (exit code {code})"),
                None => String::new(),
            };
            let attempts_label = if *attempts == 1 {
                "1 attempt".to_string()
            } else {
                format!("{attempts} attempts")
            };
            let output = truncate_tail(last_output.trim(), MAX_VERIFICATION_OUTPUT_CHARS);
            if output.is_empty() {
                format!("verification failed after {attempts_label}{code}")
            } else {
                format!("verification failed after {attempts_label}{code}:\n{output}")
            }
        }
        VerificationOutcome::SkippedUnconfigured => {
            "verification skipped: no verify.command configured".to_string()
        }
        VerificationOutcome::SkippedApprovalDenied => {
            "verification skipped: approval denied".to_string()
        }
    }
}

/// Keep at most `max` characters from the END of `text` (char-boundary safe),
/// prefixing a marker when truncated. The tail is where a failing command's
/// error usually lands.
fn truncate_tail(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let tail: String = text.chars().skip(count - max).collect();
    format!("...(truncated)\n{tail}")
}

pub(crate) mod ask_user_question;
pub(crate) mod clipboard;
pub(crate) mod delegation_dashboard;
pub(crate) mod harness_actor;
pub(crate) mod highlight;
pub(crate) mod hyperlink;
pub(crate) mod login;
pub(crate) mod markdown;
pub(crate) mod modal;
pub(crate) mod palette;
pub(crate) mod picker;
pub(crate) mod screen_mode;
pub(crate) mod selector;
pub(crate) mod settings_menu;
pub(crate) mod slash;
pub(crate) mod steering;
pub(crate) mod symbols;
pub(crate) mod task_view;
pub(crate) mod terminal_doctor;
pub(crate) mod terminal_env;
pub(crate) mod terminal_surface;
pub(crate) mod textengine;
// ADR-0042: theme trait + palettes + registry, consumed by the `palette`
// accessors which delegate to `theme::active()`.
pub(crate) mod theme;
pub(crate) mod zwj_probe;

/// True when `lines[i]` begins a unified-diff file header: a `--- ` line
/// immediately followed by a `+++ ` line and a `@@` hunk. The `@@` guard keeps
/// a removed content line that happens to start with `-- ` from being mistaken
/// for a header. Shared by the TUI panel and text (`text::diff_body`) diff
/// colorizers so both drop EVERY file-header pair in a
/// multi-file diff, not just the first.
pub(crate) fn is_diff_file_header(lines: &[&str], i: usize) -> bool {
    lines[i].starts_with("--- ")
        && lines.get(i + 1).is_some_and(|l| l.starts_with("+++ "))
        && lines.get(i + 2).is_some_and(|l| l.starts_with("@@"))
}
pub(crate) mod text;
pub(crate) mod tui;
pub(crate) mod tui_loop;

/// Terminal front-end seam (Tier 3). Implementations own all terminal I/O.
///
/// Nexus does not depend on this trait: it emits `AgentEvent`s to an
/// `AgentObserver` and consults an `ApprovalGate`. `UiBridge` adapts a `Ui`
/// onto those two Nexus seams. The CLI session driver still reads prompts and
/// renders session-driver events (`SessionStarted`/`TurnError`) through `Ui`
/// directly.
pub(crate) trait Ui {
    /// Return the next user prompt, or `None` for EOF/end of session.
    fn next_prompt(&mut self) -> Result<Option<String>>;

    /// Render one semantic event.
    fn emit(&mut self, event: UiEvent) -> Result<()>;

    /// Block for the user's decision on a gated tool call. `allow_always` is the
    /// tool's allow-always capability; `allow_project` is whether a persistent
    /// per-project grant is on offer (never for a destructive call). When both
    /// are false the front-end offers y/N only. `ctx` carries the structured
    /// review facts (destructive floor, dirty-tree paths) the front-end renders
    /// into its explanatory reason line.
    fn request_approval(
        &mut self,
        call: &ToolCall,
        allow_always: bool,
        allow_project: bool,
        ctx: &ReviewContext,
    ) -> Result<ApprovalDecision>;

    /// Block for a required human response. Unlike approval, this remains active
    /// under every permission preset and may return populated tool arguments.
    fn request_interaction(&mut self, call: &ToolCall) -> Result<InteractionOutcome>;

    /// Release any terminal state acquired for the session (e.g. bracketed
    /// paste). Called once when the session loop ends. Default: no-op.
    fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UiEvent {
    SessionStarted,
    ContextPressure {
        tier: crate::nexus::ContextPressureTier,
        measured: u64,
        effective_window: u64,
        source: crate::nexus::ContextMeasurementSource,
    },
    ProviderTurnStarted {
        turn_id: String,
    },
    ProviderTurnCompleted {
        turn_id: String,
        response_id: Option<String>,
        usage: Option<ProviderUsage>,
        timing: crate::nexus::ProviderTurnTiming,
    },
    ProviderTurnCancelled {
        turn_id: String,
    },
    ProviderTurnError {
        turn_id: String,
        message: String,
    },
    ToolLifecycle {
        provider_turn_id: String,
        call_id: String,
        name: String,
        state: crate::nexus::ToolEventState,
    },
    OutputHandleStored {
        provider_turn_id: String,
        call_id: String,
        handle_id: String,
        bytes: usize,
        lines: usize,
    },
    CompactionApplied {
        compaction_id: String,
        covered_from: String,
        covered_to: String,
        covered_messages: usize,
        original_tokens_estimate: u64,
        summary_tokens_estimate: u64,
        budget: u64,
        /// Summary source (provider / subagent / excerpts / provider-native),
        /// so the apply-time notice can name the route inline instead of
        /// leaving it discoverable only via `/compaction`.
        origin: CompactionOrigin,
    },
    CompactionLifecycle {
        job_id: String,
        state: crate::nexus::CompactionLifecycleState,
        covered_messages: usize,
        original_tokens_estimate: u64,
        message: Option<String>,
    },
    /// A background delegated worker reached a terminal state. Foreground
    /// workers never emit this because their terminal result already occupies
    /// the spawning tool block.
    WorkerLifecycle {
        worker_id: iris_subagent_runtime::WorkerId,
        status: iris_subagent_runtime::WorkerStatus,
        changed_paths: Option<usize>,
    },
    /// A microcompaction fold batch was flushed (ADR-0048, issue #400). Counts
    /// and estimates only, tagged with the trigger class that released it.
    FoldApplied {
        folds: usize,
        semantic_dedupe_folds: usize,
        tool_clearing_folds: usize,
        reclaimed_tokens_estimate: u64,
        trigger: crate::nexus::FoldTrigger,
    },
    AssistantText(String),
    AssistantTextDelta(String),
    AssistantTextEnd(String),
    /// One block of model reasoning ("thinking") for display. Block-level (not a
    /// stream); a `redacted` block carries no text and the original reasoning is
    /// never reconstructed. See [`AgentEvent::AssistantReasoning`].
    AssistantReasoning {
        text: String,
        redacted: bool,
    },
    /// One incremental chunk of the model's reasoning text, streamed while the
    /// provider is still thinking (before the answer). Display-only; redacted
    /// reasoning is never reconstructed here.
    /// See [`AgentEvent::AssistantReasoningDelta`].
    AssistantReasoningDelta(String),
    /// A boundary between two reasoning-summary parts (a blank line in the live
    /// thinking trace). Display-only; carries no text.
    AssistantReasoningSectionBreak,
    /// One incremental chunk of raw model reasoning. Display-only and separate
    /// from summary deltas so provenance is preserved through the UI bridge.
    /// See [`AgentEvent::AssistantRawReasoningDelta`].
    AssistantRawReasoningDelta(String),
    /// One incremental fragment of a *freeform/custom* tool call's input, streamed
    /// while the model is still constructing the call (ADR-0039). Display-only and
    /// inert: it never affects approval, execution, or transcript state. No
    /// freeform tool is declared in Iris today, so this does not fire in practice;
    /// the live preview UI is deferred until `apply_patch` (V4A) exists to render.
    /// See [`AgentEvent::ToolInputDelta`].
    ToolInputDelta {
        call_id: String,
        delta: String,
    },
    ToolProposed(ToolCall),
    /// A tool is about to execute; lets the front-end open a live progress cell.
    ToolStarted(ToolCall),
    /// A gated tool was auto-approved by the session allow-policy (the user
    /// chose "always" for this tool earlier). Emitted by Nexus, never inferred
    /// by the UI, so the policy stays Nexus-owned.
    ToolAutoApproved(ToolCall),
    /// A gated tool is awaiting the user's decision. Drives the in-block
    /// `▲ REVIEW` state (the affordance lives on the tool block's footer, not a
    /// separate panel); emitted by the loop's approval bridge before the
    /// blocking gate so the review block exists while the composer is frozen.
    ToolReview {
        call: ToolCall,
        allow_always: bool,
        allow_project: bool,
        /// Whether the allow-always key is the dirty-tree escalation (`all dirty
        /// files (this task)`) rather than a reusable session grant.
        dirty_gate: bool,
        /// A short danger-toned caution (`destructive`, dirty-path note,
        /// `unsandboxed`) surfaced on the review footer so the safety facts
        /// survive the decision point. `None` when the call is unremarkable.
        reason: Option<String>,
    },
    DiffPreview {
        call: ToolCall,
        diff: String,
    },
    ToolDenied(ToolCall),
    ToolResult {
        call: ToolCall,
        content: String,
        exit_code: Option<i32>,
        duration: Option<std::time::Duration>,
    },
    /// A display-only chunk of a running tool's live output.
    ToolOutputDelta {
        call_id: String,
        chunk: String,
    },
    ToolError {
        call: ToolCall,
        message: String,
    },
    ToolCancelled(ToolCall),
    /// A user message the loop injected mid-run (steering or follow-up). The
    /// front-end renders it as a user row at this point so transcript order
    /// matches provider context. See [`AgentEvent::UserMessage`].
    UserMessage(String),
    Notice(String),
    /// Silent operational metadata consumed by Wayland for session diagnostics.
    ProviderTransportRecovery,
    /// The task's final net diff (issue #264, `/diff`): a per-file summary plus
    /// the combined unified diff. Rendered through the existing diff colorizer
    /// in the TUI and as plain text on the non-TTY path. A UI-only event -- it
    /// has no `AgentEvent` source (the model never emits it).
    TaskDiff {
        summary: Vec<String>,
        diff: String,
    },
    /// Read-only `/compaction [n]` detail. TUI renders a foldable panel; text
    /// mode renders the same metadata and summary as plain lines.
    CompactionInspection {
        title: String,
        detail: Vec<String>,
        summary: String,
    },
    TurnError {
        kind: TurnErrorKind,
        message: String,
    },
    TurnComplete,
}

pub(crate) fn worker_lifecycle_message(
    worker_id: &iris_subagent_runtime::WorkerId,
    status: iris_subagent_runtime::WorkerStatus,
    changed_paths: Option<usize>,
) -> String {
    let status = match status {
        iris_subagent_runtime::WorkerStatus::Completed => "completed",
        iris_subagent_runtime::WorkerStatus::Failed => "failed",
        iris_subagent_runtime::WorkerStatus::Cancelled => "cancelled",
        iris_subagent_runtime::WorkerStatus::Interrupted => "interrupted",
        iris_subagent_runtime::WorkerStatus::Adoptable => "adoptable",
        _ => "finished",
    };
    let changed = changed_paths.map_or_else(String::new, |count| {
        let noun = if count == 1 { "file" } else { "files" };
        format!(" — {count} {noun} changed")
    });
    format!(
        "subagent {} {status}{changed}",
        delegation_dashboard::short_id(worker_id.as_str())
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnErrorKind {
    Provider,
    Auth,
}

fn provider_transport_fallback_notice(
    fallback: &crate::nexus::ProviderTransportFallback,
) -> String {
    let from = match fallback.from_transport.as_str() {
        "websocket" => "WebSocket",
        other => other,
    };
    let to = match fallback.to_transport.as_str() {
        "https_sse" => "SSE",
        other => other,
    };
    let wait = if fallback.idle_ms.is_multiple_of(1_000) {
        format!("{}s", fallback.idle_ms / 1_000)
    } else {
        format!("{}ms", fallback.idle_ms)
    };
    let last_event = fallback.last_event.as_deref().unwrap_or("none");
    if fallback.reason != "read_idle" {
        return format!(
            "{} `{}` exhausted {from} recovery after {} reconnects during {} (reason: {}); switched from {from} to {to} for this session. Invoke `$provider-stream-diagnostics` now to inspect the saved metadata.",
            fallback.provider,
            fallback.model,
            fallback.reconnect_count,
            fallback.phase,
            fallback.reason,
        );
    }
    format!(
        "{} `{}` received no {from} frames for {wait} during {}; switched from {from} to {to} for this session after {} reconnects (last provider event: {last_event}). Invoke `$provider-stream-diagnostics` now to inspect the saved metadata.",
        fallback.provider, fallback.model, fallback.phase, fallback.reconnect_count
    )
}

fn provider_reconnect_notice(reconnect: &crate::nexus::ProviderReconnect) -> String {
    let transport = match reconnect.transport.as_str() {
        "websocket" => "WebSocket",
        other => other,
    };
    let wait = if reconnect.delay_ms.is_multiple_of(1_000) {
        format!("{}s", reconnect.delay_ms / 1_000)
    } else {
        format!("{}ms", reconnect.delay_ms)
    };
    format!(
        "{transport} reconnect {}/{} in {wait} after {} during {}.",
        reconnect.retry, reconnect.max_retries, reconnect.reason, reconnect.phase
    )
}

impl UiEvent {
    /// Map one Nexus `AgentEvent` onto its presentation event. Single-sourced so
    /// both the blocking text bridge and the async loop bridge agree.
    pub(crate) fn from_agent_event(event: AgentEvent) -> Self {
        match event {
            AgentEvent::ContextPressure {
                tier,
                measured,
                effective_window,
                source,
            } => UiEvent::ContextPressure {
                tier,
                measured,
                effective_window,
                source,
            },
            AgentEvent::ProviderTurnStarted { turn_id } => UiEvent::ProviderTurnStarted { turn_id },
            AgentEvent::ProviderTurnCompleted {
                turn_id,
                response_id,
                usage,
                // Provider-neutral completion reason is metadata-only today; no
                // UI surface renders it yet, so it is intentionally dropped here.
                completion_reason: _,
                timing,
            } => UiEvent::ProviderTurnCompleted {
                turn_id,
                response_id,
                usage,
                timing,
            },
            AgentEvent::ProviderTurnCancelled { turn_id } => {
                UiEvent::ProviderTurnCancelled { turn_id }
            }
            AgentEvent::ProviderTurnError { turn_id, message } => {
                UiEvent::ProviderTurnError { turn_id, message }
            }
            AgentEvent::ProviderTransportFallback(fallback) => {
                UiEvent::Notice(provider_transport_fallback_notice(&fallback))
            }
            AgentEvent::ProviderReconnect(reconnect) => {
                UiEvent::Notice(provider_reconnect_notice(&reconnect))
            }
            AgentEvent::ProviderTransportRecovery(_) => UiEvent::ProviderTransportRecovery,
            AgentEvent::ToolLifecycle {
                provider_turn_id,
                call_id,
                name,
                state,
            } => UiEvent::ToolLifecycle {
                provider_turn_id,
                call_id,
                name,
                state,
            },
            AgentEvent::OutputHandleStored {
                provider_turn_id,
                call_id,
                handle_id,
                bytes,
                lines,
            } => UiEvent::OutputHandleStored {
                provider_turn_id,
                call_id,
                handle_id,
                bytes,
                lines,
            },
            AgentEvent::CompactionApplied {
                compaction_id,
                covered_from,
                covered_to,
                covered_messages,
                original_tokens_estimate,
                summary_tokens_estimate,
                context_tokens_after_apply: _,
                budget,
                // Generation ordinal (ADR-0047) is instrumentation for the
                // event/benchmark, not a display field; the UI does not surface
                // it, so drop it in the display mapping.
                generation: _,
                // Carry count (ADR-0044) is event/benchmark instrumentation, not
                // a display field; drop it in the display mapping too.
                carried_paths: _,
                origin,
                // Realized summarizer usage is instrumentation surfaced today
                // only via the pull-based `/compaction` inspector; the apply
                // notice shows `origin` (the must-have route) without also
                // duplicating token accounting the notice already reports via
                // `original_tokens_estimate`/`summary_tokens_estimate`.
                worker_usage: _,
            } => UiEvent::CompactionApplied {
                compaction_id,
                covered_from,
                covered_to,
                covered_messages,
                original_tokens_estimate,
                summary_tokens_estimate,
                budget,
                origin,
            },
            AgentEvent::CompactionLifecycle {
                job_id,
                state,
                covered_messages,
                original_tokens_estimate,
                origin: _,
                worker_usage: _,
                trigger_tier: _,
                message,
            } => UiEvent::CompactionLifecycle {
                job_id,
                state,
                covered_messages,
                original_tokens_estimate,
                message,
            },
            AgentEvent::FoldApplied {
                folds,
                semantic_dedupe_folds,
                tool_clearing_folds,
                reclaimed_tokens_estimate,
                trigger,
            } => UiEvent::FoldApplied {
                folds,
                semantic_dedupe_folds,
                tool_clearing_folds,
                reclaimed_tokens_estimate,
                trigger,
            },
            AgentEvent::AssistantText(text) => UiEvent::AssistantText(text),
            AgentEvent::AssistantTextDelta(delta) => UiEvent::AssistantTextDelta(delta),
            AgentEvent::AssistantTextEnd(text) => UiEvent::AssistantTextEnd(text),
            AgentEvent::AssistantReasoning { text, redacted } => {
                UiEvent::AssistantReasoning { text, redacted }
            }
            AgentEvent::AssistantReasoningDelta(delta) => UiEvent::AssistantReasoningDelta(delta),
            AgentEvent::AssistantReasoningSectionBreak => UiEvent::AssistantReasoningSectionBreak,
            AgentEvent::AssistantRawReasoningDelta(delta) => {
                UiEvent::AssistantRawReasoningDelta(delta)
            }
            AgentEvent::ToolInputDelta { call_id, delta } => {
                UiEvent::ToolInputDelta { call_id, delta }
            }
            AgentEvent::ToolProposed(call) => UiEvent::ToolProposed(call),
            AgentEvent::ToolStarted(call) => UiEvent::ToolStarted(call),
            AgentEvent::ToolAutoApproved(call) => UiEvent::ToolAutoApproved(call),
            // The dangerous skip-permissions auto-approval (ADR-0049) renders
            // through the existing auto-approved cell; the loud session-start
            // banner and the transcript audit record carry the mode itself.
            AgentEvent::ToolAutoApprovedDangerous(call) => UiEvent::ToolAutoApproved(call),
            AgentEvent::DiffPreview { call, diff } => UiEvent::DiffPreview { call, diff },
            AgentEvent::ToolDenied(call) => UiEvent::ToolDenied(call),
            AgentEvent::ToolResult {
                call,
                content,
                exit_code,
                duration,
            } => UiEvent::ToolResult {
                call,
                content,
                exit_code,
                duration,
            },
            AgentEvent::ToolOutputDelta { call_id, chunk } => {
                UiEvent::ToolOutputDelta { call_id, chunk }
            }
            AgentEvent::ToolError { call, message } => UiEvent::ToolError { call, message },
            AgentEvent::ToolCancelled(call) => UiEvent::ToolCancelled(call),
            AgentEvent::UserMessage(text) => UiEvent::UserMessage(text),
            AgentEvent::Notice(message) => UiEvent::Notice(message),
            // Dirty-tree safety (issue #262) surfaces through the existing
            // notice channel: the baseline summary and any violation are
            // rendered as advisory notices, no new UI surface.
            AgentEvent::DirtyBaseline(summary) => UiEvent::Notice(summary),
            AgentEvent::MutationViolation {
                call,
                paths,
                restored,
            } => {
                let recovery = if restored {
                    "restored from snapshot"
                } else {
                    "snapshot restore failed"
                };
                UiEvent::Notice(format!(
                    "`{}` modified protected uncommitted file(s): {} ({recovery})",
                    call.name,
                    paths.join(", ")
                ))
            }
            // Post-change verification (issue #265) surfaces through the notice
            // channel: an honest one-line pass/fail/skipped summary, plus the
            // failing command output on failure (never suppressed, never a false
            // pass). No new UI surface.
            AgentEvent::Verification(outcome) => UiEvent::Notice(verification_notice(&outcome)),
            AgentEvent::TurnComplete => UiEvent::TurnComplete,
        }
    }

    pub(crate) fn from_turn_error(error: &anyhow::Error) -> Self {
        let kind = if error.downcast_ref::<crate::errors::AuthError>().is_some() {
            TurnErrorKind::Auth
        } else {
            TurnErrorKind::Provider
        };
        Self::TurnError {
            kind,
            message: format!("{error:#}"),
        }
    }
}

/// Tier-3 adapter that backs both Nexus front-end seams with a single `Ui`.
///
/// Nexus takes `AgentObserver` and `ApprovalGate` as two independent `&self`
/// seams; the terminal `Ui` needs `&mut self`. `RefCell` carries that
/// mutability so one `Ui` can serve both seams from two shared borrows without
/// aliasing -- the Rust analogue of pi's shared captured closure state.
pub(crate) struct UiBridge<'a> {
    ui: RefCell<&'a mut dyn Ui>,
}

impl<'a> UiBridge<'a> {
    pub(crate) fn new(ui: &'a mut dyn Ui) -> Self {
        Self {
            ui: RefCell::new(ui),
        }
    }
}

impl AgentObserver for UiBridge<'_> {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        self.ui.borrow_mut().emit(UiEvent::from_agent_event(event))
    }
}

impl ApprovalGate for UiBridge<'_> {
    fn review<'a>(
        &'a self,
        call: &'a ToolCall,
        allow_always: bool,
        allow_project: bool,
        ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        Box::pin(async move {
            self.ui
                .borrow_mut()
                .request_approval(call, allow_always, allow_project, &ctx)
        })
    }

    fn interact<'a>(&'a self, call: &'a ToolCall) -> InteractionFuture<'a> {
        Box::pin(async move { self.ui.borrow_mut().request_interaction(call) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::AgentEvent;
    use std::time::Duration;

    fn call() -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
            name: "bash".to_string(),
            arguments: serde_json::json!({ "command": "echo hi" }),
        }
    }

    #[test]
    fn maps_tool_started_to_ui_event() {
        let mapped = UiEvent::from_agent_event(AgentEvent::ToolStarted(call()));
        assert_eq!(mapped, UiEvent::ToolStarted(call()));
    }

    #[test]
    fn maps_provider_completion_metadata_to_ui_event() {
        let usage = ProviderUsage {
            provider: "openai-codex".to_string(),
            model: "gpt-5.5".to_string(),
            input_tokens: 100,
            output_tokens: 20,
            cache_read_input_tokens: 64,
            cache_write_input_tokens: 0,
            reasoning_output_tokens: 5,
            total_tokens: 120,
            cache_creation: None,
        };
        let mapped = UiEvent::from_agent_event(AgentEvent::ProviderTurnCompleted {
            turn_id: "turn_1".to_string(),
            response_id: Some("resp_1".to_string()),
            usage: Some(usage.clone()),
            completion_reason: None,
            timing: crate::nexus::ProviderTurnTiming::sample(),
        });
        assert_eq!(
            mapped,
            UiEvent::ProviderTurnCompleted {
                turn_id: "turn_1".to_string(),
                response_id: Some("resp_1".to_string()),
                usage: Some(usage),
                timing: crate::nexus::ProviderTurnTiming::sample(),
            }
        );
    }

    #[test]
    fn maps_reasoning_to_ui_event() {
        let mapped = UiEvent::from_agent_event(AgentEvent::AssistantReasoning {
            text: "thinking".to_string(),
            redacted: false,
        });
        assert_eq!(
            mapped,
            UiEvent::AssistantReasoning {
                text: "thinking".to_string(),
                redacted: false,
            }
        );
    }

    #[test]
    fn maps_redacted_reasoning_to_ui_event() {
        let mapped = UiEvent::from_agent_event(AgentEvent::AssistantReasoning {
            text: String::new(),
            redacted: true,
        });
        assert_eq!(
            mapped,
            UiEvent::AssistantReasoning {
                text: String::new(),
                redacted: true,
            }
        );
    }

    #[test]
    fn maps_tool_output_delta_to_ui_event() {
        let mapped = UiEvent::from_agent_event(AgentEvent::ToolOutputDelta {
            call_id: "call_1".to_string(),
            chunk: "partial output".to_string(),
        });
        assert_eq!(
            mapped,
            UiEvent::ToolOutputDelta {
                call_id: "call_1".to_string(),
                chunk: "partial output".to_string(),
            }
        );
    }

    #[test]
    fn maps_provider_reconnect_to_safe_progress_notice() {
        let mapped = UiEvent::from_agent_event(AgentEvent::ProviderReconnect(
            crate::nexus::ProviderReconnect {
                transport: "websocket".to_string(),
                retry: 2,
                max_retries: 3,
                delay_ms: 4_000,
                reason: "read_idle".to_string(),
                phase: "awaiting_next_frame".to_string(),
            },
        ));

        let UiEvent::Notice(message) = mapped else {
            panic!("reconnect status must render through the notice channel");
        };
        assert!(message.contains("WebSocket reconnect 2/3"), "{message}");
        assert!(message.contains("4s"), "{message}");
        assert!(message.contains("read_idle"), "{message}");
        assert!(message.contains("awaiting_next_frame"), "{message}");
    }

    #[test]
    fn maps_provider_transport_recovery_to_silent_ui_event() {
        let mapped = UiEvent::from_agent_event(AgentEvent::ProviderTransportRecovery(
            crate::nexus::ProviderTransportRecovery {
                provider: "openai-codex".to_string(),
                model: "gpt-test".to_string(),
                transport: "websocket".to_string(),
                reason: "stale_reused_socket".to_string(),
                phase: "websocket_read".to_string(),
                close_code: Some(1000),
                close_reason: Some("normal".to_string()),
                socket_reused: true,
                socket_age_ms: 42,
                last_event: None,
            },
        ));

        assert_eq!(mapped, UiEvent::ProviderTransportRecovery);
    }

    #[test]
    fn maps_provider_transport_fallback_to_an_actionable_notice() {
        let mapped = UiEvent::from_agent_event(AgentEvent::ProviderTransportFallback(
            crate::nexus::ProviderTransportFallback {
                provider: "openai-codex".to_string(),
                model: "gpt-test".to_string(),
                from_transport: "websocket".to_string(),
                to_transport: "https_sse".to_string(),
                reason: "read_idle".to_string(),
                phase: "awaiting_next_frame".to_string(),
                idle_ms: 300_000,
                ws_attempt: 4,
                reconnect_count: 3,
                last_event: Some("response.created".to_string()),
            },
        ));

        let UiEvent::Notice(message) = mapped else {
            panic!("fallback must render through the notice channel");
        };
        assert!(
            message.contains("switched from WebSocket to SSE"),
            "{message}"
        );
        assert!(message.contains("300s"), "{message}");
        assert!(message.contains("after 3 reconnects"), "{message}");
        assert!(message.contains("response.created"), "{message}");
        assert!(
            message.contains("$provider-stream-diagnostics"),
            "{message}"
        );
    }

    #[test]
    fn maps_non_idle_transport_fallback_without_claiming_frame_silence() {
        let mapped = UiEvent::from_agent_event(AgentEvent::ProviderTransportFallback(
            crate::nexus::ProviderTransportFallback {
                provider: "openai-codex".to_string(),
                model: "gpt-test".to_string(),
                from_transport: "websocket".to_string(),
                to_transport: "https_sse".to_string(),
                reason: "setup_error".to_string(),
                phase: "connection_setup".to_string(),
                idle_ms: 0,
                ws_attempt: 4,
                reconnect_count: 3,
                last_event: None,
            },
        ));

        let UiEvent::Notice(message) = mapped else {
            panic!("fallback must render through the notice channel");
        };
        assert!(
            message.contains("exhausted WebSocket recovery"),
            "{message}"
        );
        assert!(message.contains("setup_error"), "{message}");
        assert!(
            !message.contains("received no WebSocket frames"),
            "{message}"
        );
    }

    /// Audit F11c/F20: `AgentEvent::CompactionApplied` carries `origin`, but the
    /// conversion used to drop it (`origin: _`). Each accepted origin must
    /// survive the conversion so the apply notice/transcript line can name the
    /// route (provider / subagent / excerpts / provider-native) instead of
    /// leaving it discoverable only via the pull-based `/compaction` inspector.
    #[test]
    fn maps_compaction_applied_preserving_origin() {
        for origin in [
            CompactionOrigin::Provider,
            CompactionOrigin::Subagent,
            CompactionOrigin::Excerpts,
            CompactionOrigin::ProviderNative,
        ] {
            let mapped = UiEvent::from_agent_event(AgentEvent::CompactionApplied {
                compaction_id: "c1".to_string(),
                covered_from: "m1".to_string(),
                covered_to: "m9".to_string(),
                covered_messages: 9,
                original_tokens_estimate: 3_400,
                summary_tokens_estimate: 442,
                context_tokens_after_apply: 107_231,
                budget: 80_000,
                generation: 1,
                carried_paths: 0,
                origin,
                worker_usage: None,
            });
            assert_eq!(
                mapped,
                UiEvent::CompactionApplied {
                    compaction_id: "c1".to_string(),
                    covered_from: "m1".to_string(),
                    covered_to: "m9".to_string(),
                    covered_messages: 9,
                    original_tokens_estimate: 3_400,
                    summary_tokens_estimate: 442,
                    budget: 80_000,
                    origin,
                },
                "origin {origin:?} must survive the AgentEvent -> UiEvent conversion"
            );
        }
    }

    #[test]
    fn maps_tool_result_with_exit_code_and_duration() {
        let mapped = UiEvent::from_agent_event(AgentEvent::ToolResult {
            call: call(),
            content: "done".to_string(),
            exit_code: Some(3),
            duration: Some(Duration::from_millis(1200)),
        });
        assert_eq!(
            mapped,
            UiEvent::ToolResult {
                call: call(),
                content: "done".to_string(),
                exit_code: Some(3),
                duration: Some(Duration::from_millis(1200)),
            }
        );
    }
}
