//! Shared ANSI-aware text helpers for TUI rendering.

use ansi_to_tui::IntoText;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

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

pub(super) fn strip_ansi_for_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.next() {
                Some('[') => consume_csi(&mut chars),
                Some(']' | 'P' | '^' | '_' | 'X') => consume_string_control(&mut chars),
                Some(_) | None => {}
            },
            '\u{009b}' => consume_csi(&mut chars),
            '\u{009d}' | '\u{0090}' | '\u{009e}' | '\u{009f}' | '\u{0098}' => {
                consume_string_control(&mut chars);
            }
            _ if ch.is_control() => {}
            _ => out.push(ch),
        }
    }
    out
}

fn consume_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for ch in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&ch) {
            break;
        }
    }
}

fn consume_string_control(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(ch) = chars.next() {
        if ch == '\u{7}' {
            break;
        }
        if ch == '\x1b' && matches!(chars.peek(), Some('\\')) {
            chars.next();
            break;
        }
    }
}
