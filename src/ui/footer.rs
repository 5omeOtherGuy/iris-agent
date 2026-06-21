//! Status-line footer data (Tier 3): the pi-style status block shown at the
//! bottom of the full-screen TUI.
//!
//! Modeled on pi-mono's interactive footer
//! (`packages/coding-agent/src/modes/interactive/components/footer.ts`): a
//! working-directory + git-branch line, then a context-usage line with the
//! active model. This module is pure data plus filesystem/env probing (HOME
//! substitution, `.git/HEAD` parsing); the ratatui rendering lives in
//! [`crate::ui::tui`] so this stays unit-testable without a terminal.
//!
//! ponytail: the context figure is Iris's per-message token *estimate*
//! (`session::estimate_tokens`), the same number auto-compaction budgets
//! against -- not provider-reported usage. Cumulative input/output/cache token
//! counts and cost (pi's `up18k down1.4k R17k $0.141`) are deliberately omitted:
//! Iris does not yet capture real provider usage (Milestone 2 token/context
//! work), and fabricating those numbers would be worse than leaving them out.

use std::path::{Path, PathBuf};

/// A snapshot of the data shown in the bottom status line. Built once per state
/// change (session start, turn boundary, model switch) by the TUI loop and
/// handed to the screen; rendering reads these fields and formats them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Footer {
    /// Working directory, already HOME-substituted (e.g. `~/projects/iris`).
    pub(crate) cwd: String,
    /// Current git branch (or short detached-HEAD sha), when in a repository.
    pub(crate) branch: Option<String>,
    /// Estimated provider-visible context size, in tokens.
    pub(crate) context_tokens: u64,
    /// The active model's context-window cap, when known from the catalog.
    pub(crate) context_window: Option<u64>,
    /// Whether auto-compaction is armed (a context budget is configured).
    pub(crate) auto_compact: bool,
    /// Active `provider/model` id, when a model selection is available.
    pub(crate) model: Option<String>,
}

impl Footer {
    /// Build a footer snapshot, probing the workspace for its display path and
    /// git branch. `model` is the active `provider/model` id (when known) and
    /// `context_window` its catalog cap.
    pub(crate) fn build(
        workspace: &Path,
        context_tokens: u64,
        context_window: Option<u64>,
        auto_compact: bool,
        model: Option<String>,
    ) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        Self {
            cwd: display_cwd(workspace, home.as_deref()),
            branch: git_branch(workspace),
            context_tokens,
            context_window,
            auto_compact,
            model,
        }
    }
}

/// Render `path` for the footer, abbreviating the home prefix to `~` (matching
/// pi's `formatCwdForFooter`). A path outside `home`, or an absent `home`, is
/// shown verbatim.
fn display_cwd(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if path == home {
            return "~".to_string();
        }
        if let Ok(rel) = path.strip_prefix(home) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

/// Resolve the current git branch for `start`, walking up to the repository
/// root. Returns the branch name (`refs/heads/<name>` -> `<name>`), a short sha
/// for a detached HEAD, or `None` outside a repository. No subprocess: reads
/// `.git/HEAD` directly, including the worktree `gitdir:` indirection.
fn git_branch(start: &Path) -> Option<String> {
    let git_dir = find_git_dir(start)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    parse_head(&head)
}

/// Parse a `.git/HEAD` body into a branch name or short detached-HEAD sha.
fn parse_head(head: &str) -> Option<String> {
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        // `refs/heads/main` -> `main`; keep the trailing segment so a slashed
        // branch name (`feat/x`) still renders its full path-relative tail.
        reference
            .strip_prefix("refs/heads/")
            .or(Some(reference))
            .map(str::to_string)
    } else if head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(head[..7].to_string())
    } else {
        None
    }
}

/// Walk up from `start` to find the `.git` directory, following a worktree
/// `.git` file's `gitdir:` pointer. Returns the directory that holds `HEAD`.
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        let candidate = current.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            // Linked worktree: the `.git` file points at the real gitdir, which
            // carries this worktree's own HEAD.
            let content = std::fs::read_to_string(&candidate).ok()?;
            let path = content.trim().strip_prefix("gitdir: ")?;
            return Some(PathBuf::from(path));
        }
        dir = current.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_cwd_abbreviates_home_prefix() {
        let home = Path::new("/home/dev");
        assert_eq!(display_cwd(Path::new("/home/dev"), Some(home)), "~");
        assert_eq!(
            display_cwd(Path::new("/home/dev/projects/iris"), Some(home)),
            "~/projects/iris"
        );
    }

    #[test]
    fn display_cwd_keeps_paths_outside_home_verbatim() {
        let home = Path::new("/home/dev");
        assert_eq!(display_cwd(Path::new("/etc/iris"), Some(home)), "/etc/iris");
        // A sibling that merely shares a prefix string is not inside home.
        assert_eq!(
            display_cwd(Path::new("/home/developer/x"), Some(home)),
            "/home/developer/x"
        );
        // No home at all -> verbatim.
        assert_eq!(display_cwd(Path::new("/home/dev/x"), None), "/home/dev/x");
    }

    #[test]
    fn parse_head_reads_branch_name() {
        assert_eq!(
            parse_head("ref: refs/heads/main\n").as_deref(),
            Some("main")
        );
        assert_eq!(
            parse_head("ref: refs/heads/feat/status-line\n").as_deref(),
            Some("feat/status-line")
        );
    }

    #[test]
    fn parse_head_reads_detached_short_sha() {
        assert_eq!(
            parse_head("0123456789abcdef0123456789abcdef01234567\n").as_deref(),
            Some("0123456")
        );
    }

    #[test]
    fn parse_head_rejects_garbage() {
        assert_eq!(parse_head(""), None);
        assert_eq!(parse_head("not-a-ref"), None);
    }

    #[test]
    fn git_branch_reads_head_from_a_real_git_dir() {
        let dir = crate::tools::test_support::temp_dir();
        let git = dir.path.join(".git");
        std::fs::create_dir_all(&git).expect("create .git");
        std::fs::write(git.join("HEAD"), "ref: refs/heads/topic\n").expect("write HEAD");
        // A nested working subdirectory still resolves the repo branch.
        let nested = dir.path.join("src/ui");
        std::fs::create_dir_all(&nested).expect("create nested");
        assert_eq!(git_branch(&nested).as_deref(), Some("topic"));
    }

    #[test]
    fn git_branch_is_none_outside_a_repository() {
        let dir = crate::tools::test_support::temp_dir();
        assert_eq!(git_branch(&dir.path), None);
    }

    #[test]
    fn git_branch_follows_a_worktree_gitdir_pointer() {
        let dir = crate::tools::test_support::temp_dir();
        // The real per-worktree gitdir holds HEAD.
        let gitdir = dir.path.join("realgit/worktrees/wt");
        std::fs::create_dir_all(&gitdir).expect("create gitdir");
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/wt-branch\n").expect("write HEAD");
        // The worktree checkout has a `.git` *file* pointing at it.
        let worktree = dir.path.join("checkout");
        std::fs::create_dir_all(&worktree).expect("create worktree");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .expect("write .git file");
        assert_eq!(git_branch(&worktree).as_deref(), Some("wt-branch"));
    }
}
