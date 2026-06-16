//! `grep` — content search backed by the ripgrep library crates.
//!
//! Uses `ignore` for the .gitignore-aware walk and glob filtering, and
//! `grep-regex` + `grep-searcher` for matching, so the tool needs no external
//! `rg` binary on PATH. Output is grouped by file with context lines and is
//! shaped for agent consumption rather than raw `rg` compatibility.

use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};
use grep::regex::RegexMatcherBuilder;
use grep::searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use serde::Deserialize;
use serde_json::{Value, json};

use super::path::{relative_display, resolve_existing};
use super::text::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head};

/// Per-line content cap so a single minified line cannot blow the budget.
const GREP_MAX_LINE_LENGTH: usize = 500;
/// Default and safety caps tuned for agent use.
const DEFAULT_GREP_LIMIT: usize = 100;
const DEFAULT_CONTEXT: usize = 2;
const MAX_FILES: usize = 50;

pub(super) const DESCRIPTION: &str = "Search file contents for a pattern. Returns matches grouped by file with line numbers and surrounding context; match lines are marked with '>'. Respects .gitignore. Output is truncated to 100 matches, 50 files, or 1MB (whichever is hit first). Long lines are truncated to 500 chars.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
            "path": { "type": "string", "description": "Directory or file to search (default: current directory)" },
            "glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
            "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
            "literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
            "context": { "type": "integer", "description": "Lines of context before and after each match (default: 2)" },
            "limit": { "type": "integer", "description": "Maximum number of matches to return (default: 100)" }
        },
        "required": ["pattern"]
    })
}

pub(super) fn execute(root: &Path, args: &Value) -> Result<super::ToolOutput> {
    let input: GrepInput =
        serde_json::from_value(args.clone()).context("grep tool arguments must include pattern")?;
    Ok(super::ToolOutput::text(grep(root, &input)?))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    literal: bool,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

/// One rendered line within a file group: either a match or a context line.
struct Line {
    number: u64,
    is_match: bool,
    text: String,
}

/// Matches for a single file, in the order the searcher emitted them.
struct FileHits {
    path: String,
    lines: Vec<Line>,
    /// Indices (into `lines`) after which the searcher reported a context gap.
    breaks: Vec<usize>,
}

fn grep(root: &Path, input: &GrepInput) -> Result<String> {
    if matches!(input.limit, Some(0)) {
        bail!("`limit` must be greater than 0");
    }
    let search = input.path.as_deref().unwrap_or(".");
    let search_path = resolve_existing(root, search)?;
    let limit = input.limit.unwrap_or(DEFAULT_GREP_LIMIT).max(1);
    let context = input.context.unwrap_or(DEFAULT_CONTEXT);

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(input.ignore_case)
        .fixed_strings(input.literal)
        .build(&input.pattern)
        .with_context(|| format!("invalid search pattern: {}", input.pattern))?;

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(context)
        .after_context(context)
        .build();

    // Glob filter, applied via gitignore-style overrides (a positive glob means
    // "only files matching this"). Anchored at the search path.
    let overrides = match &input.glob {
        Some(glob) => {
            let mut builder = OverrideBuilder::new(&search_path);
            builder
                .add(glob)
                .with_context(|| format!("invalid glob pattern: {glob}"))?;
            Some(builder.build().context("failed to build glob filter")?)
        }
        None => None,
    };

    let mut files: Vec<FileHits> = Vec::new();
    let mut total_matches = 0usize;
    let mut truncated_matches = false;
    let mut truncated_files = false;

    // `--hidden` parity: include dotfiles. `.gitignore` is still respected.
    let mut walk = WalkBuilder::new(&search_path);
    walk.hidden(false);
    if let Some(ov) = overrides {
        walk.overrides(ov);
    }

    'walk: for entry in walk.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        if files.len() >= MAX_FILES {
            truncated_files = true;
            break;
        }

        let remaining = limit - total_matches;
        let mut sink = MatchSink::new(remaining);
        searcher.search_path(&matcher, entry.path(), &mut sink).ok();

        if sink.lines.is_empty() {
            continue;
        }
        total_matches += sink.match_count;
        files.push(FileHits {
            path: relative_display(root, entry.path()),
            lines: sink.lines,
            breaks: sink.breaks,
        });
        if total_matches >= limit {
            truncated_matches = true;
            break 'walk;
        }
    }

    if files.is_empty() {
        return Ok("No matches found".to_string());
    }

    Ok(render(
        &files,
        total_matches,
        truncated_matches,
        truncated_files,
    ))
}

fn render(
    files: &[FileHits],
    total_matches: usize,
    trunc_matches: bool,
    trunc_files: bool,
) -> String {
    let file_word = if files.len() == 1 { "file" } else { "files" };
    let match_word = if total_matches == 1 {
        "match"
    } else {
        "matches"
    };
    let mut out = format!(
        "{total_matches} {match_word} in {} {file_word}\n",
        files.len()
    );

    for file in files {
        out.push('\n');
        out.push_str(&file.path);
        out.push('\n');
        for (idx, line) in file.lines.iter().enumerate() {
            let marker = if line.is_match { "> " } else { "  " };
            out.push_str(marker);
            out.push_str(&line.number.to_string());
            out.push_str("│ ");
            out.push_str(&clamp_line(&line.text));
            out.push('\n');
            if file.breaks.contains(&idx) {
                out.push_str("  ⋯\n");
            }
        }
    }

    if trunc_matches {
        out.push_str(&format!(
            "\n[truncated: match limit of {total_matches} reached; refine your search]"
        ));
    } else if trunc_files {
        out.push_str(&format!(
            "\n[truncated: file limit of {MAX_FILES} reached; refine your search]"
        ));
    }

    // Final hard caps on total size.
    let (body, size_truncated, _) = truncate_head(&out, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut out = body;
    if size_truncated {
        out.push_str("\n\n[output truncated]");
    }
    out
}

fn clamp_line(text: &str) -> String {
    if text.len() <= GREP_MAX_LINE_LENGTH {
        return text.to_string();
    }
    let mut cut = GREP_MAX_LINE_LENGTH;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &text[..cut])
}

/// Collects matches and context for a single file, stopping once `remaining`
/// match lines have been seen so the global cap is honored without over-reading.
struct MatchSink {
    lines: Vec<Line>,
    breaks: Vec<usize>,
    match_count: usize,
    remaining: usize,
    pending_break: bool,
}

impl MatchSink {
    fn new(remaining: usize) -> Self {
        Self {
            lines: Vec::new(),
            breaks: Vec::new(),
            match_count: 0,
            remaining,
            pending_break: false,
        }
    }

    fn push(&mut self, number: Option<u64>, is_match: bool, bytes: &[u8]) {
        if self.pending_break && !self.lines.is_empty() {
            self.breaks.push(self.lines.len() - 1);
        }
        self.pending_break = false;
        let text = String::from_utf8_lossy(bytes)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        self.lines.push(Line {
            number: number.unwrap_or(0),
            is_match,
            text,
        });
    }
}

impl Sink for MatchSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.push(mat.line_number(), true, mat.bytes());
        self.match_count += 1;
        // Stop this file once the global remaining budget is spent.
        Ok(self.match_count < self.remaining)
    }

    fn context(&mut self, _searcher: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, io::Error> {
        self.push(ctx.line_number(), false, ctx.bytes());
        Ok(true)
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, io::Error> {
        self.pending_break = true;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::tools::test_support::{root_of, temp_dir};

    fn run(root: &Path, input: GrepInput) -> String {
        grep(root, &input).unwrap()
    }

    fn input(pattern: &str) -> GrepInput {
        GrepInput {
            pattern: pattern.into(),
            path: None,
            glob: None,
            ignore_case: false,
            literal: false,
            context: None,
            limit: None,
        }
    }

    #[test]
    fn groups_matches_by_file_with_context_and_marker() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(
            dir.path.join("g.txt"),
            "alpha\nbeta\nneedle here\ngamma\ndelta\n",
        )
        .unwrap();

        let out = run(&root, input("needle"));

        assert!(out.starts_with("1 match in 1 file\n"), "header: {out}");
        assert!(out.contains("g.txt"));
        // Match line marked, context lines present and indented.
        assert!(out.contains("> 3│ needle here"), "out: {out}");
        assert!(out.contains("  2│ beta"), "out: {out}");
        assert!(out.contains("  4│ gamma"), "out: {out}");
    }

    #[test]
    fn no_matches_reports_cleanly() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("g.txt"), "alpha\nbeta\n").unwrap();
        assert_eq!(run(&root, input("zzz")), "No matches found");
    }

    #[test]
    fn literal_treats_regex_metachars_as_text() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("g.txt"), "a.b\naxb\n").unwrap();

        let mut literal = input("a.b");
        literal.literal = true;
        literal.context = Some(0);
        let out = run(&root, literal);
        assert!(out.contains("> 1│ a.b"), "out: {out}");
        assert!(!out.contains("axb"), "literal should not match axb: {out}");
    }

    #[test]
    fn ignore_case_matches_mixed_case() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("g.txt"), "Needle\n").unwrap();
        let mut ci = input("needle");
        ci.ignore_case = true;
        ci.context = Some(0);
        assert!(run(&root, ci).contains("> 1│ Needle"));
    }

    #[test]
    fn glob_filters_files() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("a.rs"), "needle\n").unwrap();
        fs::write(dir.path.join("b.txt"), "needle\n").unwrap();
        let mut globbed = input("needle");
        globbed.glob = Some("*.rs".into());
        let out = run(&root, globbed);
        assert!(out.contains("a.rs"), "out: {out}");
        assert!(!out.contains("b.txt"), "out: {out}");
    }

    #[test]
    fn limit_caps_matches_and_reports_truncation() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let body: String = (0..10).map(|_| "needle\n").collect();
        fs::write(dir.path.join("g.txt"), body).unwrap();
        let mut limited = input("needle");
        limited.limit = Some(3);
        limited.context = Some(0);
        let out = run(&root, limited);
        assert!(out.starts_with("3 matches in 1 file\n"), "out: {out}");
        assert!(out.contains("[truncated: match limit of 3"), "out: {out}");
    }

    #[test]
    fn respects_gitignore() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.path.join("ignored.txt"), "needle\n").unwrap();
        fs::write(dir.path.join("kept.txt"), "needle\n").unwrap();
        let out = run(&root, input("needle"));
        assert!(out.contains("kept.txt"), "out: {out}");
        assert!(!out.contains("ignored.txt"), "out: {out}");
    }

    #[test]
    fn long_lines_are_clamped() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let long = format!("needle{}\n", "x".repeat(2000));
        fs::write(dir.path.join("g.txt"), long).unwrap();
        let mut one = input("needle");
        one.context = Some(0);
        let out = run(&root, one);
        assert!(out.contains("..."), "out should be clamped: {out}");
        // No single rendered content run exceeds the clamp by much.
        assert!(out.lines().all(|l| l.len() <= GREP_MAX_LINE_LENGTH + 16));
    }
}
