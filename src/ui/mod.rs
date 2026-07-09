use std::cell::RefCell;

use anyhow::Result;

use crate::nexus::{
    AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ProviderUsage,
    ReviewContext, ToolCall, VerificationOutcome,
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

pub(crate) mod clipboard;
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

    /// Release any terminal state acquired for the session (e.g. bracketed
    /// paste). Called once when the session loop ends. Default: no-op.
    fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UiEvent {
    SessionStarted,
    ProviderTurnStarted {
        turn_id: String,
    },
    ProviderTurnCompleted {
        turn_id: String,
        response_id: Option<String>,
        usage: Option<ProviderUsage>,
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
    },
    CompactionLifecycle {
        job_id: String,
        state: crate::nexus::CompactionLifecycleState,
        covered_messages: usize,
        original_tokens_estimate: u64,
        message: Option<String>,
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
    /// The task's final net diff (issue #264, `/diff`): a per-file summary plus
    /// the combined unified diff. Rendered through the existing diff colorizer
    /// in the TUI and as plain text on the non-TTY path. A UI-only event -- it
    /// has no `AgentEvent` source (the model never emits it).
    TaskDiff {
        summary: Vec<String>,
        diff: String,
    },
    TurnError {
        kind: TurnErrorKind,
        message: String,
    },
    TurnComplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnErrorKind {
    Provider,
    Auth,
}

impl UiEvent {
    /// Map one Nexus `AgentEvent` onto its presentation event. Single-sourced so
    /// both the blocking text bridge and the async loop bridge agree.
    pub(crate) fn from_agent_event(event: AgentEvent) -> Self {
        match event {
            AgentEvent::ProviderTurnStarted { turn_id } => UiEvent::ProviderTurnStarted { turn_id },
            AgentEvent::ProviderTurnCompleted {
                turn_id,
                response_id,
                usage,
                // Provider-neutral completion reason is metadata-only today; no
                // UI surface renders it yet, so it is intentionally dropped here.
                completion_reason: _,
            } => UiEvent::ProviderTurnCompleted {
                turn_id,
                response_id,
                usage,
            },
            AgentEvent::ProviderTurnCancelled { turn_id } => {
                UiEvent::ProviderTurnCancelled { turn_id }
            }
            AgentEvent::ProviderTurnError { turn_id, message } => {
                UiEvent::ProviderTurnError { turn_id, message }
            }
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
                budget,
                // Generation ordinal (ADR-0047) is instrumentation for the
                // event/benchmark, not a display field; the UI does not surface
                // it, so drop it in the display mapping.
                generation: _,
                // Carry count (ADR-0044) is event/benchmark instrumentation, not
                // a display field; drop it in the display mapping too.
                carried_paths: _,
            } => UiEvent::CompactionApplied {
                compaction_id,
                covered_from,
                covered_to,
                covered_messages,
                original_tokens_estimate,
                summary_tokens_estimate,
                budget,
            },
            AgentEvent::CompactionLifecycle {
                job_id,
                state,
                covered_messages,
                original_tokens_estimate,
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
        // The interactive production front-end is the raw-mode TUI
        // (`ui::tui::TuiUi`): it reads Ctrl-C at an approval as a key event,
        // calls `signals::interrupt_from_terminal()` (which trips the per-turn
        // watcher's `CancellationToken`) and returns Deny, so the FIRST Ctrl-C
        // abandons a pending approval. This inline call only blocks the executor
        // when the front-end is the non-interactive `TextUi` fallback (pipes/CI,
        // or a TTY where the TUI failed to start): there a blocking stdin read
        // holds the thread, so the loop's cancellation race only lands once
        // input arrives, with a second Ctrl-C as the force-quit backstop. Either
        // way, Nexus's post-review `token.is_cancelled()` check keeps a late
        // decision from running the tool or mutating the session allow-policy.
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
        });
        assert_eq!(
            mapped,
            UiEvent::ProviderTurnCompleted {
                turn_id: "turn_1".to_string(),
                response_id: Some("resp_1".to_string()),
                usage: Some(usage),
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
