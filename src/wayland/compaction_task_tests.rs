//! Issue #450: compaction carries open task state without making folds own task
//! state.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::{Harness, SummarizerKind};
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ChatProvider, Message, MutationGuard, ProviderStream, Tools,
};
use crate::session::{SessionLog, SessionStore};
use crate::tools::{ToolState, built_in_tools};

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
    let path = std::env::temp_dir().join(format!("iris-compaction-task-{nanos}-{seq}"));
    std::fs::create_dir(&path).unwrap();
    TempDir { path }
}

fn run_git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_git_workspace() -> TempDir {
    let workspace = temp_dir();
    run_git(&workspace.path, &["init"]);
    run_git(
        &workspace.path,
        &["config", "user.email", "iris@example.test"],
    );
    run_git(&workspace.path, &["config", "user.name", "Iris Test"]);
    std::fs::write(workspace.path.join("README.md"), "seed\n").unwrap();
    run_git(&workspace.path, &["add", "README.md"]);
    run_git(&workspace.path, &["commit", "-m", "seed"]);
    workspace
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn seed_harness(
    root: &Path,
    workspace: &Path,
    messages: impl IntoIterator<Item = Message>,
    budget: Option<u64>,
    microcompaction: bool,
) -> (Harness<SilentProvider>, PathBuf) {
    let mut log = SessionLog::create_in(root, workspace).unwrap();
    for message in messages {
        log.append(&message).unwrap();
    }
    let path = log.path().to_path_buf();
    drop(log);

    let store = SessionStore::with_root(root.to_path_buf());
    let meta = store
        .list()
        .unwrap()
        .into_iter()
        .find(|m| m.path == path)
        .expect("seeded session listed");
    let stored = store.open(&meta).unwrap();
    let entry_ids = stored.entry_ids.clone();
    let log = SessionLog::resume(&path).unwrap();
    let agent = Agent::resumed(SilentProvider, built_in_tools(), stored.messages);
    let mut harness = Harness::resumed(
        agent,
        workspace.to_path_buf(),
        ToolState::new(),
        Some(log),
        entry_ids,
        budget,
    );
    harness.set_summarizer(SummarizerKind::Excerpts);
    harness.set_microcompaction(microcompaction);
    (harness, path)
}

fn open_task_with_ledger(harness: &Harness<SilentProvider>, body: &str, rel: &str) {
    let path = harness.workspace().join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    harness.git_safety.set_turn_context(Some(body.to_string()));
    harness.git_safety.note_mutation();
    let targets = [path.clone()];
    harness.git_safety.before_exec(&targets);
    let content = b"iris-owned change\n";
    std::fs::write(&path, content).unwrap();
    let hash = crate::tools::content_hash(content);
    let violations = harness.git_safety.after_exec(&targets, Some(&hash));
    assert!(violations.is_empty(), "iris write flagged: {violations:?}");
}

fn ok_read(id: &str, target: &str, content: &str) -> Message {
    Message::tool_result(
        id,
        "read",
        &json!({
            "ok": true,
            "content": content,
            "metadata": { "target": target }
        })
        .to_string(),
    )
}

fn compaction_entries(path: &Path) -> Vec<Value> {
    entries_of_type(path, "compaction")
}

fn fold_entries(path: &Path) -> Vec<Value> {
    entries_of_type(path, "fold")
}

fn entries_of_type(path: &Path, kind: &str) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["type"] == kind)
        .collect()
}

#[test]
fn manual_compaction_with_open_task_carries_task_state_into_context_and_log() {
    let root = temp_dir();
    let workspace = init_git_workspace();
    let big = format!(
        "old context that will be compacted :: {}",
        "ledger reconciliation detail. ".repeat(300)
    );
    let messages = [
        Message::user(&big),
        Message::assistant("ok"),
        Message::user("small retained turn"),
        Message::assistant("ok2"),
    ];
    let (mut harness, path) = seed_harness(&root.path, &workspace.path, messages, None, false);
    open_task_with_ledger(&harness, "fix the parser", "src/parser.rs");

    block_on(harness.compact_now(&NullObserver, &CancellationToken::new())).unwrap();
    let live = harness
        .messages()
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(live.contains("[open task state]"), "{live}");
    assert!(live.contains("fix the parser"), "{live}");
    assert!(live.contains("src/parser.rs"), "{live}");

    let compaction = compaction_entries(&path)
        .pop()
        .expect("compaction entry was persisted");
    assert_eq!(compaction["taskState"]["taskBody"], "fix the parser");
    assert_eq!(
        compaction["taskState"]["ledgerPaths"],
        json!(["src/parser.rs"])
    );

    let store = SessionStore::with_root(root.path.clone());
    let meta = store
        .list()
        .unwrap()
        .into_iter()
        .find(|m| m.path == path)
        .unwrap();
    let rebuilt = store.open(&meta).unwrap();
    let resumed = rebuilt
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(resumed.contains("[open task state]"), "{resumed}");
    assert!(resumed.contains("fix the parser"), "{resumed}");
    assert!(resumed.contains("src/parser.rs"), "{resumed}");
}

#[test]
fn fold_flush_with_open_task_does_not_mutate_task_state_or_persist_task_state() {
    let root = temp_dir();
    let workspace = init_git_workspace();
    let bulk = "long-standing project discussion detail. ".repeat(200);
    let old_read = "old file contents. ".repeat(400);
    let new_read = "new file contents. ".repeat(800);
    let messages = [
        Message::user(&bulk),
        Message::assistant(&bulk),
        Message::user("read a.rs"),
        ok_read("c1", "a.rs", &old_read),
        Message::assistant("ok"),
        Message::user("read a.rs again"),
        ok_read("c2", "a.rs", &new_read),
        Message::assistant("done"),
    ];
    let (mut harness, path) = seed_harness(&root.path, &workspace.path, messages, Some(1), true);
    open_task_with_ledger(&harness, "fix the parser", "src/parser.rs");
    let before = harness.compaction_task_state();

    let applied = harness.maybe_microcompact(&NullObserver).unwrap();
    assert_eq!(applied, 1, "one superseded read result should fold");
    assert_eq!(
        harness.compaction_task_state(),
        before,
        "folding tool results must not mutate open task state"
    );

    let folds = fold_entries(&path);
    assert_eq!(folds.len(), 1);
    assert!(
        !folds[0].as_object().unwrap().contains_key("taskState"),
        "fold entries do not own or persist task state"
    );
    assert!(
        compaction_entries(&path).is_empty(),
        "maybe_microcompact only flushes folds"
    );
}
