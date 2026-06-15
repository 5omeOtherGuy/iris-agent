//! Hashline content-hash tags and the `hashline_edit` tool.
//!
//! The tag algorithm (xxh32 over the whitespace-stripped line, encoded with the
//! `NIBBLE_STR` alphabet) is ported from pi so tags round-trip between `read`'s
//! `hashline` option and `hashline_edit`.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use xxhash_rust::xxh32::xxh32;

use super::path::resolve_existing;
use super::text::{
    atomic_write, detect_line_ending, normalize_to_lf, restore_line_endings, strip_bom,
};

/// Hashline encoding alphabet (16 letters, one per nibble), copied from pi.
const NIBBLE_STR: &[u8; 16] = b"ZPMQVRWSNKTXJBYH";

pub(super) const DESCRIPTION: &str = "Apply precise file edits using LINE#HASH tags from a prior read with hashline=true. Each edit specifies an op (replace/prepend/append), a pos anchor (\"N#AB\"), an optional end anchor for range replace, and replacement lines. Edits are validated against current file hashes and applied bottom-up to avoid index invalidation.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
            "edits": {
                "type": "array",
                "description": "Array of edit operations to apply",
                "items": {
                    "type": "object",
                    "properties": {
                        "op": { "type": "string", "enum": ["replace", "prepend", "append"], "description": "Operation type" },
                        "pos": { "type": "string", "description": "Anchor line reference in LINE#HASH format (e.g. \"5#KJ\")" },
                        "end": { "type": "string", "description": "End anchor for range replace (inclusive)" },
                        "lines": {
                            "description": "Replacement/insertion content as array of strings, single string, or null for deletion",
                            "oneOf": [
                                { "type": "array", "items": { "type": "string" } },
                                { "type": "string" },
                                { "type": "null" }
                            ]
                        }
                    },
                    "required": ["op"]
                }
            }
        },
        "required": ["path", "edits"]
    })
}

pub(super) fn execute(root: &Path, args: &Value) -> Result<String> {
    let input: HashlineEditInput = serde_json::from_value(args.clone())
        .context("hashline_edit tool arguments must include path and edits")?;
    hashline_edit(root, &input)
}

#[derive(Debug, Deserialize)]
struct HashlineEditInput {
    path: String,
    edits: Vec<HashlineOp>,
}

#[derive(Debug, Clone, Deserialize)]
struct HashlineOp {
    op: String,
    #[serde(default)]
    pos: Option<String>,
    #[serde(default)]
    end: Option<String>,
    #[serde(default)]
    lines: Option<Value>,
}

impl HashlineOp {
    fn replacement_lines(&self) -> Vec<String> {
        match &self.lines {
            None | Some(Value::Null) => vec![],
            Some(Value::String(s)) => normalize_to_lf(s).split('\n').map(String::from).collect(),
            Some(Value::Array(arr)) => arr
                .iter()
                .map(|v| match v {
                    Value::String(s) => normalize_to_lf(s),
                    other => normalize_to_lf(&other.to_string()),
                })
                .collect(),
            Some(other) => vec![normalize_to_lf(&other.to_string())],
        }
    }
}

struct ResolvedHashlineEdit {
    op: &'static str,
    start: usize,
    end: usize,
    lines: Vec<String>,
}

fn hashline_edit(root: &Path, input: &HashlineEditInput) -> Result<String> {
    if input.edits.is_empty() {
        bail!("no edits provided");
    }
    let resolved = resolve_existing(root, &input.path)?;
    let metadata =
        fs::metadata(&resolved).with_context(|| format!("file not found: {}", input.path))?;
    if !metadata.is_file() {
        bail!("path {} is not a regular file", input.path);
    }

    let raw = fs::read(&resolved).with_context(|| format!("failed to read {}", input.path))?;
    let raw_content = String::from_utf8(raw)
        .context("file contains invalid UTF-8 and cannot be safely edited as text")?;
    let (content_no_bom, had_bom) = strip_bom(&raw_content);
    let original_ending = detect_line_ending(content_no_bom);
    let normalized = normalize_to_lf(content_no_bom);
    let file_lines: Vec<&str> = normalized.split('\n').collect();

    // Validate every anchor against the current file before touching anything.
    for edit in &input.edits {
        if let Some(pos) = &edit.pos {
            validate_line_ref(pos, &file_lines, had_bom)?;
        }
        if let Some(end) = &edit.end {
            validate_line_ref(end, &file_lines, had_bom)?;
        }
    }

    let mut resolved_edits: Vec<ResolvedHashlineEdit> = Vec::new();
    for edit in &input.edits {
        let lines = edit
            .replacement_lines()
            .into_iter()
            .map(|l| strip_hashline_prefix(&l).to_string())
            .collect::<Vec<_>>();
        match edit.op.as_str() {
            "replace" => {
                let start = match &edit.pos {
                    Some(pos) => validate_line_ref(pos, &file_lines, had_bom)?,
                    None => bail!("replace operation requires a pos anchor"),
                };
                let end = match &edit.end {
                    Some(end) => validate_line_ref(end, &file_lines, had_bom)?,
                    None => start,
                };
                if end < start {
                    bail!(
                        "end anchor (line {}) is before start anchor (line {})",
                        end + 1,
                        start + 1
                    );
                }
                resolved_edits.push(ResolvedHashlineEdit {
                    op: "replace",
                    start,
                    end,
                    lines,
                });
            }
            "prepend" => {
                let idx = match &edit.pos {
                    Some(pos) => validate_line_ref(pos, &file_lines, had_bom)?,
                    None => 0,
                };
                resolved_edits.push(ResolvedHashlineEdit {
                    op: "prepend",
                    start: idx,
                    end: idx,
                    lines,
                });
            }
            "append" => {
                let idx = match &edit.pos {
                    Some(pos) => validate_line_ref(pos, &file_lines, had_bom)?,
                    None => file_lines.len().saturating_sub(1),
                };
                resolved_edits.push(ResolvedHashlineEdit {
                    op: "append",
                    start: idx,
                    end: idx,
                    lines,
                });
            }
            other => bail!("unknown op: {other:?}. Must be replace, prepend, or append."),
        }
    }

    // Apply bottom-up so earlier indices stay valid; reject overlaps.
    resolved_edits.sort_by(|a, b| {
        b.start
            .cmp(&a.start)
            .then_with(|| op_precedence(a.op).cmp(&op_precedence(b.op)))
    });
    for i in 0..resolved_edits.len() {
        for j in (i + 1)..resolved_edits.len() {
            let a = &resolved_edits[i];
            let b = &resolved_edits[j];
            if a.start <= b.end && b.start <= a.end {
                bail!(
                    "overlapping edits detected at lines {}-{} and {}-{}; combine them",
                    a.start + 1,
                    a.end + 1,
                    b.start + 1,
                    b.end + 1
                );
            }
        }
    }

    let mut lines: Vec<String> = file_lines.iter().map(|s| (*s).to_string()).collect();
    let mut any_change = false;
    for edit in &resolved_edits {
        match edit.op {
            "replace" => {
                let existing: Vec<&str> = lines[edit.start..=edit.end]
                    .iter()
                    .map(String::as_str)
                    .collect();
                let replacement: Vec<&str> = edit.lines.iter().map(String::as_str).collect();
                if existing == replacement {
                    continue;
                }
                lines.splice(edit.start..=edit.end, edit.lines.iter().cloned());
                any_change = true;
            }
            "prepend" => {
                if !edit.lines.is_empty() {
                    lines.splice(edit.start..edit.start, edit.lines.iter().cloned());
                    any_change = true;
                }
            }
            "append" if !edit.lines.is_empty() => {
                let insert_at = edit.start + 1;
                lines.splice(insert_at..insert_at, edit.lines.iter().cloned());
                any_change = true;
            }
            _ => {}
        }
    }

    if !any_change {
        bail!("no changes made to {}; all edits were no-ops", input.path);
    }

    let new_normalized = lines.join("\n");
    let restored = restore_line_endings(&new_normalized, original_ending);
    let final_content = if had_bom {
        format!("\u{FEFF}{restored}")
    } else {
        restored
    };
    atomic_write(&resolved, final_content.as_bytes())
        .with_context(|| format!("failed to write {}", input.path))?;

    Ok(format!(
        "Successfully applied hashline edits to {}.",
        input.path
    ))
}

fn op_precedence(op: &str) -> u8 {
    match op {
        "replace" => 0,
        "append" => 1,
        "prepend" => 2,
        _ => 3,
    }
}

// ============================================================================
// Hashline tag algorithm (ported from pi)
// ============================================================================

/// Compute the 2-character content hash for the (0-indexed) line.
fn compute_line_hash(line_idx: usize, line: &str) -> [u8; 2] {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let significant: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    let has_alnum = significant.chars().any(|c| c.is_alphanumeric());
    let seed = if has_alnum { 0 } else { line_idx as u32 };
    let hash = xxh32(significant.as_bytes(), seed);
    let byte = (hash & 0xFF) as usize;
    [NIBBLE_STR[byte & 0x0F], NIBBLE_STR[(byte >> 4) & 0x0F]]
}

fn compute_line_hash_with_bom(line_idx: usize, line: &str, had_bom: bool) -> [u8; 2] {
    if had_bom && line_idx == 0 {
        let with_bom = format!("\u{FEFF}{line}");
        compute_line_hash(line_idx, &with_bom)
    } else {
        compute_line_hash(line_idx, line)
    }
}

pub(super) fn format_hashline_tag(line_idx: usize, line: &str) -> String {
    let h = compute_line_hash(line_idx, line);
    format!("{}#{}{}", line_idx + 1, h[0] as char, h[1] as char)
}

fn format_hashline_tag_with_bom(line_idx: usize, line: &str, had_bom: bool) -> String {
    let h = compute_line_hash_with_bom(line_idx, line, had_bom);
    format!("{}#{}{}", line_idx + 1, h[0] as char, h[1] as char)
}

/// Parse a `LINE#HASH` reference, tolerating leading whitespace and diff
/// markers (`>`, `+`, `-`) plus spaces around `#`. Returns (1-indexed line, hash).
fn parse_hashline_tag(ref_str: &str) -> Result<(usize, [u8; 2])> {
    let bytes = ref_str.as_bytes();
    let mut i = 0;
    while i < bytes.len()
        && (bytes[i].is_ascii_whitespace() || matches!(bytes[i], b'>' | b'+' | b'-'))
    {
        i += 1;
    }
    let digit_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digit_start {
        bail!("invalid hashline reference: {ref_str:?}");
    }
    let line_num: usize = ref_str[digit_start..i]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid line number in {ref_str:?}"))?;
    if line_num == 0 {
        bail!("line number must be >= 1 in {ref_str:?}");
    }
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'#' {
        bail!("invalid hashline reference (missing '#'): {ref_str:?}");
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i + 2 > bytes.len() || !is_nibble(bytes[i]) || !is_nibble(bytes[i + 1]) {
        bail!("invalid hashline hash in {ref_str:?}");
    }
    Ok((line_num, [bytes[i], bytes[i + 1]]))
}

fn is_nibble(b: u8) -> bool {
    NIBBLE_STR.contains(&b)
}

/// Strip a `N#AB:` tag prefix that a model may echo into replacement content.
fn strip_hashline_prefix(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len()
        && (bytes[i].is_ascii_whitespace() || matches!(bytes[i], b'>' | b'+' | b'-'))
    {
        i += 1;
    }
    let digit_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digit_start {
        return line;
    }
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'#' {
        return line;
    }
    i += 1;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i + 2 > bytes.len() || !is_nibble(bytes[i]) || !is_nibble(bytes[i + 1]) {
        return line;
    }
    i += 2;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b':' {
        &line[i + 1..]
    } else {
        line
    }
}

/// Validate a tag against the file and return its 0-indexed line.
pub(super) fn validate_line_ref(
    ref_str: &str,
    file_lines: &[&str],
    had_bom: bool,
) -> Result<usize> {
    let (line_num, expected) = parse_hashline_tag(ref_str)?;
    let idx = line_num - 1;
    if idx >= file_lines.len() {
        bail!(
            "line {line_num} out of range (file has {} lines)",
            file_lines.len()
        );
    }
    let actual = compute_line_hash_with_bom(idx, file_lines[idx], had_bom);
    if actual != expected {
        let actual_tag = format_hashline_tag_with_bom(idx, file_lines[idx], had_bom);
        bail!(
            "hash mismatch at line {line_num}: expected {}#{}{}, actual is {actual_tag}; re-read the file to get current tags",
            line_num,
            expected[0] as char,
            expected[1] as char
        );
    }
    Ok(idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{root_of, temp_dir};

    #[test]
    fn hashline_edit_replaces_anchored_line() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("k.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let file_lines = vec!["alpha", "beta", "gamma", ""];
        let tag = format_hashline_tag(1, "beta");
        hashline_edit(
            &root,
            &HashlineEditInput {
                path: "k.txt".into(),
                edits: vec![HashlineOp {
                    op: "replace".into(),
                    pos: Some(tag),
                    end: None,
                    lines: Some(Value::String("BETA".into())),
                }],
            },
        )
        .unwrap();
        let _ = file_lines;
        let content = fs::read_to_string(dir.path.join("k.txt")).unwrap();
        assert_eq!(content, "alpha\nBETA\ngamma\n");
    }

    #[test]
    fn hashline_edit_rejects_stale_tag() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("m.txt"), "alpha\nbeta\n").unwrap();
        let err = hashline_edit(
            &root,
            &HashlineEditInput {
                path: "m.txt".into(),
                edits: vec![HashlineOp {
                    op: "replace".into(),
                    pos: Some("1#ZZ".into()),
                    end: None,
                    lines: Some(Value::String("x".into())),
                }],
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("hash mismatch") || err.contains("invalid hashline"));
    }
}
