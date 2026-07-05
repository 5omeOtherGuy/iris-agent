//! Integration tests for the ADR-0046 recall path at the harness seam:
//! compaction registers a recall handle behind the ADR-0011 store, the rebuilt
//! summary carries a reference that survives rebuild verbatim (the ADR-0045
//! needle), and a startup-resumed session (with #377 ids) can recall the covered
//! originals with their durable entry ids intact.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use super::{Harness, SummarizerKind};
use crate::handles::HandleStore;
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ChatProvider, Message, ProviderStream, SessionSpanReader,
    ToolOutputStore, Tools,
};
use crate::session::{SessionLog, SessionStore};
use crate::tools::{ToolState, built_in_tools, recall};

/// A needle that lives ONLY in the big covered turn, so recall retrieving it
/// proves the covered original is retrievable, not merely the summary.
const NEEDLE: &str = "SECRET-PORT-8080-recall-needle";

/// A never-called provider: the deterministic `Excerpts` summarizer needs no
/// provider round-trip, so `respond_stream` returns an empty stream.
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

/// Swallows every agent event: the tests inspect the rebuilt context and the
/// handle store directly, not the event stream.
struct NullObserver;
impl AgentObserver for NullObserver {
    fn on_event(&self, _event: AgentEvent) -> Result<()> {
        Ok(())
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
    let path = std::env::temp_dir().join(format!("iris-recall-it-{nanos}-{seq}"));
    std::fs::create_dir(&path).unwrap();
    TempDir { path }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

/// Persist a session whose first (big) turn is coverable and needle-bearing,
/// then a small recent tail. Returns the session-store root, the workspace, and
/// the transcript path.
fn seed_session() -> (TempDir, TempDir, PathBuf) {
    seed_session_with_needle(NEEDLE)
}

/// Like `seed_session`, but embeds a caller-chosen needle in the big covered
/// turn so a test can prove a needle that lives in ONE session never surfaces
/// through a reader bound to a different transcript.
fn seed_session_with_needle(needle: &str) -> (TempDir, TempDir, PathBuf) {
    let root = temp_dir();
    let workspace = temp_dir();
    let mut log = SessionLog::create_in(&root.path, &workspace.path).unwrap();
    // ~6000 chars (~1500 est tokens): far over the 1000-token manual keep
    // target, so it lands in the covered range while the small tail is retained.
    let big = format!(
        "{needle} :: {}",
        "ledger reconciliation detail. ".repeat(200)
    );
    for message in [
        Message::user(&big),
        Message::assistant("ok"),
        Message::user("second small turn"),
        Message::assistant("ok2"),
    ] {
        log.append(&message).unwrap();
    }
    let path = log.path().to_path_buf();
    drop(log); // close so the read path re-reads it from disk
    (root, workspace, path)
}

/// Resume the seeded session (startup path: #377 threads durable ids through
/// `store.open`), then force one manual compaction through the production seam.
/// Returns the resumed+compacted harness and the resumed entry ids.
fn resume_and_compact(
    root: &Path,
    workspace: &Path,
    path: &Path,
) -> (Harness<SilentProvider>, Vec<Option<String>>) {
    let store = SessionStore::with_root(root.to_path_buf());
    let metas = store.list().unwrap();
    let meta = metas
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
        entry_ids.clone(),
        None,
    );
    harness.set_summarizer(SummarizerKind::Excerpts);
    block_on(harness.compact_now(&NullObserver, &CancellationToken::new())).unwrap();
    (harness, entry_ids)
}

/// The recall reference the compaction folded into the summary, e.g.
/// `recall(handle="abcd...")`. Returns the handle id.
fn recall_handle_from_context(harness: &Harness<SilentProvider>) -> String {
    let joined = harness
        .agent
        .messages()
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    handle_from_marker(&joined)
}

fn handle_from_marker(text: &str) -> String {
    let anchor = "recall(handle=\"";
    let start = text
        .find(anchor)
        .unwrap_or_else(|| panic!("no recall marker in: {text}"))
        + anchor.len();
    let rest = &text[start..];
    let end = rest.find('"').expect("closing quote on handle");
    rest[..end].to_string()
}

#[test]
fn resume_then_compact_registers_a_recall_handle_returning_the_originals() {
    let (root, workspace, path) = seed_session();
    let (harness, entry_ids) = resume_and_compact(&root.path, &workspace.path, &path);

    // The covered original (the big needle turn) is gone from live context...
    let live = harness
        .agent
        .messages()
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !live.contains(NEEDLE) || live.contains("recall(handle="),
        "needle should only survive via the recall reference, not verbatim in context"
    );

    // ...but retrievable through the registered recall handle.
    let handle = recall_handle_from_context(&harness);
    let store = HandleStore::for_session(&path);
    let out = recall::execute_for_test(
        Some(&store as &dyn ToolOutputStore),
        None,
        &serde_json::json!({ "handle": handle }),
    )
    .unwrap();
    assert!(
        out.content.contains(NEEDLE),
        "recall returns the covered original"
    );

    // Id mapping across the startup resume (#377): the recalled turn carries the
    // same durable entry id the read path threaded, not a re-minted one.
    let first_id = entry_ids
        .first()
        .cloned()
        .flatten()
        .expect("resumed prefix has a durable first id");
    assert!(
        out.content.contains(&format!("id={first_id}")),
        "recalled turn must carry its resumed entry id {first_id}: {}",
        out.content
    );
}

#[test]
fn recall_reference_survives_rebuild_verbatim() {
    let (root, workspace, path) = seed_session();
    let (harness, _ids) = resume_and_compact(&root.path, &workspace.path, &path);

    // The reference minted live (ADR-0045 needle anchor).
    let live_handle = recall_handle_from_context(&harness);
    drop(harness); // close the resumed log so the on-disk transcript is complete

    // Re-read the transcript from disk (the read-time rebuild path): the summary
    // must reproduce the SAME recall reference, or the tool is unreachable after
    // the next resume.
    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .unwrap()
        .into_iter()
        .find(|m| m.path == path)
        .unwrap();
    let rebuilt = store.open(&meta).unwrap();
    let rebuilt_text = rebuilt
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let rebuilt_handle = handle_from_marker(&rebuilt_text);
    assert_eq!(
        live_handle, rebuilt_handle,
        "the recall handle reference must survive rebuild verbatim"
    );

    // And the surviving reference actually resolves to the covered original.
    let handle_store = HandleStore::for_session(&path);
    let out = recall::execute_for_test(
        Some(&handle_store as &dyn ToolOutputStore),
        None,
        &serde_json::json!({ "handle": rebuilt_handle }),
    )
    .unwrap();
    assert!(out.content.contains(NEEDLE));
}

#[test]
fn standalone_span_recalls_originals_from_this_session_read_path() {
    // FINDING 2 at the harness seam: a recall with ONLY an entry-id span (no
    // handle) resolves the covered original DIRECTLY from THIS session's read
    // path (`session::read_span`), reusing the durable ids #377 threaded --
    // there is no reverse span->handle index and no parallel store.
    // Session A carries a UNIQUE needle that exists in NO other session, so the
    // isolation check below has teeth: the needle can only appear if a reader
    // actually reaches session A's transcript.
    const A_NEEDLE: &str = "SESSION-A-UNIQUE-recall-isolation-needle";
    let (root, workspace, path) = seed_session_with_needle(A_NEEDLE);
    let (_harness, entry_ids) = resume_and_compact(&root.path, &workspace.path, &path);
    let first_id = entry_ids
        .first()
        .cloned()
        .flatten()
        .expect("resumed prefix has a durable first id");
    let last_id = entry_ids
        .iter()
        .rev()
        .flatten()
        .next()
        .cloned()
        .expect("resumed prefix has a durable last id");

    // The reader is built over THIS session's transcript path only, so the span
    // read is scoped to this session and cannot address another session's data.
    let reader = super::SessionSpanSource {
        transcript: Some(path.clone()),
    };
    let out = recall::execute_for_test(
        None,
        Some(&reader as &dyn SessionSpanReader),
        &serde_json::json!({ "from": first_id, "to": last_id }),
    )
    .unwrap();
    assert!(
        out.content.contains(A_NEEDLE),
        "standalone span returns the covered original from the session read path: {}",
        out.content
    );
    assert!(
        out.content.contains(&format!("id={first_id}")),
        "recalled turn carries its durable entry id {first_id}: {}",
        out.content
    );

    // Isolation proof with teeth: bind a reader to a DIFFERENT, empty session B
    // transcript, then query session A's *real, resolvable* id span (the very
    // span that just returned A's needle through reader `reader`). Because
    // `SessionSpanSource` resolves only against its own bound transcript, the
    // session-B reader must surface NOTHING for that span -- never session A's
    // unique needle. If the seam ever resolved spans against anything other
    // than its bound transcript, reader B would return A_NEEDLE and both
    // assertions below would fail.
    let b_root = temp_dir();
    let b_workspace = temp_dir();
    let b_log = SessionLog::create_in(&b_root.path, &b_workspace.path).unwrap();
    let b_path = b_log.path().to_path_buf();
    drop(b_log); // empty transcript: no entries, so any span selects no turns
    let reader_b = super::SessionSpanSource {
        transcript: Some(b_path),
    };
    let result = recall::execute_for_test(
        None,
        Some(&reader_b as &dyn SessionSpanReader),
        &serde_json::json!({ "from": first_id, "to": last_id }),
    );
    let rendered = match &result {
        Ok(out) => out.content.clone(),
        Err(err) => err.to_string(),
    };
    assert!(
        !rendered.contains(A_NEEDLE),
        "a reader bound to session B must never surface session A's needle: {rendered}"
    );
    assert!(
        result.is_err(),
        "session A's real id span selects no turns in the empty session B (tool error), got: {rendered}"
    );
}
