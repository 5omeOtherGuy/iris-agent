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
use super::{GitSafety, task_state};
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
    chain.rollback_to(0).unwrap();
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

// Test 5: settlement GC keeps the last N intermediate checkpoints (plus base)
// and never touches a foreign ref (a branch, or another task's namespace).
#[test]
fn engine_gc_keeps_last_n_and_spares_foreign_refs() {
    let repo = init_repo();
    let root = repo.path.clone();
    let file = root.join("committed.txt");

    // A foreign branch and a foreign task's checkpoint ref, both must survive GC.
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

    chain.gc(3).unwrap();
    // base + 3 kept intermediates.
    assert_eq!(task_ref_count(&root, "gc-task"), 4);
    assert_eq!(chain.len(), 3);

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
    let notice = guard_b
        .recover_and_expire()
        .expect("unsettled task noticed");
    assert!(notice.contains("unsettled"), "notice: {notice}");
    assert!(
        task_ref_count(&repo.path, &task_id) > refs_before,
        "a recovery checkpoint is appended to the chain"
    );
}

// Test 7: an unsettled task past the expiry window auto-settles as accepted and
// its checkpoint refs are GC'd (no rollback offered for code the user has lived
// with).
#[test]
fn expired_task_auto_settles_accepted_and_gcs_refs() {
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
    let notice = guard.recover_and_expire();
    assert!(
        notice.is_none(),
        "an expired task surfaces no recovery notice"
    );
    assert_eq!(
        task_ref_count(&repo.path, &task_id),
        0,
        "refs GC'd on expiry"
    );
    assert!(task_state::load_all(&git_dir).is_empty(), "record removed");
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
