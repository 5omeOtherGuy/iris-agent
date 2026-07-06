//! Scripted replay provider: replays a fixed sequence of assistant turns
//! regardless of input, recording the messages it is sent each turn so the
//! harness can estimate transcript size (the arm token proxy). The tool calls
//! in the script are chosen so the real tools run over the fixtures; the
//! outputs are real and differ between arms because the reductions differ.

use std::cell::RefCell;
use std::collections::VecDeque;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::nexus::{AssistantTurn, ChatProvider, Message, ProviderEvent, ProviderStream, Tools};
use crate::tools::bench_support;

pub(crate) struct ScriptedProvider {
    turns: RefCell<VecDeque<AssistantTurn>>,
    seen: RefCell<Vec<Vec<Message>>>,
}

impl ScriptedProvider {
    pub(crate) fn new(turns: Vec<AssistantTurn>) -> Self {
        Self {
            turns: RefCell::new(turns.into_iter().collect()),
            seen: RefCell::new(Vec::new()),
        }
    }

    /// Cumulative estimated input tokens: sum over every provider call of the
    /// estimated tokens of the transcript it was sent. Mirrors how a real
    /// provider bills input (the growing transcript, re-sent each turn). Same
    /// estimator both arms; only the ratio is meaningful.
    pub(crate) fn cumulative_input_proxy(&self) -> usize {
        self.seen
            .borrow()
            .iter()
            .map(|messages| transcript_proxy_tokens(messages))
            .sum()
    }

    /// Estimated tokens of the final (largest) transcript the provider saw --
    /// the accumulated context after every tool result landed.
    pub(crate) fn final_context_proxy(&self) -> usize {
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
    pub(crate) fn final_transcript_text(&self) -> String {
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
