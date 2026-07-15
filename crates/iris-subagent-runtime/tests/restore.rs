#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use iris_subagent_runtime::worktree::{
    RestoreBundle, RestoreEntry, RestoreRequest, RestoreSource, WorktreeCancellation,
    WorktreeConfig, WorktreeFilter, WorktreeService,
};
use iris_subagent_runtime::{HostPayload, RuntimeError};
use rand::random;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("iris-restore-{:032x}", random::<u128>()));
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
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn setup() -> (TestDir, PathBuf, String, WorktreeService) {
    let temp = TestDir::new();
    let repo = temp.0.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    fs::create_dir(repo.join("dir")).unwrap();
    fs::write(repo.join("dir/base.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let service = WorktreeService::open(WorktreeConfig::new(temp.0.join("managed"))).unwrap();
    (temp, repo, head, service)
}

struct Source {
    bundle: RestoreBundle,
    calls: AtomicUsize,
}

impl RestoreSource for Source {
    fn fetch(&self, _request: &RestoreRequest) -> Result<RestoreBundle, RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.bundle.clone())
    }
}

#[test]
fn context_only_restore_falls_back_to_clean_checkout_and_preserves_parent() {
    let (_temp, repo, head, service) = setup();
    let parent = fs::read(repo.join("dir/base.txt")).unwrap();
    let mut context = HostPayload::default();
    context.kind = "local_session".to_string();
    let source = Source {
        bundle: RestoreBundle::context_only("repo-id", &head, context.clone()),
        calls: AtomicUsize::new(0),
    };
    let request = RestoreRequest::trusted_local("session-1", &repo, "repo-id", &head);

    let result = service
        .restore(&request, &source, &WorktreeCancellation::default())
        .unwrap();

    assert!(!result.snapshot_restored);
    assert!(result.fallback_reason.unwrap().contains("clean checkout"));
    assert_eq!(result.session_context, context);
    assert_eq!(result.worktree.session_id.as_deref(), Some("session-1"));
    assert_eq!(
        fs::read(result.worktree.path.join("dir/base.txt")).unwrap(),
        parent
    );
    assert_eq!(fs::read(repo.join("dir/base.txt")).unwrap(), parent);
}

#[test]
fn fake_remote_snapshot_materializes_only_inside_fresh_worktree() {
    let (_temp, repo, head, service) = setup();
    let mut bundle = RestoreBundle::context_only("repo-id", &head, HostPayload::default());
    bundle.snapshot = Some(vec![
        RestoreEntry::file("remote.txt", b"remote bytes".to_vec()),
        RestoreEntry::symlink("remote-link", b"remote.txt".to_vec()),
    ]);
    let source = Source {
        bundle,
        calls: AtomicUsize::new(0),
    };
    let request = RestoreRequest::trusted_local("session-2", &repo, "repo-id", &head);

    let result = service
        .restore(&request, &source, &WorktreeCancellation::default())
        .unwrap();

    assert!(result.snapshot_restored);
    assert_eq!(
        fs::read(result.worktree.path.join("remote.txt")).unwrap(),
        b"remote bytes"
    );
    assert_eq!(
        fs::read_link(result.worktree.path.join("remote-link")).unwrap(),
        PathBuf::from("remote.txt")
    );
    assert!(!repo.join("remote.txt").exists());
}

#[test]
fn restore_rejects_untrusted_identity_and_escaping_entries_before_creation() {
    let (_temp, repo, head, service) = setup();
    let source = Source {
        bundle: RestoreBundle::context_only("wrong", &head, HostPayload::default()),
        calls: AtomicUsize::new(0),
    };
    let mut request = RestoreRequest::trusted_local("session-3", &repo, "repo-id", &head);
    request.trust.trusted = false;
    assert!(
        service
            .restore(&request, &source, &WorktreeCancellation::default())
            .is_err()
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);

    request.trust.trusted = true;
    assert!(
        service
            .restore(&request, &source, &WorktreeCancellation::default())
            .is_err()
    );
    assert!(service.list(&WorktreeFilter::default()).unwrap().is_empty());

    let mut bundle = RestoreBundle::context_only("repo-id", &head, HostPayload::default());
    bundle.snapshot = Some(vec![RestoreEntry::file("../escape", b"no".to_vec())]);
    let source = Source {
        bundle,
        calls: AtomicUsize::new(0),
    };
    assert!(
        service
            .restore(&request, &source, &WorktreeCancellation::default())
            .is_err()
    );
    assert!(service.list(&WorktreeFilter::default()).unwrap().is_empty());
}

#[test]
fn materialization_failure_cleans_up_created_worktree() {
    let (_temp, repo, head, service) = setup();
    let mut bundle = RestoreBundle::context_only("repo-id", &head, HostPayload::default());
    // `dir` is already a directory in the clean checkout; replacing it with a file fails.
    bundle.snapshot = Some(vec![RestoreEntry::file(
        "dir",
        b"cannot replace directory".to_vec(),
    )]);
    let source = Source {
        bundle,
        calls: AtomicUsize::new(0),
    };
    let request = RestoreRequest::trusted_local("session-4", &repo, "repo-id", &head);

    assert!(
        service
            .restore(&request, &source, &WorktreeCancellation::default())
            .is_err()
    );
    assert!(
        service
            .list(&WorktreeFilter::default())
            .unwrap()
            .iter()
            .all(|record| !record.path.exists())
    );
    assert_eq!(
        fs::read_to_string(repo.join("dir/base.txt")).unwrap(),
        "base\n"
    );
}
