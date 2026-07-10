//! Canonical Iris color roles — the single source of truth for the
//! terminal-relative palette in `docs/TUI_DESIGN_LANGUAGE.md` §Color and
//! `DESIGN.md`. Roles bind to ANSI named slots so the UI inherits the user's own
//! light/dark terminal theme; the hexes in DESIGN.md are dark-mode reference
//! approximations only. Color is signal — used sparsely and always paired with a
//! symbol + label, never as the sole carrier of state.

use std::sync::atomic::{AtomicU8, Ordering};

use ratatui::style::Color;

/// `border` — panel & composer frames. ANSI Gray.
pub(crate) const BORDER: Color = Color::Gray;

/// `muted` — metadata, hints, markers, elisions, separators (`┊`/`─`), the bulk
/// of the transcript's secondary text. ANSI DarkGray: the recessive text role,
/// dimmer than `border` yet still legible, and — unlike the `DIM` *attribute* it
/// replaces — a real color that survives DIM-blind terminals and honours each
/// named theme's own grey (docs/TUI_DESIGN_LANGUAGE.md §2.1).
pub(crate) const MUTED: Color = Color::DarkGray;

/// `stdout` — SHELL program output (the recessive grey *below* the command). A
/// light grey that sits between `ink` (default fg) and `muted`: it recedes
/// beneath the bright command line but stays far more readable than `muted`.
/// Indexed(250) approximates the `#b7b7bd` dark reference and mirrors SURFACE's
/// fixed-index precedent (dark-tuned; named themes supply their own value).
pub(crate) const STDOUT: Color = Color::Indexed(250);

/// `accent` (orange) — active mode, running, current edge dot, warnings. The
/// terminal's bright/yellow accent slot.
pub(crate) const ORANGE: Color = Color::Yellow;

/// `interactive` — selection highlight, inline code, focus. ANSI Cyan.
pub(crate) const CYAN: Color = Color::Cyan;

/// `success` — DONE / APPROVED / diff additions. ANSI Green.
pub(crate) const GREEN: Color = Color::Green;

/// `danger` — ERROR / DENIED / diff removals. ANSI Red.
pub(crate) const RED: Color = Color::Red;

/// `surface` — selection / active-row fill (overlay row highlight). The single
/// permitted tonal fill; never used behind whole panels or regions. Indexed(236)
/// approximates the `#323238` dark reference while staying in the 256-color set.
pub(crate) const SURFACE: Color = Color::Indexed(236);

/// `add-bg` — diff addition row background. Indexed(22).
pub(crate) const DIFF_ADD_BG: Color = Color::Indexed(22);

/// `del-bg` — diff removal row background. Indexed(52).
pub(crate) const DIFF_DEL_BG: Color = Color::Indexed(52);

use super::theme;

/// Effective terminal palette capability. The rich TUI classifies it once at
/// startup; pure render tests retain truecolor as the compatibility default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColorDepth {
    Ansi16 = 0,
    Ansi256 = 1,
    TrueColor = 2,
}

static COLOR_DEPTH: AtomicU8 = AtomicU8::new(ColorDepth::TrueColor as u8);

fn active_depth() -> ColorDepth {
    match COLOR_DEPTH.load(Ordering::Relaxed) {
        0 => ColorDepth::Ansi16,
        1 => ColorDepth::Ansi256,
        _ => ColorDepth::TrueColor,
    }
}

/// Classify conventional terminal capability variables. `COLORTERM` is the
/// explicit 24-bit signal; otherwise `TERM=*256color` earns the indexed ramp,
/// and an unknown/plain TERM receives the conservative 16-color grammar.
pub(crate) fn detect_color_depth(term: Option<&str>, colorterm: Option<&str>) -> ColorDepth {
    let colorterm = colorterm.unwrap_or_default().to_ascii_lowercase();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        return ColorDepth::TrueColor;
    }
    let term = term.unwrap_or_default().to_ascii_lowercase();
    if term.contains("truecolor") || term.contains("24bit") {
        ColorDepth::TrueColor
    } else if term.contains("256color") {
        ColorDepth::Ansi256
    } else {
        ColorDepth::Ansi16
    }
}

/// Resolve and install terminal color depth before the first rich-TUI frame.
pub(crate) fn configure_terminal_color_depth() {
    let term = std::env::var("TERM").ok();
    let colorterm = std::env::var("COLORTERM").ok();
    let depth = detect_color_depth(term.as_deref(), colorterm.as_deref());
    COLOR_DEPTH.store(depth as u8, Ordering::Relaxed);
}

fn nearest_cube_component(value: u8) -> (u8, u8) {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    LEVELS
        .iter()
        .copied()
        .enumerate()
        .min_by_key(|(_, level)| value.abs_diff(*level))
        .map(|(index, level)| (index as u8, level))
        .unwrap_or((0, 0))
}

fn distance_sq(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let dr = i32::from(a.0) - i32::from(b.0);
    let dg = i32::from(a.1) - i32::from(b.1);
    let db = i32::from(a.2) - i32::from(b.2);
    (dr * dr + dg * dg + db * db) as u32
}

/// Nearest xterm-256 cube/grayscale entry. Constant work per role lookup: one
/// nearest component per channel plus the analytically-nearest grayscale.
fn rgb_to_xterm(r: u8, g: u8, b: u8) -> u8 {
    let (ri, rv) = nearest_cube_component(r);
    let (gi, gv) = nearest_cube_component(g);
    let (bi, bv) = nearest_cube_component(b);
    let cube_index = 16 + 36 * ri + 6 * gi + bi;
    let cube_distance = distance_sq((r, g, b), (rv, gv, bv));

    let mean = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let gray_slot = mean.saturating_sub(8).saturating_add(5) / 10;
    let gray_slot = gray_slot.min(23) as u8;
    let gray = 8 + 10 * gray_slot;
    let gray_distance = distance_sq((r, g, b), (gray, gray, gray));
    if gray_distance < cube_distance {
        232 + gray_slot
    } else {
        cube_index
    }
}

fn adapt_color_for_depth(color: Color, fallback: Color, depth: ColorDepth) -> Color {
    match depth {
        ColorDepth::TrueColor => color,
        ColorDepth::Ansi256 => match color {
            Color::Rgb(r, g, b) => Color::Indexed(rgb_to_xterm(r, g, b)),
            other => other,
        },
        ColorDepth::Ansi16 => match color {
            Color::Reset => Color::Reset,
            _ => fallback,
        },
    }
}

fn role(color: Color, ansi16: Color) -> Color {
    adapt_color_for_depth(color, ansi16, active_depth())
}

fn diff_role_for_depth(color: Color, depth: ColorDepth) -> Color {
    match depth {
        ColorDepth::Ansi16 => Color::Reset,
        other => adapt_color_for_depth(color, Color::Reset, other),
    }
}

/// Themed `border` role: the active theme's border color.
pub(crate) fn border() -> Color {
    role(theme::active().border(), Color::Gray)
}
/// Themed `muted` role: the active theme's muted (recessive text) color.
pub(crate) fn muted() -> Color {
    role(theme::active().muted(), Color::DarkGray)
}
/// Themed `stdout` role: the active theme's SHELL-output grey.
pub(crate) fn stdout() -> Color {
    role(theme::active().stdout(), Color::Gray)
}
/// Themed `accent` role: the active theme's accent (orange) color.
pub(crate) fn orange() -> Color {
    role(theme::active().accent(), Color::Yellow)
}
/// Themed `interactive` role: the active theme's interactive (cyan) color.
pub(crate) fn cyan() -> Color {
    role(theme::active().interactive(), Color::Cyan)
}
/// Themed `success` role: the active theme's success (green) color.
pub(crate) fn green() -> Color {
    role(theme::active().success(), Color::Green)
}
/// Themed `danger` role: the active theme's danger (red) color.
pub(crate) fn red() -> Color {
    role(theme::active().danger(), Color::Red)
}
/// Themed `surface` role: the active theme's surface fill color.
pub(crate) fn surface() -> Color {
    role(theme::active().surface(), Color::DarkGray)
}
/// Themed `add-bg` role: the active theme's diff-addition background.
pub(crate) fn diff_add_bg() -> Color {
    diff_role_for_depth(theme::active().diff_add_bg(), active_depth())
}
/// Themed `del-bg` role: the active theme's diff-removal background.
pub(crate) fn diff_del_bg() -> Color {
    diff_role_for_depth(theme::active().diff_del_bg(), active_depth())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_follow_active_theme() {
        // Default (adaptive terminal) theme returns the raw palette consts.
        assert_eq!(surface(), SURFACE);
        assert_eq!(border(), BORDER);
        assert_eq!(muted(), MUTED);
        assert_eq!(stdout(), STDOUT);

        // A fixed-RGB named theme overrides the ANSI slots.
        theme::set_active("gruvbox");
        assert_ne!(green(), GREEN);
        assert!(matches!(green(), Color::Rgb(..)));
        // The recessive roles are themed too — named themes carry their own grey.
        assert!(matches!(muted(), Color::Rgb(..)));
        assert!(matches!(stdout(), Color::Rgb(..)));

        // Reset so global state does not leak to other tests.
        theme::set_active("terminal");
        assert_eq!(green(), GREEN);
        assert_eq!(muted(), MUTED);
    }

    #[test]
    fn capability_detection_prefers_explicit_truecolor_then_term_depth() {
        assert_eq!(
            detect_color_depth(Some("xterm-256color"), Some("truecolor")),
            ColorDepth::TrueColor
        );
        assert_eq!(
            detect_color_depth(Some("screen-256color"), None),
            ColorDepth::Ansi256
        );
        assert_eq!(detect_color_depth(Some("xterm"), None), ColorDepth::Ansi16);
        assert_eq!(detect_color_depth(None, None), ColorDepth::Ansi16);
    }

    #[test]
    fn fixed_rgb_degrades_to_indexed_then_semantic_ansi() {
        let rgb = Color::Rgb(0xfa, 0xbd, 0x2f);
        assert!(matches!(
            adapt_color_for_depth(rgb, Color::Yellow, ColorDepth::Ansi256),
            Color::Indexed(_)
        ));
        assert_eq!(
            adapt_color_for_depth(rgb, Color::Yellow, ColorDepth::Ansi16),
            Color::Yellow
        );
        assert_eq!(
            adapt_color_for_depth(Color::Reset, Color::Red, ColorDepth::Ansi16),
            Color::Reset
        );
        assert_eq!(
            diff_role_for_depth(Color::Rgb(39, 60, 46), ColorDepth::Ansi16),
            Color::Reset,
            "16-color diffs remove saturated row fills"
        );
        assert!(matches!(
            diff_role_for_depth(Color::Rgb(39, 60, 46), ColorDepth::Ansi256),
            Color::Indexed(_)
        ));
    }

    #[test]
    fn xterm_quantizer_keeps_primary_and_grayscale_anchors() {
        assert_eq!(rgb_to_xterm(255, 0, 0), 196);
        assert_eq!(rgb_to_xterm(128, 128, 128), 244);
    }
}
