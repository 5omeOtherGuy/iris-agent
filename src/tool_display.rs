//! Shared, presentation-only formatter for tool-call display.
//!
//! Boundary (AGENTS.md): this is the Iris CLI presentation layer. Every function
//! returns an owned `String` and performs no IO, no color, and never alters what
//! is sent to the model. The read-only vs mutating *policy* lives in
//! [`crate::tools`]; only display verb/path matching lives here.
//!
//! The text front-end calls these helpers after Nexus emits semantic UI events.

use serde_json::Value;

use crate::nexus::ToolCall;

// Single-sourced lifecycle labels (kept aligned with `iris>` / `assistant>`).
const PROPOSED: &str = "tool>";
const DENIED: &str = "denied>";
const RESULT: &str = "result>";
const ERROR: &str = "tool error>";

// Display caps for tool output bodies; presentation-only, never affect the model.
const MAX_DISPLAY_LINES: usize = 20;
const MAX_DISPLAY_CHARS: usize = 2000;
// Cap for one-line command/arg summaries.
const MAX_SUMMARY_CHARS: usize = 100;

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

/// Proposed-call line for the non-gated path (read/grep/find/ls/etc.).
pub(crate) fn proposed_line(call: &ToolCall) -> String {
    format!("{PROPOSED} {}", summarize(call))
}

/// Approval prompt body (the approver appends the y/N read on the same line).
pub(crate) fn approval_prompt(call: &ToolCall) -> String {
    format!("approve {}? [y/N] ", summarize(call))
}

/// Denied-call line.
pub(crate) fn denied_line(call: &ToolCall) -> String {
    format!("{DENIED} {}", summarize(call))
}

/// Success result line. Body is truncated for display here so all display caps
/// live with the formatter.
pub(crate) fn result_line(content: &str) -> String {
    format!("{RESULT} {}", truncate_body(content))
}

/// Tool-error line (the caller formats the anyhow chain as `{err:#}`).
pub(crate) fn error_line(message: &str) -> String {
    format!("{ERROR} {message}")
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
        Some(path) => format!("{} {}", call.name, path),
        None => redacted_fallback(call),
    }
}

/// Bash: `"bash {cmd}"` where `cmd` is the first line of `command`, truncated to
/// `MAX_SUMMARY_CHARS`. Appends the timeout only when explicitly provided:
/// `Some(0)` -> ` (no timeout)`, `Some(n)` -> ` (timeout {n}s)`, `None` -> nothing.
/// cwd is omitted (bash has no `cwd` arg and runs at the workspace root); the slot
/// is documented for a future `cwd` arg as `" (cwd {rel})"`.
fn bash_summary(call: &ToolCall) -> String {
    let command = call.arguments.get("command").and_then(Value::as_str);
    let Some(command) = command else {
        return fallback_summary(call);
    };
    let first_line = command.lines().next().unwrap_or("");
    let mut summary = format!("bash {}", truncate_inline(first_line, MAX_SUMMARY_CHARS));
    match call.arguments.get("timeout").and_then(Value::as_u64) {
        Some(0) => summary.push_str(" (no timeout)"),
        Some(n) => summary.push_str(&format!(" (timeout {n}s)")),
        None => {}
    }
    summary
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
        path,
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
        path
    )
}

fn ls_summary(call: &ToolCall) -> String {
    let path = call
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or(".");
    format!("ls {path}")
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

/// Line- and char-bounded body truncation for the result line. Keeps the full
/// output for the model untouched (that flows independently).
fn truncate_body(text: &str) -> String {
    let mut out = String::new();
    let mut truncated = false;

    for (index, line) in text.lines().enumerate() {
        if index >= MAX_DISPLAY_LINES {
            truncated = true;
            break;
        }
        if index > 0 {
            out.push('\n');
        }
        out.push_str(line);
    }

    if out.chars().count() > MAX_DISPLAY_CHARS {
        out = out.chars().take(MAX_DISPLAY_CHARS).collect();
        truncated = true;
    }

    if truncated {
        out.push_str("\n\u{2026} (truncated)");
    }
    out
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
    fn summarize_bash_long_multiline_command_is_single_truncated_line() {
        let command = format!("first {}\nsecond line", "a".repeat(200));
        let summary = summarize(&call("bash", json!({ "command": command })));
        assert!(summary.starts_with("bash first "));
        assert!(!summary.contains('\n'));
        assert!(!summary.contains("second line"));
        assert!(summary.ends_with('\u{2026}'));
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
    fn proposed_and_denied_lines_carry_summary() {
        let c = call("read", json!({ "path": "a.rs" }));
        assert_eq!(proposed_line(&c), "tool> read a.rs");
        assert_eq!(denied_line(&c), "denied> read a.rs");
    }

    #[test]
    fn approval_prompt_wraps_summary() {
        let c = call("write", json!({ "path": "note.txt", "content": "hi" }));
        assert_eq!(approval_prompt(&c), "approve write note.txt? [y/N] ");
    }

    #[test]
    fn result_line_prefixes_and_caps_long_output() {
        let text = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = result_line(&text);
        assert!(rendered.starts_with("result> "));
        assert!(rendered.contains("line 0"));
        assert!(rendered.contains("(truncated)"));
        assert!(!rendered.contains("line 99"));
    }

    #[test]
    fn result_line_keeps_short_output() {
        assert_eq!(result_line("short output"), "result> short output");
    }

    #[test]
    fn result_line_truncates_over_char_cap() {
        let body = "a".repeat(MAX_DISPLAY_CHARS + 500);
        let rendered = result_line(&body);
        assert!(rendered.contains("(truncated)"));
        assert!(rendered.chars().count() < body.chars().count());
    }

    #[test]
    fn error_line_prefixes_and_preserves_message() {
        assert_eq!(
            error_line("unknown tool: unknown"),
            "tool error> unknown tool: unknown"
        );
    }
}
