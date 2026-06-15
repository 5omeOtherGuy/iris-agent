//! `edit` — unique-match text replacement with line-ending/Unicode-tolerant
//! fuzzy fallback matching.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::Preview;
use super::path::resolve_existing;
use super::text::{
    READ_TOOL_MAX_BYTES, WRITE_TOOL_MAX_BYTES, atomic_write, detect_line_ending, normalize_to_lf,
    restore_line_endings, strip_bom,
};

pub(super) const DESCRIPTION: &str = "Edit a file by replacing text. The oldText must match a unique region; matching is exact but normalizes line endings, Unicode spaces/quotes/dashes, and ignores trailing whitespace.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
            "oldText": { "type": "string", "minLength": 1, "description": "Text to find and replace (must match uniquely; matching normalizes line endings, Unicode spaces/quotes/dashes, and ignores trailing whitespace)" },
            "newText": { "type": "string", "description": "New text to replace the old text with" }
        },
        "required": ["path", "oldText", "newText"]
    })
}

pub(super) fn execute(root: &Path, args: &Value) -> Result<String> {
    let input: EditInput = serde_json::from_value(args.clone())
        .context("edit tool arguments must include path, oldText, newText")?;
    edit(root, &input)
}

pub(super) fn preview(root: &Path, args: &Value) -> Preview {
    let input: EditInput = match serde_json::from_value(args.clone()) {
        Ok(input) => input,
        Err(_) => return Preview::Malformed,
    };
    match build_edit(root, &input) {
        Ok(plan) => Preview::Available {
            path: input.path,
            old: plan.old_content,
            new: plan.new_content,
        },
        Err(error) => Preview::Unavailable(format!("{error:#}")),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EditInput {
    path: String,
    old_text: String,
    new_text: String,
}

fn edit(root: &Path, input: &EditInput) -> Result<String> {
    let plan = build_edit(root, input)?;
    atomic_write(&plan.resolved, plan.new_content.as_bytes())
        .with_context(|| format!("failed to write {}", input.path))?;

    Ok(format!("Successfully replaced text in {}.", input.path))
}

struct EditPlan {
    resolved: std::path::PathBuf,
    old_content: String,
    new_content: String,
}

fn build_edit(root: &Path, input: &EditInput) -> Result<EditPlan> {
    if input.new_text.len() > WRITE_TOOL_MAX_BYTES {
        bail!("new text exceeds maximum allowed size");
    }
    let resolved = resolve_existing(root, &input.path)?;
    let metadata =
        fs::metadata(&resolved).with_context(|| format!("file not found: {}", input.path))?;
    if !metadata.is_file() {
        bail!("path {} is not a regular file", input.path);
    }
    if metadata.len() > READ_TOOL_MAX_BYTES {
        bail!("file is too large to edit");
    }

    let raw = fs::read(&resolved).with_context(|| format!("failed to read {}", input.path))?;
    let raw_content = String::from_utf8(raw)
        .context("file contains invalid UTF-8 and cannot be safely edited as text")?;

    let (content_no_bom, had_bom) = strip_bom(&raw_content);
    let original_ending = detect_line_ending(content_no_bom);
    let normalized_content = normalize_to_lf(content_no_bom);
    let normalized_old = normalize_to_lf(&input.old_text);

    if normalized_old.is_empty() {
        bail!("the old text cannot be empty");
    }

    let (match_start, match_len) =
        locate_unique_match(&normalized_content, &normalized_old, &input.path)?;

    // Build the replacement against the LF-normalized content, then restore the
    // file's original line ending on write.
    let normalized_new = normalize_to_lf(&input.new_text);
    let mut new_content =
        String::with_capacity(normalized_content.len() - match_len + normalized_new.len());
    new_content.push_str(&normalized_content[..match_start]);
    new_content.push_str(&normalized_new);
    new_content.push_str(&normalized_content[match_start + match_len..]);

    if new_content == normalized_content {
        bail!(
            "no changes made to {}; replacement produced identical content",
            input.path
        );
    }

    let restored = restore_line_endings(&new_content, original_ending);
    let final_content = if had_bom {
        format!("\u{FEFF}{restored}")
    } else {
        restored
    };

    Ok(EditPlan {
        resolved,
        old_content: raw_content,
        new_content: final_content,
    })
}

/// Find `needle` in `haystack`, requiring a unique match. Tries exact match
/// first, then a whitespace/Unicode-punctuation-tolerant match (mirroring pi's
/// edit normalization: Unicode spaces/quotes/dashes folded, trailing
/// whitespace per line ignored). Returns the byte range in `haystack`.
fn locate_unique_match(haystack: &str, needle: &str, path: &str) -> Result<(usize, usize)> {
    let exact = count_and_first(haystack, needle);
    match exact {
        (0, _) => {}
        (1, Some(start)) => return Ok((start, needle.len())),
        (n, _) => bail!("found {n} occurrences of the text in {path}; it must be unique"),
    }

    // Fuzzy fallback over normalized text with an offset map back to the
    // original byte positions.
    let (norm_hay, map) = normalize_for_fuzzy(haystack);
    let (norm_needle, _) = normalize_for_fuzzy(needle);
    if norm_needle.is_empty() {
        bail!("could not find the exact text in {path}; the old text must match exactly");
    }
    let (count, first) = count_and_first(&norm_hay, &norm_needle);
    match (count, first) {
        (0, _) => bail!("could not find the exact text in {path}; the old text must match exactly"),
        (1, Some(norm_start)) => {
            let norm_end = norm_start + norm_needle.len();
            let orig_start = map[norm_start];
            let orig_end = map[norm_end];
            Ok((orig_start, orig_end - orig_start))
        }
        (n, _) => bail!("found {n} occurrences of the text in {path}; it must be unique"),
    }
}

/// Count non-overlapping occurrences and report the first byte index.
fn count_and_first(haystack: &str, needle: &str) -> (usize, Option<usize>) {
    if needle.is_empty() {
        return (0, None);
    }
    let mut count = 0;
    let mut first = None;
    let mut search_from = 0;
    while let Some(rel) = haystack[search_from..].find(needle) {
        let abs = search_from + rel;
        if first.is_none() {
            first = Some(abs);
        }
        count += 1;
        search_from = abs + needle.len();
    }
    (count, first)
}

/// Build a normalized string plus a map from each normalized byte offset to the
/// originating byte offset in `input`. The map has `normalized.len() + 1`
/// entries; the final entry is the original length.
fn normalize_for_fuzzy(input: &str) -> (String, Vec<usize>) {
    // First pass: per-character normalization with origin offsets, normalizing
    // line endings to LF.
    let mut chars: Vec<(char, usize)> = Vec::new();
    let bytes = input.as_bytes();
    let mut iter = input.char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        let mapped = if ch == '\r' {
            // CRLF or lone CR collapses to a single LF.
            if iter.peek().map(|&(_, c)| c) == Some('\n') {
                iter.next();
            }
            '\n'
        } else if is_unicode_space(ch) {
            ' '
        } else if matches!(ch, '\u{2018}' | '\u{2019}') {
            '\''
        } else if matches!(ch, '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}') {
            '"'
        } else if matches!(
            ch,
            '\u{2010}'
                | '\u{2011}'
                | '\u{2012}'
                | '\u{2013}'
                | '\u{2014}'
                | '\u{2015}'
                | '\u{2212}'
        ) {
            '-'
        } else {
            ch
        };
        chars.push((mapped, idx));
    }
    let total_len = bytes.len();

    // Second pass: drop trailing whitespace before each LF and at end of input.
    let mut keep = vec![true; chars.len()];
    let mut run_is_trailing = true;
    for i in (0..chars.len()).rev() {
        let (ch, _) = chars[i];
        if ch == '\n' {
            run_is_trailing = true;
        } else if ch.is_whitespace() && run_is_trailing {
            keep[i] = false;
        } else {
            run_is_trailing = false;
        }
    }

    let mut out = String::with_capacity(input.len());
    let mut map: Vec<usize> = Vec::with_capacity(input.len() + 1);
    for (i, (ch, origin)) in chars.iter().enumerate() {
        if !keep[i] {
            continue;
        }
        let start = out.len();
        out.push(*ch);
        for _ in start..out.len() {
            map.push(*origin);
        }
    }
    map.push(total_len);
    (out, map)
}

fn is_unicode_space(c: char) -> bool {
    matches!(
        c,
        '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    ) || (c.is_whitespace() && c != '\n' && c != '\r' && c != '\t' && c != ' ')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::test_support::{root_of, temp_dir};

    #[test]
    fn edit_replaces_unique_text() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("d.txt"), "one\ntwo\nthree\n").unwrap();
        edit(
            &root,
            &EditInput {
                path: "d.txt".into(),
                old_text: "two".into(),
                new_text: "TWO".into(),
            },
        )
        .unwrap();
        let content = fs::read_to_string(dir.path.join("d.txt")).unwrap();
        assert_eq!(content, "one\nTWO\nthree\n");
    }

    #[test]
    fn edit_rejects_ambiguous_match() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("e.txt"), "dup\ndup\n").unwrap();
        let err = edit(
            &root,
            &EditInput {
                path: "e.txt".into(),
                old_text: "dup".into(),
                new_text: "x".into(),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unique"));
    }
}
