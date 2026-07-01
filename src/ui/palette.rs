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
