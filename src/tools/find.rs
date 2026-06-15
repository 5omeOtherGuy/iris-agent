//! `find` — file search by glob that shells out to fd (`fd`/`fdfind`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::find_binary;
use super::path::{relative_display, resolve_existing};
use super::text::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head};

const DEFAULT_FIND_LIMIT: usize = 1000;

pub(super) const DESCRIPTION: &str = "Search for files by glob pattern. Returns matching file paths relative to the search directory. Sorted by modification time (newest first). Respects .gitignore. Output is truncated to 1000 results or 1MB (whichever is hit first).";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Glob pattern to match files, e.g. '*.ts', '**/*.json', or 'src/**/*.spec.ts'" },
            "path": { "type": "string", "description": "Directory to search in (default: current directory)" },
            "limit": { "type": "integer", "description": "Maximum number of results (default: 1000)" }
        },
        "required": ["pattern"]
    })
}

pub(super) fn execute(root: &Path, args: &Value) -> Result<String> {
    let input: FindInput =
        serde_json::from_value(args.clone()).context("find tool arguments must include pattern")?;
    find(root, &input)
}

#[derive(Debug, Deserialize)]
struct FindInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

fn find(root: &Path, input: &FindInput) -> Result<String> {
    if matches!(input.limit, Some(0)) {
        bail!("`limit` must be greater than 0");
    }
    let fd = find_binary(&["fd", "fdfind"]).context(
        "the `find` tool requires fd (`fd` or `fdfind`), which was not found on PATH. \
         Install it: Debian/Ubuntu `apt install fd-find` (binary is `fdfind`), macOS \
         `brew install fd`, Arch `pacman -S fd`, or see https://github.com/sharkdp/fd#installation",
    )?;

    let search = input.path.as_deref().unwrap_or(".");
    let search_path = resolve_existing(root, search)?;
    let limit = input.limit.unwrap_or(DEFAULT_FIND_LIMIT).max(1);

    let args: Vec<String> = vec![
        "--glob".to_string(),
        "--color=never".to_string(),
        "--hidden".to_string(),
        "--max-results".to_string(),
        limit.to_string(),
        "--".to_string(),
        input.pattern.clone(),
        search_path.to_string_lossy().to_string(),
    ];

    let output = Command::new(fd)
        .args(&args)
        .current_dir(root)
        .output()
        .context("failed to run fd")?;

    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);
        if stderr.trim().is_empty() {
            bail!("fd exited with code {code}");
        }
        bail!("fd failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok("No files found matching pattern".to_string());
    }

    // Collect entries with modification times so we can sort newest-first.
    let mut entries: Vec<(String, Option<SystemTime>)> = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let absolute = if Path::new(line).is_absolute() {
            PathBuf::from(line)
        } else {
            search_path.join(line)
        };
        let mut rel = relative_display(&search_path, &absolute);
        if absolute.is_dir() && !rel.ends_with('/') {
            rel.push('/');
        }
        let modified = fs::metadata(&absolute).and_then(|m| m.modified()).ok();
        entries.push((rel, modified));
    }

    entries.sort_by(|a, b| match (&a.1, &b.1) {
        (Some(at), Some(bt)) => bt
            .cmp(at)
            .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
    });

    let listing: Vec<String> = entries.into_iter().map(|(rel, _)| rel).collect();
    let (body, truncated, _) =
        truncate_head(&listing.join("\n"), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut out = body;
    if truncated {
        out.push_str("\n\n[output truncated]");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{root_of, temp_dir};

    #[test]
    fn find_locates_files_when_fd_available() {
        if find_binary(&["fd", "fdfind"]).is_none() {
            return;
        }
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("target.rs"), "x").unwrap();
        let out = find(
            &root,
            &FindInput {
                pattern: "*.rs".into(),
                path: None,
                limit: None,
            },
        )
        .unwrap();
        assert!(out.contains("target.rs"));
    }
}
