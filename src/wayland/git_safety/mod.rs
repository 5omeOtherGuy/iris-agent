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

/// Checkpoints kept per task after a settlement GC (ADR-0028 "keep last N").
/// Small by design: intermediate restore points are a safety buffer, not
/// history, so a handful covers the realistic "undo the last few steps" need.
const KEEP_CHECKPOINTS: usize = 3;

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
pub(crate) struct Settlement {
    pub(crate) summary: String,
    pub(crate) task_id: String,
}

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
    /// `true` for a degraded task: no baseline, no gating.
    degraded: bool,
    /// Stable id anchoring the `refs/iris/checkpoints/<task-id>/` namespace and
    /// the persisted recovery record.
    task_id: String,
    baseline: Baseline,
    ledger: Ledger,
    /// Per-file approvals granted this task (normalized absolute paths).
    approved: BTreeSet<PathBuf>,
    /// The "all dirty files this task" escalation.
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
    /// Resolved git directory for the persisted recovery record (`None` in
    /// degraded mode).
    git_dir: Option<PathBuf>,
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
}

impl Task {
    fn active(
        task_id: String,
        baseline: Baseline,
        chain: CheckpointChain,
        git_dir: Option<PathBuf>,
        lease: Option<lock::FlockGuard>,
    ) -> Self {
        Self {
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
            created_ms: task_state::now_ms(),
            _lease: lease,
            body: None,
            sessions: Vec::new(),
        }
    }

    fn degraded(task_id: String) -> Self {
        Self {
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
            created_ms: task_state::now_ms(),
            _lease: None,
            body: None,
            sessions: Vec::new(),
        }
    }
}

struct State {
    task: Option<Task>,
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

/// The Tier-2 dirty-tree safety guard. Owned by the harness, injected into each
/// turn's `ToolEnv` as a `&dyn MutationGuard`.
pub(crate) struct GitSafety {
    /// Canonicalized workspace root; the anchor for path normalization.
    workspace: PathBuf,
    mode: Mode,
    state: Mutex<State>,
    /// In-flight async attribution scan, joined at every sync barrier.
    scan: Mutex<Option<JoinHandle<Vec<LedgerEntry>>>>,
}

impl GitSafety {
    /// Build the guard for `workspace`, detecting git vs degraded mode once.
    pub(crate) fn new(workspace: &Path) -> Self {
        let canonical = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let mode = detect_mode(&canonical);
        Self {
            workspace: canonical,
            mode,
            state: Mutex::new(State {
                task: None,
                pending_body: None,
                session_id: None,
            }),
            scan: Mutex::new(None),
        }
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
            .map(|task| task.task_id.clone())
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

    /// Join the in-flight attribution scan (if any) and fold its entries into
    /// the ledger. A hard sync barrier per ADR-0028: called at settlement and at
    /// the start of the next mutating call. Must not be called while holding the
    /// `state` lock.
    fn sync_barrier(&self) {
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

    /// Persist the current task's minimal recovery record (git tasks only).
    /// Best-effort: a persistence failure is logged, never fatal to the turn.
    fn persist_task(&self, task: &Task) {
        if task.degraded {
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
        // Sync barrier at the start of the next mutating call (ADR-0028).
        self.sync_barrier();
        let mut state = self.state.lock().unwrap();
        if state.task.is_some() {
            // Baseline already captured and announced for this task. A follow-up
            // turn joining an unsettled task must NOT rewrite its body (captured
            // once at open, ADR-0031), so drop this turn's pending preview.
            state.pending_body = None;
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
                let mut task = Task::degraded(task_id);
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
                        format!(
                            "{} dirty and {} untracked file(s) present before this change",
                            baseline.dirty_count, baseline.untracked_count
                        )
                    });
                    let git_dir = task_state::git_dir(&self.workspace);
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
                    let mut task = Task::active(task_id, baseline, chain, git_dir, lease);
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
                    let mut task = Task::degraded(task_id);
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
                let confirmed = approved_set.contains(&path)
                    && match (expected_after, after.as_deref()) {
                        (Some(expected), Some(actual)) => expected == actual,
                        _ => false,
                    };
                if confirmed {
                    // Expected: a confirmed Iris mutation. Record it and advance
                    // the baseline hash so a later call sees the new content.
                    let before = task.baseline.protected.get(&path).cloned().flatten();
                    // Capture the path's exact pre-task bytes + mode for the
                    // checkpoint chain's base (first-touch only; the chain keeps
                    // the earliest).
                    let pre = task
                        .snapshot
                        .pre_bytes(&path)
                        .and_then(|opt| opt.as_ref())
                        .map(|bytes| {
                            let mode = task
                                .pre_modes
                                .get(&path)
                                .copied()
                                .unwrap_or(FileMode::Normal);
                            (bytes.clone(), mode)
                        });
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
            self.sync_barrier();
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
