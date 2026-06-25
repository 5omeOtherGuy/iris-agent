//! Pane-level display row helpers for natural-language transcript text.
//!
//! This is a child module of `ui::tui`, so it can build retained transcript rows
//! without exposing pane rendering policy outside the Iris CLI/TUI layer.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::ui::markdown::render_markdown;

use super::rows::{FoldVis, TranscriptRow};
use super::transcript::streaming_markdown_preview;
use super::wrap::line_text;
use super::{panel_style, prompt_style};

const ASSISTANT_TEXT_PREFIX: &str = "  ";

pub(super) fn push_assistant_rows(rows: &mut Vec<TranscriptRow>, text: &str) {
    for (index, line) in render_markdown(text).into_iter().enumerate() {
        rows.push(assistant_row(line, index == 0));
    }
}

/// Build the transient transcript rows for the in-flight streamed assistant
/// text. The transcript composites these through the shared `Component` path
/// after committed history, then commits them once on `AssistantTextEnd`.
pub(super) fn streaming_assistant_rows(text: &str) -> Vec<TranscriptRow> {
    let text = streaming_markdown_preview(text);
    render_markdown(&text)
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
