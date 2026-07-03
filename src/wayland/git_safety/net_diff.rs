//! Final task diff engine (issue #264, ADR-0028).
//!
//! Presents a task's *net* diff as the deliverable: for every ledger path, the
//! change from its pre-task state (baseline side) to its current on-disk state
//! (current side), collapsed to one hunk set per file regardless of how many
//! intermediate steps touched it. Scoped to LEDGER PATHS ONLY -- a file the user
//! (not Iris) modified never appears, because it is never in the checkpoint
//! chain's pre-task snapshot.
//!
//! Two halves:
//!
//! - [`compute`] is the pure engine over [`NetPath`] inputs (pre bytes, current
//!   bytes per ledger path). It classifies create/edit/delete, detects binary
//!   content (no text diff for a binary change), and drops a path whose net
//!   change is nil (edited then reverted). It is git-free and root-free so it
//!   unit-tests on hand-built inputs.
//! - [`GitSafety::task_diff`] is the guard entry point: it joins the async
//!   attribution scan at the sync barrier (a ledger-consuming point, ADR-0028),
//!   reads each ledger path's pre-task bytes from the checkpoint chain (or the
//!   non-git snapshot fallback) and its current bytes from an explicit
//!   source-tree root, and runs [`compute`]. The root is a parameter, not the
//!   hardcoded workspace cwd, so a later worktree-apply review (#267/#271) can
//!   diff the same ledger against a different tree without changing the engine.

use std::path::{Path, PathBuf};

use super::{Chain, GitSafety};

/// One ledger path's pre-task and current content for the net diff. `pre` is the
/// exact pre-task bytes (`None` = the path did not exist pre-task, a create);
/// `cur` is the current bytes at the source-tree root (`None` = the path is
/// absent now, a delete). Byte vectors so binary content and non-UTF-8 paths
/// survive; the engine decides text-vs-binary from the bytes.
pub(crate) struct NetPath {
    /// Workspace-relative path (the display name and the join key under a root).
    pub(crate) rel: PathBuf,
    pub(crate) pre: Option<Vec<u8>>,
    pub(crate) cur: Option<Vec<u8>>,
}

/// How a ledger path changed across the task, net of intermediate steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    /// Absent pre-task, present now.
    Create,
    /// Present pre-task and now, content differs.
    Edit,
    /// Present pre-task, absent now.
    Delete,
}

/// The net change to one ledger path.
pub(crate) struct FileDiff {
    /// Workspace-relative display path (forward-slashed).
    pub(crate) path: String,
    pub(crate) kind: ChangeKind,
    /// `true` when either side is binary: reported as a binary change with no
    /// text diff (`added`/`removed` are 0, `unified` is a one-line marker).
    pub(crate) binary: bool,
    pub(crate) added: usize,
    pub(crate) removed: usize,
    /// Unified diff text for this file (headers + hunks), or the binary marker.
    pub(crate) unified: String,
}

/// A task's net diff: one [`FileDiff`] per changed ledger path, sorted by path.
#[derive(Default)]
pub(crate) struct TaskNetDiff {
    pub(crate) files: Vec<FileDiff>,
}

impl TaskNetDiff {
    /// No net Iris changes in the task (or no unsettled task).
    pub(crate) fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Total added / removed lines across all files (binary files contribute 0).
    pub(crate) fn totals(&self) -> (usize, usize) {
        self.files
            .iter()
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed))
    }

    /// Per-file summary lines plus a header, for the Tier-3 view. The header
    /// names the file count and net line totals; each file line shows its
    /// +added/-removed and a kind note (new file / deleted / binary). Never
    /// emojis. Empty when there are no changes -- the caller renders the honest
    /// "no Iris changes" message instead.
    pub(crate) fn summary_lines(&self) -> Vec<String> {
        if self.files.is_empty() {
            return Vec::new();
        }
        let (added, removed) = self.totals();
        let noun = if self.files.len() == 1 {
            "file"
        } else {
            "files"
        };
        let mut lines = vec![format!(
            "{} {noun} changed, +{added}/-{removed}",
            self.files.len()
        )];
        for file in &self.files {
            let note = match (file.binary, file.kind) {
                (true, _) => " (binary)",
                (false, ChangeKind::Create) => " (new file)",
                (false, ChangeKind::Delete) => " (deleted)",
                (false, ChangeKind::Edit) => "",
            };
            if file.binary {
                lines.push(format!("  binary  {}{note}", file.path));
            } else {
                lines.push(format!(
                    "  +{}/-{}  {}{note}",
                    file.added, file.removed, file.path
                ));
            }
        }
        lines
    }

    /// The full combined unified diff across all files, for the colorizer (TUI)
    /// or plain output (text/non-TTY). Binary files contribute a one-line marker
    /// so the plain output stays honest without emitting garbage bytes.
    pub(crate) fn unified(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            out.push_str(&file.unified);
            if !file.unified.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }
}

/// Whether the bytes look binary. Git's heuristic: a NUL byte in the content.
/// Cheap and matches how the diff colorizer would choke on binary anyway.
fn is_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

/// The pure net-diff engine: fold each ledger path's (pre, cur) bytes into a
/// [`FileDiff`], skipping paths with no net change. Root-free and git-free.
pub(crate) fn compute(inputs: Vec<NetPath>) -> TaskNetDiff {
    let mut files = Vec::new();
    for input in inputs {
        // No net change (e.g. edited twice back to the original): honestly omit.
        if input.pre == input.cur {
            continue;
        }
        let kind = match (input.pre.is_some(), input.cur.is_some()) {
            (false, true) => ChangeKind::Create,
            (true, false) => ChangeKind::Delete,
            _ => ChangeKind::Edit,
        };
        let path = display_path(&input.rel);
        let pre = input.pre.unwrap_or_default();
        let cur = input.cur.unwrap_or_default();
        if is_binary(&pre) || is_binary(&cur) {
            files.push(FileDiff {
                unified: format!("Binary file {path} changed\n"),
                path,
                kind,
                binary: true,
                added: 0,
                removed: 0,
            });
            continue;
        }
        let old = String::from_utf8_lossy(&pre);
        let new = String::from_utf8_lossy(&cur);
        let (added, removed) = count_changes(&old, &new);
        let unified = unified_diff(&path, kind, &old, &new);
        files.push(FileDiff {
            path,
            kind,
            binary: false,
            added,
            removed,
            unified,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    TaskNetDiff { files }
}

/// Workspace-relative display path, forward-slashed regardless of platform.
fn display_path(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// Count net added / removed lines between `old` and `new` via `similar`, the
/// project's existing diff library (reused, not handrolled).
fn count_changes(old: &str, new: &str) -> (usize, usize) {
    use similar::ChangeTag;
    let mut added = 0;
    let mut removed = 0;
    for change in similar::TextDiff::from_lines(old, new).iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

/// A unified diff for one file with git-style headers so the Tier-3 colorizer
/// recognizes and drops the `--- /+++ ` pair (the frame names the file). A
/// create reads from `/dev/null`; a delete writes to `/dev/null`.
fn unified_diff(path: &str, kind: ChangeKind, old: &str, new: &str) -> String {
    let old_header = match kind {
        ChangeKind::Create => "/dev/null".to_string(),
        _ => format!("a/{path}"),
    };
    let new_header = match kind {
        ChangeKind::Delete => "/dev/null".to_string(),
        _ => format!("b/{path}"),
    };
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .header(&old_header, &new_header)
        .to_string()
}

impl GitSafety {
    /// The current task's net diff (issue #264): the change from each ledger
    /// path's pre-task state to its current bytes, one hunk set per file. Scoped
    /// to ledger paths only -- user modifications never appear.
    ///
    /// `root` is the source tree the current side is read from; `None` uses the
    /// workspace (the normal on-demand `/diff` and accept-flow case). A future
    /// worktree-apply review (#267/#271) passes an alternate root to diff the
    /// same ledger against a different tree -- the seam this parameter keeps
    /// open. Joins the async attribution scan first (a ledger-consuming sync
    /// barrier, ADR-0028), so a bash-attributed change is included. Returns an
    /// empty diff when no task is unsettled.
    pub(crate) fn task_diff(&self, root: Option<&Path>) -> TaskNetDiff {
        self.sync_barrier();
        let state = self.state.lock().unwrap();
        let Some(task) = state.task.as_ref() else {
            return TaskNetDiff::default();
        };
        let root = root.unwrap_or(&self.workspace);
        let inputs = match &task.chain {
            Chain::Git(chain) => chain.net_diff_inputs(root).unwrap_or_default(),
            Chain::Fallback(store) => store.net_diff_inputs(root, &self.workspace),
        };
        compute(inputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn np(rel: &str, pre: Option<&str>, cur: Option<&str>) -> NetPath {
        NetPath {
            rel: PathBuf::from(rel),
            pre: pre.map(|s| s.as_bytes().to_vec()),
            cur: cur.map(|s| s.as_bytes().to_vec()),
        }
    }

    #[test]
    fn no_net_change_is_omitted() {
        // Edited twice back to the original: pre == cur, so it is not a change.
        let diff = compute(vec![np("a.txt", Some("same\n"), Some("same\n"))]);
        assert!(diff.is_empty());
        assert!(diff.summary_lines().is_empty());
    }

    #[test]
    fn edit_reports_one_net_hunk_set() {
        let diff = compute(vec![np("a.txt", Some("one\ntwo\n"), Some("one\nTWO\n"))]);
        assert_eq!(diff.files.len(), 1);
        let file = &diff.files[0];
        assert_eq!(file.kind, ChangeKind::Edit);
        assert_eq!((file.added, file.removed), (1, 1));
        assert!(file.unified.contains("--- a/a.txt"));
        assert!(file.unified.contains("+++ b/a.txt"));
        assert!(file.unified.contains("+one\nTWO") || file.unified.contains("+TWO"));
        assert!(file.unified.contains("-two"));
    }

    #[test]
    fn create_and_delete_use_dev_null_headers() {
        let created = compute(vec![np("new.txt", None, Some("hello\n"))]);
        assert_eq!(created.files[0].kind, ChangeKind::Create);
        assert!(created.files[0].unified.contains("--- /dev/null"));
        assert_eq!((created.files[0].added, created.files[0].removed), (1, 0));

        let deleted = compute(vec![np("gone.txt", Some("bye\n"), None)]);
        assert_eq!(deleted.files[0].kind, ChangeKind::Delete);
        assert!(deleted.files[0].unified.contains("+++ /dev/null"));
        assert_eq!((deleted.files[0].added, deleted.files[0].removed), (0, 1));
    }

    #[test]
    fn binary_change_has_no_text_diff() {
        let mut pre = b"text".to_vec();
        let mut cur = b"text".to_vec();
        pre.push(0); // a NUL makes it binary
        cur.extend_from_slice(&[0, 1]); // differs from pre and is also binary
        let diff = compute(vec![NetPath {
            rel: PathBuf::from("blob.bin"),
            pre: Some(pre),
            cur: Some(cur),
        }]);
        let file = &diff.files[0];
        assert!(file.binary);
        assert_eq!((file.added, file.removed), (0, 0));
        assert!(file.unified.contains("Binary file blob.bin changed"));
        assert!(diff.summary_lines().iter().any(|l| l.contains("binary")));
    }

    #[test]
    fn summary_reports_file_count_and_totals() {
        let diff = compute(vec![
            np("b.txt", Some("x\n"), Some("x\ny\n")),
            np("a.txt", None, Some("new\n")),
        ]);
        // Sorted by path: a.txt before b.txt.
        assert_eq!(diff.files[0].path, "a.txt");
        let summary = diff.summary_lines();
        assert_eq!(summary[0], "2 files changed, +2/-0");
        assert!(summary[1].contains("a.txt (new file)"));
    }
}
