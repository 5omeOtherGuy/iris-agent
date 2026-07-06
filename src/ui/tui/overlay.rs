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
    /// A SessionBar dropdown (directory tree / git console) at the pane top.
    /// Precedence: Editor < Palette < SessionMenu < Modal.
    SessionMenu,
    /// A modal/picker/dialog docked above the composer.
    Modal,
}

/// Build a **frameless** docked menu — the shared `Picker` / `SlashMenu` /
/// settings idiom. No box-drawing frame: structure comes from weight, a fill
/// band, and spacing, exactly like the transcript's tool blocks (§8) and the
/// start-page launcher (§12.5), so every overlay reads as part of the same
/// grammar instead of a heavier bordered dialog.
///
/// - `title` (optional): a **bold uppercase** header row.
/// - `rows`: selectable content; the highlighted row carries the `surface` fill
///   across the menu measure — never a border, never a colored accent.
/// - `footer` (optional): a dim key-hint row, set off by one blank row.
///
/// Content-fit width, clamped to the available column.
pub(crate) fn overlay_menu(
    title: Option<&str>,
    rows: Vec<(Line<'static>, bool)>,
    footer: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    let avail = width.max(1);
    // The selection fill spans the widest content row (title/footer included),
    // clamped to the column, so a highlighted row reads as an even band rather
    // than a ragged one. A small floor keeps a short menu from looking pinched.
    let content_max = rows
        .iter()
        .map(|(line, _)| display_width(&line_text(line)))
        .chain(title.map(display_width))
        .chain(footer.map(display_width))
        .max()
        .unwrap_or(0);
    let fill_width = content_max.clamp(16.min(avail), avail);

    let mut out: Vec<Line<'static>> = Vec::new();
    if let Some(title) = title {
        let mut line = Line::from(Span::styled(
            title.to_uppercase(),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        truncate_line(&mut line, avail);
        out.push(line);
    }
    for (mut line, selected) in rows {
        truncate_line(&mut line, fill_width);
        if selected {
            // Patch the surface bg onto every span (preserving fg/weight) and
            // pad the tail so the fill is an even band, not ragged to the text.
            let used = display_width(&line_text(&line));
            let fill = Style::default().bg(crate::ui::palette::surface());
            for span in &mut line.spans {
                span.style = span.style.patch(fill);
            }
            if used < fill_width {
                line.spans
                    .push(Span::styled(" ".repeat(fill_width - used), fill));
            }
        }
        out.push(line);
    }
    if let Some(footer) = footer {
        out.push(Line::default());
        let mut line = Line::from(Span::styled(footer.to_string(), dim_style()));
        truncate_line(&mut line, avail);
        out.push(line);
    }
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
                // The highlighted row gets the surface fill (overlay_menu) plus
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
        overlay_menu(None, rows, None, width)
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
    fn palette_view_renders_frameless_rows_with_surface_selection() {
        let mut palette = Palette::default();
        palette.sync("/re");
        palette.down("/re"); // select row 1 (/resume)
        let view = PaletteView::for_palette(&palette, "/re");
        let lines = view.render(80);
        // Frameless: one row per match, no top/bottom frame rules.
        assert_eq!(lines.len(), slash::matches("/re").len());
        // No box-drawing frame characters anywhere.
        let text = |line: &Line<'static>| -> String {
            line.spans.iter().map(|s| s.content.as_ref()).collect()
        };
        for line in &lines {
            let t = text(line);
            assert!(
                !t.chars().any(|c| "┌┐└┘├┤│─".contains(c)),
                "no frame chars: {t:?}"
            );
        }
        // Exactly one row carries the surface fill: the selected one, with a
        // bold command name and never a color-only (cyan) accent.
        let filled: Vec<&Line<'static>> = lines
            .iter()
            .filter(|line| {
                line.spans
                    .iter()
                    .any(|s| s.style.bg == Some(crate::ui::palette::surface()))
            })
            .collect();
        assert_eq!(filled.len(), 1, "one selected band");
        let selected = filled[0];
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

        // Unscrolled top: frameless — window rows + the dim position row, no
        // frame rules. The last row is the `(1/total)` position label.
        let mut palette = Palette::default();
        palette.sync("/");
        let lines = PaletteView::for_palette(&palette, "/").render(80);
        assert_eq!(lines.len(), PALETTE_WINDOW + 1);
        assert!(text(lines.last().unwrap()).contains(&format!("(1/{total})")));
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
        assert_eq!(lines.len(), PALETTE_WINDOW + 1);
        let rendered = lines.iter().map(&text).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("/checkpoint"), "{rendered}");
        assert!(!rendered.contains("/exit "), "{rendered}");
        assert!(
            rendered.contains(&format!("({total}/{total})")),
            "{rendered}"
        );
    }
}
