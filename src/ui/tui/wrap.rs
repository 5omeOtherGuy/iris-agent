//! Width, truncation, and wrapping helpers for the TUI pane.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{MAX_TOOL_OUTPUT_LINE_CHARS, PANEL_BODY_CHROME_WIDTH, dim_style};

/// Display width of a string as the terminal renders it, reused for word-wrap.
/// Control chars count as zero (they are not emitted).
pub(super) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Display width of a single char, clamped to at least 1 so a zero-width or
/// control char still advances the wrap and never loops forever.
fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0).max(1)
}

/// Truncate `text` to at most `max` characters (on a char boundary).
pub(super) fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

/// Truncate `text` to at most `max` terminal columns (display width), stopping on
/// a char boundary. Unlike [`truncate_chars`], this accounts for wide/CJK glyphs
/// so the result never exceeds `max` columns.
pub(super) fn truncate_to_width(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for c in text.chars() {
        let w = char_width(c);
        if used + w > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out
}

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
        for ch in span.content.chars() {
            let cw = char_width(ch);
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
            push_span_char(&mut spans, ch, span.style);
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
            pad_line_left(&mut line, prefix_width);
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

/// Greedy word-wrap `text` to `width` display columns, breaking at spaces. A
/// word that fits is moved whole onto its own row rather than split mid-token,
/// so a URL/path that fits within the width stays selectable as one unit; a
/// single word longer than the width still hard-breaks, because the row-exact
/// rendering model (one logical row = one physical row, see [`TuiUi::draw`])
/// cannot emit an over-wide row without the terminal clipping its tail.
/// Returns at least one row (possibly empty) so a blank logical line still
/// occupies a row.
pub(crate) fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 || display_width(text) <= width {
        return vec![text.to_string()];
    }
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;
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
            for ch in word.chars() {
                let cw = char_width(ch);
                // Guard `!cur.is_empty()` so a single glyph wider than the
                // whole width never emits a phantom blank row before itself.
                if cur_w + cw > width && !cur.is_empty() {
                    rows.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push(ch);
                cur_w += cw;
            }
        }
    }
    rows.push(cur);
    rows
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
