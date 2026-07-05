//! Pane-level display row helpers for natural-language transcript text.
//!
//! This is a child module of `ui::tui`, so it can build retained transcript rows
//! without exposing pane rendering policy outside the Iris CLI/TUI layer.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::ui::markdown::{MarkdownTheme, render_markdown_themed};

use super::rows::{FoldVis, TranscriptRow};
use super::wrap::line_text;
use super::{dim_style, panel_style};

pub(super) const ASSISTANT_TEXT_PREFIX: &str = "  ";

/// Columns the markdown table layout may use: the assistant content area minus
/// the leading marker/continuation prefix that `assistant_row` prepends, so a
/// full-width table line plus its prefix still fits the render width.
fn markdown_width(content_width: usize) -> usize {
    content_width
        .saturating_sub(ASSISTANT_TEXT_PREFIX.len())
        .max(1)
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
    let lines = render_markdown_themed(text, &theme, markdown_width(content_width));
    let mut rows = Vec::new();
    push_assistant_markdown_lines(&mut rows, lines);
    rows
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
        searchable: true,
    }
}

fn push_assistant_markdown_lines(rows: &mut Vec<TranscriptRow>, lines: Vec<Line<'static>>) {
    let mut at_block_start = true;
    for line in lines {
        let is_blank = line_text(&line).trim().is_empty();
        let show_marker = at_block_start && assistant_marker_target(&line);
        rows.push(assistant_row(line, show_marker));
        at_block_start = is_blank;
    }
}

fn assistant_marker_target(line: &Line<'static>) -> bool {
    let text = line_text(line);
    let trimmed = text.trim_start();
    !trimmed.is_empty()
        && !text.chars().next().is_some_and(char::is_whitespace)
        && !is_list_row(trimmed)
        && !is_structural_markdown_row(trimmed)
}

fn is_structural_markdown_row(trimmed: &str) -> bool {
    trimmed.starts_with(crate::ui::symbols::SEP)
        || trimmed.starts_with('>')
        || trimmed == "---"
        || matches!(trimmed.chars().next(), Some('┌' | '│' | '├' | '└'))
}

fn is_list_row(trimmed: &str) -> bool {
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        return true;
    }
    let Some((marker, _)) = trimmed.split_once(' ') else {
        return false;
    };
    marker
        .strip_suffix('.')
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

fn assistant_row(mut line: Line<'static>, show_marker: bool) -> TranscriptRow {
    let text = line_text(&line);
    if show_marker {
        // The assistant marker is recessive transcript chrome, not a state dot:
        // muted, never the active/live accent (docs/TUI_DESIGN_LANGUAGE.md §4).
        line.spans.insert(
            0,
            Span::styled(format!("{} ", crate::ui::symbols::ASSISTANT), dim_style()),
        );
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
        searchable: true,
    }
}
