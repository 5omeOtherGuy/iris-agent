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
