//! Pane-level display row helpers for natural-language transcript text.
//!
//! This is a child module of `ui::tui`, so it can build retained transcript rows
//! without exposing pane rendering policy outside the Iris CLI/TUI layer.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::ui::markdown::render_markdown;

use super::{TranscriptRow, line_text, panel_style, prompt_style, streaming_markdown_preview};

const ASSISTANT_TEXT_PREFIX: &str = "  ";

pub(super) fn push_assistant_rows(rows: &mut Vec<TranscriptRow>, text: &str) {
    for (index, line) in render_markdown(text).into_iter().enumerate() {
        rows.push(assistant_row(line, index == 0));
    }
}

pub(super) fn render_streaming_assistant(width: usize, text: &str, out: &mut Vec<Line<'static>>) {
    let text = streaming_markdown_preview(text);
    for (index, line) in render_markdown(&text).into_iter().enumerate() {
        assistant_row(line, index == 0).render(width, out);
    }
}

pub(super) fn push_user_rows(rows: &mut Vec<TranscriptRow>, text: &str) {
    for line in text.split('\n') {
        rows.push(TranscriptRow::new(line.to_string(), panel_style()));
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
        word_wrap: true,
        background: None,
        hrule: false,
        chrome: None,
    }
}
