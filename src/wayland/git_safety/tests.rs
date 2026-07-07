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

use super::{EXTERNAL_SETTLEMENT_NOTICE, GitSafety, RecoveryOutcome, task_state};
use crate::nexus::{
    Agent, AgentEvent, AgentObserver, ApprovalDecision, ApprovalFuture, ApprovalGate,
    AssistantTurn, ChatProvider, Message, MutationGuard, ProviderEvent, ProviderStream,
    ReviewContext, Tool, ToolCall, ToolEnv, ToolFuture, ToolOutput, Tools,
};
use crate::tools::ToolState;
use crate::wayland::{Harness, ReanchorWorkspaceError};

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

fn git_stdout(dir: &Path, args: &[&str]) -> String {
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
    String::from_utf8_lossy(&output.stdout).into_owned()
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
    assert!(
        summary.contains("committed.txt") && summary.contains("staged.txt"),
        "summary names protected files: {summary}"
    );

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
    let display = guard
        .active_task_display()
        .expect("active workflow task display");
    assert_eq!(display.approved_paths, vec!["committed.txt".to_string()]);
    assert!(!display.all_dirty_approved);
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

// Test 9: the "all dirty files (this task)" escalation covers subsequent files.
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
    let display = guard
        .active_task_display()
        .expect("active workflow task display");
    assert!(display.all_dirty_approved);
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
        summary.contains("scratch.txt"),
        "summary names protected files: {summary}"
    );
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
    guard.before_exec(&[]);
    fs::write(&dirty, "clobbered\n").unwrap();
    fs::write(&untracked, "clobbered\n").unwrap();

    let mut violations = guard.after_exec(&[], None);
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

#[test]
fn approved_dirty_bash_change_is_ledgered_and_rollbackable() {
    let repo = init_repo();
    let dirty = repo.path.join("committed.txt");
    fs::write(&dirty, "user work\n").unwrap();

    let guard = guard(&repo.path);
    guard.note_mutation();
    guard.approve(std::slice::from_ref(&dirty), false);

    guard.before_exec(&[]);
    fs::write(&dirty, "formatted by bash\n").unwrap();

    let violations = guard.after_exec(&[], None);
    assert!(
        violations.is_empty(),
        "an approved dirty path changed by bash is Iris-attributed"
    );
    assert_eq!(guard.ledger_len(), 1, "approved bash change is ledgered");

    guard.rollback(0).unwrap();
    assert_eq!(
        fs::read_to_string(&dirty).unwrap(),
        "user work\n",
        "rollback restores the pre-bash dirty bytes"
    );
}

#[test]
fn approve_all_dirty_files_covers_bash_attribution() {
    let repo = init_repo();
    let one = repo.path.join("committed.txt");
    let two = repo.path.join("second.txt");
    fs::write(&one, "user one\n").unwrap();
    fs::write(&two, "user two\n").unwrap();
    run_git(&repo.path, &["add", "second.txt"]);

    let guard = guard(&repo.path);
    guard.note_mutation();
    guard.approve(&[], true);

    guard.before_exec(&[]);
    fs::write(&one, "formatted one\n").unwrap();
    fs::write(&two, "formatted two\n").unwrap();

    let violations = guard.after_exec(&[], None);
    assert!(
        violations.is_empty(),
        "approve-all covers bash changes to all baseline dirty paths"
    );
    assert_eq!(guard.ledger_len(), 2);
}

#[test]
fn unapproved_dirty_bash_path_still_violates_and_restores() {
    let repo = init_repo();
    let approved = repo.path.join("committed.txt");
    let unapproved = repo.path.join("second.txt");
    fs::write(&approved, "approved user work\n").unwrap();
    fs::write(&unapproved, "unapproved user work\n").unwrap();
    run_git(&repo.path, &["add", "second.txt"]);

    let guard = guard(&repo.path);
    guard.note_mutation();
    guard.approve(std::slice::from_ref(&approved), false);

    guard.before_exec(&[]);
    fs::write(&approved, "approved bash change\n").unwrap();
    fs::write(&unapproved, "unapproved bash change\n").unwrap();

    let violations = guard.after_exec(&[], None);
    assert_eq!(violations, vec![unapproved.clone()]);
    assert_eq!(
        guard.ledger_len(),
        1,
        "only the approved bash change is ledgered"
    );

    guard.restore(&violations).unwrap();
    assert_eq!(
        fs::read_to_string(&approved).unwrap(),
        "approved bash change\n"
    );
    assert_eq!(
        fs::read_to_string(&unapproved).unwrap(),
        "unapproved user work\n"
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
    guard.before_exec(&[]);
    fs::write(dir.path.join("file.txt"), "changed\n").unwrap();
    assert!(guard.after_exec(&[], None).is_empty());
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

// An approved edit target whose post-call bytes match what the tool wrote is
// recorded to the ledger as Iris, not flagged.
#[test]
fn approved_change_is_ledgered_not_flagged() {
    let repo = init_repo();
    let target = repo.path.join("committed.txt");
    fs::write(&target, "dirty\n").unwrap();

    let guard = guard(&repo.path);
    guard.note_mutation();
    guard.approve(std::slice::from_ref(&target), false);

    guard.before_exec(&[]);
    fs::write(&target, "iris edit\n").unwrap();
    // Confirmed: the on-disk bytes equal what the tool reported writing.
    let violations = guard.after_exec(
        std::slice::from_ref(&target),
        Some(&crate::tools::content_hash(b"iris edit\n")),
    );
    assert!(
        violations.is_empty(),
        "a confirmed approved change is not a violation"
    );
    assert_eq!(guard.ledger_len(), 1, "the confirmed change is ledgered");
}

#[test]
fn workflow_off_keeps_guard_in_memory_without_git_task_state() {
    let repo = init_repo();
    let target = repo.path.join("new.txt");
    let git_dir = task_state::git_dir(&repo.path).expect("git dir");

    let guard = GitSafety::new_with_workflow(&repo.path, false);
    assert_eq!(guard.note_mutation(), None);
    guard.before_exec(std::slice::from_ref(&target));
    fs::write(&target, "iris\n").unwrap();
    let violations = guard.after_exec(
        std::slice::from_ref(&target),
        Some(&crate::tools::content_hash(b"iris\n")),
    );

    assert!(violations.is_empty());
    assert!(
        guard.has_ledger_entries(),
        "the guard still tracks Iris changes"
    );
    assert_eq!(guard.current_task_id(), None, "no user-facing task id");
    assert!(
        guard.active_task_display().is_none(),
        "no task badge payload"
    );
    assert!(guard.restore_points().is_empty(), "rollback UI is disabled");
    assert!(
        guard.task_diff(None).unwrap().is_empty(),
        "diff UI is disabled"
    );
    assert!(guard.recoverable_tasks().is_empty(), "no recovery rows");
    assert!(matches!(guard.recover_and_expire(), RecoveryOutcome::None));
    assert!(
        !git_dir.join("iris").exists(),
        "workflow-off guard must not create .git/iris records"
    );
    assert!(
        git_stdout(&repo.path, &["for-each-ref", "refs/iris"]).is_empty(),
        "workflow-off guard must not create refs/iris checkpoint refs"
    );
}

#[test]
fn workflow_off_still_detects_and_restores_dirty_violations() {
    let repo = init_repo();
    let dirty = repo.path.join("committed.txt");
    fs::write(&dirty, "user work\n").unwrap();

    let guard = GitSafety::new_with_workflow(&repo.path, false);
    let summary = guard.note_mutation().expect("dirty baseline notice");
    assert!(summary.contains("dirty"), "summary: {summary}");
    assert!(
        !guard
            .unapproved_protected(std::slice::from_ref(&dirty))
            .is_empty(),
        "dirty-file gate stays enabled"
    );

    guard.before_exec(&[]);
    fs::write(&dirty, "clobbered\n").unwrap();
    let violations = guard.after_exec(&[], None);
    assert_eq!(violations, vec![dirty.clone()]);
    guard.restore(&violations).unwrap();
    assert_eq!(fs::read_to_string(&dirty).unwrap(), "user work\n");
}

// TOCTOU (finding 2, ADR-0028): an approved target whose post-call bytes do NOT
// match what the tool wrote -- a concurrent user edit, or a failed/partial
// write -- is ambiguous, so it stays user-attributed and protected (a
// violation), never silently ledgered as Iris.
#[test]
fn approved_target_unconfirmed_change_stays_protected() {
    let repo = init_repo();
    let target = repo.path.join("committed.txt");
    fs::write(&target, "dirty\n").unwrap();

    let guard = guard(&repo.path);
    guard.note_mutation();
    guard.approve(std::slice::from_ref(&target), false);

    // The tool was approved and "intended" to write "iris edit\n", but a
    // concurrent external change lands different bytes on disk before the check.
    guard.before_exec(&[]);
    fs::write(&target, "user raced\n").unwrap();
    let violations = guard.after_exec(
        std::slice::from_ref(&target),
        Some(&crate::tools::content_hash(b"iris edit\n")),
    );

    assert_eq!(
        violations.len(),
        1,
        "an unconfirmed approved change is a violation, not an Iris mutation"
    );
    assert_eq!(
        guard.ledger_len(),
        0,
        "an ambiguous change is never attributed to Iris"
    );
    // Restore recovers the exact raced bytes' predecessor from the pre-call
    // snapshot, so the user's work is protected.
    guard.restore(&violations).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap(), "dirty\n");
}

// A failed/partial approved write (tool reports no confirmation hash) is
// likewise ambiguous and protected, not attributed to Iris.
#[test]
fn approved_change_without_confirmation_stays_protected() {
    let repo = init_repo();
    let target = repo.path.join("committed.txt");
    fs::write(&target, "dirty\n").unwrap();

    let guard = guard(&repo.path);
    guard.note_mutation();
    guard.approve(std::slice::from_ref(&target), false);

    guard.before_exec(&[]);
    fs::write(&target, "partial\n").unwrap();
    // `None` expected-after models a failed/cancelled or non-reporting tool.
    let violations = guard.after_exec(std::slice::from_ref(&target), None);
    assert_eq!(violations.len(), 1, "an unconfirmed change is a violation");
    assert_eq!(guard.ledger_len(), 0, "not attributed to Iris");
}

// Non-UTF-8 filenames (finding 3, Unix): a dirty file whose name is not valid
// UTF-8 is parsed from the raw `git status -z` byte stream, so it is protected
// and restorable under its exact on-disk name.
#[cfg(unix)]
#[test]
fn non_utf8_filename_is_protected_and_restorable() {
    use std::os::unix::ffi::OsStrExt;

    let repo = init_repo();
    let name = std::ffi::OsStr::from_bytes(b"bad-\xff-name.txt");
    let path = repo.path.join(name);
    fs::write(&path, "user bytes\n").unwrap();

    let guard = guard(&repo.path);
    let summary = guard
        .note_mutation()
        .expect("an untracked non-UTF-8 file surfaces a summary");
    assert!(summary.contains("untracked"), "summary: {summary}");

    assert_eq!(
        guard
            .unapproved_protected(std::slice::from_ref(&path))
            .len(),
        1,
        "a non-UTF-8 filename must be protected (not lossy-corrupted)"
    );

    // A bash-like out-of-band overwrite is detected and restored to exact bytes.
    guard.before_exec(&[]);
    fs::write(&path, "clobbered\n").unwrap();
    let violations = guard.after_exec(&[], None);
    assert_eq!(violations.len(), 1, "the out-of-band change is flagged");
    guard.restore(&violations).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"user bytes\n");
}

// Finding 4 (ADR-0028): a session swap is passive -- it drops per-file approvals
// (safe: re-prompt) but must NOT settle the task or lose the baseline's
// protection.
#[test]
fn session_swap_drops_approvals_without_settling() {
    let repo = init_repo();
    let target = repo.path.join("committed.txt");
    fs::write(&target, "dirty\n").unwrap();

    let provider = FakeProvider::new(vec![AssistantTurn::text("done")]);
    let mut harness = Harness::new(
        Agent::new(provider, crate::tools::built_in_tools()),
        repo.path.clone(),
        ToolState::new(),
        None,
        None,
    );
    // Open a task and approve the dirty file (as an in-flight edit would).
    harness.git_safety.note_mutation();
    harness
        .git_safety
        .approve(std::slice::from_ref(&target), false);
    assert!(
        harness
            .git_safety
            .unapproved_protected(std::slice::from_ref(&target))
            .is_empty(),
        "the file is approved before the swap"
    );

    // A passive session swap (`/new`).
    harness.swap_session(None, Vec::new(), Vec::new(), 0);

    assert!(
        harness.git_safety.has_task(),
        "a passive swap must NOT settle the task (baseline protection persists)"
    );
    assert!(
        !harness
            .git_safety
            .unapproved_protected(std::slice::from_ref(&target))
            .is_empty(),
        "the swap drops per-file approvals so the next touch re-prompts"
    );
}

#[test]
fn reanchor_with_active_task_requires_decision_and_carry_is_explicit() -> Result<()> {
    let repo = init_repo();
    let old_git_dir = task_state::git_dir(&repo.path).unwrap();
    let other = init_repo();
    let original_workspace = repo.path.canonicalize().unwrap();
    let other_workspace = other.path.canonicalize().unwrap();

    let provider = FakeProvider::new(vec![
        write_call("c1", "new.txt", "hi\n"),
        AssistantTurn::text("created it"),
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

    block_on(harness.submit_turn(
        "create the new file",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))?;
    assert!(
        harness.reanchor_requires_task_decision(),
        "the active durable task must force an explicit reanchor decision"
    );

    assert_eq!(
        harness.reanchor_workspace(&other.path),
        Err(ReanchorWorkspaceError::ActiveTask)
    );
    assert_eq!(
        harness.workspace(),
        original_workspace.as_path(),
        "declining/blocked reanchor leaves the guard anchored to the old workspace"
    );
    assert!(
        harness.current_task_id().is_some(),
        "the active task remains live after the blocked reanchor"
    );

    harness.reanchor_workspace_carrying_task(&other.path);
    assert_eq!(harness.workspace(), other_workspace.as_path());
    assert!(
        harness.current_task_id().is_none(),
        "explicit carry starts a fresh guard in the new worktree"
    );
    assert_eq!(
        task_state::load_all(&old_git_dir).len(),
        1,
        "explicit carry knowingly leaves the old worktree task recoverable"
    );
    Ok(())
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

struct FailAfterToolProvider {
    calls: Cell<usize>,
}

impl FailAfterToolProvider {
    fn new() -> Self {
        Self {
            calls: Cell::new(0),
        }
    }
}

impl ChatProvider for FailAfterToolProvider {
    fn respond_stream<'a>(
        &'a self,
        _messages: &'a [Message],
        _tools: &'a Tools,
        _cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        let calls = self.calls.get();
        self.calls.set(calls + 1);
        if calls == 0 {
            let turn = write_call("c1", "new.txt", "hi\n");
            return Ok(Box::pin(futures::stream::once(async move {
                Ok(ProviderEvent::Completed(turn))
            })));
        }
        Err(anyhow::anyhow!("provider failed after mutation"))
    }
}

/// Records events and counts approval reviews, answering with a canned decision.
struct CountingFrontend {
    decision: Cell<ApprovalDecision>,
    reviews: Cell<usize>,
    events: RefCell<Vec<AgentEvent>>,
    /// The review facts the last gated call carried, so a dirty-tree test can
    /// assert Nexus threads the workspace-relative `dirty_paths` to the gate.
    last_ctx: RefCell<Option<ReviewContext>>,
}

impl CountingFrontend {
    fn new(decision: ApprovalDecision) -> Self {
        Self {
            decision: Cell::new(decision),
            reviews: Cell::new(0),
            events: RefCell::new(Vec::new()),
            last_ctx: RefCell::new(None),
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
        ctx: ReviewContext,
    ) -> ApprovalFuture<'a> {
        self.reviews.set(self.reviews.get() + 1);
        *self.last_ctx.borrow_mut() = Some(ctx);
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
    // The gate receives the dirty-tree facts (workspace-relative paths), not UI
    // copy: Nexus threads them through `ReviewContext` (issue #262/ADR-0028).
    let ctx = frontend
        .last_ctx
        .borrow()
        .clone()
        .expect("the gate received a review context");
    assert_eq!(
        ctx.dirty_paths,
        vec!["committed.txt".to_string()],
        "the dirty path is threaded to the gate"
    );
    assert!(
        !ctx.destructive,
        "a plain dirty-file write is not the destructive floor"
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

#[test]
fn print_turn_accepts_workflow_task_on_success() -> Result<()> {
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let session_root = temp_dir();
    let session = crate::session::SessionLog::create_in(&session_root.path, &repo.path).unwrap();
    let session_path = session.path().to_path_buf();

    let provider = FakeProvider::new(vec![
        write_call("c1", "new.txt", "hi\n"),
        AssistantTurn::text("done"),
    ]);
    let mut harness = Harness::new(
        Agent::new(provider, crate::tools::built_in_tools()),
        repo.path.clone(),
        ToolState::new(),
        Some(session),
        None,
    );
    grant_write(&mut harness);
    let frontend = CountingFrontend::new(ApprovalDecision::Allow);

    crate::cli::run_print_turn(&mut harness, "create new file", &frontend, &frontend)?;
    drop(harness);

    assert!(
        task_state::load_all(&git_dir).is_empty(),
        "successful print mode accepts and removes the durable task record"
    );
    assert!(
        git_stdout(&repo.path, &["for-each-ref", "refs/iris"]).is_empty(),
        "successful print settlement removes checkpoint refs"
    );

    let entries: Vec<serde_json::Value> = fs::read_to_string(&session_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let settled: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e["type"] == "taskLifecycle" && e["event"] == "settled")
        .collect();
    assert_eq!(settled.len(), 1, "print success records one settlement");
    assert_eq!(settled[0]["disposition"], "print");
    Ok(())
}

#[test]
fn print_turn_failure_leaves_workflow_task_record() -> Result<()> {
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let provider = FailAfterToolProvider::new();
    let mut harness = Harness::new(
        Agent::new(provider, crate::tools::built_in_tools()),
        repo.path.clone(),
        ToolState::new(),
        None,
        None,
    );
    grant_write(&mut harness);
    let frontend = CountingFrontend::new(ApprovalDecision::Allow);

    let error = crate::cli::run_print_turn(&mut harness, "create new file", &frontend, &frontend)
        .expect_err("provider failure after a write should fail print mode");

    assert!(
        format!("{error:#}").contains("provider failed after mutation"),
        "error: {error:#}"
    );
    assert_eq!(
        task_state::load_all(&git_dir).len(),
        1,
        "failed print mode keeps the durable task for recovery"
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
    // AllowAlways in the dirty context escalates to "all dirty files (this task)".
    let frontend = CountingFrontend::new(ApprovalDecision::AllowAlways);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    assert_eq!(
        frontend.reviews.get(),
        1,
        "escalation on the first dirty file must cover the second"
    );
    Ok(())
}

/// A mutating tool that does NOT require approval (unlike the built-in
/// edit/write) yet targets a statically-known path. Exercises finding 1: a
/// mutating-but-ungated tool must still be routed through the dirty gate, not
/// skipped because `requires_approval()` is false.
struct UngatedMutator;

impl Tool for UngatedMutator {
    fn name(&self) -> &str {
        "ungated_mutate"
    }
    fn description(&self) -> &str {
        "test-only mutating tool with no approval gate"
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }
    fn execute<'a>(
        &'a self,
        args: &'a serde_json::Value,
        env: &'a ToolEnv<'_>,
        _cancel: CancellationToken,
    ) -> ToolFuture<'a> {
        let target = env
            .workspace
            .join(args["path"].as_str().unwrap_or_default());
        let content = args["content"].as_str().unwrap_or_default().to_string();
        Box::pin(async move {
            fs::write(&target, &content)?;
            // Report the written bytes so a confirmed approved write is not
            // spuriously flagged; keeps this test focused on the gate firing.
            Ok(ToolOutput::text("wrote").with(
                crate::nexus::WRITE_CONFIRM_HASH_KEY,
                json!(crate::tools::content_hash(content.as_bytes())),
            ))
        })
    }
    fn is_mutating(&self) -> bool {
        true
    }
    fn mutates_paths(&self, args: &serde_json::Value) -> Vec<PathBuf> {
        match args["path"].as_str() {
            Some(path) => vec![PathBuf::from(path)],
            None => Vec::new(),
        }
    }
    // requires_approval() defaults to false -- the crux of finding 1.
}

// Finding 1: a mutating tool with `is_mutating() == true` and a protected
// `mutates_paths()` target but `requires_approval() == false` must still route
// the dirty file through approval -- the dirty gate cannot be skipped.
#[test]
fn ungated_mutating_tool_still_gates_dirty_file() -> Result<()> {
    let repo = init_repo();
    fs::write(repo.path.join("committed.txt"), "dirty\n").unwrap();

    let provider = FakeProvider::new(vec![
        AssistantTurn {
            tool_calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "ungated_mutate".to_string(),
                arguments: json!({ "path": "committed.txt", "content": "iris\n" }),
                thought_signature: None,
            }],
            ..Default::default()
        },
        AssistantTurn::text("done"),
    ]);
    let mut harness = Harness::new(
        Agent::new(provider, Tools::new(vec![Box::new(UngatedMutator)])),
        repo.path.clone(),
        ToolState::new(),
        None,
        None,
    );
    let frontend = CountingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn("go", &frontend, &frontend, &CancellationToken::new()))?;

    assert_eq!(
        frontend.reviews.get(),
        1,
        "a mutating-but-ungated tool must still route a baseline-dirty target through approval"
    );
    Ok(())
}

// Issue #287 test (1 Q&A + 4), Harness end-to-end: a mutating turn appends a
// `TaskOpened` audit entry carrying the turn's prompt preview as body; settling
// appends a matching `TaskSettled`; a pure Q&A turn opens no task and appends
// nothing. Session-log read-back is proven to skip lifecycle entries in the
// session-module unit test; here the focus is the Harness wiring.
#[test]
fn harness_records_task_lifecycle_and_qanda_opens_nothing() -> Result<()> {
    let repo = init_repo();
    let session_root = temp_dir();
    let session = crate::session::SessionLog::create_in(&session_root.path, &repo.path).unwrap();
    let session_id = session.id().to_string();
    let session_path = session.path().to_path_buf();

    let provider = FakeProvider::new(vec![
        write_call("c1", "new.txt", "hi\n"),
        AssistantTurn::text("created it"),
        AssistantTurn::text("the answer is 42"),
    ]);
    let mut harness = Harness::new(
        Agent::new(provider, crate::tools::built_in_tools()),
        repo.path.clone(),
        ToolState::new(),
        Some(session),
        None,
    );
    grant_write(&mut harness);
    let frontend = CountingFrontend::new(ApprovalDecision::Allow);

    // A mutating turn opens a task; the harness records TaskOpened post-turn.
    block_on(harness.submit_turn(
        "create the new file",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))?;
    // Settling appends TaskSettled with a deterministic disposition.
    assert!(
        harness.accept_checkpoint().is_some(),
        "there was an unsettled task to accept"
    );
    // A pure Q&A turn (text-only) opens no task, so nothing is appended.
    block_on(harness.submit_turn(
        "what is the answer",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))?;
    drop(harness);

    let entries: Vec<serde_json::Value> = fs::read_to_string(&session_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let opened: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e["type"] == "taskLifecycle" && e["event"] == "opened")
        .collect();
    assert_eq!(
        opened.len(),
        1,
        "exactly one task opened this session (the Q&A turn opened none)"
    );
    assert_eq!(
        opened[0]["body"], "create the new file",
        "the opening turn's prompt preview is the recorded body"
    );
    let settled: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e["type"] == "taskLifecycle" && e["event"] == "settled")
        .collect();
    assert_eq!(settled.len(), 1, "exactly one settle recorded");
    assert_eq!(settled[0]["disposition"], "accepted");
    assert_eq!(
        opened[0]["taskId"], settled[0]["taskId"],
        "the settle names the same task that opened"
    );

    // The session still opens cleanly (lifecycle entries are skipped, not fatal).
    let store = crate::session::SessionStore::with_root(session_root.path.clone());
    let meta = store.find(&session_id).unwrap().unwrap();
    let stored = store.open(&meta).unwrap();
    assert!(
        stored
            .messages
            .iter()
            .any(|m| m.content == "create the new file"),
        "the user prompt is reconstructed"
    );
    Ok(())
}

#[test]
fn harness_records_external_settlement_when_user_commits_task() -> Result<()> {
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let session_root = temp_dir();
    let session = crate::session::SessionLog::create_in(&session_root.path, &repo.path).unwrap();
    let session_path = session.path().to_path_buf();

    let provider = FakeProvider::new(vec![
        write_call("c1", "new.txt", "hi\n"),
        AssistantTurn::text("created it"),
        AssistantTurn::text("ok"),
    ]);
    let mut harness = Harness::new(
        Agent::new(provider, crate::tools::built_in_tools()),
        repo.path.clone(),
        ToolState::new(),
        Some(session),
        None,
    );
    grant_write(&mut harness);
    let frontend = CountingFrontend::new(ApprovalDecision::Allow);

    block_on(harness.submit_turn(
        "create the new file",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))?;
    assert_eq!(
        task_state::load_all(&git_dir).len(),
        1,
        "the task is open before the user commit"
    );

    run_git(&repo.path, &["add", "new.txt"]);
    run_git(&repo.path, &["commit", "-q", "-m", "accept iris work"]);

    block_on(harness.submit_turn(
        "what happened?",
        &frontend,
        &frontend,
        &CancellationToken::new(),
    ))?;
    drop(harness);

    assert!(
        task_state::load_all(&git_dir).is_empty(),
        "the user commit closes the durable task"
    );
    assert!(
        frontend.events.borrow().iter().any(
            |event| matches!(event, AgentEvent::Notice(message) if message == EXTERNAL_SETTLEMENT_NOTICE)
        ),
        "the external-settlement notice is emitted"
    );

    let entries: Vec<serde_json::Value> = fs::read_to_string(&session_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let settled: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e["type"] == "taskLifecycle" && e["event"] == "settled")
        .collect();
    assert_eq!(settled.len(), 1, "exactly one settle recorded");
    assert_eq!(settled[0]["disposition"], "external");
    Ok(())
}
