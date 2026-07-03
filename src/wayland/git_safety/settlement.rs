//! Task settlement, rollback, recovery, and expiry (issue #263, ADR-0028).
//!
//! The user-facing half of the checkpoint chain: the operations that *freeze* a
//! task (accept / explicit checkpoint), *undo* it (rollback of Iris-authored
//! ledger paths plus the user's index), and *reconcile* an unsettled task on
//! resume (crash recovery, 30-day expiry). The mutation-guard mechanics
//! (baseline capture, gating, per-call checkpointing) live in the parent module;
//! these methods sit on the same [`GitSafety`] but read as one cohesive
//! "settlement" concern.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;

use super::{
    Chain, GitSafety, IrisChange, KEEP_CHECKPOINTS, Mode, RestorePoint, RollbackOutcome, Task,
    checkpoint, git, task_state,
};

impl GitSafety {
    /// Restore points offered by the rollback UI (Tier 3): the pre-task baseline
    /// first (seq 0), then each intermediate checkpoint oldest-to-newest. Empty
    /// when no task is active.
    pub(crate) fn restore_points(&self) -> Vec<RestorePoint> {
        self.sync_barrier();
        let state = self.state.lock().unwrap();
        let Some(task) = state.task.as_ref() else {
            return Vec::new();
        };
        match &task.chain {
            Chain::Git(chain) => {
                let mut out = vec![RestorePoint {
                    seq: 0,
                    label: "pre-task baseline".to_string(),
                }];
                for point in chain.restore_points() {
                    out.push(RestorePoint {
                        seq: point.seq,
                        label: point.label,
                    });
                }
                out
            }
            Chain::Fallback(store) => store
                .labels()
                .into_iter()
                .enumerate()
                .map(|(seq, label)| RestorePoint {
                    seq: seq as u64,
                    label,
                })
                .collect(),
        }
    }

    /// Settle the current task as ACCEPTED (ADR-0028): freeze the ledger, GC the
    /// intermediate checkpoints keeping the last N, and drop the recovery record
    /// so the accepted work is no longer offered for rollback. Returns a one-line
    /// summary, or `None` when no task is active.
    pub(crate) fn accept(&self) -> Option<String> {
        self.sync_barrier();
        let mut state = self.state.lock().unwrap();
        let task = state.task.take()?;
        let count = task.ledger.entries.len();
        let Task {
            mut chain,
            git_dir,
            task_id,
            ..
        } = task;
        match &mut chain {
            Chain::Git(chain) => {
                if let Err(error) = chain.gc(KEEP_CHECKPOINTS) {
                    tracing::warn!(error = %format!("{error:#}"), "checkpoint GC on accept failed");
                }
            }
            Chain::Fallback(store) => store.gc(KEEP_CHECKPOINTS),
        }
        if let Some(git_dir) = git_dir {
            task_state::remove(&git_dir, &task_id);
        }
        Some(format!(
            "accepted {count} Iris change(s); checkpoints pruned"
        ))
    }

    /// Record an explicit user checkpoint (the `/checkpoint` command) and settle
    /// the task like accept, so the next mutation opens a fresh baseline
    /// (ADR-0028: an explicit checkpoint command freezes the ledger). Returns a
    /// summary, or `None` when no task is active.
    pub(crate) fn checkpoint_now(&self) -> Option<String> {
        self.sync_barrier();
        {
            let mut state = self.state.lock().unwrap();
            if let Some(task) = state.task.as_mut()
                && let Chain::Git(chain) = &mut task.chain
            {
                let turn = task.turn;
                let _ = chain.checkpoint(turn, None, "explicit checkpoint".to_string());
            }
        }
        self.accept()
            .map(|_| "checkpoint saved; task settled".to_string())
    }

    /// Roll back the current task to restore point `seq` (0 = pre-task baseline):
    /// materialize Iris-authored ledger paths from the checkpoint tree, restore
    /// the user's index to its baseline (degrade to detect-and-warn in exotic
    /// repo states), then destroy the checkpoint refs and recovery record. Only
    /// Iris's own work is touched; user paths are never altered.
    pub(crate) fn rollback(&self, seq: u64) -> Result<RollbackOutcome> {
        self.sync_barrier();
        let mut state = self.state.lock().unwrap();
        let Some(task) = state.task.take() else {
            return Ok(RollbackOutcome {
                summary: "no active Iris task to roll back".to_string(),
                index_warning: None,
            });
        };
        let count = task.ledger.entries.len();
        let Task {
            mut chain,
            git_dir,
            task_id,
            baseline,
            ..
        } = task;
        let mut index_warning = None;
        match &mut chain {
            Chain::Git(chain) => {
                chain.rollback_to(seq)?;
                index_warning = self.restore_user_index(&baseline.index);
                if let Err(error) = chain.destroy() {
                    tracing::warn!(error = %format!("{error:#}"), "checkpoint teardown on rollback failed");
                }
            }
            Chain::Fallback(store) => store.rollback_to(seq)?,
        }
        if let Some(git_dir) = git_dir {
            task_state::remove(&git_dir, &task_id);
        }
        Ok(RollbackOutcome {
            summary: format!("rolled back {count} Iris change(s) to restore point {seq}"),
            index_warning,
        })
    }

    /// Restore the user's index to `baseline_index` (`git ls-files --stage`
    /// output) on rollback. Degrades to detect-and-warn in exotic states
    /// (mid-merge / mid-rebase) where a blind index reset is unsafe (ADR-0028).
    /// Operates on the real index deliberately -- restoring the user's staged
    /// selection is the point -- but never touches `HEAD`, the worktree, or
    /// stash.
    fn restore_user_index(&self, baseline_index: &str) -> Option<String> {
        if let Some(state) = exotic_git_state(&self.workspace) {
            return Some(format!(
                "index left unchanged: repository is {state}; restore staging manually if needed"
            ));
        }
        // Clear the index, then reload exactly the baseline entries.
        if let Err(error) = git::git_stdout(&self.workspace, &["read-tree", "--empty"]) {
            return Some(format!("could not reset index: {error:#}"));
        }
        if let Err(error) = git::git_io(
            &self.workspace,
            &["update-index", "--index-info"],
            &[],
            baseline_index.as_bytes(),
        ) {
            return Some(format!("could not restore staged entries: {error:#}"));
        }
        None
    }

    /// Record a degraded (non-git) content-snapshot restore point for the call's
    /// known targets (ADR-0028 fallback). Best-effort; no gating or attribution.
    pub(super) fn checkpoint_degraded(&self, task: &mut Task, approved: &[PathBuf]) {
        let pres: Vec<(PathBuf, Option<Vec<u8>>)> = approved
            .iter()
            .map(|path| {
                let norm = self.normalize(path);
                let pre = task.snapshot.pre_bytes(&norm).and_then(|opt| opt.clone());
                (norm, pre)
            })
            .collect();
        if pres.is_empty() {
            return;
        }
        if let Chain::Fallback(store) = &mut task.chain {
            for (path, bytes) in &pres {
                store.note_before(path, bytes.clone());
            }
            store.checkpoint("edit".to_string());
        }
    }

    /// On resume or a new session in the same repo (ADR-0028): expire stale
    /// unsettled tasks (auto-settle accepted, GC refs) and, for a live unsettled
    /// task whose disk diverged from the op-log, append a recovery checkpoint and
    /// return a one-line notice for the event stream. `None` when nothing is
    /// unsettled. Lazy: called at startup/resume, no daemon.
    pub(crate) fn recover_and_expire(&self) -> Option<String> {
        if !matches!(self.mode, Mode::Git) {
            return None;
        }
        let git_dir = task_state::git_dir(&self.workspace)?;
        let now = SystemTime::now();
        let workspace = self.workspace.to_string_lossy();
        let mut notice = None;
        for task in task_state::load_all(&git_dir) {
            if task.workspace != workspace {
                continue;
            }
            if task.is_expired(now, task_state::DEFAULT_EXPIRY) {
                let _ = checkpoint::destroy_task_refs(&self.workspace, &task.task_id);
                task_state::remove(&git_dir, &task.task_id);
                continue;
            }
            let diverged = task_state::diverged_paths(&task);
            if !diverged.is_empty()
                && let Err(error) =
                    checkpoint::append_recovery(&self.workspace, &task.task_id, &diverged)
            {
                tracing::warn!(error = %format!("{error:#}"), "recovery checkpoint append failed");
            }
            notice = Some(task.recovery_notice(now));
        }
        notice
    }
}

/// Whether the repo is in an exotic state where a blind index reset is unsafe
/// (mid-merge, mid-rebase, cherry-pick, bisect). Returns a human label for the
/// warning, or `None` for a normal state. Read-only probe of `.git` marker
/// files via the resolved git dir.
fn exotic_git_state(workspace: &Path) -> Option<&'static str> {
    let git_dir = task_state::git_dir(workspace)?;
    if git_dir.join("MERGE_HEAD").exists() {
        Some("mid-merge")
    } else if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
        Some("mid-rebase")
    } else if git_dir.join("CHERRY_PICK_HEAD").exists() {
        Some("mid-cherry-pick")
    } else if git_dir.join("BISECT_LOG").exists() {
        Some("mid-bisect")
    } else {
        None
    }
}

/// A short op-log label for a checkpoint from the paths it touched.
pub(super) fn checkpoint_label(changes: &[IrisChange]) -> String {
    match changes.split_first() {
        Some(((path, _), rest)) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            if rest.is_empty() {
                format!("edit {name}")
            } else {
                format!("edit {name} (+{} more)", rest.len())
            }
        }
        None => "checkpoint".to_string(),
    }
}

/// A fresh task id: 128 random bits as hex, so it is a valid single git ref path
/// component (no separators, `..`, or special chars) and collision-free across
/// tasks and sessions.
pub(super) fn new_task_id() -> String {
    format!("{:032x}", rand::random::<u128>())
}
