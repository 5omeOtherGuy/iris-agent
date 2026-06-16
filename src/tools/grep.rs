//! `grep` — content search backed by the ripgrep library crates.
//!
//! Uses `ignore` for the .gitignore-aware walk and glob filtering, and
//! `grep-regex` + `grep-searcher` for matching, so the tool needs no external
//! `rg` binary on PATH. Output is grouped by file with context lines and is
//! shaped for agent consumption rather than raw `rg` compatibility.
//!
//! Three output modes (`content`, `files_with_matches`, `count`) with bounded,
//! pageable results. Each run also reports structured, non-sensitive metrics
//! (mode, match/file counts, truncation, skips, next page offset) via
//! [`super::ToolOutput`] metadata. The metrics never include the pattern, path,
//! or glob, so query terms never reach metadata, the session log, or tracing.

use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};
use grep::regex::{RegexMatcher, RegexMatcherBuilder};
use grep::searcher::{
    BinaryDetection, MmapChoice, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch,
};
use ignore::WalkBuilder;
use ignore::overrides::{Override, OverrideBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

use super::path::{relative_display, resolve_existing};
use super::text::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head};

/// Per-line content cap so a single minified line cannot blow the budget.
const GREP_MAX_LINE_LENGTH: usize = 500;
/// Default and safety caps tuned for agent use.
const DEFAULT_GREP_LIMIT: usize = 100;
const DEFAULT_CONTEXT: usize = 2;
const MAX_CONTEXT: usize = 20;
const MAX_GREP_LIMIT: usize = 1_000;
const MAX_HEAD_LIMIT: usize = 500;
const MAX_OFFSET: usize = 10_000;

pub(super) const DESCRIPTION: &str = "Search file contents for a pattern. Native ripgrep-style exact search: list matching files, show matching content with context, or count matches. Respects .gitignore and stays inside the workspace. Output is bounded by limit/headLimit and long lines are truncated to 500 chars.";

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
            "limit": { "type": "integer", "description": "Maximum number of matches or matching files to scan (default: 100, capped at 1000)" },
            "outputMode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "Result shape: matching content, files containing matches, or per-file match counts (default: content)" },
            "headLimit": { "type": "integer", "description": "Maximum number of output rows to show from the selected mode (capped at 500)" },
            "offset": { "type": "integer", "description": "Output row offset for pagination (default: 0)" }
        },
        "required": ["pattern"]
    })
}

pub(super) fn execute(root: &Path, args: &Value) -> Result<super::ToolOutput> {
    let input: GrepInput =
        serde_json::from_value(args.clone()).context("grep tool arguments must include pattern")?;
    let (text, meta) = grep(root, &input)?;
    Ok(meta.attach(super::ToolOutput::text(text)))
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
    #[serde(default)]
    output_mode: OutputMode,
    #[serde(default)]
    head_limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    Content,
    FilesWithMatches,
    Count,
}

impl OutputMode {
    fn label(self) -> &'static str {
        match self {
            OutputMode::Content => "content",
            OutputMode::FilesWithMatches => "files_with_matches",
            OutputMode::Count => "count",
        }
    }
}

/// Structured, non-sensitive metrics for one grep run. Deliberately omits the
/// pattern, path, and glob so query terms never reach `ToolOutput.metadata`,
/// the session log, or tracing.
#[derive(Debug)]
struct GrepMeta {
    mode: OutputMode,
    /// Match/occurrence count where the mode measures it; `None` for
    /// `files_with_matches`, which only scans for the first hit per file.
    matches: Option<usize>,
    files: usize,
    truncated: bool,
    binary_skipped: usize,
    unreadable_skipped: usize,
    /// Row offset for the next page when results were paged, else `None`.
    next_offset: Option<usize>,
}

impl GrepMeta {
    fn attach(self, output: super::ToolOutput) -> super::ToolOutput {
        let mut grep = serde_json::Map::new();
        grep.insert("mode".to_string(), json!(self.mode.label()));
        if let Some(matches) = self.matches {
            grep.insert("matches".to_string(), json!(matches));
        }
        grep.insert("files".to_string(), json!(self.files));
        grep.insert("truncated".to_string(), json!(self.truncated));
        if self.binary_skipped > 0 {
            grep.insert("binarySkipped".to_string(), json!(self.binary_skipped));
        }
        if self.unreadable_skipped > 0 {
            grep.insert(
                "unreadableSkipped".to_string(),
                json!(self.unreadable_skipped),
            );
        }
        if let Some(next) = self.next_offset {
            grep.insert("nextOffset".to_string(), json!(next));
        }
        output.with("grep", Value::Object(grep))
    }
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

fn grep(root: &Path, input: &GrepInput) -> Result<(String, GrepMeta)> {
    let search = input.path.as_deref().unwrap_or(".");
    let search_path = resolve_existing(root, search)?;
    let limit = positive_cap("limit", input.limit, DEFAULT_GREP_LIMIT, MAX_GREP_LIMIT)?;
    let context = input.context.unwrap_or(DEFAULT_CONTEXT).min(MAX_CONTEXT);
    let page = Page::from(input)?;

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(input.ignore_case)
        .fixed_strings(input.literal)
        .build(&input.pattern)
        .with_context(|| format!("invalid search pattern: {}", input.pattern))?;

    // Glob filter, applied via gitignore-style overrides (a positive glob means
    // "only files matching this"). Anchored at the search path.
    let overrides = build_overrides(&search_path, input.glob.as_deref())?;

    match input.output_mode {
        OutputMode::Content => grep_content(
            root,
            &search_path,
            &matcher,
            overrides,
            context,
            limit,
            page,
        ),
        OutputMode::FilesWithMatches => {
            grep_files(root, &search_path, &matcher, overrides, limit, page)
        }
        OutputMode::Count => grep_count(root, &search_path, &matcher, overrides, limit, page),
    }
}

fn grep_content(
    root: &Path,
    search_path: &Path,
    matcher: &RegexMatcher,
    overrides: Option<Override>,
    context: usize,
    limit: usize,
    page: Page,
) -> Result<(String, GrepMeta)> {
    let mut searcher = content_searcher(context);
    let mut files: Vec<FileHits> = Vec::new();
    let mut total_matches = 0usize;
    let mut truncated_matches = false;
    let mut skips = SkipStats::default();

    'walk: for entry in build_walk(search_path, overrides).build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                skips.unreadable += 1;
                continue;
            }
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let remaining = limit - total_matches;
        let mut sink = MatchSink::new(remaining);
        if searcher
            .search_path(matcher, entry.path(), &mut sink)
            .is_err()
        {
            skips.unreadable += 1;
            continue;
        }
        if sink.binary {
            skips.binary += 1;
            continue;
        }

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

    let binary_skipped = skips.binary;
    let unreadable_skipped = skips.unreadable;
    if files.is_empty() {
        return Ok((
            render_no_matches(skips),
            GrepMeta {
                mode: OutputMode::Content,
                matches: Some(0),
                files: 0,
                truncated: false,
                binary_skipped,
                unreadable_skipped,
                next_offset: None,
            },
        ));
    }

    let (text, next_offset) = render_content(&files, total_matches, truncated_matches, skips, page);
    Ok((
        text,
        GrepMeta {
            mode: OutputMode::Content,
            matches: Some(total_matches),
            files: files.len(),
            truncated: truncated_matches,
            binary_skipped,
            unreadable_skipped,
            next_offset,
        },
    ))
}

fn grep_files(
    root: &Path,
    search_path: &Path,
    matcher: &RegexMatcher,
    overrides: Option<Override>,
    limit: usize,
    page: Page,
) -> Result<(String, GrepMeta)> {
    let mut searcher = plain_searcher(false);
    let mut files = Vec::new();
    let mut truncated = false;
    let mut skips = SkipStats::default();

    for entry in build_walk(search_path, overrides).build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                skips.unreadable += 1;
                continue;
            }
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let mut sink = FirstMatchSink::default();
        if searcher
            .search_path(matcher, entry.path(), &mut sink)
            .is_err()
        {
            skips.unreadable += 1;
            continue;
        }
        if sink.binary {
            skips.binary += 1;
            continue;
        }
        if sink.matched {
            files.push(relative_display(root, entry.path()));
            if files.len() >= limit {
                truncated = true;
                break;
            }
        }
    }

    let binary_skipped = skips.binary;
    let unreadable_skipped = skips.unreadable;
    if files.is_empty() {
        return Ok((
            render_no_matches(skips),
            GrepMeta {
                mode: OutputMode::FilesWithMatches,
                matches: None,
                files: 0,
                truncated: false,
                binary_skipped,
                unreadable_skipped,
                next_offset: None,
            },
        ));
    }

    let (text, next_offset) = render_files(&files, truncated, skips, page);
    Ok((
        text,
        GrepMeta {
            mode: OutputMode::FilesWithMatches,
            matches: None,
            files: files.len(),
            truncated,
            binary_skipped,
            unreadable_skipped,
            next_offset,
        },
    ))
}

fn grep_count(
    root: &Path,
    search_path: &Path,
    matcher: &RegexMatcher,
    overrides: Option<Override>,
    limit: usize,
    page: Page,
) -> Result<(String, GrepMeta)> {
    let mut searcher = plain_searcher(false);
    let mut counts = Vec::new();
    let mut total_matches = 0usize;
    let mut truncated = false;
    let mut skips = SkipStats::default();

    for entry in build_walk(search_path, overrides).build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                skips.unreadable += 1;
                continue;
            }
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let mut sink = CountSink::new(limit - total_matches);
        if searcher
            .search_path(matcher, entry.path(), &mut sink)
            .is_err()
        {
            skips.unreadable += 1;
            continue;
        }
        if sink.binary {
            skips.binary += 1;
            continue;
        }
        if sink.count > 0 {
            total_matches += sink.count;
            counts.push((relative_display(root, entry.path()), sink.count));
        }
        if total_matches >= limit {
            truncated = true;
            break;
        }
    }

    let binary_skipped = skips.binary;
    let unreadable_skipped = skips.unreadable;
    if counts.is_empty() {
        return Ok((
            render_no_matches(skips),
            GrepMeta {
                mode: OutputMode::Count,
                matches: Some(0),
                files: 0,
                truncated: false,
                binary_skipped,
                unreadable_skipped,
                next_offset: None,
            },
        ));
    }

    let (text, next_offset) = render_count(&counts, total_matches, truncated, skips, page);
    Ok((
        text,
        GrepMeta {
            mode: OutputMode::Count,
            matches: Some(total_matches),
            files: counts.len(),
            truncated,
            binary_skipped,
            unreadable_skipped,
            next_offset,
        },
    ))
}

fn positive_cap(name: &str, value: Option<usize>, default: usize, max: usize) -> Result<usize> {
    if matches!(value, Some(0)) {
        bail!("`{name}` must be greater than 0");
    }
    Ok(value.unwrap_or(default).min(max))
}

fn build_overrides(search_path: &Path, glob: Option<&str>) -> Result<Option<Override>> {
    match glob {
        Some(glob) => {
            let mut builder = OverrideBuilder::new(search_path);
            builder
                .add(glob)
                .with_context(|| format!("invalid glob pattern: {glob}"))?;
            Ok(Some(
                builder.build().context("failed to build glob filter")?,
            ))
        }
        None => Ok(None),
    }
}

fn build_walk(search_path: &Path, overrides: Option<Override>) -> WalkBuilder {
    // `--hidden --no-require-git --no-follow` parity for agent search: include
    // dotfiles, apply .gitignore even outside git repos, and never chase links.
    let mut walk = WalkBuilder::new(search_path);
    walk.hidden(false)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_path(|a, b| a.cmp(b));
    if let Some(ov) = overrides {
        walk.overrides(ov);
    }
    walk
}

fn content_searcher(context: usize) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
        .line_number(true)
        .before_context(context)
        .after_context(context)
        .binary_detection(BinaryDetection::quit(b'\0'))
        .memory_map(MmapChoice::never());
    builder.build()
}

fn plain_searcher(line_number: bool) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
        .line_number(line_number)
        .binary_detection(BinaryDetection::quit(b'\0'))
        .memory_map(MmapChoice::never());
    builder.build()
}

#[derive(Debug, Clone, Copy)]
struct Page {
    offset: usize,
    head_limit: Option<usize>,
    explicit_head_limit: bool,
}

impl Page {
    fn from(input: &GrepInput) -> Result<Self> {
        if matches!(input.head_limit, Some(0)) {
            bail!("`headLimit` must be greater than 0");
        }
        let offset = input.offset.unwrap_or(0);
        if offset > MAX_OFFSET {
            bail!("`offset` must be at most {MAX_OFFSET}");
        }
        Ok(Self {
            offset,
            head_limit: Some(
                input
                    .head_limit
                    .unwrap_or(MAX_HEAD_LIMIT)
                    .min(MAX_HEAD_LIMIT),
            ),
            explicit_head_limit: input.head_limit.is_some(),
        })
    }

    fn window(self, len: usize) -> (usize, usize) {
        let start = self.offset.min(len);
        let end = match self.head_limit {
            Some(limit) => start.saturating_add(limit).min(len),
            None => len,
        };
        (start, end)
    }

    fn notice(self, len: usize, noun: &str) -> Option<String> {
        let (start, end) = self.window(len);
        if start == 0 && end == len {
            return None;
        }
        if start >= len {
            return Some(format!("showing 0 of {len} {noun}; offset is past the end"));
        }
        let next = if end < len {
            format!("; use offset={end} for next page")
        } else {
            String::new()
        };
        Some(format!("showing {}-{end} of {len} {noun}{next}", start + 1))
    }
}

#[derive(Default)]
struct SkipStats {
    binary: usize,
    unreadable: usize,
}

impl SkipStats {
    fn notices(self) -> Vec<String> {
        let mut notices = Vec::new();
        if self.binary > 0 {
            notices.push(format!(
                "skipped {} binary {}",
                self.binary,
                plural(self.binary, "file")
            ));
        }
        if self.unreadable > 0 {
            notices.push(format!(
                "skipped {} unreadable {}",
                self.unreadable,
                plural(self.unreadable, "file")
            ));
        }
        notices
    }
}

fn render_no_matches(skips: SkipStats) -> String {
    let mut out = "No matches found".to_string();
    append_notices(&mut out, skips.notices());
    out
}

fn render_content(
    files: &[FileHits],
    total_matches: usize,
    trunc_matches: bool,
    skips: SkipStats,
    page: Page,
) -> (String, Option<usize>) {
    let file_word = plural(files.len(), "file");
    let match_word = plural(total_matches, "match");
    let mut out = format!(
        "{total_matches} {match_word} in {} {file_word}\n",
        files.len()
    );

    let mut lines = Vec::new();
    for (file_idx, file) in files.iter().enumerate() {
        for (idx, line) in file.lines.iter().enumerate() {
            let marker = if line.is_match { "> " } else { "  " };
            lines.push((
                file_idx,
                format!("{marker}{}│ {}", line.number, clamp_line(&line.text)),
            ));
            if file.breaks.contains(&idx) {
                lines.push((file_idx, "  ⋯".to_string()));
            }
        }
    }

    let (start, end) = page.window(lines.len());
    let next_offset = (end < lines.len()).then_some(end);
    let mut last_file = None;
    if start < end {
        for (file_idx, line) in &lines[start..end] {
            if last_file != Some(*file_idx) {
                out.push('\n');
                out.push_str(&files[*file_idx].path);
                out.push('\n');
                last_file = Some(*file_idx);
            }
            out.push_str(line);
            out.push('\n');
        }
    }

    let mut notices = Vec::new();
    if let Some(notice) = page.notice(lines.len(), "rendered lines") {
        notices.push(notice);
    }
    if trunc_matches {
        notices.push(format!(
            "truncated: match limit of {total_matches} reached; refine your search"
        ));
    }
    notices.extend(skips.notices());
    append_notices(&mut out, notices);
    (truncate_output(out), next_offset)
}

fn render_files(
    files: &[String],
    truncated: bool,
    skips: SkipStats,
    page: Page,
) -> (String, Option<usize>) {
    let (start, end) = page.window(files.len());
    let next_offset = (end < files.len()).then_some(end);
    let mut out = format!("Found {} {}", files.len(), plural(files.len(), "file"));
    if page.explicit_head_limit {
        let limit = page.head_limit.expect("head limit is always set");
        out.push_str(&format!(" limit: {limit}"));
    }
    if page.offset > 0 {
        out.push_str(&format!(" offset: {}", page.offset));
    }
    out.push('\n');
    if start < end {
        out.push_str(&files[start..end].join("\n"));
        out.push('\n');
    }

    let mut notices = Vec::new();
    if let Some(notice) = page.notice(files.len(), "files") {
        notices.push(notice);
    }
    if truncated {
        notices.push(format!(
            "truncated: file limit of {} reached; refine your search",
            files.len()
        ));
    }
    notices.extend(skips.notices());
    append_notices(&mut out, notices);
    (truncate_output(out), next_offset)
}

fn render_count(
    counts: &[(String, usize)],
    total_matches: usize,
    truncated: bool,
    skips: SkipStats,
    page: Page,
) -> (String, Option<usize>) {
    let (start, end) = page.window(counts.len());
    let next_offset = (end < counts.len()).then_some(end);
    let mut out = format!(
        "Found {total_matches} {} across {} {}\n",
        plural(total_matches, "occurrence"),
        counts.len(),
        plural(counts.len(), "file")
    );
    for (path, count) in &counts[start..end] {
        out.push_str(path);
        out.push(':');
        out.push_str(&count.to_string());
        out.push('\n');
    }

    let mut notices = Vec::new();
    if let Some(notice) = page.notice(counts.len(), "files") {
        notices.push(notice);
    }
    if truncated {
        notices.push(format!(
            "truncated: match limit of {total_matches} reached; refine your search"
        ));
    }
    notices.extend(skips.notices());
    append_notices(&mut out, notices);
    (truncate_output(out), next_offset)
}

fn append_notices(out: &mut String, notices: Vec<String>) {
    if notices.is_empty() {
        return;
    }
    out.push_str("\n[");
    out.push_str(&notices.join(". "));
    out.push(']');
}

fn truncate_output(out: String) -> String {
    let (body, size_truncated, _) = truncate_head(&out, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut out = body;
    if size_truncated {
        out.push_str("\n\n[output truncated]");
    }
    out
}

fn plural(count: usize, singular: &str) -> &str {
    if count == 1 {
        singular
    } else {
        match singular {
            "occurrence" => "occurrences",
            "match" => "matches",
            "file" => "files",
            other => other,
        }
    }
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
    binary: bool,
}

impl MatchSink {
    fn new(remaining: usize) -> Self {
        Self {
            lines: Vec::new(),
            breaks: Vec::new(),
            match_count: 0,
            remaining,
            pending_break: false,
            binary: false,
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

    fn binary_data(
        &mut self,
        _searcher: &Searcher,
        _binary_byte_offset: u64,
    ) -> Result<bool, io::Error> {
        self.binary = true;
        Ok(false)
    }
}

#[derive(Default)]
struct FirstMatchSink {
    matched: bool,
    binary: bool,
}

impl Sink for FirstMatchSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, _mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.matched = true;
        Ok(false)
    }

    fn binary_data(
        &mut self,
        _searcher: &Searcher,
        _binary_byte_offset: u64,
    ) -> Result<bool, io::Error> {
        self.binary = true;
        Ok(false)
    }
}

struct CountSink {
    count: usize,
    remaining: usize,
    binary: bool,
}

impl CountSink {
    fn new(remaining: usize) -> Self {
        Self {
            count: 0,
            remaining,
            binary: false,
        }
    }
}

impl Sink for CountSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, _mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        self.count += 1;
        Ok(self.count < self.remaining)
    }

    fn binary_data(
        &mut self,
        _searcher: &Searcher,
        _binary_byte_offset: u64,
    ) -> Result<bool, io::Error> {
        self.binary = true;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::tools::test_support::{TestDir, root_of, temp_dir};

    /// A scratch workspace for the broader paging/bounds tests.
    struct GrepHarness {
        dir: TestDir,
        root: PathBuf,
    }

    impl GrepHarness {
        fn new() -> Self {
            let dir = temp_dir();
            let root = root_of(&dir);
            Self { dir, root }
        }

        fn write(&self, path: &str, content: impl AsRef<[u8]>) {
            let path = self.dir.path.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }

        fn native(&self, input: GrepInput) -> String {
            run(&self.root, input)
        }
    }

    fn run(root: &Path, input: GrepInput) -> String {
        grep(root, &input).unwrap().0
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
            output_mode: OutputMode::Content,
            head_limit: None,
            offset: None,
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
    fn files_with_matches_mode_lists_paths_and_pages() {
        let dir = temp_dir();
        let root = root_of(&dir);
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(dir.path.join(name), "needle\n").unwrap();
        }

        let mut files = input("needle");
        files.output_mode = OutputMode::FilesWithMatches;
        files.head_limit = Some(1);
        files.offset = Some(1);
        let out = run(&root, files);

        assert!(out.starts_with("Found 3 files"), "out: {out}");
        assert!(!out.contains("a.txt"), "out: {out}");
        assert!(out.contains("b.txt"), "out: {out}");
        assert!(!out.contains("c.txt"), "out: {out}");
        assert!(out.contains("showing 2-2 of 3 files"), "out: {out}");
    }

    #[test]
    fn count_mode_reports_occurrences_by_file() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("a.txt"), "needle\nneedle\n").unwrap();
        fs::write(dir.path.join("b.txt"), "needle\n").unwrap();

        let mut count = input("needle");
        count.output_mode = OutputMode::Count;
        let out = run(&root, count);

        assert!(
            out.starts_with("Found 3 occurrences across 2 files"),
            "out: {out}"
        );
        assert!(out.contains("a.txt:2"), "out: {out}");
        assert!(out.contains("b.txt:1"), "out: {out}");
    }

    #[test]
    fn content_mode_paginates_rendered_lines() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(
            dir.path.join("g.txt"),
            "needle one\nneedle two\nneedle three\n",
        )
        .unwrap();

        let mut paged = input("needle");
        paged.context = Some(0);
        paged.head_limit = Some(2);
        paged.offset = Some(1);
        let out = run(&root, paged);

        assert!(out.starts_with("3 matches in 1 file"), "out: {out}");
        assert!(!out.contains("needle one"), "out: {out}");
        assert!(out.contains("needle two"), "out: {out}");
        assert!(out.contains("showing 2-3"), "out: {out}");
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
    fn context_is_capped() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let body = (0..100)
            .map(|i| {
                if i == 50 {
                    "needle".to_string()
                } else {
                    format!("line {i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(dir.path.join("g.txt"), body).unwrap();
        let mut capped = input("needle");
        capped.context = Some(10_000);

        let out = run(&root, capped);
        let rendered_lines = out.lines().filter(|line| line.contains('│')).count();
        assert_eq!(rendered_lines, MAX_CONTEXT * 2 + 1, "out: {out}");
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

    #[test]
    fn binary_files_are_skipped_and_summarized() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("binary.bin"), b"needle\0hidden").unwrap();
        fs::write(dir.path.join("text.txt"), "needle\n").unwrap();

        let out = run(&root, input("needle"));

        assert!(out.contains("text.txt"), "out: {out}");
        assert!(!out.contains("binary.bin"), "out: {out}");
        assert!(out.contains("skipped 1 binary file"), "out: {out}");
    }

    #[test]
    fn rejects_paths_outside_workspace() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let outside = std::env::temp_dir();
        let mut escaped = input("needle");
        escaped.path = Some(outside.to_string_lossy().to_string());

        let err = grep(&root, &escaped).unwrap_err().to_string();

        assert!(err.contains("path escapes workspace"), "err: {err}");
    }

    #[test]
    fn pages_broad_file_results() {
        let h = GrepHarness::new();
        for name in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            h.write(name, "needle\n");
        }

        let mut first = input("needle");
        first.output_mode = OutputMode::FilesWithMatches;
        first.head_limit = Some(2);
        let first = h.native(first);
        assert!(first.contains("a.txt"), "out: {first}");
        assert!(first.contains("b.txt"), "out: {first}");
        assert!(!first.contains("c.txt"), "out: {first}");
        assert!(first.contains("use offset=2 for next page"), "out: {first}");

        let mut second = input("needle");
        second.output_mode = OutputMode::FilesWithMatches;
        second.head_limit = Some(2);
        second.offset = Some(2);
        let second = h.native(second);
        assert!(!second.contains("a.txt"), "out: {second}");
        assert!(second.contains("c.txt"), "out: {second}");
        assert!(second.contains("d.txt"), "out: {second}");
    }

    #[test]
    fn default_output_is_bounded_and_pageable() {
        let h = GrepHarness::new();
        let body = (0..650)
            .map(|i| format!("needle {i:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        h.write("huge.txt", body);

        let mut broad = input("needle");
        broad.context = Some(0);
        broad.limit = Some(650);
        let out = h.native(broad);

        assert!(out.lines().count() <= 510, "out was too large: {out}");
        assert!(out.contains("showing 1-500"), "out: {out}");
        assert!(out.contains("use offset=500 for next page"), "out: {out}");
        assert!(out.contains("needle 499"), "out: {out}");
        assert!(!out.contains("needle 500"), "out: {out}");
    }

    #[test]
    fn execute_attaches_structured_metrics_without_leaking_query() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("a.txt"), "needle\nneedle\n").unwrap();
        fs::write(dir.path.join("b.txt"), "needle\n").unwrap();

        let output = execute(&root, &json!({ "pattern": "needle" })).unwrap();
        let grep = output
            .metadata
            .get("grep")
            .expect("grep metadata is present");
        assert_eq!(grep["mode"], "content");
        assert_eq!(grep["matches"], 3);
        assert_eq!(grep["files"], 2);
        assert_eq!(grep["truncated"], false);

        // Structured telemetry must never carry the raw query terms.
        let meta = serde_json::to_string(&Value::Object(output.metadata)).unwrap();
        assert!(
            !meta.contains("needle"),
            "metadata leaked the pattern: {meta}"
        );
    }

    #[test]
    fn count_mode_metadata_reports_occurrences_and_files() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("a.txt"), "needle\nneedle\n").unwrap();
        fs::write(dir.path.join("b.txt"), "needle\n").unwrap();

        let output = execute(
            &root,
            &json!({ "pattern": "needle", "outputMode": "count" }),
        )
        .unwrap();
        let grep = output.metadata.get("grep").expect("grep metadata");
        assert_eq!(grep["mode"], "count");
        assert_eq!(grep["matches"], 3);
        assert_eq!(grep["files"], 2);
    }

    #[test]
    fn files_mode_metadata_omits_match_count() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("a.txt"), "needle\n").unwrap();

        let output = execute(
            &root,
            &json!({ "pattern": "needle", "outputMode": "files_with_matches" }),
        )
        .unwrap();
        let grep = output.metadata.get("grep").expect("grep metadata");
        assert_eq!(grep["mode"], "files_with_matches");
        assert_eq!(grep["files"], 1);
        // files mode does not count matches, so the field is omitted.
        assert!(
            grep.get("matches").is_none(),
            "files mode set matches: {grep}"
        );
    }
}
