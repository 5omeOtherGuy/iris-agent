//! Output-derived result summaries for tool panels.
//!
//! [`summarize_output`] is an opt-in seam that turns a tool's *output* into a
//! short, honest count string (`142 passed \u{00b7} 0 failed`, `3 matches \u{00b7} 2 files`)
//! for the SHELL result-row meta tail and the EXPLORE op-row count column.
//! Placement and separators stay per-family (the SHELL renderer draws a `\u{250a}`
//! tail; EXPLORE draws a right-aligned column with no `\u{250a}`); this module owns
//! only the *parsed* count string.
//!
//! Honesty boundary: every parser reads real command output and returns `None`
//! whenever it cannot confidently recognize the shape. It never fabricates a
//! count, and no value here is hardcoded per command -- unrecognized output
//! yields no summary and the row stays bare. This mirrors the design system's
//! rule: "Counts are real or omitted."
//!
//! Parsers are keyed by tool name; the `bash` tool sub-dispatches on the
//! program name of the first recognized command segment (cargo / git / rg).

use serde_json::Value;

use crate::nexus::ToolCall;

/// The interior separator between parts (` \u{00b7} `), matching the design system's
/// `142 passed \u{00b7} 0 failed`. The *family* separator (`\u{250a}` for SHELL) is applied
/// by the renderer, not here.
const PART_SEP: &str = " \u{00b7} ";

/// A parsed, output-derived result summary: an ordered list of honest count
/// parts (e.g. `["142 passed", "0 failed"]`). Rendering (placement, family
/// separator) is the caller's job; [`render`](ResultSummary::render) only joins
/// the parts with the shared `\u{00b7}` separator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResultSummary {
    parts: Vec<String>,
}

impl ResultSummary {
    /// Build a summary from parts, or `None` when there is nothing to show.
    /// Centralizing the empty check keeps every parser's "no confident count"
    /// path a plain `None`.
    fn new(parts: Vec<String>) -> Option<Self> {
        if parts.is_empty() {
            None
        } else {
            Some(Self { parts })
        }
    }

    /// The joined tail text, parts separated by ` \u{00b7} `.
    pub(crate) fn render(&self) -> String {
        self.parts.join(PART_SEP)
    }
}

/// Summarize a finished tool's output, or `None` when no matcher recognizes it.
///
/// `content` is the tool's authoritative output text (metadata is not on the
/// UI wire, so parsers read the text). `exit_code` is the process status when
/// known; matchers rely primarily on output shape, so it is advisory.
pub(crate) fn summarize_output(
    call: &ToolCall,
    content: &str,
    exit_code: Option<i32>,
) -> Option<ResultSummary> {
    match call.name.as_str() {
        "read" => summarize_read(content),
        "grep" => summarize_grep(content),
        "find" => summarize_find(content),
        "ls" => summarize_ls(content),
        "bash" => summarize_bash(call, content, exit_code),
        _ => None,
    }
}

// --- EXPLORE-family parsers (Iris tool output; stable, self-owned shapes) ----

/// `read`: count the line-numbered body lines (`  12\u{2192}...`). An empty file
/// reads as `0 lines`; non-empty output with no numbered lines is unrecognized
/// (`None`) rather than guessed.
fn summarize_read(content: &str) -> Option<ResultSummary> {
    if content.is_empty() {
        return ResultSummary::new(vec!["0 lines".to_string()]);
    }
    let count = content
        .lines()
        .filter(|line| is_numbered_line(line))
        .count();
    if count == 0 {
        return None;
    }
    ResultSummary::new(vec![count_label(count as u64, "line", "lines")])
}

/// A `read` body line: optional leading spaces, ASCII digits, then the `\u{2192}`
/// separator the read tool emits (`{:>width$}\u{2192}{line}`).
fn is_numbered_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    digits > 0 && trimmed[digits..].starts_with('\u{2192}')
}

/// `grep`: parse the tool's summary first line. Handles the default `content`
/// mode (`N matches in M files`), `files_with_matches` (`Found M files`), and
/// the empty result (`No matches found`). The `count` mode is left to `None`.
fn summarize_grep(content: &str) -> Option<ResultSummary> {
    let first = content.lines().next()?.trim();
    if first == "No matches found" {
        return ResultSummary::new(vec!["0 matches".to_string()]);
    }
    if let Some(summary) = parse_matches_in_files(first) {
        return Some(summary);
    }
    // `files_with_matches`: "Found M file(s)" (optionally trailed by paging).
    let rest = first.strip_prefix("Found ")?;
    let (m, after) = leading_u64(rest)?;
    if after.trim_start().starts_with("file") {
        return ResultSummary::new(vec![count_label(m, "file", "files")]);
    }
    None
}

/// `<n> match(es) in <m> file(s)` -> `["N matches", "M files"]`.
fn parse_matches_in_files(line: &str) -> Option<ResultSummary> {
    let (n, rest) = leading_u64(line)?;
    let rest = rest.strip_prefix(" match")?;
    let rest = rest.strip_prefix("es").unwrap_or(rest);
    let rest = rest.strip_prefix(" in ")?;
    let (m, rest) = leading_u64(rest)?;
    if !rest.trim_start().starts_with("file") {
        return None;
    }
    ResultSummary::new(vec![
        count_label(n, "match", "matches"),
        count_label(m, "file", "files"),
    ])
}

/// `find`: newline-separated paths. Empty result is `0 matches`; a truncated
/// listing has no honest total, so it is omitted (`None`).
fn summarize_find(content: &str) -> Option<ResultSummary> {
    if content.trim() == "No files found matching pattern" {
        return ResultSummary::new(vec!["0 matches".to_string()]);
    }
    if content.contains("[output truncated]") {
        return None;
    }
    let count = nonblank_line_count(content);
    if count == 0 {
        return None;
    }
    ResultSummary::new(vec![count_label(count, "match", "matches")])
}

/// `ls`: one entry per line. An empty directory is `0 entries`; a truncated
/// listing is omitted (`None`).
fn summarize_ls(content: &str) -> Option<ResultSummary> {
    if content.trim() == "(empty directory)" {
        return ResultSummary::new(vec!["0 entries".to_string()]);
    }
    if content.contains("[output truncated]") {
        return None;
    }
    let count = nonblank_line_count(content);
    if count == 0 {
        return None;
    }
    ResultSummary::new(vec![count_label(count, "entry", "entries")])
}

// --- SHELL-family (bash) parsers, sub-dispatched on the program name ---------

/// `bash`: locate the first recognized command segment and run its matcher on
/// the merged output. Segments are split on shell control operators so a
/// `cd sub && cargo test` still resolves to `cargo`. The matcher's own shape
/// check is the real guard: an unrecognized output shape returns `None`.
fn summarize_bash(call: &ToolCall, content: &str, exit_code: Option<i32>) -> Option<ResultSummary> {
    let command = call.arguments.get("command").and_then(Value::as_str)?;
    for tokens in command_segments(command) {
        let argv = strip_command_prefixes(&tokens);
        let Some((program, rest)) = argv.split_first() else {
            continue;
        };
        let program = program_name(program);
        let sub = rest.first().copied();
        let summary = match (program, sub) {
            ("cargo", Some("test")) => cargo_test(content),
            ("cargo", Some("build" | "check" | "clippy" | "run")) => {
                cargo_build(content, exit_code)
            }
            ("git", Some("status")) => git_status(content),
            ("rg", _) => rg_content(content),
            _ => None,
        };
        if summary.is_some() {
            return summary;
        }
    }
    None
}

/// `cargo test`: sum `test result: ...` lines (one per test binary plus
/// doctests) into `X passed \u{00b7} Y failed`. No such line -> `None`.
fn cargo_test(content: &str) -> Option<ResultSummary> {
    let mut passed = 0u64;
    let mut failed = 0u64;
    let mut found = false;
    for line in content.lines() {
        if line.trim_start().starts_with("test result:") {
            found = true;
            passed += count_before(line, "passed").unwrap_or(0);
            failed += count_before(line, "failed").unwrap_or(0);
        }
    }
    if !found {
        return None;
    }
    ResultSummary::new(vec![format!("{passed} passed"), format!("{failed} failed")])
}

/// `cargo build`/`check`/`clippy`/`run`: only the *failure* case has an honest
/// output count -- `error: ... due to N previous error(s)` -> `N errors \u{00b7}
/// compile failed`. Gated on a non-zero exit status so a successful run whose
/// output merely contains the phrase "due to ... errors" is never mislabeled a
/// compile failure; a successful or unreported build yields no count.
fn cargo_build(content: &str, exit_code: Option<i32>) -> Option<ResultSummary> {
    if !matches!(exit_code, Some(code) if code != 0) {
        return None;
    }
    let count = content.lines().find_map(|line| {
        let idx = line.find("due to ")?;
        let rest = &line[idx + "due to ".len()..];
        leading_u64(rest).map(|(n, _)| n)
    })?;
    ResultSummary::new(vec![
        count_label(count, "error", "errors"),
        "compile failed".to_string(),
    ])
}

/// `git status`: parse the short/porcelain form (`XY path`) into per-category
/// counts. The default long, prose form does not match the two-column code
/// shape and yields `None`.
fn git_status(content: &str) -> Option<ResultSummary> {
    let mut modified = 0u64;
    let mut added = 0u64;
    let mut deleted = 0u64;
    let mut renamed = 0u64;
    let mut untracked = 0u64;
    let mut rows = 0u64;
    for line in content.lines() {
        if line.trim().is_empty() || line.starts_with('[') {
            continue; // trailing host notices, blank separators
        }
        let bytes = line.as_bytes();
        if bytes.len() < 3 || bytes[2] != b' ' {
            return None;
        }
        let x = bytes[0] as char;
        let y = bytes[1] as char;
        if !is_porcelain_code(x) || !is_porcelain_code(y) {
            return None;
        }
        rows += 1;
        if x == '?' && y == '?' {
            untracked += 1;
        } else if x == 'R' || y == 'R' {
            renamed += 1;
        } else if x == 'A' || y == 'A' {
            added += 1;
        } else if x == 'D' || y == 'D' {
            deleted += 1;
        } else if x == 'M' || y == 'M' || x == 'T' || y == 'T' {
            modified += 1;
        }
    }
    if rows == 0 {
        return None;
    }
    let mut parts = Vec::new();
    push_count(&mut parts, modified, "modified");
    push_count(&mut parts, added, "added");
    push_count(&mut parts, deleted, "deleted");
    push_count(&mut parts, renamed, "renamed");
    push_count(&mut parts, untracked, "untracked");
    ResultSummary::new(parts)
}

fn is_porcelain_code(c: char) -> bool {
    matches!(c, ' ' | 'M' | 'A' | 'D' | 'R' | 'C' | 'U' | 'T' | '?' | '!')
}

/// `rg` (default, non-TTY): `path:line:text` per match. Strict -- every
/// non-blank line must match, otherwise the shape is unknown (`-l`, `-c`,
/// `--stats`, context) and the result is `None`.
fn rg_content(content: &str) -> Option<ResultSummary> {
    let mut files = std::collections::BTreeSet::new();
    let mut matches = 0u64;
    for line in content.lines().filter(|line| !line.is_empty()) {
        let (path, rest) = line.split_once(':')?;
        let (number, _) = rest.split_once(':')?;
        if path.is_empty() || number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        files.insert(path);
        matches += 1;
    }
    if matches == 0 {
        return None;
    }
    ResultSummary::new(vec![
        count_label(matches, "match", "matches"),
        count_label(files.len() as u64, "file", "files"),
    ])
}

// --- Shared helpers ---------------------------------------------------------

/// Split a bash command string into candidate argv token lists, one per
/// segment. Segments are cut on newlines and the shell control operators
/// `&&`, `||`, `|`, `;`, `&`. Quote-awareness is intentionally omitted: this
/// only locates a leading program name, and a mis-split segment simply fails
/// its matcher's shape check.
fn command_segments(command: &str) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    for line in command.split('\n') {
        let normalized = line.replace("&&", "\n").replace("||", "\n");
        for segment in normalized.split(['\n', '|', ';', '&']) {
            let tokens: Vec<String> = segment.split_whitespace().map(String::from).collect();
            if !tokens.is_empty() {
                segments.push(tokens);
            }
        }
    }
    segments
}

/// Drop leading env-var assignments and common command wrappers so the program
/// name is the first meaningful token (`FOO=1 sudo cargo test` -> `cargo ...`).
fn strip_command_prefixes(tokens: &[String]) -> Vec<&str> {
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i].as_str();
        if matches!(token, "sudo" | "env" | "command" | "time" | "nice") || is_env_assignment(token)
        {
            i += 1;
        } else {
            break;
        }
    }
    tokens[i..].iter().map(String::as_str).collect()
}

fn is_env_assignment(token: &str) -> bool {
    match token.find('=') {
        Some(pos) if pos > 0 => token[..pos]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        _ => false,
    }
}

/// The program name from a token, path-stripped (`/usr/bin/cargo` -> `cargo`).
fn program_name(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// Parse leading ASCII digits as a `u64`, returning the value and the rest.
fn leading_u64(text: &str) -> Option<(u64, &str)> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let value = text[..digits].parse().ok()?;
    Some((value, &text[digits..]))
}

/// The numeric value immediately preceding `word` (`142 passed` -> `142`),
/// splitting on whitespace and `;`/`,` so `142 passed; 0 failed` parses cleanly.
fn count_before(text: &str, word: &str) -> Option<u64> {
    let tokens: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c == ';' || c == ',')
        .filter(|token| !token.is_empty())
        .collect();
    tokens
        .iter()
        .position(|token| *token == word)
        .filter(|&pos| pos > 0)
        .and_then(|pos| tokens[pos - 1].parse().ok())
}

fn nonblank_line_count(content: &str) -> u64 {
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count() as u64
}

fn count_label(count: u64, singular: &str, plural: &str) -> String {
    let noun = if count == 1 { singular } else { plural };
    format!("{count} {noun}")
}

fn push_count(parts: &mut Vec<String>, count: u64, label: &str) {
    if count > 0 {
        parts.push(format!("{count} {label}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(command: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
            name: "bash".to_string(),
            arguments: json!({ "command": command }),
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            thought_signature: None,
            name: name.to_string(),
            arguments: json!({}),
        }
    }

    fn rendered(call: &ToolCall, content: &str, exit_code: Option<i32>) -> Option<String> {
        summarize_output(call, content, exit_code).map(|summary| summary.render())
    }

    #[test]
    fn read_counts_numbered_lines() {
        let content = "  1\u{2192}fn main() {\n  2\u{2192}    ok\n  3\u{2192}}";
        assert_eq!(
            rendered(&call("read"), content, None).as_deref(),
            Some("3 lines")
        );
    }

    #[test]
    fn read_singular_line() {
        let content = "1\u{2192}only";
        assert_eq!(
            rendered(&call("read"), content, None).as_deref(),
            Some("1 line")
        );
    }

    #[test]
    fn read_empty_file_is_zero_lines() {
        assert_eq!(
            rendered(&call("read"), "", None).as_deref(),
            Some("0 lines")
        );
    }

    #[test]
    fn read_ignores_trailing_notice() {
        let content =
            "  1\u{2192}a\n  2\u{2192}b\n\n[3 more lines in file. Use offset=3 to continue.]";
        assert_eq!(
            rendered(&call("read"), content, None).as_deref(),
            Some("2 lines")
        );
    }

    #[test]
    fn read_unrecognized_output_is_none() {
        assert_eq!(rendered(&call("read"), "not a numbered file", None), None);
    }

    #[test]
    fn grep_content_mode() {
        assert_eq!(
            rendered(
                &call("grep"),
                "3 matches in 2 files\n\nsrc/a.rs\n> 1\u{2502} x",
                None
            )
            .as_deref(),
            Some("3 matches \u{00b7} 2 files"),
        );
    }

    #[test]
    fn grep_singular_match_and_file() {
        assert_eq!(
            rendered(&call("grep"), "1 match in 1 file\n\nsrc/a.rs", None).as_deref(),
            Some("1 match \u{00b7} 1 file"),
        );
    }

    #[test]
    fn grep_files_with_matches_mode() {
        assert_eq!(
            rendered(&call("grep"), "Found 4 files\nsrc/a.rs\nsrc/b.rs", None).as_deref(),
            Some("4 files"),
        );
    }

    #[test]
    fn grep_no_matches() {
        assert_eq!(
            rendered(&call("grep"), "No matches found", None).as_deref(),
            Some("0 matches")
        );
    }

    #[test]
    fn grep_count_mode_is_none() {
        assert_eq!(
            rendered(
                &call("grep"),
                "Found 7 occurrences across 2 files\nsrc/a.rs:5",
                None
            ),
            None
        );
    }

    #[test]
    fn find_counts_paths() {
        assert_eq!(
            rendered(&call("find"), "src/a.rs\nsrc/b.rs\nsrc/c.rs", None).as_deref(),
            Some("3 matches"),
        );
    }

    #[test]
    fn find_no_results_is_zero() {
        assert_eq!(
            rendered(&call("find"), "No files found matching pattern", None).as_deref(),
            Some("0 matches"),
        );
    }

    #[test]
    fn find_truncated_is_none() {
        assert_eq!(
            rendered(
                &call("find"),
                "src/a.rs\nsrc/b.rs\n\n[output truncated]",
                None
            ),
            None
        );
    }

    #[test]
    fn ls_counts_entries() {
        assert_eq!(
            rendered(&call("ls"), "- 12 B  a.rs\n- 40 B  b.rs", None).as_deref(),
            Some("2 entries"),
        );
    }

    #[test]
    fn ls_empty_directory_is_zero() {
        assert_eq!(
            rendered(&call("ls"), "(empty directory)", None).as_deref(),
            Some("0 entries")
        );
    }

    #[test]
    fn ls_truncated_is_none() {
        assert_eq!(
            rendered(&call("ls"), "a.rs\nb.rs\n\n[output truncated]", None),
            None
        );
    }

    #[test]
    fn cargo_test_sums_result_lines() {
        let content = "running 3 tests\ntest result: ok. 142 passed; 0 failed; 0 ignored\n\
             test result: ok. 8 passed; 0 failed; 0 ignored";
        assert_eq!(
            rendered(&bash("cargo test"), content, Some(0)).as_deref(),
            Some("150 passed \u{00b7} 0 failed"),
        );
    }

    #[test]
    fn cargo_test_reports_failures() {
        let content = "test result: FAILED. 140 passed; 2 failed; 0 ignored";
        assert_eq!(
            rendered(&bash("cargo test context::emit"), content, Some(101)).as_deref(),
            Some("140 passed \u{00b7} 2 failed"),
        );
    }

    #[test]
    fn cargo_test_without_result_line_is_none() {
        assert_eq!(
            rendered(&bash("cargo test"), "compiling...\n", Some(0)),
            None
        );
    }

    #[test]
    fn cargo_build_failure_reports_error_count() {
        let content = "error[E0382]: borrow of moved value\n\
             error: aborting due to 3 previous errors\n";
        assert_eq!(
            rendered(&bash("cargo build --release"), content, Some(101)).as_deref(),
            Some("3 errors \u{00b7} compile failed"),
        );
    }

    #[test]
    fn cargo_build_single_error_singular() {
        let content = "error: could not compile `iris` due to 1 previous error";
        assert_eq!(
            rendered(&bash("cargo check"), content, Some(101)).as_deref(),
            Some("1 error \u{00b7} compile failed"),
        );
    }

    #[test]
    fn cargo_build_success_is_none() {
        assert_eq!(
            rendered(
                &bash("cargo build"),
                "   Compiling iris\n    Finished dev\n",
                Some(0)
            ),
            None,
        );
    }

    #[test]
    fn cargo_run_success_with_error_phrase_in_output_is_none() {
        // A successful `cargo run` whose PROGRAM output happens to contain the
        // failure phrase must not be mislabeled a compile failure (exit 0).
        let content = "    Finished dev\n     Running `target/debug/app`\n\
             giving up due to 2 previous errors from the parser";
        assert_eq!(rendered(&bash("cargo run"), content, Some(0)), None);
    }

    #[test]
    fn git_status_porcelain_counts_categories() {
        let content = " M src/a.rs\n M src/b.rs\nMM src/c.rs\nA  src/d.rs\n?? new.rs\n?? other.rs";
        assert_eq!(
            rendered(&bash("git status --porcelain"), content, Some(0)).as_deref(),
            Some("3 modified \u{00b7} 1 added \u{00b7} 2 untracked"),
        );
    }

    #[test]
    fn git_status_long_form_is_none() {
        let content = "On branch main\nChanges not staged for commit:\n\tmodified:   src/a.rs";
        assert_eq!(rendered(&bash("git status"), content, Some(0)), None);
    }

    #[test]
    fn rg_default_content_counts_matches_and_files() {
        let content = "src/a.rs:12:let x = 1;\nsrc/a.rs:40:let y = 2;\nsrc/b.rs:3:let z = 3;";
        assert_eq!(
            rendered(&bash("rg 'let'"), content, Some(0)).as_deref(),
            Some("3 matches \u{00b7} 2 files"),
        );
    }

    #[test]
    fn rg_files_list_shape_is_none() {
        // `rg -l` emits bare paths (no `:line:`), which must not be summarized.
        assert_eq!(
            rendered(&bash("rg -l needle"), "src/a.rs\nsrc/b.rs", Some(0)),
            None
        );
    }

    #[test]
    fn bash_resolves_command_after_cd_and_assignments() {
        let content = "test result: ok. 5 passed; 0 failed; 0 ignored";
        assert_eq!(
            rendered(
                &bash("cd crates/core && RUST_LOG=info cargo test"),
                content,
                Some(0)
            )
            .as_deref(),
            Some("5 passed \u{00b7} 0 failed"),
        );
    }

    #[test]
    fn bash_unrecognized_command_is_none() {
        assert_eq!(rendered(&bash("echo hello"), "hello", Some(0)), None);
    }

    #[test]
    fn unknown_tool_is_none() {
        assert_eq!(rendered(&call("write"), "anything", None), None);
    }
}
