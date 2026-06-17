//! Tier-2 Wayland harness.
//!
//! Owns the execution surface (workspace + [`ToolState`]) and session
//! persistence, wrapping the bare in-memory [`Agent`]. Mirrors pi's
//! `AgentHarness` (`packages/agent/src/harness/agent-harness.ts`), which owns
//! the `ExecutionEnv` and the session store, feeds the env into each run, and
//! appends transcript messages itself -- the bare agent stays persistence- and
//! filesystem-free.

use std::cell::RefCell;
use std::path::PathBuf;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::handles::HandleStore;
use crate::nexus::{Agent, AgentObserver, ApprovalGate, ChatProvider, ToolEnv};
use crate::session::SessionLog;
use crate::tools::ToolState;

/// Wraps a bare [`Agent`] with the execution env it runs against and the
/// optional transcript log it persists to.
pub(crate) struct Harness<P> {
    pub(crate) agent: Agent<P>,
    workspace: PathBuf,
    // Shared so the loop can hand a `&ToolEnv` to several concurrency-safe tools
    // at once; tool bodies borrow it only for their synchronous duration.
    state: RefCell<ToolState>,
    // Optional transcript persistence. When present, new messages are appended
    // to the JSONL log after each turn (`persisted` tracks how many of the
    // agent's messages are already on disk). None when no log could be opened,
    // so the harness runs the agent fully in-memory.
    session: Option<SessionLog>,
    persisted: usize,
    // Out-of-context store for oversized tool outputs (issue #61). Present only
    // when a transcript log is attached, since handles live beside the session
    // file; an in-memory session keeps every output inline.
    output_store: Option<HandleStore>,
}

impl<P: ChatProvider> Harness<P> {
    /// Wrap a bare agent with its execution surface and optional transcript log.
    pub(crate) fn new(
        agent: Agent<P>,
        workspace: PathBuf,
        state: ToolState,
        session: Option<SessionLog>,
    ) -> Self {
        Self::with_persisted(agent, workspace, state, session, 0)
    }

    /// Wrap a resumed agent whose first `persisted` messages are already on
    /// disk in `session`. The cursor starts past the reconstructed history so
    /// only new turns are appended, continuing the same transcript instead of
    /// rewriting the loaded entries.
    pub(crate) fn resumed(
        agent: Agent<P>,
        workspace: PathBuf,
        state: ToolState,
        session: Option<SessionLog>,
        persisted: usize,
    ) -> Self {
        Self::with_persisted(agent, workspace, state, session, persisted)
    }

    fn with_persisted(
        agent: Agent<P>,
        workspace: PathBuf,
        state: ToolState,
        session: Option<SessionLog>,
        persisted: usize,
    ) -> Self {
        // Derive the handle store from the session file so oversized outputs are
        // stored beside the transcript that references them.
        let output_store = session
            .as_ref()
            .map(|log| HandleStore::for_session(log.path()));
        Self {
            agent,
            workspace,
            state: RefCell::new(state),
            session,
            persisted,
            output_store,
        }
    }

    /// Run one turn against the owned execution env, then persist any new
    /// transcript messages. The env is injected into the bare loop (mirroring
    /// `AgentHarness` passing `env` into the run); persistence lives here, not
    /// in the loop.
    pub(crate) async fn submit_turn(
        &mut self,
        prompt: &str,
        obs: &dyn AgentObserver,
        gate: &dyn ApprovalGate,
        token: &CancellationToken,
    ) -> Result<()> {
        let env = ToolEnv {
            workspace: &self.workspace,
            state: &self.state,
            output_store: self
                .output_store
                .as_ref()
                .map(|store| store as &dyn crate::nexus::ToolOutputStore),
        };
        // The turn span covers the loop; `Instrument` carries it across awaits
        // (a held `enter()` guard does not).
        let result = self
            .agent
            .submit_turn(prompt, obs, gate, &env, token)
            .instrument(tracing::info_span!("turn"))
            .await;
        // Persist whatever the turn produced even when it ended in an error, so
        // the transcript records the user prompt and any tool work. Best-effort:
        // a write failure is logged, never fatal to the session.
        self.persist_new_messages();
        result
    }

    /// Append messages not yet written to the transcript log, advancing the
    /// persisted cursor. No-op when no log is attached.
    fn persist_new_messages(&mut self) {
        let Some(log) = self.session.as_mut() else {
            return;
        };
        let messages = self.agent.messages();
        while self.persisted < messages.len() {
            if let Err(error) = log.append(&messages[self.persisted]) {
                tracing::warn!(error = %format!("{error:#}"), "failed to persist session message");
                return;
            }
            self.persisted += 1;
        }
    }
}
