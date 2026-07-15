use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use iris_subagent_runtime::worktree::{
    CreationMode, ProcessLiveness, ProcessOutput, ProcessRunner, ProcessSpec, RemoveOptions,
    RemoveOutcome, StrategyPreference, SystemProcessRunner, WorktreeCancellation, WorktreeConfig,
    WorktreeCreateRequest, WorktreeFilter, WorktreeService, WorktreeStatus,
};
use iris_subagent_runtime::{GroupId, RuntimeError};
use rand::random;

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("iris-worktree-{label}-{:032x}", random::<u128>()));
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

fn repo(root: &Path) -> PathBuf {
    fs::create_dir_all(root).unwrap();
    let repo = root.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("tracked.txt"), "base\n").unwrap();
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-qm", "base"]);
    repo
}

fn service(root: &Path) -> WorktreeService {
    WorktreeService::open(WorktreeConfig::new(root)).unwrap()
}

#[test]
fn linked_lifecycle_is_detached_durable_rebuildable_and_parent_immutable() {
    let temp = TestDir::new("lifecycle");
    let repo = repo(&temp.0);
    let managed = temp.0.join("managed");
    let parent_before = fs::read(repo.join("tracked.txt")).unwrap();
    let head_before = git(&repo, &["rev-parse", "HEAD"]);
    let service = service(&managed);

    let record = service
        .create(
            WorktreeCreateRequest::worker(&repo),
            &WorktreeCancellation::default(),
        )
        .unwrap();

    assert_eq!(record.creation_mode, CreationMode::Linked);
    assert_eq!(record.base_commit, head_before);
    assert!(
        !Command::new("git")
            .args(["symbolic-ref", "-q", "HEAD"])
            .current_dir(&record.path)
            .status()
            .unwrap()
            .success()
    );
    assert!(!record.path.join(".iris-worktree").exists());
    assert_eq!(fs::read(repo.join("tracked.txt")).unwrap(), parent_before);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), record.base_commit);
    assert_eq!(service.list(&WorktreeFilter::default()).unwrap().len(), 1);
    assert_eq!(service.show(&record.id).unwrap(), record);

    fs::remove_file(managed.join("registry.jsonl")).unwrap();
    fs::write(managed.join("registry.jsonl"), b"").unwrap();
    let rebuilt = service.rebuild(&WorktreeCancellation::default()).unwrap();
    assert_eq!(rebuilt, vec![record.clone()]);

    let outcome = service
        .remove(
            &record.id,
            RemoveOptions::dry_run_force(),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    assert_eq!(outcome, RemoveOutcome::WouldRemove(record.path.clone()));
    assert!(record.path.exists());
    service
        .remove(
            &record.id,
            RemoveOptions::force(),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    assert!(!record.path.exists());
    assert_eq!(
        service.show(&record.id).unwrap().status,
        WorktreeStatus::Removed
    );
}

#[test]
fn create_rejects_non_git_bare_and_jj_without_records() {
    let temp = TestDir::new("invalid");
    let managed = temp.0.join("managed");
    let service = service(&managed);
    let plain = temp.0.join("plain");
    fs::create_dir(&plain).unwrap();
    assert!(
        service
            .create(
                WorktreeCreateRequest::worker(&plain),
                &WorktreeCancellation::default()
            )
            .is_err()
    );

    let bare = temp.0.join("bare.git");
    fs::create_dir(&bare).unwrap();
    git(&bare, &["init", "--bare", "-q"]);
    assert!(
        service
            .create(
                WorktreeCreateRequest::worker(&bare),
                &WorktreeCancellation::default()
            )
            .is_err()
    );

    let valid = repo(&temp.0.join("other"));
    fs::create_dir(valid.join(".jj")).unwrap();
    let error = service
        .create(
            WorktreeCreateRequest::worker(&valid),
            &WorktreeCancellation::default(),
        )
        .unwrap_err();
    assert!(matches!(error, RuntimeError::UnsupportedWorkspace(_)));
    assert!(service.list(&WorktreeFilter::default()).unwrap().is_empty());
}

#[test]
fn removal_refuses_missing_marker_and_symlink_escape() {
    let temp = TestDir::new("guards");
    let repo = repo(&temp.0);
    let managed = temp.0.join("managed");
    let service = service(&managed);
    let record = service
        .create(
            WorktreeCreateRequest::worker(&repo),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    fs::remove_file(
        managed
            .join("control")
            .join(format!("{}.json", record.id.as_str())),
    )
    .unwrap();

    assert!(
        service
            .remove(
                &record.id,
                RemoveOptions::force(),
                &WorktreeCancellation::default()
            )
            .is_err()
    );
    assert!(record.path.exists());

    // Cleanup through git after intentionally corrupting service metadata.
    git(
        &repo,
        &[
            "worktree",
            "remove",
            "--force",
            record.path.to_str().unwrap(),
        ],
    );
}

struct Dead;

impl ProcessLiveness for Dead {
    fn is_alive(&self, _pid: u32) -> bool {
        false
    }
}

#[test]
fn dead_owner_becomes_adoptable_and_requires_explicit_adoption() {
    let temp = TestDir::new("adopt");
    let repo = repo(&temp.0);
    let managed = temp.0.join("managed");
    let first = service(&managed);
    let record = first
        .create(
            WorktreeCreateRequest::worker(&repo),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    drop(first);

    let second = WorktreeService::with_ports(
        WorktreeConfig::new(&managed),
        Arc::new(SystemProcessRunner),
        Arc::new(Dead),
    )
    .unwrap();
    let report = second
        .gc(RemoveOptions::default(), &WorktreeCancellation::default())
        .unwrap();

    assert_eq!(report.adoptable, vec![record.id.clone()]);
    assert!(report.prune_suppressed);
    assert!(record.path.exists());
    let adopted = second
        .adopt(&record.id, &WorktreeCancellation::default())
        .unwrap();
    assert_eq!(adopted.status, WorktreeStatus::Alive);
    assert_eq!(adopted.owner_instance_id, *second.instance_id());
    second
        .remove(
            &record.id,
            RemoveOptions::force(),
            &WorktreeCancellation::default(),
        )
        .unwrap();
}

struct Alive;

impl ProcessLiveness for Alive {
    fn is_alive(&self, _pid: u32) -> bool {
        true
    }
}

#[test]
fn group_candidate_selection_is_durable_and_replaceable_before_apply() {
    let temp = TestDir::new("selection");
    let repo = repo(&temp.0);
    let service = service(&temp.0.join("managed"));
    let group_id = GroupId::new();
    let mut first_request = WorktreeCreateRequest::worker(&repo);
    first_request.group_id = Some(group_id.clone());
    let first = service
        .create(first_request, &WorktreeCancellation::default())
        .unwrap();
    let mut second_request = WorktreeCreateRequest::worker(&repo);
    second_request.group_id = Some(group_id);
    let second = service
        .create(second_request, &WorktreeCancellation::default())
        .unwrap();

    assert!(service.select_group_candidate(&first.id).unwrap().selected);
    assert!(service.select_group_candidate(&second.id).unwrap().selected);
    assert!(!service.show(&first.id).unwrap().selected);
    assert!(service.show(&second.id).unwrap().selected);

    for record in [first, second] {
        service
            .remove(
                &record.id,
                RemoveOptions::force(),
                &WorktreeCancellation::default(),
            )
            .unwrap();
    }
}

#[test]
fn reused_live_pid_without_the_instance_lease_is_not_trusted() {
    let temp = TestDir::new("pid-reuse");
    let repo = repo(&temp.0);
    let managed = temp.0.join("managed");
    let first = service(&managed);
    let record = first
        .create(
            WorktreeCreateRequest::worker(&repo),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    drop(first);

    let second = WorktreeService::with_ports(
        WorktreeConfig::new(&managed),
        Arc::new(SystemProcessRunner),
        Arc::new(Alive),
    )
    .unwrap();
    let report = second
        .gc(RemoveOptions::default(), &WorktreeCancellation::default())
        .unwrap();
    assert_eq!(report.adoptable, vec![record.id.clone()]);
    second
        .remove(
            &record.id,
            RemoveOptions::force(),
            &WorktreeCancellation::default(),
        )
        .unwrap();
}

#[test]
fn only_pristine_candidates_return_to_pool() {
    let temp = TestDir::new("pool");
    let repo = repo(&temp.0);
    let managed = temp.0.join("managed");
    let service = service(&managed);
    let record = service
        .prewarm(&repo, 1, &WorktreeCancellation::default())
        .unwrap()
        .pop()
        .unwrap();
    let acquired = service
        .acquire_pooled(&repo, &WorktreeCancellation::default())
        .unwrap()
        .unwrap();
    assert_eq!(acquired.id, record.id);
    fs::write(acquired.path.join("secret.tmp"), "do not leak").unwrap();
    assert!(
        service
            .release_to_pool(&acquired.id, &WorktreeCancellation::default())
            .is_err()
    );
    assert!(acquired.path.join("secret.tmp").exists());
    service
        .remove(
            &acquired.id,
            RemoveOptions::force(),
            &WorktreeCancellation::default(),
        )
        .unwrap();
}

#[derive(Default)]
struct FakeBtrfs {
    system: SystemProcessRunner,
    advance_source_before_snapshot: bool,
}

impl ProcessRunner for FakeBtrfs {
    fn run(
        &self,
        spec: &ProcessSpec,
        cancellation: &WorktreeCancellation,
    ) -> Result<ProcessOutput, RuntimeError> {
        if spec.program != "btrfs" {
            return self.system.run(spec, cancellation);
        }
        match spec.args.as_slice() {
            [subvolume, show, _] if subvolume == "subvolume" && show == "show" => {
                Ok(ProcessOutput::success(Vec::new()))
            }
            [subvolume, snapshot, source, destination]
                if subvolume == "subvolume" && snapshot == "snapshot" =>
            {
                if self.advance_source_before_snapshot {
                    let source = Path::new(source);
                    fs::write(source.join("tracked.txt"), "raced\n").unwrap();
                    git(source, &["add", "tracked.txt"]);
                    git(source, &["commit", "-qm", "raced"]);
                }
                copy_tree(Path::new(source), Path::new(destination));
                Ok(ProcessOutput::success(Vec::new()))
            }
            [subvolume, delete, destination] if subvolume == "subvolume" && delete == "delete" => {
                fs::remove_dir_all(destination).unwrap();
                Ok(ProcessOutput::success(Vec::new()))
            }
            _ => panic!("unexpected btrfs command: {:?}", spec.args),
        }
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn btrfs_preferred_non_head_base_falls_back_to_exact_linked_worktree() {
    let temp = TestDir::new("btrfs-base");
    let repo = repo(&temp.0);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(repo.join("tracked.txt"), "head\n").unwrap();
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-qm", "head"]);
    let managed = temp.0.join("managed");
    let service = WorktreeService::with_ports(
        WorktreeConfig::new(&managed),
        Arc::new(FakeBtrfs::default()),
        Arc::new(Dead),
    )
    .unwrap();
    let mut request = WorktreeCreateRequest::worker(&repo);
    request.base = Some(base.clone());
    request.strategy = StrategyPreference::BtrfsPreferred;

    let record = service
        .create(request, &WorktreeCancellation::default())
        .unwrap();

    assert_eq!(record.creation_mode, CreationMode::Linked);
    assert_eq!(record.base_commit, base);
    assert_eq!(git(&record.path, &["rev-parse", "HEAD"]), base);
    assert_eq!(
        fs::read_to_string(record.path.join("tracked.txt")).unwrap(),
        "base\n"
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
fn btrfs_snapshot_race_falls_back_to_the_resolved_base() {
    let temp = TestDir::new("btrfs-race");
    let repo = repo(&temp.0);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let managed = temp.0.join("managed");
    let service = WorktreeService::with_ports(
        WorktreeConfig::new(&managed),
        Arc::new(FakeBtrfs {
            advance_source_before_snapshot: true,
            ..FakeBtrfs::default()
        }),
        Arc::new(Dead),
    )
    .unwrap();
    let mut request = WorktreeCreateRequest::worker(&repo);
    request.strategy = StrategyPreference::BtrfsPreferred;

    let record = service
        .create(request, &WorktreeCancellation::default())
        .unwrap();

    assert_eq!(record.creation_mode, CreationMode::Linked);
    assert_eq!(record.base_commit, base);
    assert_eq!(git(&record.path, &["rev-parse", "HEAD"]), base);
    assert_eq!(
        fs::read_to_string(record.path.join("tracked.txt")).unwrap(),
        "base\n"
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
fn deterministic_btrfs_strategy_records_and_matches_deletion_backend() {
    let temp = TestDir::new("btrfs");
    let repo = repo(&temp.0);
    let managed = temp.0.join("managed");
    let service = WorktreeService::with_ports(
        WorktreeConfig::new(&managed),
        Arc::new(FakeBtrfs::default()),
        Arc::new(Dead),
    )
    .unwrap();
    let mut request = WorktreeCreateRequest::worker(&repo);
    request.strategy = StrategyPreference::BtrfsPreferred;

    let record = service
        .create(request, &WorktreeCancellation::default())
        .unwrap();

    assert_eq!(record.creation_mode, CreationMode::BtrfsSnapshot);
    assert!(record.path.join(".git/iris-subagent-runtime.json").exists());
    service
        .remove(
            &record.id,
            RemoveOptions::force(),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    assert!(!record.path.exists());
}
