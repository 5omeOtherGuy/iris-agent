//! `read` — line-numbered file reads with offset/limit windowing and an
//! optional hashline-tagged rendering for use with `hashline_edit`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::hashline::format_hashline_tag;
use super::path::resolve_existing;
use super::text::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, READ_TOOL_MAX_BYTES};

pub(super) const DESCRIPTION: &str = "Read the contents of a text file. Output is truncated to 2000 lines or 1MB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
            "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
            "limit": { "type": "integer", "description": "Maximum number of lines to read" },
            "hashline": { "type": "boolean", "description": "When true, output each line as N#AB:content where N is the line number and AB is a content hash. Use with hashline_edit tool for precise edits." }
        },
        "required": ["path"]
    })
}

pub(super) fn execute(root: &Path, args: &Value) -> Result<String> {
    let input: ReadInput =
        serde_json::from_value(args.clone()).context("read tool arguments must include path")?;
    read(root, &input)
}

#[derive(Debug, Deserialize)]
struct ReadInput {
    path: String,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    hashline: bool,
}

fn read(root: &Path, input: &ReadInput) -> Result<String> {
    if matches!(input.limit, Some(limit) if limit <= 0) {
        bail!("`limit` must be greater than 0");
    }
    if matches!(input.offset, Some(offset) if offset < 0) {
        bail!("`offset` must be non-negative");
    }

    let resolved = resolve_existing(root, &input.path)?;
    let metadata =
        fs::metadata(&resolved).with_context(|| format!("failed to stat {}", input.path))?;
    if !metadata.is_file() {
        bail!("path {} is not a regular file", input.path);
    }
    if metadata.len() > READ_TOOL_MAX_BYTES {
        bail!(
            "file is too large ({} bytes). Max allowed is {READ_TOOL_MAX_BYTES} bytes.",
            metadata.len()
        );
    }

    let bytes = fs::read(&resolved).with_context(|| format!("failed to read {}", input.path))?;
    let content = String::from_utf8_lossy(&bytes);

    let mut lines: Vec<&str> = content.split('\n').collect();
    // A trailing newline produces a final empty element that is not a real line.
    if content.ends_with('\n') {
        lines.pop();
    }
    let total_lines = lines.len();
    if total_lines == 0 {
        return Ok(String::new());
    }

    let offset = input.offset.unwrap_or(1).max(1) as usize;
    let start = offset - 1;
    if start >= total_lines {
        bail!("offset {offset} is beyond end of file ({total_lines} lines total)");
    }
    let limit = input.limit.map_or(DEFAULT_MAX_LINES, |l| l as usize).max(1);
    let mut end = (start + limit).min(total_lines);

    let width = end.to_string().len().max(1);
    let mut rendered: Vec<String> = Vec::new();
    let mut byte_count = 0usize;
    let mut byte_capped = false;
    for (offset_in_window, idx) in (start..end).enumerate() {
        let line = lines[idx].strip_suffix('\r').unwrap_or(lines[idx]);
        let formatted = if input.hashline {
            format!("{}:{line}", format_hashline_tag(idx, line))
        } else {
            format!("{:>width$}\u{2192}{line}", idx + 1)
        };
        byte_count += formatted.len() + 1;
        if byte_count > DEFAULT_MAX_BYTES && offset_in_window > 0 {
            end = idx;
            byte_capped = true;
            break;
        }
        rendered.push(formatted);
    }

    let mut out = rendered.join("\n");
    if end < total_lines {
        let next_offset = end + 1;
        if byte_capped {
            out.push_str(&format!(
                "\n\n[Showing lines {}-{end} of {total_lines} (1MB limit). Use offset={next_offset} to continue.]",
                start + 1
            ));
        } else {
            let remaining = total_lines - end;
            let plural = if remaining == 1 { "" } else { "s" };
            out.push_str(&format!(
                "\n\n[{remaining} more line{plural} in file. Use offset={next_offset} to continue.]"
            ));
        }
    }
    Ok(out)
}

/// Convenience entry used by integration tests: read with default options.
#[cfg(test)]
pub(crate) fn read_file(workspace: &Path, path: &str) -> Result<String> {
    let root = super::path::workspace_root(workspace)?;
    read(
        &root,
        &ReadInput {
            path: path.to_string(),
            offset: None,
            limit: None,
            hashline: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::hashline::validate_line_ref;
    use crate::tools::test_support::{root_of, temp_dir};

    #[test]
    fn read_returns_line_numbered_content() {
        let dir = temp_dir();
        fs::write(dir.path.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let out = read_file(&dir.path, "a.txt").unwrap();
        assert!(out.contains("\u{2192}alpha"));
        assert!(out.contains("3\u{2192}gamma"));
    }

    #[test]
    fn read_offset_and_limit_window() {
        let dir = temp_dir();
        let body: String = (1..=10).map(|n| format!("line{n}\n")).collect();
        fs::write(dir.path.join("b.txt"), body).unwrap();
        let root = root_of(&dir);
        let out = read(
            &root,
            &ReadInput {
                path: "b.txt".into(),
                offset: Some(3),
                limit: Some(2),
                hashline: false,
            },
        )
        .unwrap();
        assert!(out.contains("3\u{2192}line3"));
        assert!(out.contains("4\u{2192}line4"));
        assert!(!out.contains("line5"));
        assert!(out.contains("more lines in file"));
    }

    #[test]
    fn read_rejects_escape() {
        let dir = temp_dir();
        let err = read_file(&dir.path, "../escape.txt")
            .unwrap_err()
            .to_string();
        assert!(err.contains("escapes workspace") || err.contains("failed to resolve path"));
    }

    #[test]
    fn hashline_tag_roundtrips_through_read_and_validation() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("h.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let rendered = read(
            &root,
            &ReadInput {
                path: "h.txt".into(),
                offset: None,
                limit: None,
                hashline: true,
            },
        )
        .unwrap();
        // First rendered line is `1#XY:alpha`; parse its tag and validate it.
        let first = rendered.lines().next().unwrap();
        let tag = first.split(':').next().unwrap();
        let lines = vec!["alpha", "beta", "gamma", ""];
        assert_eq!(validate_line_ref(tag, &lines, false).unwrap(), 0);
    }
}
