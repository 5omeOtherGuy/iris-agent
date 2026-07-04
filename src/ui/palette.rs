//! Canonical Iris color roles — the single source of truth for the
//! terminal-relative palette in `docs/TUI_DESIGN_LANGUAGE.md` §Color and
//! `DESIGN.md`. Roles bind to ANSI named slots so the UI inherits the user's own
//! light/dark terminal theme; the hexes in DESIGN.md are dark-mode reference
//! approximations only. Color is signal — used sparsely and always paired with a
//! symbol + label, never as the sole carrier of state.

use ratatui::style::Color;

/// `border` — panel & composer frames. ANSI Gray.
pub(crate) const BORDER: Color = Color::Gray;

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

/// Themed `border` role: the active theme's border color.
pub(crate) fn border() -> Color {
    theme::active().border()
}
/// Themed `accent` role: the active theme's accent (orange) color.
pub(crate) fn orange() -> Color {
    theme::active().accent()
}
/// Themed `interactive` role: the active theme's interactive (cyan) color.
pub(crate) fn cyan() -> Color {
    theme::active().interactive()
}
/// Themed `success` role: the active theme's success (green) color.
pub(crate) fn green() -> Color {
    theme::active().success()
}
/// Themed `danger` role: the active theme's danger (red) color.
pub(crate) fn red() -> Color {
    theme::active().danger()
}
/// Themed `surface` role: the active theme's surface fill color.
pub(crate) fn surface() -> Color {
    theme::active().surface()
}
/// Themed `add-bg` role: the active theme's diff-addition background.
pub(crate) fn diff_add_bg() -> Color {
    theme::active().diff_add_bg()
}
/// Themed `del-bg` role: the active theme's diff-removal background.
pub(crate) fn diff_del_bg() -> Color {
    theme::active().diff_del_bg()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessors_follow_active_theme() {
        // Default (adaptive terminal) theme returns the raw palette consts.
        assert_eq!(surface(), SURFACE);
        assert_eq!(border(), BORDER);

        // A fixed-RGB named theme overrides the ANSI slots.
        theme::set_active("gruvbox");
        assert_ne!(green(), GREEN);
        assert!(matches!(green(), Color::Rgb(..)));

        // Reset so global state does not leak to other tests.
        theme::set_active("terminal");
        assert_eq!(green(), GREEN);
    }
}
