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

use anyhow::Result;

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
    /// `true` when the path's current on-disk bytes diverged from Iris's last
    /// recorded write (the checkpoint-chain tip): a user edit landed after
    /// Iris's last touch. ADR-0028's TOCTOU rule attributes the ambiguous bytes
    /// to the user, so `cur` here is Iris's last recorded state (the tip blob),
    /// NOT the user's newer disk bytes, and the diff carries a divergence
    /// notice. Always `false` for the non-git fallback (no tip to compare).
    pub(crate) diverged: bool,
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
    /// `true` when the current side is Iris's last recorded state rather than the
    /// live disk bytes, because the user edited this path after Iris's last write
    /// (ADR-0028 TOCTOU rule). Surfaced as a per-file notice in the summary so
    /// both the TUI and text renderers show that user bytes were excluded.
    pub(crate) diverged: bool,
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
            let kind_note = match (file.binary, file.kind) {
                (true, _) => " (binary)",
                (false, ChangeKind::Create) => " (new file)",
                (false, ChangeKind::Delete) => " (deleted)",
                (false, ChangeKind::Edit) => "",
            };
            let divergence_note = if file.diverged { DIVERGENCE_NOTICE } else { "" };
            if file.binary {
                lines.push(format!(
                    "  binary  {}{kind_note}{divergence_note}",
                    file.path
                ));
            } else {
                lines.push(format!(
                    "  +{}/-{}  {}{kind_note}{divergence_note}",
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

/// The per-file notice appended in the summary when a ledger path's current disk
/// bytes diverged from Iris's last recorded write: the diff shows Iris's last
/// recorded state, not the user's newer bytes (ADR-0028 TOCTOU rule).
const DIVERGENCE_NOTICE: &str =
    " (modified after Iris's last write; showing Iris's last recorded state)";

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
                diverged: input.diverged,
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
            diverged: input.diverged,
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    TaskNetDiff { files }
}

/// Workspace-relative display path. An ASCII-clean path renders verbatim
/// (forward-slashed); a path whose bytes are non-UTF-8 or contain control /
/// quote / backslash characters is rendered git `core.quotePath`-style so its
/// exact byte identity survives instead of collapsing to U+FFFD replacement
/// characters (issue #264 finding 3).
fn display_path(rel: &Path) -> String {
    quote_path(&display_bytes(rel))
}

/// Raw display bytes of a workspace-relative path. On Unix the exact filesystem
/// bytes (which may not be UTF-8); elsewhere the lossy string with backslashes
/// normalized to forward slashes (Windows separators are not literal bytes).
#[cfg(unix)]
fn display_bytes(rel: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    rel.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn display_bytes(rel: &Path) -> Vec<u8> {
    rel.to_string_lossy().replace('\\', "/").into_bytes()
}

/// git `core.quotePath`-style C quoting of raw path bytes. A path that is valid
/// UTF-8 with no control / quote / backslash bytes renders verbatim (common
/// unicode names stay readable); anything else is wrapped in double quotes with
/// C-style escapes and octal (`\NNN`) escapes for the remaining bytes, so a
/// non-UTF-8 name keeps its exact identity.
fn quote_path(bytes: &[u8]) -> String {
    let special = bytes
        .iter()
        .any(|&b| b < 0x20 || b == 0x7f || b == b'"' || b == b'\\');
    if !special && let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    let mut out = String::with_capacity(bytes.len() + 2);
    out.push('"');
    for &b in bytes {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x09 => out.push_str("\\t"),
            0x0a => out.push_str("\\n"),
            0x0b => out.push_str("\\v"),
            0x0c => out.push_str("\\f"),
            0x0d => out.push_str("\\r"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\{b:03o}")),
        }
    }
    out.push('"');
    out
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
    ///
    /// Fails closed (issue #264 finding 2): a checkpoint/blob read error is
    /// propagated, never swallowed into an empty diff. A silent empty diff would
    /// read as "no Iris changes" and let the accept flow settle a task whose
    /// changes were never actually shown -- so callers must surface the error and
    /// must NOT proceed as if the diff were empty.
    pub(crate) fn task_diff(&self, root: Option<&Path>) -> Result<TaskNetDiff> {
        self.sync_barrier();
        let state = self.state.lock().unwrap();
        let Some(task) = state.task.as_ref() else {
            return Ok(TaskNetDiff::default());
        };
        let root = root.unwrap_or(&self.workspace);
        let inputs = match &task.chain {
            Chain::Git(chain) => chain.net_diff_inputs(root)?,
            Chain::Fallback(store) => store.net_diff_inputs(root, &self.workspace),
        };
        Ok(compute(inputs))
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
            diverged: false,
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
            diverged: false,
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

    #[test]
    fn diverged_path_carries_a_summary_notice() {
        // A diverged path (user edited after Iris's last write): `cur` is Iris's
        // last recorded state and the summary flags it so the exclusion is
        // visible in both renderers.
        let diff = compute(vec![NetPath {
            rel: PathBuf::from("a.txt"),
            pre: Some(b"base\n".to_vec()),
            cur: Some(b"iris last\n".to_vec()),
            diverged: true,
        }]);
        assert!(diff.files[0].diverged);
        let summary = diff.summary_lines();
        assert!(
            summary[1].contains("showing Iris's last recorded state"),
            "summary: {summary:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_path_renders_git_quoted_not_replacement_chars() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        // A filename with a raw 0xFF byte -- not valid UTF-8.
        let rel = PathBuf::from(OsStr::from_bytes(b"bad\xff.txt"));
        let diff = compute(vec![NetPath {
            rel,
            pre: None,
            cur: Some(b"hi\n".to_vec()),
            diverged: false,
        }]);
        let path = &diff.files[0].path;
        assert_eq!(path, "\"bad\\377.txt\"", "got: {path}");
        assert!(
            !path.contains('\u{fffd}'),
            "path identity must not collapse to U+FFFD: {path}"
        );
    }

    #[test]
    fn utf8_path_with_unicode_renders_verbatim() {
        // A valid-UTF-8 unicode name stays readable (not octal-escaped).
        let diff = compute(vec![np("caf\u{e9}.txt", None, Some("x\n"))]);
        assert_eq!(diff.files[0].path, "caf\u{e9}.txt");
    }
}
