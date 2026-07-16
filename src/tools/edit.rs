//! `edit` — unique-match text replacement with line-ending/Unicode-tolerant
//! fuzzy fallback matching.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::nexus::ClassifiedError;

use super::path::resolve_existing;
use super::text::{
    READ_TOOL_MAX_BYTES, WRITE_TOOL_MAX_BYTES, atomic_write, detect_line_ending, normalize_to_lf,
    restore_line_endings, strip_bom,
};
use super::{ObservedFiles, Preview};

pub(super) const DESCRIPTION: &str = "Replace text in an existing file. A prior non-skim read must match its current contents. Unless `replace_all` is true, `old_string` must identify exactly one region.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file_path": { "type": "string", "description": "File path, relative or absolute." },
            "old_string": { "type": "string", "minLength": 1, "description": "Current text to replace." },
            "new_string": { "type": "string", "description": "Replacement text; must differ from old_string." },
            "replace_all": { "type": "boolean", "default": false, "description": "Replace every occurrence." }
        },
        "required": ["file_path", "old_string", "new_string"],
        "additionalProperties": false
    })
}

pub(super) fn execute(
    root: &Path,
    args: &Value,
    observed: &mut ObservedFiles,
) -> Result<super::ToolOutput> {
    let input: EditInput = Deserialize::deserialize(args)
        .context("edit tool arguments must include file_path, old_string, new_string")?;
    // Record the edited file for the compaction carry (ADR-0044).
    Ok(edit(root, &input, observed)?.with_workspace_target(root, &input.file_path))
}

pub(super) fn preview(root: &Path, args: &Value) -> Preview {
    let input: EditInput = match Deserialize::deserialize(args) {
        Ok(input) => input,
        Err(_) => return Preview::Malformed,
    };
    match build_edit(root, &input) {
        // Header uses the workspace-relative resolved path (not the raw,
        // possibly-absolute `file_path` arg) so it matches `write` and never
        // produces a `a//home/...` double slash.
        Ok(plan) => Preview::Available {
            path: super::path::relative_display(root, &plan.resolved),
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

fn edit(root: &Path, input: &EditInput, observed: &mut ObservedFiles) -> Result<super::ToolOutput> {
    let plan = build_edit(root, input)?;
    // `edit` only ever targets an existing file; require the agent to have seen
    // its current contents so a stale edit cannot silently clobber changes.
    if super::path::restrictions_enabled() {
        observed.ensure_fresh(&plan.resolved, plan.old_content.as_bytes())?;
    }
    atomic_write(&plan.resolved, plan.new_content.as_bytes())
        .with_context(|| format!("failed to write {}", input.file_path))?;
    observed.observe(&plan.resolved, plan.new_content.as_bytes());

    let occurrences = plan.replaced;
    let plural = if occurrences == 1 { "" } else { "s" };
    let mut message = format!(
        "Successfully replaced {occurrences} occurrence{plural} in {}.",
        input.file_path
    );
    // Conditional echo (ADR-0038): an exact-match success stays terse; when the
    // tolerant (fuzzy) fallback fired, append a compact snippet of the applied
    // region so the model's view of the file cannot drift silently. Failure
    // detail is never echoed on success (ADR-0036: success is cheap).
    if let Some(snippet) = &plan.applied_snippet {
        message.push_str("\nApplied region (tolerant match):\n");
        message.push_str(snippet);
    }
    // Failure-class telemetry (ADR-0038): record only the outcome class in
    // metadata (ADR-0021) so a transcript-level, per-model join can measure how
    // often each recovery path fires. Success is exact vs tolerant-match-fired;
    // the failure classes (not-found / not-unique / stale-file) ride their
    // actionable error text, which is the transcript record for a failed call.
    let outcome_class = if plan.used_fuzzy {
        "tolerant-match-fired"
    } else {
        "exact"
    };
    // Report the exact post-edit bytes so the dirty-tree guard can confirm an
    // approved edit against disk (ADR-0028 TOCTOU rule); Nexus strips this key
    // before it reaches provider context.
    Ok(super::ToolOutput::text(message)
        .with("occurrences", json!(occurrences))
        .with("edit_outcome", json!(outcome_class))
        .with(
            crate::nexus::WRITE_CONFIRM_HASH_KEY,
            json!(super::content_hash(plan.new_content.as_bytes())),
        ))
}

struct EditPlan {
    resolved: std::path::PathBuf,
    old_content: String,
    new_content: String,
    replaced: usize,
    /// True when the tolerant (fuzzy) fallback matched, i.e. the exact pass
    /// found nothing. Drives the conditional echo and telemetry class.
    used_fuzzy: bool,
    /// Compact, line-numbered snippet of the first applied region, present only
    /// when `used_fuzzy` so the conditional echo can show the model what the
    /// tolerant match actually changed.
    applied_snippet: Option<String>,
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

    let (ranges, used_fuzzy) = locate_matches(
        &normalized_content,
        &normalized_old,
        input.replace_all,
        &input.file_path,
    )?;

    // Build the replacement against the LF-normalized content, then restore the
    // file's original line ending on write. Ranges are non-overlapping and
    // ascending, so one left-to-right pass applies every replacement. Record the
    // byte span of the first replacement in the rebuilt content so a tolerant
    // match can echo the applied region.
    let normalized_new = normalize_to_lf(&input.new_string);
    let mut new_content =
        String::with_capacity(normalized_content.len() + ranges.len() * normalized_new.len());
    let mut cursor = 0;
    let mut first_change: Option<(usize, usize)> = None;
    for &(start, len) in &ranges {
        new_content.push_str(&normalized_content[cursor..start]);
        let change_start = new_content.len();
        new_content.push_str(&normalized_new);
        first_change.get_or_insert((change_start, new_content.len()));
        cursor = start + len;
    }
    new_content.push_str(&normalized_content[cursor..]);

    let applied_snippet = if used_fuzzy {
        first_change.map(|(start, end)| region_snippet(&new_content, start, end))
    } else {
        None
    };

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
        used_fuzzy,
        applied_snippet,
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
) -> Result<(Vec<(usize, usize)>, bool)> {
    let exact = find_all(haystack, needle);
    if !exact.is_empty() {
        let ranges = exact
            .into_iter()
            .map(|start| (start, needle.len()))
            .collect();
        return Ok((select(ranges, replace_all, path)?, false));
    }

    // Fuzzy fallback over normalized text with an offset map back to the
    // original byte positions.
    let (norm_hay, map) = normalize_for_fuzzy(haystack);
    let (norm_needle, _) = normalize_for_fuzzy(needle);
    if norm_needle.is_empty() {
        return Err(not_found_error(path, haystack, needle));
    }
    let fuzzy = find_all(&norm_hay, &norm_needle);
    if fuzzy.is_empty() {
        return Err(not_found_error(path, haystack, needle));
    }
    let ranges = fuzzy
        .into_iter()
        .map(|norm_start| {
            let orig_start = map[norm_start];
            let orig_end = map[norm_start + norm_needle.len()];
            (orig_start, orig_end - orig_start)
        })
        .collect();
    Ok((select(ranges, replace_all, path)?, true))
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
        n => Err(ClassifiedError::new(
            "not-unique",
            format!(
                "found {n} occurrences of the text in {path}; pass replace_all=true to replace \
                 all of them, or add surrounding context to old_string so it uniquely identifies \
                 one location"
            ),
        )
        .with("occurrences", json!(n))
        .into()),
    }
}

/// The `not-found` failure: the human-readable guidance stays exactly as
/// informative as before, including the closest-candidate region so the model
/// can re-anchor without re-reading the whole file (ADR-0036); the `not-found`
/// class rides as additive machine-readable metadata (ADR-0040).
fn not_found_error(path: &str, haystack: &str, needle: &str) -> anyhow::Error {
    let mut message = format!(
        "could not find the text in {path}. old_string must match the file's current contents \
         (line endings and Unicode spaces/quotes/dashes are normalized and trailing whitespace is \
         ignored, but indentation and other characters must match exactly). Re-read the file and \
         copy the exact text to replace."
    );
    // Failure detail is complete but compact (ADR-0036): point at the region of
    // the file that most resembles old_string so the model can re-anchor without
    // re-reading the whole file.
    if let Some((line, region)) = closest_candidate_region(haystack, needle) {
        message.push_str(&format!(
            "\nClosest matching region (around line {line}):\n{region}"
        ));
    }
    ClassifiedError::new("not-found", message).into()
}

/// A compact, line-numbered snippet of the content spanning the byte range
/// `[start, end)` plus two lines of surrounding context. Used to echo an
/// applied tolerant-match region back to the model.
fn region_snippet(content: &str, start: usize, end: usize) -> String {
    let start_line = content[..start.min(content.len())].matches('\n').count();
    let end_line = content[..end.min(content.len())].matches('\n').count();
    numbered_lines(content, start_line, end_line, 2)
}

/// Find the file line that most resembles the first non-blank line of `needle`
/// (by shared whitespace-delimited words) and return its 1-based line number
/// plus a small numbered snippet around it. `None` when nothing overlaps.
fn closest_candidate_region(content: &str, needle: &str) -> Option<(usize, String)> {
    let target = needle.lines().find(|line| !line.trim().is_empty())?.trim();
    let target_words: Vec<&str> = target.split_whitespace().collect();
    if target_words.is_empty() {
        return None;
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let mut best: Option<(usize, usize)> = None; // (score, line index)
    for (idx, line) in lines.iter().enumerate() {
        let score = line
            .split_whitespace()
            .filter(|word| target_words.contains(word))
            .count();
        if score > 0
            && best
                .map(|(best_score, _)| score > best_score)
                .unwrap_or(true)
        {
            best = Some((score, idx));
        }
    }
    let (_, idx) = best?;
    Some((idx + 1, numbered_lines(content, idx, idx, 2)))
}

/// Render lines `[from_line - context ..= to_line + context]` (0-based, clamped)
/// as `NNNN | text`, with over-long lines truncated. Shared by the tolerant-echo
/// snippet and the not-found closest-candidate region.
fn numbered_lines(content: &str, from_line: usize, to_line: usize, context: usize) -> String {
    const MAX_LINE_CHARS: usize = 200;
    let lines: Vec<&str> = content.split('\n').collect();
    let last = lines.len().saturating_sub(1);
    let from = from_line.saturating_sub(context);
    let to = (to_line + context).min(last);
    let mut out = String::new();
    for (offset, idx) in (from..=to).enumerate() {
        if offset > 0 {
            out.push('\n');
        }
        let text = lines.get(idx).copied().unwrap_or("");
        let shown: String = text.chars().take(MAX_LINE_CHARS).collect();
        let ellipsis = if text.chars().count() > MAX_LINE_CHARS {
            " ..."
        } else {
            ""
        };
        out.push_str(&format!("{:>4} | {shown}{ellipsis}", idx + 1));
    }
    out
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

    fn run_output(
        root: &Path,
        path: &str,
        old: &str,
        new: &str,
        replace_all: bool,
    ) -> Result<super::super::ToolOutput> {
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

    fn run(root: &Path, path: &str, old: &str, new: &str, replace_all: bool) -> Result<String> {
        run_output(root, path, old, new, replace_all).map(|output| output.content)
    }

    #[test]
    fn schema_encodes_unique_replacement_defaults() {
        let schema = parameters();
        assert_eq!(schema["properties"]["old_string"]["minLength"], 1);
        assert_eq!(schema["properties"]["replace_all"]["default"], false);
        assert!(
            schema["properties"]["file_path"]["description"]
                .as_str()
                .unwrap()
                .contains("relative or absolute")
        );
        assert!(DESCRIPTION.contains("prior non-skim read"));
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
    fn edit_exact_success_payload_has_no_file_content_and_exact_class() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(dir.path.join("d.txt"), "one\ntwo\nthree\n").unwrap();
        let out = run_output(&root, "d.txt", "two", "TWO", false).unwrap();
        // Exact-match success stays terse: one line, no echoed file content and
        // no applied-region snippet.
        assert_eq!(out.content, "Successfully replaced 1 occurrence in d.txt.");
        assert!(!out.content.contains("Applied region"), "{}", out.content);
        assert!(!out.content.contains("three"), "{}", out.content);
        // Telemetry class rides the metadata (ADR-0021), visible in the transcript.
        assert_eq!(
            out.metadata.get("edit_outcome").and_then(|v| v.as_str()),
            Some("exact")
        );
    }

    #[test]
    fn edit_tolerant_success_echoes_applied_region_and_class() {
        let dir = temp_dir();
        let root = root_of(&dir);
        // File uses ASCII quotes; old_string uses Unicode curly quotes, so the
        // exact pass misses and the tolerant (fuzzy) fallback fires.
        fs::write(dir.path.join("c.txt"), "let name = \"Iris\";\n").unwrap();
        let out = run_output(&root, "c.txt", "\u{201C}Iris\u{201D}", "\"IRIS\"", false).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path.join("c.txt")).unwrap(),
            "let name = \"IRIS\";\n"
        );
        // Conditional echo (ADR-0038): tolerant success carries a compact
        // snippet of the applied region so the model's file view cannot drift.
        assert!(out.content.contains("Applied region"), "{}", out.content);
        assert!(out.content.contains("IRIS"), "{}", out.content);
        assert_eq!(
            out.metadata.get("edit_outcome").and_then(|v| v.as_str()),
            Some("tolerant-match-fired")
        );
    }

    #[test]
    fn edit_not_found_includes_closest_candidate_region() {
        let dir = temp_dir();
        let root = root_of(&dir);
        fs::write(
            dir.path.join("p.txt"),
            "the quick brown fox\njumps over\nthe lazy dog\n",
        )
        .unwrap();
        let err = run(&root, "p.txt", "the quick brown cat", "x", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("could not find the text"), "{err}");
        // Failure carries the closest-candidate region so the model can re-anchor.
        assert!(err.contains("Closest matching region"), "{err}");
        assert!(err.contains("the quick brown fox"), "{err}");
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
