//! Shared ANSI-aware text helpers for TUI rendering.
//!
//! ANSI/OSC/APC stripping is owned by [`crate::ui::textengine`]; this module
//! only adapts the ratatui span path. `strip_ansi_for_text` is the engine's
//! `clean_text` (strips escape sequences AND remaining control characters).

use ansi_to_tui::IntoText;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

pub(super) use crate::ui::textengine::clean_text as strip_ansi_for_text;
use crate::ui::textengine::{ZwjShaping, expand_tabs, normalize_zwj_with, zwj_shaping};

pub(super) fn ansi_spans(text: &str, default_style: Style) -> Vec<Span<'static>> {
    ansi_spans_shaped(text, default_style, zwj_shaping())
}

/// Shaping-parametrized core of [`ansi_spans`], split out so the ZWJ
/// substitution wiring is testable with an explicit verdict instead of the
/// process-wide global (issue #351).
fn ansi_spans_shaped(text: &str, default_style: Style, shaping: ZwjShaping) -> Vec<Span<'static>> {
    // Width-stabilize ZWJ emoji clusters for non-shaping terminals (issue #351);
    // a borrowed no-op when the probe found shaping or did not run.
    let normalized = normalize_zwj_with(shaping, text);
    let expanded = expand_tabs(&normalized);
    if let Ok(parsed_text) = expanded.as_str().into_text() {
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
    vec![Span::styled(strip_ansi_for_text(&expanded), default_style)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAMILY: &str = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    const MAN: &str = "\u{1f468}";

    fn joined(spans: &[Span<'static>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn ansi_spans_substitutes_zwj_only_when_unshaped() {
        let input = format!("before {FAMILY} after");

        // Unshaped: the family cluster collapses to a single face.
        let unshaped =
            ansi_spans_shaped(&input, Style::default(), ZwjShaping::Unshaped { actual: 6 });
        assert_eq!(joined(&unshaped), format!("before {MAN} after"));

        // Shaped / Unknown: the cluster is preserved verbatim.
        for shaping in [ZwjShaping::Shaped, ZwjShaping::Unknown] {
            let spans = ansi_spans_shaped(&input, Style::default(), shaping);
            assert_eq!(
                joined(&spans),
                input,
                "unexpected substitution for {shaping:?}"
            );
        }
    }
}
