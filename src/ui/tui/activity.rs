//! UI-owned work-phase model for the always-visible working header.
//!
//! The header must never go blank while a task runs, so the phase is a coarse,
//! provider-neutral summary of "what is happening now", derived purely from
//! display events and lifecycle calls -- never from provider or model identity.
//! Keeping the phase and its labels here (not in `screen.rs`) is what lets the
//! status rail stay provider-agnostic: `screen.rs` only asks the phase for a
//! label, it never spells one out.

use crate::nexus::ToolCall;
use crate::ui::UiEvent;

/// Coarse phase of the running task, surfaced as the working-header label.
///
/// The Screen holds one of these while a turn is active; when idle the header
/// is not shown, so there is no dedicated `Idle` variant. Transitions are
/// driven by [`WorkPhase::on_event`] (the display-event stream) plus the
/// Screen's approval lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum WorkPhase {
    /// A task was submitted; the first request is not acknowledged yet. The
    /// default so the header is meaningful within one frame of turn start.
    #[default]
    Starting,
    /// The request is in flight; awaiting the first streamed event.
    WaitingProvider,
    /// A reasoning summary is streaming (the model is thinking out loud).
    Thinking,
    /// Assistant answer text is streaming.
    Answering,
    /// A tool call was proposed and is about to be gated or run.
    PreparingTool,
    /// A tool's approval prompt is the primary surface. The working animation is
    /// suppressed while this is set, so it never competes with the decision.
    AwaitingApproval,
    /// A tool is executing; the label names the tool and its target.
    RunningTool { label: String },
    /// The provider turn finished: the task is wrapping up, or about to loop
    /// back to the model after a tool result.
    Finishing,
}

impl WorkPhase {
    /// The provider-neutral status label shown in the working header. Every
    /// label describes the activity, never the provider or model.
    pub(crate) fn label(&self) -> &str {
        match self {
            WorkPhase::Starting => "Starting",
            WorkPhase::WaitingProvider => "Waiting for model",
            WorkPhase::Thinking => "Thinking",
            WorkPhase::Answering => "Responding",
            WorkPhase::PreparingTool => "Preparing tool",
            WorkPhase::AwaitingApproval => "Awaiting approval",
            WorkPhase::RunningTool { label } => label,
            WorkPhase::Finishing => "Finishing",
        }
    }

    /// Build the running-tool phase from a call, naming the tool and its target
    /// (`Running bash \u00b7 $ ls`, `Running edit \u00b7 src/foo.rs`). The target is the
    /// same neutral, deterministic summary the approval prompt uses; it is
    /// derived from the call, never from the model.
    pub(crate) fn running_tool(call: &ToolCall) -> WorkPhase {
        let name = &call.name;
        let target = crate::tool_display::run_target(call);
        let label = if target.is_empty() || target == *name {
            format!("Running {name}")
        } else {
            format!("Running {name} \u{b7} {target}")
        };
        WorkPhase::RunningTool { label }
    }

    /// The phase implied by a display event, or `None` to keep the current
    /// phase. Approval (`AwaitingApproval`) and its clearing are owned by the
    /// Screen lifecycle (`show_approval`/`clear_approval`), not the event
    /// stream, so they are not produced here.
    pub(crate) fn on_event(event: &UiEvent) -> Option<WorkPhase> {
        match event {
            UiEvent::ProviderTurnStarted { .. } => Some(WorkPhase::WaitingProvider),
            UiEvent::AssistantReasoningDelta(_)
            | UiEvent::AssistantReasoningSectionBreak
            | UiEvent::AssistantRawReasoningDelta(_) => Some(WorkPhase::Thinking),
            UiEvent::AssistantText(_) | UiEvent::AssistantTextDelta(_) => {
                Some(WorkPhase::Answering)
            }
            UiEvent::ToolProposed(_) => Some(WorkPhase::PreparingTool),
            UiEvent::ToolStarted(call) => Some(WorkPhase::running_tool(call)),
            // A running tool's output keeps the RunningTool phase (and its label).
            UiEvent::ToolOutputDelta { .. } => None,
            // Every terminal step of a provider turn or the whole task winds
            // down to the neutral Finishing label, so a cancel or error never
            // leaves a stale "Thinking"/"Responding"/"Waiting for model" on the
            // header while the turn unwinds (the old `provider_waiting` bool
            // cleared on these too).
            UiEvent::ToolResult { .. }
            | UiEvent::ToolError { .. }
            | UiEvent::ToolCancelled(_)
            | UiEvent::ToolDenied(_)
            | UiEvent::ProviderTurnCompleted { .. }
            | UiEvent::ProviderTurnCancelled { .. }
            | UiEvent::ProviderTurnError { .. }
            | UiEvent::TurnError { .. } => Some(WorkPhase::Finishing),
            _ => None,
        }
    }
}
