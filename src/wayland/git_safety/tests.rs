//! Dirty-tree safety tests (issue #262, ADR-0028).
//!
//! Guard-level unit tests drive [`GitSafety`] directly against scratch git
//! repos; loop-level integration tests drive a [`Harness`] with a fake provider
//! to prove the Nexus choke-point routes a dirty file through approval even when
//! a project grant would otherwise auto-run the call.

use std::cell::{Cell, RefCell};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::GitSafety;
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate,
    AssistantTurn, ChatProvider, Message, MutationGuard, ProviderEvent, ProviderStream, ToolCall,
    Tools,
};
use crate::tools::ToolState;
use crate::wayland::Harness;

// --- scratch repo + temp dir helpers ------------------------------------

struct TempDir {
    path: PathBuf,
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn temp_dir() -> TempDir {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("iris-git-safety-{nanos}-{seq}"));
    fs::create_dir(&path).unwrap();
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

/// A git repo with one committed file (`committed.txt`) so `HEAD` exists.
fn init_repo() -> TempDir {
    let dir = temp_dir();
    run_git(&dir.path, &["init", "-q", "-b", "main"]);
    run_git(&dir.path, &["config", "user.email", "test@example.com"]);
    run_git(&dir.path, &["config", "user.name", "Test"]);
    fs::write(dir.path.join("committed.txt"), "base\n").unwrap();
    run_git(&dir.path, &["add", "committed.txt"]);
    run_git(&dir.path, &["commit", "-q", "-m", "init"]);
    dir
}

fn guard(dir: &Path) -> GitSafety {
    GitSafety::new(dir)
}

// --- guard-level unit tests ---------------------------------------------

// Test 6: clean-tree fast path -- lazy (no baseline until first mutation) and
// no protected paths, so nothing is gated.
#[test]
fn clean_tree_is_lazy_and_ungated() {
    let repo = init_repo();
    let guard = guard(&repo.path);

    // Lazy: no task baseline exists until a mutation notes it.
    assert!(
        !guard.has_task(),
        "baseline must not be captured before a mutation"
    );

    // First mutation: clean tree, so no summary is surfaced and no path is
    // protected.
    assert_eq!(guard.note_mutation(), None);
    assert!(guard.has_task(), "first mutation captures the baseline");
    assert!(
        guard
            .unapproved_protected(&[repo.path.join("committed.txt")])
            .is_empty()
    );
}

// Test 1 (guard level) + Test 8: a dirty tree is captured with the index, and a
// dirty file is reported as protected and unapproved.
#[test]
fn dirty_file_is_protected_and_index_captured() {
    let repo = init_repo();
    // Make committed.txt dirty, and stage a new file so the index differs.
    fs::write(repo.path.join("committed.txt"), "dirty\n").unwrap();
    fs::write(repo.path.join("staged.txt"), "staged\n").unwrap();
    run_git(&repo.path, &["add", "staged.txt"]);

    let guard = guard(&repo.path);
    let summary = guard
        .note_mutation()
        .expect("dirty tree surfaces a summary");
    assert!(summary.contains("dirty"), "summary: {summary}");

    // Test 8: the baseline captured the index (staged.txt shows in ls-files).
    let index = guard.baseline_index().expect("baseline present");
    assert!(index.contains("staged.txt"), "index: {index}");

    // committed.txt (dirty) and staged.txt (staged) are both protected.
    let protected = guard.unapproved_protected(&[
        repo.path.join("committed.txt"),
        repo.path.join("staged.txt"),
    ]);
    assert_eq!(protected.len(), 2, "both dirty/staged files are protected");
}

// Test 2: an approved file is not re-prompted until settlement; a new task
// (after settle) re-prompts.
#[test]
fn approval_is_per_task_and_expires_on_settlement() {
    let repo = init_repo();
    fs::write(repo.path.join("committed.txt"), "dirty\n").unwrap();
    let target = repo.path.join("committed.txt");

    let guard = guard(&repo.path);
    guard.note_mutation();
    assert!(
        !guard
            .unapproved_protected(std::slice::from_ref(&target))
            .is_empty()
    );

    // Approve just this file: no longer flagged.
    guard.approve(std::slice::from_ref(&target), false);
    assert!(
        guard
            .unapproved_protected(std::slice::from_ref(&target))
            .is_empty()
    );

    // Settlement clears approvals; the next task re-captures and re-prompts.
    guard.settle();
    guard.note_mutation();
    assert!(
        !guard.unapproved_protected(&[target]).is_empty(),
        "a new task must re-prompt for the still-dirty file"
    );
}

// Test 9: the "all dirty files this task" escalation covers subsequent files.
#[test]
fn escalation_covers_all_dirty_files() {
    let repo = init_repo();
    fs::write(repo.path.join("committed.txt"), "dirty\n").unwrap();
    fs::write(repo.path.join("second.txt"), "two\n").unwrap();
    run_git(&repo.path, &["add", "second.txt"]);

    let guard = guard(&repo.path);
    guard.note_mutation();
    // Escalate on the first file.
    guard.approve(&[repo.path.join("committed.txt")], true);
    // A different dirty file is now already covered.
    assert!(
        guard
            .unapproved_protected(&[repo.path.join("second.txt")])
            .is_empty(),
        "escalation must cover subsequent dirty files"
    );
}

// Test 4: an untracked user file is protected (flagged), so it is never touched
// without approval.
#[test]
fn untracked_file_is_protected() {
    let repo = init_repo();
    fs::write(repo.path.join("scratch.txt"), "user\n").unwrap();

    let guard = guard(&repo.path);
    let summary = guard.note_mutation().expect("untracked surfaces a summary");
    assert!(summary.contains("untracked"), "summary: {summary}");
    assert!(
        !guard
            .unapproved_protected(&[repo.path.join("scratch.txt")])
            .is_empty()
    );
}

// Test 3 + Test 5 + Test 10: a bash-like out-of-band overwrite of a protected
// file is detected as a violation (attributed to the user, TOCTOU rule) and
// restore recovers the exact bytes.
#[test]
fn bash_violation_detected_and_restored() {
    let repo = init_repo();
    let dirty = repo.path.join("committed.txt");
    fs::write(&dirty, "user work\n").unwrap();
    let untracked = repo.path.join("scratch.txt");
    fs::write(&untracked, "untracked user work\n").unwrap();

    let guard = guard(&repo.path);
    guard.note_mutation();

    // A mutating tool with no approved target (bash): snapshot, then simulate
    // the command overwriting both a dirty and an untracked protected file.
    guard.before_exec();
    fs::write(&dirty, "clobbered\n").unwrap();
    fs::write(&untracked, "clobbered\n").unwrap();

    let mut violations = guard.after_exec(&[]);
    violations.sort();
    assert_eq!(violations.len(), 2, "both protected files flagged");

    // Restore recovers exact original bytes (Test 3/5).
    guard.restore(&violations).unwrap();
    assert_eq!(fs::read_to_string(&dirty).unwrap(), "user work\n");
    assert_eq!(
        fs::read_to_string(&untracked).unwrap(),
        "untracked user work\n"
    );
}

// Test 7: a non-git cwd degrades -- a one-line notice, tools work, no gating.
#[test]
fn non_git_directory_degrades_without_gating() {
    let dir = temp_dir();
    fs::write(dir.path.join("file.txt"), "content\n").unwrap();

    let guard = guard(&dir.path);
    let notice = guard.note_mutation().expect("degrade surfaces a notice");
    assert!(
        notice.contains("degraded") || notice.contains("not a git"),
        "notice: {notice}"
    );
    // No gating: nothing is ever protected.
    assert!(
        guard
            .unapproved_protected(&[dir.path.join("file.txt")])
            .is_empty()
    );
    // The detection path is a no-op in degraded mode.
    guard.before_exec();
    fs::write(dir.path.join("file.txt"), "changed\n").unwrap();
    assert!(guard.after_exec(&[]).is_empty());
}

// A `.jj/` workspace degrades like non-git (ADR-0028 interop note).
#[test]
fn jj_workspace_degrades() {
    let repo = init_repo();
    fs::create_dir(repo.path.join(".jj")).unwrap();
    fs::write(repo.path.join("committed.txt"), "dirty\n").unwrap();

    let guard = guard(&repo.path);
    let notice = guard.note_mutation().expect("jj degrade surfaces a notice");
    assert!(notice.contains("jj"), "notice: {notice}");
    assert!(
        guard
            .unapproved_protected(&[repo.path.join("committed.txt")])
            .is_empty()
    );
}

// An approved edit target that changes is recorded to the ledger, not flagged.
#[test]
fn approved_change_is_ledgered_not_flagged() {
    let repo = init_repo();
    let target = repo.path.join("committed.txt");
    fs::write(&target, "dirty\n").unwrap();

    let guard = guard(&repo.path);
    guard.note_mutation();
    guard.approve(std::slice::from_ref(&target), false);

    guard.before_exec();
    fs::write(&target, "iris edit\n").unwrap();
    let violations = guard.after_exec(std::slice::from_ref(&target));
    assert!(
        violations.is_empty(),
        "an approved change is not a violation"
    );
    assert_eq!(guard.ledger_len(), 1, "the approved change is ledgered");
}

// --- loop-level integration (fake provider + Harness) -------------------

struct FakeProvider {
    responses: RefCell<Vec<AssistantTurn>>,
}

impl FakeProvider {
    fn new(mut responses: Vec<AssistantTurn>) -> Self {
        responses.reverse();
        Self {
            responses: RefCell::new(responses),
        }
    }
}

impl ChatProvider for FakeProvider {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        let turn = self
            .responses
            .borrow_mut()
            .pop()
            .unwrap_or_else(|| AssistantTurn::text("done"));
        Ok(Box::pin(futures::stream::once(async move {
            Ok(ProviderEvent::Completed(turn))
        })))
    }
}

/// Records events and counts approval reviews, answering with a canned decision.
struct CountingFrontend {
    decision: Cell<ApprovalDecision>,
    reviews: Cell<usize>,
    events: RefCell<Vec<AgentEvent>>,
}

impl CountingFrontend {
    fn new(decision: ApprovalDecision) -> Self {
        Self {
            decision: Cell::new(decision),
            reviews: Cell::new(0),
            events: RefCell::new(Vec::new()),
        }
    }
}

impl AgentObserver for CountingFrontend {
    fn on_event(&self, event: AgentEvent) -> Result<()> {
        self.events.borrow_mut().push(event);
        Ok(())
    }
}

impl ApprovalGate for CountingFrontend {
    fn review<'a>(
        &'a self,
        _call: &'a ToolCall,
        _allow_always: bool,
        _allow_project: bool,
    ) -> ApprovalFuture<'a> {
        self.reviews.set(self.reviews.get() + 1);
        let decision = self.decision.get();
        Box::pin(async move { Ok(decision) })
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn write_call(id: &str, path: &str, content: &str) -> AssistantTurn {
    AssistantTurn {
        tool_calls: vec![ToolCall {
            id: id.to_string(),
            name: "write".to_string(),
            arguments: json!({ "path": path, "content": content }),
            thought_signature: None,
        }],
        ..Default::default()
    }
}

/// Grant `write` at the project layer so a non-dirty write auto-approves.
fn grant_write<P: ChatProvider>(harness: &mut Harness<P>) {
    let mut policy = crate::nexus::ProjectPolicy::default();
    policy.tools.insert("write".to_string());
    harness.agent.set_project_policy(policy);
}

// Test 1 (loop level): a write to a baseline-dirty file is routed through
// approval even though a project grant would otherwise auto-run write.
#[test]
fn dirty_write_prompts_despite_project_grant() -> Result<()> {
    let repo = init_repo();
    fs::write(repo.path.join("committed.txt"), "dirty\n").unwrap();

    let provider = FakeProvider::new(vec![
        write_call("c1", "committed.txt", "iris\n"),
        AssistantTurn::text("done"),
    ]);
    let mut harness = Harness::new(
        Agent::new(provider, crate::tools::built_in_tools()),
        repo.path.clone(),
        ToolState::new(),
        None,
        None,
    );
    grant_write(&mut harness);
    let frontend = CountingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    assert_eq!(
        frontend.reviews.get(),
        1,
        "a dirty-file write must prompt despite the project grant"
    );
    Ok(())
}

// Control for Test 1: a write to a clean (untracked, not-in-baseline) path with
// the same project grant auto-approves without a prompt.
#[test]
fn clean_write_auto_approves_with_project_grant() -> Result<()> {
    let repo = init_repo();

    let provider = FakeProvider::new(vec![
        write_call("c1", "brand_new.txt", "hello\n"),
        AssistantTurn::text("done"),
    ]);
    let mut harness = Harness::new(
        Agent::new(provider, crate::tools::built_in_tools()),
        repo.path.clone(),
        ToolState::new(),
        None,
        None,
    );
    grant_write(&mut harness);
    let frontend = CountingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    assert_eq!(
        frontend.reviews.get(),
        0,
        "a non-dirty write is auto-approved by the project grant"
    );
    Ok(())
}

// Test 9 (loop level): the AllowAlways escalation on the first dirty file covers
// a second dirty file with no further prompt.
#[test]
fn escalation_in_loop_covers_second_dirty_file() -> Result<()> {
    let repo = init_repo();
    fs::write(repo.path.join("committed.txt"), "dirty one\n").unwrap();
    fs::write(repo.path.join("second.txt"), "dirty two\n").unwrap();
    run_git(&repo.path, &["add", "second.txt"]);

    let provider = FakeProvider::new(vec![
        write_call("c1", "committed.txt", "iris one\n"),
        write_call("c2", "second.txt", "iris two\n"),
        AssistantTurn::text("done"),
    ]);
    let mut harness = Harness::new(
        Agent::new(provider, crate::tools::built_in_tools()),
        repo.path.clone(),
        ToolState::new(),
        None,
        None,
    );
    // Grant write at the project layer so, once the dirty gate is escalated
    // away, the base write approval no longer prompts either -- isolating the
    // dirty-gate escalation as the only thing that could re-prompt.
    grant_write(&mut harness);
    // AllowAlways in the dirty context escalates to "all dirty files this task".
    let frontend = CountingFrontend::new(ApprovalDecision::AllowAlways);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    assert_eq!(
        frontend.reviews.get(),
        1,
        "escalation on the first dirty file must cover the second"
    );
    Ok(())
}
