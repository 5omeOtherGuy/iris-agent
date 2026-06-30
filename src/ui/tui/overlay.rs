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
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget};

use crate::ui::slash::{self, Palette, SlashCommand};

use super::component::Component;
use super::wrap::display_width;
use super::{TEXT_COLUMN_X_PADDING_U16, dim_style};

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
    /// A modal/picker/dialog docked above the composer.
    Modal,
}

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
    fn render(&self, _width: usize) -> Vec<Line<'static>> {
        let command_width = self
            .matches
            .iter()
            .map(|cmd| display_width(cmd.name))
            .max()
            .unwrap_or(0);
        self.matches
            .iter()
            .enumerate()
            .map(|(i, cmd)| {
                let selected_row = i == self.selected;
                let name_style = if selected_row {
                    Style::default().fg(crate::ui::palette::CYAN)
                } else {
                    Style::default()
                };
                let description_style = if selected_row {
                    Style::default().fg(crate::ui::palette::CYAN)
                } else {
                    dim_style()
                };
                let gap = command_width
                    .saturating_sub(display_width(cmd.name))
                    .saturating_add(2);
                Line::from(vec![
                    Span::styled(cmd.name.to_string(), name_style),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(cmd.description, description_style),
                ])
            })
            .collect()
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
    fn palette_view_renders_one_line_per_match_with_selection_accent() {
        let mut palette = Palette::default();
        palette.sync("/");
        palette.down("/"); // select row 1
        let view = PaletteView::for_palette(&palette, "/");
        let lines = view.render(80);
        assert_eq!(lines.len(), slash::matches("/").len());
        assert!(lines.len() > 1);
        // The selected row (1) is accented cyan on both name and description.
        let selected = &lines[1];
        assert!(
            selected
                .spans
                .iter()
                .any(|s| s.style.fg == Some(Color::Cyan)),
            "selected row should be cyan-accented"
        );
        // A non-selected row uses the default name style (no cyan name).
        let other = &lines[0];
        assert_ne!(other.spans[0].style.fg, Some(Color::Cyan));
    }

    #[test]
    fn palette_view_empty_when_no_matches() {
        let palette = Palette::default();
        let view = PaletteView::for_palette(&palette, "no-slash");
        assert!(view.render(80).is_empty());
    }
}
