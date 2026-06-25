//! Shared ANSI-aware text helpers for TUI rendering.
//!
//! ANSI/OSC/APC stripping is owned by [`crate::ui::textengine`]; this module
//! only adapts the ratatui span path. `strip_ansi_for_text` is the engine's
//! `clean_text` (strips escape sequences AND remaining control characters).

use ansi_to_tui::IntoText;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

pub(super) use crate::ui::textengine::clean_text as strip_ansi_for_text;

pub(super) fn ansi_spans(text: &str, default_style: Style) -> Vec<Span<'static>> {
    if let Ok(parsed_text) = text.into_text() {
        // Flatten any parsed sub-lines (a stray \r can split one input line)
        // so no styled content is dropped.
        let mut spans = Vec::new();
        for parsed in parsed_text.lines {
            for mut span in parsed.spans {
                let content = strip_ansi_for_text(span.content.as_ref());
                if content.is_empty() {
                    continue;
                }
                span.content = content.into();
                if matches!(span.style.fg, None | Some(Color::Reset))
                    && let Some(fg) = default_style.fg
                {
                    span.style = span.style.fg(fg);
                }
                spans.push(span);
            }
        }
        if !spans.is_empty() {
            return spans;
        }
    }
    vec![Span::styled(strip_ansi_for_text(text), default_style)]
}
