//! Hardened `jj` subprocess helpers for jj-backed dirty-tree safety.
//!
//! The guard uses jj as the working-copy authority when a workspace is managed
//! by Jujutsu. Commands here are intentionally narrow: detect the workspace,
//! force jj's normal working-copy snapshot, read status/op ids, and restore to a
//! recorded operation during rollback.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};

use super::baseline::Baseline;
use super::snapshot::hash_file;

#[derive(Debug, Clone)]
pub(super) struct Workspace {
    pub(super) state_dir: PathBuf,
}

pub(super) fn detect(workspace: &Path) -> Option<Workspace> {
    let output = jj(workspace, &["root"]).ok()?;
    if !output.status.success() {
        return None;
    }
    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if root.as_os_str().is_empty() {
        return None;
    }
    let root = root.canonicalize().unwrap_or(root);
    // Native integration relies on operation-window reads. A `jj` that can find
    // the workspace but cannot perform this non-snapshotting query is not
    // compatible and must remain in file-only degraded mode.
    current_operation_id(&root).ok()?;
    let state_dir = root.join(".jj").join("iris");
    Some(Workspace { state_dir })
}

pub(super) fn jj(workspace: &Path, args: &[&str]) -> Result<Output> {
    Command::new("jj")
        .args(["--no-pager", "--color", "never"])
        .args(args)
        .current_dir(workspace)
        .env("JJ_USER", "Iris")
        .env("JJ_EMAIL", "iris@example.invalid")
        .output()
        .context("failed to spawn jj subprocess")
}

fn jj_stdout(workspace: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = jj(workspace, args)?;
    if !output.status.success() {
        bail!(
            "jj {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

/// Force jj to observe the current working-copy state using its documented
/// snapshot path. `jj status` snapshots first, auto-tracking new non-ignored
/// files according to `snapshot.auto-track`; ignored files are never tracked
/// automatically.
pub(super) fn snapshot(workspace: &Path) -> Result<Vec<u8>> {
    jj_stdout(workspace, &["status"])
}

/// Read the current operation without snapshotting a dirty working copy.
pub(super) fn current_operation_id(workspace: &Path) -> Result<String> {
    let templated = jj_stdout(
        workspace,
        &[
            "--ignore-working-copy",
            "op",
            "log",
            "--limit",
            "1",
            "--no-graph",
            "-T",
            "id ++ \"\\n\"",
        ],
    );
    if let Ok(out) = templated {
        let id = String::from_utf8_lossy(&out).trim().to_string();
        if !id.is_empty() {
            return Ok(id);
        }
    }
    let out = jj_stdout(
        workspace,
        &[
            "--ignore-working-copy",
            "op",
            "log",
            "--limit",
            "1",
            "--no-graph",
        ],
    )?;
    let text = String::from_utf8_lossy(&out);
    text.split_whitespace()
        .find(|token| token.chars().all(|c| c.is_ascii_hexdigit()) && token.len() >= 8)
        .map(str::to_string)
        .context("could not parse jj operation id")
}

pub(super) fn restore_operation(workspace: &Path, operation: &str) -> Result<()> {
    jj_stdout(workspace, &["op", "restore", operation]).map(|_| ())
}

pub(super) fn capture_baseline(
    workspace: &Path,
    normalize: impl Fn(&Path) -> PathBuf,
) -> Result<Baseline> {
    let status = snapshot(workspace)?;
    let mut protected = std::collections::BTreeMap::new();
    let mut dirty_count = 0;
    let mut untracked_count = 0;
    for entry in parse_status(&String::from_utf8_lossy(&status)) {
        if entry.untracked {
            untracked_count += 1;
        } else {
            dirty_count += 1;
        }
        let abs = normalize(&entry.path);
        protected.insert(abs.clone(), hash_file(&abs));
    }
    Ok(Baseline {
        protected,
        dirty_count,
        untracked_count,
        index: String::new(),
    })
}

struct StatusEntry {
    path: PathBuf,
    untracked: bool,
}

fn parse_status(status: &str) -> Vec<StatusEntry> {
    let mut entries = Vec::new();
    for raw in status.lines() {
        let line = raw.trim_start();
        if line.len() < 2 {
            continue;
        }
        let code = &line[..1];
        if !matches!(code, "A" | "M" | "D" | "R" | "C" | "?") {
            continue;
        }
        let rest = line[1..].trim();
        if rest.is_empty() {
            continue;
        }
        let path = rest.split(" -> ").last().unwrap_or(rest);
        entries.push(StatusEntry {
            path: PathBuf::from(path),
            untracked: code == "?",
        });
    }
    entries
}
