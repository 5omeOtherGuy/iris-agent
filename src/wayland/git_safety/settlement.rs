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
    Baseline, Chain, CheckpointChain, GitSafety, IrisChange, Mode, RestorePoint, RollbackOutcome,
    Settlement, Task, baseline, checkpoint, git, is_linked_worktree, lock, push_session_deduped,
    task_state,
};

/// How a persisted record classifies during recovery (ADR-0030). Leased (live
/// foreign) records never reach this enum -- they are skipped before classifying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskClass {
    /// Lease-free and lock-protocol-stamped: a crashed orphan safe to adopt.
    Recoverable,
    /// No lock metadata (predates the lease protocol): unknown, never
    /// auto-adopted (ADR-0030). Explicit selection required.
    Legacy,
}

/// A record surfaced to recovery: a crashed orphan or an unknown-legacy record.
/// Live foreign (leased) tasks are never included. The #288 resume-task picker
/// (Tier 3) renders these rows, so the display payload (`body`/`sessions`) is
/// surfaced alongside the recovery seam fields. `body`/`sessions` are opaque
/// display copy (ADR-0031): recovery classification never branches on them.
#[derive(Debug, Clone)]
pub(crate) struct RecoverableTask {
    pub(crate) task_id: String,
    // `workspace` is part of the recovery seam spec (#285); it is surfaced (and
    // asserted by tests) but the picker does not render it -- rows are already
    // workspace-scoped -- so no production code reads it yet.
    #[allow(dead_code)]
    pub(crate) workspace: String,
    /// Age since the record was last updated, for notice wording / picker order.
    pub(crate) age: Duration,
    /// Opaque display body (ADR-0031): the prompt preview of the turn that
    /// opened the task, or `None` for a legacy record with no recorded body.
    pub(crate) body: Option<String>,
    /// Opaque display join (ADR-0031): the session ids that worked this task.
    /// The picker shows `sessions.len()` as the linked-session count.
    pub(crate) sessions: Vec<String>,
    /// Recovery classification (ADR-0030). The picker does not branch on it
    /// (rows are rendered from `body`/`sessions`); recovery policy does.
    pub(crate) class: TaskClass,
}

impl RecoverableTask {
    /// Whether this record predates the lease protocol (ADR-0030): an unknown
    /// record adoptable only by explicit selection, marked as legacy in the UI.
    /// Display-only classification; recovery policy uses the private [`TaskClass`]
    /// directly and never this bool.
    pub(crate) fn is_legacy(&self) -> bool {
        matches!(self.class, TaskClass::Legacy)
    }
}

fn linked_recoverable_for_session(
    tasks: &[RecoverableTask],
    session_id: &str,
) -> Option<RecoverableTask> {
    let mut linked = tasks
        .iter()
        .filter(|task| {
            task.class == TaskClass::Recoverable
                && task.sessions.iter().any(|session| session == session_id)
        })
        .cloned();
    let first = linked.next()?;
    linked.next().is_none().then_some(first)
}

#[cfg(test)]
impl RecoverableTask {
    /// Construct a recoverable row for Tier-3 unit tests (`ui::picker`) without a
    /// repo. Fixed to the `Recoverable` class -- the picker renders body / age /
    /// sessions and never branches on the class -- so tests do not need to name
    /// the git-safety-private [`TaskClass`].
    pub(crate) fn for_test(
        task_id: &str,
        age: Duration,
        body: Option<&str>,
        sessions: &[&str],
    ) -> Self {
        RecoverableTask {
            task_id: task_id.to_string(),
            workspace: "/proj".to_string(),
            age,
            body: body.map(str::to_string),
            sessions: sessions.iter().map(|s| s.to_string()).collect(),
            class: TaskClass::Recoverable,
        }
    }

    /// Construct a legacy row (no recorded body, `Legacy` class) for Tier-3 unit
    /// tests that need the legacy marker without naming the private [`TaskClass`].
    pub(crate) fn for_test_legacy(task_id: &str, age: Duration) -> Self {
        RecoverableTask {
            task_id: task_id.to_string(),
            workspace: "/proj".to_string(),
            age,
            body: None,
            sessions: Vec::new(),
            class: TaskClass::Legacy,
        }
    }
}

/// The outcome of adopting a recoverable task, surfaced to Tier 3 (#288). Maps
/// the internal [`PersistedTask`](task_state::PersistedTask) to just the opaque
/// display payload the adoption notice / "also resume" offer needs, so the
/// private record type never leaks past the `Harness`.
#[derive(Debug, Clone)]
pub(crate) struct AdoptedTask {
    pub(crate) task_id: String,
    pub(crate) body: Option<String>,
    pub(crate) sessions: Vec<String>,
}

/// Why a task adoption request could not be completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdoptError {
    /// This process already owns an active task; adopting another would orphan
    /// or entangle rollback state.
    TaskActive,
    /// The requested record is gone, belongs to another live owner, or could not
    /// be rehydrated from its checkpoint refs.
    Unavailable,
}

/// What resume/startup recovery decided (ADR-0030/ADR-0031 policy), returned to
/// Tier 3 so the >1/legacy case opens the picker instead of only printing a
/// notice. `None` = nothing recoverable; `Notice` = the single-orphan auto-adopt
/// (unchanged UX); `ResumeLinked` = a resumed session links exactly one
/// recoverable task and should get an explicit resume-task offer; `Picker` =
/// explicit selection required over these rows.
#[derive(Debug)]
pub(crate) enum RecoveryOutcome {
    None,
    Notice(String),
    ResumeLinked(RecoverableTask),
    Picker(Vec<RecoverableTask>),
}

impl RecoveryOutcome {
    /// How many recoverable tasks require explicit selection (the `Picker` case).
    /// `None`/`Notice` (nothing, or the single auto-adopted orphan) count zero.
    /// Feeds the start page's `Tasks` badge instead of forcing a picker open.
    pub(crate) fn recoverable_count(&self) -> usize {
        match self {
            RecoveryOutcome::Picker(tasks) => tasks.len(),
            RecoveryOutcome::ResumeLinked(_) => 1,
            RecoveryOutcome::None | RecoveryOutcome::Notice(_) => 0,
        }
    }
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
        if !task.durable {
            return Vec::new();
        }
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

    /// Settle the current task as ACCEPTED (ADR-0028): freeze the ledger, delete
    /// its checkpoint refs, and drop the recovery record so the accepted work is
    /// no longer offered for rollback. Returns the
    /// settled task id plus a one-line summary (ADR-0031: the harness appends a
    /// `TaskSettled` audit entry from the id), or `None` when no task is active.
    pub(crate) fn accept(&self) -> Option<Settlement> {
        self.sync_barrier();
        let mut state = self.state.lock().unwrap();
        if state.task.as_ref().is_some_and(|task| !task.durable) {
            return None;
        }
        let task = state.task.take()?;
        let count = task.ledger.entries.len();
        Some(self.destroy_settled_task(
            task,
            format!("accepted {count} Iris change(s); checkpoints removed"),
        ))
    }

    /// Record an explicit user checkpoint (the `/checkpoint` command) without
    /// settling the task (ADR-0052): append a labeled restore point and keep the
    /// task's record, approvals, and baseline alive. Returns a one-line summary,
    /// or `None` when no task is active.
    pub(crate) fn checkpoint_now(&self) -> Option<String> {
        self.sync_barrier();
        let mut state = self.state.lock().unwrap();
        if state.task.as_ref().is_some_and(|task| !task.durable) {
            return None;
        }
        let task = state.task.as_mut()?;
        match &mut task.chain {
            Chain::Git(chain) => {
                if let Err(error) =
                    chain.checkpoint(task.turn, None, "explicit checkpoint".to_string())
                {
                    tracing::warn!(error = %format!("{error:#}"), "explicit checkpoint create failed");
                    return Some("could not save checkpoint; task is still open".to_string());
                }
            }
            Chain::Fallback(store) => store.checkpoint("explicit checkpoint".to_string()),
        }
        self.persist_task(task);
        Some("checkpoint saved; task is still open".to_string())
    }

    /// Roll back the current task to restore point `seq` (0 = pre-task baseline):
    /// materialize Iris-authored ledger paths from the checkpoint tree, restore
    /// the user's index to its baseline (degrade to detect-and-warn in exotic
    /// repo states), then destroy the checkpoint refs and recovery record. Only
    /// Iris's own work is touched; user paths are never altered.
    pub(crate) fn rollback(&self, seq: u64) -> Result<RollbackOutcome> {
        self.sync_barrier();
        let mut state = self.state.lock().unwrap();
        if state.task.as_ref().is_some_and(|task| !task.durable) {
            return Ok(RollbackOutcome {
                summary: "no active Iris task to roll back".to_string(),
                settled_task_id: None,
                index_warning: None,
                preserved_notices: Vec::new(),
            });
        }
        let Some(task) = state.task.take() else {
            return Ok(RollbackOutcome {
                summary: "no active Iris task to roll back".to_string(),
                settled_task_id: None,
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
            settled_task_id: Some(task_id),
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
    /// record, requires explicit selection: Tier 3 opens the resume-task picker
    /// over the recoverable rows (#288). `None` when nothing is recoverable. A
    /// `Notice` always names the record actually adopted, fixing the ADR-0030
    /// notice/adopt mismatch. Lazy: called at startup/resume, no daemon.
    pub(crate) fn recover_and_expire(&self) -> RecoveryOutcome {
        self.recover_and_expire_inner(None)
    }

    /// Variant of [`recover_and_expire`](Self::recover_and_expire) for an
    /// explicit session resume: after the stale sweep, if exactly one
    /// recoverable row links the resumed session id, return it as an explicit
    /// resume-task offer instead of applying the workspace-wide auto-adopt rule.
    /// Zero or multiple linked rows fall back to the normal recovery policy.
    pub(crate) fn recover_and_expire_for_session(&self, session_id: &str) -> RecoveryOutcome {
        self.recover_and_expire_inner(Some(session_id))
    }

    fn recover_and_expire_inner(&self, resumed_session: Option<&str>) -> RecoveryOutcome {
        if !self.workflow_enabled {
            return RecoveryOutcome::None;
        }
        if !matches!(self.mode, Mode::Git) {
            return RecoveryOutcome::None;
        }
        let Some(git_dir) = task_state::git_dir(&self.workspace) else {
            return RecoveryOutcome::None;
        };
        let now = SystemTime::now();
        self.expire_stale(&git_dir, now);
        let recoverable = self.recoverable_tasks();
        if recoverable.is_empty() {
            return RecoveryOutcome::None;
        }
        if let Some(session_id) = resumed_session
            && let Some(task) = linked_recoverable_for_session(&recoverable, session_id)
        {
            return RecoveryOutcome::ResumeLinked(task);
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
            return match self.adopt_task(&adoptable[0].task_id) {
                Ok(record) => RecoveryOutcome::Notice(record.recovery_notice(now)),
                Err(_) => RecoveryOutcome::None,
            };
        }
        // More than one orphan, or any unknown-legacy record: explicit selection
        // is required (ADR-0030). Tier 3 opens the resume-task picker over these
        // rows (#288) instead of auto-adopting.
        RecoveryOutcome::Picker(recoverable)
    }

    /// Expire stale unsettled tasks in this workspace (ADR-0028 30-day window):
    /// each auto-settles as accepted and its `refs/iris/*` refs are deleted --
    /// by then the changes are the user's de facto working state. Each record's
    /// teardown runs under the repo mutation lock so it never tears against a
    /// concurrent write (ADR-0030). The same recovery pass also repairs old
    /// accepted/checkpointed tasks that leaked recordless checkpoint refs.
    pub(super) fn expire_stale(&self, git_dir: &Path, now: SystemTime) {
        let workspace = self.workspace.to_string_lossy().into_owned();
        for task in task_state::load_all(git_dir) {
            if task.workspace != workspace {
                continue;
            }
            if !task.is_expired(now, task_state::DEFAULT_EXPIRY)
                && !self.persisted_paths_clean_in_git(&task)
            {
                continue;
            }
            // Never touch a live foreign task, even an old one (ADR-0030): claim
            // its lease non-blocking first. A held lease means a live process
            // still owns it -- skip. For a legacy record with no lease file the
            // claim creates and takes it (lease-free), so legacy expiry still
            // works. Holding the claimed lease through teardown also blocks a
            // concurrent adopt of this orphan mid-delete.
            let lease = match lock::try_exclusive_settled(&lock::lease_path(git_dir, &task.task_id))
            {
                Ok(Some(guard)) => guard,
                _ => continue,
            };
            lock::with_mutation_lock(git_dir, || {
                let _ = checkpoint::destroy_task_refs(&self.workspace, &task.task_id);
                task_state::remove(git_dir, &task.task_id);
            });
            drop(lease);
        }
        self.sweep_orphan_checkpoint_refs(git_dir);
    }

    /// Repair already-polluted repos by deleting checkpoint namespaces that have
    /// no surviving task record. Checkpoint refs are stored in the shared ref
    /// namespace, so linked worktree records must be considered together: a
    /// namespace is orphaned only when no worktree git-dir has a record for it.
    /// A stale lease file, if present, must also be claimable before deletion.
    fn sweep_orphan_checkpoint_refs(&self, git_dir: &Path) {
        let task_ids = match checkpoint::list_checkpoint_task_ids(&self.workspace) {
            Ok(task_ids) => task_ids,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "checkpoint orphan-ref sweep could not list refs");
                return;
            }
        };
        if task_ids.is_empty() {
            return;
        }

        let git_dirs = repo_git_dirs(&self.workspace, git_dir);
        let recorded: BTreeSet<String> = git_dirs
            .iter()
            .flat_map(|dir| task_state::load_all(dir))
            .map(|task| task.task_id)
            .collect();

        for task_id in task_ids {
            if recorded.contains(&task_id) {
                continue;
            }
            let Some(lease_claims) = claim_existing_leases(&git_dirs, &task_id) else {
                continue;
            };
            let destroyed = lock::with_mutation_lock(
                git_dir,
                || match checkpoint::destroy_task_refs(&self.workspace, &task_id) {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(task_id = %task_id, error = %format!("{error:#}"), "checkpoint orphan-ref sweep failed");
                        false
                    }
                },
            );
            if destroyed {
                for (path, _guard) in lease_claims {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }

    /// Enumerate this workspace's unsettled records and classify each (ADR-0030).
    /// A **leased** record is a live foreign task -- skipped, never returned, so
    /// a caller can never adopt or checkpoint a task another process owns. A
    /// **lease-free, lock-protocol** record is a recoverable orphan; a record
    /// with no lock metadata is unknown-**legacy**. Returns the recoverable +
    /// legacy records only. This is the seam the #288 resume picker plugs into.
    pub(crate) fn recoverable_tasks(&self) -> Vec<RecoverableTask> {
        if !self.workflow_enabled {
            return Vec::new();
        }
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
            // Never list a live foreign task: probe the record's lease FIRST. If
            // any process holds it the task is live and owned elsewhere -- skip
            // it regardless of protocol. A legacy record another process has
            // adopted holds its lease while still reading lock_protocol=None, so
            // classifying before the lease check would wrongly list it
            // (ADR-0030/0031 invariant: leased tasks are never listed).
            if !lock::is_lease_free(&lock::lease_path(&git_dir, &task.task_id)) {
                continue;
            }
            // Lease-free: classify by lock protocol. The known protocol is a
            // recoverable orphan; a missing (legacy) or unknown/future protocol
            // is surfaced as unknown and never auto-adopted (ADR-0030 degrade
            // direction -- spurious "unknown", never adoption of a task we
            // cannot vet).
            let class = if task.lock_protocol.as_deref() == Some(lock::LOCK_PROTOCOL) {
                TaskClass::Recoverable
            } else {
                TaskClass::Legacy
            };
            let age = now
                .duration_since(UNIX_EPOCH + Duration::from_millis(task.updated_ms))
                .unwrap_or_default();
            out.push(RecoverableTask {
                task_id: task.task_id,
                workspace: task.workspace,
                age,
                body: task.body,
                sessions: task.sessions,
                class,
            });
        }
        // Deterministic order so the picker's default (first) row is stable
        // across runs -- `load_all` reflects nondeterministic `read_dir` order.
        // Newest-updated first (smallest age), `task_id` as a stable tie-break.
        out.sort_by(|a, b| a.age.cmp(&b.age).then_with(|| a.task_id.cmp(&b.task_id)));
        out
    }

    /// Adopt a recoverable orphan by id (ADR-0030): claim its lease (bailing if a
    /// live process grabbed it first), reconcile disk vs the op-log (append a
    /// FULL recovery snapshot for any diverged path, under the mutation lock),
    /// and rehydrate it as this process's active task so a post-restart
    /// `/rollback` / `/accept` / `/checkpoint` operates on the real chain.
    /// Returns the adopted record so the caller's notice names the record it
    /// actually acted on.
    pub(super) fn adopt_task(
        &self,
        task_id: &str,
    ) -> Result<task_state::PersistedTask, AdoptError> {
        if !matches!(self.mode, Mode::Git) {
            return Err(AdoptError::Unavailable);
        }
        if !self.workflow_enabled {
            return Err(AdoptError::Unavailable);
        }
        if self.state.lock().unwrap().task.is_some() {
            return Err(AdoptError::TaskActive);
        }
        let git_dir = task_state::git_dir(&self.workspace).ok_or(AdoptError::Unavailable)?;
        let workspace = self.workspace.to_string_lossy().into_owned();
        let record = task_state::load_all(&git_dir)
            .into_iter()
            .find(|task| task.task_id == task_id && task.workspace == workspace)
            .ok_or(AdoptError::Unavailable)?;
        // Claim the lease for the task's lifetime. If a live process holds it,
        // this is a foreign live task -- do not adopt.
        let lease = match lock::try_exclusive_settled(&lock::lease_path(&git_dir, task_id)) {
            Ok(Some(guard)) => guard,
            _ => return Err(AdoptError::Unavailable),
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
        if self.rehydrate_task(&git_dir, &record, lease) {
            Ok(record)
        } else if self.state.lock().unwrap().task.is_some() {
            Err(AdoptError::TaskActive)
        } else {
            Err(AdoptError::Unavailable)
        }
    }

    /// Adopt a recoverable task and surface just its opaque display payload to
    /// Tier 3 (#288): the same [`adopt_task`](Self::adopt_task) side effects
    /// (lease claim, disk reconciliation, chain rehydration, session-join
    /// append), mapped to an [`AdoptedTask`] so the private record type stays
    /// behind the seam.
    pub(crate) fn adopt(&self, task_id: &str) -> Result<AdoptedTask, AdoptError> {
        let record = self.adopt_task(task_id)?;
        Ok(AdoptedTask {
            task_id: record.task_id,
            body: record.body,
            sessions: record.sessions,
        })
    }

    /// Rebuild an active [`Task`] from a persisted unsettled record and its
    /// durable `refs/iris/*` chain (ADR-0028 crash recovery), so settlement
    /// operations work after a restart. The chain is loaded from refs; the
    /// baseline is re-captured against the current disk (so continued mutation
    /// still gates today's dirty files -- the safe direction) but its index is
    /// the ORIGINAL staged state from the record, the selection a rollback must
    /// restore. The caller's already-acquired `lease` is moved onto the task so
    /// this process holds ownership for the task's lifetime. Returns whether the
    /// task became active.
    fn rehydrate_task(
        &self,
        git_dir: &Path,
        persisted: &task_state::PersistedTask,
        lease: lock::FlockGuard,
    ) -> bool {
        let session_id = {
            let state = self.state.lock().unwrap();
            if state.task.is_some() {
                return false;
            }
            state.session_id.clone()
        };
        let ledger_paths: Vec<PathBuf> = persisted.expected.keys().map(PathBuf::from).collect();
        let chain = match CheckpointChain::load(
            self.workspace.clone(),
            persisted.task_id.clone(),
            &ledger_paths,
        ) {
            Ok(chain) => chain,
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "could not rehydrate checkpoint chain on resume");
                return false;
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
        let linked_worktree = is_linked_worktree(&self.workspace, Some(git_dir));
        let mut task = Task::active(
            persisted.task_id.clone(),
            baseline,
            chain,
            Some(git_dir.to_path_buf()),
            linked_worktree,
            Some(lease),
        );
        task.created_ms = persisted.created_ms;
        // Carry the record's opaque display payload (ADR-0031) so a later
        // `persist_task` (on continued mutation) re-writes it verbatim instead
        // of clobbering it. Then append THIS process's session id to the live
        // join (ordered, consecutive-deduped) and persist just that under the
        // mutation lock -- preserving `expected`/`tip_seq`, which a fresh
        // `persist_task` (empty ledger) would reset. Never read by enforcement.
        task.body = persisted.body.clone();
        task.sessions = persisted.sessions.clone();
        if let Some(id) = &session_id {
            push_session_deduped(&mut task.sessions, id);
        }
        if task.sessions != persisted.sessions {
            let mut updated = persisted.clone();
            updated.sessions = task.sessions.clone();
            lock::with_mutation_lock(git_dir, || {
                if let Err(error) = task_state::save(git_dir, &updated) {
                    tracing::warn!(error = %format!("{error:#}"), "failed to persist session link on adopt");
                }
            });
        }
        let mut state = self.state.lock().unwrap();
        if state.task.is_none() {
            state.task = Some(task);
            true
        } else {
            false
        }
    }
}

/// Git exposes checkpoint refs through the common ref store, while Iris task
/// records live beside each linked worktree's git-dir. Collect every git-dir in
/// this repository so an orphan-ref sweep does not delete a namespace still
/// recorded by another linked worktree.
fn repo_git_dirs(workspace: &Path, current_git_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = BTreeSet::new();
    dirs.insert(current_git_dir.to_path_buf());

    if let Ok(out) = git::git_stdout(workspace, &["worktree", "list", "--porcelain"]) {
        let text = String::from_utf8_lossy(&out);
        for line in text.lines() {
            let Some(path) = line.strip_prefix("worktree ") else {
                continue;
            };
            if let Some(git_dir) = task_state::git_dir(Path::new(path)) {
                dirs.insert(git_dir);
            }
        }
    }

    dirs.into_iter().collect()
}

/// Claim any existing stale lease files for `task_id`. Missing files are already
/// lease-free and are not created just for the sweep. Any held/erroring lease
/// blocks deletion in the safe direction.
fn claim_existing_leases(
    git_dirs: &[PathBuf],
    task_id: &str,
) -> Option<Vec<(PathBuf, lock::FlockGuard)>> {
    let mut claims = Vec::new();
    for git_dir in git_dirs {
        let path = lock::lease_path(git_dir, task_id);
        if !path.exists() {
            continue;
        }
        match lock::try_exclusive_settled(&path) {
            Ok(Some(guard)) => claims.push((path, guard)),
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(task_id = %task_id, path = %path.display(), error = %error, "checkpoint orphan-ref sweep could not claim lease");
                return None;
            }
        }
    }
    Some(claims)
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

#[cfg(test)]
mod recovery_outcome_tests {
    use super::*;

    #[test]
    fn recoverable_count_includes_explicit_resume_offer() {
        assert_eq!(RecoveryOutcome::None.recoverable_count(), 0);
        assert_eq!(
            RecoveryOutcome::Notice("adopted".to_string()).recoverable_count(),
            0
        );
        assert_eq!(
            RecoveryOutcome::ResumeLinked(RecoverableTask::for_test(
                "linked",
                Duration::from_secs(60),
                Some("x"),
                &["session-a"],
            ))
            .recoverable_count(),
            1
        );
        let tasks = vec![
            RecoverableTask::for_test("a", Duration::from_secs(60), Some("x"), &[]),
            RecoverableTask::for_test_legacy("b", Duration::from_secs(60)),
        ];
        assert_eq!(RecoveryOutcome::Picker(tasks).recoverable_count(), 2);
    }

    #[test]
    fn resumed_session_offer_requires_exactly_one_recoverable_link() {
        let linked = RecoverableTask::for_test(
            "linked",
            Duration::from_secs(60),
            Some("work"),
            &["session-a"],
        );
        let other = RecoverableTask::for_test(
            "other",
            Duration::from_secs(60),
            Some("other"),
            &["session-b"],
        );
        let tasks = vec![linked.clone(), other.clone()];
        assert_eq!(
            linked_recoverable_for_session(&tasks, "session-a")
                .map(|task| task.task_id)
                .as_deref(),
            Some("linked")
        );
        assert!(
            linked_recoverable_for_session(&tasks, "missing").is_none(),
            "zero linked tasks should not offer"
        );

        let two_linked = vec![
            linked,
            RecoverableTask::for_test(
                "second",
                Duration::from_secs(60),
                Some("second"),
                &["session-a"],
            ),
            other,
        ];
        assert!(
            linked_recoverable_for_session(&two_linked, "session-a").is_none(),
            "multiple linked tasks are never guessed between"
        );

        let legacy = RecoverableTask {
            task_id: "legacy".to_string(),
            workspace: "/proj".to_string(),
            age: Duration::from_secs(60),
            body: None,
            sessions: vec!["session-a".to_string()],
            class: TaskClass::Legacy,
        };
        assert!(
            linked_recoverable_for_session(&[legacy], "session-a").is_none(),
            "legacy rows are not recoverable auto-offers"
        );
    }
}
