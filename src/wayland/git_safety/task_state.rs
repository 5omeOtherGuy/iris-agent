//! Task-state persistence, crash recovery, and expiry (issue #263, ADR-0028).
//!
//! The checkpoint chain itself is durable in git refs, but crash recovery and
//! the 30-day expiry sweep need a small record of the *unsettled* task that
//! outlives a process: its id, when it was last active, and the op-log's
//! expected on-disk state so a later session can detect divergence (a crash, a
//! `^C`, or an external edit between the last checkpoint and resume).
//!
//! Records live in `<git-dir>/iris/tasks/<task-id>.json` -- repo-scoped like the
//! `refs/iris/*` chain they describe, so a *new* session in the same repo finds
//! the unsettled task (recovery is per-repo, not per-session). This mirrors the
//! session store's "state beside the thing it belongs to" pattern
//! (`src/handles.rs`, `src/session.rs`); the git dir is the natural sibling
//! since the refs already live there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::git::git_stdout;

/// Default unsettled-task expiry window (ADR-0028: 30 days). An unsettled task
/// untouched this long auto-settles as *accepted* -- by then its changes are the
/// user's de facto working state, so expiring toward rollback would revert code
/// they have lived with.
pub(super) const DEFAULT_EXPIRY: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Persisted record of one unsettled task. Paths are stored as lossy strings for
/// the recovery/expiry bookkeeping only; the git refs hold exact path bytes, so
/// a non-UTF-8 path degrades recovery divergence detection for that entry, never
/// the byte-exact rollback itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PersistedTask {
    pub(super) task_id: String,
    /// Canonicalized workspace root, so a repo-dir scan can rebuild absolute
    /// ledger paths.
    pub(super) workspace: String,
    pub(super) created_ms: u64,
    pub(super) updated_ms: u64,
    /// Op-log's expected on-disk content hash per ledger path (`None` = the path
    /// should be absent). Divergence = any path whose current hash differs.
    pub(super) expected: BTreeMap<String, Option<String>>,
    /// Highest checkpoint sequence recorded, so recovery can append after it.
    pub(super) tip_seq: u64,
    /// The task baseline's `git ls-files --stage` output, so a post-restart
    /// rollback can restore the user's staged selection (ADR-0028: the index is
    /// protected state). `#[serde(default)]` so a record written before this
    /// field existed deserializes to an empty index (rollback then leaves
    /// staging untouched rather than failing).
    #[serde(default)]
    pub(super) baseline_index: String,
    /// Opaque id of the process that last wrote this record (ADR-0030). Purely
    /// informational -- liveness/ownership is proven by the per-task `flock`
    /// lease, not this field. `#[serde(default)]` so a legacy record (written
    /// before the lease protocol) deserializes to `None`.
    #[serde(default)]
    pub(super) owner: Option<String>,
    /// The lock protocol this record was written under (e.g. `"flock-v1"`).
    /// `None` marks a legacy record that predates the lease protocol: recovery
    /// classifies it as "unknown" and never auto-adopts it (ADR-0030).
    #[serde(default)]
    pub(super) lock_protocol: Option<String>,
}

impl PersistedTask {
    /// Whether this task is past the expiry window relative to `now`.
    pub(super) fn is_expired(&self, now: SystemTime, window: Duration) -> bool {
        let updated = UNIX_EPOCH + Duration::from_millis(self.updated_ms);
        now.duration_since(updated)
            .map(|age| age >= window)
            .unwrap_or(false)
    }

    /// A one-line "unsettled diff from <when>" recovery notice (ADR-0028).
    pub(super) fn recovery_notice(&self, now: SystemTime) -> String {
        let updated = UNIX_EPOCH + Duration::from_millis(self.updated_ms);
        let age = now
            .duration_since(updated)
            .map(human_age)
            .unwrap_or_else(|_| "moments".to_string());
        format!(
            "unsettled Iris changes from {age} ago -- view / accept / roll back / ignore (task {})",
            self.task_id
        )
    }
}

/// Compare the persisted op-log state against the current disk, returning the
/// ledger paths whose on-disk content diverged from what the last checkpoint
/// captured. A non-empty result means the working copy moved out from under the
/// op-log (crash / external edit): the caller synthesizes a recovery checkpoint
/// of the actual disk state before offering rollback (jj stale-working-copy
/// pattern).
pub(super) fn diverged_paths(task: &PersistedTask) -> Vec<PathBuf> {
    let mut diverged = Vec::new();
    for (path, expected) in &task.expected {
        let abs = PathBuf::from(path);
        let current = std::fs::read(&abs)
            .ok()
            .map(|b| crate::tools::content_hash(&b));
        if &current != expected {
            diverged.push(abs);
        }
    }
    diverged
}

/// Resolve the repo's git directory (absolute), where the task records and the
/// `refs/iris/*` chain live. `None` when `workspace` is not a git repo.
pub(super) fn git_dir(workspace: &Path) -> Option<PathBuf> {
    let out = git_stdout(workspace, &["rev-parse", "--absolute-git-dir"]).ok()?;
    let text = String::from_utf8_lossy(&out);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn tasks_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("iris").join("tasks")
}

fn record_path(git_dir: &Path, task_id: &str) -> PathBuf {
    tasks_dir(git_dir).join(format!("{task_id}.json"))
}

/// Persist (create or overwrite) a task record. Best-effort durability: written
/// to a temp file then renamed so a crash mid-write never leaves a truncated
/// record.
pub(super) fn save(git_dir: &Path, task: &PersistedTask) -> Result<()> {
    let dir = tasks_dir(git_dir);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create task dir {}", dir.display()))?;
    let path = record_path(git_dir, &task.task_id);
    let temp = dir.join(format!("{}.tmp", task.task_id));
    let json = serde_json::to_vec_pretty(task).context("failed to serialize task state")?;
    std::fs::write(&temp, &json)
        .with_context(|| format!("failed to write task record {}", temp.display()))?;
    std::fs::rename(&temp, &path)
        .with_context(|| format!("failed to finalize task record {}", path.display()))?;
    Ok(())
}

/// Delete a task record and its lease lock-file (settlement teardown). No-op
/// when already gone. Removing the lease file prevents `.lock` accumulation; it
/// is safe because settlement drops the owning `Task` (releasing this process's
/// lease) before this runs, and task ids never repeat.
pub(super) fn remove(git_dir: &Path, task_id: &str) {
    let _ = std::fs::remove_file(record_path(git_dir, task_id));
    let _ = std::fs::remove_file(super::lock::lease_path(git_dir, task_id));
}

/// Load every persisted (unsettled) task record in the repo. A record exists
/// only while its task is unsettled -- settlement removes it -- so any record
/// found is an unsettled task to recover or expire.
pub(super) fn load_all(git_dir: &Path) -> Vec<PersistedTask> {
    let dir = tasks_dir(git_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut tasks = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path)
            && let Ok(task) = serde_json::from_slice::<PersistedTask>(&bytes)
        {
            tasks.push(task);
        }
    }
    tasks
}

/// Milliseconds since the Unix epoch, saturating.
pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Coarse human age for the recovery notice ("3 days", "2 hours", "5 minutes").
fn human_age(age: Duration) -> String {
    let secs = age.as_secs();
    if secs >= 86_400 {
        let days = secs / 86_400;
        format!("{days} day{}", plural(days))
    } else if secs >= 3_600 {
        let hours = secs / 3_600;
        format!("{hours} hour{}", plural(hours))
    } else if secs >= 60 {
        let mins = secs / 60;
        format!("{mins} minute{}", plural(mins))
    } else {
        format!("{secs} second{}", plural(secs))
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}
