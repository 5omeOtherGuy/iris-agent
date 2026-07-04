//! Overlay/focus layer for the composer menu region.
//!
//! pi-mono's overlays (`tui.ts#L459-1049`) FLOAT: they are composited over the
//! base document at an anchor/margin position and capture focus. Iris's two
//! overlays -- the modal/picker and the slash palette -- are DOCKED instead:
//! they reserve a region directly above the composer (allocated by
//! `chrome_heights` in `screen.rs`) and the editor shifts down to make room.
//! This module keeps that honest layout while supplying the two pieces pi-mono
//! couples to overlays:
//!
//! - [`FocusTarget`]: the single source of truth for which layer owns keyboard
//!   input. The precedence (Editor < Palette < Modal) mirrors pi-mono's overlay
//!   focus stack and replaces the implicit `modal_open()` / `palette.is_active`
//!   checks the input loop used to scatter across `tui_loop.rs`.
//! - The docked overlay render path: both the modal and the palette render
//!   through the [`super::component::Component`] contract and paint into the
//!   reserved menu region via [`render_menu_lines`].
//!
//! A true floating anchor/margin compositor, overlay handles
//! (hide/show/focus/unfocus/focus-restore), and multiple simultaneous overlays
//! are deferred: Iris has no floating UI to migrate today, so adding the
//! compositor now would be an abstraction with no real caller. See the new ADR
//! and `implementation-notes.html`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget};

use crate::ui::slash::{self, Palette, SlashCommand};

use super::component::Component;
use super::wrap::{display_width, line_text, truncate_line};
use super::{TEXT_COLUMN_X_PADDING_U16, border_style, dim_style};

/// Which layer currently owns keyboard input.
///
/// Derived from overlay state (see [`super::Screen::focus`]); the highest active
/// layer wins. Used both to route input in `tui_loop.rs` and to pick the docked
/// menu component in `screen.rs`, so focus is explicit and single-sourced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum FocusTarget {
    /// The composer text editor (base layer).
    #[default]
    Editor,
    /// The slash-command palette docked above the composer.
    Palette,
    /// A SessionBar dropdown (directory tree / git console) at the pane top.
    /// Precedence: Editor < Palette < SessionMenu < Modal.
    SessionMenu,
    /// A modal/picker/dialog docked above the composer.
    Modal,
}

/// Build a bordered top-layer overlay box (the `Picker`/`SlashMenu` idiom): an
/// optional uppercase title row, selectable rows (the highlighted row gets the
/// `surface` fill — never a colored border), and an optional dim footer of key
/// hints, all inside one square box-drawing frame. Content-fit width, clamped
/// to the available column.
pub(crate) fn overlay_box(
    title: Option<&str>,
    rows: Vec<(Line<'static>, bool)>,
    footer: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    let max_inner = width.saturating_sub(4).max(8);
    let content_max = rows
        .iter()
        .map(|(line, _)| display_width(&line_text(line)))
        .chain(title.map(display_width))
        .chain(footer.map(display_width))
        .max()
        .unwrap_or(0);
    let inner = content_max.clamp(max_inner.min(16), max_inner);
    let rule = |left: char, right: char| {
        Line::from(Span::styled(
            format!("{left}{}{right}", "─".repeat(inner + 2)),
            border_style(),
        ))
    };
    let boxed_row = |mut line: Line<'static>, selected: bool| {
        truncate_line(&mut line, inner);
        let used = display_width(&line_text(&line));
        if selected {
            for span in &mut line.spans {
                span.style = span.style.bg(crate::ui::palette::surface());
            }
        }
        let pad_style = if selected {
            Style::default().bg(crate::ui::palette::surface())
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::styled("│".to_string(), border_style()),
            Span::styled(" ".to_string(), pad_style),
        ];
        spans.extend(line.spans);
        if used < inner {
            spans.push(Span::styled(" ".repeat(inner - used), pad_style));
        }
        spans.push(Span::styled(" ".to_string(), pad_style));
        spans.push(Span::styled("│".to_string(), border_style()));
        Line::from(spans)
    };
    let mut out = vec![rule('┌', '┐')];
    if let Some(title) = title {
        out.push(boxed_row(
            Line::from(Span::styled(
                title.to_uppercase(),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            false,
        ));
        out.push(rule('├', '┤'));
    }
    for (line, selected) in rows {
        out.push(boxed_row(line, selected));
    }
    if let Some(footer) = footer {
        out.push(rule('├', '┤'));
        out.push(boxed_row(
            Line::from(Span::styled(footer.to_string(), dim_style())),
            false,
        ));
    }
    out.push(rule('└', '┘'));
    out
}

/// Max command rows the palette shows at once. A longer match list scrolls to
/// keep the selection visible (the [`crate::ui::selector::Selector`] windowing
/// idiom) with a dim `(n/total)` position row, so the boxed palette always fits
/// the docked menu budget instead of clipping its bottom frame.
const PALETTE_WINDOW: usize = 8;

/// A [`Component`] view over the slash palette's current matches and selection.
///
/// The palette's STATE lives in [`Palette`] (`slash.rs`); this is its render
/// face, so the palette participates in the component path like every other
/// surface. Output is identical to the former `screen::render_palette`.
pub(super) struct PaletteView<'a> {
    matches: Vec<&'a SlashCommand>,
    selected: usize,
}

impl<'a> PaletteView<'a> {
    /// Build the view from live palette state and the current editor input.
    pub(super) fn for_palette(palette: &Palette, input: &'a str) -> Self {
        Self {
            matches: slash::matches(input),
            selected: palette.selected(),
        }
    }
}

impl Component for PaletteView<'_> {
    fn render(&self, width: usize) -> Vec<Line<'static>> {
        if self.matches.is_empty() {
            return Vec::new();
        }
        let command_width = self
            .matches
            .iter()
            .map(|cmd| display_width(cmd.name))
            .max()
            .unwrap_or(0);
        // Scrolled window over the matches, keeping the selection visible. The
        // scroll arithmetic and the position row below are the shared
        // `Selector` windowing helpers -- the palette clamps and filters by
        // prefix, so it reuses the math without adopting the full Selector.
        let scrolled = self.matches.len() > PALETTE_WINDOW;
        let offset = crate::ui::selector::scroll_offset(self.selected, PALETTE_WINDOW);
        let mut rows: Vec<(Line<'static>, bool)> = self
            .matches
            .iter()
            .enumerate()
            .skip(offset)
            .take(PALETTE_WINDOW)
            .map(|(i, cmd)| {
                let selected_row = i == self.selected;
                // The highlighted row gets the surface fill (overlay_box) plus
                // a bold command name; the description stays muted. Never a
                // color-only selection accent.
                let name_style = if selected_row {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let gap = command_width
                    .saturating_sub(display_width(cmd.name))
                    .saturating_add(2);
                (
                    Line::from(vec![
                        Span::styled(cmd.name.to_string(), name_style),
                        Span::raw(" ".repeat(gap)),
                        Span::styled(cmd.description, dim_style()),
                    ]),
                    selected_row,
                )
            })
            .collect();
        if scrolled {
            rows.push((
                Line::from(Span::styled(
                    crate::ui::selector::position_label(self.selected, self.matches.len()),
                    dim_style(),
                )),
                false,
            ));
        }
        overlay_box(None, rows, None, width)
    }
}

/// Paint a docked overlay's lines into the reserved menu `area`: a one-row top
/// inset, a left text-column indent, and a two-row vertical inset. The inner
/// rect math is preserved byte-for-byte from the former `render_palette` /
/// `render_plain_menu_lines`; `Paragraph` clips overflow exactly as before.
pub(super) fn render_menu_lines(buf: &mut Buffer, area: Rect, lines: Vec<Line<'static>>) {
    // Defense in depth: the caller only paints when `heights.menu > 0`, but a
    // zero-sized area must never reach `Paragraph::render`.
    if area.height == 0 || area.width == 0 {
        return;
    }
    let inner = Rect {
        x: area.x + TEXT_COLUMN_X_PADDING_U16,
        y: area.y + u16::from(area.height > 1),
        width: area
            .width
            .saturating_sub(TEXT_COLUMN_X_PADDING_U16.saturating_mul(2))
            .max(1),
        height: area.height.saturating_sub(2).max(1),
    };
    Paragraph::new(Text::from(lines)).render(inner, buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::slash::Palette;
    use ratatui::style::Color;

    #[test]
    fn focus_target_default_is_editor() {
        assert_eq!(FocusTarget::default(), FocusTarget::Editor);
    }

    #[test]
    fn palette_view_renders_framed_rows_with_surface_selection() {
        let mut palette = Palette::default();
        palette.sync("/re");
        palette.down("/re"); // select row 1 (/resume)
        let view = PaletteView::for_palette(&palette, "/re");
        let lines = view.render(80);
        // One boxed row per match, plus the top and bottom frame rules.
        assert_eq!(lines.len(), slash::matches("/re").len() + 2);
        // The selected row uses the surface fill + a bold command name — never
        // a color-only (cyan) accent.
        let selected = &lines[2];
        assert!(
            selected
                .spans
                .iter()
                .any(|s| s.style.bg == Some(crate::ui::palette::surface())),
            "selected row should carry the surface fill: {selected:?}"
        );
        assert!(
            selected
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "selected command name should be bold: {selected:?}"
        );
        assert!(
            selected
                .spans
                .iter()
                .all(|s| s.style.fg != Some(Color::Cyan)),
            "no cyan selection accent: {selected:?}"
        );
        // A non-selected row has no fill.
        let other = &lines[1];
        assert!(
            other
                .spans
                .iter()
                .all(|s| s.style.bg != Some(crate::ui::palette::surface())),
            "unselected rows are unfilled: {other:?}"
        );
    }

    #[test]
    fn palette_view_empty_when_no_matches() {
        let palette = Palette::default();
        let view = PaletteView::for_palette(&palette, "no-slash");
        assert!(view.render(80).is_empty());
    }

    #[test]
    fn palette_view_windows_long_match_lists_and_follows_the_selection() {
        let total = slash::matches("/").len();
        assert!(
            total > PALETTE_WINDOW,
            "registry no longer exercises palette windowing"
        );
        let text = |line: &Line<'static>| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };

        // Unscrolled top: window rows + position row + two frame rules, with a
        // complete bottom frame (the docked menu budget stays honest).
        let mut palette = Palette::default();
        palette.sync("/");
        let lines = PaletteView::for_palette(&palette, "/").render(80);
        assert_eq!(lines.len(), PALETTE_WINDOW + 3);
        assert!(text(lines.last().unwrap()).contains('└'));
        let rendered = lines.iter().map(&text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("/exit"), "{rendered}");
        assert!(rendered.contains(&format!("(1/{total})")), "{rendered}");
        assert!(!rendered.contains("/logout"), "{rendered}");

        // Walking past the window scrolls it: the last command becomes visible
        // and the first scrolls out.
        for _ in 0..total {
            palette.down("/");
        }
        let lines = PaletteView::for_palette(&palette, "/").render(80);
        assert_eq!(lines.len(), PALETTE_WINDOW + 3);
        let rendered = lines.iter().map(&text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("/checkpoint"), "{rendered}");
        assert!(!rendered.contains("/exit "), "{rendered}");
        assert!(
            rendered.contains(&format!("({total}/{total})")),
            "{rendered}"
        );
    }
}
