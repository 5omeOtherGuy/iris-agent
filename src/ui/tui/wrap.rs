//! Ratatui span-layout helpers for the TUI pane: padding, span clipping, and
//! line wrapping. All width / truncation / plain-text wrap math is delegated to
//! the unified [`crate::ui::textengine`] so there is a single grapheme-aware
//! source of truth (this module no longer measures width itself).

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use super::{MAX_TOOL_OUTPUT_LINE_CHARS, PANEL_BODY_CHROME_WIDTH, dim_style};
use crate::ui::textengine::cluster_advance;
// Re-exported so existing `super::wrap::{display_width, ...}` imports keep
// resolving while the implementations live in the engine.
pub(crate) use crate::ui::textengine::wrap_to_width;
pub(super) use crate::ui::textengine::{display_width, truncate_chars, truncate_to_width};

/// Clamp one logical tool-output line so it wraps to at most `max_rows` physical
/// rows at `width` (accounting for panel body chrome), appending an ellipsis
/// when content is dropped. This keeps the head/tail fold a HARD physical-row cap
/// even when a single line (e.g. a minified blob) would otherwise wrap to far
/// more rows than its slice budget.
pub(super) fn clamp_output_line(raw: &str, width: usize, max_rows: usize) -> String {
    let line = truncate_chars(raw, MAX_TOOL_OUTPUT_LINE_CHARS);
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
        .map(|span| span.content.as_ref())
        .collect()
}

pub(super) fn truncate_line(line: &mut Line<'static>, max: usize) {
    let max = max.max(1);
    let mut used = 0;
    let mut spans = Vec::new();
    for span in std::mem::take(&mut line.spans) {
        if used >= max {
            break;
        }
        let content = truncate_to_width(span.content.as_ref(), max - used);
        used += display_width(&content);
        if !content.is_empty() {
            spans.push(Span::styled(content, span.style));
        }
    }
    line.spans = spans;
}

pub(super) fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

pub(super) fn take_spans_to_width(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut used = 0usize;
    let mut out = Vec::new();
    for span in spans {
        if used >= max {
            break;
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
    {
        last.content.to_mut().push(ch);
        return;
    }
    spans.push(Span::styled(ch.to_string(), style));
}

fn push_span_str(spans: &mut Vec<Span<'static>>, cluster: &str, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
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

    for span in &line.spans {
        for cluster in span.content.graphemes(true) {
            let cw = cluster_advance(cluster);
            if cur_w > 0 && cur_w + cw > width {
                out.push(Line::from(std::mem::take(&mut spans)));
                cur_w = 0;
                if let Some(prefix) = continuation_prefix {
                    let prefix = truncate_to_width(prefix, width.saturating_sub(1));
                    if !prefix.is_empty() {
                        cur_w = display_width(&prefix);
                        spans.push(Span::styled(prefix, dim_style()));
                    }
                }
            }
            push_span_str(&mut spans, cluster, span.style);
            cur_w += cw;
        }
    }

    out.push(Line::from(spans));
}

fn styled_physical_row(
    cells: &[(char, Style)],
    cursor: &mut usize,
    physical: &str,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for rc in physical.chars() {
        while *cursor < cells.len() && cells[*cursor].0 != rc {
            *cursor += 1;
        }
        let style = cells.get(*cursor).map_or(Style::default(), |(_, st)| *st);
        push_span_char(&mut spans, rc, style);
        *cursor += 1;
    }
    Line::from(spans)
}

pub(super) fn push_wrapped_line_wordwise_with_prefix(
    line: &Line<'static>,
    width: usize,
    continuation_prefix: &'static str,
    out: &mut Vec<Line<'static>>,
) {
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
        .collect();
    if cells.is_empty() {
        out.push(Line::default());
        return;
    }
    let text: String = cells.iter().map(|(ch, _)| *ch).collect();
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

        let content_text: String = content_cells.iter().map(|(ch, _)| *ch).collect();
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
    let remainder: String = cells[cursor..].iter().map(|(ch, _)| *ch).collect();
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
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
        .collect();
    if cells.is_empty() {
        out.push(Line::default());
        return;
    }
    let text: String = cells.iter().map(|(ch, _)| *ch).collect();
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
    let remainder = text
        .strip_prefix(first)
        .unwrap_or_default()
        .strip_prefix(' ')
        .unwrap_or_default();
    for physical in wrap_to_width(remainder, continuation_width) {
        if !physical.is_empty() {
            out.push(Line::from(vec![
                Span::styled(prefix, dim_style()),
                Span::styled(physical, style),
            ]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{display_width, wrap_to_width};

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
}
