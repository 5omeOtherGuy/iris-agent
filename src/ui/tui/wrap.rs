//! Ratatui span-layout helpers for the TUI pane: padding, span clipping, and
//! line wrapping. All width / truncation / plain-text wrap math is delegated to
//! the unified [`crate::ui::textengine`] so there is a single grapheme-aware
//! source of truth (this module no longer measures width itself).

use std::rc::Rc;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use super::{MAX_TOOL_OUTPUT_LINE_CHARS, PANEL_BODY_CHROME_WIDTH, dim_style};
use crate::ui::hyperlink;
use crate::ui::textengine::cluster_advance;

/// One visible character with its style and the OPEN marker (if any) it falls
/// under. Both link and file-ref markers are folded into this per-char
/// annotation as their full marker bytes, so the word-wise wrappers wrap clean
/// visible text (markers never enter width math) yet can re-emit the exact same
/// marker pair -- preserving its kind -- on every physical row.
type LinkCell = (char, Style, Option<Rc<str>>);

/// Flatten a line's spans into [`LinkCell`]s, dropping the zero-width marker
/// spans and tagging every enclosed visible char with the active OPEN marker.
fn link_cells(line: &Line<'static>) -> Vec<LinkCell> {
    let mut cells = Vec::new();
    let mut current: Option<Rc<str>> = None;
    for span in &line.spans {
        let content = span.content.as_ref();
        if hyperlink::open_marker_uri(content).is_some() {
            current = Some(Rc::from(content));
            continue;
        }
        if hyperlink::is_close(content) {
            current = None;
            continue;
        }
        for ch in content.chars() {
            cells.push((ch, span.style, current.clone()));
        }
    }
    cells
}

fn link_eq(a: &Option<Rc<str>>, b: &Option<Rc<str>>) -> bool {
    a.as_deref() == b.as_deref()
}
// Re-exported so existing `super::wrap::{display_width, ...}` imports keep
// resolving while the implementations live in the engine.
pub(crate) use crate::ui::textengine::wrap_to_width;
pub(super) use crate::ui::textengine::{
    display_width, ellipsize_to_width, truncate_clusters_with_ellipsis, truncate_to_width,
};

/// Clamp one logical tool-output line so it wraps to at most `max_rows` physical
/// rows at `width` (accounting for panel body chrome), appending an ellipsis
/// when content is dropped. This keeps the head/tail fold a HARD physical-row cap
/// even when a single line (e.g. a minified blob) would otherwise wrap to far
/// more rows than its slice budget.
pub(super) fn clamp_output_line(raw: &str, width: usize, max_rows: usize) -> String {
    let line = truncate_clusters_with_ellipsis(raw, MAX_TOOL_OUTPUT_LINE_CHARS);
    let usable = width.saturating_sub(PANEL_BODY_CHROME_WIDTH).max(1);
    let max_cols = usable.saturating_mul(max_rows.max(1));
    if display_width(&line) <= max_cols {
        return line;
    }
    format!(
        "{}\u{2026}",
        truncate_to_width(&line, max_cols.saturating_sub(1))
    )
}

/// Estimate the physical rows a tool-output line occupies after wrapping to
/// `width`, accounting for the panel borders and body padding. At least one row. ANSI
/// escapes are counted as visible width (a conservative over-estimate that only
/// makes the flood cap trip slightly earlier), which is fine for a guard.
pub(super) fn wrapped_row_estimate(line: &str, width: usize) -> usize {
    let usable = width.saturating_sub(PANEL_BODY_CHROME_WIDTH).max(1);
    display_width(line).div_ceil(usable).max(1)
}

pub(super) fn pad_line_left(line: &mut Line<'static>, padding: usize) {
    if padding > 0 {
        line.spans
            .insert(0, Span::styled(" ".repeat(padding), line.style));
    }
}

pub(super) fn pad_line_right(line: &mut Line<'static>, padding: usize) {
    if padding > 0 {
        line.spans
            .push(Span::styled(" ".repeat(padding), line.style));
    }
}

pub(super) fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .filter(|span| !hyperlink::is_marker(span.content.as_ref()))
        .map(|span| span.content.as_ref())
        .collect()
}

pub(super) fn truncate_line(line: &mut Line<'static>, max: usize) {
    let max = max.max(1);
    let mut used = 0;
    let mut spans = Vec::new();
    for span in std::mem::take(&mut line.spans) {
        // Zero-width link markers are kept regardless of the width budget so a
        // truncated link still serializes a complete OSC 8 pair.
        if hyperlink::is_marker(span.content.as_ref()) {
            spans.push(span);
            continue;
        }
        if used >= max {
            continue;
        }
        let content = truncate_to_width(span.content.as_ref(), max - used);
        used += display_width(&content);
        if !content.is_empty() {
            spans.push(Span::styled(content, span.style));
        }
    }
    line.spans = spans;
}

/// Fit a styled line to `max` columns and disclose any removed suffix with the
/// house ellipsis. The cut remains span- and grapheme-safe; zero-width
/// hyperlink markers survive through [`truncate_line`].
pub(super) fn ellipsize_line(line: &mut Line<'static>, max: usize) {
    if display_width(&line_text(line)) <= max {
        return;
    }
    if max == 0 {
        line.spans.clear();
        return;
    }
    let style = line
        .spans
        .iter()
        .rev()
        .find(|span| !hyperlink::is_marker(span.content.as_ref()))
        .map_or(line.style, |span| span.style);
    if max == 1 {
        line.spans = vec![Span::styled("\u{2026}", style)];
        return;
    }
    truncate_line(line, max - 1);
    line.spans.push(Span::styled("\u{2026}", style));
}

pub(super) fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .filter(|span| !hyperlink::is_marker(span.content.as_ref()))
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

pub(super) fn take_spans_to_width(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut used = 0usize;
    let mut out = Vec::new();
    for span in spans {
        if hyperlink::is_marker(span.content.as_ref()) {
            out.push(span);
            continue;
        }
        if used >= max {
            continue;
        }
        let content = truncate_to_width(span.content.as_ref(), max - used);
        used += display_width(&content);
        if !content.is_empty() {
            out.push(Span::styled(content, span.style));
        }
    }
    out
}

fn push_span_char(spans: &mut Vec<Span<'static>>, ch: char, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
        && !hyperlink::is_marker(last.content.as_ref())
    {
        last.content.to_mut().push(ch);
        return;
    }
    spans.push(Span::styled(ch.to_string(), style));
}

fn push_span_str(spans: &mut Vec<Span<'static>>, cluster: &str, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
        && !hyperlink::is_marker(last.content.as_ref())
    {
        last.content.to_mut().push_str(cluster);
        return;
    }
    spans.push(Span::styled(cluster.to_string(), style));
}

pub(super) fn push_wrapped_line(
    line: &Line<'static>,
    width: usize,
    continuation_prefix: Option<&'static str>,
    out: &mut Vec<Line<'static>>,
) {
    // Ponytail: ANSI rows keep color and hard-wrap only; upgrade to
    // span-aware word wrap when long colorized logs prove worth the extra code.
    let width = width.max(1);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0;
    // The OPEN marker currently active (its full bytes), carried across physical
    // rows so a marked run that wraps re-opens on each new row (and closes at the
    // end of the row it leaves), keeping every emitted row's marker pair
    // complete and preserving the marker kind (link vs file-ref).
    let mut open_marker: Option<Rc<str>> = None;

    for span in &line.spans {
        let content = span.content.as_ref();
        if hyperlink::open_marker_uri(content).is_some() {
            open_marker = Some(Rc::from(content));
            spans.push(Span::raw(content.to_string()));
            continue;
        }
        if hyperlink::is_close(content) {
            open_marker = None;
            spans.push(hyperlink::close_span());
            continue;
        }
        for cluster in content.graphemes(true) {
            let cw = cluster_advance(cluster);
            if cur_w > 0 && cur_w + cw > width {
                if open_marker.is_some() {
                    spans.push(hyperlink::close_span());
                }
                out.push(Line::from(std::mem::take(&mut spans)));
                cur_w = 0;
                if let Some(prefix) = continuation_prefix {
                    let prefix = truncate_to_width(prefix, width.saturating_sub(1));
                    if !prefix.is_empty() {
                        cur_w = display_width(&prefix);
                        spans.push(Span::styled(prefix, dim_style()));
                    }
                }
                if let Some(marker) = &open_marker {
                    spans.push(Span::raw((**marker).to_string()));
                }
            }
            push_span_str(&mut spans, cluster, span.style);
            cur_w += cw;
        }
    }

    out.push(Line::from(spans));
}

fn styled_physical_row(cells: &[LinkCell], cursor: &mut usize, physical: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur_link: Option<Rc<str>> = None;
    for rc in physical.chars() {
        while *cursor < cells.len() && cells[*cursor].0 != rc {
            *cursor += 1;
        }
        let (style, link) = cells
            .get(*cursor)
            .map(|(_, style, link)| (*style, link.clone()))
            .unwrap_or((Style::default(), None));
        // Emit an OSC 8 marker transition when the link target changes. A row
        // that starts inside a link opens it here; the trailing close below
        // makes the row self-contained so a wrapped link re-opens per row.
        if !link_eq(&link, &cur_link) {
            if cur_link.is_some() {
                spans.push(hyperlink::close_span());
            }
            if let Some(marker) = &link {
                spans.push(Span::raw((**marker).to_string()));
            }
            cur_link = link;
        }
        push_span_char(&mut spans, rc, style);
        *cursor += 1;
    }
    if cur_link.is_some() {
        spans.push(hyperlink::close_span());
    }
    Line::from(spans)
}

pub(super) fn push_wrapped_line_wordwise_with_prefix(
    line: &Line<'static>,
    width: usize,
    continuation_prefix: &'static str,
    out: &mut Vec<Line<'static>>,
) {
    let cells = link_cells(line);
    if cells.is_empty() {
        out.push(Line::default());
        return;
    }
    let text: String = cells.iter().map(|(ch, _, _)| *ch).collect();
    let width = width.max(1);

    if text.starts_with(continuation_prefix) {
        let prefix_chars = continuation_prefix.chars().count();
        let prefix_width = display_width(continuation_prefix);
        let content_width = width.saturating_sub(prefix_width).max(1);
        let content_cells = &cells[prefix_chars..];
        if content_cells.is_empty() {
            out.push(Line::from(Span::styled(continuation_prefix, dim_style())));
            return;
        }

        let content_text: String = content_cells.iter().map(|(ch, _, _)| *ch).collect();
        let mut cursor = 0usize;
        for physical in wrap_to_width(&content_text, content_width) {
            let mut line = styled_physical_row(content_cells, &mut cursor, &physical);
            // Re-emit the prefix (the dim reasoning rail) on every physical
            // line so the rail runs the full height of the block, instead of
            // silently padding it away with spaces.
            line.spans
                .insert(0, Span::styled(continuation_prefix, dim_style()));
            out.push(line);
        }
        return;
    }

    let first = wrap_to_width(&text, width)
        .into_iter()
        .next()
        .unwrap_or_default();
    let mut cursor = 0usize;
    out.push(styled_physical_row(&cells, &mut cursor, &first));
    if cursor < cells.len() && cells[cursor].0 == ' ' {
        cursor += 1;
    }
    if cursor >= cells.len() {
        return;
    }
    let continuation_width = width
        .saturating_sub(display_width(continuation_prefix))
        .max(1);
    let remainder: String = cells[cursor..].iter().map(|(ch, _, _)| *ch).collect();
    for physical in wrap_to_width(&remainder, continuation_width) {
        if physical.is_empty() {
            continue;
        }
        let mut line = styled_physical_row(&cells, &mut cursor, &physical);
        pad_line_left(&mut line, display_width(continuation_prefix));
        out.push(line);
        if cursor < cells.len() && cells[cursor].0 == ' ' {
            cursor += 1;
        }
    }
}

/// Word-aware, URL-safe wrap of a styled line, preserving each span's style.
/// Reuses [`wrap_to_width`] for the break positions (so it breaks at spaces and
/// keeps long URL/path tokens intact), then re-applies styles by walking the
/// original chars in parallel. Used for Markdown prose; ANSI/gutter rows keep
/// the char-hard-wrap [`push_wrapped_line`] so their leading-space prefixes are
/// never collapsed.
pub(super) fn push_wrapped_line_wordwise(
    line: &Line<'static>,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let cells = link_cells(line);
    if cells.is_empty() {
        out.push(Line::default());
        return;
    }
    let text: String = cells.iter().map(|(ch, _, _)| *ch).collect();
    let mut cursor = 0;
    for physical in wrap_to_width(&text, width.max(1)) {
        out.push(styled_physical_row(&cells, &mut cursor, &physical));
    }
}

pub(super) fn push_wrapped_row_with_prefix(
    text: &str,
    style: Style,
    width: usize,
    prefix: &'static str,
    out: &mut Vec<Line<'static>>,
) {
    let prefix_width = display_width(prefix);
    let content_width = width.saturating_sub(prefix_width).max(1);
    for physical in wrap_to_width(text, content_width) {
        let mut spans = Vec::new();
        if !prefix.is_empty() {
            spans.push(Span::styled(prefix, dim_style()));
        }
        if !physical.is_empty() {
            spans.push(Span::styled(physical, style));
        }
        out.push(Line::from(spans));
    }
}

pub(super) fn push_wrapped_row(
    text: &str,
    style: Style,
    width: usize,
    continuation_prefix: Option<&'static str>,
    out: &mut Vec<Line<'static>>,
) {
    let rows = wrap_to_width(text, width);
    let Some(first) = rows.first() else {
        return;
    };
    out.push(Line::from(Span::styled(first.clone(), style)));
    let Some(prefix) = continuation_prefix else {
        for physical in rows.into_iter().skip(1) {
            out.push(Line::from(Span::styled(physical, style)));
        }
        return;
    };
    if first.is_empty() {
        return;
    }

    let continuation_width = width.saturating_sub(display_width(prefix)).max(1);
    let remainder = continuation_remainder(text, first);
    for physical in wrap_to_width(remainder, continuation_width) {
        if !physical.is_empty() {
            out.push(Line::from(vec![
                Span::styled(prefix, dim_style()),
                Span::styled(physical, style),
            ]));
        }
    }
}

fn continuation_remainder<'a>(text: &'a str, first: &str) -> &'a str {
    if first.is_empty() {
        return text;
    }
    if let Some(rest) = text.strip_prefix(first) {
        return rest.strip_prefix(' ').unwrap_or(rest);
    }
    if let Some(start) = text.find(first) {
        let rest = &text[start + first.len()..];
        return rest.strip_prefix(' ').unwrap_or(rest);
    }
    debug_assert!(
        false,
        "first wrapped row should be recoverable from original text"
    );
    text
}

#[cfg(test)]
mod tests {
    use super::{
        continuation_remainder, display_width, line_text, push_wrapped_line,
        push_wrapped_line_wordwise, push_wrapped_line_wordwise_with_prefix, wrap_to_width,
    };
    use crate::ui::hyperlink;
    use crate::ui::markdown::{MarkdownTheme, render_markdown_themed};
    use ratatui::text::{Line, Span};

    /// The visible text of a line, markers excluded (mirrors `line_text`).
    fn visible(line: &Line<'static>) -> String {
        line_text(line)
    }

    /// Count of OSC 8 OPEN and CLOSE markers on a wrapped physical row.
    fn marker_counts(line: &Line<'static>) -> (usize, usize) {
        let opens = line
            .spans
            .iter()
            .filter(|s| hyperlink::marker_uri(s.content.as_ref()).is_some())
            .count();
        let closes = line
            .spans
            .iter()
            .filter(|s| hyperlink::is_close(s.content.as_ref()))
            .count();
        (opens, closes)
    }

    #[test]
    fn markers_are_zero_width_in_wrap_math() {
        // A line whose only difference is an embedded link marker pair must wrap
        // to the SAME physical rows (same visible text) as the bare line: the
        // no-escapes-in-width-math invariant.
        let bare = Line::from("alpha beta gamma delta");
        let linked = Line::from(hyperlink::link_spans(
            "https://x.dev",
            vec![Span::raw("alpha beta gamma delta")],
        ));
        for width in [6usize, 11, 22] {
            let mut bare_rows = Vec::new();
            let mut linked_rows = Vec::new();
            push_wrapped_line_wordwise(&bare, width, &mut bare_rows);
            push_wrapped_line_wordwise(&linked, width, &mut linked_rows);
            let bare_text: Vec<String> = bare_rows.iter().map(visible).collect();
            let linked_text: Vec<String> = linked_rows.iter().map(visible).collect();
            assert_eq!(
                bare_text, linked_text,
                "width {width}: visible text diverged"
            );
        }
    }

    #[test]
    fn wordwise_wrap_reopens_a_link_across_physical_rows() {
        // A link whose label wraps must re-open on each physical row so every
        // row carries a self-contained OSC 8 marker pair.
        let line = Line::from(hyperlink::link_spans(
            "https://x.dev",
            vec![Span::raw("alpha beta gamma")],
        ));
        let mut rows = Vec::new();
        push_wrapped_line_wordwise(&line, 6, &mut rows);
        assert!(rows.len() >= 2, "expected the link label to wrap: {rows:?}");
        for row in &rows {
            let (opens, closes) = marker_counts(row);
            assert_eq!(opens, 1, "row missing single open marker: {row:?}");
            assert_eq!(closes, 1, "row missing single close marker: {row:?}");
        }
    }

    #[test]
    fn wordwise_prefix_wrap_reopens_a_link_across_rows() {
        // The markdown assistant path uses the prefixed word-wise wrapper.
        let prefix = "  ";
        let mut spans = vec![Span::raw(prefix)];
        spans.extend(hyperlink::link_spans(
            "https://x.dev",
            vec![Span::raw("alpha beta gamma")],
        ));
        let line = Line::from(spans);
        let mut rows = Vec::new();
        push_wrapped_line_wordwise_with_prefix(&line, 8, prefix, &mut rows);
        assert!(rows.len() >= 2, "expected wrap: {rows:?}");
        for row in &rows {
            let (opens, closes) = marker_counts(row);
            assert_eq!(opens, 1, "row missing open marker: {row:?}");
            assert_eq!(closes, 1, "row missing close marker: {row:?}");
        }
    }

    #[test]
    fn span_preserving_wrap_reopens_a_link_across_rows() {
        let line = Line::from(hyperlink::link_spans(
            "https://x.dev",
            vec![Span::raw("abcdefghij")],
        ));
        let mut rows = Vec::new();
        push_wrapped_line(&line, 4, None, &mut rows);
        assert!(rows.len() >= 2, "expected hard-break wrap: {rows:?}");
        for row in &rows {
            let (opens, closes) = marker_counts(row);
            assert_eq!(opens, 1, "row missing open marker: {row:?}");
            assert_eq!(closes, 1, "row missing close marker: {row:?}");
        }
        // Reassembling the visible text across rows rebuilds the label.
        let joined: String = rows.iter().map(visible).collect();
        assert_eq!(joined, "abcdefghij");
    }

    #[test]
    fn highlighted_wide_glyph_code_wraps_within_display_width() {
        // A CJK string literal (each glyph 2 columns) inside a highlighted rust
        // block must wrap by display width, never char count: highlighting runs
        // before span wrap, so the span-aware wrapper still sees intact clusters
        // and no physical row exceeds the target width.
        let md = "```rust\nlet s = \"\u{4e2d}\u{6587}\u{4e2d}\u{6587}\u{4e2d}\u{6587}\";\n```";
        let theme = MarkdownTheme::default().with_code_highlighting();
        let lines = render_markdown_themed(md, &theme, 80);
        let code: Vec<_> = lines
            .iter()
            .filter(|l| line_text(l).contains('\u{4e2d}'))
            .collect();
        assert_eq!(code.len(), 1, "expected one highlighted code line");
        // Prove it was actually highlighted (a colored span), not the dim fallback.
        assert!(
            code[0].spans.iter().any(|s| s.style.fg.is_some()),
            "code line was not highlighted"
        );
        let width = 10;
        let mut out = Vec::new();
        for line in &code {
            push_wrapped_line(line, width, Some("    "), &mut out);
        }
        for row in &out {
            assert!(
                display_width(&line_text(row)) <= width,
                "wrapped highlighted row exceeds width {width}: {:?}",
                line_text(row)
            );
        }
    }

    #[test]
    fn wrap_breaks_long_line_at_spaces_and_hard_breaks_long_words() {
        assert_eq!(
            wrap_to_width("alpha beta gamma", 11),
            vec!["alpha beta".to_string(), "gamma".to_string()]
        );
        assert_eq!(
            wrap_to_width("abcdefgh", 3),
            vec!["abc".to_string(), "def".to_string(), "gh".to_string()]
        );
        assert_eq!(wrap_to_width("short", 80), vec!["short".to_string()]);
    }

    #[test]
    fn url_that_fits_moves_to_its_own_row_unbroken() {
        // A URL that fits within the width is flushed whole onto its own row
        // (not split mid-token), so it stays selectable as one unit.
        let url = "https://ex.com/p";
        let rows = wrap_to_width(&format!("see {url} now"), 16);
        assert!(
            rows.iter().any(|r| r == url),
            "url not kept whole: {rows:?}"
        );
        assert!(rows.iter().any(|r| r == "see"));
        assert!(rows.iter().any(|r| r == "now"));
    }

    #[test]
    fn over_long_url_hard_breaks_without_losing_characters() {
        // A URL longer than the width must hard-break: the row-exact model
        // cannot emit an over-wide row without the terminal clipping its tail,
        // so visibility wins over keeping it on one row. Every character
        // survives -- concatenating the fragments rebuilds the URL exactly.
        let url = "https://example.com/very/long/path/that/exceeds/the/width";
        let rows = wrap_to_width(url, 20);
        assert!(rows.len() > 1, "expected a hard-break: {rows:?}");
        assert_eq!(
            rows.concat(),
            url,
            "characters lost while wrapping: {rows:?}"
        );
        assert!(
            rows.iter().all(|r| display_width(r) <= 20),
            "a row exceeded the width (would clip on render): {rows:?}"
        );
    }

    #[test]
    fn long_plain_word_still_hard_breaks() {
        // A non-URL/path long token still hard-breaks (no overflow).
        assert_eq!(
            wrap_to_width("abcdefgh", 3),
            vec!["abc".to_string(), "def".to_string(), "gh".to_string()]
        );
    }

    #[test]
    fn continuation_remainder_fallback_preserves_text_when_first_is_not_prefix() {
        assert_eq!(
            continuation_remainder("  indented prompt wraps", "indented"),
            "prompt wraps"
        );
    }
}
