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

/// One `git status --porcelain=v1 -z` record.
struct StatusEntry {
    path: String,
    /// Rename/copy source path, also protected.
    source: Option<String>,
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
        let abs = normalize(Path::new(&entry.path));
        let hash = hash_file(&abs);
        protected.insert(abs, hash);
        if let Some(source) = entry.source {
            let abs = normalize(Path::new(&source));
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

/// Parse NUL-delimited porcelain-v1 status. Each record is `XY <path>`; a
/// rename/copy (`R`/`C` in either column) is followed by an extra NUL token
/// holding the source path. `!!` (ignored) records are skipped.
fn parse_status_z(bytes: &[u8]) -> Vec<StatusEntry> {
    let text = String::from_utf8_lossy(bytes);
    let mut tokens = text.split('\0').filter(|token| !token.is_empty());
    let mut entries = Vec::new();
    while let Some(token) = tokens.next() {
        // "XY " prefix (2 status chars + space) then the path.
        if token.len() < 4 {
            continue;
        }
        let status = &token[..2];
        if status == "!!" {
            continue;
        }
        let path = token[3..].to_string();
        let untracked = status == "??";
        let is_rename = status.contains('R') || status.contains('C');
        let source = if is_rename {
            tokens.next().map(str::to_string)
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
