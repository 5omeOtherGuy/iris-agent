//! Workspace path resolution — the tool sandbox enforcement point.
//!
//! Every tool resolves a requested path against the canonicalized workspace
//! root and refuses to escape it (including via `..` and symlinks).

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

pub(super) fn workspace_root(workspace: &Path) -> Result<PathBuf> {
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
fn lexical_normalize(path: &Path) -> PathBuf {
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
pub(super) fn resolve_existing(root: &Path, requested: &str) -> Result<PathBuf> {
    let candidate = lexical_normalize(&join_request(root, requested));
    let resolved = candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve path {requested}"))?;
    if !resolved.starts_with(root) {
        bail!("path escapes workspace: {requested}");
    }
    Ok(resolved)
}

/// Resolve a path for create/overwrite, confined to the workspace. The path
/// need not exist, but no existing ancestor may symlink outside the workspace.
pub(super) fn resolve_for_write(root: &Path, requested: &str) -> Result<PathBuf> {
    let candidate = lexical_normalize(&join_request(root, requested));
    if !candidate.starts_with(root) {
        bail!("path escapes workspace: {requested}");
    }
    let mut ancestor = candidate.as_path();
    loop {
        if ancestor.exists() {
            let canonical = ancestor
                .canonicalize()
                .with_context(|| format!("failed to resolve path {requested}"))?;
            if !canonical.starts_with(root) {
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
