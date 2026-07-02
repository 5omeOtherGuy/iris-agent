//! Reusable component/container render abstraction for the Tier-3 TUI.
//!
//! This is Iris's idiomatic Rust analogue of pi-mono's
//! `packages/tui/src/tui.ts` `Component`/`Container` contract. The render unit
//! is Ratatui's `Line<'static>` (Iris's existing render currency) in place of
//! pi-mono's `string[]`. The abstraction sits ABOVE
//! [`crate::ui::terminal_surface::TerminalSurface`]: components produce the
//! final composited `Vec<Line>` and the terminal surface still owns all
//! diff/append/replay decisions. Nothing here touches terminal I/O.
//!
//! Three pieces, mirroring the prior art:
//! - [`Component`]: width-driven render plus an optional `invalidate` hook.
//! - [`Container`]: composites ordered children into one line list.
//! - [`CURSOR_MARKER`] / [`take_cursor_position`]: the focus cursor contract. A
//!   focused component may emit the zero-width marker at its cursor position;
//!   the composition root locates and strips it, yielding a `(row, col)` for
//!   hardware-cursor placement. Iris's editor renders its own reversed block
//!   cursor today, so no shipped component emits the marker yet; the mechanism
//!   is the seam the deferred full-TUI editor builds on.

use ratatui::text::{Line, Span};

use super::wrap::display_width;

/// A renderable UI building block.
///
/// Mirrors pi-mono `Component` (`tui.ts#L39-60`): `render(width)` returns the
/// component's lines for the current viewport width.
///
/// Two pi-mono hooks are intentionally omitted to honor the "no stub methods
/// with no callers" rule rather than ship dead contract surface:
/// - `handleInput`: Iris routes typed crossterm events through the focus/overlay
///   layer (see [`super::overlay`] and [`super::FocusTarget`]) instead of
///   pi-mono's stringly-typed input, so render-only components (transcript rows,
///   panels, modal, palette) stay free of input concerns.
/// - `invalidate`: no Iris component caches render state today; the hook is
///   deferred until a caching surface (markdown cache, theming) needs it, at
///   which point it lands with a real caller.
pub(crate) trait Component {
    /// Render this component to styled lines for `width` columns.
    fn render(&self, width: usize) -> Vec<Line<'static>>;

    /// Append this component's lines to `out`. The default allocates via
    /// [`Component::render`]; hot composite paths (the transcript renders up to
    /// `MAX_TRANSCRIPT_ROWS` rows per frame) override this to append directly and
    /// avoid a per-call `Vec`, matching the pre-abstraction push loops.
    fn render_into(&self, width: usize, out: &mut Vec<Line<'static>>) {
        out.extend(self.render(width));
    }
}

/// Composite an ordered sequence of borrowed components into one line list.
///
/// Width is propagated unchanged to every child and their rendered lines are
/// concatenated in order -- the borrowed-children counterpart to
/// [`Container::render`]. Kept separate so hot paths (the transcript renders
/// thousands of rows per frame) can composite `&dyn Component` without boxing.
#[cfg(test)]
pub(crate) fn composite<'a>(
    children: impl IntoIterator<Item = &'a dyn Component>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for child in children {
        child.render_into(width, &mut out);
    }
    out
}

/// Owns ordered child components and composites them top-to-bottom.
///
/// Iris's analogue of pi-mono `Container` (`tui.ts#L226-265`). Used by the
/// composition root to assemble whole sections (transcript, working indicator,
/// composer chrome) without the root knowing each section's internals.
#[derive(Default)]
pub(crate) struct Container {
    children: Vec<Box<dyn Component>>,
}

impl Container {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Append a child; children render in insertion order.
    pub(crate) fn add_child(&mut self, component: Box<dyn Component>) {
        self.children.push(component);
    }
}

impl Component for Container {
    fn render(&self, width: usize) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        self.render_into(width, &mut out);
        out
    }

    fn render_into(&self, width: usize, out: &mut Vec<Line<'static>>) {
        for child in &self.children {
            child.render_into(width, out);
        }
    }
}

/// Zero-width cursor-position marker, an APC (Application Program Command)
/// sequence terminals ignore. A focused component embeds this in its render
/// output at the cursor; [`take_cursor_position`] finds, locates, and strips it.
/// Mirrors pi-mono `CURSOR_MARKER` (`tui.ts#L87`).
pub(crate) const CURSOR_MARKER: &str = "\x1b_pi:c\x07";

/// Find the first [`CURSOR_MARKER`] in `lines`, strip it, and return its
/// `(row, column)` (column = display width of the content before it on that
/// row). Returns `None` when no marker is present, which is the case for every
/// component Iris ships today (the editor draws its own block cursor), so the
/// composition root calling this is a no-op strip until a marker-emitting
/// (e.g. hardware-cursor) component lands.
pub(crate) fn take_cursor_position(lines: &mut [Line<'static>]) -> Option<(usize, usize)> {
    for (row, line) in lines.iter_mut().enumerate() {
        // Locate the marker (span index + byte offset) in one pass.
        let Some((span_idx, marker_at)) = line
            .spans
            .iter()
            .enumerate()
            .find_map(|(i, span)| span.content.find(CURSOR_MARKER).map(|at| (i, at)))
        else {
            continue;
        };
        let prefix_width: usize = line.spans[..span_idx]
            .iter()
            .map(|span| display_width(span.content.as_ref()))
            .sum();
        let span = &line.spans[span_idx];
        let content = span.content.as_ref();
        let before = &content[..marker_at];
        let after = &content[marker_at + CURSOR_MARKER.len()..];
        let column = prefix_width + display_width(before);
        let style = span.style;
        let mut replacement: Vec<Span<'static>> = Vec::new();
        if !before.is_empty() {
            replacement.push(Span::styled(before.to_string(), style));
        }
        if !after.is_empty() {
            replacement.push(Span::styled(after.to_string(), style));
        }
        line.spans.splice(span_idx..=span_idx, replacement);
        return Some((row, column));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};

    /// Minimal fixed-output component for contract tests.
    struct Static {
        lines: Vec<Line<'static>>,
    }

    impl Static {
        fn text(lines: &[&str]) -> Self {
            Self {
                lines: lines.iter().map(|t| Line::from(t.to_string())).collect(),
            }
        }
    }

    impl Component for Static {
        fn render(&self, _width: usize) -> Vec<Line<'static>> {
            self.lines.clone()
        }
    }

    /// A component that echoes the width it was rendered at, proving width
    /// propagation through the container.
    struct WidthEcho;
    impl Component for WidthEcho {
        fn render(&self, width: usize) -> Vec<Line<'static>> {
            vec![Line::from(width.to_string())]
        }
    }

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn container_composites_children_in_order() {
        let mut container = Container::new();
        container.add_child(Box::new(Static::text(&["a", "b"])));
        container.add_child(Box::new(Static::text(&["c"])));
        let lines = container.render(80);
        assert_eq!(texts(&lines), vec!["a", "b", "c"]);
    }

    #[test]
    fn container_propagates_width_to_each_child() {
        let mut container = Container::new();
        container.add_child(Box::new(WidthEcho));
        container.add_child(Box::new(WidthEcho));
        let lines = container.render(42);
        assert_eq!(texts(&lines), vec!["42", "42"]);
    }

    #[test]
    fn empty_container_renders_nothing() {
        let container = Container::new();
        assert!(container.render(80).is_empty());
        assert!(container.render(0).is_empty());
    }

    #[test]
    fn container_handles_zero_width() {
        // Zero width must not panic; children decide their own degenerate output.
        let mut container = Container::new();
        container.add_child(Box::new(WidthEcho));
        let lines = container.render(0);
        assert_eq!(texts(&lines), vec!["0"]);
    }

    #[test]
    fn composite_matches_manual_concatenation() {
        let a = Static::text(&["x", "y"]);
        let b = Static::text(&["z"]);
        let children: Vec<&dyn Component> = vec![&a, &b];
        let lines = composite(children, 10);
        assert_eq!(texts(&lines), vec!["x", "y", "z"]);
    }

    #[test]
    fn take_cursor_position_locates_and_strips_marker() {
        let mut lines = vec![
            Line::from("no marker here".to_string()),
            Line::from(vec![
                Span::styled("ab".to_string(), Style::default().fg(Color::Red)),
                Span::styled(
                    format!("cd{CURSOR_MARKER}ef"),
                    Style::default().fg(Color::Green),
                ),
            ]),
        ];
        let pos = take_cursor_position(&mut lines);
        // Row 1, column = width("ab") + width("cd") = 4.
        assert_eq!(pos, Some((1, 4)));
        // Marker stripped, surrounding text and styling preserved, split cleanly.
        assert_eq!(texts(&lines), vec!["no marker here", "abcdef"]);
        assert_eq!(lines[1].spans.len(), 3);
        assert_eq!(lines[1].spans[1].content.as_ref(), "cd");
        assert_eq!(lines[1].spans[2].content.as_ref(), "ef");
        // The split spans inherit the original span's style (a regression here
        // would silently drop styling on either side of the cursor).
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Green));
        assert_eq!(lines[1].spans[2].style.fg, Some(Color::Green));
    }

    #[test]
    fn take_cursor_position_none_when_absent() {
        let mut lines = vec![Line::from("plain".to_string())];
        assert_eq!(take_cursor_position(&mut lines), None);
        assert_eq!(texts(&lines), vec!["plain"]);
    }

    #[test]
    fn take_cursor_position_marker_at_line_start() {
        let mut lines = vec![Line::from(format!("{CURSOR_MARKER}tail"))];
        assert_eq!(take_cursor_position(&mut lines), Some((0, 0)));
        assert_eq!(texts(&lines), vec!["tail"]);
    }
}
