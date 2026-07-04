//! Task settlement, rollback, recovery, and expiry (issue #263, ADR-0028).
//!
//! The user-facing half of the checkpoint chain: the operations that *freeze* a
//! task (accept / explicit checkpoint), *undo* it (rollback of Iris-authored
//! ledger paths plus the user's index), and *reconcile* an unsettled task on
//! resume (crash recovery, 30-day expiry). The mutation-guard mechanics
//! (baseline capture, gating, per-call checkpointing) live in the parent module;
//! these methods sit on the same [`GitSafety`] but read as one cohesive
//! "settlement" concern.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;

use super::{
    Baseline, Chain, CheckpointChain, GitSafety, IrisChange, KEEP_CHECKPOINTS, Mode, RestorePoint,
    RollbackOutcome, Task, baseline, checkpoint, git, lock, task_state,
};

/// How a persisted record classifies during recovery (ADR-0030). Leased (live
/// foreign) records never reach this enum -- they are skipped before classifying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TaskClass {
    /// Lease-free and lock-protocol-stamped: a crashed orphan safe to adopt.
    Recoverable,
    /// No lock metadata (predates the lease protocol): unknown, never
    /// auto-adopted (ADR-0030). Explicit selection required.
    Legacy,
}

/// A record surfaced to recovery: a crashed orphan or an unknown-legacy record.
/// Live foreign (leased) tasks are never included. Kept small and evolvable --
/// #287 adds body/session links and #288's resume picker reads this list.
#[derive(Debug, Clone)]
pub(super) struct RecoverableTask {
    pub(super) task_id: String,
    // `workspace` and `age` are part of the recovery seam spec (#285) that the
    // #288 resume picker consumes; they are surfaced now (and asserted by tests)
    // but not yet read by non-test production code.
    #[allow(dead_code)]
    pub(super) workspace: String,
    /// Age since the record was last updated, for notice wording / picker order.
    #[allow(dead_code)]
    pub(super) age: Duration,
    pub(super) class: TaskClass,
}

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
        // Keep the task lease held (`_lease`) through the whole teardown: while it
        // is held no other process can adopt or checkpoint this task, closing the
        // TOCTOU window between settling and record removal (ADR-0030).
        let Task {
            mut chain,
            git_dir,
            task_id,
            _lease,
            ..
        } = task;
        let gc_chain = |chain: &mut Chain| match chain {
            Chain::Git(chain) => {
                if let Err(error) = chain.gc(KEEP_CHECKPOINTS) {
                    tracing::warn!(error = %format!("{error:#}"), "checkpoint GC on accept failed");
                }
            }
            Chain::Fallback(store) => store.gc(KEEP_CHECKPOINTS),
        };
        // Serialize the ref GC + record removal against concurrent processes
        // (ADR-0030): one short mutation-lock hold around the shared writes.
        if let Some(git_dir) = git_dir {
            lock::with_mutation_lock(&git_dir, || {
                gc_chain(&mut chain);
                task_state::remove(&git_dir, &task_id);
            });
        } else {
            gc_chain(&mut chain);
        }
        drop(_lease);
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
                preserved_notices: Vec::new(),
            });
        };
        // Keep the task lease held (`_lease`) through teardown so no concurrent
        // recovery can adopt this task while it is being rolled back (ADR-0030).
        let Task {
            mut chain,
            git_dir,
            task_id,
            baseline,
            ledger,
            turn,
            _lease,
            ..
        } = task;
        let mut index_warning = None;
        let mut preserved_notices = Vec::new();
        let mut count = ledger.entries.len();
        match &mut chain {
            Chain::Git(chain) => {
                count = count.max(chain.ledger_path_count());
                // ADR-0028 TOCTOU reconciliation: before restoring, re-hash every
                // ledger path against Iris's last recorded state (the chain tip).
                // Any diverged path was edited by the user after Iris's last
                // write -- it is user-attributed and must never be clobbered.
                let diverged = chain.diverged_from_tip().unwrap_or_default();
                let excluded: BTreeSet<PathBuf> = diverged.iter().cloned().collect();
                if !excluded.is_empty() {
                    // Capture the current disk (including the user's newer bytes)
                    // as a full recovery checkpoint first, so nothing is lost even
                    // transiently, then exclude the diverged paths from restore.
                    if let Err(error) =
                        chain.checkpoint(turn, None, "pre-rollback snapshot".to_string())
                    {
                        tracing::warn!(error = %format!("{error:#}"), "pre-rollback recovery snapshot failed");
                    }
                    for path in &diverged {
                        preserved_notices.push(format!(
                            "kept your edit to {} (changed after Iris's last write; not rolled back)",
                            path.display()
                        ));
                    }
                }
                chain.rollback_to_excluding(seq, &excluded)?;
                index_warning = self.restore_user_index(&baseline.index);
                // Serialize the ref destroy against concurrent processes
                // (ADR-0030); the held lease already blocks any adopt.
                let destroy = |chain: &mut CheckpointChain| {
                    if let Err(error) = chain.destroy() {
                        tracing::warn!(error = %format!("{error:#}"), "checkpoint teardown on rollback failed");
                    }
                };
                match git_dir.as_ref() {
                    Some(gd) => lock::with_mutation_lock(gd, || destroy(chain)),
                    None => destroy(chain),
                }
            }
            Chain::Fallback(store) => store.rollback_to(seq)?,
        }
        if let Some(git_dir) = git_dir {
            lock::with_mutation_lock(&git_dir, || task_state::remove(&git_dir, &task_id));
        }
        drop(_lease);
        Ok(RollbackOutcome {
            summary: format!("rolled back {count} Iris change(s) to restore point {seq}"),
            index_warning,
            preserved_notices,
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

    /// On resume or a new session in the same repo: reconcile the unsettled tasks
    /// this process may adopt and surface a one-line notice for the event stream.
    /// Composes the ADR-0030 recovery policy from the three seams below:
    /// [`expire_stale`](Self::expire_stale) sweeps stale records, then
    /// [`recoverable_tasks`](Self::recoverable_tasks) classifies the rest
    /// (leased/live-foreign records are skipped). Exactly one lease-free
    /// non-legacy orphan preserves the current auto-adopt UX via
    /// [`adopt_task`](Self::adopt_task); more than one, or any unknown-legacy
    /// record, requires explicit selection -- until the resume-task picker (#288)
    /// lands this is a notice listing the task ids. `None` when nothing is
    /// recoverable. The notice always names the record actually adopted, fixing
    /// the ADR-0030 notice/adopt mismatch. Lazy: called at startup/resume, no
    /// daemon.
    pub(crate) fn recover_and_expire(&self) -> Option<String> {
        if !matches!(self.mode, Mode::Git) {
            return None;
        }
        let git_dir = task_state::git_dir(&self.workspace)?;
        let now = SystemTime::now();
        self.expire_stale(&git_dir, now);
        let recoverable = self.recoverable_tasks();
        if recoverable.is_empty() {
            return None;
        }
        let adoptable: Vec<&RecoverableTask> = recoverable
            .iter()
            .filter(|task| task.class == TaskClass::Recoverable)
            .collect();
        let has_legacy = recoverable
            .iter()
            .any(|task| task.class == TaskClass::Legacy);
        if adoptable.len() == 1 && !has_legacy {
            // Preserve the current UX: auto-adopt the single orphan, naming the
            // record actually adopted in the notice.
            let record = self.adopt_task(&adoptable[0].task_id)?;
            return Some(record.recovery_notice(now));
        }
        Some(selection_notice(&recoverable))
    }

    /// Expire stale unsettled tasks in this workspace (ADR-0028 30-day window):
    /// each auto-settles as accepted and its `refs/iris/*` refs are GC'd -- by
    /// then the changes are the user's de facto working state. Each record's
    /// teardown runs under the repo mutation lock so it never tears against a
    /// concurrent write (ADR-0030).
    pub(super) fn expire_stale(&self, git_dir: &Path, now: SystemTime) {
        let workspace = self.workspace.to_string_lossy().into_owned();
        for task in task_state::load_all(git_dir) {
            if task.workspace != workspace {
                continue;
            }
            if !task.is_expired(now, task_state::DEFAULT_EXPIRY) {
                continue;
            }
            // Never touch a live foreign task, even an old one (ADR-0030): claim
            // its lease non-blocking first. A held lease means a live process
            // still owns it -- skip. For a legacy record with no lease file the
            // claim creates and takes it (lease-free), so legacy expiry still
            // works. Holding the claimed lease through teardown also blocks a
            // concurrent adopt of this orphan mid-delete.
            let lease = match lock::try_exclusive(&lock::lease_path(git_dir, &task.task_id)) {
                Ok(Some(guard)) => guard,
                _ => continue,
            };
            lock::with_mutation_lock(git_dir, || {
                let _ = checkpoint::destroy_task_refs(&self.workspace, &task.task_id);
                task_state::remove(git_dir, &task.task_id);
            });
            drop(lease);
        }
    }

    /// Enumerate this workspace's unsettled records and classify each (ADR-0030).
    /// A **leased** record is a live foreign task -- skipped, never returned, so
    /// a caller can never adopt or checkpoint a task another process owns. A
    /// **lease-free, lock-protocol** record is a recoverable orphan; a record
    /// with no lock metadata is unknown-**legacy**. Returns the recoverable +
    /// legacy records only. This is the seam the #288 resume picker plugs into.
    pub(super) fn recoverable_tasks(&self) -> Vec<RecoverableTask> {
        if !matches!(self.mode, Mode::Git) {
            return Vec::new();
        }
        let Some(git_dir) = task_state::git_dir(&self.workspace) else {
            return Vec::new();
        };
        let now = SystemTime::now();
        let workspace = self.workspace.to_string_lossy().into_owned();
        let mut out = Vec::new();
        for task in task_state::load_all(&git_dir) {
            if task.workspace != workspace {
                continue;
            }
            // A legacy record (no lock metadata) is always unknown, regardless of
            // any lease -- it predates the protocol, so no reliable ownership can
            // be inferred.
            let class = if task.lock_protocol.is_none() {
                TaskClass::Legacy
            } else if !lock::is_lease_free(&lock::lease_path(&git_dir, &task.task_id)) {
                // Leased: a live foreign task. Never adopt or list it.
                continue;
            } else if task.lock_protocol.as_deref() == Some(lock::LOCK_PROTOCOL) {
                TaskClass::Recoverable
            } else {
                // Lease-free but stamped with an unknown/future lock protocol
                // whose liveness semantics this build cannot interpret: surface
                // as unknown, never auto-adopt (ADR-0030 degrade direction --
                // spurious "unknown", never adoption of a task we cannot vet).
                TaskClass::Legacy
            };
            let age = now
                .duration_since(UNIX_EPOCH + Duration::from_millis(task.updated_ms))
                .unwrap_or_default();
            out.push(RecoverableTask {
                task_id: task.task_id,
                workspace: task.workspace,
                age,
                class,
            });
        }
        out
    }

    /// Adopt a recoverable orphan by id (ADR-0030): claim its lease (bailing if a
    /// live process grabbed it first), reconcile disk vs the op-log (append a
    /// FULL recovery snapshot for any diverged path, under the mutation lock),
    /// and rehydrate it as this process's active task so a post-restart
    /// `/rollback` / `/accept` / `/checkpoint` operates on the real chain.
    /// Returns the adopted record so the caller's notice names the record it
    /// actually acted on; `None` when the record is gone or now leased.
    pub(super) fn adopt_task(&self, task_id: &str) -> Option<task_state::PersistedTask> {
        if !matches!(self.mode, Mode::Git) {
            return None;
        }
        let git_dir = task_state::git_dir(&self.workspace)?;
        let workspace = self.workspace.to_string_lossy().into_owned();
        let record = task_state::load_all(&git_dir)
            .into_iter()
            .find(|task| task.task_id == task_id && task.workspace == workspace)?;
        // Claim the lease for the task's lifetime. If a live process holds it,
        // this is a foreign live task -- do not adopt.
        let lease = match lock::try_exclusive(&lock::lease_path(&git_dir, task_id)) {
            Ok(Some(guard)) => guard,
            _ => return None,
        };
        // Reconcile disk vs the op-log first (append a FULL recovery snapshot for
        // any diverged path), so the rehydrated chain's tip reflects the actual
        // disk state before it is offered for rollback. Serialized against
        // concurrent processes by the mutation lock.
        let diverged = task_state::diverged_paths(&record);
        if !diverged.is_empty() {
            lock::with_mutation_lock(&git_dir, || {
                if let Err(error) = checkpoint::append_recovery(&self.workspace, task_id, &diverged)
                {
                    tracing::warn!(error = %format!("{error:#}"), "recovery checkpoint append failed");
                }
            });
        }
        self.rehydrate_task(&git_dir, &record, lease);
        Some(record)
    }

    /// Rebuild an active [`Task`] from a persisted unsettled record and its
    /// durable `refs/iris/*` chain (ADR-0028 crash recovery), so settlement
    /// operations work after a restart. The chain is loaded from refs; the
    /// baseline is re-captured against the current disk (so continued mutation
    /// still gates today's dirty files -- the safe direction) but its index is
    /// the ORIGINAL staged state from the record, the selection a rollback must
    /// restore. The caller's already-acquired `lease` is moved onto the task so
    /// this process holds ownership for the task's lifetime. No-op when a task is
    /// already active.
    fn rehydrate_task(
        &self,
        git_dir: &Path,
        persisted: &task_state::PersistedTask,
        lease: lock::FlockGuard,
    ) {
        {
            let state = self.state.lock().unwrap();
            if state.task.is_some() {
                return;
            }
        }
        let ledger_paths: Vec<PathBuf> = persisted.expected.keys().map(PathBuf::from).collect();
        let chain = match CheckpointChain::load(
            self.workspace.clone(),
            persisted.task_id.clone(),
            &ledger_paths,
        ) {
            Ok(chain) => chain,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "could not rehydrate checkpoint chain on resume");
                return;
            }
        };
        let mut baseline = baseline::capture(&self.workspace, |path| self.normalize(path))
            .unwrap_or_else(|_| Baseline {
                protected: BTreeMap::new(),
                dirty_count: 0,
                untracked_count: 0,
                index: String::new(),
            });
        baseline.index = persisted.baseline_index.clone();
        let mut task = Task::active(
            persisted.task_id.clone(),
            baseline,
            chain,
            Some(git_dir.to_path_buf()),
            Some(lease),
        );
        task.created_ms = persisted.created_ms;
        let mut state = self.state.lock().unwrap();
        if state.task.is_none() {
            state.task = Some(task);
        }
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

/// A one-line notice when recovery cannot auto-adopt (more than one recoverable
/// orphan, or any unknown-legacy record): explicit selection is required
/// (ADR-0030), and until the #288 resume picker lands this lists the task ids so
/// the user can act. Legacy records are flagged as "unknown" so the user knows
/// they are not auto-adopted.
fn selection_notice(tasks: &[RecoverableTask]) -> String {
    let ids: Vec<String> = tasks
        .iter()
        .map(|task| match task.class {
            TaskClass::Recoverable => task.task_id.clone(),
            TaskClass::Legacy => format!("{} (unknown)", task.task_id),
        })
        .collect();
    format!(
        "{} unsettled Iris task(s) need attention -- resume one to view / accept / roll back (tasks: {})",
        tasks.len(),
        ids.join(", ")
    )
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
