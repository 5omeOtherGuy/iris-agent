//! `ls` — list a directory, alphabetized, with `/` suffixes for directories.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::path::resolve_existing;
use super::text::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head};

const DEFAULT_LS_LIMIT: usize = 500;
const LS_SCAN_HARD_LIMIT: usize = 20_000;

pub(super) const DESCRIPTION: &str = "List directory contents. Returns entries sorted alphabetically, with '/' suffix for directories. Includes dotfiles. Output is truncated to 500 entries or 1MB (whichever is hit first).";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Directory to list (default: current directory)" },
            "limit": { "type": "integer", "description": "Maximum number of entries to return (default: 500)" }
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

    let mut entries: Vec<String> = Vec::new();
    for entry in fs::read_dir(&dir_path).with_context(|| format!("cannot read directory: {dir}"))? {
        if entries.len() >= LS_SCAN_HARD_LIMIT {
            break;
        }
        let entry = entry.context("cannot read directory entry")?;
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry
            .file_type()
            .map(|ft| {
                ft.is_dir()
                    || (ft.is_symlink() && entry.metadata().map(|m| m.is_dir()).unwrap_or(false))
            })
            .unwrap_or(false);
        entries.push(if is_dir { format!("{name}/") } else { name });
    }

    if entries.is_empty() {
        return Ok("(empty directory)".to_string());
    }

    entries.sort_by_key(|name| name.to_lowercase());
    let mut truncated_entries = false;
    if entries.len() > limit {
        entries.truncate(limit);
        truncated_entries = true;
    }

    let (body, truncated_bytes, _) =
        truncate_head(&entries.join("\n"), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut out = body;
    if truncated_entries || truncated_bytes {
        out.push_str("\n\n[output truncated]");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{root_of, temp_dir};

    #[test]
    fn ls_lists_entries_with_dir_suffix() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir(dir.path.join("sub")).unwrap();
        fs::write(dir.path.join("file.txt"), "x").unwrap();
        let out = ls(
            &root,
            &LsInput {
                path: None,
                limit: None,
            },
        )
        .unwrap();
        assert!(out.contains("sub/"));
        assert!(out.contains("file.txt"));
    }
}
