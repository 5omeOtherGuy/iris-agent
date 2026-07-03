//! `read` — line-numbered file reads with offset/limit windowing.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ObservedFiles;
use super::path::resolve_existing;
use super::text::{READ_TOOL_MAX_BYTES, render_line_window, validate_offset_limit};

pub(super) const DESCRIPTION: &str = "Read the contents of a text file. Output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete.";

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

pub(super) fn execute(
    root: &Path,
    args: &Value,
    observed: &mut ObservedFiles,
) -> Result<super::ToolOutput> {
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

fn read(root: &Path, input: &ReadInput, observed: &mut ObservedFiles) -> Result<super::ToolOutput> {
    // Reject bad window args before any filesystem work, matching `read`'s
    // original pre-I/O validation order (the shared windower re-checks anyway).
    validate_offset_limit(input.offset, input.limit)?;

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

    // `bytes.len()` equals `content.len()` here (valid UTF-8), so the shared
    // windower's `total_bytes` matches the file's byte length.
    Ok(render_line_window(content, input.offset, input.limit)?.into_output())
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
    .map(|output| output.content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{root_of, temp_dir};
    use crate::tools::text::DEFAULT_MAX_BYTES;

    #[test]
    fn read_returns_line_numbered_content() {
        let dir = temp_dir();
        fs::write(dir.path.join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let out = read_file(&dir.path, "a.txt").unwrap();
        assert!(out.contains("\u{2192}alpha"));
        assert!(out.contains("3\u{2192}gamma"));
    }

    #[test]
    fn read_result_contract_reports_bounded_metadata() {
        let dir = temp_dir();
        fs::write(dir.path.join("a.txt"), "alpha\nbeta\n").unwrap();
        let root = root_of(&dir);
        let output = read(
            &root,
            &ReadInput {
                path: "a.txt".into(),
                offset: None,
                limit: Some(1),
            },
            &mut ObservedFiles::new(),
        )
        .unwrap();

        assert_eq!(output.metadata.get("bytes"), Some(&json!(11)));
        assert_eq!(output.metadata.get("lines"), Some(&json!(1)));
        assert_eq!(output.metadata.get("total_lines"), Some(&json!(2)));
        assert_eq!(output.metadata.get("truncated"), Some(&json!(true)));
        assert!(output.content.contains("Use offset=2 to continue"));
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
        .unwrap()
        .content;
        assert!(out.contains("3\u{2192}line3"));
        assert!(out.contains("4\u{2192}line4"));
        assert!(!out.contains("line5"));
        assert!(out.contains("more lines in file"));
    }

    #[test]
    fn read_truncates_single_line_over_default_byte_cap() {
        let dir = temp_dir();
        fs::write(
            dir.path.join("long.txt"),
            "x".repeat(DEFAULT_MAX_BYTES + 1024),
        )
        .unwrap();

        let out = read_file(&dir.path, "long.txt").unwrap();

        assert!(
            out.len() < DEFAULT_MAX_BYTES + 256,
            "len={} out tail={:?}",
            out.len(),
            &out[out.len().saturating_sub(128)..]
        );
        assert!(out.contains("50KB limit"), "{out}");
    }

    #[test]
    fn read_validates_window_args_before_touching_the_file() {
        // `limit`/`offset` are rejected before path resolution / file I/O, so a
        // bad arg surfaces the arg error even when the path does not exist.
        let dir = temp_dir();
        let root = root_of(&dir);
        let err = read(
            &root,
            &ReadInput {
                path: "missing.txt".into(),
                offset: None,
                limit: Some(0),
            },
            &mut ObservedFiles::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("`limit` must be greater than 0"), "{err}");
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
