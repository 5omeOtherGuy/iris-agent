use std::cell::RefCell;

use anyhow::Result;

use crate::nexus::{
    AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ProviderUsage,
    ToolCall,
};

pub(crate) mod login;
pub(crate) mod markdown;
pub(crate) mod modal;
pub(crate) mod palette;
pub(crate) mod picker;
pub(crate) mod selector;
pub(crate) mod slash;
pub(crate) mod steering;
pub(crate) mod symbols;
pub(crate) mod terminal_surface;
pub(crate) mod textengine;

/// True when `lines[i]` begins a unified-diff file header: a `--- ` line
/// immediately followed by a `+++ ` line and a `@@` hunk. The `@@` guard keeps
/// a removed content line that happens to start with `-- ` from being mistaken
/// for a header. Shared by the TUI (`tui::diff_rows`) and text
/// (`text::diff_body`) diff colorizers so both drop EVERY file-header pair in a
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
    /// tool's allow-always capability; when false the front-end offers y/N only.
    fn request_approval(&mut self, call: &ToolCall, allow_always: bool)
    -> Result<ApprovalDecision>;

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
    ToolProposed(ToolCall),
    /// A tool is about to execute; lets the front-end open a live progress cell.
    ToolStarted(ToolCall),
    /// A gated tool was auto-approved by the session allow-policy (the user
    /// chose "always" for this tool earlier). Emitted by Nexus, never inferred
    /// by the UI, so the policy stays Nexus-owned.
    ToolAutoApproved(ToolCall),
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
            } => UiEvent::CompactionApplied {
                compaction_id,
                covered_from,
                covered_to,
                covered_messages,
                original_tokens_estimate,
                summary_tokens_estimate,
                budget,
            },
            AgentEvent::AssistantText(text) => UiEvent::AssistantText(text),
            AgentEvent::AssistantTextDelta(delta) => UiEvent::AssistantTextDelta(delta),
            AgentEvent::AssistantTextEnd(text) => UiEvent::AssistantTextEnd(text),
            AgentEvent::AssistantReasoning { text, redacted } => {
                UiEvent::AssistantReasoning { text, redacted }
            }
            AgentEvent::ToolProposed(call) => UiEvent::ToolProposed(call),
            AgentEvent::ToolStarted(call) => UiEvent::ToolStarted(call),
            AgentEvent::ToolAutoApproved(call) => UiEvent::ToolAutoApproved(call),
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
    fn review<'a>(&'a self, call: &'a ToolCall, allow_always: bool) -> ApprovalFuture<'a> {
        Box::pin(async move { self.ui.borrow_mut().request_approval(call, allow_always) })
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
