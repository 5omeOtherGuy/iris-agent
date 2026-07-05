//! Rich per-run instrumentation + the zero-prompt approval gate.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use anyhow::Result;

use crate::nexus::{
    AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate, ReviewContext,
    ToolCall, ToolEventState,
};

/// Rich per-run instrumentation. Beyond the headline input-token total, it
/// captures the per-turn token trajectory, output/reasoning/cache tokens, a
/// per-tool call histogram (re-running grep or re-reading a file shows up here
/// -- the signal for "the reduced output was harder to interpret"), and handle
/// offloads. All of it is logged as JSONL so no observation is thrown away.
#[derive(Default)]
pub(crate) struct BenchObserver {
    pub(crate) final_text: RefCell<String>,
    pub(crate) usage_input_tokens: Cell<u64>,
    pub(crate) output_tokens: Cell<u64>,
    pub(crate) reasoning_tokens: Cell<u64>,
    pub(crate) cache_read: Cell<u64>,
    pub(crate) total_tokens: Cell<u64>,
    pub(crate) provider_turns: Cell<u32>,
    /// Successful tool executions keyed by tool name (grep/read/edit/find/ls).
    pub(crate) tool_counts: RefCell<BTreeMap<String, u32>>,
    /// Count of large outputs offloaded behind a handle (more likely in arm B).
    pub(crate) handles_stored: Cell<u32>,
    /// Per provider turn: (input_tokens, output_tokens), in order.
    pub(crate) per_turn: RefCell<Vec<(u64, u64)>>,
}

impl BenchObserver {
    pub(crate) fn final_text(&self) -> String {
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
pub(crate) struct ZeroPromptGate {
    pub(crate) consulted: Cell<bool>,
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
