//! Integration tests for the opt-in microcompaction fold pass at the harness
//! seam (ADR-0048, #378): folds batch only at/above the micro-watermark (never
//! per-turn), the opt-in flag gates fold WRITING (off -> no folds), a folded
//! result is rewritten in memory AND recorded durably, and a resumed session
//! rebuilds through the persisted fold regardless of the current setting.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::Harness;
use crate::nexus::{Agent, ChatProvider, Message, ProviderStream, Tools};
use crate::session::{SessionLog, SessionStore};
use crate::tools::{ToolState, built_in_tools};

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
    harness
}

/// Count `fold` entries in a transcript.
fn fold_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["type"] == "fold")
        .count()
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

    // Budget so high the micro-watermark (budget/2) sits ABOVE the current total:
    // the batch must not run (no per-turn folding below the watermark).
    let high_budget = Some(total.saturating_mul(4));
    let mut below = resume(&root.path, &workspace.path, &path, high_budget, true);
    let applied = below.maybe_microcompact().unwrap();
    assert_eq!(applied, 0, "below the micro-watermark, nothing folds");
    assert_eq!(
        fold_count(&path),
        0,
        "no fold entry is written below the watermark"
    );
    drop(below);

    // Budget low enough that the watermark (budget/2) is at/below the total: the
    // batch runs and folds the superseded read.
    let low_budget = Some(total);
    let mut above = resume(&root.path, &workspace.path, &path, low_budget, true);
    let applied = above.maybe_microcompact().unwrap();
    assert_eq!(
        applied, 1,
        "at/above the watermark the superseded read folds"
    );
    assert_eq!(
        fold_count(&path),
        1,
        "exactly one durable fold entry is written"
    );
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

    // Opt-in OFF: even with the budget low enough that the watermark is crossed,
    // no fold is written and the in-memory needle survives.
    let mut off = resume(&root.path, &workspace.path, &path, Some(total), false);
    let applied = off.maybe_microcompact().unwrap();
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
    assert_eq!(on.maybe_microcompact().unwrap(), 1);
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
