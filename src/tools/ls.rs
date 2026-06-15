//! `ls` — list a directory: directories first, then files (case-insensitive),
//! with `/` suffixes for directories. Optionally renders an indented tree up to
//! a requested depth. Includes dotfiles. Symlinked directories are shown but not
//! descended into, so the walk cannot loop or escape the workspace.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::path::resolve_existing;
use super::text::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head};

const DEFAULT_LS_LIMIT: usize = 500;
const LS_SCAN_HARD_LIMIT: usize = 20_000;

pub(super) const DESCRIPTION: &str = "List directory contents: directories first, then files (case-insensitive), with '/' suffix for directories. Includes dotfiles. Set recursive=true (or depth>1) for an indented tree up to `depth` levels. Output is truncated to 500 entries or 1MB (whichever is hit first).";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Directory to list (default: current directory)" },
            "limit": { "type": "integer", "description": "Maximum number of entries to return (default: 500)" },
            "recursive": { "type": "boolean", "description": "List subdirectories as an indented tree (default: false)" },
            "depth": { "type": "integer", "description": "Levels to descend: 1 = immediate children (default), 2 = children and grandchildren, etc. recursive=true implies at least 2." }
        }
    })
}

pub(super) fn execute(root: &Path, args: &Value) -> Result<String> {
    let input: LsInput =
        serde_json::from_value(args.clone()).context("ls tool arguments are invalid")?;
    ls(root, &input)
}

#[derive(Debug, Deserialize)]
struct LsInput {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    recursive: bool,
    #[serde(default)]
    depth: Option<usize>,
}

fn ls(root: &Path, input: &LsInput) -> Result<String> {
    if matches!(input.limit, Some(0)) {
        bail!("`limit` must be greater than 0");
    }
    let dir = input.path.as_deref().unwrap_or(".");
    let dir_path = resolve_existing(root, dir)?;
    if !dir_path.is_dir() {
        bail!("not a directory: {dir}");
    }
    let limit = input.limit.unwrap_or(DEFAULT_LS_LIMIT).max(1);
    let cap = limit.min(LS_SCAN_HARD_LIMIT);

    // Explicit depth wins; bare `recursive` means a 2-level tree; default is flat.
    let max_depth = match (input.recursive, input.depth) {
        (_, Some(d)) => d.max(1),
        (true, None) => 2,
        (false, None) => 1,
    };

    let mut lines: Vec<String> = Vec::new();
    let mut truncated = false;
    append_dir(
        &dir_path,
        dir,
        0,
        max_depth,
        cap,
        &mut lines,
        &mut truncated,
    )?;

    if lines.is_empty() {
        return Ok("(empty directory)".to_string());
    }

    let (body, truncated_bytes, _) =
        truncate_head(&lines.join("\n"), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut out = body;
    if truncated || truncated_bytes {
        out.push_str("\n\n[output truncated]");
    }
    Ok(out)
}

/// Append one directory level (and, within `max_depth`, its subdirectories) to
/// `lines`. `depth` is 0 for the listed directory's immediate children. A failed
/// read of the top directory is an error; failures deeper in the tree are
/// skipped so one unreadable subdirectory does not abort the whole listing.
fn append_dir(
    dir_path: &Path,
    dir_label: &str,
    depth: usize,
    max_depth: usize,
    cap: usize,
    lines: &mut Vec<String>,
    truncated: &mut bool,
) -> Result<()> {
    let read = match fs::read_dir(dir_path) {
        Ok(read) => read,
        Err(error) if depth == 0 => {
            return Err(error).with_context(|| format!("cannot read directory: {dir_label}"));
        }
        Err(_) => return Ok(()),
    };

    // (name, is_dir, is_symlink)
    let mut entries: Vec<(String, bool, bool)> = Vec::new();
    for entry in read {
        let Ok(entry) = entry else { continue };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let is_symlink = file_type.is_symlink();
        let is_dir = file_type.is_dir()
            || (is_symlink && entry.metadata().map(|m| m.is_dir()).unwrap_or(false));
        entries.push((
            entry.file_name().to_string_lossy().to_string(),
            is_dir,
            is_symlink,
        ));
    }

    // Directories first, then files; case-insensitive within each group.
    entries.sort_by_key(|(name, is_dir, _)| (!is_dir, name.to_lowercase()));

    let indent = "  ".repeat(depth);
    for (name, is_dir, is_symlink) in entries {
        if lines.len() >= cap {
            *truncated = true;
            return Ok(());
        }
        let suffix = if is_dir { "/" } else { "" };
        lines.push(format!("{indent}{name}{suffix}"));

        // Descend into real subdirectories only: never follow a symlink, so the
        // walk cannot cycle or leave the resolved root.
        if is_dir && !is_symlink && depth + 1 < max_depth {
            let child = dir_path.join(&name);
            append_dir(&child, &name, depth + 1, max_depth, cap, lines, truncated)?;
            if *truncated {
                return Ok(());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{root_of, temp_dir};

    fn ls_in(root: &Path, recursive: bool, depth: Option<usize>) -> String {
        ls(
            root,
            &LsInput {
                path: None,
                limit: None,
                recursive,
                depth,
            },
        )
        .unwrap()
    }

    #[test]
    fn ls_lists_entries_with_dir_suffix() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir(dir.path.join("sub")).unwrap();
        fs::write(dir.path.join("file.txt"), "x").unwrap();
        let out = ls_in(&root, false, None);
        assert!(out.contains("sub/"));
        assert!(out.contains("file.txt"));
    }

    #[test]
    fn ls_orders_directories_first_then_case_insensitive() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir(dir.path.join("zeta")).unwrap();
        fs::create_dir(dir.path.join("src")).unwrap();
        fs::write(dir.path.join("B.txt"), "x").unwrap();
        fs::write(dir.path.join("a.txt"), "x").unwrap();
        let out = ls_in(&root, false, None);
        assert_eq!(out, "src/\nzeta/\na.txt\nB.txt");
    }

    #[test]
    fn ls_default_does_not_descend() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir_all(dir.path.join("src/tools")).unwrap();
        fs::write(dir.path.join("src/tools/grep.rs"), "x").unwrap();
        let out = ls_in(&root, false, None);
        assert_eq!(out, "src/");
    }

    #[test]
    fn ls_recursive_renders_indented_tree() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir_all(dir.path.join("src/auth")).unwrap();
        fs::create_dir_all(dir.path.join("src/tools")).unwrap();
        fs::write(dir.path.join("src/tools/grep.rs"), "x").unwrap();
        fs::write(dir.path.join("Cargo.toml"), "x").unwrap();
        let out = ls_in(&root, true, Some(3));
        assert_eq!(out, "src/\n  auth/\n  tools/\n    grep.rs\nCargo.toml");
    }

    #[test]
    fn ls_depth_bounds_descent() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir_all(dir.path.join("src/tools")).unwrap();
        fs::write(dir.path.join("src/tools/grep.rs"), "x").unwrap();
        // recursive with default depth (2): shows src/ and its children, not grandchildren.
        let out = ls_in(&root, true, None);
        assert_eq!(out, "src/\n  tools/");
        assert!(!out.contains("grep.rs"));
    }

    #[cfg(unix)]
    #[test]
    fn ls_does_not_descend_symlinked_directories() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir(dir.path.join("realdir")).unwrap();
        fs::write(dir.path.join("realdir/child.txt"), "x").unwrap();
        symlink(dir.path.join("realdir"), dir.path.join("link")).unwrap();
        let out = ls_in(&root, true, Some(3));
        // realdir is descended; the symlink `link` is shown but not followed.
        assert!(out.contains("realdir/\n  child.txt"), "{out}");
        assert!(!out.contains("link/\n  child.txt"), "{out}");
    }

    #[test]
    fn ls_rejects_zero_limit() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let err = ls(
            &root,
            &LsInput {
                path: None,
                limit: Some(0),
                recursive: false,
                depth: None,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("limit"), "{err}");
    }
}
