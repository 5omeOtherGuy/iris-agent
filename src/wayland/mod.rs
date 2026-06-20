//! Tier-2 Wayland harness.
//!
//! Owns the execution surface (workspace + [`ToolState`]) and session
//! persistence, wrapping the bare in-memory [`Agent`]. Mirrors pi's
//! `AgentHarness` (`packages/agent/src/harness/agent-harness.ts`), which owns
//! the `ExecutionEnv` and the session store, feeds the env into each run, and
//! appends transcript messages itself -- the bare agent stays persistence- and
//! filesystem-free.

pub(crate) mod system_prompt;

use std::cell::RefCell;
use std::path::PathBuf;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::handles::HandleStore;
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalGate, ChatProvider, Message, Role, ToolEnv,
};
use crate::session::{SessionLog, estimate_tokens};
use crate::tools::ToolState;

/// Maximum characters in an auto-compaction summary, so compacting a large
/// range always shrinks the context regardless of how long the covered turns
/// were.
const MAX_SUMMARY_CHARS: usize = 4000;
/// Per-message excerpt cap inside the summary.
const MAX_EXCERPT_CHARS: usize = 160;

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
    // Entry ids of the persisted messages, parallel to the first `persisted`
    // agent messages. `Some(id)` = a coverable on-disk `message` entry; `None`
    // = not coverable (a resumed loaded message, tracked id-less). The
    // auto-compaction policy covers a contiguous run of `Some`-id messages.
    entry_ids: Vec<Option<String>>,
    // Context token budget that triggers auto-compaction, or `None` to disable
    // it (in-memory loop tests). The Tier-3 app passes the configured budget.
    budget: Option<u64>,
    // Out-of-context store for oversized tool outputs (issue #61). Present only
    // when a transcript log is attached, since handles live beside the session
    // file; an in-memory session keeps every output inline.
    output_store: Option<HandleStore>,
}

/// A chosen compaction: the half-open index range `[start, end)` of covered
/// messages and the inclusive entry-id bounds that range maps to on disk.
struct CompactionPlan {
    start: usize,
    end: usize,
    from_id: String,
    to_id: String,
}

impl<P: ChatProvider> Harness<P> {
    /// Wrap a bare agent with its execution surface and optional transcript log.
    /// `budget` is the context token budget that triggers auto-compaction, or
    /// `None` to disable it.
    pub(crate) fn new(
        agent: Agent<P>,
        workspace: PathBuf,
        state: ToolState,
        session: Option<SessionLog>,
        budget: Option<u64>,
    ) -> Self {
        Self::build(agent, workspace, state, session, 0, Vec::new(), budget)
    }

    /// Wrap a resumed agent whose first `persisted` messages are already on
    /// disk in `session`. The cursor starts past the reconstructed history so
    /// only new turns are appended, continuing the same transcript instead of
    /// rewriting the loaded entries.
    ///
    /// The loaded history is tracked id-less (entry ids `None`), so this slice
    /// does not re-compact already-loaded messages -- only turns appended after
    /// resume become coverable. The store's read-time rebuild already applied
    /// any prior compaction entries, so resumed context is summary-aware on
    /// arrival.
    //
    // ponytail: id-less loaded history is the known ceiling -- a resumed
    // session whose rebuilt bulk alone exceeds the budget cannot shrink further
    // until new coverable turns accumulate. Upgrade path = surface per-message
    // entry ids from the read/rebuild path so loaded originals stay coverable.
    pub(crate) fn resumed(
        agent: Agent<P>,
        workspace: PathBuf,
        state: ToolState,
        session: Option<SessionLog>,
        persisted: usize,
        budget: Option<u64>,
    ) -> Self {
        Self::build(
            agent,
            workspace,
            state,
            session,
            persisted,
            vec![None; persisted],
            budget,
        )
    }

    fn build(
        agent: Agent<P>,
        workspace: PathBuf,
        state: ToolState,
        session: Option<SessionLog>,
        persisted: usize,
        entry_ids: Vec<Option<String>>,
        budget: Option<u64>,
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
            entry_ids,
            budget,
            output_store,
        }
    }

    /// Swap the active provider at a safe turn boundary, delegating to the bare
    /// agent (which re-plans the model-visible tool surface). Tier 3 owns the
    /// active selection, system prompt, and provider construction; the harness
    /// only forwards the rebuilt provider so persistence/compaction state is
    /// untouched.
    pub(crate) fn replace_provider(&mut self, provider: P) {
        self.agent.replace_provider(provider);
    }

    /// Record a runtime mode switch as a first-class `modelSelection` entry in
    /// the transcript log. Best-effort (no-op without an attached log), mirroring
    /// message persistence: a switch is still applied even if it cannot be
    /// audited. `base_url` is deliberately not recorded.
    pub(crate) fn record_selection_event(
        &mut self,
        provider: &str,
        model: &str,
        reasoning: Option<&str>,
    ) -> Result<()> {
        let Some(log) = self.session.as_mut() else {
            return Ok(());
        };
        // The selection entry chains onto the leaf and advances the log's id
        // cursor, but it is not a transcript message: `persisted`/`entry_ids`
        // (aligned to agent messages) are intentionally left untouched, so the
        // next message append still chains correctly through it.
        log.append_selection(provider, model, reasoning)?;
        Ok(())
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
        // Safe turn boundary: before the provider request, compact if the
        // current context exceeds the configured budget. The prior turn's
        // transcript is complete here (every tool call answered), so the
        // covered range never splits a pending tool-call/result pair.
        self.maybe_auto_compact(obs)?;
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
            match log.append(&messages[self.persisted]) {
                Ok(id) => {
                    // Track the assigned entry id so a later compaction can
                    // reference this message as a coverage bound.
                    self.entry_ids.push(Some(id));
                    self.persisted += 1;
                }
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "failed to persist session message");
                    return;
                }
            }
        }
    }

    /// If the current context exceeds the budget, compact at this safe turn
    /// boundary: append a `compaction` entry covering an older message range and
    /// replace the in-memory context with `summary + retained tail`, so the
    /// next provider request uses the summary instead of the covered messages.
    /// No-op when auto-compaction is disabled, no log is attached, the context
    /// is within budget, or nothing coverable remains.
    fn maybe_auto_compact(&mut self, obs: &dyn AgentObserver) -> Result<()> {
        let Some(budget) = self.budget else {
            return Ok(());
        };
        // Compaction is a durable read-time view; without a log there is no
        // place to record it, so skip rather than mutate history in memory.
        if self.session.is_none() {
            return Ok(());
        }

        // Current provider-visible context total, using the same per-message
        // convention the store persists and rebuilds with.
        let total = context_tokens(self.agent.messages());
        if total <= budget {
            return Ok(());
        }

        let messages = self.agent.messages().to_vec();
        let Some(plan) = self.plan_compaction(&messages, budget) else {
            // Nothing coverable (e.g. resumed id-less history or a single
            // oversized message at a tool boundary): a no-op, never history
            // destruction or a faked token count.
            return Ok(());
        };

        let covered = plan.end - plan.start;
        let summary = summarize(&messages[plan.start..plan.end]);
        let summary_tokens = estimate_tokens(&summary);

        let log = self
            .session
            .as_mut()
            .expect("session present checked above");
        let compaction_id =
            log.append_compaction(&plan.from_id, &plan.to_id, &summary, Some(summary_tokens))?;
        tracing::info!(
            covered,
            from = %plan.from_id,
            to = %plan.to_id,
            compaction_id = %compaction_id,
            "auto-compacted context over token budget"
        );

        // Rebuild the in-memory context in place: anything before the covered
        // range (prior summaries) is kept, the covered range becomes one summary
        // message, and the retained tail follows. This mirrors the store's
        // read-time rebuild so live and resumed context agree.
        //
        // `entry_ids` tracks only the persisted prefix. The summary stands in
        // for a compaction entry now on disk, and the covered range and kept
        // tail were persisted earlier, so they are all represented on disk. Any
        // tail beyond `old_persisted` (a prior failed write left it unpersisted)
        // is carried in `new_messages` but left past the cursor, so
        // `persist_new_messages` still writes it on the next turn.
        let old_persisted = self.persisted;
        let mut new_messages = Vec::with_capacity(messages.len() - covered + 1);
        let mut new_entry_ids: Vec<Option<String>> =
            Vec::with_capacity(old_persisted - covered + 1);
        for (message, id) in messages[..plan.start]
            .iter()
            .zip(&self.entry_ids[..plan.start])
        {
            new_messages.push(message.clone());
            new_entry_ids.push(id.clone());
        }
        new_messages.push(Message::user(&summary));
        new_entry_ids.push(None);
        for (offset, message) in messages[plan.end..].iter().enumerate() {
            new_messages.push(message.clone());
            // Only the persisted portion of the tail keeps a tracked id.
            if plan.end + offset < old_persisted {
                new_entry_ids.push(self.entry_ids[plan.end + offset].clone());
            }
        }

        self.agent.replace_messages(new_messages);
        // Cursor = the messages represented on disk: kept leading + the summary
        // (its compaction entry) + the persisted tail.
        self.persisted = new_entry_ids.len();
        self.entry_ids = new_entry_ids;

        obs.on_event(AgentEvent::Notice(format!(
            "compacted {covered} earlier message(s) to stay within the {budget}-token context budget."
        )))
    }

    /// Choose the message range to compact. Keeps the largest recent tail whose
    /// token sum stays within budget and compacts the older coverable messages
    /// before it, clamped to the persisted/id-bearing region and adjusted so the
    /// covered range never splits a tool-call/tool-result pair. `None` when no
    /// coverable range remains.
    fn plan_compaction(&self, messages: &[Message], budget: u64) -> Option<CompactionPlan> {
        // Coverable region: the persisted prefix with known entry ids.
        let n = self.persisted.min(messages.len());
        // Keep the recent tail within a low-water target below the budget, not
        // the full budget: the new summary contributes its own tokens, so a
        // tail filling the whole budget would push the context back over budget
        // immediately and cause per-turn compaction thrash. Three-quarters
        // leaves headroom for the summary and the next prompt.
        let keep_target = budget.saturating_mul(3) / 4;
        let mut k = messages.len();
        let mut tail = 0u64;
        while k > 0 {
            let t = estimate_tokens(&messages[k - 1].content);
            if tail.saturating_add(t) > keep_target {
                break;
            }
            tail = tail.saturating_add(t);
            k -= 1;
        }
        // Covered range ends at the tail boundary, never past the coverable
        // (persisted, id-bearing) region.
        let mut end = k.min(n);
        // Start at the first coverable (Some-id) message; bail if none.
        let mut start = (0..end).find(|&i| self.entry_ids.get(i).is_some_and(Option::is_some))?;
        // Keep the covered range a contiguous run of coverable ids.
        if let Some(none_at) = (start..end).find(|&i| self.entry_ids[i].is_none()) {
            end = none_at;
        }
        // Never begin a covered range on an orphan tool result.
        while start < end && messages[start].role == Role::Tool {
            start += 1;
        }
        // Never split a tool-call / tool-result pair at the tail boundary.
        while end > start
            && (messages[end - 1].role == Role::AssistantToolCall
                || messages.get(end).is_some_and(|m| m.role == Role::Tool))
        {
            end -= 1;
        }
        if start >= end {
            return None;
        }
        Some(CompactionPlan {
            start,
            end,
            from_id: self.entry_ids[start].clone()?,
            to_id: self.entry_ids[end - 1].clone()?,
        })
    }
}

/// Sum the per-message token estimates of a context, saturating so a corrupted
/// or extreme value never panics/wraps.
fn context_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|m| estimate_tokens(&m.content))
        .fold(0, u64::saturating_add)
}

/// Produce a deterministic stand-in summary for a covered message range.
///
/// ponytail: this is the smallest real summarizer seam -- a deterministic,
/// non-fabricating excerpt of the covered turns, bounded by [`MAX_SUMMARY_CHARS`]
/// so a large range always compresses. It is the explicit swap point for a
/// provider/local summarizer later (issue #55 defers high-quality summaries);
/// storage and rebuild (`session.rs`) are independent of how this text is made.
fn summarize(messages: &[Message]) -> String {
    let mut out = format!(
        "[auto-compacted summary of {} earlier message(s)]",
        messages.len()
    );
    for message in messages {
        out.push('\n');
        out.push_str(message.role.as_str());
        out.push_str(": ");
        out.push_str(&truncate_chars(message.content.trim(), MAX_EXCERPT_CHARS));
    }
    truncate_chars(&out, MAX_SUMMARY_CHARS)
}

/// Truncate to at most `max` characters (char-boundary safe), appending an
/// ellipsis when shortened.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let kept: String = text.chars().take(max).collect();
        format!("{kept}...")
    }
}
