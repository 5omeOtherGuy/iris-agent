//! Tier-2 dirty-tree safety (issue #262, ADR-0028).
//!
//! [`GitSafety`] is the Tier-2 implementation of the Tier-1 [`MutationGuard`]
//! contract. It owns all git knowledge (baseline capture, hashing, status
//! scans); the core loop asks through the trait and stays git-free. It realizes
//! the foundation of Milestone 5:
//!
//! - **Settlement-based task boundaries.** A task opens lazily at the first
//!   mutating tool call (baseline captured then, never for pure Q&A) and closes
//!   at [`settle`](GitSafety::settle) -- the seam #263 (checkpoint/rollback)
//!   plugs into. The next mutation opens a fresh task with a fresh baseline.
//! - **Choke-point gate.** [`unapproved_protected`](MutationGuard::unapproved_protected)
//!   answers whether an `edit`/`write` target is a pre-existing dirty file the
//!   loop must route through approval; approvals are per-file, per-task, with an
//!   "all dirty files" escalation, and expire at settlement.
//! - **Bash detection + restore.** [`before_exec`](MutationGuard::before_exec)
//!   snapshots the protected set; [`after_exec`](MutationGuard::after_exec)
//!   re-checks it (blocking, cheap) and flags any out-of-band change as a
//!   violation the loop halts and restores from
//!   [`restore`](MutationGuard::restore).
//! - **Attribution ledger.** Approved Iris changes and post-command git-status
//!   changes are recorded (async scan, joined at sync barriers).
//! - **Honest degrade.** A non-git directory or a `.jj/` workspace disables
//!   gating and announces itself; it never fakes a guarantee.
//!
//! Deliberately out of scope here (later issues): `refs/iris/*` checkpoints,
//! rollback, the final diff, and the verification loop (#263/#264/#265). The
//! [`ledger`] op-log, [`settle`](GitSafety::settle) hook, and [`snapshot`]
//! storage are the seams they build on.

mod baseline;
mod checkpoint;
pub(crate) mod git;
mod ledger;
mod lock;
mod net_diff;
mod settlement;
mod snapshot;
mod task_state;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::SystemTime;

use anyhow::Result;

use crate::nexus::MutationGuard;

use baseline::Baseline;
use checkpoint::{CheckpointChain, Mode as FileMode};
use ledger::{Attribution, Ledger, LedgerEntry};
pub(crate) use settlement::{AdoptError, AdoptedTask, RecoverableTask, RecoveryOutcome};
use settlement::{checkpoint_label, new_task_id};
use snapshot::{FallbackStore, Snapshot};

pub(crate) use net_diff::TaskNetDiff;

/// A ledger path's pre-task content for the checkpoint chain: the exact bytes
/// and file mode captured before Iris first touched it, or `None` when the path
/// did not exist pre-task (a create).
type PreImage = Option<(Vec<u8>, FileMode)>;

/// One confirmed Iris change fed to the checkpoint chain: the touched path plus
/// its captured pre-task image.
type IrisChange = (PathBuf, PreImage);

/// A restore point offered to the user by the rollback UI (Tier 3 renders it).
/// `seq` names the checkpoint (0 = pre-task baseline); `label` is the op-log
/// description.
#[derive(Debug, Clone)]
pub(crate) struct RestorePoint {
    pub(crate) seq: u64,
    pub(crate) label: String,
}

/// A settled task (accept / explicit checkpoint): the user-facing summary plus
/// the settled task id (ADR-0031), so the harness can append a `TaskSettled`
/// audit entry. The id is metadata for the session-log join; enforcement never
/// keys off it.
#[derive(Debug, Clone)]
pub(crate) struct Settlement {
    pub(crate) summary: String,
    pub(crate) task_id: String,
}

pub(crate) const EXTERNAL_SETTLEMENT_NOTICE: &str = "Iris changes committed by you — task closed.";

/// Outcome of a rollback attempt, surfaced to the user (Tier 3).
pub(crate) struct RollbackOutcome {
    /// Human summary of what was restored.
    pub(crate) summary: String,
    /// The settled task id when a task was actually rolled back (ADR-0031), for
    /// the harness's `TaskSettled` audit append; `None` when there was no active
    /// task. Display/join metadata only -- enforcement never reads it.
    pub(crate) settled_task_id: Option<String>,
    /// Set when the user index could not be safely restored (mid-merge/rebase):
    /// the degrade-to-detect-and-warn path (ADR-0028).
    pub(crate) index_warning: Option<String>,
    /// Per-path notices for ledger paths left untouched because the user edited
    /// them after Iris's last write (ADR-0028 TOCTOU rule): rollback preserves
    /// the user's newer bytes instead of clobbering them.
    pub(crate) preserved_notices: Vec<String>,
}

/// How the guard operates for this workspace.
enum Mode {
    /// A git working tree: full baseline + gating.
    Git,
    /// Non-git, `.jj/`, or a capture failure: no gating, honest notice. The
    /// string is the one-line reason surfaced once at the first mutation.
    Degraded(String),
}

/// The task's rollback store: a git-object checkpoint chain for a git worktree,
/// or plain content snapshots for a degraded (non-git / jj) workspace.
enum Chain {
    Git(CheckpointChain),
    Fallback(FallbackStore),
}

/// Per-task state, created lazily at the first mutating call.
struct Task {
    /// Whether this in-memory guard state is backed by the durable task
    /// workflow (records, leases, checkpoint refs, lifecycle UI). Guard-only
    /// tasks still protect dirty files and keep an in-memory ledger, but are not
    /// user-facing workflow tasks.
    durable: bool,
    /// `true` for a degraded task: no baseline, no gating.
    degraded: bool,
    /// Stable id anchoring the `refs/iris/checkpoints/<task-id>/` namespace and
    /// the persisted recovery record.
    task_id: String,
    baseline: Baseline,
    ledger: Ledger,
    /// Per-file approvals granted this task (normalized absolute paths).
    approved: BTreeSet<PathBuf>,
    /// The "all dirty files (this task)" escalation.
    all_dirty_approved: bool,
    /// Pre-call byte snapshot of the protected set + this call's targets
    /// (refreshed each call).
    snapshot: Snapshot,
    /// Pre-call file modes for the snapshotted paths, so a checkpoint captures a
    /// path's pre-mutation mode (refreshed each call alongside `snapshot`).
    pre_modes: BTreeMap<PathBuf, FileMode>,
    /// Task-local op sequence, advanced per recorded mutation.
    turn: u64,
    /// Rollback store (git chain or content-snapshot fallback).
    chain: Chain,
    /// Resolved git directory for task-local metadata. Durable workflow tasks
    /// persist recovery records here; guard-only tasks keep it only to detect a
    /// linked worktree that was later removed from Git's registry.
    git_dir: Option<PathBuf>,
    /// Whether this task opened in a linked Git worktree. If that worktree's
    /// admin dir and `.git` file both disappear during cleanup, any leftover
    /// files are orphaned, not protected worktree state to resurrect.
    linked_worktree: bool,
    /// When this task first opened (epoch millis), for the expiry sweep.
    created_ms: u64,
    /// The per-task advisory `flock` lease, held for the task's lifetime
    /// (ADR-0030). Kept only to be dropped at settlement -- closing the fd
    /// releases the lease, so recovery in another process sees the task as
    /// orphaned. `None` in degraded mode or when the lease could not be
    /// acquired (a logged, best-effort degrade).
    _lease: Option<lock::FlockGuard>,
    /// Opaque display body: the prompt preview of the turn whose first mutation
    /// opened this task (ADR-0031), captured once and never rewritten. Written
    /// verbatim into the persisted record; NO enforcement path reads it.
    body: Option<String>,
    /// Opaque display join: session ids that worked this task, ordered and
    /// consecutive-deduped (ADR-0031). Written verbatim into the record; NO
    /// enforcement path reads it.
    sessions: Vec<String>,
    /// True when the current clean ledger state was observed immediately after
    /// Iris/tool execution. External settlement waits for a dirty->clean
    /// transition that happens outside Iris.
    external_settlement_blocked_while_clean: bool,
}

impl Task {
    fn active(
        task_id: String,
        baseline: Baseline,
        chain: CheckpointChain,
        git_dir: Option<PathBuf>,
        linked_worktree: bool,
        lease: Option<lock::FlockGuard>,
    ) -> Self {
        Self {
            durable: true,
            degraded: false,
            task_id,
            baseline,
            ledger: Ledger::default(),
            approved: BTreeSet::new(),
            all_dirty_approved: false,
            snapshot: Snapshot::default(),
            pre_modes: BTreeMap::new(),
            turn: 0,
            chain: Chain::Git(chain),
            git_dir,
            linked_worktree,
            created_ms: task_state::now_ms(),
            _lease: lease,
            body: None,
            sessions: Vec::new(),
            external_settlement_blocked_while_clean: false,
        }
    }

    fn guard_only(
        task_id: String,
        baseline: Baseline,
        git_dir: Option<PathBuf>,
        linked_worktree: bool,
    ) -> Self {
        Self {
            durable: false,
            degraded: false,
            task_id,
            baseline,
            ledger: Ledger::default(),
            approved: BTreeSet::new(),
            all_dirty_approved: false,
            snapshot: Snapshot::default(),
            pre_modes: BTreeMap::new(),
            turn: 0,
            chain: Chain::Fallback(FallbackStore::default()),
            git_dir,
            linked_worktree,
            created_ms: task_state::now_ms(),
            _lease: None,
            body: None,
            sessions: Vec::new(),
            external_settlement_blocked_while_clean: false,
        }
    }

    fn degraded(task_id: String, durable: bool) -> Self {
        Self {
            durable,
            degraded: true,
            task_id,
            baseline: Baseline {
                protected: Default::default(),
                dirty_count: 0,
                untracked_count: 0,
                index: String::new(),
            },
            ledger: Ledger::default(),
            approved: BTreeSet::new(),
            all_dirty_approved: false,
            snapshot: Snapshot::default(),
            pre_modes: BTreeMap::new(),
            turn: 0,
            chain: Chain::Fallback(FallbackStore::default()),
            git_dir: None,
            linked_worktree: false,
            created_ms: task_state::now_ms(),
            _lease: None,
            body: None,
            sessions: Vec::new(),
            external_settlement_blocked_while_clean: false,
        }
    }
}

struct State {
    task: Option<Task>,
    /// Clean-ledger settlement events observed inside sync barriers. The guard
    /// can detect the repository state, but the Wayland harness owns session
    /// lifecycle writes and UI notices, so callers drain these after barriers.
    external_settlements: Vec<Settlement>,
    /// The current turn's prompt preview, handed by the harness before each turn
    /// (ADR-0031). `note_mutation` consumes it as the opening task's `body` and
    /// clears it; a follow-up turn joining an unsettled task clears it without
    /// rewriting the body. Opaque display payload only.
    pending_body: Option<String>,
    /// The current session id, stamped onto a task's `sessions` join at open and
    /// appended (consecutive-deduped) when a task is rehydrated/adopted. Opaque
    /// display payload only.
    session_id: Option<String>,
}

/// Append `session_id` to a task's `sessions` join, skipping a consecutive
/// duplicate so re-adopting from the same session never grows the vec (ADR-0031
/// ordered, consecutive-deduped). Opaque display payload; never read by
/// enforcement.
fn push_session_deduped(sessions: &mut Vec<String>, session_id: &str) {
    if sessions.last().map(String::as_str) != Some(session_id) {
        sessions.push(session_id.to_string());
    }
}

fn push_recent_path(paths: &mut Vec<String>, path: String) {
    if let Some(pos) = paths.iter().position(|existing| *existing == path) {
        paths.remove(pos);
    }
    paths.push(path);
}

/// The Tier-2 dirty-tree safety guard. Owned by the harness, injected into each
/// turn's `ToolEnv` as a `&dyn MutationGuard`.
pub(crate) struct GitSafety {
    /// Canonicalized workspace root; the anchor for path normalization.
    workspace: PathBuf,
    mode: Mode,
    workflow_enabled: bool,
    state: Mutex<State>,
    /// In-flight async attribution scan, joined at every sync barrier.
    scan: Mutex<Option<JoinHandle<Vec<LedgerEntry>>>>,
}

impl GitSafety {
    /// Build the guard for `workspace`, detecting git vs degraded mode once.
    pub(crate) fn new(workspace: &Path) -> Self {
        Self::new_with_workflow(workspace, true)
    }

    /// Build the guard with the durable task workflow explicitly enabled or
    /// disabled. The dirty-tree guard runs in both modes; this flag gates only
    /// records, leases, refs, recovery, and user-facing task UI.
    pub(crate) fn new_with_workflow(workspace: &Path, workflow_enabled: bool) -> Self {
        let canonical = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let mode = detect_mode(&canonical);
        Self {
            workspace: canonical,
            mode,
            workflow_enabled,
            state: Mutex::new(State {
                task: None,
                external_settlements: Vec::new(),
                pending_body: None,
                session_id: None,
            }),
            scan: Mutex::new(None),
        }
    }

    /// Display-only join index for the resume picker: every session id recorded
    /// on an unsettled task in this workspace. Unlike recovery classification,
    /// this does not probe leases or branch on task metadata; it only projects
    /// the opaque ADR-0031 `sessions` payload for UI markers.
    pub(crate) fn task_linked_session_ids(&self) -> BTreeSet<String> {
        if !self.workflow_enabled || !matches!(self.mode, Mode::Git) {
            return BTreeSet::new();
        }
        let Some(git_dir) = task_state::git_dir(&self.workspace) else {
            return BTreeSet::new();
        };
        let workspace = self.workspace.to_string_lossy().into_owned();
        task_state::load_all(&git_dir)
            .into_iter()
            .filter(|task| task.workspace == workspace)
            .flat_map(|task| task.sessions)
            .collect()
    }

    /// Flip the durable workflow flag at an inter-turn boundary. Disabling is
    /// refused while a durable task is active because hiding it would knowingly
    /// orphan review/rollback state. Enabling while a guard-only task is active
    /// starts the durable workflow at the next mutation; prior guard-only
    /// changes remain protected by the safety floor but never gain retroactive
    /// rollback history.
    pub(crate) fn set_workflow_enabled(
        &mut self,
        enabled: bool,
    ) -> std::result::Result<(), &'static str> {
        self.sync_barrier();
        if !enabled && self.has_active_workflow_task() {
            return Err("finish the current task before disabling task workflow");
        }
        if enabled && !self.workflow_enabled {
            let mut state = self.state.lock().unwrap();
            if state.task.as_ref().is_some_and(|task| !task.durable) {
                state.task = None;
            }
        }
        self.workflow_enabled = enabled;
        Ok(())
    }

    /// Settle the current task (ADR-0028 settlement boundary): join any pending
    /// scan, freeze and drop the ledger/approvals, so the next mutation opens a
    /// fresh baseline. This is the seam #263 replaces with commit/rollback
    /// settlement; the checkpoint chain hangs off the same call. Only an
    /// explicit user action settles (accept/rollback/checkpoint) -- passive
    /// actions like a session swap use [`discard_approvals`](Self::discard_approvals)
    /// instead. Exercised by tests; wired into production by #263.
    #[allow(dead_code)]
    pub(crate) fn settle(&self) {
        self.sync_barrier();
        self.state.lock().unwrap().task = None;
    }

    /// Drop per-task approvals WITHOUT settling the task (ADR-0028: session end
    /// and other passive actions never settle -- settlement is accept, rollback,
    /// or an explicit checkpoint only). A session swap (`/new`/`/resume`) is such
    /// a passive boundary: it must not mark the dirty task accepted or reset the
    /// baseline's protection. The task's baseline/ledger persist so protection
    /// survives the swap; only the per-file approvals -- judged against the prior
    /// conversation -- are cleared, so the next touch of a still-dirty file
    /// re-prompts (the safe direction). The resume/recovery notice is #263.
    pub(crate) fn discard_approvals(&self) {
        let mut state = self.state.lock().unwrap();
        if let Some(task) = state.task.as_mut() {
            task.approved.clear();
            task.all_dirty_approved = false;
        }
    }

    /// Set the current turn's prompt preview (ADR-0031), handed by the harness
    /// before each turn. `note_mutation` consumes it as the opening task's
    /// opaque `body` if this turn opens a task, and clears it otherwise so a
    /// follow-up turn never rewrites an existing task's body. Deterministic
    /// display payload; no enforcement path reads it.
    pub(crate) fn set_turn_context(&self, preview: Option<String>) {
        self.state.lock().unwrap().pending_body = preview;
    }

    /// Set the current session id (ADR-0031): stamped onto a task's opaque
    /// `sessions` join at open, and appended (consecutive-deduped) when this
    /// process rehydrates/adopts a task. Display payload only.
    pub(crate) fn set_session_id(&self, id: String) {
        self.state.lock().unwrap().session_id = Some(id);
    }

    /// The active task's id, or `None` when no task is open (ADR-0031). Polled by
    /// the harness post-turn to observe "a task opened this turn" so it can
    /// append the `TaskOpened` audit entry.
    pub(crate) fn current_task_id(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .task
            .as_ref()
            .filter(|task| task.durable)
            .map(|task| task.task_id.clone())
    }

    /// Read-only display payload of the ACTIVE (live, unsettled) task, for the
    /// unified task UI (ADR-0031): its id plus opaque display copy and live
    /// approval scope. `None` when no task is open. Display-only observation --
    /// no enforcement path reads it, and reading it never mutates task state.
    pub(crate) fn active_task_display(&self) -> Option<ActiveTaskDisplay> {
        let state = self.state.lock().unwrap();
        let task = state.task.as_ref()?;
        if !task.durable {
            return None;
        }
        Some(ActiveTaskDisplay {
            task_id: task.task_id.clone(),
            body: task.body.clone(),
            sessions: task.sessions.clone(),
            approved_paths: task
                .approved
                .iter()
                .map(|path| self.workspace_display_path(path))
                .collect(),
            all_dirty_approved: task.all_dirty_approved,
        })
    }

    /// Display-only compaction carry for an open durable task. It joins pending
    /// attribution first, then returns the opaque task body plus bounded
    /// workspace-relative ledger paths. Enforcement never reads this payload; it
    /// exists only so compacted context can remind the model that unreviewed Iris
    /// changes are still open.
    pub(crate) fn active_task_compaction_state(
        &self,
        max_paths: usize,
    ) -> Option<(Option<String>, Vec<String>)> {
        self.sync_barrier();
        let state = self.state.lock().unwrap();
        let task = state.task.as_ref()?;
        if !task.durable {
            return None;
        }
        let mut paths = Vec::new();
        for entry in &task.ledger.entries {
            if let Some(path) = self.workspace_compaction_path(&entry.path) {
                push_recent_path(&mut paths, path);
            }
        }
        if paths.is_empty()
            && let Chain::Git(chain) = &task.chain
        {
            for path in chain.ledger_paths() {
                if let Some(path) = self.workspace_compaction_path(path) {
                    push_recent_path(&mut paths, path);
                }
            }
        }
        if paths.len() > max_paths {
            paths.drain(0..paths.len() - max_paths);
        }
        let body = task
            .body
            .as_deref()
            .filter(|body| !body.is_empty())
            .map(str::to_string);
        (body.is_some() || !paths.is_empty()).then_some((body, paths))
    }

    /// Number of ledger entries recorded in the current task (test-only).
    #[cfg(test)]
    pub(crate) fn ledger_len(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .task
            .as_ref()
            .map(|task| task.ledger.entries.len())
            .unwrap_or(0)
    }

    pub(crate) fn has_ledger_entries(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .task
            .as_ref()
            .is_some_and(|task| !task.ledger.entries.is_empty())
    }

    pub(crate) fn has_active_workflow_task(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .task
            .as_ref()
            .is_some_and(|task| task.durable)
    }

    /// Drain externally-observed settlements. This joins any pending attribution
    /// first, so a user commit/revert after Iris's last write closes the task at
    /// the same hard barrier that final diff/rollback already depend on.
    pub(crate) fn drain_external_settlements(&self) -> Vec<Settlement> {
        self.sync_barrier();
        self.state
            .lock()
            .unwrap()
            .external_settlements
            .drain(..)
            .collect()
    }

    /// The captured index (`git ls-files --stage`) of the current baseline
    /// (test-only): asserts the index is part of the baseline.
    #[cfg(test)]
    pub(crate) fn baseline_index(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .task
            .as_ref()
            .map(|task| task.baseline.index.clone())
    }

    /// Whether a task baseline is currently captured (test-only): asserts the
    /// lazy-capture boundary (no baseline until the first mutation).
    #[cfg(test)]
    pub(crate) fn has_task(&self) -> bool {
        self.state.lock().unwrap().task.is_some()
    }

    /// Normalize a path to an absolute, symlink-resolved form so a
    /// baseline-captured git path and a tool-supplied path compare equal.
    /// Falls back to resolving the parent (for a not-yet-existing target) and
    /// finally to the lexical absolute path.
    fn normalize(&self, path: &Path) -> PathBuf {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        };
        if let Ok(canonical) = absolute.canonicalize() {
            return canonical;
        }
        match (absolute.parent(), absolute.file_name()) {
            (Some(parent), Some(name)) => match parent.canonicalize() {
                Ok(parent) => parent.join(name),
                Err(_) => absolute,
            },
            _ => absolute,
        }
    }

    fn workspace_display_path(&self, path: &Path) -> String {
        crate::display_path::workspace_path(&self.workspace, path)
    }

    fn workspace_compaction_path(&self, path: &Path) -> Option<String> {
        crate::display_path::workspace_path_if_inside(&self.workspace, path)
    }

    fn protected_path_summary(&self, baseline: &Baseline, max: usize) -> String {
        let paths: Vec<String> = baseline
            .protected
            .keys()
            .map(|path| self.workspace_display_path(path))
            .collect();
        bounded_path_summary(&paths, max)
    }

    /// Join the in-flight attribution scan (if any) and fold its entries into
    /// the ledger, then check for user-controlled external settlement. A hard
    /// sync barrier per ADR-0028: called at settlement and user/harness
    /// boundaries. Must not be called while holding the `state` lock.
    fn sync_barrier(&self) {
        self.join_attribution_scan();
        if let Some(settlement) = self.settle_external_if_clean() {
            self.state
                .lock()
                .unwrap()
                .external_settlements
                .push(settlement);
        }
    }

    fn join_attribution_scan(&self) {
        let handle = self.scan.lock().unwrap().take();
        if let Some(handle) = handle
            && let Ok(entries) = handle.join()
        {
            let mut state = self.state.lock().unwrap();
            if let Some(task) = state.task.as_mut() {
                let mut touched: Vec<PathBuf> = Vec::new();
                for entry in entries {
                    touched.push(entry.path.clone());
                    task.ledger.record(entry);
                }
                // Fold the async bash attribution into the checkpoint chain: for
                // a previously-clean file a command changed, capture its
                // committed content as the pre-task image (best-effort) and open
                // a restore point over the running diff. A file with no committed
                // predecessor is treated as a create (base rollback deletes it).
                // As in `after_exec`, these per-task ref writes are single-writer
                // under the held lease and need no mutation lock; the shared
                // record write in `persist_task` is the serialized one.
                if !touched.is_empty()
                    && let Chain::Git(chain) = &mut task.chain
                {
                    for path in &touched {
                        let pre = checkpoint::committed_blob(&self.workspace, path);
                        if let Err(error) = chain.note_before(path, pre) {
                            tracing::warn!(error = %format!("{error:#}"), "bash checkpoint pre-image capture failed");
                        }
                    }
                    let turn = task.turn;
                    if let Err(error) = chain.checkpoint(turn, None, "bash change".to_string()) {
                        tracing::warn!(error = %format!("{error:#}"), "bash checkpoint create failed");
                    }
                }
                self.persist_task(task);
            }
        }
    }

    pub(crate) fn observe_iris_execution_boundary(&self) {
        self.join_attribution_scan();
        let paths = {
            let state = self.state.lock().unwrap();
            let Some(task) = state.task.as_ref() else {
                return;
            };
            if !task.durable
                || task.degraded
                || task.ledger.entries.is_empty()
                || !matches!(self.mode, Mode::Git)
            {
                return;
            }
            task.ledger
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>()
        };
        let clean = self.ledger_paths_clean_in_git(paths.iter().map(PathBuf::as_path));
        if let Some(task) = self.state.lock().unwrap().task.as_mut() {
            task.external_settlement_blocked_while_clean = clean;
        }
    }

    fn settle_external_if_clean(&self) -> Option<Settlement> {
        let paths = {
            let state = self.state.lock().unwrap();
            let task = state.task.as_ref()?;
            if task.degraded || task.ledger.entries.is_empty() || !matches!(self.mode, Mode::Git) {
                return None;
            }
            task.ledger
                .entries
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>()
        };
        let clean = self.ledger_paths_clean_in_git(paths.iter().map(PathBuf::as_path));
        let mut state = self.state.lock().unwrap();
        if !clean {
            if let Some(task) = state.task.as_mut() {
                task.external_settlement_blocked_while_clean = false;
            }
            return None;
        }
        if state
            .task
            .as_ref()
            .is_some_and(|task| task.external_settlement_blocked_while_clean)
        {
            return None;
        }
        let task = state.task.take()?;
        let durable = task.durable;
        drop(state);
        durable.then(|| self.destroy_settled_task(task, EXTERNAL_SETTLEMENT_NOTICE.to_string()))
    }

    fn persisted_paths_clean_in_git(&self, task: &task_state::PersistedTask) -> bool {
        if task.expected.is_empty() || !matches!(self.mode, Mode::Git) {
            return false;
        }
        self.ledger_paths_clean_in_git(task.expected.keys().map(|path| Path::new(path.as_str())))
    }

    fn ledger_paths_clean_in_git<'a>(&self, paths: impl Iterator<Item = &'a Path>) -> bool {
        let mut scoped = BTreeSet::new();
        for path in paths {
            let display = path
                .strip_prefix(&self.workspace)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            scoped.insert(display);
        }
        if scoped.is_empty() {
            return false;
        }
        let mut args = vec![
            "status".to_string(),
            "--porcelain=v1".to_string(),
            "--untracked-files=all".to_string(),
            "--".to_string(),
        ];
        args.extend(scoped);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        match git::git_stdout(&self.workspace, &refs) {
            Ok(stdout) => stdout.is_empty(),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "could not check task ledger cleanliness");
                false
            }
        }
    }

    fn destroy_settled_task(&self, task: Task, summary: String) -> Settlement {
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
        let destroy_chain = |chain: &mut Chain| match chain {
            Chain::Git(chain) => {
                if let Err(error) = chain.destroy() {
                    tracing::warn!(error = %format!("{error:#}"), "checkpoint teardown on settlement failed");
                }
            }
            Chain::Fallback(_) => {}
        };
        // Serialize the ref teardown + record removal against concurrent processes
        // (ADR-0030): one short mutation-lock hold around the shared writes.
        if let Some(git_dir) = git_dir {
            lock::with_mutation_lock(&git_dir, || {
                destroy_chain(&mut chain);
                task_state::remove(&git_dir, &task_id);
            });
        } else {
            destroy_chain(&mut chain);
        }
        drop(_lease);
        Settlement { summary, task_id }
    }

    /// Persist the current task's minimal recovery record (git tasks only).
    /// Best-effort: a persistence failure is logged, never fatal to the turn.
    fn persist_task(&self, task: &Task) {
        if !task.durable || task.degraded {
            return;
        }
        let Some(git_dir) = task.git_dir.as_ref() else {
            return;
        };
        let tip_seq = match &task.chain {
            Chain::Git(chain) => chain.len() as u64,
            Chain::Fallback(store) => store.len() as u64,
        };
        // Expected on-disk state = the latest recorded content hash per ledger
        // path (later entries win), for resume-time divergence detection.
        let mut expected = BTreeMap::new();
        for entry in &task.ledger.entries {
            expected.insert(
                entry.path.to_string_lossy().into_owned(),
                entry.after.clone(),
            );
        }
        let record = task_state::PersistedTask {
            task_id: task.task_id.clone(),
            workspace: self.workspace.to_string_lossy().into_owned(),
            created_ms: task.created_ms,
            updated_ms: task_state::now_ms(),
            expected,
            tip_seq,
            baseline_index: task.baseline.index.clone(),
            owner: Some(lock::process_owner()),
            lock_protocol: Some(lock::LOCK_PROTOCOL.to_string()),
            // Opaque display payload, written verbatim (ADR-0031). Never read by
            // any enforcement/recovery path.
            body: task.body.clone(),
            sessions: task.sessions.clone(),
        };
        // Serialize the record write against concurrent processes (ADR-0030).
        if let Err(error) = lock::with_mutation_lock(git_dir, || task_state::save(git_dir, &record))
        {
            tracing::warn!(error = %format!("{error:#}"), "failed to persist task recovery record");
        }
    }
}

/// Detect the guard mode for a canonicalized workspace. A `.jj/` colocated
/// workspace degrades like non-git (jj owns the working-copy lifecycle,
/// ADR-0028 interop note); a missing git binary or non-repo also degrades.
fn detect_mode(workspace: &Path) -> Mode {
    if workspace.join(".jj").exists() {
        return Mode::Degraded(
            "jj workspace detected: dirty-tree gating is disabled (jj owns the working copy)"
                .to_string(),
        );
    }
    if git::is_git_worktree(workspace) {
        Mode::Git
    } else {
        Mode::Degraded(
            "not a git repository: dirty-tree safety runs in degraded mode (no approval gating)"
                .to_string(),
        )
    }
}

fn canonical_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn is_linked_worktree(workspace: &Path, git_dir: Option<&Path>) -> bool {
    let Some(git_dir) = git_dir else {
        return false;
    };
    let Some(common_dir) = task_state::git_common_dir(workspace) else {
        return false;
    };
    canonical_or_self(git_dir) != canonical_or_self(&common_dir)
}

fn orphaned_linked_worktree(workspace: &Path, task: &Task) -> bool {
    task.linked_worktree
        && task.git_dir.as_ref().is_some_and(|dir| !dir.exists())
        && !workspace.join(".git").exists()
}

/// Read-only display payload of the active (live, unsettled) task, surfaced to
/// the unified task UI (ADR-0031). Mirrors [`AdoptedTask`](settlement::AdoptedTask):
/// opaque `body`/`sessions` display copy plus the live approval scope, so the
/// private [`Task`] never leaks past the harness. No enforcement or recovery
/// path reads these fields.
#[derive(Debug, Clone)]
pub(crate) struct ActiveTaskDisplay {
    pub(crate) task_id: String,
    pub(crate) body: Option<String>,
    pub(crate) sessions: Vec<String>,
    pub(crate) approved_paths: Vec<String>,
    pub(crate) all_dirty_approved: bool,
}

/// Read-only view of one persisted (unsettled) Iris task, for the session-bar
/// git status snapshot (issue: SessionBar disclosures). Sourced from the
/// durable `<git-dir>/iris/tasks/*.json` records so it can be read from any
/// thread and for any worktree without touching live [`GitSafety`] state.
#[derive(Debug, Clone)]
pub(crate) struct UnsettledTaskView {
    pub(crate) task_id: String,
    /// Time since the record was last updated.
    pub(crate) age: std::time::Duration,
    /// Ledger paths with the op-log's expected on-disk content hash (`None` =
    /// the path should be absent). A path whose current hash matches is an
    /// Iris-unsettled change; a diverged path is user-attributed (the same
    /// certainty rule as rollback's TOCTOU reconciliation).
    pub(crate) expected: Vec<(PathBuf, Option<String>)>,
}

/// Load the unsettled-task records for `workspace` (read-only; no rehydration,
/// no expiry side effects). Empty when the directory is not a git worktree or
/// holds no unsettled task. Task records are per-worktree by construction
/// (linked worktrees resolve to `.git/worktrees/<name>`), so probing each
/// worktree path yields that worktree's own tasks.
pub(crate) fn unsettled_tasks(workspace: &Path) -> Vec<UnsettledTaskView> {
    let Some(git_dir) = task_state::git_dir(workspace) else {
        return Vec::new();
    };
    let canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let workspace_str = canonical.to_string_lossy().into_owned();
    let now = task_state::now_ms();
    task_state::load_all(&git_dir)
        .into_iter()
        .filter(|task| task.workspace == workspace_str)
        .map(|task| UnsettledTaskView {
            age: std::time::Duration::from_millis(now.saturating_sub(task.updated_ms)),
            expected: task
                .expected
                .iter()
                .map(|(path, hash)| (PathBuf::from(path), hash.clone()))
                .collect(),
            task_id: task.task_id,
        })
        .collect()
}

fn bounded_path_summary(paths: &[String], max: usize) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let shown = paths.len().min(max);
    let mut summary = paths[..shown].join(", ");
    if shown < paths.len() {
        summary.push_str(&format!(", +{} more", paths.len() - shown));
    }
    summary
}

/// Attribution scan body (runs on a background thread): re-capture status and
/// attribute any file dirty now but not protected at baseline to Iris (the
/// running command changed a previously-clean file). Best-effort: a capture
/// failure yields no entries.
fn attribution_scan(
    workspace: PathBuf,
    baseline_paths: BTreeSet<PathBuf>,
    turn: u64,
) -> Vec<LedgerEntry> {
    let normalize = |path: &Path| -> PathBuf {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            workspace.join(path)
        };
        absolute.canonicalize().unwrap_or(absolute)
    };
    let Ok(current) = baseline::capture(&workspace, normalize) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    current
        .protected
        .into_iter()
        .filter(|(path, _)| !baseline_paths.contains(path))
        .map(|(path, after)| LedgerEntry {
            path,
            before: None,
            after,
            attribution: Attribution::Iris,
            turn,
            tool_call: None,
            timestamp: now,
        })
        .collect()
}

impl MutationGuard for GitSafety {
    fn note_mutation(&self) -> Option<String> {
        // Join attribution at the start of the next mutating call (ADR-0028),
        // but do not drain external settlement here: this is inside Iris/tool
        // execution, not a user-controlled boundary.
        self.join_attribution_scan();
        let mut state = self.state.lock().unwrap();
        if state.task.is_some() {
            // Baseline already captured and announced for this task. A follow-up
            // turn joining an unsettled task must NOT rewrite its body (captured
            // once at open, ADR-0031), so drop this turn's pending preview.
            state.pending_body = None;
            // The live join, however, must record the CURRENT session: after a
            // passive session swap (/new, /resume) the SAME unsettled task is
            // joined by a new session, and the record's `sessions` vec is the
            // authoritative live join for recovery UX (ADR-0031). Append it
            // (consecutive-deduped, so a same-session follow-up is a no-op) and
            // re-persist so the new join survives a crash -- without touching
            // body. `sessions` stays opaque display payload; no enforcement path
            // reads it.
            let session_id = state.session_id.clone();
            let mut changed = false;
            if let (Some(id), Some(task)) = (session_id.as_ref(), state.task.as_mut()) {
                let before = task.sessions.len();
                push_session_deduped(&mut task.sessions, id);
                changed = task.sessions.len() != before;
            }
            if changed && let Some(task) = state.task.as_ref() {
                self.persist_task(task);
            }
            return None;
        }
        let task_id = new_task_id();
        // Consume the opening turn's prompt preview as the task body (once,
        // never rewritten) and stamp the current session id onto its join. Both
        // are opaque display payload -- no enforcement path reads them.
        let body = state.pending_body.take();
        let session_id = state.session_id.clone();
        match &self.mode {
            Mode::Degraded(reason) => {
                let mut task = Task::degraded(task_id, self.workflow_enabled);
                task.body = body;
                if let Some(id) = session_id {
                    push_session_deduped(&mut task.sessions, &id);
                }
                state.task = Some(task);
                Some(reason.clone())
            }
            Mode::Git => match baseline::capture(&self.workspace, |path| self.normalize(path)) {
                Ok(baseline) => {
                    let announce = baseline.dirty_count > 0 || baseline.untracked_count > 0;
                    let summary = announce.then(|| {
                        let mut summary = format!(
                            "{} dirty and {} untracked file(s) present before this change",
                            baseline.dirty_count, baseline.untracked_count
                        );
                        let protected = self.protected_path_summary(&baseline, 5);
                        if !protected.is_empty() {
                            summary.push_str(": ");
                            summary.push_str(&protected);
                        }
                        summary
                    });
                    let git_dir = task_state::git_dir(&self.workspace);
                    let linked_worktree = is_linked_worktree(&self.workspace, git_dir.as_deref());
                    if !self.workflow_enabled {
                        state.task = Some(Task::guard_only(
                            task_id,
                            baseline,
                            git_dir,
                            linked_worktree,
                        ));
                        return summary;
                    }
                    // Acquire the per-task lease for the task's lifetime: it
                    // proves this process owns and is live on the task, so
                    // recovery in another process skips it (ADR-0030). A brand-new
                    // random id is always lease-free; a lock-file IO error is a
                    // logged, best-effort degrade (the turn still proceeds).
                    let lease = git_dir.as_ref().and_then(|dir| {
                        match lock::try_exclusive(&lock::lease_path(dir, &task_id)) {
                            Ok(guard) => guard,
                            Err(error) => {
                                tracing::warn!(error = %error, "could not acquire task lease; recovery liveness unprotected this task");
                                None
                            }
                        }
                    });
                    let chain = CheckpointChain::new(self.workspace.clone(), task_id.clone());
                    let mut task =
                        Task::active(task_id, baseline, chain, git_dir, linked_worktree, lease);
                    task.body = body;
                    if let Some(id) = session_id {
                        push_session_deduped(&mut task.sessions, &id);
                    }
                    self.persist_task(&task);
                    state.task = Some(task);
                    summary
                }
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "git baseline capture failed; degrading dirty-tree safety this task");
                    let mut task = Task::degraded(task_id, self.workflow_enabled);
                    task.body = body;
                    if let Some(id) = session_id {
                        push_session_deduped(&mut task.sessions, &id);
                    }
                    state.task = Some(task);
                    Some(format!(
                        "could not read git status ({error:#}); dirty-tree gating disabled this task"
                    ))
                }
            },
        }
    }

    fn unapproved_protected(&self, paths: &[PathBuf]) -> Vec<PathBuf> {
        if paths.is_empty() {
            return Vec::new();
        }
        let state = self.state.lock().unwrap();
        let Some(task) = state.task.as_ref() else {
            return Vec::new();
        };
        if task.degraded || task.all_dirty_approved {
            return Vec::new();
        }
        paths
            .iter()
            .map(|path| self.normalize(path))
            .filter(|path| {
                task.baseline.protected.contains_key(path) && !task.approved.contains(path)
            })
            .collect()
    }

    fn approve(&self, paths: &[PathBuf], all_dirty: bool) {
        let mut state = self.state.lock().unwrap();
        let Some(task) = state.task.as_mut() else {
            return;
        };
        if all_dirty {
            task.all_dirty_approved = true;
            return;
        }
        for path in paths {
            task.approved.insert(self.normalize(path));
        }
    }

    fn before_exec(&self, paths: &[PathBuf]) {
        let mut state = self.state.lock().unwrap();
        let Some(task) = state.task.as_mut() else {
            return;
        };
        // Snapshot the protected set (git mode) plus this call's known targets so
        // the checkpoint chain can capture a clean file's exact pre-task content
        // before Iris overwrites it. In degraded mode there is no protected set,
        // but the targets still feed the content-snapshot fallback.
        let targets = paths.iter().map(|path| self.normalize(path));
        let capture: BTreeSet<PathBuf> = if task.degraded {
            targets.collect()
        } else {
            task.baseline
                .protected
                .keys()
                .cloned()
                .chain(targets)
                .collect()
        };
        task.pre_modes = capture
            .iter()
            .filter(|path| path.exists())
            .map(|path| (path.clone(), FileMode::of(path)))
            .collect();
        task.snapshot = Snapshot::capture(capture);
    }

    fn after_exec(&self, approved: &[PathBuf], expected_after: Option<&str>) -> Vec<PathBuf> {
        let mut violations = Vec::new();
        let mut scan_input = None;
        {
            let mut state = self.state.lock().unwrap();
            if state
                .task
                .as_ref()
                .is_some_and(|task| orphaned_linked_worktree(&self.workspace, task))
            {
                tracing::info!(
                    workspace = %self.workspace.display(),
                    "linked worktree disappeared during cleanup; dropping guard without restoring orphaned files"
                );
                state.task = None;
                return Vec::new();
            }
            let Some(task) = state.task.as_mut() else {
                return Vec::new();
            };
            if task.degraded {
                // Degraded (non-git / jj): no gating or violation detection, but
                // still record a content-snapshot restore point for the known
                // targets so a rollback can undo Iris's own work (reduced
                // guarantees, ADR-0028 Alternative 3).
                self.checkpoint_degraded(task, approved);
                return Vec::new();
            }
            let approved_set: BTreeSet<PathBuf> =
                approved.iter().map(|path| self.normalize(path)).collect();
            // Confirmed Iris changes to feed the checkpoint chain: (path, pre-task
            // bytes+mode). Collected first so the chain's git work runs after the
            // ledger loop without overlapping the snapshot borrow.
            let mut iris_changes: Vec<IrisChange> = Vec::new();
            for path in task.snapshot.changed_paths() {
                let after = snapshot::hash_file(&path);
                // Attribute an approved-target change to Iris only when the
                // post-call bytes are exactly what the tool reported writing.
                // A mismatch means the change is ambiguous -- a concurrent user
                // edit landed on the approved file, or the write failed/was
                // partial -- so ADR-0028's TOCTOU rule keeps it user-attributed
                // and protected rather than silently advancing the baseline.
                let confirmed_target = approved_set.contains(&path)
                    && match (expected_after, after.as_deref()) {
                        (Some(expected), Some(actual)) => expected == actual,
                        _ => false,
                    };
                let approved_bash_change = approved.is_empty()
                    && (task.all_dirty_approved || task.approved.contains(&path));
                if confirmed_target || approved_bash_change {
                    // Expected: a confirmed Iris mutation. Record it and advance
                    // the baseline hash so a later call sees the new content.
                    let before = task.baseline.protected.get(&path).cloned().flatten();
                    // Capture the path's exact pre-task bytes + mode for the
                    // checkpoint chain's base (first-touch only; the chain keeps
                    // the earliest).
                    let pre = match task.snapshot.pre_bytes(&path) {
                        Some(Some(bytes)) => {
                            let mode = task
                                .pre_modes
                                .get(&path)
                                .copied()
                                .unwrap_or(FileMode::Normal);
                            Some((bytes.clone(), mode))
                        }
                        Some(None) => None,
                        None => checkpoint::committed_blob(&self.workspace, &path),
                    };
                    iris_changes.push((path.clone(), pre));
                    task.turn += 1;
                    task.ledger.record(LedgerEntry {
                        path: path.clone(),
                        before,
                        after: after.clone(),
                        attribution: Attribution::Iris,
                        turn: task.turn,
                        tool_call: None,
                        timestamp: SystemTime::now(),
                    });
                    task.baseline.protected.insert(path, after);
                } else {
                    // Out-of-band or unconfirmed change to a protected file:
                    // attributed to the user (TOCTOU rule) and halted by the loop.
                    violations.push(path);
                }
            }
            // A confirmed set of Iris changes opens a new checkpoint over the
            // running (unsettled) diff: snapshot the current ledger-path content
            // into the chain as one restore point (ADR-0028 auto-checkpoint).
            //
            // The `refs/iris/checkpoints/<task-id>/` writes below are NOT wrapped
            // in the repo mutation lock: this task's refs are single-writer by
            // construction -- only the process holding this task's advisory lease
            // writes them, and recovery/expiry/adoption in another process must
            // first claim that lease (ADR-0030), so no other process can read or
            // write these refs concurrently. The shared write that DOES race
            // across processes -- the record file in the shared tasks dir -- is
            // serialized inside `persist_task`.
            if !iris_changes.is_empty() {
                let turn = task.turn;
                let label = checkpoint_label(&iris_changes);
                if let Chain::Git(chain) = &mut task.chain {
                    for (path, pre) in &iris_changes {
                        if let Err(error) = chain.note_before(path, pre.clone()) {
                            tracing::warn!(error = %format!("{error:#}"), "checkpoint pre-image capture failed");
                        }
                    }
                    if let Err(error) = chain.checkpoint(turn, None, label) {
                        tracing::warn!(error = %format!("{error:#}"), "checkpoint create failed");
                    }
                }
                self.persist_task(task);
            }
            // Async attribution only for the non-targeted (bash-like) path: a
            // command with no statically-known target may have changed other
            // files. A targeted edit/write is already accounted for above.
            if matches!(self.mode, Mode::Git) && approved.is_empty() {
                let baseline_paths: BTreeSet<PathBuf> =
                    task.baseline.protected.keys().cloned().collect();
                scan_input = Some((baseline_paths, task.turn));
            }
        }
        // Outside the state lock: barrier the prior scan, then spawn the next.
        if let Some((baseline_paths, turn)) = scan_input {
            self.join_attribution_scan();
            let workspace = self.workspace.clone();
            let handle =
                std::thread::spawn(move || attribution_scan(workspace, baseline_paths, turn));
            *self.scan.lock().unwrap() = Some(handle);
        }
        violations
    }

    fn restore(&self, paths: &[PathBuf]) -> Result<()> {
        let state = self.state.lock().unwrap();
        let Some(task) = state.task.as_ref() else {
            return Ok(());
        };
        task.snapshot.restore(paths)
    }
}

#[cfg(test)]
mod checkpoint_tests;
#[cfg(test)]
mod tests;
