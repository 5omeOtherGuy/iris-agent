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
use super::{GitSafety, lock, task_state};
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
    let notice = guard_b
        .recover_and_expire()
        .expect("unsettled task noticed");
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
    guard_b
        .recover_and_expire()
        .expect("unsettled task noticed");

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
    let child = Command::new("flock")
        .args([
            "--no-fork",
            "-x",
            "-n",
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

    // Recovery never auto-adopts it: the notice lists it, no task becomes active.
    let notice = guard.recover_and_expire().expect("a notice surfaces");
    assert!(
        notice.contains(task_id) && notice.contains("unknown"),
        "the notice lists the unknown-legacy task id: {notice}"
    );
    assert!(
        !guard.has_task(),
        "a legacy record is never auto-adopted as the active task"
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

    // Release the lock ~400ms from now, on another thread.
    let hold = Duration::from_millis(400);
    let killer = std::thread::spawn(move || {
        std::thread::sleep(hold);
        let _ = holder.kill();
        let _ = holder.wait();
    });

    let guard = GitSafety::new(&repo.path);
    let start = Instant::now();
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
    let notice = guard.recover_and_expire().expect("the orphan is recovered");

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
