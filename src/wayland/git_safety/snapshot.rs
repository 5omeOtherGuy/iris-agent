//! Protected-set content snapshot + content hashing (issue #262, ADR-0028).
//!
//! Before a mutating tool call the guard snapshots the exact bytes of every
//! protected (pre-existing dirty/untracked) file. After the call it compares the
//! on-disk bytes against this snapshot to detect an out-of-band write, and can
//! restore the pre-call bytes verbatim. This is the recovery guarantee for the
//! Tier-2 (foreground bash) surface: "recoverable, not untouchable".
//!
//! This is deliberately a plain byte snapshot, not a git object: it is the
//! degraded fallback the ADR keeps for non-git directories and the cheap
//! per-command recovery buffer for the bash path. The git-object-backed
//! checkpoint chain (#263) layers on top; this is its storage seam.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Hex SHA-256 of `bytes`. Delegates to [`crate::tools::content_hash`] so the
/// guard's on-disk re-hash and a mutating tool's reported post-write hash use
/// one convention and stay directly comparable (ADR-0028 write confirmation).
pub(super) fn hash_bytes(bytes: &[u8]) -> String {
    crate::tools::content_hash(bytes)
}

/// Hex SHA-256 of a file's contents, or `None` when the path cannot be read
/// (absent, e.g. a staged deletion, or unreadable). `None` is a first-class
/// "absent" marker in the baseline and ledger, not an error.
pub(super) fn hash_file(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|bytes| hash_bytes(&bytes))
}

/// Pre-call byte snapshot of the protected set. `Some(bytes)` = file present at
/// snapshot time; `None` = absent (so restore removes a file the command
/// created in a protected path).
#[derive(Default)]
pub(super) struct Snapshot {
    files: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

impl Snapshot {
    /// Snapshot the current bytes of every path in `paths`.
    pub(super) fn capture(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let files = paths
            .into_iter()
            .map(|path| {
                let content = fs::read(&path).ok();
                (path, content)
            })
            .collect();
        Self { files }
    }

    /// The pre-call bytes captured for `path`: `Some(Some(bytes))` = present at
    /// snapshot time, `Some(None)` = captured-but-absent, `None` = not captured.
    /// The checkpoint chain (#263) reads this to blob a ledger path's exact
    /// pre-task content without re-reading a now-mutated file.
    pub(super) fn pre_bytes(&self, path: &Path) -> Option<&Option<Vec<u8>>> {
        self.files.get(path)
    }

    /// Paths whose on-disk bytes differ from the snapshot (created, deleted, or
    /// modified since capture).
    pub(super) fn changed_paths(&self) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|(path, snapped)| fs::read(path).ok() != **snapped)
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// Restore the given paths to their snapshot bytes exactly. A path snapshot
    /// as present is rewritten; a path snapshot as absent is removed. Best-effort
    /// per path; the first hard write failure is returned.
    pub(super) fn restore(&self, paths: &[PathBuf]) -> Result<()> {
        for path in paths {
            match self.files.get(path) {
                Some(Some(bytes)) => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent).with_context(|| {
                            format!("failed to recreate parent for {}", path.display())
                        })?;
                    }
                    fs::write(path, bytes)
                        .with_context(|| format!("failed to restore {}", path.display()))?;
                }
                Some(None) => {
                    // Snapshot recorded the path as absent: undo a creation.
                    let _ = fs::remove_file(path);
                }
                None => {}
            }
        }
        Ok(())
    }
}

/// Non-git checkpoint fallback (issue #263, ADR-0028 Alternative 3): plain
/// content snapshots of the paths Iris touches, so a rollback can restore them
/// even in a directory git cannot back. It mirrors the git chain's before/point
/// model (`seq 0` = pre-task base, `seq >= 1` = each op) but with reduced
/// guarantees -- in-process restore points only, no ref-anchored durability
/// across a crash, no git rename/mode object semantics -- which the guard
/// announces as degraded. Deliberately minimal: the honest fallback, not a
/// second full engine.
/// A path -> pre/at-op bytes map (`None` = absent). The shared shape of the
/// fallback base and each op restore point.
type ContentMap = BTreeMap<PathBuf, Option<Vec<u8>>>;

#[derive(Default)]
pub(super) struct FallbackStore {
    /// Pre-task bytes per touched path (first-touch wins). `None` = the path did
    /// not exist pre-task, so a base rollback removes it.
    before: ContentMap,
    /// Ordered op restore points (seq 1..), each a full snapshot of every
    /// touched path at that op, with its label.
    points: Vec<(String, ContentMap)>,
}

impl FallbackStore {
    /// Capture a path's pre-task bytes the first time it is touched (idempotent).
    pub(super) fn note_before(&mut self, path: &Path, bytes: Option<Vec<u8>>) {
        self.before.entry(path.to_path_buf()).or_insert(bytes);
    }

    /// Append a restore point snapshotting the current on-disk bytes of every
    /// touched path.
    pub(super) fn checkpoint(&mut self, label: String) {
        let snapshot = self
            .before
            .keys()
            .map(|path| (path.clone(), fs::read(path).ok()))
            .collect();
        self.points.push((label, snapshot));
    }

    /// Number of op restore points recorded (excludes the base).
    pub(super) fn len(&self) -> usize {
        self.points.len()
    }

    /// Restore-point labels for the UI: base first, then each op oldest-first.
    pub(super) fn labels(&self) -> Vec<String> {
        let mut out = vec!["pre-task baseline".to_string()];
        out.extend(self.points.iter().map(|(label, _)| label.clone()));
        out
    }

    /// Restore every touched path to its state at `seq` (0 = pre-task base).
    pub(super) fn rollback_to(&self, seq: u64) -> Result<()> {
        let files: &ContentMap = if seq == 0 {
            &self.before
        } else {
            match self.points.get((seq - 1) as usize) {
                Some((_, files)) => files,
                None => return Ok(()),
            }
        };
        for (path, bytes) in files {
            match bytes {
                Some(bytes) => {
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent).with_context(|| {
                            format!("failed to recreate parent for {}", path.display())
                        })?;
                    }
                    fs::write(path, bytes)
                        .with_context(|| format!("failed to restore {}", path.display()))?;
                }
                None => {
                    let _ = fs::remove_file(path);
                }
            }
        }
        Ok(())
    }

    /// Keep only the newest `keep` op restore points (base always kept),
    /// mirroring the git chain's settlement GC.
    pub(super) fn gc(&mut self, keep: usize) {
        let drop = self.points.len().saturating_sub(keep);
        self.points.drain(0..drop);
    }
}
