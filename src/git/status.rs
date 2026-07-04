//! One read-only git status snapshot for the session bar and its dropdowns.
//!
//! [`GitStatus`] is captured off the render loop (a background thread via
//! [`GitStatusCache`]) and painted last-known: branch/upstream/ahead-behind and
//! the entry classes from one `git status --porcelain=v2 --branch -z` parse,
//! plus stash count, recent branches, worktrees, and the jj-style task overlay
//! (ADR-0028) read from the durable per-worktree task records.
//!
//! Attribution split: while an unsettled task exists, the dirty count is
//! partitioned -- `iris_unsettled` = ledger paths whose on-disk hash still
//! matches the op-log's chain tip; `user_dirty` = everything else (a path that
//! diverged after Iris's last write counts as USER, the same certainty rule as
//! rollback's TOCTOU reconciliation). With no task everything is
//! undifferentiated `user_dirty`.
//!
//! No network, ever: `⇡`/`⇣` are computed against the last-fetched upstream.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::wayland::git_safety::{self, git};

/// Recent branches fetched for the switch list (`-committerdate` order).
const RECENT_BRANCH_CAP: usize = 16;

/// An unsettled Iris task summarized on a worktree row (`◇ unsettled · 3h`).
#[derive(Debug, Clone)]
pub(crate) struct TaskBadge {
    /// Ledger path count. Captured for the task board; the worktree row
    /// renders the badge as `◇ unsettled · <age>` today.
    #[allow(dead_code)]
    pub(crate) files: u32,
    pub(crate) age: Duration,
}

/// One recent local branch for the switch list.
#[derive(Debug, Clone)]
pub(crate) struct BranchInfo {
    pub(crate) name: String,
    pub(crate) age: Option<Duration>,
    /// Set when the branch is checked out in another worktree (`[WT]` tag).
    pub(crate) worktree: Option<PathBuf>,
}

/// One worktree of the repository (`git worktree list`).
#[derive(Debug, Clone)]
pub(crate) struct WorktreeInfo {
    pub(crate) path: PathBuf,
    pub(crate) branch: Option<String>,
    pub(crate) is_current: bool,
    pub(crate) unsettled: Option<TaskBadge>,
}

/// The current worktree's unsettled task, for the git dropdown's TASK group.
#[derive(Debug, Clone)]
pub(crate) struct TaskSummary {
    pub(crate) task_id: String,
    pub(crate) age: Duration,
}

/// The full snapshot. All counts are files, not hunks.
#[derive(Debug, Clone, Default)]
pub(crate) struct GitStatus {
    /// Current branch; `None` = detached HEAD.
    pub(crate) branch: Option<String>,
    /// `<short-sha> "<subject>"` when detached.
    pub(crate) detached_at: Option<String>,
    pub(crate) upstream: Option<String>,
    pub(crate) ahead: u32,
    pub(crate) behind: u32,
    /// Tracked entries with worktree-side changes.
    pub(crate) modified: u32,
    /// Tracked entries with index-side (staged) changes.
    pub(crate) staged: u32,
    pub(crate) untracked: u32,
    pub(crate) unmerged: u32,
    pub(crate) stash: u32,
    pub(crate) last_commit_age: Option<Duration>,
    pub(crate) recent_branches: Vec<BranchInfo>,
    pub(crate) worktrees: Vec<WorktreeInfo>,
    /// Whether the cwd is a linked (non-main) worktree (`[WT]` tag at rest).
    pub(crate) is_linked_worktree: bool,
    /// Every uncommitted file (changed + renamed + unmerged + untracked).
    pub(crate) total_uncommitted: u32,
    /// Iris-unsettled ledger files (on-disk hash still matches the chain tip).
    pub(crate) iris_unsettled: u32,
    /// User-attributed uncommitted files (total − iris paths; everything when
    /// no task is unsettled).
    pub(crate) user_dirty: u32,
    pub(crate) task: Option<TaskSummary>,
    /// Workspace-relative uncommitted paths attributed to the user.
    pub(crate) user_paths: Vec<String>,
    /// Workspace-relative Iris-unsettled ledger paths.
    pub(crate) iris_paths: Vec<String>,
}

impl GitStatus {
    /// Whether any uncommitted change exists (drives the resting indicator).
    pub(crate) fn is_dirty(&self) -> bool {
        self.total_uncommitted > 0
    }
}

/// Raw parse of `git status --porcelain=v2 --branch -z` output.
#[derive(Debug, Default)]
struct Porcelain {
    head: Option<String>,
    upstream: Option<String>,
    ahead: u32,
    behind: u32,
    modified: u32,
    staged: u32,
    untracked: u32,
    unmerged: u32,
    /// Every uncommitted path, workspace-relative as git reports it.
    paths: Vec<String>,
}

/// Parse porcelain-v2 output over bytes (paths are not guaranteed UTF-8; a
/// non-UTF-8 path is decoded lossily for display-only use here). With `-z`
/// each record ends in NUL and a rename's original path is one extra NUL token.
fn parse_porcelain_v2(bytes: &[u8]) -> Porcelain {
    let mut out = Porcelain::default();
    let mut tokens = bytes.split(|&b| b == 0).filter(|t| !t.is_empty());
    while let Some(token) = tokens.next() {
        let text = String::from_utf8_lossy(token);
        if let Some(header) = text.strip_prefix("# ") {
            if let Some(head) = header.strip_prefix("branch.head ") {
                out.head = Some(head.trim().to_string());
            } else if let Some(upstream) = header.strip_prefix("branch.upstream ") {
                out.upstream = Some(upstream.trim().to_string());
            } else if let Some(ab) = header.strip_prefix("branch.ab ") {
                for part in ab.split_whitespace() {
                    if let Some(n) = part.strip_prefix('+') {
                        out.ahead = n.parse().unwrap_or(0);
                    } else if let Some(n) = part.strip_prefix('-') {
                        out.behind = n.parse().unwrap_or(0);
                    }
                }
            }
            continue;
        }
        let mut fields = text.splitn(2, ' ');
        let kind = fields.next().unwrap_or("");
        let rest = fields.next().unwrap_or("");
        match kind {
            "1" | "2" => {
                // `<XY> <sub> <mH> <mI> <mW> <hH> <hI> [<Xscore>] <path>`
                let mut parts = rest.split(' ');
                let xy = parts.next().unwrap_or("..");
                let mut x = xy.chars();
                let staged = x.next().is_some_and(|c| c != '.');
                let modified = x.next().is_some_and(|c| c != '.');
                if staged {
                    out.staged += 1;
                }
                if modified {
                    out.modified += 1;
                }
                let skip = if kind == "1" { 6 } else { 7 };
                let path = rest.splitn(skip + 2, ' ').nth(skip + 1).unwrap_or("");
                if !path.is_empty() {
                    out.paths.push(path.to_string());
                }
                if kind == "2" {
                    // Consume the rename's original-path token.
                    let _ = tokens.next();
                }
            }
            "u" => {
                out.unmerged += 1;
                // `<XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>`
                let path = rest.splitn(10, ' ').nth(9).unwrap_or("");
                if !path.is_empty() {
                    out.paths.push(path.to_string());
                }
            }
            "?" => {
                out.untracked += 1;
                if !rest.is_empty() {
                    out.paths.push(rest.to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// One `git worktree list --porcelain` block.
fn parse_worktree_list(text: &str) -> Vec<(PathBuf, Option<String>)> {
    let mut out = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut branch: Option<String> = None;
    for line in text.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(p) = path.take() {
                out.push((p, branch.take()));
            }
            branch = None;
            continue;
        }
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        }
    }
    out
}

/// Duration since a unix-seconds timestamp (zero on clock skew).
fn age_since_unix(seconds: u64) -> Duration {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Duration::from_secs(now.saturating_sub(seconds))
}

fn stdout_string(workspace: &Path, args: &[&str]) -> Option<String> {
    git::git_stdout(workspace, args)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Probe a worktree path for unsettled Iris task records → a badge.
fn worktree_badge(path: &Path) -> Option<TaskBadge> {
    let tasks = git_safety::unsettled_tasks(path);
    let files: usize = tasks.iter().map(|t| t.expected.len()).sum();
    let age = tasks.iter().map(|t| t.age).min()?;
    Some(TaskBadge {
        files: files as u32,
        age,
    })
}

/// Capture the full snapshot for `workspace`. `None` when the directory is not
/// a git working tree. Blocking (several git subprocesses): call it from the
/// [`GitStatusCache`] background thread, never the render loop.
pub(crate) fn capture(workspace: &Path) -> Option<GitStatus> {
    if !git::is_git_worktree(workspace) {
        return None;
    }
    let canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let porcelain_bytes = git::git_stdout(
        workspace,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ],
    )
    .ok()?;
    let porcelain = parse_porcelain_v2(&porcelain_bytes);

    let detached = porcelain.head.as_deref() == Some("(detached)");
    let branch = if detached {
        None
    } else {
        porcelain.head.clone()
    };
    let detached_at = detached.then(|| {
        stdout_string(workspace, &["log", "-1", "--format=%h %s"])
            .map(|line| line.trim().to_string())
            .unwrap_or_default()
    });

    let stash = stdout_string(workspace, &["stash", "list", "--format=%gd"])
        .map(|text| text.lines().count() as u32)
        .unwrap_or(0);

    let last_commit_age = stdout_string(workspace, &["log", "-1", "--format=%ct"])
        .and_then(|text| text.trim().parse::<u64>().ok())
        .map(age_since_unix);

    // Worktrees first, so recent branches can carry a checked-out-elsewhere tag.
    let worktrees_raw = stdout_string(workspace, &["worktree", "list", "--porcelain"])
        .map(|text| parse_worktree_list(&text))
        .unwrap_or_default();
    let mut worktrees: Vec<WorktreeInfo> = Vec::new();
    let mut is_linked_worktree = false;
    for (index, (path, wt_branch)) in worktrees_raw.iter().enumerate() {
        let resolved = path.canonicalize().unwrap_or_else(|_| path.clone());
        let is_current = resolved == canonical;
        if is_current && index > 0 {
            is_linked_worktree = true;
        }
        worktrees.push(WorktreeInfo {
            path: path.clone(),
            branch: wt_branch.clone(),
            is_current,
            unsettled: worktree_badge(path),
        });
    }

    let recent = stdout_string(
        workspace,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--count=16",
            "--format=%(refname:short)\t%(committerdate:unix)",
            "refs/heads",
        ],
    )
    .unwrap_or_default();
    let mut recent_branches: Vec<BranchInfo> = Vec::new();
    for line in recent.lines().take(RECENT_BRANCH_CAP) {
        let (name, when) = line.split_once('\t').unwrap_or((line, ""));
        if name.is_empty() {
            continue;
        }
        let worktree = worktrees
            .iter()
            .find(|wt| !wt.is_current && wt.branch.as_deref() == Some(name))
            .map(|wt| wt.path.clone());
        recent_branches.push(BranchInfo {
            name: name.to_string(),
            age: when.trim().parse::<u64>().ok().map(age_since_unix),
            worktree,
        });
    }

    // Task overlay + attribution split (ADR-0028 certainty rule).
    let tasks = git_safety::unsettled_tasks(workspace);
    let relative = |path: &Path| -> String {
        path.strip_prefix(&canonical)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    };
    let mut iris_paths: Vec<String> = Vec::new();
    let mut task = None;
    if let Some(view) = tasks.first() {
        for (path, expected) in &view.expected {
            let current = std::fs::read(path)
                .ok()
                .map(|bytes| crate::tools::content_hash(&bytes));
            if &current == expected {
                iris_paths.push(relative(path));
            }
        }
        iris_paths.sort();
        task = Some(TaskSummary {
            task_id: view.task_id.clone(),
            age: view.age,
        });
    }
    let user_paths: Vec<String> = porcelain
        .paths
        .iter()
        .filter(|path| !iris_paths.iter().any(|iris| iris == *path))
        .cloned()
        .collect();
    let total_uncommitted = (porcelain
        .paths
        .len()
        .max(iris_paths.len() + user_paths.len())) as u32;
    let iris_unsettled = iris_paths.len() as u32;
    let user_dirty = total_uncommitted.saturating_sub(iris_unsettled);

    Some(GitStatus {
        branch,
        detached_at,
        upstream: porcelain.upstream,
        ahead: porcelain.ahead,
        behind: porcelain.behind,
        modified: porcelain.modified,
        staged: porcelain.staged,
        untracked: porcelain.untracked,
        unmerged: porcelain.unmerged,
        stash,
        last_commit_age,
        recent_branches,
        worktrees,
        is_linked_worktree,
        total_uncommitted,
        iris_unsettled,
        user_dirty,
        task,
        user_paths,
        iris_paths,
    })
}

/// Last-known snapshot store with debounced background refresh. The render
/// loop only ever reads [`latest`](Self::latest) (cheap lock) and compares
/// [`generation`](Self::generation) to know when to repaint; a refresh runs on
/// its own thread and never blocks a draw.
#[derive(Clone, Default)]
pub(crate) struct GitStatusCache {
    inner: Arc<CacheInner>,
}

#[derive(Default)]
struct CacheInner {
    latest: Mutex<Option<GitStatus>>,
    generation: AtomicU64,
    /// Refresh coordination, guarded as one unit so the "finish" and "park a
    /// follow-up request" decisions are atomic: no reentrant locking and no
    /// window where a request lands after the worker decides to stop but
    /// before it clears `running`.
    refresh: Mutex<RefreshState>,
}

#[derive(Default)]
struct RefreshState {
    /// A worker thread is currently capturing.
    running: bool,
    /// Workspace of a refresh requested while a worker was in flight; the
    /// worker drains it before it stops, so a request after a checkout or
    /// re-anchor is never lost to a stale in-flight capture.
    pending: Option<PathBuf>,
}

impl GitStatusCache {
    /// The last captured snapshot (`None` before the first refresh completes
    /// or when the workspace is not a git repo).
    pub(crate) fn latest(&self) -> Option<GitStatus> {
        self.inner.latest.lock().unwrap().clone()
    }

    /// Monotonic snapshot generation; changes exactly when a refresh lands.
    pub(crate) fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    /// Kick a background refresh for `workspace`. Coalescing without loss: a
    /// refresh already in flight parks the request as pending (latest wins)
    /// and the worker drains it before finishing, so a request issued after a
    /// checkout or re-anchor is never dropped in favor of a stale capture.
    pub(crate) fn request_refresh(&self, workspace: PathBuf) {
        {
            let mut state = self.inner.refresh.lock().unwrap();
            if state.running {
                // A worker is capturing; park this workspace (latest wins) and
                // let that worker drain it before it stops.
                state.pending = Some(workspace);
                return;
            }
            state.running = true;
        }
        let inner = Arc::clone(&self.inner);
        std::thread::spawn(move || {
            let mut workspace = workspace;
            loop {
                let status = capture(&workspace);
                *inner.latest.lock().unwrap() = status;
                inner.generation.fetch_add(1, Ordering::AcqRel);
                // Under the coordination lock, either drain a parked request or
                // clear `running`. Doing both under one lock closes the race: a
                // request either parked before this (drained here) or arrives
                // after `running` is false (starts a fresh worker).
                let mut state = inner.refresh.lock().unwrap();
                match state.pending.take() {
                    Some(next) => {
                        drop(state);
                        workspace = next;
                    }
                    None => {
                        state.running = false;
                        break;
                    }
                }
            }
        });
    }
}

/// Coarse compact age for metas (`3h`, `2d`, `5m`, `now`).
pub(crate) fn compact_age(age: Duration) -> String {
    let secs = age.as_secs();
    if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs >= 3_600 {
        format!("{}h", secs / 3_600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        "now".to_string()
    }
}

/// Coarse human age for the git dropdown status line (`3h ago`).
pub(crate) fn human_age(age: Duration) -> String {
    format!("{} ago", compact_age(age))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(records: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for record in records {
            out.extend_from_slice(record.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn porcelain_v2_parses_branch_upstream_and_ahead_behind() {
        let bytes = z(&[
            "# branch.oid 46b1045",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -1",
        ]);
        let parsed = parse_porcelain_v2(&bytes);
        assert_eq!(parsed.head.as_deref(), Some("main"));
        assert_eq!(parsed.upstream.as_deref(), Some("origin/main"));
        assert_eq!(parsed.ahead, 2);
        assert_eq!(parsed.behind, 1);
        assert_eq!(parsed.paths.len(), 0);
    }

    #[test]
    fn porcelain_v2_classifies_entries_and_collects_paths() {
        let bytes = z(&[
            "# branch.head main",
            // Staged-only change.
            "1 M. N... 100644 100644 100644 aaaa bbbb src/a.rs",
            // Worktree-only change.
            "1 .M N... 100644 100644 100644 aaaa aaaa src/b.rs",
            // Staged + worktree.
            "1 MM N... 100644 100644 100644 aaaa bbbb src/c.rs",
            // Rename (staged), original path is its own token.
            "2 R. N... 100644 100644 100644 aaaa bbbb R100 new.rs",
            "old.rs",
            // Unmerged.
            "u UU N... 100644 100644 100644 100644 a b c conflicted.rs",
            // Untracked.
            "? notes.txt",
        ]);
        let parsed = parse_porcelain_v2(&bytes);
        assert_eq!(parsed.staged, 3, "{parsed:?}");
        assert_eq!(parsed.modified, 2, "{parsed:?}");
        assert_eq!(parsed.unmerged, 1);
        assert_eq!(parsed.untracked, 1);
        assert_eq!(
            parsed.paths,
            vec![
                "src/a.rs",
                "src/b.rs",
                "src/c.rs",
                "new.rs",
                "conflicted.rs",
                "notes.txt"
            ]
        );
    }

    #[test]
    fn porcelain_v2_detached_head() {
        let bytes = z(&["# branch.head (detached)"]);
        let parsed = parse_porcelain_v2(&bytes);
        assert_eq!(parsed.head.as_deref(), Some("(detached)"));
    }

    #[test]
    fn worktree_list_parses_paths_and_branches() {
        let text = "worktree /home/u/repo\nHEAD aaaa\nbranch refs/heads/main\n\nworktree /home/u/wt/feat\nHEAD bbbb\nbranch refs/heads/feat/x\n\nworktree /home/u/wt/pin\nHEAD cccc\ndetached\n";
        let parsed = parse_worktree_list(text);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].0, PathBuf::from("/home/u/repo"));
        assert_eq!(parsed[0].1.as_deref(), Some("main"));
        assert_eq!(parsed[1].1.as_deref(), Some("feat/x"));
        assert_eq!(parsed[2].1, None);
    }

    #[test]
    fn compact_age_buckets() {
        assert_eq!(compact_age(Duration::from_secs(10)), "now");
        assert_eq!(compact_age(Duration::from_secs(120)), "2m");
        assert_eq!(compact_age(Duration::from_secs(3 * 3600)), "3h");
        assert_eq!(compact_age(Duration::from_secs(2 * 86_400)), "2d");
        assert_eq!(human_age(Duration::from_secs(3 * 3600)), "3h ago");
    }

    /// Self-cleaning scratch dir (same idiom as the git-safety tests; no
    /// tempfile dependency).
    struct TempDir {
        path: PathBuf,
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn temp_dir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("iris-git-status-{nanos}-{seq}"));
        std::fs::create_dir(&path).unwrap();
        TempDir { path }
    }

    /// End-to-end capture against a real temp repo: branch, dirty counts, a
    /// linked worktree, and the task-record attribution split.
    #[test]
    fn capture_snapshots_a_real_repository() {
        let dir = temp_dir();
        let root = dir.path.join("repo");
        std::fs::create_dir(&root).unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        std::fs::write(root.join("new.txt"), "untracked\n").unwrap();

        let status = capture(&root).expect("git repo snapshot");
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.untracked, 1);
        assert_eq!(status.modified, 1);
        assert_eq!(status.total_uncommitted, 2);
        // No task: everything is undifferentiated user dirt.
        assert_eq!(status.iris_unsettled, 0);
        assert_eq!(status.user_dirty, 2);
        assert!(!status.is_linked_worktree);
        assert!(status.last_commit_age.is_some());
        assert!(
            status
                .recent_branches
                .iter()
                .any(|branch| branch.name == "main")
        );
        assert_eq!(status.worktrees.len(), 1);
        assert!(status.worktrees[0].is_current);

        // Non-repo directory yields no snapshot.
        assert!(capture(dir.path.as_path()).is_none());
    }

    /// Concurrency guard: many overlapping `request_refresh` calls from several
    /// threads must never deadlock the coordination lock, and every worker must
    /// terminate (leaving `running` clear). Regression test for a reentrant
    /// re-lock in the drain path that froze the whole UI within seconds.
    #[test]
    fn concurrent_request_refresh_never_deadlocks() {
        let dir = temp_dir();
        let cache = GitStatusCache::default();
        let workspace = dir.path.clone();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let cache = cache.clone();
                let workspace = workspace.clone();
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        cache.request_refresh(workspace.clone());
                    }
                })
            })
            .collect();
        for handle in threads {
            handle.join().unwrap();
        }
        // Let any in-flight worker finish, then the coordination state must be
        // idle (no worker stuck holding the lock) and a fresh request still
        // advances the generation.
        let before = cache.generation();
        for _ in 0..50 {
            if !cache.inner.refresh.lock().unwrap().running {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !cache.inner.refresh.lock().unwrap().running,
            "a worker is stuck running -- coordination deadlock"
        );
        cache.request_refresh(workspace);
        for _ in 0..50 {
            if cache.generation() > before {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(cache.generation() > before, "refresh did not advance");
    }
}
