//! `find` — file search by glob, native via the `ignore` + `globset` crates.
//!
//! Walks the search directory honoring `.gitignore` (and `.ignore`) rules and
//! matches each entry's path against the requested glob. `fd` is itself a CLI
//! over these same crates, so running them in-process drops a subprocess (and
//! the fail-when-`fd`-absent path) and returns structured results. Glob matching
//! mirrors `fd`'s defaults: smart-case (case-insensitive unless the pattern has
//! an uppercase char) and `*` not crossing `/` while `**` does.
//!
//! ponytail: single-threaded `ignore::Walk` that scans the whole subtree before
//! sorting/limiting. Fine for normal workspaces; switch to `WalkParallel` +
//! early-stop if find ever runs hot on very large trees.

use std::fs;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use globset::GlobBuilder;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{Value, json};

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

pub(super) fn execute(root: &Path, args: &Value) -> Result<super::ToolOutput> {
    let input: FindInput =
        serde_json::from_value(args.clone()).context("find tool arguments must include pattern")?;
    Ok(super::ToolOutput::text(find(root, &input)?))
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
    let search = input.path.as_deref().unwrap_or(".");
    let search_path = resolve_existing(root, search)?;
    let limit = input.limit.unwrap_or(DEFAULT_FIND_LIMIT).max(1);

    // A pattern without a path separator matches the file name at any depth
    // (fd / Glob-tool convention); one with a separator matches the relative
    // path under the search directory.
    let normalized = if input.pattern.contains('/') {
        input.pattern.clone()
    } else {
        format!("**/{}", input.pattern)
    };
    // Smart-case like `fd`: case-insensitive unless the pattern has an uppercase
    // character. `literal_separator(true)` keeps `*` from crossing `/` (only
    // `**` does), matching standard glob and the documented examples.
    let smart_case = !input.pattern.chars().any(|c| c.is_uppercase());
    let matcher = GlobBuilder::new(&normalized)
        .case_insensitive(smart_case)
        .literal_separator(true)
        .build()
        .with_context(|| format!("invalid glob pattern: {}", input.pattern))?
        .compile_matcher();

    let mut entries: Vec<(String, Option<SystemTime>)> = Vec::new();
    let walker = WalkBuilder::new(&search_path)
        .hidden(false) // include dotfiles, matching the previous `fd --hidden`
        .require_git(false) // honor .gitignore even outside a git repository
        .build();
    for entry in walker {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path == search_path {
            continue; // the search root itself is not a result
        }
        // Skip VCS metadata, as `fd` does even with `--hidden`.
        if path.components().any(|c| c.as_os_str() == ".git") {
            continue;
        }
        let Ok(rel_path) = path.strip_prefix(&search_path) else {
            continue;
        };
        if !matcher.is_match(rel_path) {
            continue;
        }
        let mut rel = relative_display(&search_path, path);
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && !rel.ends_with('/') {
            rel.push('/');
        }
        let modified = fs::metadata(path).and_then(|m| m.modified()).ok();
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
    entries.truncate(limit);

    if entries.is_empty() {
        return Ok("No files found matching pattern".to_string());
    }

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
    use std::time::{Duration, SystemTime};

    fn input(pattern: &str, limit: Option<usize>) -> FindInput {
        FindInput {
            pattern: pattern.into(),
            path: None,
            limit,
        }
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn find_matches_basename_at_any_depth() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir(dir.path.join("src")).unwrap();
        fs::write(dir.path.join("top.rs"), "x").unwrap();
        fs::write(dir.path.join("src/nested.rs"), "x").unwrap();
        let out = find(&root, &input("*.rs", None)).unwrap();
        assert!(out.contains("top.rs"), "{out}");
        assert!(out.contains("src/nested.rs"), "{out}");
    }

    #[test]
    fn find_path_pattern_matches_relative_path() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir(dir.path.join("src")).unwrap();
        fs::write(dir.path.join("top.rs"), "x").unwrap();
        fs::write(dir.path.join("src/nested.rs"), "x").unwrap();
        let out = find(&root, &input("src/*.rs", None)).unwrap();
        assert!(out.contains("src/nested.rs"), "{out}");
        assert!(!out.contains("top.rs"), "{out}");
    }

    #[test]
    fn find_respects_gitignore() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(dir.path.join("ignored.rs"), "x").unwrap();
        fs::write(dir.path.join("kept.rs"), "x").unwrap();
        let out = find(&root, &input("*.rs", None)).unwrap();
        assert!(out.contains("kept.rs"), "{out}");
        assert!(!out.contains("ignored.rs"), "{out}");
    }

    #[test]
    fn find_sorts_newest_first() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let old = dir.path.join("old.rs");
        let new = dir.path.join("new.rs");
        fs::write(&old, "x").unwrap();
        fs::write(&new, "x").unwrap();
        let base = SystemTime::now();
        set_mtime(&old, base - Duration::from_secs(60));
        set_mtime(&new, base);
        let out = find(&root, &input("*.rs", None)).unwrap();
        let new_pos = out.find("new.rs").unwrap();
        let old_pos = out.find("old.rs").unwrap();
        assert!(new_pos < old_pos, "newest first expected: {out}");
    }

    #[test]
    fn find_limit_keeps_newest() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let base = SystemTime::now();
        for (i, name) in ["a.rs", "b.rs", "c.rs"].iter().enumerate() {
            let p = dir.path.join(name);
            fs::write(&p, "x").unwrap();
            set_mtime(&p, base - Duration::from_secs(i as u64 * 60));
        }
        // a.rs is newest, c.rs oldest; limit 2 keeps the two newest.
        let out = find(&root, &input("*.rs", Some(2))).unwrap();
        assert_eq!(out.lines().count(), 2, "{out}");
        assert!(out.contains("a.rs") && out.contains("b.rs"), "{out}");
        assert!(!out.contains("c.rs"), "{out}");
    }

    #[test]
    fn find_glob_is_smart_case() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("lower.rs"), "x").unwrap();
        fs::write(dir.path.join("UPPER.RS"), "x").unwrap();
        // All-lowercase pattern is case-insensitive (matches fd's smart-case).
        let any = find(&root, &input("*.rs", None)).unwrap();
        assert!(
            any.contains("lower.rs") && any.contains("UPPER.RS"),
            "{any}"
        );
        // A pattern with an uppercase char is case-sensitive.
        let exact = find(&root, &input("*.RS", None)).unwrap();
        assert!(
            exact.contains("UPPER.RS") && !exact.contains("lower.rs"),
            "{exact}"
        );
    }

    #[test]
    fn find_star_does_not_cross_directories() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir_all(dir.path.join("src/deep")).unwrap();
        fs::write(dir.path.join("src/top.rs"), "x").unwrap();
        fs::write(dir.path.join("src/deep/low.rs"), "x").unwrap();
        // `*` must not cross `/`: src/*.rs matches src/top.rs but not src/deep/low.rs.
        let out = find(&root, &input("src/*.rs", None)).unwrap();
        assert!(out.contains("src/top.rs"), "{out}");
        assert!(!out.contains("src/deep/low.rs"), "{out}");
    }

    #[test]
    fn find_no_match_returns_message() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("a.rs"), "x").unwrap();
        let out = find(&root, &input("*.zzz", None)).unwrap();
        assert_eq!(out, "No files found matching pattern");
    }

    #[test]
    fn find_rejects_zero_limit() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let err = find(&root, &input("*.rs", Some(0)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("limit"), "{err}");
    }
}
