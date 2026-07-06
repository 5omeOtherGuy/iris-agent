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

use std::collections::HashMap;
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

pub(super) const DESCRIPTION: &str = "Search for files by glob pattern. Returns matching file paths relative to the search directory. Sorted by modification time (newest first). Respects .gitignore. Output is truncated to 1000 results or 50KB (whichever is hit first); a truncated result ends with a summary line carrying the exact total match count and the top directories by omitted-match count so the glob can be narrowed without a blind re-run.";

/// Number of top directories named in a truncation summary.
const SUMMARY_TOP_DIRS: usize = 5;

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

pub(super) fn execute(root: &Path, args: &Value, reduce: bool) -> Result<super::ToolOutput> {
    let input: FindInput =
        serde_json::from_value(args.clone()).context("find tool arguments must include pattern")?;
    Ok(super::ToolOutput::text(find(root, &input, reduce)?))
}

#[derive(Debug, Deserialize)]
struct FindInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `group` (issue #210 benchmark arm): `true` picks the shipped group-by-
/// directory compaction when it is smaller; `false` forces the flat listing
/// baseline that grouping is compared against. Only the tokens-per-task
/// benchmark's baseline arm passes `false`.
fn find(root: &Path, input: &FindInput, group: bool) -> Result<String> {
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
    if entries.is_empty() {
        return Ok("No files found matching pattern".to_string());
    }

    let total = entries.len();
    let all_rel: Vec<String> = entries.into_iter().map(|(rel, _)| rel).collect();
    let user_limit = input.limit.is_some();
    Ok(render_results(
        &all_rel,
        total,
        limit,
        user_limit,
        DEFAULT_MAX_LINES,
        DEFAULT_MAX_BYTES,
        group,
    ))
}

/// Render the sorted match list, compacting only when matches would otherwise
/// be dropped. Under-cap results (and explicit-`limit` results that still fit)
/// stay byte-identical to the historical flat listing; a result that omits any
/// match is rendered compactly and carries a summary of what was cut.
fn render_results(
    all_rel: &[String],
    total: usize,
    limit: usize,
    user_limit: bool,
    max_lines: usize,
    max_bytes: usize,
    group: bool,
) -> String {
    let candidates = &all_rel[..total.min(limit)];
    let flat_all = candidates.join("\n");
    let flat_exceeds = candidates.len() > max_lines || flat_all.len() > max_bytes;
    let omitted_by_limit = total - candidates.len();
    // Compact when the byte/line rail would truncate the listing, or when the
    // default limit (not an explicit user limit) silently drops matches.
    let needs_compact = flat_exceeds || (omitted_by_limit > 0 && !user_limit);
    if !needs_compact {
        let (body, truncated, _) = truncate_head(&flat_all, max_lines, max_bytes);
        let mut out = body;
        if truncated {
            out.push_str("\n\n[output truncated]");
        }
        return out;
    }
    render_compact(all_rel, total, limit, max_lines, max_bytes, group)
}

/// A representation fitted to the byte/line budget: how many leading paths it
/// shows and its rendered byte size (used to pick flat vs grouped).
struct Fitted {
    body: String,
    shown: usize,
    bytes: usize,
}

/// Compact rendering for a truncated result: pick the smaller of the flat and
/// grouped forms of the shown prefix, then append the omitted-match summary.
fn render_compact(
    all_rel: &[String],
    total: usize,
    limit: usize,
    max_lines: usize,
    max_bytes: usize,
    group: bool,
) -> String {
    let candidates = &all_rel[..total.min(limit)];
    let flat = fit_flat(candidates, max_lines, max_bytes);
    let grouped = fit_grouped(candidates, max_lines, max_bytes);
    // Grouping wins only when it shows strictly more paths, or the same paths in
    // fewer bytes. Ties and "grouped is larger" keep the flat form (the
    // one-file-per-directory case, where `dir/ name` costs more than `dir/name`).
    // The benchmark baseline arm (`group == false`) forces flat regardless, so
    // grouping's contribution to the token delta can be measured.
    let use_grouped = group
        && (grouped.shown > flat.shown
            || (grouped.shown == flat.shown && grouped.bytes < flat.bytes));
    let (body, shown) = if use_grouped {
        (grouped.body, grouped.shown)
    } else {
        (flat.body, flat.shown)
    };
    let mut out = body;
    let omitted = &all_rel[shown..];
    if !omitted.is_empty() {
        out.push_str(&summarize_omitted(omitted, total, shown));
    }
    out
}

/// Fit as many leading paths as the budget allows, one path per line.
fn fit_flat(paths: &[String], max_lines: usize, max_bytes: usize) -> Fitted {
    let mut bytes = 0usize;
    let mut shown = 0usize;
    for p in paths {
        if shown + 1 > max_lines {
            break;
        }
        let add = if shown > 0 { 1 + p.len() } else { p.len() };
        if bytes + add > max_bytes {
            break;
        }
        bytes += add;
        shown += 1;
    }
    Fitted {
        body: paths[..shown].join("\n"),
        shown,
        bytes,
    }
}

/// Fit as many leading paths as the budget allows, grouped by parent directory
/// (`dir/ a.rs b.rs`). Directories appear in first-seen (newest-first) order.
fn fit_grouped(paths: &[String], max_lines: usize, max_bytes: usize) -> Fitted {
    let mut order: Vec<String> = Vec::new();
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    let mut bytes = 0usize;
    let mut shown = 0usize;
    for p in paths {
        let (parent, name) = split_parent_name(p);
        let dir_disp = if parent.is_empty() {
            ".".to_string()
        } else {
            parent.to_string()
        };
        let is_new = !map.contains_key(&dir_disp);
        let add = if is_new {
            let newline = usize::from(!order.is_empty());
            newline + dir_disp.len() + 2 + name.len() // "dir/ name"
        } else {
            1 + name.len() // " name"
        };
        if is_new && order.len() + 1 > max_lines {
            break;
        }
        if bytes + add > max_bytes {
            break;
        }
        bytes += add;
        if is_new {
            order.push(dir_disp.clone());
        }
        map.entry(dir_disp).or_default().push(name);
        shown += 1;
    }
    let body = order
        .iter()
        .map(|d| format!("{d}/ {}", map[d].join(" ")))
        .collect::<Vec<_>>()
        .join("\n");
    Fitted { body, shown, bytes }
}

/// Split a relative path into (parent, file name), preserving a trailing `/` on
/// directory entries as part of the name (`src/foo/` -> (`src`, `foo/`)).
fn split_parent_name(path: &str) -> (&str, String) {
    let is_dir = path.ends_with('/');
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    match trimmed.rsplit_once('/') {
        Some((parent, name)) => (
            parent,
            if is_dir {
                format!("{name}/")
            } else {
                name.to_string()
            },
        ),
        None => (
            "",
            if is_dir {
                format!("{trimmed}/")
            } else {
                trimmed.to_string()
            },
        ),
    }
}

/// Terse summary of omitted matches: exact total, shown/omitted counts, and the
/// top directories by omitted-match count so the caller can narrow the glob.
fn summarize_omitted(omitted: &[String], total: usize, shown: usize) -> String {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for p in omitted {
        let (parent, _) = split_parent_name(p);
        *counts.entry(parent).or_default() += 1;
    }
    let mut dirs: Vec<(&str, usize)> = counts.into_iter().collect();
    // Highest omitted count first; ties broken by directory for stable output.
    dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let top = dirs
        .iter()
        .take(SUMMARY_TOP_DIRS)
        .map(|(dir, count)| {
            let disp = if dir.is_empty() { "." } else { *dir };
            format!("{disp}/ ({count})")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let more = if dirs.len() > SUMMARY_TOP_DIRS {
        ", ..."
    } else {
        ""
    };
    format!(
        "\n\n[{total} matches, {shown} shown, {} omitted]\nomitted by dir: {top}{more}",
        omitted.len()
    )
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
        let out = find(&root, &input("*.rs", None), true).unwrap();
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
        let out = find(&root, &input("src/*.rs", None), true).unwrap();
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
        let out = find(&root, &input("*.rs", None), true).unwrap();
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
        let out = find(&root, &input("*.rs", None), true).unwrap();
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
        let out = find(&root, &input("*.rs", Some(2)), true).unwrap();
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
        let any = find(&root, &input("*.rs", None), true).unwrap();
        assert!(
            any.contains("lower.rs") && any.contains("UPPER.RS"),
            "{any}"
        );
        // A pattern with an uppercase char is case-sensitive.
        let exact = find(&root, &input("*.RS", None), true).unwrap();
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
        let out = find(&root, &input("src/*.rs", None), true).unwrap();
        assert!(out.contains("src/top.rs"), "{out}");
        assert!(!out.contains("src/deep/low.rs"), "{out}");
    }

    #[test]
    fn find_no_match_returns_message() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("a.rs"), "x").unwrap();
        let out = find(&root, &input("*.zzz", None), true).unwrap();
        assert_eq!(out, "No files found matching pattern");
    }

    #[test]
    fn find_rejects_zero_limit() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let err = find(&root, &input("*.rs", Some(0)), true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("limit"), "{err}");
    }

    // --- compaction unit tests (issue #340) ---

    fn owned(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|s| s.to_string()).collect()
    }

    // Real captured `find`-style listings from a checked-out repo (codex-rs),
    // used as compaction fixtures. concentrated: many `.rs` files sharing
    // parent directories (grouping should win). singletons: one file per
    // directory (grouping should lose).
    fn concentrated() -> Vec<String> {
        include_str!("find_corpus/concentrated.txt")
            .lines()
            .map(str::to_string)
            .collect()
    }
    fn singletons() -> Vec<String> {
        include_str!("find_corpus/singletons.txt")
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn split_parent_name_handles_files_dirs_and_root() {
        assert_eq!(
            split_parent_name("src/foo/a.rs"),
            ("src/foo", "a.rs".into())
        );
        assert_eq!(split_parent_name("a.rs"), ("", "a.rs".into()));
        assert_eq!(split_parent_name("src/foo/"), ("src", "foo/".into()));
        assert_eq!(split_parent_name("foo/"), ("", "foo/".into()));
    }

    #[test]
    fn under_cap_result_is_byte_identical_flat() {
        // No omission and within caps => exactly the historical flat listing.
        let paths = owned(&["z.rs", "src/a.rs", "src/b.rs"]);
        let out = render_results(&paths, paths.len(), 1000, false, 2000, 50 * 1024, true);
        assert_eq!(out, paths.join("\n"));
    }

    #[test]
    fn explicit_limit_that_fits_stays_flat_without_summary() {
        // Mirrors find_limit_keeps_newest: an explicit user limit that fits the
        // byte/line caps prints the flat prefix with no summary.
        let paths = owned(&["a.rs", "b.rs", "c.rs"]);
        let out = render_results(&paths[..2], 3, 2, true, 2000, 50 * 1024, true);
        assert_eq!(out, "a.rs\nb.rs");
    }

    #[test]
    fn truncation_summary_reports_exact_total_and_top_dir() {
        // Force compaction with a tiny byte cap over a real listing so most
        // matches are omitted; the summary must carry the exact total and the
        // correct top-directory-by-omitted-count.
        let all = concentrated();
        let total = all.len();
        let out = render_compact(&all, total, total, 2000, 400, true);
        let summary = out
            .lines()
            .find(|l| l.starts_with('[') && l.contains("matches,"))
            .expect("summary line present");
        // Parse [T matches, S shown, O omitted].
        let nums: Vec<usize> = summary
            .trim_matches(['[', ']'])
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().unwrap())
            .collect();
        let (reported_total, shown, omitted) = (nums[0], nums[1], nums[2]);
        assert_eq!(reported_total, total, "exact total match count");
        assert_eq!(shown + omitted, total, "shown + omitted == total");
        assert!(omitted > 0, "fixture must over-cap: {summary}");

        // Independently recompute the plurality-omitted directory and assert the
        // summary names it with the correct count (DoD #4: counts are correct).
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for p in &all[shown..] {
            *counts.entry(split_parent_name(p).0).or_default() += 1;
        }
        let (top_dir, top_count) = counts
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
            .map(|(d, c)| (*d, *c))
            .unwrap();
        let omit_line = out
            .lines()
            .find(|l| l.starts_with("omitted by dir:"))
            .expect("omitted-by-dir line present");
        assert!(
            omit_line.contains(&format!("{top_dir}/ ({top_count})")),
            "top omitted dir {top_dir}/ ({top_count}) must lead: {omit_line}"
        );
    }

    #[test]
    fn grouping_used_when_measurably_smaller() {
        // Concentrated real tree: grouped is chosen and renders `dir/ a b`.
        let all = concentrated();
        let flat = fit_flat(&all, usize::MAX, usize::MAX);
        let grouped = fit_grouped(&all, usize::MAX, usize::MAX);
        assert_eq!(flat.shown, grouped.shown, "both fit the whole set");
        assert!(
            grouped.bytes < flat.bytes,
            "grouped {} should be < flat {}",
            grouped.bytes,
            flat.bytes
        );
        let out = render_compact(&all, all.len(), 200, 2000, 50 * 1024, true);
        assert!(
            out.lines().next().unwrap().contains("/ "),
            "grouped body expected: {}",
            out.lines().next().unwrap()
        );
    }

    #[test]
    fn benchmark_baseline_arm_forces_flat_even_when_grouping_would_win() {
        // Issue #210 arm switch: on a tree where grouping is measurably smaller,
        // the baseline arm (`group == false`) still renders the flat listing, so
        // grouping's token contribution can be isolated. The baseline is never
        // smaller than the grouped arm on the same set.
        let all = concentrated();
        let grouped = render_compact(&all, all.len(), 200, 2000, 50 * 1024, true);
        let flat = render_compact(&all, all.len(), 200, 2000, 50 * 1024, false);
        // Grouped picks the `dir/ a b` body; flat keeps one path per line.
        assert!(grouped.lines().next().unwrap().contains("/ "));
        let first = flat.lines().next().unwrap();
        assert!(
            !first.contains("/ ") && first == all[0],
            "baseline arm must render flat, got {first}"
        );
        assert!(
            flat.len() >= grouped.len(),
            "flat baseline ({} bytes) must not beat grouped ({} bytes)",
            flat.len(),
            grouped.len(),
        );
    }

    #[test]
    fn grouping_not_used_when_flat_is_smaller() {
        // One file per directory: `dir/ name` costs more than `dir/name`, so the
        // flat form must stay (DoD #5).
        let all = singletons();
        let flat = fit_flat(&all, usize::MAX, usize::MAX);
        let grouped = fit_grouped(&all, usize::MAX, usize::MAX);
        assert_eq!(flat.shown, grouped.shown);
        assert!(
            grouped.bytes >= flat.bytes,
            "grouped {} should not beat flat {} on singletons",
            grouped.bytes,
            flat.bytes
        );
        // render_compact picks flat: first line is a single path, not `dir/ a b`.
        let out = render_compact(&all, all.len(), 5, 2000, 50 * 1024, true);
        let first = out.lines().next().unwrap();
        assert_eq!(first, all[0], "flat body expected, got {first}");
    }

    #[test]
    fn grouping_shows_more_within_a_tight_byte_budget() {
        // Under a shared byte budget the denser grouped form fits more paths.
        let all = concentrated();
        let flat = fit_flat(&all, 2000, 300);
        let grouped = fit_grouped(&all, 2000, 300);
        assert!(
            grouped.shown > flat.shown,
            "grouped {} should show more than flat {}",
            grouped.shown,
            flat.shown
        );
    }

    // --- benchmark corpus (issue #340, ADR-0036 rule 5) ---
    //
    // before = flat listing (one path per line); after = grouped-by-directory
    // listing of the same set. Both render the whole fixture (uncapped) to
    // measure the representation compression, independent of the byte rail.

    fn flat_listing(paths: &[String]) -> String {
        paths.join("\n")
    }
    fn grouped_listing(paths: &[String]) -> String {
        fit_grouped(paths, usize::MAX, usize::MAX).body
    }

    #[test]
    fn bench_concentrated_grouping_reduces() {
        let all = concentrated();
        let before = flat_listing(&all);
        let after = grouped_listing(&all);
        crate::tools::bench_support::assert_min_reduction(
            "find (concentrated .rs)",
            &before,
            &after,
            40,
        );
        // Zero quality loss: sampled real paths survive grouping verbatim.
        crate::tools::bench_support::assert_survives_verbatim(
            "find (concentrated .rs)",
            &after,
            &[
                all[0].rsplit_once('/').unwrap().1,
                all[all.len() / 2].rsplit_once('/').unwrap().1,
            ],
        );
    }

    #[test]
    fn bench_singletons_flat_stays_flat() {
        // Grouping must not be smaller here, so the flat form is kept.
        let all = singletons();
        let flat = fit_flat(&all, usize::MAX, usize::MAX);
        let grouped = fit_grouped(&all, usize::MAX, usize::MAX);
        assert!(grouped.bytes >= flat.bytes);
    }

    #[test]
    fn find_benchmark_report() {
        // Prints the table committed to docs/benchmarks/issue-340-find-compaction.md.
        // Regenerate with: cargo test find_benchmark_report -- --nocapture
        use crate::tools::bench_support::{report_header, report_row};
        println!("{}", report_header());
        let conc = concentrated();
        println!(
            "{}",
            report_row(
                "find (concentrated .rs)",
                &flat_listing(&conc),
                &grouped_listing(&conc),
                "group-by-dir",
            )
        );
        let single = singletons();
        let single_flat = flat_listing(&single);
        let single_grouped = grouped_listing(&single);
        // Grouping loses here, so the shipped form is flat (passthrough).
        let (single_after, via) = if single_grouped.len() < single_flat.len() {
            (single_grouped.as_str(), "group-by-dir")
        } else {
            (single_flat.as_str(), "(flat)")
        };
        println!(
            "{}",
            report_row("find (one file per dir)", &single_flat, single_after, via,)
        );
    }
}
