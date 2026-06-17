use std::cell::RefCell;

use anyhow::Result;

use crate::nexus::{
    AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ToolCall,
};

pub(crate) mod slash;
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
    AssistantText(String),
    AssistantTextDelta(String),
    AssistantTextEnd(String),
    ToolProposed(ToolCall),
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
    },
    ToolError {
        call: ToolCall,
        message: String,
    },
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
            AgentEvent::AssistantText(text) => UiEvent::AssistantText(text),
            AgentEvent::AssistantTextDelta(delta) => UiEvent::AssistantTextDelta(delta),
            AgentEvent::AssistantTextEnd(text) => UiEvent::AssistantTextEnd(text),
            AgentEvent::ToolProposed(call) => UiEvent::ToolProposed(call),
            AgentEvent::ToolAutoApproved(call) => UiEvent::ToolAutoApproved(call),
            AgentEvent::DiffPreview { call, diff } => UiEvent::DiffPreview { call, diff },
            AgentEvent::ToolDenied(call) => UiEvent::ToolDenied(call),
            AgentEvent::ToolResult { call, content } => UiEvent::ToolResult { call, content },
            AgentEvent::ToolError { call, message } => UiEvent::ToolError { call, message },
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
