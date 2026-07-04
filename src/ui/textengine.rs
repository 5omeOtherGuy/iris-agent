//! Unified text engine: the single source of truth for the Iris TUI's display
//! width, ANSI/OSC/APC parsing, and width-aware wrap / truncate / slice.
//!
//! Modeled conceptually on pi-mono `packages/tui/src/utils.ts` (consulted, not
//! ported). It collapses three previously divergent implementations:
//!   * `tui/wrap.rs` width/wrap/truncate (was char-based)
//!   * `tui/text.rs` ANSI stripper (`strip_ansi_for_text` + CSI/OSC/APC consume)
//!   * `ui/text.rs` ANSI stripper + `visible_width` (the latter counted
//!     `chars().count()`, miscounting CJK/emoji -- fixed here).
//!
//! Width is measured by grapheme cluster so emoji ZWJ / VS16 / flag / combining
//! sequences are never split across a wrap or truncate boundary. `unicode-width`
//! 0.2 already returns the correct *cluster* width for a whole grapheme (ZWJ
//! sequences -> 2, combining mark + base -> base width); the engine's job is to
//! iterate by grapheme and keep clusters intact.
//!
//! ANSI handling: one parser recognizes CSI (`ESC [ ... final`), OSC / DCS / PM
//! / APC / SOS string controls (`ESC ] / P / ^ / _ / X ... BEL|ST`), and the
//! 8-bit C1 forms. The ANSI-aware wrap/truncate/slice carry SGR state across
//! physical rows and reopen OSC 8 hyperlinks on each row so a wrapped link stays
//! clickable -- a capability the ratatui span path cannot express (it drops OSC
//! 8 entirely). Emitting clickable links is a deferred UI feature; the engine
//! only *preserves* them.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------
// Width
// ---------------------------------------------------------------------------

/// Display width of `text` as the terminal renders it, summed over grapheme
/// clusters. ANSI escapes are NOT stripped (they are measured as their literal
/// width); callers that need the post-strip width use [`visible_width`]. This
/// matches the historical `wrap.rs::display_width` (raw `UnicodeWidthStr`),
/// which several call sites rely on as a conservative over-estimate (e.g. the
/// tool-output flood guard).
pub(crate) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Display width of a single grapheme cluster.
pub(crate) fn cluster_width(cluster: &str) -> usize {
    UnicodeWidthStr::width(cluster)
}

/// Cluster width clamped to at least 1, so a zero-width / control cluster still
/// advances a wrap loop and never spins forever. Mirrors the old `char_width`.
pub(crate) fn cluster_advance(cluster: &str) -> usize {
    cluster_width(cluster).max(1)
}

/// Visible width after removing ANSI/OSC/APC escape sequences. This is the
/// correct measure for strings that may contain escapes; it replaces the buggy
/// `ui/text.rs::visible_width` which used `chars().count()`.
pub(crate) fn visible_width(text: &str) -> usize {
    // Fast path: with no ESC (0x1b) and no possible C1 lead byte (0xc2 prefixes
    // U+0080..U+00BF, which covers every C1 introducer), there are no escape
    // sequences to strip. With no tabs, control chars contribute 0 to
    // UnicodeWidthStr either way, so this is identical to stripping first --
    // without the allocation.
    if text.bytes().all(|b| b != 0x1b && b != 0xc2 && b != b'\t') {
        return display_width(text);
    }
    display_width(&clean_text(text))
}

// ---------------------------------------------------------------------------
// ANSI / OSC / APC parsing
// ---------------------------------------------------------------------------

/// Remove ANSI/OSC/APC escape sequences but keep every other character
/// (including bare control characters such as TAB). General-purpose strip used
/// where the caller wants the raw text minus styling.
pub(crate) fn strip_ansi(input: &str) -> String {
    transform(input, false)
}

/// Remove ANSI/OSC/APC escape sequences AND remaining control characters,
/// producing a clean display string. Byte-for-byte compatible with the historic
/// `tui/text.rs::strip_ansi_for_text` (the `\r`-splitting / footer-cleaning
/// path).
pub(crate) fn clean_text(input: &str) -> String {
    transform(&expand_tabs(input), true)
}

/// Expand TAB characters to standard 8-column tab stops, measured in display
/// columns from the start of each logical line. ANSI/OSC/APC escapes are copied
/// through without advancing the column so callers can expand before stripping
/// or parsing SGR styling.
pub(crate) fn expand_tabs(input: &str) -> String {
    if !input.contains('\t') {
        return input.to_string();
    }
    const TAB_STOP: usize = 8;

    let mut out = String::with_capacity(input.len());
    let mut col = 0usize;
    let mut pos = 0usize;
    while pos < input.len() {
        if let Some(len) = ansi_sequence_len_at(input, pos) {
            out.push_str(&input[pos..pos + len]);
            pos += len;
            continue;
        }

        let rest = &input[pos..];
        let Some(cluster) = rest.graphemes(true).next() else {
            break;
        };
        match cluster {
            "\t" => {
                let spaces = TAB_STOP - (col % TAB_STOP);
                out.push_str(&" ".repeat(spaces));
                col += spaces;
            }
            "\n" => {
                out.push('\n');
                col = 0;
            }
            _ => {
                out.push_str(cluster);
                if !cluster.chars().any(char::is_control) {
                    col += cluster_width(cluster);
                }
            }
        }
        pos += cluster.len();
    }
    out
}

fn ansi_sequence_len_at(s: &str, pos: usize) -> Option<usize> {
    if pos >= s.len() {
        return None;
    }
    let rest = &s[pos..];
    let mut iter = rest.char_indices();
    let (_, intro) = iter.next()?;
    match intro {
        '\x1b' => {
            let (_, second) = iter.next()?;
            match second {
                '[' => ansi_csi_len(rest),
                ']' | 'P' | '^' | '_' | 'X' => ansi_string_control_len(rest),
                _ => Some(rest.char_indices().nth(2).map_or(rest.len(), |(i, _)| i)),
            }
        }
        '\u{009b}' => ansi_csi_len(rest),
        '\u{009d}' | '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => {
            ansi_string_control_len(rest)
        }
        _ => None,
    }
}

fn ansi_csi_len(rest: &str) -> Option<usize> {
    let mut chars = rest.char_indices();
    let (_, first) = chars.next()?;
    if first == '\x1b' {
        chars.next();
    }
    for (i, ch) in chars {
        if ('\u{40}'..='\u{7e}').contains(&ch) {
            return Some(i + ch.len_utf8());
        }
    }
    Some(rest.len())
}

fn ansi_string_control_len(rest: &str) -> Option<usize> {
    let mut chars = rest.char_indices().peekable();
    chars.next()?;
    if rest.starts_with('\x1b') {
        chars.next();
    }
    while let Some((i, ch)) = chars.next() {
        if ch == '\u{7}' || ch == '\u{009c}' {
            return Some(i + ch.len_utf8());
        }
        if ch == '\x1b' && matches!(chars.peek(), Some((_, '\\'))) {
            let (j, st) = chars.next()?;
            return Some(j + st.len_utf8());
        }
    }
    Some(rest.len())
}

fn transform(input: &str, drop_controls: bool) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    consume_csi(&mut chars);
                }
                Some(']' | 'P' | '^' | '_' | 'X') => {
                    chars.next();
                    consume_string_control(&mut chars);
                }
                // ESC + any other single byte (e.g. ESC M): drop both.
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            // 8-bit C1 CSI introducer.
            '\u{009b}' => consume_csi(&mut chars),
            // 8-bit C1 string-control introducers: OSC, DCS, SOS, PM, APC.
            '\u{009d}' | '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => {
                consume_string_control(&mut chars);
            }
            _ if ch.is_control() => {
                if !drop_controls {
                    out.push(ch);
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

fn consume_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for ch in chars.by_ref() {
        // CSI final byte is in 0x40..=0x7e.
        if ('\u{40}'..='\u{7e}').contains(&ch) {
            break;
        }
    }
}

fn consume_string_control(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(ch) = chars.next() {
        // Terminators: BEL, 8-bit C1 ST (U+009C), or 7-bit ST (ESC \).
        if ch == '\u{7}' || ch == '\u{009c}' {
            break;
        }
        if ch == '\x1b' && matches!(chars.peek(), Some('\\')) {
            chars.next();
            break;
        }
    }
}

// The single ANSI/OSC/APC escape scanner (`ansi_sequence_len_at` + helpers,
// used by `expand_tabs`) lives above in this module.

// ---------------------------------------------------------------------------
// Plain (escape-free) truncate / wrap
// ---------------------------------------------------------------------------

/// Truncate `text` to at most `max` characters (grapheme-cluster boundary safe),
/// no ellipsis. Mirrors the historic `wrap.rs::truncate_chars` but never splits
/// a cluster.
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    let mut out = String::new();
    for (count, cluster) in text.graphemes(true).enumerate() {
        if count >= max {
            break;
        }
        out.push_str(cluster);
    }
    out
}

/// Truncate `text` to at most `max` terminal columns (display width), stopping on
/// a grapheme-cluster boundary so wide/emoji/combining clusters are kept whole.
pub(crate) fn truncate_to_width(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for cluster in text.graphemes(true) {
        let w = cluster_width(cluster);
        if used + w > max {
            break;
        }
        out.push_str(cluster);
        used += w;
    }
    out
}

/// Greedy word-wrap `text` to `width` display columns, breaking at spaces. A
/// word that fits is moved whole onto its own row (so a URL/path stays
/// selectable as one unit); a word longer than the width hard-breaks at grapheme
/// boundaries. Returns at least one row (possibly empty). Grapheme-aware
/// successor to `wrap.rs::wrap_to_width`.
pub(crate) fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 || display_width(text) <= width {
        return vec![text.to_string()];
    }
    let leading_spaces = text.bytes().take_while(|byte| *byte == b' ').count();
    if leading_spaces > 0 {
        let rest = &text[leading_spaces..];
        let mut rows = Vec::new();
        let mut remaining = leading_spaces;
        while remaining >= width {
            rows.push(" ".repeat(width));
            remaining -= width;
        }
        if rest.is_empty() {
            if remaining > 0 {
                rows.push(" ".repeat(remaining));
            }
            return rows;
        }
        let first_width = width.saturating_sub(remaining).max(1);
        let mut rest_rows = wrap_to_width(rest, first_width);
        if remaining > 0
            && let Some(first) = rest_rows.first_mut()
        {
            first.insert_str(0, &" ".repeat(remaining));
        }
        rows.extend(rest_rows);
        return rows;
    }
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for (i, word) in text.split(' ').enumerate() {
        let word_w = display_width(word);
        if i > 0 && !cur.is_empty() && cur_w + 1 + word_w <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + word_w;
            continue;
        }
        if !cur.is_empty() {
            rows.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if word_w <= width {
            cur.push_str(word);
            cur_w = word_w;
        } else {
            for cluster in word.graphemes(true) {
                let cw = cluster_advance(cluster);
                // Guard `!cur.is_empty()` so a single cluster wider than the
                // whole width never emits a phantom blank row before itself.
                if cur_w + cw > width && !cur.is_empty() {
                    rows.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push_str(cluster);
                cur_w += cw;
            }
        }
    }
    rows.push(cur);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_width_matches_grapheme_sum_for_wide_and_emoji() {
        assert_eq!(display_width("hello"), 5);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("e\u{301}"), 1); // combining acute
        assert_eq!(display_width("😀"), 2);
        assert_eq!(display_width("👨\u{200d}👩\u{200d}👧"), 2); // ZWJ family
        assert_eq!(display_width("🇺🇸"), 2); // flag
    }

    #[test]
    fn visible_width_strips_ansi_and_counts_display_columns() {
        // The historic bug: chars().count() returned 4 for "中文" inside SGR.
        assert_eq!(visible_width("\x1b[31m中文\x1b[0m"), 4);
        assert_eq!(visible_width("\x1b[1mhi\x1b[0m"), 2);
        // OSC 8 hyperlink wrapper contributes no width.
        assert_eq!(
            visible_width("\x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\"),
            4
        );
    }

    #[test]
    fn strip_ansi_keeps_controls_clean_text_drops_them() {
        assert_eq!(strip_ansi("\x1b[31ma\tb\x1b[0m"), "a\tb");
        assert_eq!(clean_text("\x1b[31ma\tb\x1b[0m"), "a       b");
        assert_eq!(clean_text("1234567\tb"), "1234567 b");
        // 8-bit C1 CSI and bracketed-paste markers are removed.
        assert_eq!(strip_ansi("\u{009b}31mx"), "x");
        assert_eq!(strip_ansi("\x1b[200~hi\x1b[201~"), "hi");
    }

    #[test]
    fn strip_ansi_handles_osc_with_st_and_bel() {
        assert_eq!(strip_ansi("\x1b]8;;https://a\x07txt\x1b]8;;\x07"), "txt");
        assert_eq!(strip_ansi("\x1b]0;title\x1b\\body"), "body");
        // 8-bit C1 ST (U+009C) also terminates a string control instead of
        // swallowing the following visible text.
        assert_eq!(strip_ansi("\x1b]0;title\u{009c}body"), "body");
    }

    #[test]
    fn truncate_to_width_keeps_clusters_whole() {
        assert_eq!(truncate_to_width("hello", 3), "hel");
        assert_eq!(truncate_to_width("中文字", 3), "中"); // 2 cols fit, next is 2
        // A combining mark stays attached to its base.
        assert_eq!(truncate_to_width("e\u{301}x", 1), "e\u{301}");
        // ZWJ emoji is kept whole or dropped, never split.
        assert_eq!(truncate_to_width("😀x", 2), "😀");
        assert_eq!(truncate_to_width("😀x", 1), "");
    }

    #[test]
    fn truncate_chars_counts_clusters() {
        assert_eq!(truncate_chars("abcdef", 3), "abc");
        assert_eq!(truncate_chars("a😀b", 2), "a😀");
    }

    #[test]
    fn wrap_to_width_breaks_at_spaces_and_hard_breaks_long_words() {
        assert_eq!(
            wrap_to_width("alpha beta gamma", 11),
            vec!["alpha beta", "gamma"]
        );
        assert_eq!(wrap_to_width("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap_to_width("short", 80), vec!["short"]);
    }

    #[test]
    fn wrap_to_width_preserves_leading_spaces_on_first_wrapped_row() {
        let rows = wrap_to_width("  indented prompt wraps", 12);

        assert!(rows[0].starts_with("  "), "{rows:?}");
        assert!(rows.iter().all(|row| display_width(row) <= 12), "{rows:?}");
    }

    #[test]
    fn wrap_to_width_keeps_emoji_clusters_intact() {
        // Two double-width emoji in a 2-col field must land on separate rows,
        // never split mid-cluster.
        let rows = wrap_to_width("😀😀", 2);
        assert_eq!(rows, vec!["😀", "😀"]);
    }
}
