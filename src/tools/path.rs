//! Workspace path resolution.
//!
//! Safety restrictions are development opt-in via `IRIS_SECURITY_OPT_IN=1`.
//! By default, tools resolve paths but do not confine them to the workspace.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

pub(crate) fn restrictions_enabled() -> bool {
    // Keep legacy safety tests meaningful while the shipped/dev binary defaults
    // to unrestricted unless explicitly opted in.
    cfg!(test) || restrictions_enabled_value(std::env::var("IRIS_SECURITY_OPT_IN").ok().as_deref())
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
    let candidate = lexical_normalize(&join_request(root, requested));
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve path {requested}"))?;
    if restrictions_enabled() && !resolved.starts_with(root) {
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

pub(super) fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::restrictions_enabled_value;

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
