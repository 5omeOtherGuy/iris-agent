use std::path::Path;
use std::process::Command;
use std::time::Instant;

use iris_subagent_runtime::worktree::{
    CreationMode, RemoveOptions, StrategyPreference, WorktreeCancellation, WorktreeConfig,
    WorktreeCreateRequest, WorktreeService,
};
use rand::random;

#[test]
fn real_btrfs_snapshot_timing_or_honest_skip() {
    if !cfg!(target_os = "linux") {
        eprintln!("SKIP btrfs timing: direct Btrfs snapshots are Linux-only");
        return;
    }
    let temp = std::env::temp_dir().join(format!("iris-btrfs-timing-{:032x}", random::<u128>()));
    std::fs::create_dir_all(&temp).unwrap();
    let fs_type = Command::new("stat")
        .args(["-f", "-c", "%T", temp.to_str().unwrap()])
        .output();
    if !fs_type.is_ok_and(|output| output.status.success() && output.stdout.starts_with(b"btrfs")) {
        eprintln!("SKIP btrfs timing: temporary storage is not Btrfs");
        std::fs::remove_dir_all(temp).unwrap();
        return;
    }
    if !Command::new("btrfs")
        .arg("version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        eprintln!("SKIP btrfs timing: btrfs tooling is unavailable");
        std::fs::remove_dir_all(temp).unwrap();
        return;
    }

    let source = temp.join("source");
    let created = Command::new("btrfs")
        .args(["subvolume", "create", source.to_str().unwrap()])
        .output();
    if !created.is_ok_and(|output| output.status.success()) {
        eprintln!("SKIP btrfs timing: insufficient privilege to create a test subvolume");
        std::fs::remove_dir_all(temp).unwrap();
        return;
    }
    git(&source, &["init", "-q"]);
    git(&source, &["config", "user.email", "test@example.com"]);
    git(&source, &["config", "user.name", "Test"]);
    std::fs::write(source.join("file"), "content\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-qm", "base"]);
    let service = WorktreeService::open(WorktreeConfig::new(temp.join("managed"))).unwrap();
    let mut request = WorktreeCreateRequest::worker(&source);
    request.strategy = StrategyPreference::BtrfsPreferred;
    let start = Instant::now();
    let record = service
        .create(request, &WorktreeCancellation::default())
        .unwrap();
    let elapsed = start.elapsed();
    assert_eq!(record.creation_mode, CreationMode::BtrfsSnapshot);
    eprintln!("Btrfs snapshot creation: {} us", elapsed.as_micros());
    service
        .remove(
            &record.id,
            RemoveOptions::force(),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    let mut linked_request = WorktreeCreateRequest::worker(&source);
    linked_request.strategy = StrategyPreference::Linked;
    let linked_start = Instant::now();
    let linked = service
        .create(linked_request, &WorktreeCancellation::default())
        .unwrap();
    let linked_elapsed = linked_start.elapsed();
    assert_eq!(linked.creation_mode, CreationMode::Linked);
    eprintln!(
        "Btrfs snapshot: {} us; linked worktree: {} us; delta: {} us",
        elapsed.as_micros(),
        linked_elapsed.as_micros(),
        linked_elapsed.as_micros() as i128 - elapsed.as_micros() as i128
    );
    service
        .remove(
            &linked.id,
            RemoveOptions::force(),
            &WorktreeCancellation::default(),
        )
        .unwrap();
    let _ = Command::new("btrfs")
        .args(["subvolume", "delete", source.to_str().unwrap()])
        .status();
    let _ = std::fs::remove_dir_all(temp);
}

fn git(cwd: &Path, args: &[&str]) {
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
}
