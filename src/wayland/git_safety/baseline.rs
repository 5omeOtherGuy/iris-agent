//! Task baseline capture (issue #262, ADR-0028).
//!
//! At the first mutating tool call of a task the guard captures a baseline of
//! the workspace's uncommitted state: every dirty (tracked, modified/staged)
//! and untracked file with its content hash, plus the index (`git ls-files
//! --stage`). The protected set = the keys of `protected`; the choke-point gate
//! and the bash detection layer both read it. Renames protect both endpoints.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::git::git_stdout;
use super::snapshot::hash_file;

/// The uncommitted state of the workspace at task start.
pub(super) struct Baseline {
    /// Protected files keyed by normalized absolute path -> content hash
    /// (`None` when absent on disk, e.g. a staged deletion).
    pub(super) protected: BTreeMap<PathBuf, Option<String>>,
    /// Count of dirty (tracked) entries, for the surfaced summary.
    pub(super) dirty_count: usize,
    /// Count of untracked entries, for the surfaced summary.
    pub(super) untracked_count: usize,
    /// `git ls-files --stage` output: the staged/index state to protect and
    /// (in #263) restore. Captured verbatim; this slice records it (read back
    /// via the test accessor and #263's index-restore), so it is a seam field.
    #[allow(dead_code)]
    pub(super) index: String,
}

/// One `git status --porcelain=v1 -z` record. Paths are kept as [`PathBuf`]
/// (built from raw bytes, never lossy-decoded) so a non-UTF-8 filename is
/// hashed, snapshotted, and restored under its exact on-disk name.
struct StatusEntry {
    path: PathBuf,
    /// Rename/copy source path, also protected.
    source: Option<PathBuf>,
    untracked: bool,
}

/// Capture the baseline for `workspace` (already canonicalized). `normalize`
/// maps a workspace-relative git path to the same normalized absolute form the
/// guard uses for the mutating-tool paths, so membership checks are exact.
pub(super) fn capture(workspace: &Path, normalize: impl Fn(&Path) -> PathBuf) -> Result<Baseline> {
    let status = git_stdout(
        workspace,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let index =
        String::from_utf8_lossy(&git_stdout(workspace, &["ls-files", "--stage"])?).into_owned();

    let mut protected = BTreeMap::new();
    let mut dirty_count = 0;
    let mut untracked_count = 0;
    for entry in parse_status_z(&status) {
        if entry.untracked {
            untracked_count += 1;
        } else {
            dirty_count += 1;
        }
        let abs = normalize(&entry.path);
        let hash = hash_file(&abs);
        protected.insert(abs, hash);
        if let Some(source) = entry.source {
            let abs = normalize(&source);
            let hash = hash_file(&abs);
            protected.insert(abs, hash);
        }
    }

    Ok(Baseline {
        protected,
        dirty_count,
        untracked_count,
        index,
    })
}

/// Build a [`PathBuf`] from raw git path bytes without lossy decoding. On Unix
/// the bytes are the exact on-disk filename (`git status -z` never quotes), so a
/// non-UTF-8 name round-trips and stays protected/restorable.
#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    PathBuf::from(OsStr::from_bytes(bytes))
}

/// Documented degrade for non-Unix targets (this repo targets Unix): a non-UTF-8
/// path cannot be represented losslessly, so it is decoded lossily with a
/// warning rather than silently corrupted. Iris does not currently ship on such
/// targets; this keeps the code honest if that changes.
#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    match std::str::from_utf8(bytes) {
        Ok(text) => PathBuf::from(text),
        Err(_) => {
            tracing::warn!(
                "non-UTF-8 git path on a non-Unix target; dirty-tree protection degraded for this path"
            );
            PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
        }
    }
}

/// Parse NUL-delimited porcelain-v1 status directly over the byte stream (paths
/// are not guaranteed UTF-8). Each record is `XY <path>`; a rename/copy (`R`/`C`
/// in either column) is followed by an extra NUL token holding the source path.
/// `!!` (ignored) records are skipped.
fn parse_status_z(bytes: &[u8]) -> Vec<StatusEntry> {
    let mut tokens = bytes.split(|&b| b == 0).filter(|token| !token.is_empty());
    let mut entries = Vec::new();
    while let Some(token) = tokens.next() {
        // "XY " prefix (2 ASCII status chars + a space) then the path bytes.
        if token.len() < 4 {
            continue;
        }
        let (x, y) = (token[0], token[1]);
        if x == b'!' && y == b'!' {
            continue;
        }
        let path = path_from_bytes(&token[3..]);
        let untracked = x == b'?' && y == b'?';
        let is_rename = x == b'R' || y == b'R' || x == b'C' || y == b'C';
        let source = if is_rename {
            tokens.next().map(path_from_bytes)
        } else {
            None
        };
        entries.push(StatusEntry {
            path,
            source,
            untracked,
        });
    }
    entries
}
