//! `read` — line-numbered file reads with offset/limit windowing.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ObservedFiles;
use super::path::resolve_existing;
use super::text::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, READ_TOOL_MAX_BYTES};

pub(super) const DESCRIPTION: &str = "Read the contents of a text file. Output is truncated to 2000 lines or 1MB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
            "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
            "limit": { "type": "integer", "description": "Maximum number of lines to read" }
        },
        "required": ["path"]
    })
}

pub(super) fn execute(root: &Path, args: &Value, observed: &mut ObservedFiles) -> Result<String> {
    let input: ReadInput =
        serde_json::from_value(args.clone()).context("read tool arguments must include path")?;
    read(root, &input, observed)
}

#[derive(Debug, Deserialize)]
struct ReadInput {
    path: String,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

fn read(root: &Path, input: &ReadInput, observed: &mut ObservedFiles) -> Result<String> {
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
    if bytes.contains(&0) {
        bail!(
            "file appears to be binary and cannot be safely read as text: {}",
            input.path
        );
    }
    let content = std::str::from_utf8(&bytes).with_context(|| {
        format!(
            "file is not valid UTF-8 and cannot be safely read as text: {}",
            input.path
        )
    })?;
    // The agent now knows this file's current bytes; record it so a later
    // edit/write can detect changes made behind its back. `read` always loads
    // the full file even when offset/limit windows the output.
    observed.observe(&resolved, &bytes);

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
        let formatted = format!("{:>width$}\u{2192}{line}", idx + 1);
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
        },
        &mut ObservedFiles::new(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
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
            },
            &mut ObservedFiles::new(),
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
    fn read_rejects_invalid_utf8_instead_of_lossy_rendering() {
        let dir = temp_dir();
        fs::write(dir.path.join("binary.dat"), [b'a', 0xFF, b'b']).unwrap();

        let err = read_file(&dir.path, "binary.dat").unwrap_err().to_string();

        assert!(err.contains("not valid UTF-8"), "{err}");
    }

    #[test]
    fn read_rejects_nul_containing_binary_file() {
        let dir = temp_dir();
        fs::write(dir.path.join("nul.dat"), b"alpha\0beta").unwrap();

        let err = read_file(&dir.path, "nul.dat").unwrap_err().to_string();

        assert!(err.contains("binary"), "{err}");
    }
}
