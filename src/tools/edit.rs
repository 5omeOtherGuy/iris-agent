//! `edit` — unique-match text replacement with line-ending/Unicode-tolerant
//! fuzzy fallback matching.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use super::path::resolve_existing;
use super::text::{
    READ_TOOL_MAX_BYTES, WRITE_TOOL_MAX_BYTES, atomic_write, detect_line_ending, normalize_to_lf,
    restore_line_endings, strip_bom,
};
use super::{ObservedFiles, Preview};

pub(super) const DESCRIPTION: &str = "Edit a file by replacing text. By default old_string must match a unique region; set replace_all=true to replace every occurrence. Matching is exact but normalizes line endings, Unicode spaces/quotes/dashes, and ignores trailing whitespace.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file_path": { "type": "string", "description": "The absolute path to the file to modify" },
            "old_string": { "type": "string", "description": "The text to replace" },
            "new_string": { "type": "string", "description": "The text to replace it with (must be different from old_string)" },
            "replace_all": { "type": "boolean", "description": "Replace all occurrences of old_string (default false)" }
        },
        "required": ["file_path", "old_string", "new_string"],
        "additionalProperties": false
    })
}

pub(super) fn execute(root: &Path, args: &Value, observed: &mut ObservedFiles) -> Result<String> {
    let input: EditInput = serde_json::from_value(args.clone())
        .context("edit tool arguments must include file_path, old_string, new_string")?;
    edit(root, &input, observed)
}

pub(super) fn preview(root: &Path, args: &Value) -> Preview {
    let input: EditInput = match serde_json::from_value(args.clone()) {
        Ok(input) => input,
        Err(_) => return Preview::Malformed,
    };
    match build_edit(root, &input) {
        Ok(plan) => Preview::Available {
            path: input.file_path,
            old: plan.old_content,
            new: plan.new_content,
        },
        Err(error) => Preview::Unavailable(format!("{error:#}")),
    }
}

#[derive(Debug, Deserialize)]
struct EditInput {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

fn edit(root: &Path, input: &EditInput, observed: &mut ObservedFiles) -> Result<String> {
    let plan = build_edit(root, input)?;
    // `edit` only ever targets an existing file; require the agent to have seen
    // its current contents so a stale edit cannot silently clobber changes.
    observed.ensure_fresh(&plan.resolved, plan.old_content.as_bytes())?;
    atomic_write(&plan.resolved, plan.new_content.as_bytes())
        .with_context(|| format!("failed to write {}", input.file_path))?;
    observed.observe(&plan.resolved, plan.new_content.as_bytes());

    let occurrences = plan.replaced;
    let plural = if occurrences == 1 { "" } else { "s" };
    Ok(format!(
        "Successfully replaced {occurrences} occurrence{plural} in {}.",
        input.file_path
    ))
}

struct EditPlan {
    resolved: std::path::PathBuf,
    old_content: String,
    new_content: String,
    replaced: usize,
}

fn build_edit(root: &Path, input: &EditInput) -> Result<EditPlan> {
    if input.new_string.len() > WRITE_TOOL_MAX_BYTES {
        bail!("new_string exceeds maximum allowed size");
    }
    let resolved = resolve_existing(root, &input.file_path)?;
    let metadata =
        fs::metadata(&resolved).with_context(|| format!("file not found: {}", input.file_path))?;
    if !metadata.is_file() {
        bail!("path {} is not a regular file", input.file_path);
    }
    if metadata.len() > READ_TOOL_MAX_BYTES {
        bail!("file is too large to edit");
    }

    let raw = fs::read(&resolved).with_context(|| format!("failed to read {}", input.file_path))?;
    let raw_content = String::from_utf8(raw)
        .context("file contains invalid UTF-8 and cannot be safely edited as text")?;

    let (content_no_bom, had_bom) = strip_bom(&raw_content);
    let original_ending = detect_line_ending(content_no_bom);
    let normalized_content = normalize_to_lf(content_no_bom);
    let normalized_old = normalize_to_lf(&input.old_string);

    if normalized_old.is_empty() {
        bail!("old_string cannot be empty");
    }

    let ranges = locate_matches(
        &normalized_content,
        &normalized_old,
        input.replace_all,
        &input.file_path,
    )?;

    // Build the replacement against the LF-normalized content, then restore the
    // file's original line ending on write. Ranges are non-overlapping and
    // ascending, so one left-to-right pass applies every replacement.
    let normalized_new = normalize_to_lf(&input.new_string);
    let mut new_content =
        String::with_capacity(normalized_content.len() + ranges.len() * normalized_new.len());
    let mut cursor = 0;
    for &(start, len) in &ranges {
        new_content.push_str(&normalized_content[cursor..start]);
        new_content.push_str(&normalized_new);
        cursor = start + len;
    }
    new_content.push_str(&normalized_content[cursor..]);

    if new_content == normalized_content {
        bail!(
            "no changes made to {}; replacement produced identical content",
            input.file_path
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
        replaced: ranges.len(),
    })
}

/// Locate the byte ranges in `haystack` to replace. Prefers exact matches; with
/// none, falls back to a whitespace/Unicode-punctuation-tolerant match
/// (mirroring pi's edit normalization: Unicode spaces/quotes/dashes folded,
/// trailing whitespace per line ignored). Without `replace_all` the match must
/// be unique; with it, every occurrence is returned. Ranges are non-overlapping
/// and ascending.
fn locate_matches(
    haystack: &str,
    needle: &str,
    replace_all: bool,
    path: &str,
) -> Result<Vec<(usize, usize)>> {
    let exact = find_all(haystack, needle);
    if !exact.is_empty() {
        let ranges = exact
            .into_iter()
            .map(|start| (start, needle.len()))
            .collect();
        return select(ranges, replace_all, path);
    }

    // Fuzzy fallback over normalized text with an offset map back to the
    // original byte positions.
    let (norm_hay, map) = normalize_for_fuzzy(haystack);
    let (norm_needle, _) = normalize_for_fuzzy(needle);
    if norm_needle.is_empty() {
        bail!("{}", not_found_message(path));
    }
    let fuzzy = find_all(&norm_hay, &norm_needle);
    if fuzzy.is_empty() {
        bail!("{}", not_found_message(path));
    }
    let ranges = fuzzy
        .into_iter()
        .map(|norm_start| {
            let orig_start = map[norm_start];
            let orig_end = map[norm_start + norm_needle.len()];
            (orig_start, orig_end - orig_start)
        })
        .collect();
    select(ranges, replace_all, path)
}

/// Apply the uniqueness policy: `replace_all` keeps every range; otherwise the
/// match must be unique, and an ambiguous match returns an actionable error.
fn select(
    ranges: Vec<(usize, usize)>,
    replace_all: bool,
    path: &str,
) -> Result<Vec<(usize, usize)>> {
    if replace_all {
        return Ok(ranges);
    }
    match ranges.len() {
        1 => Ok(ranges),
        n => bail!(
            "found {n} occurrences of the text in {path}; pass replace_all=true to replace all of \
             them, or add surrounding context to old_string so it uniquely identifies one location"
        ),
    }
}

fn not_found_message(path: &str) -> String {
    format!(
        "could not find the text in {path}. old_string must match the file's current contents \
         (line endings and Unicode spaces/quotes/dashes are normalized and trailing whitespace is \
         ignored, but indentation and other characters must match exactly). Re-read the file and \
         copy the exact text to replace."
    )
}

/// All non-overlapping occurrences of `needle` in `haystack`, as ascending byte
/// indices.
fn find_all(haystack: &str, needle: &str) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut positions = Vec::new();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(needle) {
        let abs = from + rel;
        positions.push(abs);
        from = abs + needle.len();
    }
    positions
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

    fn run(root: &Path, path: &str, old: &str, new: &str, replace_all: bool) -> Result<String> {
        // The mechanics tests below exercise matching/replacement, not the
        // stale-file guard, so simulate a prior read of the existing file.
        let mut observed = ObservedFiles::new();
        if let Ok(bytes) = fs::read(root.join(path)) {
            observed.observe(&root.join(path), &bytes);
        }
        edit(
            root,
            &EditInput {
                file_path: path.into(),
                old_string: old.into(),
                new_string: new.into(),
                replace_all,
            },
            &mut observed,
        )
    }

    #[test]
    fn edit_without_prior_read_is_rejected() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("d.txt"), "one\ntwo\n").unwrap();
        let mut observed = ObservedFiles::new();
        let err = edit(
            &root,
            &EditInput {
                file_path: "d.txt".into(),
                old_string: "two".into(),
                new_string: "TWO".into(),
                replace_all: false,
            },
            &mut observed,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("has not been read this session"), "{err}");
        // File is untouched.
        assert_eq!(
            fs::read_to_string(dir.path.join("d.txt")).unwrap(),
            "one\ntwo\n"
        );
    }

    #[test]
    fn edit_on_stale_file_is_rejected() {
        let dir = temp_dir();
        let root = root_of(&dir);
        let path = dir.path.join("d.txt");
        fs::write(&path, "one\ntwo\n").unwrap();
        let mut observed = ObservedFiles::new();
        observed.observe(&path, b"one\ntwo\n");
        // The file changes on disk behind the agent's back.
        fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let err = edit(
            &root,
            &EditInput {
                file_path: "d.txt".into(),
                old_string: "two".into(),
                new_string: "TWO".into(),
                replace_all: false,
            },
            &mut observed,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("changed since it was last read"), "{err}");
    }

    #[test]
    fn edit_replaces_unique_text() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("d.txt"), "one\ntwo\nthree\n").unwrap();
        let msg = run(&root, "d.txt", "two", "TWO", false).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path.join("d.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
        assert!(msg.contains("1 occurrence"), "{msg}");
    }

    #[test]
    fn edit_rejects_ambiguous_match_and_suggests_replace_all() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("e.txt"), "dup\ndup\n").unwrap();
        let err = run(&root, "e.txt", "dup", "x", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("found 2 occurrences"), "{err}");
        assert!(err.contains("replace_all=true"), "{err}");
    }

    #[test]
    fn edit_replace_all_replaces_every_occurrence() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("f.txt"), "dup\ndup\ndup\n").unwrap();
        let msg = run(&root, "f.txt", "dup", "x", true).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path.join("f.txt")).unwrap(),
            "x\nx\nx\n"
        );
        assert!(msg.contains("3 occurrences"), "{msg}");
    }

    #[test]
    fn edit_replace_all_is_idempotent_single_match() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("h.txt"), "a\nb\nc\n").unwrap();
        let msg = run(&root, "h.txt", "b", "B", true).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path.join("h.txt")).unwrap(),
            "a\nB\nc\n"
        );
        assert!(msg.contains("1 occurrence"), "{msg}");
    }

    #[test]
    fn edit_not_found_error_is_actionable() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("g.txt"), "alpha\n").unwrap();
        let err = run(&root, "g.txt", "beta", "x", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("could not find the text"), "{err}");
        assert!(err.contains("Re-read the file"), "{err}");
    }
}
