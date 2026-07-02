//! Tier-2 Wayland harness.
//!
//! Owns the execution surface (workspace + [`ToolState`]) and session
//! persistence, wrapping the bare in-memory [`Agent`]. Mirrors pi's
//! `AgentHarness` (`packages/agent/src/harness/agent-harness.ts`), which owns
//! the `ExecutionEnv` and the session store, feeds the env into each run, and
//! appends transcript messages itself -- the bare agent stays persistence- and
//! filesystem-free.

pub(crate) mod approvals;
pub(crate) mod system_prompt;
pub(crate) mod trust;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::handles::HandleStore;
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalGate, ChatProvider, Message, ProviderUsage, Role,
    SteeringSource, ToolEnv,
};
use crate::session::{
    SessionLog, UsageTotals, estimate_tokens, message_token_estimate, read_usage_totals,
};
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
    // Mid-run user-message queue (steering + follow-up). `None` outside the
    // interactive TUI: the text/non-TTY path and the loop tests never steer.
    // Shared (`Rc`) with the Tier-3 input loop, which enqueues what the user
    // typed while the turn ran; the bare agent drains it at safe injection
    // points. Forwarded per-turn as a borrow; never owned by the bare agent.
    steering: Option<Rc<dyn SteeringSource>>,
    // Cumulative provider-reported token ledger for this session (issue #206).
    // Seeded from the log's persisted `usage` entries (so a resumed session's
    // ledger continues) and extended after every provider turn.
    usage_totals: UsageTotals,
    // The most recent provider turn's usage, for the on-demand per-turn readout.
    last_usage: Option<ProviderUsage>,
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
        // Continue the persisted cumulative usage ledger (zero for a new file).
        let usage_totals = session
            .as_ref()
            .map(|log| read_usage_totals(log.path()))
            .unwrap_or_default();
        Self {
            agent,
            workspace,
            state: RefCell::new(state),
            session,
            persisted,
            entry_ids,
            budget,
            output_store,
            steering: None,
            usage_totals,
            last_usage: None,
        }
    }

    /// Install the mid-run steering/follow-up source (the Tier-3 app's typed
    /// queue). Shared via `Rc` so the input loop keeps enqueuing into the same
    /// queue the turn drains. Set once per session; the text/non-TTY path leaves
    /// it unset, so no steering is ever injected there.
    pub(crate) fn set_steering_source(&mut self, steering: Rc<dyn SteeringSource>) {
        self.steering = Some(steering);
    }

    /// Swap the active provider at a safe turn boundary, delegating to the bare
    /// agent (which re-plans the model-visible tool surface). Tier 3 owns the
    /// active selection, system prompt, and provider construction; the harness
    /// only forwards the rebuilt provider so persistence/compaction state is
    /// untouched.
    pub(crate) fn replace_provider(&mut self, provider: P) {
        self.agent.replace_provider(provider);
    }

    /// Swap the whole session at a safe turn boundary: install a different
    /// transcript log (and its handle store), reset the persistence cursor and
    /// entry-id tracking, and replace the agent's in-memory context. Drives the
    /// in-session `/resume` (loaded messages, `resumed` already on disk) and
    /// `/new` (empty messages, `resumed` = 0) swaps. The caller rebuilds and
    /// installs the provider via [`replace_provider`](Self::replace_provider)
    /// first; workspace, tool state, and budget are unchanged. Mirrors the
    /// [`build`](Self::build) cursor setup so live and resumed persistence agree.
    pub(crate) fn swap_session(
        &mut self,
        session: Option<SessionLog>,
        messages: Vec<Message>,
        resumed: usize,
    ) {
        self.output_store = session
            .as_ref()
            .map(|log| HandleStore::for_session(log.path()));
        self.usage_totals = session
            .as_ref()
            .map(|log| read_usage_totals(log.path()))
            .unwrap_or_default();
        self.last_usage = None;
        self.session = session;
        self.persisted = resumed;
        self.entry_ids = vec![None; resumed];
        self.agent.reset_session(messages);
    }

    /// Workspace root this harness executes against (the key for
    /// project-scoped state such as persistent approvals).
    pub(crate) fn workspace(&self) -> &std::path::Path {
        &self.workspace
    }

    /// Id of the attached transcript log, or `None` for an in-memory session.
    pub(crate) fn session_id(&self) -> Option<&str> {
        self.session.as_ref().map(SessionLog::id)
    }

    /// On-disk path of the attached transcript log, or `None` for an in-memory
    /// session.
    pub(crate) fn session_path(&self) -> Option<&std::path::Path> {
        self.session.as_ref().map(SessionLog::path)
    }

    /// The provider-visible conversation context, for read-only inspection
    /// (`/copy`, `/session`, `/debug`). Same view the persistence cursor walks.
    pub(crate) fn messages(&self) -> &[Message] {
        self.agent.messages()
    }

    /// Estimated tokens of the current provider-visible context, using the same
    /// per-message convention as persistence and auto-compaction.
    pub(crate) fn context_token_estimate(&self) -> u64 {
        context_tokens(self.agent.messages())
    }

    /// The configured auto-compaction context budget, when enabled.
    pub(crate) fn context_budget(&self) -> Option<u64> {
        self.budget
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
        let result = self.run_turn(prompt, obs, gate, token).await;
        let Err(error) = result else {
            return Ok(());
        };
        // Context-overflow recovery (issue #211): the provider rejected the
        // request as exceeding the model's context window -- token estimates
        // drift, so proactive compaction can undershoot. Compact now and retry
        // once; the failed turn already truncated its unanswered prompt, so
        // resubmitting the same prompt keeps the transcript valid. Anything
        // else (including a second overflow) surfaces honestly.
        if token.is_cancelled()
            || !crate::errors::is_context_overflow(&error)
            || !self.compact_for_overflow(obs)?
        {
            return Err(error);
        }
        obs.on_event(AgentEvent::Notice(
            "the provider rejected the request as exceeding the model's context window; \
             compacted older context and retrying."
                .to_string(),
        ))?;
        self.run_turn(prompt, obs, gate, token).await
    }

    /// One provider turn against the owned execution env, persisting whatever
    /// it produced (even on error, so the transcript records the user prompt
    /// and any tool work; persistence is best-effort and never fatal).
    async fn run_turn(
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
            // Streaming is Nexus-owned: it injects a per-call sink on the
            // exclusive path. The harness env carries none.
            output_sink: None,
        };
        // Capture provider-reported usage as the turn's events stream through,
        // so the harness owns the session ledger without any front-end wiring.
        let capture = UsageCapture {
            inner: obs,
            seen: RefCell::new(Vec::new()),
        };
        // The turn span covers the loop; `Instrument` carries it across awaits
        // (a held `enter()` guard does not).
        let result = self
            .agent
            .submit_turn(
                prompt,
                &capture,
                gate,
                &env,
                token,
                self.steering.as_deref(),
            )
            .instrument(tracing::info_span!("turn"))
            .await;
        self.persist_new_messages();
        self.record_usage(capture.seen.into_inner());
        result
    }

    /// Fold one turn's provider usage reports into the cumulative ledger and
    /// persist each as a `usage` entry. Best-effort like message persistence:
    /// a write failure is logged, never fatal to the session.
    fn record_usage(&mut self, reports: Vec<ProviderUsage>) {
        for usage in reports {
            self.usage_totals.add(&usage);
            if let Some(log) = self.session.as_mut()
                && let Err(error) = log.append_usage(&usage)
            {
                tracing::warn!(error = %format!("{error:#}"), "failed to persist usage entry");
            }
            self.last_usage = Some(usage);
        }
    }

    /// Cumulative provider-reported token totals for this session, including
    /// turns persisted before a resume.
    pub(crate) fn usage_totals(&self) -> UsageTotals {
        self.usage_totals
    }

    /// The most recent provider turn's reported usage, when any turn has run.
    pub(crate) fn last_usage(&self) -> Option<&ProviderUsage> {
        self.last_usage.as_ref()
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
        // Current provider-visible context total, using the same per-message
        // convention the store persists and rebuilds with.
        let total = context_tokens(self.agent.messages());
        if total <= budget {
            return Ok(());
        }
        self.compact_to(budget, obs)?;
        Ok(())
    }

    /// Forced compaction after a provider context-window rejection (issue
    /// #211): the estimates said the context fit, the provider disagreed, so
    /// shrink relative to what is actually there rather than the configured
    /// budget. Targets half the current estimated total (bounded by the
    /// configured budget when one is set). Returns whether anything was
    /// compacted -- `false` (nothing coverable) means the caller must surface
    /// the original error instead of retrying.
    fn compact_for_overflow(&mut self, obs: &dyn AgentObserver) -> Result<bool> {
        let total = context_tokens(self.agent.messages());
        let target = self.budget.unwrap_or(total).min(total) / 2;
        if target == 0 {
            return Ok(false);
        }
        self.compact_to(target, obs)
    }

    /// Compact the coverable prefix so the retained tail fits within `budget`,
    /// appending a durable `compaction` entry and installing the rebuilt
    /// context. Returns whether a compaction was applied.
    fn compact_to(&mut self, budget: u64, obs: &dyn AgentObserver) -> Result<bool> {
        // Compaction is a durable read-time view; without a log there is no
        // place to record it, so skip rather than mutate history in memory.
        if self.session.is_none() {
            return Ok(false);
        }

        let messages = self.agent.messages().to_vec();
        let Some(plan) = self.plan_compaction(&messages, budget) else {
            // Nothing coverable (e.g. resumed id-less history or a single
            // oversized message at a tool boundary): a no-op, never history
            // destruction or a faked token count.
            return Ok(false);
        };

        let covered = plan.end - plan.start;
        let original_tokens = context_tokens(&messages[plan.start..plan.end]);
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

        obs.on_event(AgentEvent::CompactionApplied {
            compaction_id,
            covered_from: plan.from_id,
            covered_to: plan.to_id,
            covered_messages: covered,
            original_tokens_estimate: original_tokens,
            summary_tokens_estimate: summary_tokens,
            budget,
        })?;
        obs.on_event(AgentEvent::Notice(format!(
            "compacted {covered} earlier message(s) to stay within the {budget}-token context budget."
        )))?;
        Ok(true)
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
            let t = message_token_estimate(&messages[k - 1]);
            if tail.saturating_add(t) > keep_target {
                break;
            }
            tail = tail.saturating_add(t);
            k -= 1;
        }
        // Covered range ends at the tail boundary, never past the coverable
        // (persisted, id-bearing) region.
        let mut end = k.min(n);
        // If the retained tail would begin inside an assistant/tool-use turn,
        // pull the boundary left so reasoning -> text -> tool calls -> results
        // stay together. A boundary before a user message (or at EOF) is already
        // between turns.
        if end < messages.len() && messages[end].role != Role::User {
            end = assistant_turn_start(messages, end);
        }
        // Start at the first coverable (Some-id) message; bail if none.
        let mut start = (0..end).find(|&i| self.entry_ids.get(i).is_some_and(Option::is_some))?;
        // Keep the covered range a contiguous run of coverable ids.
        if let Some(none_at) = (start..end).find(|&i| self.entry_ids[i].is_none()) {
            end = none_at;
        }
        // Never begin a covered range on an orphan tool fragment.
        while start < end
            && (messages[start].role == Role::Tool
                || messages[start].role == Role::AssistantToolCall)
        {
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

/// Pass-through observer that also captures each provider turn's reported
/// usage, so the harness can extend its session ledger after the turn without
/// requiring any front-end to forward events back (issue #206).
struct UsageCapture<'a> {
    inner: &'a dyn AgentObserver,
    seen: RefCell<Vec<ProviderUsage>>,
}

impl AgentObserver for UsageCapture<'_> {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        if let AgentEvent::ProviderTurnCompleted {
            usage: Some(usage), ..
        } = &event
        {
            self.seen.borrow_mut().push(usage.clone());
        }
        self.inner.on_event(event)
    }
}

/// Sum the per-message token estimates of a context, saturating so a corrupted
/// or extreme value never panics/wraps.
fn context_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(message_token_estimate)
        .fold(0, u64::saturating_add)
}

fn assistant_turn_start(messages: &[Message], mut index: usize) -> usize {
    while index > 0 && messages[index - 1].role != Role::User {
        index -= 1;
    }
    index
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
