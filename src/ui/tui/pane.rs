//! Pane-level display row helpers for natural-language transcript text.
//!
//! This is a child module of `ui::tui`, so it can build retained transcript rows
//! without exposing pane rendering policy outside the Iris CLI/TUI layer.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::ui::markdown::{LineClass, MarkdownTheme, render_markdown_classified};

use super::panel_style;
use super::rows::{FoldVis, Measure, TranscriptRow};
use super::wrap::line_text;

/// The 2-cell gutter every transcript turn hangs its body under. It holds the
/// user's `›` marker on the first line of a user turn; the agent's gutter is
/// blank (the agent is the transcript's default voice and speaks unmarked). The
/// body column is the same for both — one shared text column at the gutter's
/// right edge.
pub(super) const TEXT_GUTTER: &str = "  ";

/// Columns the markdown table layout may use: the agent content area minus the
/// leading gutter that `assistant_row` prepends, so a full-width table line plus
/// its gutter still fits the render width.
fn markdown_width(content_width: usize) -> usize {
    content_width.saturating_sub(TEXT_GUTTER.len()).max(1)
}

pub(super) fn push_assistant_rows(rows: &mut Vec<TranscriptRow>, width: usize, text: &str) {
    rows.extend(assistant_rows(text, width));
}

/// Build assistant transcript rows for `text` laid out at the assistant content
/// column `content_width`. Shared by the committed-history path
/// ([`push_assistant_rows`]) and the live stream controller so a streamed line
/// renders identically whether it is in the mutable tail or committed to
/// scrollback.
pub(super) fn assistant_rows(text: &str, content_width: usize) -> Vec<TranscriptRow> {
    let theme = MarkdownTheme::default()
        .with_code_highlighting()
        .with_hyperlinks();
    let lines = render_markdown_classified(text, &theme, markdown_width(content_width));
    let mut rows = Vec::new();
    push_assistant_markdown_lines(&mut rows, lines);
    rows
}

pub(super) fn push_user_rows(rows: &mut Vec<TranscriptRow>, text: &str) {
    // The `›` marks the turn, not every line: only the first non-empty line
    // carries it, so a multi-line ask reads as one marked block hanging under
    // one marker.
    let mut marked = false;
    for line in text.split('\n') {
        let show_marker = !marked && !line.trim().is_empty();
        marked |= show_marker;
        rows.push(user_row(line, show_marker));
    }
}

fn user_row(text: &str, show_marker: bool) -> TranscriptRow {
    if text.is_empty() {
        return TranscriptRow::new(String::new(), panel_style());
    }
    // `›` in ink+bold is the transcript's one scannable anchor (the eye jumps
    // to "what did I ask?"); the body hangs under it on the shared text column.
    // Monochrome-safe: marker + position carry it, not color.
    let (lead, lead_style) = if show_marker {
        (
            format!("{} ", crate::ui::symbols::USER),
            panel_style().add_modifier(Modifier::BOLD),
        )
    } else {
        (TEXT_GUTTER.to_string(), panel_style())
    };
    let line = Line::from(vec![
        Span::styled(lead, lead_style),
        Span::styled(text.to_string(), panel_style()),
    ]);
    // A user turn body is prose — it wraps at the reader's measure (spec §3).
    marker_row(text.to_string(), line, Measure::Prose)
}

fn push_assistant_markdown_lines(
    rows: &mut Vec<TranscriptRow>,
    lines: Vec<(Line<'static>, LineClass)>,
) {
    for (line, class) in lines {
        rows.push(assistant_row(line, class));
    }
}

/// The agent speaks unmarked: its rendered markdown line hangs on the shared
/// text column under a blank gutter, no `›` (the user's turn is the marked one,
/// docs/TUI_DESIGN_LANGUAGE.md §7). Prose lines wrap at the reader's measure;
/// fenced/indented code and tables stay full width (spec §3).
fn assistant_row(mut line: Line<'static>, class: LineClass) -> TranscriptRow {
    let text = line_text(&line);
    line.spans
        .insert(0, Span::styled(TEXT_GUTTER, Style::default()));
    let measure = match class {
        LineClass::Prose => Measure::Prose,
        LineClass::Mechanical => Measure::Mechanical,
    };
    marker_row(text, line, measure)
}

/// A conversation row: `text` is the searchable content (no gutter) and `line`
/// is the same content with its 2-cell gutter already prepended, laid out on the
/// shared text column and wrapping under the gutter. `measure` selects prose
/// (reader's measure) vs mechanical (full width) text wrapping.
fn marker_row(text: String, line: Line<'static>, measure: Measure) -> TranscriptRow {
    TranscriptRow {
        text,
        style: panel_style(),
        continuation_prefix: Some(TEXT_GUTTER),
        line: Some(line),
        fold: FoldVis::Always,
        word_wrap: true,
        background: None,
        hrule: false,
        chrome: None,
        searchable: true,
        measure,
    }
}
