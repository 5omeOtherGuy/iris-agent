//! Shared, presentation-only formatter for tool-call display.
//!
//! Boundary (AGENTS.md): this is the Iris CLI presentation layer. Every function
//! returns an owned `String`, never colors, and never alters what is sent to the
//! model. The read-only vs mutating *policy* lives in [`crate::tools`]; only
//! display verb/path matching lives here. Color and layout (frames, glyphs) live
//! one layer up in [`crate::ui::text`]. The one IO exception is [`display_path`],
//! which reads the working directory to render tool paths consistently.
//!
//! The text front-end calls these helpers after Nexus emits semantic UI events.

use std::path::Path;

use serde_json::Value;

use crate::nexus::ToolCall;

// Display caps for folded tool output bodies; presentation-only, never affect
// the model (the full output still flows to the provider independently).
const MAX_DISPLAY_LINES: usize = 12;
const MAX_DISPLAY_CHARS: usize = 2000;
// Cap for one-line command/arg summaries.
const MAX_SUMMARY_CHARS: usize = 100;

/// A long tool-output body folded to a bounded preview. `hidden_lines` is the
/// number of trailing source lines omitted from `preview`; zero means the whole
/// body is shown. The caller renders the "(+N more lines)" affordance so the
/// indicator can be colored without baking ANSI into this layer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Folded {
    pub(crate) preview: String,
    pub(crate) hidden_lines: usize,
}

/// Core shared piece: a one-line, per-tool summary of a proposed call.
///
/// Reused by the proposed line, the approval prompt, and the denied line so the
/// summary is single-sourced (Codex parallel: `exec_snippet`).
pub(crate) fn summarize(call: &ToolCall) -> String {
    match call.name.as_str() {
        "read" | "write" | "edit" => file_summary(call),
        "bash" => bash_summary(call),
        "grep" => grep_summary(call),
        "find" => find_summary(call),
        "ls" => ls_summary(call),
        _ => fallback_summary(call),
    }
}

/// What the terminal should say a shell-like tool ran. For `bash`, Codex shows
/// the command itself (`npm install`), not the transport tool name (`bash npm
/// install`). Non-shell tools keep the ordinary one-line summary.
pub(crate) fn run_target(call: &ToolCall) -> String {
    if call.name != "bash" {
        return summarize(call);
    }
    let Some(command) = call.arguments.get("command").and_then(Value::as_str) else {
        return fallback_summary(call);
    };
    let (line, hidden) = bash_display_line(command);
    let mut target = truncate_inline(line, MAX_SUMMARY_CHARS);
    if hidden > 0 {
        let plural = if hidden == 1 { "" } else { "s" };
        target.push_str(&format!(" (+{hidden} more line{plural})"));
    }
    match call.arguments.get("timeout").and_then(Value::as_u64) {
        Some(0) => target.push_str(" (no timeout)"),
        Some(n) => target.push_str(&format!(" (timeout {n}s)")),
        None => {}
    }
    target
}

pub(crate) fn is_exploration_tool(call: &ToolCall) -> bool {
    matches!(call.name.as_str(), "read" | "grep" | "find" | "ls")
}

pub(crate) fn exploration_summary(call: &ToolCall) -> String {
    let summary = summarize(call);
    match call.name.as_str() {
        "read" => summary
            .strip_prefix("read ")
            .map_or_else(|| summary.clone(), |rest| format!("Read {rest}")),
        "grep" => summary
            .strip_prefix("grep ")
            .map_or_else(|| summary.clone(), |rest| format!("Search {rest}")),
        "find" => summary
            .strip_prefix("find ")
            .map_or_else(|| summary.clone(), |rest| format!("Find {rest}")),
        "ls" => summary
            .strip_prefix("ls ")
            .map_or_else(|| summary.clone(), |rest| format!("List {rest}")),
        _ => summary,
    }
}

/// Fold a tool-output body to a bounded preview plus a hidden-line count.
///
/// Line-bounded first (keep at most [`MAX_DISPLAY_LINES`] source lines), then a
/// char cap as a second guard against a single huge line. The full output is
/// never altered for the model; this only governs what the terminal shows.
pub(crate) fn fold(content: &str) -> Folded {
    let total = content.lines().count();
    let mut preview = String::new();
    let mut shown = 0;
    for line in content.lines().take(MAX_DISPLAY_LINES) {
        if shown > 0 {
            preview.push('\n');
        }
        preview.push_str(line);
        shown += 1;
    }
    let mut hidden = total.saturating_sub(shown);

    if preview.chars().count() > MAX_DISPLAY_CHARS {
        preview = preview.chars().take(MAX_DISPLAY_CHARS).collect();
        // A char-capped preview may cut mid-line, so anything not fully shown is
        // hidden; report at least one hidden line so the affordance appears.
        hidden = hidden.max(1);
    }

    Folded {
        preview,
        hidden_lines: hidden,
    }
}

/// Normalize a tool path for display so `write`/`edit`/`read` render the same
/// way regardless of whether the model passed a relative or an absolute path
/// (`edit`'s schema asks for an absolute path, the others relative). An
/// absolute path under the working directory becomes workspace-relative; a
/// leading `./` is trimmed. Presentation-only; the model still sees the raw
/// path it sent.
///
/// ponytail: anchored on `current_dir()` because Iris always runs with
/// workspace == cwd (see `main::run_agent`). If that ever diverges, thread the
/// real workspace root through instead.
fn display_path(raw: &str) -> String {
    let path = Path::new(raw);
    if path.is_absolute()
        && let Ok(cwd) = std::env::current_dir()
        && let Ok(rel) = path.strip_prefix(&cwd)
    {
        return rel.to_string_lossy().into_owned();
    }
    raw.strip_prefix("./").unwrap_or(raw).to_string()
}

/// File tools: `"{name} {path}"`. Falls back to a redacted compact-arg summary if
/// `path` is absent or non-string, so malformed calls never echo a large or
/// sensitive `content` field.
fn file_summary(call: &ToolCall) -> String {
    // `edit` uses Claude's `file_path`; `read`/`write` use `path`.
    let path = call
        .arguments
        .get("file_path")
        .or_else(|| call.arguments.get("path"))
        .and_then(Value::as_str);
    match path {
        Some(path) => format!("{} {}", call.name, display_path(path)),
        None => redacted_fallback(call),
    }
}

/// Bash: `"bash {cmd}"` where `cmd` is the first *meaningful* line of the script.
/// Shell setup lines (`set ...`, comments/shebangs, a bare `cd ...`) are skipped
/// so an approval never shows a no-op prefix like `set -e` in place of the real
/// command. When the script has more than one non-blank line, a `(+N more lines)`
/// hint is appended so the reviewer knows code is hidden. Truncated to
/// `MAX_SUMMARY_CHARS`. Appends the timeout only when explicitly provided:
/// `Some(0)` -> ` (no timeout)`, `Some(n)` -> ` (timeout {n}s)`, `None` -> nothing.
/// cwd is omitted (bash has no `cwd` arg and runs at the workspace root); the slot
/// is documented for a future `cwd` arg as `" (cwd {rel})"`.
fn bash_summary(call: &ToolCall) -> String {
    let command = call.arguments.get("command").and_then(Value::as_str);
    let Some(command) = command else {
        return fallback_summary(call);
    };
    let (line, hidden) = bash_display_line(command);
    let mut summary = format!("bash {}", truncate_inline(line, MAX_SUMMARY_CHARS));
    if hidden > 0 {
        let plural = if hidden == 1 { "" } else { "s" };
        summary.push_str(&format!(" (+{hidden} more line{plural})"));
    }
    match call.arguments.get("timeout").and_then(Value::as_u64) {
        Some(0) => summary.push_str(" (no timeout)"),
        Some(n) => summary.push_str(&format!(" (timeout {n}s)")),
        None => {}
    }
    summary
}

/// Choose the first meaningful (already-trimmed) line of a bash script and count
/// the other non-blank lines hidden from the one-line summary. Skipping shell
/// setup keeps the approval honest: the reviewer sees the command that does the
/// work, not a leading `set -e`/comment/`cd`.
fn bash_display_line(command: &str) -> (&str, usize) {
    let nonblank: Vec<&str> = command
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let line = nonblank
        .iter()
        .copied()
        .find(|line| !is_bash_setup_line(line))
        .or_else(|| nonblank.first().copied())
        .unwrap_or("");
    (line, nonblank.len().saturating_sub(1))
}

/// Shell setup that should not stand in for the real command in a summary:
/// comments/shebangs, `set ...` option lines, and a standalone `cd ...` that does
/// not chain another command.
fn is_bash_setup_line(line: &str) -> bool {
    line.starts_with('#')
        || line == "set"
        || line.starts_with("set ")
        || (line.starts_with("cd ")
            && !line.contains("&&")
            && !line.contains(';')
            && !line.contains('|'))
}

fn grep_summary(call: &ToolCall) -> String {
    let pattern = call
        .arguments
        .get("pattern")
        .and_then(Value::as_str)
        .unwrap_or("<missing pattern>");
    let path = call
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(".");
    let glob = call
        .arguments
        .get("glob")
        .and_then(Value::as_str)
        .map(|glob| format!(" ({glob})"))
        .unwrap_or_default();
    format!(
        "grep {} in {}{}",
        truncate_inline(pattern, MAX_SUMMARY_CHARS),
        display_path(path),
        glob
    )
}

fn find_summary(call: &ToolCall) -> String {
    let pattern = call
        .arguments
        .get("pattern")
        .and_then(Value::as_str)
        .unwrap_or("<missing pattern>");
    let path = call
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(".");
    format!(
        "find {} in {}",
        truncate_inline(pattern, MAX_SUMMARY_CHARS),
        display_path(path)
    )
}

fn ls_summary(call: &ToolCall) -> String {
    let path = call
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(".");
    format!("ls {}", display_path(path))
}

/// Fallback for unknown tools: `"{name} {compact_args}"`, the args serialized
/// to one line and capped.
fn fallback_summary(call: &ToolCall) -> String {
    let compact = serde_json::to_string(&call.arguments).unwrap_or_default();
    format!(
        "{} {}",
        call.name,
        truncate_inline(&compact, MAX_SUMMARY_CHARS)
    )
}

/// Like [`fallback_summary`] but drops a `content` field before serializing, so an
/// approval prompt for a malformed mutating file tool cannot splash file contents
/// inline even before the char cap applies.
fn redacted_fallback(call: &ToolCall) -> String {
    let redacted = match &call.arguments {
        Value::Object(map) => {
            let mut map = map.clone();
            map.remove("content");
            Value::Object(map)
        }
        other => other.clone(),
    };
    let compact = serde_json::to_string(&redacted).unwrap_or_default();
    format!(
        "{} {}",
        call.name,
        truncate_inline(&compact, MAX_SUMMARY_CHARS)
    )
}

/// Single-line char truncation with a trailing ellipsis on a char boundary.
/// `truncate_body` is line-oriented and appends a newline marker, which is wrong
/// for an inline summary; this is the inline analog.
fn truncate_inline(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments,
        }
    }

    #[test]
    fn display_path_makes_write_and_edit_paths_consistent() {
        // The model passes `edit` an absolute path and `write` a relative one;
        // both must render the same workspace-relative path.
        let cwd = std::env::current_dir().unwrap();
        let abs = cwd.join("tmp_x/subdir/sample.txt");
        let edit = summarize(&call(
            "edit",
            json!({ "file_path": abs.to_string_lossy(), "old_string": "a", "new_string": "b" }),
        ));
        let write = summarize(&call(
            "write",
            json!({ "path": "./tmp_x/subdir/sample.txt", "content": "hi" }),
        ));
        assert_eq!(edit, "edit tmp_x/subdir/sample.txt");
        assert_eq!(write, "write tmp_x/subdir/sample.txt");
    }

    #[test]
    fn summarize_file_tools_use_path() {
        assert_eq!(
            summarize(&call("read", json!({ "path": "a.rs" }))),
            "read a.rs"
        );
        assert_eq!(
            summarize(&call(
                "edit",
                json!({ "file_path": "src/x.rs", "old_string": "a", "new_string": "b" })
            )),
            "edit src/x.rs"
        );
        assert_eq!(
            summarize(&call(
                "write",
                json!({ "path": "out.txt", "content": "hi" })
            )),
            "write out.txt"
        );
    }

    #[test]
    fn summarize_file_tool_missing_path_redacts_content() {
        let big = "x".repeat(5000);
        let summary = summarize(&call("write", json!({ "content": big.clone() })));
        assert!(summary.starts_with("write "));
        assert!(!summary.contains(&big), "must not echo full content");
        assert!(
            !summary.contains("xxxxxxxxxx"),
            "content dropped before serialize"
        );
        assert!(summary.chars().count() <= "write ".len() + MAX_SUMMARY_CHARS + 1);
    }

    #[test]
    fn summarize_bash_command_and_timeout() {
        assert_eq!(
            summarize(&call("bash", json!({ "command": "echo hi" }))),
            "bash echo hi"
        );
        assert_eq!(
            summarize(&call("bash", json!({ "command": "echo hi", "timeout": 5 }))),
            "bash echo hi (timeout 5s)"
        );
        assert_eq!(
            summarize(&call("bash", json!({ "command": "echo hi", "timeout": 0 }))),
            "bash echo hi (no timeout)"
        );
    }

    #[test]
    fn run_target_omits_bash_transport_prefix() {
        assert_eq!(
            run_target(&call(
                "bash",
                json!({ "command": "set -e\nnpm install", "timeout": 5 })
            )),
            "npm install (+1 more line) (timeout 5s)"
        );
        assert_eq!(
            exploration_summary(&call("grep", json!({ "pattern": "needle", "path": "src" }))),
            "Search needle in src"
        );
    }

    #[test]
    fn summarize_bash_long_multiline_command_is_single_truncated_line() {
        let command = format!("first {}\nsecond line", "a".repeat(200));
        let summary = summarize(&call("bash", json!({ "command": command })));
        assert!(summary.starts_with("bash first "));
        assert!(!summary.contains('\n'));
        assert!(!summary.contains("second line"));
        // First line is truncated (ellipsis) and the hidden second line is counted.
        assert!(summary.contains('\u{2026}'));
        assert!(summary.ends_with("(+1 more line)"));
    }

    #[test]
    fn summarize_bash_skips_setup_line_and_counts_hidden() {
        let command = "set -e\nmkdir -p out\npwd";
        let summary = summarize(&call("bash", json!({ "command": command, "timeout": 120 })));
        assert_eq!(summary, "bash mkdir -p out (+2 more lines) (timeout 120s)");
    }

    #[test]
    fn summarize_bash_standalone_cd_is_skipped() {
        let summary = summarize(&call("bash", json!({ "command": "cd src\ncargo build" })));
        assert_eq!(summary, "bash cargo build (+1 more line)");
    }

    #[test]
    fn summarize_search_tools_without_raw_json() {
        assert_eq!(
            summarize(&call(
                "grep",
                json!({ "pattern": "delta", "path": "tmp", "glob": "*.txt" })
            )),
            "grep delta in tmp (*.txt)"
        );
        assert_eq!(
            summarize(&call("find", json!({ "pattern": "*.rs", "path": "src" }))),
            "find *.rs in src"
        );
        assert_eq!(summarize(&call("ls", json!({ "path": "src" }))), "ls src");
        assert!(!summarize(&call("grep", json!({ "pattern": "x" }))).contains('{'));
    }

    #[test]
    fn fold_keeps_short_output_whole() {
        let folded = fold("line a\nline b");
        assert_eq!(folded.preview, "line a\nline b");
        assert_eq!(folded.hidden_lines, 0);
    }

    #[test]
    fn fold_bounds_long_output_and_counts_hidden_lines() {
        let text = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let folded = fold(&text);
        assert_eq!(folded.preview.lines().count(), MAX_DISPLAY_LINES);
        assert!(folded.preview.contains("line 0"));
        assert!(!folded.preview.contains("line 99"));
        assert_eq!(folded.hidden_lines, 100 - MAX_DISPLAY_LINES);
    }

    #[test]
    fn fold_char_cap_marks_hidden_for_one_huge_line() {
        let body = "a".repeat(MAX_DISPLAY_CHARS + 500);
        let folded = fold(&body);
        assert!(folded.preview.chars().count() <= MAX_DISPLAY_CHARS);
        assert!(folded.hidden_lines >= 1);
    }
}
