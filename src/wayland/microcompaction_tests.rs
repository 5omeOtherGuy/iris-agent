//! Integration tests for the opt-in microcompaction fold pass at the harness
//! seam (ADR-0048, #378, #400): detection recomputes the pending set every
//! turn boundary, flushing waits for a trigger (watermark = Class C backstop;
//! compaction boundary = A1), the opt-in flag gates fold WRITING (off -> no
//! folds), a folded result is rewritten in memory AND recorded durably with
//! its trigger tag, and a resumed session rebuilds through the persisted fold
//! regardless of the current setting.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{BackgroundCompaction, Harness};
use crate::config::{CompactionCacheTiming, Settings};
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ChatProvider, CompactionOrigin, ContextPressureTier,
    FoldTrigger, Message, ProviderStream, SessionSpanReader, ToolCall, Tools,
};
use crate::session::{SessionLog, SessionStore};
use crate::tools::{ToolState, built_in_tools, recall};

/// Swallows every agent event: most tests inspect the rebuilt context and the
/// transcript directly, not the event stream.
struct NullObserver;
impl AgentObserver for NullObserver {
    fn on_event(&self, _event: AgentEvent) -> Result<()> {
        Ok(())
    }
}

/// Records compaction lifecycle events so the settings-off cancellation test can
/// assert the harness emits a `Cancelled` transition (spec IV.17) instead of
/// dropping the job silently.
#[derive(Default)]
struct LifecycleRecorder {
    events: RefCell<Vec<(crate::nexus::CompactionLifecycleState, Option<String>)>>,
}
impl AgentObserver for LifecycleRecorder {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        if let AgentEvent::CompactionLifecycle { state, message, .. } = event {
            self.events.borrow_mut().push((state, message));
        }
        Ok(())
    }
}

/// Records every fold observer event, so tests can assert the fold pass
/// surfaces its trigger tag (issue #400, design §4.4).
#[derive(Default)]
struct FoldRecorder {
    folds: RefCell<Vec<(usize, u64, FoldTrigger)>>,
}
impl AgentObserver for FoldRecorder {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        if let AgentEvent::FoldApplied {
            folds,
            reclaimed_tokens_estimate,
            trigger,
            ..
        } = event
        {
            self.folds
                .borrow_mut()
                .push((folds, reclaimed_tokens_estimate, trigger));
        }
        Ok(())
    }
}

/// A needle that lives only in the superseded (foldable) read body, so its
/// disappearance from rebuilt context proves the fold happened.
const NEEDLE: &str = "SUPERSEDED-READ-NEEDLE-7788";

struct SilentProvider;
impl ChatProvider for SilentProvider {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        Ok(Box::pin(futures::stream::empty()))
    }
}

struct TempDir {
    path: PathBuf,
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
fn temp_dir() -> TempDir {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("iris-microcompact-it-{nanos}-{seq}"));
    std::fs::create_dir(&path).unwrap();
    TempDir { path }
}

fn ok_read(call: &str, target: &str, body: &str) -> Message {
    Message::tool_result(
        call,
        "read",
        &json!({
            "ok": true,
            "content": body,
            "metadata": { "target": target }
        })
        .to_string(),
    )
}

fn read_call(call: &str, target: &str) -> Message {
    Message::assistant_tool_call(&ToolCall {
        id: call.to_string(),
        name: "read".to_string(),
        arguments: json!({"path": target}),
        thought_signature: None,
    })
}

/// Seed a session where an early large read of `a.rs` (needle-bearing) is
/// superseded by a later read of the same path, followed by a recent tail. The
/// superseded read sits before the protected fold tail, so it is foldable once
/// the context reaches the micro-watermark.
fn seed_session() -> (TempDir, TempDir, PathBuf) {
    let root = temp_dir();
    let workspace = temp_dir();
    let mut log = SessionLog::create_in(&root.path, &workspace.path).unwrap();
    let big = format!("{NEEDLE} :: {}", "reconciliation detail. ".repeat(300));
    // The superseding read's body must fill the protected fold tail
    // (MICRO_FOLD_KEEP_TOKENS) so the earlier superseded read sits BEFORE the
    // tail and is foldable.
    let big2 = "current contents. ".repeat(800);
    for message in [
        Message::user("read a.rs"),
        read_call("c1", "a.rs"),
        ok_read("c1", "a.rs", &big),
        Message::assistant("ok"),
        Message::user("read a.rs again"),
        read_call("c2", "a.rs"),
        ok_read("c2", "a.rs", &big2),
        Message::assistant("done"),
    ] {
        log.append(&message).unwrap();
    }
    let path = log.path().to_path_buf();
    drop(log);
    (root, workspace, path)
}

/// Resume the seeded session into a harness with a chosen `budget` and
/// microcompaction flag (startup path threads durable ids through `store.open`).
fn resume(
    root: &Path,
    workspace: &Path,
    path: &Path,
    budget: Option<u64>,
    microcompaction: bool,
) -> Harness<SilentProvider> {
    let store = SessionStore::with_root(root.to_path_buf());
    let meta = store
        .list()
        .unwrap()
        .into_iter()
        .find(|m| m.path == path)
        .expect("seeded session is listed");
    let stored = store.open(&meta).unwrap();
    let entry_ids = stored.entry_ids.clone();
    let log = SessionLog::resume(path).unwrap();
    let agent = Agent::resumed(SilentProvider, built_in_tools(), stored.messages);
    let mut harness = Harness::resumed(
        agent,
        workspace.to_path_buf(),
        ToolState::new(),
        Some(log),
        entry_ids,
        budget,
    );
    harness.set_microcompaction(microcompaction);
    if let Some(budget) = budget {
        harness.set_microcompaction_watermark(budget / 2);
    }
    harness
}

#[test]
fn disabling_auto_compaction_cancels_the_background_job_and_clears_diagnostics() {
    // The `/settings` live-apply path: an in-flight background job plus a
    // disabled trigger must cancel the job so the status chip clears (the loop
    // reconciles the chip from `background_running`).
    let (root, workspace, path) = seed_session();
    let mut harness = resume(&root.path, &workspace.path, &path, Some(200_000), false);

    // Enabled trigger: diagnostics report the resolved ladder + automatic on.
    let mut trigger = Settings::default().compaction_trigger().unwrap();
    harness.set_compaction_trigger(200_000.into(), trigger);
    let diag = harness.context_diagnostics().expect("ladder present");
    assert!(diag.automatic_enabled);
    assert_eq!(diag.ladder.effective_window, 200_000);

    // Install a running background job so the chip would be lit.
    let (_sender, receiver) = mpsc::channel();
    harness.compaction.background = Some(BackgroundCompaction {
        job_id: "job".to_string(),
        session_id: None,
        from_id: "1".to_string(),
        to_id: "2".to_string(),
        covered_messages: 2,
        original_tokens: 10,
        receiver,
        token: CancellationToken::new(),
        origin: CompactionOrigin::Subagent,
        trigger_tier: Some(ContextPressureTier::Start),
        started_at: std::time::Instant::now(),
        selection_generation: 0,
    });
    assert!(
        harness.context_diagnostics().unwrap().background_running,
        "job is running"
    );

    // Disable automatic compaction and cancel the job (the settings apply path).
    trigger.enabled = false;
    harness.set_compaction_trigger(200_000.into(), trigger);
    assert!(
        harness.cancel_auto_compaction(&NullObserver).unwrap(),
        "a job was cancelled"
    );

    let diag = harness
        .context_diagnostics()
        .expect("ladder still resolves");
    assert!(!diag.automatic_enabled, "automatic reads off");
    assert!(
        !diag.background_running,
        "the chip clears: no job runs after cancel"
    );
    // A no-op cancel (no job) reports nothing to clear.
    assert!(!harness.cancel_auto_compaction(&NullObserver).unwrap());
}

#[test]
fn disabling_auto_compaction_emits_a_cancelled_lifecycle_event() {
    // Spec IV.17: turning automatic compaction off in `/settings` must not drop
    // an in-flight background job silently. The settings-off cancellation emits
    // a `Cancelled` lifecycle so observers/logs record the Running -> Cancelled
    // transition (the UI chip is reconciled separately by the loop).
    let (root, workspace, path) = seed_session();
    let mut harness = resume(&root.path, &workspace.path, &path, Some(200_000), false);

    let (_sender, receiver) = mpsc::channel();
    harness.compaction.background = Some(BackgroundCompaction {
        job_id: "job".to_string(),
        session_id: None,
        from_id: "1".to_string(),
        to_id: "2".to_string(),
        covered_messages: 2,
        original_tokens: 10,
        receiver,
        token: CancellationToken::new(),
        origin: CompactionOrigin::Subagent,
        trigger_tier: Some(ContextPressureTier::Start),
        started_at: std::time::Instant::now(),
        selection_generation: 0,
    });

    let recorder = LifecycleRecorder::default();
    assert!(
        harness.cancel_auto_compaction(&recorder).unwrap(),
        "a job was cancelled"
    );

    let events = recorder.events.borrow();
    assert_eq!(events.len(), 1, "exactly one lifecycle event fires");
    let (state, message) = &events[0];
    assert_eq!(*state, crate::nexus::CompactionLifecycleState::Cancelled);
    let message = message.as_deref().unwrap_or_default();
    assert!(
        message.contains("automatic compaction disabled in settings"),
        "the cancellation names the settings-off cause: {message}"
    );

    // A no-op cancel (no job) emits nothing.
    let recorder = LifecycleRecorder::default();
    assert!(!harness.cancel_auto_compaction(&recorder).unwrap());
    assert!(
        recorder.events.borrow().is_empty(),
        "no job means no lifecycle event"
    );
}

/// Count `fold` entries in a transcript.
fn fold_count(path: &Path) -> usize {
    fold_entries(path).len()
}

/// The parsed `fold` entries in a transcript, for trigger-tag assertions.
fn fold_entries(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["type"] == "fold")
        .collect()
}

fn set_timing(
    harness: &mut Harness<SilentProvider>,
    timing: CompactionCacheTiming,
    trigger_tokens: u64,
) {
    let mut policy = Settings {
        microcompaction: Some(true),
        ..Settings::default()
    }
    .tool_result_compaction()
    .unwrap();
    policy.cache_timing = timing;
    policy.trigger_tokens = trigger_tokens;
    harness.set_tool_result_compaction(policy);
}

#[test]
fn active_background_range_freezes_inside_folds_but_outside_folds_flush() {
    let root = temp_dir();
    let workspace = temp_dir();
    let mut log = SessionLog::create_in(&root.path, &workspace.path).unwrap();
    let mut ids = Vec::new();
    for (index, body) in ["old one", "old two", "current three"]
        .into_iter()
        .enumerate()
    {
        let call = format!("c{}", index + 1);
        for message in [
            Message::user("read a.rs"),
            read_call(&call, "a.rs"),
            ok_read(&call, "a.rs", body),
            Message::assistant("ok"),
        ] {
            ids.push(log.append(&message).unwrap());
        }
    }
    let path = log.path().to_path_buf();
    let session_id = log.id().to_string();
    drop(log);

    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .unwrap()
        .into_iter()
        .find(|meta| meta.path == path)
        .unwrap();
    let stored = store.open(&meta).unwrap();
    let session = SessionLog::resume(&path).unwrap();
    let agent = Agent::resumed(SilentProvider, built_in_tools(), stored.messages);
    let mut harness = Harness::resumed(
        agent,
        workspace.path.clone(),
        ToolState::new(),
        Some(session),
        stored.entry_ids,
        None,
    );
    let mut policy = Settings {
        microcompaction: Some(true),
        ..Settings::default()
    }
    .tool_result_compaction()
    .unwrap();
    policy.cache_timing = CompactionCacheTiming::Immediate;
    policy.semantic_dedupe.protect_recent_tokens = 0;
    policy.semantic_dedupe.protect_recent_tool_results = 0;
    policy.semantic_dedupe.retain_per_path = 1;
    harness.set_tool_result_compaction(policy);

    let (_sender, receiver) = mpsc::channel();
    harness.compaction.background = Some(BackgroundCompaction {
        job_id: "compaction_freeze".to_string(),
        session_id: Some(session_id),
        from_id: ids[0].clone(),
        to_id: ids[3].clone(),
        covered_messages: 4,
        original_tokens: 10,
        receiver,
        token: CancellationToken::new(),
        origin: CompactionOrigin::Subagent,
        trigger_tier: Some(ContextPressureTier::Start),
        started_at: std::time::Instant::now(),
        selection_generation: 0,
    });

    let (frozen, _frozen_tokens) = harness.frozen_fold_stats();
    assert_eq!(frozen, 1);
    let pending = harness.pending_folds();
    assert_eq!(pending.len(), 1, "the c1 result is frozen under the job");
    assert_eq!(pending[0].tool_call_id, "c2", "outside fold stays eligible");
    assert_eq!(harness.maybe_microcompact(&NullObserver).unwrap(), 1);
    assert_eq!(fold_count(&path), 1, "only the outside fold is durable");

    harness.compaction.cancel_background();
    let released = harness.pending_folds();
    assert_eq!(released.len(), 1, "the frozen fold releases with the slot");
    assert_eq!(released[0].tool_call_id, "c1");
    assert_eq!(harness.maybe_microcompact(&NullObserver).unwrap(), 1);
    assert_eq!(fold_count(&path), 2);
}

#[test]
fn cache_timing_break_only_uses_breaks_but_not_pressure() {
    let (root, workspace, path) = seed_session();
    let mut harness = resume(&root.path, &workspace.path, &path, None, true);
    set_timing(&mut harness, CompactionCacheTiming::BreakOnly, 1);
    assert_eq!(harness.maybe_microcompact(&NullObserver).unwrap(), 0);
    harness
        .record_selection_event("provider-b", "model-b", None)
        .unwrap();
    let recorder = FoldRecorder::default();
    assert_eq!(harness.maybe_microcompact(&recorder).unwrap(), 1);
    assert_eq!(recorder.folds.borrow()[0].2, FoldTrigger::SelectionSwitch);
    assert_eq!(fold_entries(&path)[0]["trigger"], "A2");
}

#[test]
fn cache_timing_pressure_only_ignores_breaks_until_watermark() {
    let (root, workspace, path) = seed_session();
    let mut harness = resume(&root.path, &workspace.path, &path, None, true);
    let total = harness.context_token_estimate();
    set_timing(&mut harness, CompactionCacheTiming::PressureOnly, total + 1);
    harness
        .record_selection_event("provider-b", "model-b", None)
        .unwrap();
    assert_eq!(harness.maybe_microcompact(&NullObserver).unwrap(), 0);
    set_timing(&mut harness, CompactionCacheTiming::PressureOnly, total);
    let recorder = FoldRecorder::default();
    assert_eq!(harness.maybe_microcompact(&recorder).unwrap(), 1);
    assert_eq!(recorder.folds.borrow()[0].2, FoldTrigger::Watermark);
    assert_eq!(fold_entries(&path)[0]["trigger"], "C");
}

#[test]
fn cache_timing_immediate_flushes_with_honest_trigger() {
    let (root, workspace, path) = seed_session();
    let mut harness = resume(&root.path, &workspace.path, &path, None, true);
    set_timing(&mut harness, CompactionCacheTiming::Immediate, u64::MAX);
    let recorder = FoldRecorder::default();
    assert_eq!(harness.maybe_microcompact(&recorder).unwrap(), 1);
    assert_eq!(recorder.folds.borrow()[0].2, FoldTrigger::Immediate);
    assert_eq!(fold_entries(&path)[0]["trigger"], "I");
}

#[test]
fn folded_result_is_recoverable_by_tool_call_id_from_the_original_transcript() {
    let (root, workspace, path) = seed_session();
    let mut harness = resume(&root.path, &workspace.path, &path, None, true);
    set_timing(&mut harness, CompactionCacheTiming::Immediate, u64::MAX);
    assert_eq!(harness.maybe_microcompact(&NullObserver).unwrap(), 1);
    assert!(
        !harness
            .messages()
            .iter()
            .any(|message| message.content.contains(NEEDLE))
    );

    let reader = super::SessionSpanSource {
        transcript: Some(path),
    };
    let output = recall::execute_for_test(
        None,
        Some(&reader as &dyn SessionSpanReader),
        &json!({"tool_call_id":"c1"}),
    )
    .unwrap();
    assert!(output.content.contains(NEEDLE));
    assert!(output.content.contains("tool=read"));
    assert!(output.content.contains("a.rs"));
}

#[test]
fn no_fold_below_the_micro_watermark_and_a_fold_at_or_above_it() {
    let (root, workspace, path) = seed_session();

    // Read the context total from a probe resume (no fold pass invoked, so the
    // transcript stays pristine).
    let probe = resume(&root.path, &workspace.path, &path, None, true);
    let total = probe.context_token_estimate();
    assert!(total > 0);
    drop(probe);

    // Watermark set high enough to sit ABOVE the current total:
    // the batch must not run (no per-turn folding below the watermark).
    let high_budget = Some(total.saturating_mul(4));
    let mut below = resume(&root.path, &workspace.path, &path, high_budget, true);
    let applied = below.maybe_microcompact(&NullObserver).unwrap();
    assert_eq!(applied, 0, "below the micro-watermark, nothing folds");
    assert_eq!(
        fold_count(&path),
        0,
        "no fold entry is written below the watermark"
    );
    drop(below);

    // Watermark set low enough to sit at/below the total: the
    // batch runs and folds the superseded read.
    let low_budget = Some(total);
    let mut above = resume(&root.path, &workspace.path, &path, low_budget, true);
    let applied = above.maybe_microcompact(&NullObserver).unwrap();
    assert_eq!(
        applied, 1,
        "at/above the watermark the superseded read folds"
    );
    assert_eq!(
        fold_count(&path),
        1,
        "exactly one durable fold entry is written"
    );
    assert_eq!(fold_entries(&path)[0]["reasons"], json!(["semanticDedupe"]));
    // The fold rewrote the in-memory result content: the needle is gone and the
    // stub names the recoverable path.
    let joined = above
        .agent
        .messages()
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !joined.contains(NEEDLE),
        "folded body left in-memory context"
    );
    assert!(
        joined.contains("a.rs"),
        "the stub names the superseded path"
    );
}

#[test]
fn microcompaction_off_writes_no_folds_even_above_the_watermark() {
    let (root, workspace, path) = seed_session();
    let probe = resume(&root.path, &workspace.path, &path, None, false);
    let total = probe.context_token_estimate();
    drop(probe);

    // Opt-in OFF: even with the watermark crossed,
    // no fold is written and the in-memory needle survives.
    let mut off = resume(&root.path, &workspace.path, &path, Some(total), false);
    let applied = off.maybe_microcompact(&NullObserver).unwrap();
    assert_eq!(applied, 0, "opt-in off -> the fold pass is a no-op");
    assert_eq!(fold_count(&path), 0, "opt-in off -> no fold entry written");
    let joined = off
        .agent
        .messages()
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains(NEEDLE), "nothing was folded while off");
}

#[test]
fn a_fold_written_while_on_rebuilds_after_the_setting_is_turned_off() {
    let (root, workspace, path) = seed_session();
    let probe = resume(&root.path, &workspace.path, &path, None, true);
    let total = probe.context_token_estimate();
    drop(probe);

    // Fold once with microcompaction ON.
    let mut on = resume(&root.path, &workspace.path, &path, Some(total), true);
    assert_eq!(on.maybe_microcompact(&NullObserver).unwrap(), 1);
    drop(on);

    // Reopen the transcript: rebuild honors the persisted fold regardless of the
    // (now irrelevant) live setting -- the rebuilt result is the stub, and the
    // needle is only recoverable through the verbatim original / recall.
    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .unwrap()
        .into_iter()
        .find(|m| m.path == path)
        .unwrap();
    let reopened = store.open(&meta).unwrap();
    let joined = reopened
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !joined.contains(NEEDLE),
        "persisted fold must apply on rebuild"
    );
    assert!(
        joined.contains("a.rs"),
        "the stub names the path after rebuild"
    );
}

#[test]
fn detection_holds_pending_folds_until_a_trigger_fires() {
    // Hold-path regression (issue #400, design §4.1/§4.4): the fold policy
    // DETECTS the superseded read at every boundary, but with no trigger (cache
    // presumed warm, context below the watermark) nothing is flushed -- no fold
    // entry, no in-memory rewrite, no observer event.
    let (root, workspace, path) = seed_session();
    let probe = resume(&root.path, &workspace.path, &path, None, true);
    let total = probe.context_token_estimate();
    drop(probe);

    let high_budget = Some(total.saturating_mul(4));
    let mut held = resume(&root.path, &workspace.path, &path, high_budget, true);
    assert_eq!(
        held.pending_fold_count(),
        1,
        "detection sees the superseded read even while holding"
    );
    let (pending, reclaimable) = held.pending_fold_stats();
    assert_eq!(pending, 1);
    assert!(
        reclaimable > 0,
        "the pending set reports its reclaimable mass for /context"
    );
    let recorder = FoldRecorder::default();
    assert_eq!(held.maybe_microcompact(&recorder).unwrap(), 0);
    assert_eq!(fold_count(&path), 0, "held folds are never persisted");
    assert!(
        recorder.folds.borrow().is_empty(),
        "no fold event while holding"
    );
    let joined = held
        .agent
        .messages()
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains(NEEDLE), "held fold leaves context verbatim");
    drop(held);

    // The pending set is derived state: a fresh resume recomputes the same
    // pending fold from the transcript alone (no persistence of the ledger).
    let resumed = resume(&root.path, &workspace.path, &path, high_budget, true);
    assert_eq!(
        resumed.pending_fold_count(),
        1,
        "pending set survives resume by recomputation"
    );
}

#[test]
fn pending_folds_are_empty_when_microcompaction_is_off() {
    // Detection is gated by the opt-in exactly like flushing: off means the
    // scheduler holds nothing and the accounting surface reports zero pending.
    let (root, workspace, path) = seed_session();
    let off = resume(&root.path, &workspace.path, &path, Some(1), false);
    assert_eq!(off.pending_fold_count(), 0);
}

#[test]
fn watermark_flush_is_tagged_class_c_on_the_entry_and_the_event() {
    // Trigger-source tagging (issue #400, design §4.4): a watermark-driven
    // flush records `C` on the persisted fold entry and on the observer event.
    let (root, workspace, path) = seed_session();
    let probe = resume(&root.path, &workspace.path, &path, None, true);
    let total = probe.context_token_estimate();
    drop(probe);

    let mut above = resume(&root.path, &workspace.path, &path, Some(total), true);
    let recorder = FoldRecorder::default();
    assert_eq!(above.maybe_microcompact(&recorder).unwrap(), 1);
    let entries = fold_entries(&path);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["trigger"], "C", "fold entry carries the trigger");
    let folds = recorder.folds.borrow();
    assert_eq!(folds.len(), 1, "one fold event per flush batch");
    let (count, reclaimed, trigger) = folds[0];
    assert_eq!(count, 1);
    assert!(reclaimed > 0, "the fold event reports reclaimed mass");
    assert_eq!(trigger, FoldTrigger::Watermark);
}

#[test]
fn compaction_boundary_flush_is_tagged_class_a1() {
    // When the context also exceeds the compaction budget, the same boundary
    // will compact: the fold flush rides that break and is attributed A1, not C.
    let (root, workspace, path) = seed_session();
    let probe = resume(&root.path, &workspace.path, &path, None, true);
    let total = probe.context_token_estimate();
    drop(probe);

    // Budget below the current total: compaction would fire this boundary.
    let mut over = resume(&root.path, &workspace.path, &path, Some(total / 2), true);
    let recorder = FoldRecorder::default();
    assert_eq!(over.maybe_microcompact(&recorder).unwrap(), 1);
    let entries = fold_entries(&path);
    assert_eq!(entries[0]["trigger"], "A1");
    assert_eq!(
        recorder.folds.borrow()[0].2,
        FoldTrigger::CompactionBoundary
    );
}

/// A profile with no cold threshold and no minimum, so only the armed break
/// flags (A2/A3) and the watermark can trigger -- isolates the flag logic.
fn neutral_profile() -> super::CacheProfile {
    super::CacheProfile::default()
}

/// Rewrite every entry timestamp in a transcript to `ts_ms`, simulating a
/// session whose prior activity is old (cold resume, trigger A4).
fn rewrite_timestamps(path: &Path, ts_ms: u64) {
    let rewritten: String = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| {
            let mut value: Value = serde_json::from_str(line).unwrap();
            if value.get("timestamp").is_some() {
                value["timestamp"] = json!(ts_ms);
            }
            format!("{value}\n")
        })
        .collect();
    std::fs::write(path, rewritten).unwrap();
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[test]
fn a_selection_switch_flushes_pending_folds_and_is_tagged_a2() {
    // A2 (issue #400): a model/provider switch is a full prefix-cache break,
    // so pending folds flush before the next request even far below the
    // watermark -- and the flag is boundary-scoped (consumed once).
    let (root, workspace, path) = seed_session();
    let probe = resume(&root.path, &workspace.path, &path, None, true);
    let total = probe.context_token_estimate();
    drop(probe);

    let mut h = resume(
        &root.path,
        &workspace.path,
        &path,
        Some(total.saturating_mul(4)),
        true,
    );
    h.set_cache_profile(neutral_profile());
    h.note_active_selection("prov-a", "model-a", None);
    h.record_selection_event("prov-a", "model-b", None).unwrap();
    let recorder = FoldRecorder::default();
    assert_eq!(h.maybe_microcompact(&recorder).unwrap(), 1);
    assert_eq!(recorder.folds.borrow()[0].2, FoldTrigger::SelectionSwitch);
    let entries = fold_entries(&path);
    assert_eq!(entries[0]["trigger"], "A2");
}

#[test]
fn a_reasoning_only_switch_is_tagged_a3_and_an_identical_reselection_holds() {
    let (root, workspace, path) = seed_session();
    let probe = resume(&root.path, &workspace.path, &path, None, true);
    let total = probe.context_token_estimate();
    drop(probe);

    let mut h = resume(
        &root.path,
        &workspace.path,
        &path,
        Some(total.saturating_mul(4)),
        true,
    );
    h.set_cache_profile(neutral_profile());
    // An identical re-selection changes no request bytes: nothing is armed.
    h.note_active_selection("prov-a", "model-a", Some("medium"));
    h.record_selection_event("prov-a", "model-a", Some("medium"))
        .unwrap();
    let recorder = FoldRecorder::default();
    assert_eq!(h.maybe_microcompact(&recorder).unwrap(), 0);
    assert_eq!(fold_count(&path), 0);

    // A reasoning-only change is a message-level break: folds are covered.
    h.record_selection_event("prov-a", "model-a", Some("high"))
        .unwrap();
    assert_eq!(h.maybe_microcompact(&recorder).unwrap(), 1);
    assert_eq!(recorder.folds.borrow()[0].2, FoldTrigger::ReasoningSwitch);
    assert_eq!(fold_entries(&path)[0]["trigger"], "A3");
}

#[test]
fn a_cold_resume_flushes_at_the_first_boundary_and_is_tagged_a4() {
    // A4: the transcript's last activity is far past the profile's cold
    // threshold, so the cache is expired and the first boundary flushes free.
    let (root, workspace, path) = seed_session();
    // Two hours idle vs a 6-minute cold threshold.
    rewrite_timestamps(&path, unix_now_ms().saturating_sub(2 * 60 * 60 * 1000));
    let probe = resume(&root.path, &workspace.path, &path, None, true);
    let total = probe.context_token_estimate();
    drop(probe);

    let mut h = resume(
        &root.path,
        &workspace.path,
        &path,
        Some(total.saturating_mul(4)),
        true,
    );
    h.set_cache_profile(super::CacheProfile {
        cold_after: Some(std::time::Duration::from_secs(6 * 60)),
        ..neutral_profile()
    });
    let recorder = FoldRecorder::default();
    assert_eq!(h.maybe_microcompact(&recorder).unwrap(), 1);
    assert_eq!(recorder.folds.borrow()[0].2, FoldTrigger::ColdResume);
    assert_eq!(fold_entries(&path)[0]["trigger"], "A4");
}

#[test]
fn a_warm_resume_holds_and_the_a4_check_is_first_boundary_only() {
    // The seeded timestamps are fresh (now), so the resume is warm: no A4.
    let (root, workspace, path) = seed_session();
    let probe = resume(&root.path, &workspace.path, &path, None, true);
    let total = probe.context_token_estimate();
    drop(probe);

    let mut h = resume(
        &root.path,
        &workspace.path,
        &path,
        Some(total.saturating_mul(4)),
        true,
    );
    h.set_cache_profile(super::CacheProfile {
        cold_after: Some(std::time::Duration::from_secs(6 * 60)),
        ..neutral_profile()
    });
    let recorder = FoldRecorder::default();
    assert_eq!(h.maybe_microcompact(&recorder).unwrap(), 0);
    assert_eq!(fold_count(&path), 0, "warm resume holds the pending folds");
}

#[test]
fn a_context_below_the_minimum_cacheable_prefix_flushes_and_is_tagged_a5() {
    // A5: below the provider's minimum cacheable prefix nothing is cached,
    // so a fold breaks nothing and pending folds flush free.
    let (root, workspace, path) = seed_session();
    let probe = resume(&root.path, &workspace.path, &path, None, true);
    let total = probe.context_token_estimate();
    drop(probe);

    let mut h = resume(
        &root.path,
        &workspace.path,
        &path,
        Some(total.saturating_mul(4)),
        true,
    );
    h.set_cache_profile(super::CacheProfile {
        min_cacheable_tokens: total.saturating_mul(2),
        ..neutral_profile()
    });
    let recorder = FoldRecorder::default();
    assert_eq!(h.maybe_microcompact(&recorder).unwrap(), 1);
    assert_eq!(recorder.folds.borrow()[0].2, FoldTrigger::BelowMinCacheable);
    assert_eq!(fold_entries(&path)[0]["trigger"], "A5");
}

#[test]
fn manual_compact_flushes_pending_folds_tagged_a6_before_compacting() {
    // A6, through the production seam: `/compact` is a user-initiated break;
    // pending folds ride it (fold-then-compact order), tagged A6. The seed
    // carries bulky old text turns so the covered range still shrinks after
    // the fold reclaimed the superseded read.
    let root = temp_dir();
    let workspace = temp_dir();
    let mut log = SessionLog::create_in(&root.path, &workspace.path).unwrap();
    let bulk = "long-standing project discussion detail. ".repeat(200);
    let big = format!("{NEEDLE} :: {}", "reconciliation detail. ".repeat(300));
    let big2 = "current contents. ".repeat(800);
    for message in [
        Message::user(&bulk),
        Message::assistant(&bulk),
        Message::user("read a.rs"),
        ok_read("c1", "a.rs", &big),
        Message::assistant("ok"),
        Message::user("read a.rs again"),
        ok_read("c2", "a.rs", &big2),
        Message::assistant("done"),
    ] {
        log.append(&message).unwrap();
    }
    let path = log.path().to_path_buf();
    drop(log);
    let probe = resume(&root.path, &workspace.path, &path, None, true);
    let total = probe.context_token_estimate();
    drop(probe);

    // Watermark far above the total: only the manual break can flush.
    let mut h = resume(
        &root.path,
        &workspace.path,
        &path,
        Some(total.saturating_mul(4)),
        true,
    );
    h.set_cache_profile(neutral_profile());
    let recorder = FoldRecorder::default();
    let token = CancellationToken::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(h.compact_now(&recorder, &token))
        .unwrap();
    let entries = fold_entries(&path);
    assert_eq!(
        entries.len(),
        1,
        "the pending fold flushed with the compact"
    );
    assert_eq!(entries[0]["trigger"], "A6");
    assert_eq!(recorder.folds.borrow()[0].2, FoldTrigger::ManualCompact);
    // And the compaction itself still ran (a durable compaction entry exists).
    let compactions = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["type"] == "compaction")
        .count();
    assert_eq!(
        compactions, 1,
        "manual compaction proceeded after the flush"
    );
}

#[test]
fn a_mid_session_idle_gap_past_the_cold_threshold_flushes_and_is_tagged_b() {
    // Class B (Phase 2): after the first boundary consumed the resume-time
    // A4 check, a mid-session idle gap past the profile's cold threshold
    // releases the pending folds -- the cache is expired, so the flush is
    // free. The gap is measured from the transcript's last activity.
    let (root, workspace, path) = seed_session();
    let probe = resume(&root.path, &workspace.path, &path, None, true);
    let total = probe.context_token_estimate();
    drop(probe);

    let mut h = resume(
        &root.path,
        &workspace.path,
        &path,
        Some(total.saturating_mul(4)),
        true,
    );
    // First boundary: neutral profile, consumes the A4 resume check and holds.
    h.set_cache_profile(neutral_profile());
    let recorder = FoldRecorder::default();
    assert_eq!(h.maybe_microcompact(&recorder).unwrap(), 0);

    // Mid-session: install a cold threshold far below the (real) idle gap.
    h.set_cache_profile(super::CacheProfile {
        cold_after: Some(std::time::Duration::from_millis(10)),
        ..neutral_profile()
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(h.maybe_microcompact(&recorder).unwrap(), 1);
    assert_eq!(recorder.folds.borrow()[0].2, FoldTrigger::InferredCold);
    assert_eq!(fold_entries(&path)[0]["trigger"], "B");
}

#[test]
fn a_short_mid_session_gap_below_the_threshold_holds() {
    // The inferred-cold trigger never fires while the gap is within the
    // profile threshold: recent transcript activity means a warm cache.
    let (root, workspace, path) = seed_session();
    let probe = resume(&root.path, &workspace.path, &path, None, true);
    let total = probe.context_token_estimate();
    drop(probe);

    let mut h = resume(
        &root.path,
        &workspace.path,
        &path,
        Some(total.saturating_mul(4)),
        true,
    );
    h.set_cache_profile(neutral_profile());
    let recorder = FoldRecorder::default();
    assert_eq!(h.maybe_microcompact(&recorder).unwrap(), 0);

    // A generous threshold: the seconds-old transcript is well within it.
    h.set_cache_profile(super::CacheProfile {
        cold_after: Some(std::time::Duration::from_secs(60 * 60)),
        ..neutral_profile()
    });
    assert_eq!(h.maybe_microcompact(&recorder).unwrap(), 0);
    assert_eq!(fold_count(&path), 0, "a warm mid-session cache holds");
}

#[test]
fn fold_event_reaches_the_ui_layer_with_its_tag() {
    // The nexus fold event maps into the UI event stream carrying its trigger
    // (issue #400 M1 DoD): wayland -> nexus event -> UiEvent.
    let event = AgentEvent::FoldApplied {
        folds: 2,
        semantic_dedupe_folds: 2,
        tool_clearing_folds: 0,
        reclaimed_tokens_estimate: 1200,
        trigger: FoldTrigger::Watermark,
    };
    match crate::ui::UiEvent::from_agent_event(event) {
        crate::ui::UiEvent::FoldApplied {
            folds,
            reclaimed_tokens_estimate,
            trigger,
            ..
        } => {
            assert_eq!(folds, 2);
            assert_eq!(reclaimed_tokens_estimate, 1200);
            assert_eq!(trigger, FoldTrigger::Watermark);
        }
        other => panic!("expected FoldApplied, got {other:?}"),
    }
}
