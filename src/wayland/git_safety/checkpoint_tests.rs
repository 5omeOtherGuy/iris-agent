//! Task checkpoint/rollback tests (issue #263, ADR-0028).
//!
//! Engine tests drive [`CheckpointChain`] directly against scratch git repos to
//! prove git-tree restore semantics (create/edit/delete/rename/binary/mode), GC
//! scoping, and that `HEAD`/index/stash are never touched. Guard tests drive
//! [`GitSafety`] end-to-end for clean/dirty rollback, index restore, multiple
//! restore points, crash-recovery reconciliation, expiry, and the non-git
//! fallback.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::checkpoint::{CheckpointChain, Mode};
use super::settlement::TaskClass;
use super::{GitSafety, RecoveryOutcome, lock, task_state};

/// Extract the single-orphan auto-adopt notice from a [`RecoveryOutcome`],
/// panicking on any other variant. Keeps the recovery tests that assert on the
/// notice wording terse after the #288 enum change.
fn expect_notice(outcome: RecoveryOutcome) -> String {
    match outcome {
        RecoveryOutcome::Notice(notice) => notice,
        other => panic!("expected a recovery notice, got {other:?}"),
    }
}
use crate::nexus::MutationGuard;

// --- scratch repo helpers (mirrors tests.rs; kept local for cohesion) ----

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
    let path = std::env::temp_dir().join(format!("iris-ckpt-test-{nanos}-{seq}"));
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

fn git_out(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A git repo with one committed file so `HEAD` exists.
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

/// Count the task's checkpoint refs (base + intermediates).
fn task_ref_count(dir: &Path, task_id: &str) -> usize {
    let prefix = format!("refs/iris/checkpoints/{task_id}/");
    let out = git_out(dir, &["for-each-ref", "--format=%(refname)", &prefix]);
    out.lines().filter(|l| !l.is_empty()).count()
}

/// Count every Iris-owned ref.
fn iris_ref_count(dir: &Path) -> usize {
    let out = git_out(dir, &["for-each-ref", "--format=%(refname)", "refs/iris/"]);
    out.lines().filter(|l| !l.is_empty()).count()
}

/// Create one checkpoint-looking ref for `task_id` pointing at HEAD. The tests
/// use this to simulate repos polluted by the old accept/checkpoint leak.
fn write_checkpoint_ref(dir: &Path, task_id: &str) {
    let head = git_out(dir, &["rev-parse", "HEAD"]);
    run_git(
        dir,
        &[
            "update-ref",
            &format!("refs/iris/checkpoints/{task_id}/0000000000"),
            &head,
        ],
    );
}

/// Drive an Iris-authored write of `content` to `path` through the guard's
/// targeted (edit/write) path, asserting it is not flagged as a violation.
fn iris_write(guard: &GitSafety, path: &Path, content: &[u8]) {
    let targets = [path.to_path_buf()];
    guard.before_exec(&targets);
    fs::write(path, content).unwrap();
    let hash = crate::tools::content_hash(content);
    let violations = guard.after_exec(&targets, Some(&hash));
    assert!(violations.is_empty(), "iris write flagged: {violations:?}");
}

// --- engine tests (CheckpointChain over scratch repos) -------------------

// Test 4: a single chain round-trips create / edit / delete / rename / binary
// through git-tree restore semantics.
#[test]
fn engine_round_trips_create_edit_delete_rename_binary() {
    let repo = init_repo();
    let root = repo.path.clone();
    let edit = root.join("committed.txt");
    let created = root.join("created.txt");
    let deleted = root.join("to_delete.txt");
    let rename_src = root.join("old_name.txt");
    let rename_dst = root.join("new_name.txt");
    let binary = root.join("blob.bin");

    // Pre-task disk state.
    fs::write(&deleted, "will be deleted\n").unwrap();
    fs::write(&rename_src, "moved content\n").unwrap();
    let bin_before = vec![0u8, 159, 146, 150, 255, 0, 1];
    fs::write(&binary, &bin_before).unwrap();

    let mut chain = CheckpointChain::new(root.clone(), "engine1".to_string());
    // Capture pre-task images.
    chain
        .note_before(&edit, Some((b"base\n".to_vec(), Mode::Normal)))
        .unwrap();
    chain.note_before(&created, None).unwrap();
    chain
        .note_before(
            &deleted,
            Some((b"will be deleted\n".to_vec(), Mode::Normal)),
        )
        .unwrap();
    chain
        .note_before(
            &rename_src,
            Some((b"moved content\n".to_vec(), Mode::Normal)),
        )
        .unwrap();
    chain.note_before(&rename_dst, None).unwrap();
    chain
        .note_before(&binary, Some((bin_before.clone(), Mode::Normal)))
        .unwrap();

    // Apply the Iris changes on disk.
    fs::write(&edit, "edited\n").unwrap();
    fs::write(&created, "brand new\n").unwrap();
    fs::remove_file(&deleted).unwrap();
    fs::remove_file(&rename_src).unwrap();
    fs::write(&rename_dst, "moved content\n").unwrap();
    let bin_after = vec![9u8, 8, 7, 255, 254, 0];
    fs::write(&binary, &bin_after).unwrap();

    chain.checkpoint(1, None, "changes".to_string()).unwrap();

    // Roll back to the pre-task base: every ledger path returns to pre-task.
    chain
        .rollback_to_excluding(0, &std::collections::BTreeSet::new())
        .unwrap();
    assert_eq!(fs::read(&edit).unwrap(), b"base\n");
    assert!(
        !created.exists(),
        "a created file is removed on base rollback"
    );
    assert_eq!(fs::read(&deleted).unwrap(), b"will be deleted\n");
    assert_eq!(fs::read(&rename_src).unwrap(), b"moved content\n");
    assert!(!rename_dst.exists(), "the rename destination is removed");
    assert_eq!(fs::read(&binary).unwrap(), bin_before);
}

// Test 5: settlement teardown destroys only the task namespace and never touches
// a foreign ref (a branch, or another task's namespace).
#[test]
fn engine_destroy_removes_task_refs_and_spares_foreign_refs() {
    let repo = init_repo();
    let root = repo.path.clone();
    let file = root.join("committed.txt");

    // A foreign branch and a foreign task's checkpoint ref, both must survive
    // this task's teardown.
    run_git(&root, &["branch", "keep-me"]);
    let head = git_out(&root, &["rev-parse", "HEAD"]);
    run_git(
        &root,
        &[
            "update-ref",
            "refs/iris/checkpoints/other-task/0000000001",
            &head,
        ],
    );

    let mut chain = CheckpointChain::new(root.clone(), "gc-task".to_string());
    chain
        .note_before(&file, Some((b"base\n".to_vec(), Mode::Normal)))
        .unwrap();
    for i in 0..6 {
        fs::write(&file, format!("v{i}\n")).unwrap();
        chain.checkpoint(i + 1, None, format!("v{i}")).unwrap();
    }
    assert_eq!(chain.len(), 6);

    chain.destroy().unwrap();
    assert_eq!(task_ref_count(&root, "gc-task"), 0);
    assert_eq!(chain.len(), 0);

    // Foreign refs untouched.
    assert_eq!(
        git_out(&root, &["rev-parse", "--verify", "refs/heads/keep-me"]),
        head
    );
    assert_eq!(task_ref_count(&root, "other-task"), 1);
}

// Test 9: building checkpoints never moves HEAD, the user index, or the stash.
#[test]
fn engine_never_touches_head_index_or_stash() {
    let repo = init_repo();
    let root = repo.path.clone();
    let file = root.join("committed.txt");

    // Give the user a staged change and a stash entry.
    fs::write(&file, "staged edit\n").unwrap();
    run_git(&root, &["add", "committed.txt"]);
    fs::write(root.join("stashme.txt"), "temp\n").unwrap();
    run_git(&root, &["add", "stashme.txt"]);
    run_git(&root, &["stash", "push", "-q", "-m", "wip"]);

    let head_before = git_out(&root, &["rev-parse", "HEAD"]);
    let index_before = git_out(&root, &["ls-files", "--stage"]);
    let stash_before = git_out(&root, &["rev-parse", "refs/stash"]);

    let mut chain = CheckpointChain::new(root.clone(), "untouched".to_string());
    chain
        .note_before(&file, Some((b"base\n".to_vec(), Mode::Normal)))
        .unwrap();
    fs::write(&file, "iris edit\n").unwrap();
    chain.checkpoint(1, None, "edit".to_string()).unwrap();

    assert_eq!(git_out(&root, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git_out(&root, &["ls-files", "--stage"]), index_before);
    assert_eq!(git_out(&root, &["rev-parse", "refs/stash"]), stash_before);
}

// --- guard integration tests --------------------------------------------

// Test 1: rollback in a clean tree restores the exact pre-task state.
#[test]
fn rollback_clean_tree_restores_pre_task() {
    let repo = init_repo();
    let guard = GitSafety::new(&repo.path);
    let created = repo.path.join("new.txt");
    let edited = repo.path.join("committed.txt");

    guard.note_mutation();
    iris_write(&guard, &created, b"iris made this\n");
    iris_write(&guard, &edited, b"iris changed this\n");

    let points = guard.restore_points();
    assert!(points.len() >= 2, "base + at least one checkpoint");

    guard.rollback(0).unwrap();
    assert!(!created.exists(), "a created file is removed");
    assert_eq!(fs::read(&edited).unwrap(), b"base\n", "the edit is undone");
    assert!(
        guard.restore_points().is_empty(),
        "rollback settles the task (no restore points remain)"
    );
}

// Test 2: rollback in a dirty tree leaves the user's dirty + untracked files
// byte-identical and restores the user's index (staging) to its baseline.
#[test]
fn rollback_dirty_tree_preserves_user_work_and_index() {
    let repo = init_repo();
    // User's pre-task dirty state: a modified tracked file, a staged file, and
    // an untracked file.
    fs::write(repo.path.join("committed.txt"), "user dirty\n").unwrap();
    fs::write(repo.path.join("staged.txt"), "user staged\n").unwrap();
    run_git(&repo.path, &["add", "staged.txt"]);
    fs::write(repo.path.join("untracked.txt"), "user untracked\n").unwrap();

    let guard = GitSafety::new(&repo.path);
    guard.note_mutation();
    let baseline_index = guard.baseline_index().unwrap();

    // Iris creates a file (its own work), and the user's index drifts (a new
    // staged file) during the task.
    iris_write(&guard, &repo.path.join("iris.txt"), b"iris work\n");
    fs::write(repo.path.join("extra.txt"), "extra\n").unwrap();
    run_git(&repo.path, &["add", "extra.txt"]);

    let outcome = guard.rollback(0).unwrap();
    assert!(
        outcome.index_warning.is_none(),
        "clean state restores index"
    );

    // Iris's own work is gone; the user's dirty/untracked work is byte-identical.
    assert!(!repo.path.join("iris.txt").exists());
    assert_eq!(
        fs::read(repo.path.join("committed.txt")).unwrap(),
        b"user dirty\n"
    );
    assert_eq!(
        fs::read(repo.path.join("untracked.txt")).unwrap(),
        b"user untracked\n"
    );
    // The index is restored to baseline: extra.txt is no longer staged.
    let index_now = git_out(&repo.path, &["ls-files", "--stage"]);
    assert_eq!(index_now, baseline_index.trim());
}

// Test 3: auto-checkpointing accumulates multiple restore points; rolling back
// to an intermediate one restores that step, not the pre-task base.
#[test]
fn multi_restore_point_rolls_back_to_intermediate() {
    let repo = init_repo();
    let guard = GitSafety::new(&repo.path);
    let file = repo.path.join("step.txt");

    guard.note_mutation();
    iris_write(&guard, &file, b"step one\n");
    iris_write(&guard, &file, b"step two\n");
    iris_write(&guard, &file, b"step three\n");

    let points = guard.restore_points();
    // base + three checkpoints.
    assert_eq!(
        points.len(),
        4,
        "one restore point per mutating call + base"
    );

    // Roll back to the first intermediate checkpoint (after "step one").
    let first = points[1].seq;
    guard.rollback(first).unwrap();
    assert_eq!(fs::read(&file).unwrap(), b"step one\n");
}

// Test 6: a crash leaves the disk diverged from the op-log; resume reconciles by
// appending a recovery checkpoint and surfacing a one-line notice.
#[test]
fn crash_resume_reconciles_and_notifies() {
    let repo = init_repo();
    let file = repo.path.join("committed.txt");

    // Session A: Iris edits, task stays unsettled (a crash -- no accept/rollback).
    let task_id = {
        let guard = GitSafety::new(&repo.path);
        guard.note_mutation();
        iris_write(&guard, &file, b"iris edit\n");
        // Grab the task id from the persisted record.
        let git_dir = task_state::git_dir(&repo.path).unwrap();
        let tasks = task_state::load_all(&git_dir);
        assert_eq!(tasks.len(), 1, "one unsettled task persisted");
        tasks[0].task_id.clone()
    };
    let refs_before = task_ref_count(&repo.path, &task_id);

    // Simulate divergence: the working copy moves after the last checkpoint.
    fs::write(&file, "diverged after crash\n").unwrap();

    // Session B: a fresh guard in the same repo reconciles on resume.
    let guard_b = GitSafety::new(&repo.path);
    let notice = expect_notice(guard_b.recover_and_expire());
    assert!(notice.contains("unsettled"), "notice: {notice}");
    assert!(
        task_ref_count(&repo.path, &task_id) > refs_before,
        "a recovery checkpoint is appended to the chain"
    );
}

// Test 7: an unsettled task past the expiry window auto-settles as accepted and
// its checkpoint refs are deleted (no rollback offered for code the user has
// lived with).
#[test]
fn expired_task_auto_settles_accepted_and_deletes_refs() {
    let repo = init_repo();
    let file = repo.path.join("committed.txt");

    let (task_id, git_dir) = {
        let guard = GitSafety::new(&repo.path);
        guard.note_mutation();
        iris_write(&guard, &file, b"old work\n");
        let git_dir = task_state::git_dir(&repo.path).unwrap();
        let task = task_state::load_all(&git_dir).pop().unwrap();
        (task.task_id.clone(), git_dir)
    };
    assert!(task_ref_count(&repo.path, &task_id) > 0);

    // Backdate the persisted record beyond the expiry window.
    let mut task = task_state::load_all(&git_dir).pop().unwrap();
    task.updated_ms = task_state::now_ms() - (31 * 24 * 60 * 60 * 1000);
    task_state::save(&git_dir, &task).unwrap();

    // Resume sweep expires it: no notice, refs gone, record removed.
    let guard = GitSafety::new(&repo.path);
    assert!(
        matches!(guard.recover_and_expire(), RecoveryOutcome::None),
        "an expired task surfaces no recovery notice"
    );
    assert_eq!(
        task_ref_count(&repo.path, &task_id),
        0,
        "refs deleted on expiry"
    );
    assert!(task_state::load_all(&git_dir).is_empty(), "record removed");
}

#[test]
fn accept_destroys_checkpoint_refs_and_record() {
    let repo = init_repo();
    let file = repo.path.join("committed.txt");
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let guard = GitSafety::new(&repo.path);

    guard.note_mutation();
    iris_write(&guard, &file, b"accepted once\n");
    iris_write(&guard, &file, b"accepted twice\n");
    let task_id = task_state::load_all(&git_dir).pop().unwrap().task_id;
    assert!(task_ref_count(&repo.path, &task_id) > 0);

    guard.accept().expect("a task was active to accept");

    assert_eq!(
        task_ref_count(&repo.path, &task_id),
        0,
        "accepted tasks leave no rollback refs behind"
    );
    assert_eq!(
        iris_ref_count(&repo.path),
        0,
        "refs/iris/ is empty after accept settlement"
    );
    assert!(task_state::load_all(&git_dir).is_empty(), "record removed");
}

#[test]
fn checkpoint_now_destroys_checkpoint_refs_and_record() {
    let repo = init_repo();
    let file = repo.path.join("committed.txt");
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let guard = GitSafety::new(&repo.path);

    guard.note_mutation();
    iris_write(&guard, &file, b"checkpointed\n");
    let task_id = task_state::load_all(&git_dir).pop().unwrap().task_id;
    assert!(task_ref_count(&repo.path, &task_id) > 0);

    guard
        .checkpoint_now()
        .expect("a task was active to checkpoint");

    assert_eq!(
        task_ref_count(&repo.path, &task_id),
        0,
        "explicit checkpoint settlement leaves no rollback refs behind"
    );
    assert!(task_state::load_all(&git_dir).is_empty(), "record removed");
}

#[test]
fn orphan_ref_sweep_removes_recordless_free_namespace() {
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let task_id = "recordlessfree";
    write_checkpoint_ref(&repo.path, task_id);
    assert_eq!(task_ref_count(&repo.path, task_id), 1);

    let guard = GitSafety::new(&repo.path);
    guard.expire_stale(&git_dir, SystemTime::now());

    assert_eq!(
        task_ref_count(&repo.path, task_id),
        0,
        "recordless checkpoint namespaces are swept"
    );
}

#[test]
fn orphan_ref_sweep_skips_leased_recordless_namespace() {
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let task_id = "recordlessleased";
    write_checkpoint_ref(&repo.path, task_id);
    let lease = lock::try_exclusive(&lock::lease_path(&git_dir, task_id))
        .unwrap()
        .expect("lease acquired");

    let guard = GitSafety::new(&repo.path);
    guard.expire_stale(&git_dir, SystemTime::now());
    assert_eq!(
        task_ref_count(&repo.path, task_id),
        1,
        "a held stale lease blocks orphan cleanup"
    );

    drop(lease);
    guard.expire_stale(&git_dir, SystemTime::now());
    assert_eq!(
        task_ref_count(&repo.path, task_id),
        0,
        "cleanup resumes once the stale lease is free"
    );
}

#[test]
fn orphan_ref_sweep_keeps_linked_worktree_recorded_namespace() {
    let repo = init_repo();
    let primary_git_dir = task_state::git_dir(&repo.path).unwrap();
    let linked = temp_dir();
    fs::remove_dir(&linked.path).unwrap();
    run_git(
        &repo.path,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked-sweep",
            linked.path.to_str().unwrap(),
        ],
    );

    let (task_id, linked_git_dir) = {
        let guard = GitSafety::new(&linked.path);
        guard.note_mutation();
        iris_write(&guard, &linked.path.join("linked.txt"), b"linked work\n");
        let linked_git_dir = task_state::git_dir(&linked.path).unwrap();
        let task_id = task_state::load_all(&linked_git_dir).pop().unwrap().task_id;
        (task_id, linked_git_dir)
    };
    assert!(task_ref_count(&repo.path, &task_id) > 0);

    let primary_guard = GitSafety::new(&repo.path);
    primary_guard.expire_stale(&primary_git_dir, SystemTime::now());
    assert!(
        task_ref_count(&repo.path, &task_id) > 0,
        "a namespace recorded by a linked worktree is not an orphan"
    );

    task_state::remove(&linked_git_dir, &task_id);
    primary_guard.expire_stale(&primary_git_dir, SystemTime::now());
    assert_eq!(
        task_ref_count(&repo.path, &task_id),
        0,
        "once every linked-worktree record is gone, the leaked namespace is swept"
    );
}

// Test 8: in a non-git directory the guard degrades to content-snapshot restore
// points that still undo Iris's own work.
#[test]
fn non_git_fallback_snapshots_and_restores() {
    let dir = temp_dir();
    let existing = dir.path.join("existing.txt");
    fs::write(&existing, "original\n").unwrap();

    let guard = GitSafety::new(&dir.path);
    let notice = guard.note_mutation().expect("degrade surfaces a notice");
    assert!(notice.contains("degraded") || notice.contains("not a git"));

    // Iris edits an existing file and creates a new one (degraded fallback
    // records content snapshots).
    iris_write_degraded(&guard, &existing, b"iris changed it\n");
    let created = dir.path.join("created.txt");
    iris_write_degraded(&guard, &created, b"iris new file\n");

    let points = guard.restore_points();
    assert!(points.len() >= 2, "base + fallback checkpoints: {points:?}");

    guard.rollback(0).unwrap();
    assert_eq!(fs::read(&existing).unwrap(), b"original\n", "edit undone");
    assert!(!created.exists(), "created file removed");
}

/// Degraded-mode write helper: no gating, so `after_exec` records a fallback
/// content snapshot for the targets. The confirm hash is irrelevant in degraded
/// mode (no attribution), so a plain write suffices.
fn iris_write_degraded(guard: &GitSafety, path: &Path, content: &[u8]) {
    let targets = [path.to_path_buf()];
    guard.before_exec(&targets);
    fs::write(path, content).unwrap();
    let _ = guard.after_exec(&targets, Some(&crate::tools::content_hash(content)));
}

// Finding 1 (CRITICAL): a user edit to a ledger path landing after Iris's last
// write is preserved on rollback -- rollback must not clobber the user's newer
// bytes (ADR-0028 TOCTOU rule). Non-diverged ledger paths still roll back, and
// the user is told which path was kept.
#[test]
fn rollback_preserves_user_edit_to_ledger_path() {
    let repo = init_repo();
    let guard = GitSafety::new(&repo.path);
    let edited = repo.path.join("committed.txt");
    let created = repo.path.join("iris_only.txt");

    guard.note_mutation();
    iris_write(&guard, &edited, b"iris wrote this\n");
    iris_write(&guard, &created, b"iris made this\n");

    // The user edits a ledger path out of band, after Iris's last recorded write.
    fs::write(&edited, b"user changed it later\n").unwrap();

    let outcome = guard.rollback(0).unwrap();

    // The user's newer bytes survive: nothing silently lost.
    assert_eq!(
        fs::read(&edited).unwrap(),
        b"user changed it later\n",
        "a user edit after Iris's last write is never clobbered by rollback"
    );
    // A non-diverged ledger path still rolls back (the created file is removed).
    assert!(
        !created.exists(),
        "non-diverged ledger paths still roll back"
    );
    // The user is told which path was preserved.
    assert!(
        outcome
            .preserved_notices
            .iter()
            .any(|line| line.contains("committed.txt")),
        "a per-path preserved notice names the diverged file: {:?}",
        outcome.preserved_notices
    );
}

// Finding 2 (HIGH): after a restart the unsettled task is rehydrated from its
// persisted record + refs, so `/rollback` actually rolls back instead of
// reporting "no unsettled Iris changes".
#[test]
fn resume_rehydrates_task_and_rollback_restores() {
    let repo = init_repo();
    let edited = repo.path.join("committed.txt");
    let created = repo.path.join("iris_new.txt");

    // Session A: Iris works, task stays unsettled (a crash -- no accept/rollback).
    {
        let guard = GitSafety::new(&repo.path);
        guard.note_mutation();
        iris_write(&guard, &edited, b"iris edit\n");
        iris_write(&guard, &created, b"iris new file\n");
    }

    // Session B: a fresh guard over the same repo/session dir reconciles and
    // rehydrates the unsettled task.
    let guard_b = GitSafety::new(&repo.path);
    let notice = expect_notice(guard_b.recover_and_expire());
    assert!(notice.contains("unsettled"), "notice: {notice}");

    let points = guard_b.restore_points();
    assert!(
        !points.is_empty(),
        "the rehydrated task offers restore points post-restart"
    );

    let outcome = guard_b.rollback(0).unwrap();
    assert!(
        outcome.summary.contains("rolled back"),
        "rollback succeeds post-restart: {}",
        outcome.summary
    );
    assert_eq!(
        fs::read(&edited).unwrap(),
        b"base\n",
        "the pre-task content is restored after a simulated restart"
    );
    assert!(
        !created.exists(),
        "Iris's created file is removed on rollback"
    );
}

// Finding 3 (HIGH): a recovery checkpoint is a FULL snapshot of every ledger
// path, not just the diverged ones -- rolling back to a recovery point must not
// delete-bomb the non-diverged ledger paths.
#[test]
fn recovery_checkpoint_is_full_snapshot_not_delete_bomb() {
    let repo = init_repo();
    let edited = repo.path.join("committed.txt");

    // Session A: Iris touches two ledger paths, task stays unsettled.
    {
        let guard = GitSafety::new(&repo.path);
        guard.note_mutation();
        iris_write(&guard, &edited, b"iris edited\n");
        iris_write(&guard, &repo.path.join("iris_extra.txt"), b"iris extra\n");
    }

    // Divergence on ONE path only (a crash-time external edit).
    fs::write(&edited, b"diverged content\n").unwrap();

    // Session B: reconcile (append a recovery checkpoint) + rehydrate.
    let guard_b = GitSafety::new(&repo.path);
    expect_notice(guard_b.recover_and_expire());

    // A recovery checkpoint was appended (the newest restore point past base).
    let points = guard_b.restore_points();
    assert!(
        points.iter().map(|p| p.seq).max().unwrap() > 0,
        "a recovery checkpoint was appended"
    );

    // Inspect the recovery commit's tree directly: it must be a FULL snapshot of
    // every ledger path, not just the diverged one. (Asserting on the tree
    // isolates finding 3 -- a rollback-time check would be masked by finding 1's
    // divergence exclusion, which independently spares an on-disk path.)
    let task_id = task_state::load_all(&task_state::git_dir(&repo.path).unwrap())
        .pop()
        .unwrap()
        .task_id;
    let recovery_ref = git_out(
        &repo.path,
        &[
            "for-each-ref",
            "--sort=-refname",
            "--count=1",
            "--format=%(objectname)",
            &format!("refs/iris/checkpoints/{task_id}/"),
        ],
    );
    let tree = git_out(&repo.path, &["ls-tree", "-r", "--name-only", &recovery_ref]);
    let names: Vec<&str> = tree.lines().collect();
    assert!(
        names.contains(&"committed.txt"),
        "recovery tree holds the diverged path: {names:?}"
    );
    assert!(
        names.contains(&"iris_extra.txt"),
        "recovery tree holds the non-diverged path too (no delete-bomb): {names:?}"
    );
    // The diverged path's ACTUAL disk bytes are what the snapshot captured.
    let captured = git_out(
        &repo.path,
        &["cat-file", "blob", &format!("{recovery_ref}:committed.txt")],
    );
    assert_eq!(
        captured, "diverged content",
        "the recovery snapshot captures the diverged path's actual disk state"
    );
}

// --- final task diff (issue #264) -----------------------------------------

// Drive a bash-style out-of-band change (no approved target) through the guard,
// so the async attribution scan runs; the caller joins it at a later sync
// barrier (e.g. `task_diff`). Mutates `path` on disk via `mutate`.
fn bash_change(guard: &GitSafety, mutate: impl FnOnce()) {
    guard.before_exec(&[]);
    mutate();
    let _ = guard.after_exec(&[], None);
}

// Deliverable 1 + test 1: the net diff is scoped to ledger paths only. A dirty
// tracked file and an untracked file the user (not Iris) owns never appear;
// only Iris's own change does.
#[test]
fn net_diff_excludes_user_dirty_and_untracked() {
    let repo = init_repo();
    // User's pre-task work: a modified tracked file and an untracked file.
    fs::write(repo.path.join("committed.txt"), "user dirty\n").unwrap();
    fs::write(repo.path.join("user_untracked.txt"), "user only\n").unwrap();

    let guard = GitSafety::new(&repo.path);
    guard.note_mutation();
    // Iris's own work: a brand-new file.
    iris_write(&guard, &repo.path.join("iris_new.txt"), b"iris made this\n");

    let diff = guard.task_diff(None).unwrap();
    assert_eq!(diff.files.len(), 1, "only Iris's ledger path appears");
    assert_eq!(diff.files[0].path, "iris_new.txt");
    assert_eq!(diff.files[0].kind, super::net_diff::ChangeKind::Create);
    assert!(
        !diff.unified().contains("user dirty") && !diff.unified().contains("user only"),
        "the user's dirty/untracked work never leaks into the diff"
    );
}

// Test 2: a file edited several times in one task shows one net hunk set
// (baseline -> current), not a per-step diff.
#[test]
fn net_diff_collapses_repeated_edits() {
    let repo = init_repo();
    let guard = GitSafety::new(&repo.path);
    let file = repo.path.join("committed.txt");

    guard.note_mutation();
    iris_write(&guard, &file, b"step one\n");
    iris_write(&guard, &file, b"step two\n");
    iris_write(&guard, &file, b"final\n");

    let diff = guard.task_diff(None).unwrap();
    assert_eq!(diff.files.len(), 1);
    let file_diff = &diff.files[0];
    assert_eq!(file_diff.kind, super::net_diff::ChangeKind::Edit);
    // Net is base -> final: one line removed, one added; the intermediate
    // "step one"/"step two" never appear.
    assert_eq!((file_diff.added, file_diff.removed), (1, 1));
    assert!(file_diff.unified.contains("+final"));
    assert!(file_diff.unified.contains("-base"));
    assert!(!file_diff.unified.contains("step one"));
    assert!(!file_diff.unified.contains("step two"));
}

// Test 3: a binary file Iris changes is summarized as a binary change with no
// text diff.
#[test]
fn net_diff_reports_binary_without_text() {
    let repo = init_repo();
    let guard = GitSafety::new(&repo.path);
    let bin = repo.path.join("blob.bin");

    guard.note_mutation();
    iris_write(&guard, &bin, &[0u8, 1, 2, 3, 255]);

    let diff = guard.task_diff(None).unwrap();
    assert_eq!(diff.files.len(), 1);
    assert!(diff.files[0].binary, "NUL content is reported as binary");
    assert_eq!((diff.files[0].added, diff.files[0].removed), (0, 0));
    assert!(diff.unified().contains("Binary file blob.bin changed"));
    assert!(diff.summary_lines().iter().any(|l| l.contains("binary")));
}

// Test 3 (delete/rename shape): a bash-attributed delete of a pre-existing
// tracked file renders as a delete. A rename in the ledger's shape is exactly
// this delete of the old path plus a create of the new one.
#[test]
fn net_diff_reports_delete_via_attribution() {
    let repo = init_repo();
    let guard = GitSafety::new(&repo.path);
    let file = repo.path.join("committed.txt");

    guard.note_mutation();
    bash_change(&guard, || {
        fs::remove_file(&file).unwrap();
    });

    let diff = guard.task_diff(None).unwrap();
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "committed.txt");
    assert_eq!(diff.files[0].kind, super::net_diff::ChangeKind::Delete);
    assert!(diff.files[0].unified.contains("+++ /dev/null"));
}

// Test 4: an empty diff is honest -- no task, or a task whose changes net to
// nothing (edited then reverted), both report no files.
#[test]
fn net_diff_empty_for_no_task_and_reverted_change() {
    let repo = init_repo();
    let guard = GitSafety::new(&repo.path);
    // No mutation yet: no unsettled task.
    assert!(guard.task_diff(None).unwrap().is_empty());

    let file = repo.path.join("committed.txt");
    guard.note_mutation();
    iris_write(&guard, &file, b"changed\n");
    iris_write(&guard, &file, b"base\n"); // reverted to the pre-task content
    assert!(
        guard.task_diff(None).unwrap().is_empty(),
        "a change reverted to its pre-task state nets to nothing"
    );
}

// Deliverable 3 + test 6: the source-tree root is a parameter. The current side
// is read from the given root, not the hardcoded workspace, so the same ledger
// diffs against an alternate tree.
#[test]
fn net_diff_respects_alternate_source_root() {
    let repo = init_repo();
    let guard = GitSafety::new(&repo.path);
    let file = repo.path.join("committed.txt");

    guard.note_mutation();
    iris_write(&guard, &file, b"workspace version\n");

    // An alternate tree holds a different current version of the same rel path.
    let alt = temp_dir();
    fs::write(alt.path.join("committed.txt"), "alt version\n").unwrap();

    let diff = guard.task_diff(Some(&alt.path)).unwrap();
    assert_eq!(diff.files.len(), 1);
    assert!(
        diff.files[0].unified.contains("+alt version"),
        "the current side comes from the alternate root, not the workspace"
    );
    assert!(!diff.files[0].unified.contains("workspace version"));
}

// Deliverable 2 + test 7: `task_diff` joins the async attribution scan at its
// sync barrier, so a bash-attributed change to a previously-clean file is
// visible in the diff.
#[test]
fn net_diff_includes_bash_attributed_change_after_barrier() {
    let repo = init_repo();
    let guard = GitSafety::new(&repo.path);
    let file = repo.path.join("committed.txt");

    guard.note_mutation();
    // A bash-style change with no approved target: attribution runs async.
    bash_change(&guard, || {
        fs::write(&file, "bash changed\n").unwrap();
    });

    // The barrier inside task_diff joins the scan before computing.
    let diff = guard.task_diff(None).unwrap();
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "committed.txt");
    assert_eq!(diff.files[0].kind, super::net_diff::ChangeKind::Edit);
    assert!(diff.files[0].unified.contains("+bash changed"));
    assert!(diff.files[0].unified.contains("-base"));
}

// The non-git fallback computes the same ledger-scoped net diff from content
// snapshots (reduced guarantees, ADR-0028 Alternative 3).
#[test]
fn net_diff_degraded_fallback_scopes_to_touched_paths() {
    let dir = temp_dir(); // not a git repo -> degraded mode
    let guard = GitSafety::new(&dir.path);
    guard.note_mutation();
    let file = dir.path.join("note.txt");
    // Degraded writes go through an approved target so the fallback records them.
    guard.approve(std::slice::from_ref(&file), false);
    guard.before_exec(std::slice::from_ref(&file));
    fs::write(&file, "iris note\n").unwrap();
    let _ = guard.after_exec(
        std::slice::from_ref(&file),
        Some(&crate::tools::content_hash(b"iris note\n")),
    );

    let diff = guard.task_diff(None).unwrap();
    assert_eq!(diff.files.len(), 1);
    assert_eq!(diff.files[0].path, "note.txt");
    assert_eq!(diff.files[0].kind, super::net_diff::ChangeKind::Create);
}

// Finding 1 (issue #264, ADR-0028 TOCTOU): when the user edits a ledger path
// after Iris's last recorded write, the net diff must show Iris's last recorded
// state (the chain tip), NOT the user's bytes as Iris output, and must flag the
// divergence with an explicit per-path notice.
#[test]
fn net_diff_excludes_user_bytes_written_after_iris() {
    let repo = init_repo();
    let guard = GitSafety::new(&repo.path);
    let file = repo.path.join("committed.txt");

    guard.note_mutation();
    iris_write(&guard, &file, b"iris content\n");
    // The user edits the same ledger path out of band, after Iris's last write.
    fs::write(&file, "user content\n").unwrap();

    let diff = guard.task_diff(None).unwrap();
    assert_eq!(diff.files.len(), 1);
    let file_diff = &diff.files[0];
    assert!(file_diff.diverged, "the path is flagged diverged");
    assert!(
        file_diff.unified.contains("iris content"),
        "the diff shows Iris's last recorded state, not the user's bytes"
    );
    assert!(
        !file_diff.unified.contains("user content"),
        "the user's bytes never render as Iris output"
    );
    assert!(
        diff.summary_lines()
            .iter()
            .any(|l| l.contains("showing Iris's last recorded state")),
        "an explicit per-path divergence notice is surfaced in the summary"
    );
}

// Finding 2 (issue #264): a checkpoint/blob read failure must fail closed --
// `task_diff` returns an error, never a silent empty "no Iris changes" diff that
// would let the accept flow settle a task whose changes were never shown.
#[test]
fn net_diff_fails_closed_on_unreadable_checkpoint() {
    let repo = init_repo();
    let guard = GitSafety::new(&repo.path);
    let file = repo.path.join("committed.txt");

    guard.note_mutation();
    iris_write(&guard, &file, b"iris content\n");

    // Corrupt the object store so the checkpoint tree/blob reads fail.
    fs::remove_dir_all(repo.path.join(".git/objects")).unwrap();

    assert!(
        guard.task_diff(None).is_err(),
        "a checkpoint read error must propagate, not become an empty diff"
    );
}

// --- task-ownership lease + mutation lock (issue #285, ADR-0030) -----------
//
// Cross-process liveness is a real per-process `flock`, so these tests use real
// child processes. A foreign live task holder is simulated with the `flock(1)`
// utility (`--no-fork` so the spawned child *is* the lock-holding process and a
// SIGKILL of `child.id()` releases the lease). Tests that need `flock` skip with
// a note when it is absent, so CI never flakes on a missing binary; Linux CI has
// util-linux `flock`.

use std::time::{Duration, Instant};

/// Whether the util-linux `flock(1)` binary (with `--no-fork`) is available.
/// Gates the cross-process tests so a machine without it skips rather than fails.
fn have_flock() -> bool {
    Command::new("flock")
        .arg("--help")
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("--no-fork"))
        .unwrap_or(false)
}

/// Spawn a foreign process that holds an exclusive `flock` on `lock_path` for the
/// test's lifetime. `--no-fork` makes the returned child the actual lock holder
/// (it execs `sleep`), so `child.kill()` (SIGKILL) releases the lock by closing
/// the only fd -- exactly a process crash.
fn spawn_foreign_holder(lock_path: &Path) -> std::process::Child {
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    // `-w 5` (block up to 5s) rather than `-n`: the whole test binary shares one
    // fd table, so a concurrent test's `Command::spawn` can dup this lock's fd
    // across the fork->exec window and briefly pin it. A non-blocking `flock -n`
    // would then lose the race and exit without ever holding the lock, leaving a
    // dead "holder" and a spuriously lease-free record. Blocking past the
    // transient pin (which clears when the unrelated child execs) makes the
    // holder reliably acquire; the 5s cap keeps a genuinely stuck lock bounded.
    let child = Command::new("flock")
        .args([
            "--no-fork",
            "-x",
            "-w",
            "5",
            lock_path.to_str().unwrap(),
            "sleep",
            "1000",
        ])
        .spawn()
        .expect("spawn flock holder");
    // Wait until the holder has actually acquired the lock before returning, so
    // the test observes the leased state deterministically.
    wait_until(Duration::from_secs(5), || !lock::is_lease_free(lock_path));
    child
}

/// Poll `cond` until it is true or `timeout` elapses. Returns whether it became
/// true. Cheap sleep between polls; used to await real cross-process lock state.
fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

/// Create one unsettled task record in `repo` (via the real guard path so the
/// record carries production fields and the lease is released when the guard
/// drops), writing a fresh file `rel` so it never trips dirty-file approval.
/// Returns the new task id.
fn create_unsettled_task(repo: &Path, rel: &str) -> String {
    let git_dir = task_state::git_dir(repo).unwrap();
    let before: std::collections::BTreeSet<String> = task_state::load_all(&git_dir)
        .into_iter()
        .map(|t| t.task_id)
        .collect();
    {
        let guard = GitSafety::new(repo);
        guard.note_mutation();
        iris_write(&guard, &repo.join(rel), b"iris content\n");
    } // guard dropped: its lease fd closes, so the record is now lease-free
    task_state::load_all(&git_dir)
        .into_iter()
        .map(|t| t.task_id)
        .find(|id| !before.contains(id))
        .expect("a new unsettled task record")
}

// Test 1 (#285): two live task records coexist; neither process adopts the
// other, and `recoverable_tasks()` lists only the lease-free record. The first
// record's lease is held by a foreign live process (`flock`), so it is a live
// foreign task that must be skipped.
#[test]
fn recoverable_tasks_skips_live_foreign_lease() {
    if !have_flock() {
        eprintln!("skipping recoverable_tasks_skips_live_foreign_lease: flock(1) unavailable");
        return;
    }
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();

    let foreign = create_unsettled_task(&repo.path, "foreign.txt");
    let mine = create_unsettled_task(&repo.path, "mine.txt");

    // A foreign live process holds the first task's lease.
    let mut holder = spawn_foreign_holder(&lock::lease_path(&git_dir, &foreign));

    let guard = GitSafety::new(&repo.path);
    let recoverable = guard.recoverable_tasks();

    // Only the lease-free record is recoverable; the live foreign one is skipped.
    let ids: Vec<&str> = recoverable.iter().map(|t| t.task_id.as_str()).collect();
    assert!(
        ids.contains(&mine.as_str()),
        "the lease-free record is recoverable: {ids:?}"
    );
    assert!(
        !ids.contains(&foreign.as_str()),
        "the live foreign (leased) record is never listed: {ids:?}"
    );
    // The recoverable record carries its workspace + a small age (fields the #288
    // picker consumes).
    let ws = repo
        .path
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let rec = recoverable.iter().find(|t| t.task_id == mine).unwrap();
    assert_eq!(rec.workspace, ws);
    assert!(rec.age < Duration::from_secs(3600));
    assert_eq!(rec.class, TaskClass::Recoverable);

    // Adopting the foreign live task is refused (its lease is held).
    assert!(
        guard.adopt_task(&foreign).is_none(),
        "a live foreign task is never adopted"
    );

    holder.kill().unwrap();
    holder.wait().unwrap();
}

// Test 2 (#285): a SIGKILL'd process releases its lease by construction, so its
// task becomes recoverable and adoptable on the next startup.
#[test]
fn sigkilled_lease_becomes_recoverable() {
    if !have_flock() {
        eprintln!("skipping sigkilled_lease_becomes_recoverable: flock(1) unavailable");
        return;
    }
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let task_id = create_unsettled_task(&repo.path, "work.txt");
    let lease = lock::lease_path(&git_dir, &task_id);

    // A live process holds the lease: the task is skipped by recovery.
    let mut holder = spawn_foreign_holder(&lease);
    let guard = GitSafety::new(&repo.path);
    assert!(
        !guard
            .recoverable_tasks()
            .iter()
            .any(|t| t.task_id == task_id),
        "while leased, the task is a live foreign task (skipped)"
    );

    // SIGKILL the holder: the OS closes its fd, releasing the lease.
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || lock::is_lease_free(&lease)),
        "the crashed process's lease is released"
    );

    // Now the orphan is recoverable and adoptable.
    assert!(
        guard
            .recoverable_tasks()
            .iter()
            .any(|t| t.task_id == task_id),
        "after the crash the task becomes recoverable"
    );
    let adopted = guard.adopt_task(&task_id).expect("the orphan is adoptable");
    assert_eq!(adopted.task_id, task_id);
    assert!(
        guard.has_task(),
        "the adopted task is now this process's active task"
    );
}

// Test 3 (#285): a legacy record without lock metadata deserializes (serde
// defaults), is surfaced as unknown, and is never auto-adopted.
#[test]
fn legacy_record_is_unknown_and_never_auto_adopted() {
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let ws = repo
        .path
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let tasks_dir = git_dir.join("iris").join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();

    // A record in the pre-ADR-0030 shape: no `owner`/`lock_protocol` fields.
    let task_id = "legacyabc123";
    let now_ms = task_state::now_ms();
    let legacy_json = format!(
        r#"{{"task_id":"{task_id}","workspace":"{ws}","created_ms":{now_ms},"updated_ms":{now_ms},"expected":{{}},"tip_seq":0}}"#
    );
    fs::write(tasks_dir.join(format!("{task_id}.json")), legacy_json).unwrap();

    // It deserializes and classifies as legacy/unknown.
    let guard = GitSafety::new(&repo.path);
    let recoverable = guard.recoverable_tasks();
    let rec = recoverable
        .iter()
        .find(|t| t.task_id == task_id)
        .expect("the legacy record deserializes and is surfaced");
    assert_eq!(rec.class, TaskClass::Legacy, "no lock metadata => unknown");

    // Recovery never auto-adopts it: it opens the picker (explicit selection),
    // listing the legacy row, and no task becomes active (#288, ADR-0030).
    let RecoveryOutcome::Picker(rows) = guard.recover_and_expire() else {
        panic!("an unknown-legacy record requires explicit selection (picker)");
    };
    assert!(
        rows.iter().any(|t| t.task_id == task_id),
        "the picker lists the unknown-legacy task id"
    );
    assert!(
        !guard.has_task(),
        "a legacy record is never auto-adopted as the active task"
    );
}

// #288 review fix: a LEASED record is never listed, even a legacy one. Another
// process may have adopted a legacy record and hold its lease while the record
// still reads lock_protocol=None, so `recoverable_tasks` must probe the lease
// BEFORE classifying. flock is per open-file-description, so a same-process
// probe on a different fd still observes the held lease -- no subprocess needed.
#[test]
fn leased_legacy_record_is_not_listed() {
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let ws = repo
        .path
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let tasks_dir = git_dir.join("iris").join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();
    let task_id = "legacyleased01";
    let now_ms = task_state::now_ms();
    let legacy_json = format!(
        r#"{{"task_id":"{task_id}","workspace":"{ws}","created_ms":{now_ms},"updated_ms":{now_ms},"expected":{{}},"tip_seq":0}}"#
    );
    fs::write(tasks_dir.join(format!("{task_id}.json")), legacy_json).unwrap();

    let guard = GitSafety::new(&repo.path);
    // Hold the record's lease in-process (simulating a live adopter).
    let held = lock::try_exclusive(&lock::lease_path(&git_dir, task_id))
        .unwrap()
        .expect("lease acquired");
    assert!(
        !guard
            .recoverable_tasks()
            .iter()
            .any(|t| t.task_id == task_id),
        "a leased (live) legacy record is never listed"
    );

    // Once the lease is free, the same legacy record surfaces as unknown.
    drop(held);
    assert!(
        guard
            .recoverable_tasks()
            .iter()
            .any(|t| t.task_id == task_id && t.class == TaskClass::Legacy),
        "a lease-free legacy record is surfaced as unknown"
    );
}

// Test 4 (#285): settle vs adopt are serialized by the repo mutation lock. A
// foreign process holds the mutation lock, so an `adopt_task` that must write
// (append a recovery checkpoint) blocks until the lock is released -- proving no
// torn interleave between a concurrent settle and adopt.
#[test]
fn adopt_serializes_on_mutation_lock() {
    if !have_flock() {
        eprintln!("skipping adopt_serializes_on_mutation_lock: flock(1) unavailable");
        return;
    }
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let file = repo.path.join("work.txt");
    let task_id = create_unsettled_task(&repo.path, "work.txt");

    // Diverge the disk so adopt must append a recovery checkpoint -- the write
    // path that takes the mutation lock.
    fs::write(&file, b"diverged on disk\n").unwrap();

    // A foreign process holds the repo mutation lock.
    let mut holder = spawn_foreign_holder(&lock::mutation_lock_path(&git_dir));

    let guard = GitSafety::new(&repo.path);
    // Anchor the release to the SAME `start` the wait assertion reads. The killer
    // previously slept a fixed 400ms from its own scheduling: under a loaded
    // parallel runner it could begin (and finish) that countdown before this
    // thread reached `adopt_task`, so `waited` (measured from `start`) fell below
    // the threshold even though adopt did block on the lock. Pinning the release
    // to `start + hold` makes `waited` independent of scheduling skew -- adopt
    // cannot return until the mutation lock frees at `release_at`.
    let start = Instant::now();
    let release_at = start + Duration::from_millis(400);
    let killer = std::thread::spawn(move || {
        let now = Instant::now();
        if release_at > now {
            std::thread::sleep(release_at - now);
        }
        let _ = holder.kill();
        let _ = holder.wait();
    });

    let adopted = guard.adopt_task(&task_id);
    let waited = start.elapsed();

    killer.join().unwrap();

    assert!(
        adopted.is_some(),
        "adopt still succeeds once the lock is free"
    );
    assert!(
        waited >= Duration::from_millis(250),
        "adopt blocked on the mutation lock until the foreign holder released it \
         (waited {waited:?}); settle and adopt cannot interleave"
    );
}

// Regression (#349): the settled lease probe rides out a transient hold but
// still skips a lease held throughout. Recovery classifies an orphan by a
// non-blocking `flock` probe, and `flock` is inherited across `fork()`, so an
// unrelated child that dups a lease fd pins it until it `exec`s -- which under
// the parallel test runner intermittently made a lease-free orphan read as a
// live foreign task. `try_exclusive_settled` re-probes across a short window to
// tell that transient pin (clears at `exec`) from a genuine live owner (held
// continuously). Both directions are asserted so the fix never adopts a task
// another process still owns.
#[test]
fn settled_lease_probe_rides_out_transient_hold_but_skips_live() {
    if !have_flock() {
        eprintln!("skipping settled_lease_probe_...: flock(1) unavailable");
        return;
    }
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();

    // Transient hold: a foreign holder releases well within the settle window,
    // so the settled claim acquires once the lock frees.
    let transient = lock::lease_path(&git_dir, "transientlease");
    let mut holder = spawn_foreign_holder(&transient);
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(80));
        let _ = holder.kill();
        let _ = holder.wait();
    });
    let acquired = lock::try_exclusive_settled(&transient).unwrap();
    killer.join().unwrap();
    assert!(
        acquired.is_some(),
        "the settled probe rides out a hold that clears within the window"
    );
    drop(acquired);

    // Live hold: a foreign holder keeps the lease past the settle window, so the
    // settled claim re-probes then still reports contention -- a live foreign
    // task is never adopted.
    let live = lock::lease_path(&git_dir, "livelease");
    let mut holder = spawn_foreign_holder(&live);
    let start = Instant::now();
    let claim = lock::try_exclusive_settled(&live).unwrap();
    let waited = start.elapsed();
    assert!(
        claim.is_none(),
        "a lease held throughout the settle window is still skipped"
    );
    assert!(
        waited >= Duration::from_millis(100),
        "the claim re-probed across the settle window before bailing (waited {waited:?})"
    );
    holder.kill().unwrap();
    holder.wait().unwrap();
}

// Test 5 (#285): the recovery notice names the record actually adopted, not
// some other scanned record. With one lease-free orphan plus one live foreign
// (leased) task, recovery adopts the orphan and the notice names IT.
#[test]
fn recovery_notice_names_the_adopted_record() {
    if !have_flock() {
        eprintln!("skipping recovery_notice_names_the_adopted_record: flock(1) unavailable");
        return;
    }
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();

    let foreign = create_unsettled_task(&repo.path, "foreign.txt");
    let orphan = create_unsettled_task(&repo.path, "orphan.txt");

    // The foreign task is held live; only the orphan is adoptable.
    let mut holder = spawn_foreign_holder(&lock::lease_path(&git_dir, &foreign));

    let guard = GitSafety::new(&repo.path);
    let notice = expect_notice(guard.recover_and_expire());

    assert!(
        notice.contains(&orphan),
        "the notice names the adopted (orphan) record: {notice}"
    );
    assert!(
        !notice.contains(&foreign),
        "the notice never names the live foreign record: {notice}"
    );

    holder.kill().unwrap();
    holder.wait().unwrap();
}

// --- task metadata: opaque body + session join (issue #287, ADR-0031) ----

// Issue #287 test (1), record level: a mutating turn stamps the turn's prompt
// preview as the task's opaque `body`, and the current session id onto its
// `sessions` join.
#[test]
fn note_mutation_stamps_turn_context_as_body_and_session() {
    let repo = init_repo();
    let guard = GitSafety::new(&repo.path);
    guard.set_session_id("sessionaaaa".to_string());
    guard.set_turn_context(Some("fix the parser bug".to_string()));
    guard.note_mutation();
    iris_write(&guard, &repo.path.join("committed.txt"), b"iris\n");

    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let record = task_state::load_all(&git_dir)
        .pop()
        .expect("a task record was persisted");
    assert_eq!(
        record.body.as_deref(),
        Some("fix the parser bug"),
        "the opening turn's preview is captured as the opaque body"
    );
    assert_eq!(
        record.sessions,
        vec!["sessionaaaa".to_string()],
        "the current session id is stamped onto the join at open"
    );
}

// Issue #287 test (2): a follow-up turn joining an unsettled task never rewrites
// the body; a follow-up AFTER settlement opens a fresh task capturing the new
// turn's preview.
#[test]
fn follow_up_leaves_body_unchanged_then_new_task_after_settle() {
    let repo = init_repo();
    let edited = repo.path.join("committed.txt");
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let guard = GitSafety::new(&repo.path);

    // Turn 1 opens the task with body "first turn".
    guard.set_turn_context(Some("first turn".to_string()));
    guard.note_mutation();
    iris_write(&guard, &edited, b"one\n");
    assert_eq!(
        task_state::load_all(&git_dir)
            .pop()
            .unwrap()
            .body
            .as_deref(),
        Some("first turn")
    );

    // Turn 2 joins the SAME unsettled task: body must stay "first turn".
    guard.set_turn_context(Some("second turn".to_string()));
    guard.note_mutation();
    iris_write(&guard, &edited, b"two\n");
    assert_eq!(
        task_state::load_all(&git_dir)
            .pop()
            .unwrap()
            .body
            .as_deref(),
        Some("first turn"),
        "a follow-up turn joining an unsettled task never rewrites body"
    );

    // Settle, then a follow-up opens a fresh task capturing the new preview.
    guard.accept().expect("a task was active to accept");
    guard.set_turn_context(Some("third turn".to_string()));
    guard.note_mutation();
    iris_write(&guard, &edited, b"three\n");
    assert_eq!(
        task_state::load_all(&git_dir)
            .pop()
            .unwrap()
            .body
            .as_deref(),
        Some("third turn"),
        "a post-settlement follow-up opens a new task with the new turn's body"
    );
}

// Issue #287 (review fix): a passive session swap (/new, /resume) that joins
// the SAME unsettled task appends the new session to the record's live join
// (consecutive-deduped) without rewriting body -- the sessions vec is the
// authoritative recovery-UX join (ADR-0031).
#[test]
fn session_swap_joining_unsettled_task_appends_new_session() {
    let repo = init_repo();
    let edited = repo.path.join("committed.txt");
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let guard = GitSafety::new(&repo.path);

    // Session A opens the task.
    guard.set_session_id("sessionaaaa".to_string());
    guard.set_turn_context(Some("open in A".to_string()));
    guard.note_mutation();
    iris_write(&guard, &edited, b"one\n");
    assert_eq!(
        task_state::load_all(&git_dir).pop().unwrap().sessions,
        vec!["sessionaaaa".to_string()]
    );

    // Passive swap to session B (task stays unsettled); B mutates and joins.
    guard.set_session_id("sessionbbbb".to_string());
    guard.set_turn_context(Some("join in B".to_string()));
    guard.note_mutation();
    iris_write(&guard, &edited, b"two\n");
    let record = task_state::load_all(&git_dir).pop().unwrap();
    assert_eq!(
        record.sessions,
        vec!["sessionaaaa".to_string(), "sessionbbbb".to_string()],
        "the joining session is appended to the live join, ordered after A"
    );
    assert_eq!(
        record.body.as_deref(),
        Some("open in A"),
        "the swap-join never rewrites the body captured at open"
    );

    // A same-session follow-up is a no-op (consecutive-dedup).
    guard.set_turn_context(Some("more in B".to_string()));
    guard.note_mutation();
    iris_write(&guard, &edited, b"three\n");
    assert_eq!(
        task_state::load_all(&git_dir).pop().unwrap().sessions,
        vec!["sessionaaaa".to_string(), "sessionbbbb".to_string()],
        "a same-session follow-up does not duplicate the session id"
    );
}

// Issue #287 test (3): a second process rehydrating (adopting) the orphan
// appends its own session id to the record's join -- ordered and
// consecutive-deduped, written under the mutation lock.
#[test]
fn rehydrate_appends_session_id_ordered_and_deduped() {
    let repo = init_repo();
    let edited = repo.path.join("committed.txt");
    let git_dir = task_state::git_dir(&repo.path).unwrap();

    // Session A opens the task, then crashes (never settles).
    {
        let guard = GitSafety::new(&repo.path);
        guard.set_session_id("sessionaaaa".to_string());
        guard.set_turn_context(Some("work".to_string()));
        guard.note_mutation();
        iris_write(&guard, &edited, b"iris\n");
    }
    assert_eq!(
        task_state::load_all(&git_dir).pop().unwrap().sessions,
        vec!["sessionaaaa".to_string()]
    );

    // Session B adopts and appends its session id (ordered after A's).
    {
        let guard_b = GitSafety::new(&repo.path);
        guard_b.set_session_id("sessionbbbb".to_string());
        expect_notice(guard_b.recover_and_expire());
    }
    assert_eq!(
        task_state::load_all(&git_dir).pop().unwrap().sessions,
        vec!["sessionaaaa".to_string(), "sessionbbbb".to_string()],
        "the adopting session id is appended in order"
    );

    // Session C re-adopts with the SAME id as the last: consecutive-deduped, so
    // the join does not grow.
    {
        let guard_c = GitSafety::new(&repo.path);
        guard_c.set_session_id("sessionbbbb".to_string());
        expect_notice(guard_c.recover_and_expire());
    }
    assert_eq!(
        task_state::load_all(&git_dir).pop().unwrap().sessions,
        vec!["sessionaaaa".to_string(), "sessionbbbb".to_string()],
        "a consecutive duplicate session id is not appended"
    );
}

// Issue #287 test (5): recovery consults ONLY the task record (+ lease), never
// the session log. The crash-skew rows degrade per the display rule while
// recovery is unaffected:
//   - record present, no `TaskOpened` event  -> still recoverable (row 1).
//   - `TaskOpened` present, no record         -> nothing recoverable (row 2).
//   - record removed at settle, no `TaskSettled` yet -> nothing recoverable (row 3).
#[test]
fn recovery_consults_only_record_not_lifecycle_events() {
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();

    // Row 2 baseline: no record at all -> recovery finds nothing (a dangling
    // audit event in a session log could never make a task recoverable, because
    // git-safety never reads the log).
    assert!(GitSafety::new(&repo.path).recoverable_tasks().is_empty());

    // Row 1: a record exists (task opened, then a crash before any settle) with
    // no matching TaskSettled -- recovery lists it from the record alone.
    {
        let guard = GitSafety::new(&repo.path);
        guard.set_session_id("s1".to_string());
        guard.set_turn_context(Some("body".to_string()));
        guard.note_mutation();
        iris_write(&guard, &repo.path.join("committed.txt"), b"x\n");
    }
    assert_eq!(task_state::load_all(&git_dir).len(), 1);
    let guard_b = GitSafety::new(&repo.path);
    assert_eq!(
        guard_b.recoverable_tasks().len(),
        1,
        "the record alone drives recovery"
    );

    // Row 3: settlement removes the record; nothing is recoverable afterward,
    // independent of whether a TaskSettled audit entry was ever appended.
    guard_b.recover_and_expire();
    guard_b.accept().expect("the adopted task settles");
    assert!(
        task_state::load_all(&git_dir).is_empty(),
        "settlement removed the record"
    );
    assert!(
        GitSafety::new(&repo.path).recoverable_tasks().is_empty(),
        "no record -> nothing recoverable (display rule: settled or expired)"
    );
}

// Issue #287 test (6): a legacy record written before body/sessions existed
// deserializes to defaults (None / empty).
#[test]
fn legacy_record_without_body_or_sessions_deserializes_to_defaults() {
    let json = r#"{
        "task_id": "abc",
        "workspace": "/w",
        "created_ms": 1,
        "updated_ms": 2,
        "expected": {},
        "tip_seq": 0
    }"#;
    let record: task_state::PersistedTask =
        serde_json::from_str(json).expect("legacy record deserializes");
    assert!(record.body.is_none(), "missing body defaults to None");
    assert!(
        record.sessions.is_empty(),
        "missing sessions defaults to empty"
    );
}

// Issue #287 test (7): expiry removes the record and its refs but never touches
// the session log (a separate file git-safety has no handle to).
#[test]
fn expiry_removes_record_and_refs_but_leaves_session_log_untouched() {
    let repo = init_repo();
    let session_root = temp_dir();
    let mut log = crate::session::SessionLog::create_in(&session_root.path, &repo.path).unwrap();
    log.append_task_opened("audit-task", Some("audit body"))
        .unwrap();
    log.append_task_settled("audit-task", "accepted").unwrap();
    let session_path = log.path().to_path_buf();
    drop(log);
    let session_before = fs::read(&session_path).unwrap();

    // Open a task, then backdate its record beyond the 30-day expiry window.
    {
        let guard = GitSafety::new(&repo.path);
        guard.set_turn_context(Some("body".to_string()));
        guard.note_mutation();
        iris_write(&guard, &repo.path.join("committed.txt"), b"iris\n");
    }
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let mut record = task_state::load_all(&git_dir).pop().unwrap();
    let task_id = record.task_id.clone();
    record.updated_ms = task_state::now_ms() - (31 * 24 * 60 * 60 * 1000);
    task_state::save(&git_dir, &record).unwrap();
    assert!(
        task_ref_count(&repo.path, &task_id) > 0,
        "refs exist pre-expiry"
    );

    GitSafety::new(&repo.path).recover_and_expire();

    assert!(
        task_state::load_all(&git_dir).is_empty(),
        "expiry removed the record"
    );
    assert_eq!(
        task_ref_count(&repo.path, &task_id),
        0,
        "expiry destroyed the task refs"
    );
    assert_eq!(
        fs::read(&session_path).unwrap(),
        session_before,
        "expiry left the session log byte-for-byte untouched"
    );
}

// --- resume-task picker seam (issue #288, ADR-0031) ----------------------

// #288 test: multiple recoverable records => recovery opens the picker listing
// ALL lease-free rows; adopting the chosen task leaves the others untouched.
#[test]
fn multiple_recoverable_tasks_open_picker_and_adopt_only_chosen() {
    let repo = init_repo();

    let first = create_unsettled_task(&repo.path, "first.txt");
    let second = create_unsettled_task(&repo.path, "second.txt");

    // More than one lease-free orphan: recovery requires explicit selection, so
    // it opens the picker rather than auto-adopting either.
    let guard = GitSafety::new(&repo.path);
    let RecoveryOutcome::Picker(rows) = guard.recover_and_expire() else {
        panic!("more than one recoverable task requires the picker");
    };
    let ids: Vec<&str> = rows.iter().map(|r| r.task_id.as_str()).collect();
    assert!(
        ids.contains(&first.as_str()) && ids.contains(&second.as_str()),
        "the picker lists all lease-free rows: {ids:?}"
    );
    assert!(
        !guard.has_task(),
        "opening the picker never auto-adopts a task"
    );

    // Adopt exactly the chosen task; the other record stays recoverable.
    let adopted = guard.adopt(&second).expect("the chosen task adopts");
    assert_eq!(adopted.task_id, second);
    assert!(guard.has_task(), "the chosen task is now active");
    let remaining: Vec<String> = GitSafety::new(&repo.path)
        .recoverable_tasks()
        .into_iter()
        .map(|r| r.task_id)
        .collect();
    assert!(
        remaining.contains(&first),
        "the unchosen task is untouched and still recoverable: {remaining:?}"
    );
}

// #288 test: a recoverable row surfaces the opaque body + linked-session join so
// the picker can render a body preview and a session count.
#[test]
fn recoverable_task_row_surfaces_body_and_sessions() {
    let repo = init_repo();

    // Session A opens the task with a body and its session id, then crashes.
    {
        let guard = GitSafety::new(&repo.path);
        guard.set_session_id("sessionaaaa".to_string());
        guard.set_turn_context(Some("fix the parser".to_string()));
        guard.note_mutation();
        iris_write(&guard, &repo.path.join("work.txt"), b"iris\n");
    }

    let rows = GitSafety::new(&repo.path).recoverable_tasks();
    let row = rows.first().expect("one recoverable row");
    assert_eq!(
        row.body.as_deref(),
        Some("fix the parser"),
        "the row carries the opaque body for the picker preview"
    );
    assert_eq!(
        row.sessions,
        vec!["sessionaaaa".to_string()],
        "the row carries the linked-session join"
    );
}

// #288 test: adopt-then-settle round trip works post-restart. A crashed task is
// adopted from the picker seam, then `/rollback`-equivalent settlement operates
// on the rehydrated chain and undoes Iris's work.
#[test]
fn adopt_then_settle_round_trip_post_restart() {
    let repo = init_repo();
    let edited = repo.path.join("committed.txt");
    let created = repo.path.join("iris_new.txt");

    // Session A works, task stays unsettled (a crash).
    let task_id = {
        let guard = GitSafety::new(&repo.path);
        guard.note_mutation();
        iris_write(&guard, &edited, b"iris edit\n");
        iris_write(&guard, &created, b"iris new\n");
        let git_dir = task_state::git_dir(&repo.path).unwrap();
        task_state::load_all(&git_dir).pop().unwrap().task_id
    };

    // Session B adopts the orphan explicitly (picker path), then rolls it back.
    let guard_b = GitSafety::new(&repo.path);
    let adopted = guard_b.adopt(&task_id).expect("the orphan adopts");
    assert_eq!(adopted.task_id, task_id);
    let outcome = guard_b.rollback(0).unwrap();
    assert!(
        outcome.summary.contains("rolled back"),
        "settlement operates on the rehydrated chain: {}",
        outcome.summary
    );
    assert_eq!(
        fs::read(&edited).unwrap(),
        b"base\n",
        "the adopted chain rolls back Iris's edit"
    );
    assert!(
        !created.exists(),
        "the adopted chain removes Iris's new file"
    );
}

// #288 test: a legacy record (no body / no sessions) is adoptable from the
// picker seam and surfaces an unknown (None) body + empty session join.
#[test]
fn legacy_record_adopts_with_unknown_body_and_no_sessions() {
    let repo = init_repo();
    let git_dir = task_state::git_dir(&repo.path).unwrap();
    let ws = repo
        .path
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let tasks_dir = git_dir.join("iris").join("tasks");
    fs::create_dir_all(&tasks_dir).unwrap();

    // A pre-ADR-0030/0031 record: no lock metadata, body, or sessions.
    let task_id = "legacyadopt01";
    let now_ms = task_state::now_ms();
    let legacy_json = format!(
        r#"{{"task_id":"{task_id}","workspace":"{ws}","created_ms":{now_ms},"updated_ms":{now_ms},"expected":{{}},"tip_seq":0}}"#
    );
    fs::write(tasks_dir.join(format!("{task_id}.json")), legacy_json).unwrap();

    let guard = GitSafety::new(&repo.path);
    let row = guard
        .recoverable_tasks()
        .into_iter()
        .find(|r| r.task_id == task_id)
        .expect("the legacy row is surfaced");
    assert!(row.body.is_none(), "legacy body is unknown (None)");
    assert!(row.sessions.is_empty(), "legacy session join is empty");

    let adopted = guard.adopt(task_id).expect("the legacy record adopts");
    assert_eq!(adopted.task_id, task_id);
    assert!(adopted.body.is_none(), "adopted legacy body stays unknown");
    assert!(
        adopted.sessions.is_empty(),
        "adopted legacy record has zero linked sessions"
    );
}
