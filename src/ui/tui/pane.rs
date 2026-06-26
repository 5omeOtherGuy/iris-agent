//! Pane-level display row helpers for natural-language transcript text.
//!
//! This is a child module of `ui::tui`, so it can build retained transcript rows
//! without exposing pane rendering policy outside the Iris CLI/TUI layer.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::ui::markdown::{MarkdownTheme, render_markdown_themed};

use super::rows::{FoldVis, TranscriptRow};
use super::transcript::streaming_markdown_preview;
use super::wrap::line_text;
use super::{TEXT_COLUMN_X_PADDING, panel_style, prompt_style};

pub(super) const ASSISTANT_TEXT_PREFIX: &str = "  ";

/// Columns the markdown table layout may use: the assistant content area minus
/// the leading marker/continuation prefix that `assistant_row` prepends, so a
/// full-width table line plus its prefix still fits the render width.
fn markdown_width(content_width: usize) -> usize {
    content_width
        .saturating_sub(ASSISTANT_TEXT_PREFIX.len())
        .max(1)
}

/// The assistant content column for a given full frame width, matching the
/// inset `TranscriptRow::render` applies (`width - 2 * TEXT_COLUMN_X_PADDING`).
/// Used by the streaming path, which is handed the full frame width.
fn content_width(frame_width: usize) -> usize {
    frame_width
        .saturating_sub(TEXT_COLUMN_X_PADDING.saturating_mul(2))
        .max(1)
}

pub(super) fn push_assistant_rows(rows: &mut Vec<TranscriptRow>, width: usize, text: &str) {
    let theme = MarkdownTheme::default();
    let lines = render_markdown_themed(text, &theme, markdown_width(width));
    for (index, line) in lines.into_iter().enumerate() {
        rows.push(assistant_row(line, index == 0));
    }
}

/// Build the transient transcript rows for the in-flight streamed assistant
/// text. The transcript composites these through the shared `Component` path
/// after committed history, then commits them once on `AssistantTextEnd`.
pub(super) fn streaming_assistant_rows(text: &str, width: usize) -> Vec<TranscriptRow> {
    let text = streaming_markdown_preview(text);
    let theme = MarkdownTheme::default();
    // `width` is the full frame here; reduce to the assistant content column
    // so table layout matches the width these rows are rendered into.
    render_markdown_themed(&text, &theme, markdown_width(content_width(width)))
        .into_iter()
        .enumerate()
        .map(|(index, line)| assistant_row(line, index == 0))
        .collect()
}

pub(super) fn push_user_rows(rows: &mut Vec<TranscriptRow>, text: &str) {
    for line in text.split('\n') {
        rows.push(user_row(line));
    }
}

fn user_row(text: &str) -> TranscriptRow {
    if text.is_empty() {
        return TranscriptRow::new(String::new(), panel_style());
    }

    TranscriptRow {
        text: text.to_string(),
        style: panel_style(),
        continuation_prefix: Some(ASSISTANT_TEXT_PREFIX),
        line: None,
        fold: FoldVis::Always,
        word_wrap: true,
        background: None,
        hrule: false,
        chrome: None,
    }
}

fn assistant_row(mut line: Line<'static>, first: bool) -> TranscriptRow {
    let text = line_text(&line);
    if first {
        line.spans.insert(0, Span::styled("● ", prompt_style()));
    } else {
        line.spans
            .insert(0, Span::styled(ASSISTANT_TEXT_PREFIX, Style::default()));
    }

    TranscriptRow {
        text,
        style: panel_style(),
        continuation_prefix: Some(ASSISTANT_TEXT_PREFIX),
        line: Some(line),
        fold: FoldVis::Always,
        word_wrap: true,
        background: None,
        hrule: false,
        chrome: None,
    }
}
