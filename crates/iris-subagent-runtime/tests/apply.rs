#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use iris_subagent_runtime::worktree::{
    ApplyConflictKind, ApplyDisposition, ApplyOptions, MutationEntry, MutationManifest,
    RemoveOptions, WorktreeCancellation, WorktreeConfig, WorktreeCreateRequest, WorktreeService,
};
use rand::random;

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("iris-apply-{label}-{:032x}", random::<u128>()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn setup() -> (TestDir, PathBuf, WorktreeService) {
    let temp = TestDir::new("repo");
    let repo = temp.0.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("edit.txt"), "base edit\n").unwrap();
    fs::write(repo.join("delete.txt"), "delete me\n").unwrap();
    fs::write(repo.join("rename.txt"), "rename me\n").unwrap();
    fs::write(repo.join("binary.bin"), [0_u8, 1, 2, 255]).unwrap();
    fs::write(repo.join("script.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let service = WorktreeService::open(WorktreeConfig::new(temp.0.join("managed"))).unwrap();
    (temp, repo, service)
}

fn child(
    service: &WorktreeService,
    repo: &Path,
) -> iris_subagent_runtime::worktree::WorktreeRecord {
    service
        .create(
            WorktreeCreateRequest::worker(repo),
            &WorktreeCancellation::default(),
        )
        .unwrap()
}

#[test]
fn apply_handles_files_binary_delete_rename_chmod_symlink_without_touching_git_state() {
    let (_temp, repo, service) = setup();
    let record = child(&service, &repo);
    fs::write(record.path.join("edit.txt"), "child edit\n").unwrap();
    fs::write(record.path.join("create.txt"), "created\n").unwrap();
    fs::remove_file(record.path.join("delete.txt")).unwrap();
    fs::rename(
        record.path.join("rename.txt"),
        record.path.join("renamed.txt"),
    )
    .unwrap();
    fs::write(record.path.join("binary.bin"), [0_u8, 9, 0, 255]).unwrap();
    let mut permissions = fs::metadata(record.path.join("script.sh"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(record.path.join("script.sh"), permissions).unwrap();
    symlink("edit.txt", record.path.join("link")).unwrap();
    let manifest = MutationManifest::new(vec![
        MutationEntry::path("edit.txt"),
        MutationEntry::path("create.txt"),
        MutationEntry::path("delete.txt"),
        MutationEntry::rename("rename.txt", "renamed.txt"),
        MutationEntry::path("binary.bin"),
        MutationEntry::path("script.sh"),
        MutationEntry::path("link"),
    ]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let index = git(&repo, &["ls-files", "--stage"]);

    let plan = service
        .plan_apply(&record.id, &manifest, &WorktreeCancellation::default())
        .unwrap();
    let result = service
        .apply(
            &plan,
            &ApplyOptions::new(),
            &WorktreeCancellation::default(),
        )
        .unwrap();

    assert_eq!(result.disposition, ApplyDisposition::Complete);
    assert_eq!(
        fs::read_to_string(repo.join("edit.txt")).unwrap(),
        "child edit\n"
    );
    assert_eq!(
        fs::read_to_string(repo.join("create.txt")).unwrap(),
        "created\n"
    );
    assert!(!repo.join("delete.txt").exists());
    assert!(!repo.join("rename.txt").exists());
    assert_eq!(
        fs::read_to_string(repo.join("renamed.txt")).unwrap(),
        "rename me\n"
    );
    assert_eq!(
        fs::read(repo.join("binary.bin")).unwrap(),
        [0_u8, 9, 0, 255]
    );
    assert_ne!(
        fs::metadata(repo.join("script.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(
        fs::read_link(repo.join("link")).unwrap(),
        PathBuf::from("edit.txt")
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&repo, &["ls-files", "--stage"]), index);
    assert_eq!(
        service
            .apply(
                &plan,
                &ApplyOptions::new(),
                &WorktreeCancellation::default()
            )
            .unwrap()
            .disposition,
        ApplyDisposition::AlreadyApplied
    );
    fs::write(repo.join("edit.txt"), "post apply drift\n").unwrap();
    assert!(
        service
            .apply(
                &plan,
                &ApplyOptions::new(),
                &WorktreeCancellation::default()
            )
            .is_err()
    );

    service
        .remove(
            &record.id,
            RemoveOptions::force(),
            &WorktreeCancellation::default(),
        )
        .unwrap();
}

#[test]
fn dirty_parent_and_committed_base_drift_require_per_file_approval() {
    let (_temp, repo, service) = setup();
    let record = child(&service, &repo);
    fs::write(record.path.join("edit.txt"), "child\n").unwrap();
    fs::write(repo.join("edit.txt"), "user dirty\n").unwrap();
    let manifest = MutationManifest::new(vec![MutationEntry::path("edit.txt")]);
    let dirty_plan = service
        .plan_apply(&record.id, &manifest, &WorktreeCancellation::default())
        .unwrap();

    let partial = service
        .apply(
            &dirty_plan,
            &ApplyOptions::new(),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    assert_eq!(partial.disposition, ApplyDisposition::Partial);
    assert_eq!(partial.conflicts[0].kind, ApplyConflictKind::DirtyParent);
    assert_eq!(
        fs::read_to_string(repo.join("edit.txt")).unwrap(),
        "user dirty\n"
    );

    let mut approved = ApplyOptions::new();
    approved
        .approved_overwrites
        .insert(PathBuf::from("edit.txt"));
    let complete = service
        .apply(&dirty_plan, &approved, &WorktreeCancellation::default())
        .unwrap();
    assert_eq!(complete.disposition, ApplyDisposition::Complete);

    // A second candidate created at the old base conflicts after parent commits drift.
    let second = child(&service, &repo);
    fs::write(second.path.join("delete.txt"), "child delete replacement\n").unwrap();
    fs::write(repo.join("delete.txt"), "committed parent drift\n").unwrap();
    git(&repo, &["add", "delete.txt"]);
    git(&repo, &["commit", "-qm", "parent drift"]);
    let drift_plan = service
        .plan_apply(
            &second.id,
            &MutationManifest::new(vec![MutationEntry::path("delete.txt")]),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    assert!(drift_plan.operations[0].base_drift);
    let partial = service
        .apply(
            &drift_plan,
            &ApplyOptions::new(),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    assert_eq!(partial.conflicts[0].kind, ApplyConflictKind::BaseDrift);
    assert_eq!(
        fs::read_to_string(repo.join("delete.txt")).unwrap(),
        "committed parent drift\n"
    );
}

#[test]
fn apply_revalidates_child_and_parent_bytes_after_review() {
    let (_temp, repo, service) = setup();
    let record = child(&service, &repo);
    fs::write(record.path.join("edit.txt"), "reviewed\n").unwrap();
    let plan = service
        .plan_apply(
            &record.id,
            &MutationManifest::new(vec![MutationEntry::path("edit.txt")]),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    fs::write(record.path.join("edit.txt"), "forged after review\n").unwrap();

    assert!(
        service
            .apply(
                &plan,
                &ApplyOptions::new(),
                &WorktreeCancellation::default()
            )
            .unwrap_err()
            .to_string()
            .contains("child bytes changed")
    );
    assert_eq!(
        fs::read_to_string(repo.join("edit.txt")).unwrap(),
        "base edit\n"
    );
}

#[test]
fn escaping_symlink_requires_separate_explicit_approval() {
    let (_temp, repo, service) = setup();
    let record = child(&service, &repo);
    symlink("../../outside", record.path.join("escape-link")).unwrap();
    let plan = service
        .plan_apply(
            &record.id,
            &MutationManifest::new(vec![MutationEntry::path("escape-link")]),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    assert!(plan.operations[0].escaping_symlink);

    let partial = service
        .apply(
            &plan,
            &ApplyOptions::new(),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    assert_eq!(
        partial.conflicts[0].kind,
        ApplyConflictKind::EscapingSymlink
    );
    assert!(!repo.join("escape-link").exists());

    let mut options = ApplyOptions::new();
    options.approved_escaping_symlinks = BTreeSet::from([PathBuf::from("escape-link")]);
    let complete = service
        .apply(&plan, &options, &WorktreeCancellation::default())
        .unwrap();
    assert_eq!(complete.disposition, ApplyDisposition::Complete);
    assert_eq!(
        fs::read_link(repo.join("escape-link")).unwrap(),
        PathBuf::from("../../outside")
    );
}

#[test]
fn gitlink_changes_remain_reviewable_but_are_never_applied() {
    let (_temp, repo, service) = setup();
    let commit = git(&repo, &["rev-parse", "HEAD"]);
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            &commit,
            "dependency",
        ],
    );
    git(&repo, &["commit", "-qm", "add gitlink"]);
    let record = child(&service, &repo);
    if record.path.join("dependency").exists() {
        fs::remove_dir_all(record.path.join("dependency")).unwrap();
    }
    let plan = service
        .plan_apply(
            &record.id,
            &MutationManifest::new(vec![MutationEntry::path("dependency")]),
            &WorktreeCancellation::default(),
        )
        .unwrap();

    let result = service
        .apply(
            &plan,
            &ApplyOptions::new(),
            &WorktreeCancellation::default(),
        )
        .unwrap();

    assert_eq!(result.disposition, ApplyDisposition::Partial);
    assert_eq!(result.conflicts[0].kind, ApplyConflictKind::Gitlink);
    assert!(git(&repo, &["ls-files", "--stage", "dependency"]).starts_with("160000 "));
}

#[test]
fn manifest_path_escape_is_rejected_before_review_or_write() {
    let (_temp, repo, service) = setup();
    let record = child(&service, &repo);

    let error = service
        .plan_apply(
            &record.id,
            &MutationManifest::new(vec![MutationEntry::path("../outside")]),
            &WorktreeCancellation::default(),
        )
        .unwrap_err();

    assert!(error.to_string().contains("workspace-relative"));
    assert_eq!(
        fs::read_to_string(repo.join("edit.txt")).unwrap(),
        "base edit\n"
    );
}
