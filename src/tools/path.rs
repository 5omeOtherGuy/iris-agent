//! Workspace path resolution.
//!
//! Parent-session safety restrictions are development opt-in via
//! `IRIS_SECURITY_OPT_IN=1`. Delegated agents install a thread-local per-agent
//! override so their mutation paths stay confined independent of process settings.

use std::cell::Cell;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

thread_local! {
    static RESTRICTIONS_OVERRIDE: Cell<Option<bool>> = const { Cell::new(None) };
}

pub(crate) fn restrictions_enabled() -> bool {
    RESTRICTIONS_OVERRIDE.get().unwrap_or_else(|| {
        cfg!(test)
            || restrictions_enabled_value(std::env::var("IRIS_SECURITY_OPT_IN").ok().as_deref())
    })
}

pub(crate) fn workspace_confinement_required() -> bool {
    RESTRICTIONS_OVERRIDE.get() == Some(true)
}

/// Runs one synchronous tool body with an optional workspace-confinement policy.
/// The override is thread-local and nestable, so independent scheduler/blocking
/// threads cannot change each other's policy. `None` preserves the parent
/// process's legacy environment/test behavior.
pub(crate) fn with_restrictions<T>(restrictions: Option<bool>, operation: impl FnOnce() -> T) -> T {
    let Some(restrictions) = restrictions else {
        return operation();
    };
    let previous = RESTRICTIONS_OVERRIDE.replace(Some(restrictions));
    struct Reset(Option<bool>);
    impl Drop for Reset {
        fn drop(&mut self) {
            RESTRICTIONS_OVERRIDE.set(self.0);
        }
    }
    let _reset = Reset(previous);
    operation()
}

fn restrictions_enabled_value(value: Option<&str>) -> bool {
    matches!(value, Some("1" | "true" | "yes" | "on"))
}

pub(crate) fn workspace_root(workspace: &Path) -> Result<PathBuf> {
    workspace
        .canonicalize()
        .with_context(|| format!("failed to resolve workspace {}", workspace.display()))
}

fn join_request(root: &Path, requested: &str) -> PathBuf {
    let candidate = Path::new(requested);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    }
}

/// Lexically normalize `.` and `..` without touching the filesystem.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve a path that must already exist, confined to the workspace.
pub(crate) fn resolve_existing(root: &Path, requested: &str) -> Result<PathBuf> {
    resolve_existing_in_roots(root, requested, &[])
}

/// Resolve an existing path for a read-only consumer. When confinement is on,
/// the target must be in the workspace or one of the trusted extra roots.
/// Callers must derive extras from host-discovered resources, never model input.
pub(crate) fn resolve_existing_in_roots(
    root: &Path,
    requested: &str,
    extra_roots: &[PathBuf],
) -> Result<PathBuf> {
    let candidate = lexical_normalize(&join_request(root, requested));
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve path {requested}"))?;
    let in_extra_root = extra_roots.iter().any(|extra| resolved.starts_with(extra));
    if restrictions_enabled() && !resolved.starts_with(root) && !in_extra_root {
        bail!("path escapes workspace: {requested}");
    }
    Ok(resolved)
}

/// Resolve a path for create/overwrite, confined to the workspace. The path
/// need not exist, but no existing ancestor may symlink outside the workspace.
pub(super) fn resolve_for_write(root: &Path, requested: &str) -> Result<PathBuf> {
    let candidate = lexical_normalize(&join_request(root, requested));
    if restrictions_enabled() && !candidate.starts_with(root) {
        bail!("path escapes workspace: {requested}");
    }
    let mut ancestor = candidate.as_path();
    loop {
        if ancestor.exists() {
            let canonical = ancestor
                .canonicalize()
                .with_context(|| format!("failed to resolve path {requested}"))?;
            if restrictions_enabled() && !canonical.starts_with(root) {
                bail!("path escapes workspace: {requested}");
            }
            break;
        }
        match ancestor.parent() {
            Some(parent) => ancestor = parent,
            None => break,
        }
    }
    Ok(candidate)
}

/// Whether `requested` resolves strictly inside `root`, ALWAYS enforced --
/// independent of the `IRIS_SECURITY_OPT_IN` execution-time confinement.
///
/// The auto-approval preset (ADR-0032) uses this to keep an outside-workspace
/// target on the prompt path even where runtime path confinement is opt-out:
/// auto is a fresh silent-execution decision, so it fails closed regardless of
/// the confinement toggle. An unresolvable workspace root, a lexical escape, or
/// an existing ancestor that canonicalizes outside `root` (a symlink out) all
/// report `false`.
pub(crate) fn is_inside_workspace(root: &Path, requested: &str) -> bool {
    if requested.is_empty() {
        return false;
    }
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let candidate = lexical_normalize(&join_request(&root, requested));
    if !candidate.starts_with(&root) {
        return false;
    }
    // Reject a symlinked existing ancestor that escapes the workspace. The
    // deepest existing ancestor is the one the write would actually resolve
    // through; canonicalizing it collapses any symlink hop.
    let mut ancestor = candidate.as_path();
    loop {
        if ancestor.exists() {
            return matches!(ancestor.canonicalize(), Ok(canonical) if canonical.starts_with(&root));
        }
        match ancestor.parent() {
            Some(parent) => ancestor = parent,
            None => return false,
        }
    }
}

pub(super) fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

/// The workspace-relative form of `requested` when it resolves strictly inside
/// `root`, else `None`. Combines the always-enforced containment check
/// ([`is_inside_workspace`]) with relative rendering so callers that persist or
/// re-inject a path (the compaction carry, ADR-0044) never leak an absolute
/// path or a `..` escape: an absolute path outside the workspace, a traversal
/// escape, or a symlinked ancestor pointing out all yield `None`. Enforced
/// independent of the `IRIS_SECURITY_OPT_IN` execution-time toggle, because the
/// carry is durable context, not a one-shot execution decision.
pub(crate) fn workspace_relative(root: &Path, requested: &str) -> Option<String> {
    // Strict carry floor (ADR-0044): reject at the derivation boundary rather
    // than cosmetically normalizing. An absolute path, or any path with a `..`
    // (`ParentDir`) component, is dropped BEFORE normalization -- so a path that
    // points inside the workspace via an absolute prefix, or one that traverses
    // out and back in, is never carried even though it would normalize to an
    // in-workspace relative form.
    let path = Path::new(requested);
    if path.is_absolute() {
        return None;
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return None;
    }
    if !is_inside_workspace(root, requested) {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let candidate = lexical_normalize(&join_request(&root, requested));
    let rel = candidate.strip_prefix(&root).ok()?;
    Some(rel.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{root_of, temp_dir};
    use super::{
        restrictions_enabled, restrictions_enabled_value, with_restrictions, workspace_relative,
    };

    #[test]
    fn workspace_relative_keeps_inside_paths_and_drops_escapes() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let root = root.as_path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), b"x").unwrap();

        // Inside the workspace: rendered workspace-relative, no leading root.
        assert_eq!(
            workspace_relative(root, "src/a.rs").as_deref(),
            Some("src/a.rs")
        );
        // A `.`-noisy but inside path normalizes to the same relative form.
        assert_eq!(
            workspace_relative(root, "./src/./a.rs").as_deref(),
            Some("src/a.rs")
        );
        // Absolute path outside the workspace: rejected (no leak).
        assert_eq!(workspace_relative(root, "/etc/passwd"), None);
        // Traversal escape above the workspace root: rejected.
        assert_eq!(workspace_relative(root, "../../etc/passwd"), None);
        // Empty request: rejected.
        assert_eq!(workspace_relative(root, ""), None);
    }

    #[test]
    fn workspace_relative_rejects_absolute_inside_and_traversal_back_inside() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let root = root.as_path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), b"x").unwrap();

        // An absolute path that points INSIDE the workspace is rejected at the
        // carry boundary (ADR-0044): the persisted carry must be a plain
        // workspace-relative path, never an absolute one, even when it resolves
        // inside.
        let abs_inside = root.join("src/a.rs");
        assert_eq!(workspace_relative(root, abs_inside.to_str().unwrap()), None);
        // A `..`-containing path that normalizes back inside the workspace is
        // rejected before normalization, not cosmetically collapsed.
        assert_eq!(workspace_relative(root, "src/../src/a.rs"), None);
        assert_eq!(workspace_relative(root, "./src/../src/a.rs"), None);
    }

    #[test]
    fn per_agent_restrictions_are_nested_and_thread_local() {
        assert!(
            restrictions_enabled(),
            "test default should remain confined"
        );
        with_restrictions(Some(false), || {
            assert!(!restrictions_enabled());
            with_restrictions(Some(true), || assert!(restrictions_enabled()));
            assert!(!restrictions_enabled());
            assert!(std::thread::spawn(restrictions_enabled).join().unwrap());
        });
        assert!(restrictions_enabled());
    }

    #[test]
    fn security_restrictions_require_explicit_opt_in_value() {
        for value in [None, Some(""), Some("0"), Some("false"), Some("off")] {
            assert!(
                !restrictions_enabled_value(value),
                "{value:?} should be off"
            );
        }
        for value in [Some("1"), Some("true"), Some("yes"), Some("on")] {
            assert!(restrictions_enabled_value(value), "{value:?} should be on");
        }
    }
}
