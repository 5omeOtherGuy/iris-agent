use anyhow::Result;

use crate::approval::ApprovalDecision;
use crate::nexus::ToolCall;

pub(crate) mod text;

/// Front-end seam between Nexus and the terminal UI.
///
/// Implementations own all terminal I/O. Nexus drives turns and approval policy,
/// but reads prompts and requests approval only through this trait.
pub(crate) trait Ui {
    /// Return the next user prompt, or `None` for EOF/end of session.
    fn next_prompt(&mut self) -> Result<Option<String>>;

    /// Render one semantic event.
    fn emit(&mut self, event: UiEvent) -> Result<()>;

    /// Block for the user's decision on a gated tool call.
    fn request_approval(&mut self, call: &ToolCall) -> Result<ApprovalDecision>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UiEvent {
    SessionStarted,
    AssistantText(String),
    AssistantTextDelta(String),
    AssistantTextEnd(String),
    ToolProposed(ToolCall),
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

pub(crate) fn is_exit_command(prompt: &str) -> bool {
    matches!(prompt.trim(), "/exit" | "/quit")
}
