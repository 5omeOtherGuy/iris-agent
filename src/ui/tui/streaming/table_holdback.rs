// SPDX-License-Identifier: Apache-2.0
// Derived from codex-rs/tui/src/streaming/table_holdback.rs and
// codex-rs/tui/src/table_detect.rs (OpenAI Codex, Apache-2.0).
// Changes from upstream: Codex holds back only pipe-table regions because its
// agent markdown renderer preserves source newlines as hard line breaks, which
// keeps every other rendered-line prefix stable. Iris renders a markdown
// SoftBreak as a space (src/ui/markdown.rs), so a bare '\n' does NOT make a
// rendered-line prefix stable. This scanner therefore generalizes the holdback:
// it returns the largest source prefix consisting only of *closed* markdown
// blocks (paragraphs/tables/quotes/headings closed by a blank line, fenced code
// closed by its fence, lists closed by a following non-list line). A pipe table
// is just one contiguous non-blank block, so this subsumes Codex's dedicated
// table holdback while also fixing paragraph/list reflow on Iris's renderer.

//! Commit-boundary scanner: how much accumulated source is safe to commit to
//! scrollback without a later delta reflowing an already-committed row.
//!
//! [`safe_commit_end`] returns a byte offset `end` such that
//! `render(source[..end])` is guaranteed to be a prefix of
//! `render(source[..longer])` for any longer complete source. Everything at or
//! after `end` is the still-mutable region (the active tail), held until the
//! block closes or the stream finalizes.

/// The kind of block currently open at the scan cursor. Tables, blockquotes,
/// headings, and HTML runs are all treated as `Para`: a contiguous run of
/// non-blank lines that closes on a blank line.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    /// Between blocks: the cursor sits on a committable boundary.
    None,
    /// A paragraph-like run (also tables/quotes/headings) held until a blank.
    Para,
    /// A list, held until a following non-indented, non-item line (loose lists
    /// keep interior blank lines, so a blank never closes a list).
    List,
    /// A fenced code block, held until its closing fence.
    Code,
}

/// Return the largest byte offset up to which `source` is safe to commit.
///
/// `source` is expected to contain only newline-terminated lines (the caller
/// feeds it from a newline-gated collector); any trailing partial line is
/// ignored and left for finalize.
pub(super) fn safe_commit_end(source: &str) -> usize {
    let mut fence: Option<(char, usize)> = None;
    let mut block = Block::None;
    let mut offset = 0usize;
    let mut safe_end = 0usize;

    for line in source.split_inclusive('\n') {
        if !line.ends_with('\n') {
            // Trailing partial line: never committable here.
            break;
        }
        let line_len = line.len();
        let raw = &line[..line_len - 1];
        let trimmed = raw.trim();
        let is_blank = trimmed.is_empty();
        let indented = raw.starts_with(' ') || raw.starts_with('\t');

        if let Some(open) = fence {
            if is_fence_close(trimmed, open) {
                fence = None;
                block = Block::None;
                safe_end = offset + line_len;
            }
            offset += line_len;
            continue;
        }

        if let Some(open) = fence_open(trimmed) {
            fence = Some(open);
            if block == Block::None {
                block = Block::Code;
            }
            // If a fence opens inside a held Para/List run, keep holding: the
            // whole run commits together once the fence closes and a boundary
            // is reached.
            offset += line_len;
            continue;
        }

        match block {
            Block::None => {
                if is_blank {
                    safe_end = offset + line_len;
                } else if is_list_item(raw) {
                    block = Block::List;
                } else {
                    block = Block::Para;
                }
            }
            Block::Para => {
                if is_blank {
                    block = Block::None;
                    safe_end = offset + line_len;
                }
            }
            Block::List => {
                if is_blank || is_list_item(raw) || indented {
                    // Interior blank lines and continuations keep the list open
                    // (loose vs tight is decided by the whole list).
                } else {
                    // A non-indented, non-item line proves the list ended at the
                    // preceding boundary; commit everything before this line.
                    safe_end = offset;
                    block = Block::Para;
                }
            }
            Block::Code => {}
        }
        offset += line_len;
    }

    if block == Block::None {
        safe_end = offset;
    }
    safe_end
}

/// Detect an opening code fence: a line whose trimmed text starts with three or
/// more of the same fence character (backtick or tilde). Returns the fence
/// character and the run length.
fn fence_open(trimmed: &str) -> Option<(char, usize)> {
    let ch = trimmed.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let run = trimmed.chars().take_while(|&c| c == ch).count();
    (run >= 3).then_some((ch, run))
}

/// Detect a closing code fence: a line whose trimmed text is only the fence
/// character repeated at least as many times as the opening run.
fn is_fence_close(trimmed: &str, open: (char, usize)) -> bool {
    let (ch, open_len) = open;
    if trimmed.is_empty() {
        return false;
    }
    let run = trimmed.chars().take_while(|&c| c == ch).count();
    run >= open_len && trimmed.chars().all(|c| c == ch)
}

/// Whether a raw line begins a list item (bullet or ordered), allowing leading
/// indentation.
fn is_list_item(raw: &str) -> bool {
    let t = raw.trim_start();
    let bytes = t.as_bytes();
    match bytes.first() {
        Some(b'-') | Some(b'*') | Some(b'+') => {
            matches!(bytes.get(1), None | Some(b' ') | Some(b'\t'))
        }
        Some(c) if c.is_ascii_digit() => {
            let mut i = 0;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            // Cap the marker length so a long numeric line is not mistaken for
            // an ordered-list item.
            if i == 0 || i > 9 {
                return false;
            }
            matches!(bytes.get(i), Some(b'.') | Some(b')'))
                && matches!(bytes.get(i + 1), None | Some(b' ') | Some(b'\t'))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_paragraph_is_held_until_blank() {
        // A single unterminated paragraph line commits nothing (it may merge
        // with the next line under Iris's soft-break-as-space rendering).
        assert_eq!(safe_commit_end("Hello\n"), 0);
        assert_eq!(safe_commit_end("Hello\nworld\n"), 0);
        // A blank line closes the paragraph -> the whole thing is committable.
        assert_eq!(
            safe_commit_end("Hello\nworld\n\n"),
            "Hello\nworld\n\n".len()
        );
    }

    #[test]
    fn paragraph_boundary_commits_prefix_only() {
        let src = "First para.\n\nSecond para in progress\n";
        // Only the first paragraph + its blank separator are committable.
        assert_eq!(safe_commit_end(src), "First para.\n\n".len());
    }

    #[test]
    fn table_is_held_until_its_terminating_blank() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        // No trailing blank: the whole table stays in the tail (offset 0).
        assert_eq!(safe_commit_end(src), 0);
        let closed = "| a | b |\n|---|---|\n| 1 | 2 |\n\n";
        // A trailing blank closes the table; it commits as one complete unit.
        assert_eq!(safe_commit_end(closed), closed.len());
    }

    #[test]
    fn table_after_paragraph_holds_only_the_table() {
        let src = "intro\n\n| a | b |\n|---|---|\n";
        // The intro paragraph commits; the incomplete table is held.
        assert_eq!(safe_commit_end(src), "intro\n\n".len());
    }

    #[test]
    fn list_is_held_until_a_following_non_list_line() {
        let src = "- one\n- two\n";
        assert_eq!(safe_commit_end(src), 0, "open list held");
        let loose = "- one\n\n- two\n";
        assert_eq!(safe_commit_end(loose), 0, "interior blank does not close");
        let terminated = "- one\n- two\n\nAfter\n";
        // The list commits once a non-indented, non-item line proves it ended.
        assert_eq!(safe_commit_end(terminated), "- one\n- two\n\n".len());
    }

    #[test]
    fn ordered_list_detected() {
        let terminated = "1. one\n2. two\n\nAfter\n";
        assert_eq!(safe_commit_end(terminated), "1. one\n2. two\n\n".len());
    }

    #[test]
    fn indented_continuation_keeps_list_open() {
        let src = "- one\n\n  still item one\n\nAfter\n";
        assert_eq!(safe_commit_end(src), "- one\n\n  still item one\n\n".len());
    }

    #[test]
    fn fenced_code_block_held_until_close() {
        let open = "```rust\nlet x = 1;\n";
        assert_eq!(safe_commit_end(open), 0, "unterminated fence held");
        let closed = "```rust\nlet x = 1;\n```\n";
        assert_eq!(
            safe_commit_end(closed),
            closed.len(),
            "closed fence commits"
        );
    }

    #[test]
    fn pipe_line_inside_fence_is_not_a_table_boundary() {
        // A blank line would normally close a Para, but inside a fence it must
        // not; the whole fenced block commits together on close.
        let src = "```\n| not | a table |\n\n| still | code |\n```\n";
        assert_eq!(safe_commit_end(src), src.len());
    }

    #[test]
    fn trailing_partial_line_is_never_committed() {
        // No trailing newline on the last line.
        let src = "First.\n\nSecond partial";
        assert_eq!(safe_commit_end(src), "First.\n\n".len());
    }

    #[test]
    fn heading_run_after_paragraph_is_conservatively_held_together() {
        // Conservative: held as one run until a blank line, always correct.
        let src = "para\n# Heading\n\n";
        assert_eq!(safe_commit_end(src), src.len());
    }
}
