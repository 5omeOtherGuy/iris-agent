//! `grep` — content search that shells out to ripgrep (`rg`).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::find_binary;
use super::hashline::format_hashline_tag;
use super::path::{relative_display, resolve_existing};
use super::text::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, truncate_head};

const GREP_MAX_LINE_LENGTH: usize = 500;
const DEFAULT_GREP_LIMIT: usize = 100;

pub(super) const DESCRIPTION: &str = "Search file contents for a pattern. Returns matching lines with file paths and line numbers. Respects .gitignore. Output is truncated to 100 matches or 1MB (whichever is hit first). Long lines are truncated to 500 chars. Use hashline=true to get N#AB content-hash tags for use with hashline_edit.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": { "type": "string", "description": "Search pattern (regex or literal string)" },
            "path": { "type": "string", "description": "Directory or file to search (default: current directory)" },
            "glob": { "type": "string", "description": "Filter files by glob pattern, e.g. '*.ts' or '**/*.spec.ts'" },
            "ignoreCase": { "type": "boolean", "description": "Case-insensitive search (default: false)" },
            "literal": { "type": "boolean", "description": "Treat pattern as literal string instead of regex (default: false)" },
            "context": { "type": "integer", "description": "Number of lines to show before and after each match (default: 0)" },
            "limit": { "type": "integer", "description": "Maximum number of matches to return (default: 100)" },
            "hashline": { "type": "boolean", "description": "When true, output each line as N#AB:content where N is the line number and AB is a content hash. Use with hashline_edit tool for precise edits." }
        },
        "required": ["pattern"]
    })
}

pub(super) fn execute(root: &Path, args: &Value) -> Result<String> {
    let input: GrepInput =
        serde_json::from_value(args.clone()).context("grep tool arguments must include pattern")?;
    grep(root, &input)
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
    hashline: bool,
}

fn grep(root: &Path, input: &GrepInput) -> Result<String> {
    if matches!(input.limit, Some(0)) {
        bail!("`limit` must be greater than 0");
    }
    let rg = find_binary(&["rg", "ripgrep"])
        .context("ripgrep (rg) is not available (please install ripgrep)")?;

    let search = input.path.as_deref().unwrap_or(".");
    let search_path = resolve_existing(root, search)?;
    let limit = input.limit.unwrap_or(DEFAULT_GREP_LIMIT).max(1);
    let context = input.context.unwrap_or(0);

    let mut args: Vec<String> = vec![
        "--line-number".to_string(),
        "--no-heading".to_string(),
        "--with-filename".to_string(),
        "--color=never".to_string(),
        "--hidden".to_string(),
        "--max-count".to_string(),
        limit.to_string(),
    ];
    if input.ignore_case {
        args.push("--ignore-case".to_string());
    }
    if input.literal {
        args.push("--fixed-strings".to_string());
    }
    if context > 0 {
        args.push("--context".to_string());
        args.push(context.to_string());
    }
    if let Some(glob) = &input.glob {
        args.push("--glob".to_string());
        args.push(glob.clone());
    }
    args.push("--".to_string());
    args.push(input.pattern.clone());
    args.push(search_path.to_string_lossy().to_string());

    let output = Command::new(rg)
        .args(&args)
        .current_dir(root)
        .output()
        .context("failed to run ripgrep")?;

    // ripgrep exits 1 when there are no matches; that is not an error.
    if !output.status.success() && output.status.code() != Some(1) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);
        if stderr.trim().is_empty() {
            bail!("ripgrep exited with code {code}");
        }
        bail!("ripgrep failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return Ok("No matches found".to_string());
    }

    // Rewrite absolute paths to workspace-relative and cap line length.
    let mut rendered: Vec<String> = Vec::new();
    for line in stdout.lines() {
        rendered.push(rewrite_grep_line(root, &search_path, line, input.hashline));
    }
    let (body, truncated, _) =
        truncate_head(&rendered.join("\n"), DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
    let mut out = body;
    if truncated {
        out.push_str("\n\n[output truncated]");
    }
    Ok(out)
}

fn rewrite_grep_line(root: &Path, search_path: &Path, line: &str, hashline: bool) -> String {
    // ripgrep lines look like `path:line:content` (match) or `path-line-content`
    // (context). Rewrite the leading absolute path to a workspace-relative one
    // and truncate over-long content.
    let search_str = search_path.to_string_lossy();
    let rest = if let Some(stripped) = line.strip_prefix(search_str.as_ref()) {
        let rel = relative_display(root, search_path);
        format!("{rel}{stripped}")
    } else {
        line.to_string()
    };

    let rest = if hashline {
        add_hashline_to_grep_line(&rest).unwrap_or(rest)
    } else {
        rest
    };

    if rest.len() > GREP_MAX_LINE_LENGTH {
        let mut cut = GREP_MAX_LINE_LENGTH;
        while cut > 0 && !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &rest[..cut])
    } else {
        rest
    }
}

fn add_hashline_to_grep_line(line: &str) -> Option<String> {
    let (path, line_number, separator, content) = split_grep_line(line)?;
    let line_idx = line_number.checked_sub(1)?;
    let tag = format_hashline_tag(line_idx, content);
    Some(format!("{path}{separator}{tag}{separator}{content}"))
}

fn split_grep_line(line: &str) -> Option<(&str, usize, char, &str)> {
    for (idx, separator) in line.char_indices() {
        if !matches!(separator, ':' | '-') {
            continue;
        }

        let after_separator = &line[idx + separator.len_utf8()..];
        let digit_len = after_separator
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        if digit_len == 0 {
            continue;
        }

        let after_digits = &after_separator[digit_len..];
        if !after_digits.starts_with(separator) {
            continue;
        }

        let line_number = after_separator[..digit_len].parse().ok()?;
        let content = &after_digits[separator.len_utf8()..];
        return Some((&line[..idx], line_number, separator, content));
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::tools::test_support::{root_of, temp_dir};

    #[test]
    fn grep_finds_matches_when_rg_available() {
        if find_binary(&["rg", "ripgrep"]).is_none() {
            return;
        }
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("g.txt"), "needle here\nhaystack\n").unwrap();
        let out = grep(
            &root,
            &GrepInput {
                pattern: "needle".into(),
                path: None,
                glob: None,
                ignore_case: false,
                literal: false,
                context: None,
                limit: None,
                hashline: false,
            },
        )
        .unwrap();
        assert!(out.contains("needle here"));
        assert!(out.contains("g.txt"));
    }

    #[test]
    fn grep_hashline_tags_match_result_lines_when_rg_available() {
        if find_binary(&["rg", "ripgrep"]).is_none() {
            return;
        }
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("h.txt"), "alpha\nneedle here\ngamma\n").unwrap();
        let out = grep(
            &root,
            &GrepInput {
                pattern: "needle".into(),
                path: Some("h.txt".into()),
                glob: None,
                ignore_case: false,
                literal: false,
                context: None,
                limit: None,
                hashline: true,
            },
        )
        .unwrap();

        let expected = format_hashline_tag(1, "needle here");
        assert!(out.contains(&format!(":{expected}:needle here")), "{out}");
    }

    #[test]
    fn grep_hashline_tags_context_lines_when_rg_available() {
        if find_binary(&["rg", "ripgrep"]).is_none() {
            return;
        }
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("ctx.txt"), "alpha\nneedle here\ngamma\n").unwrap();
        let out = grep(
            &root,
            &GrepInput {
                pattern: "needle".into(),
                path: Some("ctx.txt".into()),
                glob: None,
                ignore_case: false,
                literal: false,
                context: Some(1),
                limit: None,
                hashline: true,
            },
        )
        .unwrap();

        let before = format_hashline_tag(0, "alpha");
        let matched = format_hashline_tag(1, "needle here");
        let after = format_hashline_tag(2, "gamma");
        assert!(out.contains(&format!("-{before}-alpha")), "{out}");
        assert!(out.contains(&format!(":{matched}:needle here")), "{out}");
        assert!(out.contains(&format!("-{after}-gamma")), "{out}");
    }
}
