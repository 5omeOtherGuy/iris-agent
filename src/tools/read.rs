//! `read` — line-numbered file reads with offset/limit windowing.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::ObservedFiles;
use super::path::resolve_existing;
use super::skim::skim_mask;
use super::text::{
    READ_TOOL_MAX_BYTES, render_line_window, render_line_window_masked, validate_offset_limit,
};

pub(super) const DESCRIPTION: &str = "Read the contents of a text file. Output is truncated to 2000 lines or 50KB (whichever is hit first). Use offset/limit for large files. When you need the full file, continue with offset until complete. For exploration-only reads, skim: true strips comments, docstrings, and blank lines; a full read (without skim) is still required before editing a file.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
            "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
            "limit": { "type": "integer", "description": "Maximum number of lines to read" },
            "skim": { "type": "boolean", "description": "Exploration mode: strip comments, docstrings, and blank lines from source files (line numbers keep their original values). A skim read does not satisfy read-before-edit; do a full read before modifying the file." }
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
    #[serde(default)]
    skim: bool,
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
    if input.skim {
        // Skim is an exploration read of filtered content: deliberately NOT
        // observed, so edit/write still demand a full-fidelity read first
        // (exact-text edits must never be based on filtered content).
        return skim_read(content, input, &resolved);
    }

    // The agent now knows this file's current bytes; record it so a later
    // edit/write can detect changes made behind its back. `read` always loads
    // the full file even when offset/limit windows the output.
    observed.observe(&resolved, &bytes);

    // `bytes.len()` equals `content.len()` here (valid UTF-8), so the shared
    // windower's `total_bytes` matches the file's byte length.
    Ok(render_line_window(content, input.offset, input.limit)?.into_output())
}

/// Skim rendering with its safety guards. Falls back to the full window (and
/// says so in `metadata.skim`) whenever skimming would not help: unknown or
/// data-format extensions, a skim that empties a non-empty window, or a skim
/// that is not smaller than the full rendering (never-worse).
fn skim_read(content: &str, input: &ReadInput, resolved: &Path) -> Result<super::ToolOutput> {
    let full = render_line_window(content, input.offset, input.limit)?;
    let extension = resolved.extension().and_then(|e| e.to_str());
    let Some(mask) = skim_mask(content, extension) else {
        return Ok(full
            .into_output()
            .with("skim", json!("full (file type is never skimmed)")));
    };
    let mut skimmed = render_line_window_masked(content, input.offset, input.limit, Some(&mask))?;
    if skimmed.lines_shown == 0 && full.lines_shown > 0 {
        // Emptied-non-empty guard: stripping removed the whole window.
        return Ok(full
            .into_output()
            .with("skim", json!("full (skim emptied the file)")));
    }
    let omitted = full.lines_shown - skimmed.lines_shown;
    skimmed.content.push_str(&format!(
        "\n\n[skim: {omitted} lines hidden; full read required before edit]"
    ));
    if skimmed.content.len() >= full.content.len() {
        // Never-worse guard: skim must save tokens or it is not applied.
        return Ok(full
            .into_output()
            .with("skim", json!("full (skim was not smaller)")));
    }
    Ok(skimmed.into_output().with("skim", json!("applied")))
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
            skim: false,
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
                skim: false,
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
                skim: false,
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
                skim: false,
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

    // --- skim mode (issue #337) ---

    fn read_args(
        dir: &crate::tools::test_support::TestDir,
        args: serde_json::Value,
        observed: &mut ObservedFiles,
    ) -> super::super::ToolOutput {
        let root = root_of(dir);
        execute(&root, &args, observed).unwrap()
    }

    #[test]
    fn skim_strips_boilerplate_and_keeps_true_line_numbers() {
        let dir = temp_dir();
        fs::write(
            dir.path.join("s.rs"),
            "// top comment explaining the module in some detail\n\nfn main() {\n    // inner note about the call below\n    body();\n}\n",
        )
        .unwrap();
        let out = read_args(
            &dir,
            json!({"path": "s.rs", "skim": true}),
            &mut ObservedFiles::new(),
        );

        assert_eq!(out.metadata.get("skim"), Some(&json!("applied")));
        // Kept lines carry their original numbers (gaps where lines were
        // stripped), so offsets and follow-up full reads stay coherent.
        assert!(
            out.content.contains("3\u{2192}fn main() {"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("5\u{2192}    body();"),
            "{}",
            out.content
        );
        assert!(out.content.contains("6\u{2192}}"), "{}", out.content);
        assert!(!out.content.contains("top comment"), "{}", out.content);
        assert!(!out.content.contains("inner"), "{}", out.content);
        assert!(!out.content.contains("1\u{2192}"), "{}", out.content);
        assert!(!out.content.contains("2\u{2192}"), "{}", out.content);
        assert!(
            out.content.contains("[skim: 3 lines hidden"),
            "{}",
            out.content
        );
    }

    #[test]
    fn skim_absent_is_byte_identical_to_full_read() {
        let dir = temp_dir();
        fs::write(dir.path.join("s.rs"), "// c\nfn f() {}\n").unwrap();
        let full = read_args(&dir, json!({"path": "s.rs"}), &mut ObservedFiles::new());
        let explicit_false = read_args(
            &dir,
            json!({"path": "s.rs", "skim": false}),
            &mut ObservedFiles::new(),
        );
        assert_eq!(full.content, explicit_false.content);
        assert!(full.metadata.get("skim").is_none());
        assert!(full.content.contains("1\u{2192}// c"));
    }

    #[test]
    fn skim_never_strips_data_formats() {
        let dir = temp_dir();
        fs::write(
            dir.path.join("d.json"),
            "{\n  \"glob\": \"packages/*\"\n}\n",
        )
        .unwrap();
        let skim = read_args(
            &dir,
            json!({"path": "d.json", "skim": true}),
            &mut ObservedFiles::new(),
        );
        let full = read_args(&dir, json!({"path": "d.json"}), &mut ObservedFiles::new());
        assert_eq!(skim.content, full.content);
        assert_eq!(
            skim.metadata.get("skim"),
            Some(&json!("full (file type is never skimmed)"))
        );
    }

    #[test]
    fn skim_emptied_non_empty_file_falls_back_to_full() {
        let dir = temp_dir();
        fs::write(dir.path.join("c.rs"), "// only\n// comments\n").unwrap();
        let out = read_args(
            &dir,
            json!({"path": "c.rs", "skim": true}),
            &mut ObservedFiles::new(),
        );
        assert_eq!(
            out.metadata.get("skim"),
            Some(&json!("full (skim emptied the file)"))
        );
        assert!(out.content.contains("1\u{2192}// only"));
        assert!(out.content.contains("2\u{2192}// comments"));
    }

    #[test]
    fn skim_never_worse_returns_full_when_not_smaller() {
        let dir = temp_dir();
        // Nothing to strip: skim output (plus its notice) is not smaller than
        // the full rendering, so the full rendering wins.
        fs::write(dir.path.join("n.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let out = read_args(
            &dir,
            json!({"path": "n.rs", "skim": true}),
            &mut ObservedFiles::new(),
        );
        assert_eq!(
            out.metadata.get("skim"),
            Some(&json!("full (skim was not smaller)"))
        );
        assert!(!out.content.contains("[skim:"), "{}", out.content);
        assert!(out.content.contains("1\u{2192}fn a() {}"));
    }

    #[test]
    fn skim_window_notices_stay_in_original_line_space() {
        let dir = temp_dir();
        fs::write(
            dir.path.join("w.rs"),
            "// long leading comment about function a below\nfn a() {}\n// long comment describing function b below\nfn b() {}\nfn c() {}\n",
        )
        .unwrap();
        let out = read_args(
            &dir,
            json!({"path": "w.rs", "skim": true, "offset": 1, "limit": 4}),
            &mut ObservedFiles::new(),
        );
        assert_eq!(out.metadata.get("skim"), Some(&json!("applied")));
        assert!(
            out.content.contains("2\u{2192}fn a() {}"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("4\u{2192}fn b() {}"),
            "{}",
            out.content
        );
        // The continuation offset counts original file lines, not skim lines.
        assert!(
            out.content.contains("Use offset=5 to continue"),
            "{}",
            out.content
        );
    }

    #[test]
    fn edit_after_skim_only_read_is_rejected_like_unread() {
        let dir = temp_dir();
        fs::write(dir.path.join("e.rs"), "// c\nfn f() {}\n").unwrap();
        let root = root_of(&dir);
        let mut observed = ObservedFiles::new();
        read_args(&dir, json!({"path": "e.rs", "skim": true}), &mut observed);

        let err = super::super::edit::execute(
            &root,
            &json!({
                "file_path": root.join("e.rs").to_string_lossy(),
                "old_string": "fn f() {}",
                "new_string": "fn g() {}"
            }),
            &mut observed,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("has not been read this session"), "{err}");
    }

    #[test]
    fn skim_fallback_to_full_content_still_does_not_observe() {
        let dir = temp_dir();
        // Data-format skim falls back to the full rendering, but it was still
        // a skim request: edit must stay rejected.
        fs::write(dir.path.join("f.rs"), "fn a() {}\n").unwrap();
        let root = root_of(&dir);
        let mut observed = ObservedFiles::new();
        read_args(&dir, json!({"path": "f.rs", "skim": true}), &mut observed);

        let err = super::super::edit::execute(
            &root,
            &json!({
                "file_path": root.join("f.rs").to_string_lossy(),
                "old_string": "fn a() {}",
                "new_string": "fn b() {}"
            }),
            &mut observed,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("has not been read this session"), "{err}");
    }

    #[test]
    fn edit_after_skim_then_full_read_succeeds() {
        let dir = temp_dir();
        fs::write(dir.path.join("g.rs"), "// c\nfn f() {}\n").unwrap();
        let root = root_of(&dir);
        let mut observed = ObservedFiles::new();
        read_args(&dir, json!({"path": "g.rs", "skim": true}), &mut observed);
        read_args(&dir, json!({"path": "g.rs"}), &mut observed);

        super::super::edit::execute(
            &root,
            &json!({
                "file_path": root.join("g.rs").to_string_lossy(),
                "old_string": "fn f() {}",
                "new_string": "fn g() {}"
            }),
            &mut observed,
        )
        .unwrap();
        assert!(
            fs::read_to_string(dir.path.join("g.rs"))
                .unwrap()
                .contains("fn g() {}")
        );
    }
}
