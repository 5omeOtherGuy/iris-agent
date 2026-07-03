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
mod git;
mod ledger;
mod snapshot;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::SystemTime;

use anyhow::Result;

use crate::nexus::MutationGuard;

use baseline::Baseline;
use ledger::{Attribution, Ledger, LedgerEntry};
use snapshot::Snapshot;

/// How the guard operates for this workspace.
enum Mode {
    /// A git working tree: full baseline + gating.
    Git,
    /// Non-git, `.jj/`, or a capture failure: no gating, honest notice. The
    /// string is the one-line reason surfaced once at the first mutation.
    Degraded(String),
}

/// Per-task state, created lazily at the first mutating call.
struct Task {
    /// `true` for a degraded task: no baseline, no gating.
    degraded: bool,
    baseline: Baseline,
    ledger: Ledger,
    /// Per-file approvals granted this task (normalized absolute paths).
    approved: BTreeSet<PathBuf>,
    /// The "all dirty files this task" escalation.
    all_dirty_approved: bool,
    /// Pre-call byte snapshot of the protected set (refreshed each call).
    snapshot: Snapshot,
    /// Task-local op sequence, advanced per recorded mutation.
    turn: u64,
}

impl Task {
    fn active(baseline: Baseline) -> Self {
        Self {
            degraded: false,
            baseline,
            ledger: Ledger::default(),
            approved: BTreeSet::new(),
            all_dirty_approved: false,
            snapshot: Snapshot::default(),
            turn: 0,
        }
    }

    fn degraded() -> Self {
        Self {
            degraded: true,
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
            turn: 0,
        }
    }
}

struct State {
    task: Option<Task>,
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
            state: Mutex::new(State { task: None }),
            scan: Mutex::new(None),
        }
    }

    /// Settle the current task (ADR-0028 settlement boundary): join any pending
    /// scan, freeze and drop the ledger/approvals, so the next mutation opens a
    /// fresh baseline. This is the seam #263 replaces with commit/rollback
    /// settlement; the checkpoint chain hangs off the same call.
    pub(crate) fn settle(&self) {
        self.sync_barrier();
        self.state.lock().unwrap().task = None;
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
                for entry in entries {
                    task.ledger.record(entry);
                }
            }
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
            // Baseline already captured and announced for this task.
            return None;
        }
        match &self.mode {
            Mode::Degraded(reason) => {
                state.task = Some(Task::degraded());
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
                    state.task = Some(Task::active(baseline));
                    summary
                }
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "git baseline capture failed; degrading dirty-tree safety this task");
                    state.task = Some(Task::degraded());
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

    fn before_exec(&self) {
        let mut state = self.state.lock().unwrap();
        let Some(task) = state.task.as_mut() else {
            return;
        };
        if task.degraded {
            return;
        }
        let protected: Vec<PathBuf> = task.baseline.protected.keys().cloned().collect();
        task.snapshot = Snapshot::capture(protected);
    }

    fn after_exec(&self, approved: &[PathBuf]) -> Vec<PathBuf> {
        let mut violations = Vec::new();
        let mut scan_input = None;
        {
            let mut state = self.state.lock().unwrap();
            let Some(task) = state.task.as_mut() else {
                return Vec::new();
            };
            if task.degraded {
                return Vec::new();
            }
            let approved_set: BTreeSet<PathBuf> =
                approved.iter().map(|path| self.normalize(path)).collect();
            for path in task.snapshot.changed_paths() {
                if approved_set.contains(&path) {
                    // Expected: an approved Iris mutation. Record it and advance
                    // the baseline hash so a later call sees the new content.
                    let before = task.baseline.protected.get(&path).cloned().flatten();
                    let after = snapshot::hash_file(&path);
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
                    // Out-of-band change to a protected file: attributed to the
                    // user (TOCTOU rule) and halted by the loop.
                    violations.push(path);
                }
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
mod tests;
