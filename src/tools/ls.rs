//! `ls` — list a directory: directories first, then files (case-insensitive),
//! with `/` suffixes for directories. Optionally renders an indented tree up to
//! a requested depth. Includes dotfiles. Symlinked directories are shown but not
//! descended into, so the walk cannot loop. Workspace confinement is opt-in via
//! `IRIS_SECURITY_OPT_IN=1`.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use globset::GlobMatcher;
use serde::Deserialize;
use serde_json::{Value, json};

use super::path::resolve_existing;
use super::text::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};

const DEFAULT_LS_LIMIT: usize = 500;

/// Number of top extensions named in a truncation summary.
const SUMMARY_TOP_EXT: usize = 5;

/// Upper bound on entries collected during the walk, so a model-supplied deep
/// `depth`/`recursive` listing cannot traverse an arbitrarily large tree just
/// to render a capped result. Well above `DEFAULT_LS_LIMIT`, so ordinary
/// directories still get an exact total; past it, the summary reports a lower
/// bound (`>=N ... scan capped`).
const SCAN_BUDGET: usize = 10_000;

pub(super) const DESCRIPTION: &str = "List directory contents: directories first, then files (case-insensitive), with '/' suffix for directories. Includes dotfiles. Set ignore to a glob or array of globs to filter relative paths. Set recursive=true (or depth>1) for an indented tree up to `depth` levels. Set long=true to prefix each entry with a type marker (d/f/l) and human-readable size. Output is truncated to 500 entries or 50KB (whichever is hit first); a truncated listing ends with a summary line carrying the exact total, the dirs/files split, and the dominant omitted extensions so what was cut is visible without a blind re-list.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Directory to list (default: current directory)" },
            "limit": { "type": "integer", "description": "Maximum number of entries to return (default: 500)" },
            "recursive": { "type": "boolean", "description": "List subdirectories as an indented tree (default: false)" },
            "depth": { "type": "integer", "description": "Levels to descend: 1 = immediate children (default), 2 = children and grandchildren, etc. recursive=true implies at least 2." },
            "long": { "type": "boolean", "description": "Prefix each entry with a type marker (d/f/l) and human-readable size (default false)" },
            "ignore": {
                "description": "Glob pattern or array of patterns for relative paths to exclude",
                "anyOf": [
                    { "type": "string" },
                    { "type": "array", "items": { "type": "string" } }
                ]
            }
        }
    })
}

// `_reduce` (issue #210 benchmark arm): `ls` ships no default-on output
// reduction (issue #339 is not started), so the switch is accepted for a
// uniform `run_off_thread` body signature and ignored.
pub(super) fn execute(root: &Path, args: &Value, _reduce: bool) -> Result<super::ToolOutput> {
    let input: LsInput = Deserialize::deserialize(args).context("ls tool arguments are invalid")?;
    // Record the listed directory for the compaction carry (ADR-0044); the
    // workspace root itself renders empty and is not carried.
    let target = input.path.clone().unwrap_or_else(|| ".".to_string());
    Ok(ls(root, &input)?.with_workspace_target(root, &target))
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
    #[serde(default)]
    long: bool,
    #[serde(default)]
    ignore: Option<IgnoreInput>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IgnoreInput {
    One(String),
    Many(Vec<String>),
}

impl IgnoreInput {
    fn patterns(&self) -> &[String] {
        match self {
            Self::One(pattern) => std::slice::from_ref(pattern),
            Self::Many(patterns) => patterns,
        }
    }
}

fn ls(root: &Path, input: &LsInput) -> Result<super::ToolOutput> {
    if matches!(input.limit, Some(0)) {
        bail!("`limit` must be greater than 0");
    }
    let dir = input.path.as_deref().unwrap_or(".");
    let dir_path = resolve_existing(root, dir)?;
    if !dir_path.is_dir() {
        bail!("not a directory: {dir}");
    }
    let limit = input.limit.unwrap_or(DEFAULT_LS_LIMIT).max(1);
    let ignore = input
        .ignore
        .iter()
        .flat_map(|input| input.patterns().iter())
        .map(|pattern| super::find::compile_glob(pattern))
        .collect::<Result<Vec<_>>>()?;

    // Explicit depth wins; bare `recursive` means a 2-level tree; default is flat.
    let max_depth = match (input.recursive, input.depth) {
        (_, Some(d)) => d.max(1),
        (true, None) => 2,
        (false, None) => 1,
    };

    // Collect the entry set first, then render. Collection walks only within
    // `max_depth`, never follows symlinks (so it cannot loop or leave the
    // resolved root), and stops at `SCAN_BUDGET` entries so a deep recursive
    // request cannot traverse an unbounded tree. When the budget is hit the
    // total becomes a lower bound and the summary says so.
    let mut entries: Vec<Entry> = Vec::new();
    let walk = WalkOptions {
        max_depth,
        budget: SCAN_BUDGET,
        ignore: &ignore,
    };
    collect_entries(&dir_path, Path::new(""), dir, 0, &walk, &mut entries)?;

    if entries.is_empty() {
        return Ok(super::ToolOutput::text("(empty directory)").with("entries", json!(0)));
    }

    let total = entries.len();
    let scan_capped = total >= SCAN_BUDGET;
    let rendered = render_listing(
        &entries,
        limit,
        input.long,
        DEFAULT_MAX_LINES,
        DEFAULT_MAX_BYTES,
        scan_capped,
    );
    Ok(super::ToolOutput::text(rendered.body)
        .with("entries", json!(total))
        .with("truncated", json!(rendered.truncated)))
}

/// One collected directory entry, carrying enough to render a line and to
/// summarize a truncated listing.
struct Entry {
    depth: usize,
    name: String,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
}

/// A rendered listing: the body (with the truncation summary already appended
/// when the listing overflowed) and whether it was truncated.
struct Rendered {
    body: String,
    truncated: bool,
}

struct WalkOptions<'a> {
    max_depth: usize,
    budget: usize,
    ignore: &'a [GlobMatcher],
}

/// Human-readable byte size, e.g. `537 B`, `1.5 KB`, `3.4 MB`.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Collect one directory level (and, within `max_depth`, its subdirectories) in
/// display order: directories first, then files (case-insensitive), each
/// subdirectory's entries immediately after it. `depth` is 0 for the listed
/// directory's immediate children. A failed read of the top directory is an
/// error; failures deeper in the tree are skipped so one unreadable subdirectory
/// does not abort the whole listing.
fn collect_entries(
    dir_path: &Path,
    relative_dir: &Path,
    dir_label: &str,
    depth: usize,
    options: &WalkOptions<'_>,
    out: &mut Vec<Entry>,
) -> Result<()> {
    if out.len() >= options.budget {
        return Ok(());
    }
    let read = match fs::read_dir(dir_path) {
        Ok(read) => read,
        Err(error) if depth == 0 => {
            let context = format!("cannot read directory: {}", dir_label);
            return Err(error).context(context);
        }
        Err(_) => return Ok(()),
    };

    let mut level: Vec<Entry> = Vec::new();
    for entry in read {
        let Ok(entry) = entry else { continue };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name();
        let relative_path = relative_dir.join(&name);
        if options
            .ignore
            .iter()
            .any(|matcher| matcher.is_match(&relative_path))
        {
            continue;
        }
        let is_symlink = file_type.is_symlink();
        let is_dir = file_type.is_dir()
            || (is_symlink && entry.metadata().map(|m| m.is_dir()).unwrap_or(false));
        // Size is only meaningful (and only rendered) for regular files.
        let size = if is_dir || is_symlink {
            0
        } else {
            entry.metadata().map(|m| m.len()).unwrap_or(0)
        };
        level.push(Entry {
            depth,
            name: name.to_string_lossy().to_string(),
            is_dir,
            is_symlink,
            size,
        });
    }

    // Directories first, then files; case-insensitive within each group.
    level.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase()));

    for entry in level {
        // Stop the whole walk once the scan budget is reached; the caller marks
        // the total as a lower bound.
        if out.len() >= options.budget {
            return Ok(());
        }
        // Descend into real subdirectories only: never follow a symlink, so the
        // walk cannot cycle or leave the resolved root.
        let child = if entry.is_dir && !entry.is_symlink && depth + 1 < options.max_depth {
            Some(dir_path.join(&entry.name))
        } else {
            None
        };
        let child_label = entry.name.clone();
        out.push(entry);
        if let Some(child) = child {
            let child_relative = relative_dir.join(&child_label);
            collect_entries(
                &child,
                &child_relative,
                &child_label,
                depth + 1,
                options,
                out,
            )?;
        }
    }
    Ok(())
}

/// Render a single entry line: an optional `long` type marker and size column,
/// then depth indentation, the name, and a `/` suffix for directories.
fn render_line(entry: &Entry, long: bool) -> String {
    let indent = "  ".repeat(entry.depth);
    let suffix = if entry.is_dir { "/" } else { "" };
    let name = &entry.name;
    if long {
        let marker = if entry.is_symlink {
            "l"
        } else if entry.is_dir {
            "d"
        } else {
            "f"
        };
        let size_col = if entry.is_dir || entry.is_symlink {
            "-".to_string()
        } else {
            human_size(entry.size)
        };
        format!("{marker} {size_col:>8} {indent}{name}{suffix}")
    } else {
        format!("{indent}{name}{suffix}")
    }
}

/// Render entries up to the cap and the byte/line rail, appending a truncation
/// summary when any entry is left out. An under-cap listing is byte-identical to
/// the historical flat/tree output (no summary).
fn render_listing(
    entries: &[Entry],
    limit: usize,
    long: bool,
    max_lines: usize,
    max_bytes: usize,
    scan_capped: bool,
) -> Rendered {
    let total = entries.len();
    let mut lines: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut shown = 0usize;
    for entry in entries {
        if shown >= limit || shown >= max_lines {
            break;
        }
        let line = render_line(entry, long);
        let add = if shown > 0 {
            1 + line.len()
        } else {
            line.len()
        };
        // Always emit at least the first line; past that, stop before the byte cap.
        if shown > 0 && bytes + add > max_bytes {
            break;
        }
        bytes += add;
        lines.push(line);
        shown += 1;
    }

    let mut body = lines.join("\n");
    let truncated = shown < total || scan_capped;
    if truncated {
        body.push_str(&summarize_omitted(entries, shown, scan_capped));
    }
    Rendered { body, truncated }
}

/// Terse summary appended to a truncated listing: the exact total with its
/// dirs/files split, the shown/omitted counts, and the dominant file extensions
/// among the omitted entries, so the model knows what was cut without
/// re-listing.
fn summarize_omitted(entries: &[Entry], shown: usize, scan_capped: bool) -> String {
    let total = entries.len();
    let dirs = entries.iter().filter(|e| e.is_dir).count();
    let files = total - dirs;
    let omitted = &entries[shown..];

    // Dominant extensions among omitted files; directories carry none.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for e in omitted {
        if e.is_dir {
            continue;
        }
        let ext = Path::new(&e.name)
            .extension()
            .map(|x| format!(".{}", x.to_string_lossy()))
            .unwrap_or_else(|| "(no ext)".to_string());
        *counts.entry(ext).or_default() += 1;
    }

    let ext_line = if counts.is_empty() {
        String::new()
    } else {
        let mut top: Vec<(String, usize)> = counts.into_iter().collect();
        // Highest omitted count first; ties broken by extension for stable output.
        top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let more = if top.len() > SUMMARY_TOP_EXT {
            ", ..."
        } else {
            ""
        };
        let listed = top
            .iter()
            .take(SUMMARY_TOP_EXT)
            .map(|(ext, n)| format!("{ext} ({n})"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("\nomitted ext: {listed}{more}")
    };

    // When the scan hit its budget, total/dirs/files/omitted are lower bounds:
    // mark it so the model does not read them as exact.
    if scan_capped {
        format!(
            "\n\n[>={total} entries (scan capped): {dirs} dirs, {files} files; {shown} shown, >={} omitted]{ext_line}",
            omitted.len()
        )
    } else {
        format!(
            "\n\n[{total} entries: {dirs} dirs, {files} files; {shown} shown, {} omitted]{ext_line}",
            omitted.len()
        )
    }
}

#[cfg(test)]
#[path = "ls_corpus/corpus.rs"]
mod corpus;

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
                long: false,
                ignore: None,
            },
        )
        .unwrap()
        .content
    }

    #[test]
    fn human_size_formats_units() {
        assert_eq!(human_size(537), "537 B");
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn ls_long_shows_kind_and_size() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir(dir.path.join("sub")).unwrap();
        fs::write(dir.path.join("a.txt"), "hello").unwrap();
        let out = ls(
            &root,
            &LsInput {
                path: None,
                limit: None,
                recursive: false,
                depth: None,
                long: true,
                ignore: None,
            },
        )
        .unwrap()
        .content;
        // Directory first with a `d` marker and `-` size, then the file with an
        // `f` marker and its byte size.
        assert_eq!(out, "d        - sub/\nf      5 B a.txt");
    }

    #[test]
    fn ls_reports_entry_count_metadata() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir(dir.path.join("sub")).unwrap();
        fs::write(dir.path.join("a.txt"), "x").unwrap();
        let output = ls(
            &root,
            &LsInput {
                path: None,
                limit: None,
                recursive: false,
                depth: None,
                long: false,
                ignore: None,
            },
        )
        .unwrap();
        assert_eq!(output.metadata.get("entries"), Some(&json!(2)));
        assert_eq!(output.metadata.get("truncated"), Some(&json!(false)));
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
    fn ls_ignore_string_filters_matching_entries() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("app.log"), "x").unwrap();
        fs::write(dir.path.join("app.rs"), "x").unwrap();

        let output = execute(&root, &json!({ "ignore": "*.log" }), false).unwrap();

        assert_eq!(output.content, "app.rs");
        assert_eq!(output.metadata.get("entries"), Some(&json!(1)));
    }

    #[test]
    fn ls_ignore_array_filters_multiple_patterns_and_prunes_directories() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir_all(dir.path.join("target/debug")).unwrap();
        fs::create_dir_all(dir.path.join("src/generated")).unwrap();
        fs::write(dir.path.join("target/debug/build.log"), "x").unwrap();
        fs::write(dir.path.join("src/generated/schema.rs"), "x").unwrap();
        fs::write(dir.path.join("src/lib.rs"), "x").unwrap();

        let output = execute(
            &root,
            &json!({
                "recursive": true,
                "depth": 3,
                "ignore": ["target", "**/generated"]
            }),
            false,
        )
        .unwrap();

        assert_eq!(output.content, "src/\n  lib.rs");
        assert_eq!(output.metadata.get("entries"), Some(&json!(2)));
    }

    #[test]
    fn ls_rejects_invalid_ignore_glob() {
        let dir = temp_dir();
        let root = root_of(&dir);

        let error = execute(&root, &json!({ "ignore": "[" }), false)
            .unwrap_err()
            .to_string();

        assert!(error.contains("invalid glob pattern"), "{error}");
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
                long: false,
                ignore: None,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("limit"), "{err}");
    }

    fn ls_limited(root: &Path, limit: usize, recursive: bool, depth: Option<usize>) -> String {
        ls(
            root,
            &LsInput {
                path: None,
                limit: Some(limit),
                recursive,
                depth,
                long: false,
                ignore: None,
            },
        )
        .unwrap()
        .content
    }

    #[test]
    fn ls_under_cap_has_no_summary() {
        // An untruncated listing stays byte-identical to the historical output:
        // no summary line is appended.
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir(dir.path.join("sub")).unwrap();
        fs::write(dir.path.join("a.rs"), "x").unwrap();
        let out = ls_in(&root, false, None);
        assert_eq!(out, "sub/\na.rs");
        assert!(!out.contains("entries:"), "{out}");
        assert!(!out.contains("omitted"), "{out}");
    }

    #[test]
    fn ls_truncation_appends_summary_with_totals_and_ext() {
        let dir = temp_dir();
        let root = root_of(&dir);
        for d in ["adir", "bdir", "cdir"] {
            fs::create_dir(dir.path.join(d)).unwrap();
        }
        for i in 0..6 {
            fs::write(dir.path.join(format!("f{i}.rs")), "x").unwrap();
        }
        for i in 0..2 {
            fs::write(dir.path.join(format!("n{i}.txt")), "x").unwrap();
        }
        // 3 dirs + 8 files = 11 entries; limit 4 shows the 3 dirs and f0.rs.
        let output = ls(
            &root,
            &LsInput {
                path: None,
                limit: Some(4),
                recursive: false,
                depth: None,
                long: false,
                ignore: None,
            },
        )
        .unwrap();
        let body = &output.content;
        assert!(
            body.contains("[11 entries: 3 dirs, 8 files; 4 shown, 7 omitted]"),
            "{body}"
        );
        // Omitted files: f1..f5 (5 .rs) and n0,n1 (2 .txt); dirs carry no ext.
        assert!(body.contains("omitted ext: .rs (5), .txt (2)"), "{body}");
        assert_eq!(output.metadata.get("entries"), Some(&json!(11)));
        assert_eq!(output.metadata.get("truncated"), Some(&json!(true)));
    }

    #[test]
    fn ls_summary_labels_extensionless_files() {
        let dir = temp_dir();
        let root = root_of(&dir);
        for n in ["Makefile", "README", "LICENSE"] {
            fs::write(dir.path.join(n), "x").unwrap();
        }
        // Case-insensitive file order: LICENSE, Makefile, README; limit 1 shows LICENSE.
        let out = ls_limited(&root, 1, false, None);
        assert!(
            out.contains("[3 entries: 0 dirs, 3 files; 1 shown, 2 omitted]"),
            "{out}"
        );
        assert!(out.contains("omitted ext: (no ext) (2)"), "{out}");
    }

    #[test]
    fn ls_tree_truncation_summary_counts_full_tree() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::create_dir(dir.path.join("src")).unwrap();
        for i in 0..5 {
            fs::write(dir.path.join(format!("src/m{i}.rs")), "x").unwrap();
        }
        // depth 2 collects src/ and its 5 files = 6 entries; limit 2 omits 4,
        // and the summary counts the whole depth-bounded tree, not just the cap.
        let out = ls_limited(&root, 2, true, Some(2));
        assert!(
            out.contains("[6 entries: 1 dirs, 5 files; 2 shown, 4 omitted]"),
            "{out}"
        );
        assert!(out.contains("omitted ext: .rs (4)"), "{out}");
    }

    #[test]
    fn ls_scan_budget_bounds_recursive_collection() {
        // A deep recursive request must not traverse an unbounded tree. With a
        // tiny budget the walk stops early and the summary reports a lower bound
        // instead of an exact (and expensive-to-compute) total.
        let dir = temp_dir();
        for d in ["a", "b", "c"] {
            fs::create_dir(dir.path.join(d)).unwrap();
            for i in 0..3 {
                fs::write(dir.path.join(format!("{d}/f{i}.rs")), "x").unwrap();
            }
        }
        // Full depth-2 tree = 3 dirs + 9 files = 12 entries; budget 4 stops at 4.
        let mut entries = Vec::new();
        let walk = WalkOptions {
            max_depth: 2,
            budget: 4,
            ignore: &[],
        };
        collect_entries(&dir.path, Path::new(""), ".", 0, &walk, &mut entries).unwrap();
        assert_eq!(entries.len(), 4, "collection is bounded by the scan budget");

        let out = render_listing(
            &entries,
            2,
            false,
            DEFAULT_MAX_LINES,
            DEFAULT_MAX_BYTES,
            true,
        )
        .body;
        assert!(out.contains("scan capped"), "{out}");
        assert!(out.contains(">=4 entries"), "{out}");
    }
}
