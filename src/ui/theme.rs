//! Theme trait — the single indirection point for Iris' color roles (ADR-0042).
//!
//! Iris is terminal-relative by default: [`TerminalTheme`] returns the ANSI-slot
//! roles from [`super::palette`], so the UI inherits the user's own light/dark
//! terminal theme and stays correct under `NO_COLOR`. Named themes (Gruvbox,
//! Catppuccin, Nord, Tokyo Night) are an opt-in exception: fixed-RGB palettes
//! that deliberately ignore the terminal theme, selected by id via
//! [`resolve`].
//!
//! Themes only supply role colors. The two laws of color from
//! `docs/TUI_DESIGN_LANGUAGE.md` §2 (color is a point signal; never color
//! alone) are enforced by render code, not by the theme.
//!
//! Call sites read roles through the themed [`super::palette`] accessors (e.g.
//! `palette::border()`), which delegate to [`active`]; the raw `palette::ROLE`
//! constants remain only as the terminal theme's ANSI-slot source.

use std::sync::RwLock;

use ratatui::style::Color;

use super::palette;

/// The eight color roles every theme must supply. Mirrors [`super::palette`].
pub(crate) trait Theme: Sync {
    /// Stable identifier used in config and the `/theme` picker (e.g. `"gruvbox"`).
    fn id(&self) -> &'static str;
    /// Human-readable name for the picker (e.g. `"Gruvbox Dark"`).
    #[allow(dead_code)] // reserved for the theme picker label
    fn name(&self) -> &'static str;

    /// `border` — panel & composer frames.
    fn border(&self) -> Color;
    /// `accent` — active mode, running, meter edge dot, warnings.
    fn accent(&self) -> Color;
    /// `interactive` — selection focus, inline code.
    fn interactive(&self) -> Color;
    /// `success` — DONE / APPROVED, diff additions.
    fn success(&self) -> Color;
    /// `danger` — ERROR / DENIED, diff removals, stderr.
    fn danger(&self) -> Color;
    /// `surface` — selection / active-row fill only.
    fn surface(&self) -> Color;
    /// `add-bg` — diff addition row background.
    fn diff_add_bg(&self) -> Color;
    /// `del-bg` — diff removal row background.
    fn diff_del_bg(&self) -> Color;
}

/// Default, terminal-relative theme: roles bind to ANSI named slots so Iris
/// inherits the user's terminal theme. Values come from [`super::palette`].
pub(crate) struct TerminalTheme;

impl Theme for TerminalTheme {
    fn id(&self) -> &'static str {
        "terminal"
    }
    fn name(&self) -> &'static str {
        "Terminal (adaptive)"
    }
    fn border(&self) -> Color {
        palette::BORDER
    }
    fn accent(&self) -> Color {
        palette::ORANGE
    }
    fn interactive(&self) -> Color {
        palette::CYAN
    }
    fn success(&self) -> Color {
        palette::GREEN
    }
    fn danger(&self) -> Color {
        palette::RED
    }
    fn surface(&self) -> Color {
        palette::SURFACE
    }
    fn diff_add_bg(&self) -> Color {
        palette::DIFF_ADD_BG
    }
    fn diff_del_bg(&self) -> Color {
        palette::DIFF_DEL_BG
    }
}

/// A fixed-RGB named theme. Static color table; no terminal adaptation.
struct FixedTheme {
    id: &'static str,
    name: &'static str,
    border: Color,
    accent: Color,
    interactive: Color,
    success: Color,
    danger: Color,
    surface: Color,
    diff_add_bg: Color,
    diff_del_bg: Color,
}

impl Theme for FixedTheme {
    fn id(&self) -> &'static str {
        self.id
    }
    fn name(&self) -> &'static str {
        self.name
    }
    fn border(&self) -> Color {
        self.border
    }
    fn accent(&self) -> Color {
        self.accent
    }
    fn interactive(&self) -> Color {
        self.interactive
    }
    fn success(&self) -> Color {
        self.success
    }
    fn danger(&self) -> Color {
        self.danger
    }
    fn surface(&self) -> Color {
        self.surface
    }
    fn diff_add_bg(&self) -> Color {
        self.diff_add_bg
    }
    fn diff_del_bg(&self) -> Color {
        self.diff_del_bg
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

static TERMINAL: TerminalTheme = TerminalTheme;

static GRUVBOX: FixedTheme = FixedTheme {
    id: "gruvbox",
    name: "Gruvbox Dark",
    border: rgb(0x92, 0x83, 0x74),
    accent: rgb(0xfa, 0xbd, 0x2f),
    interactive: rgb(0x8e, 0xc0, 0x7c),
    success: rgb(0xb8, 0xbb, 0x26),
    danger: rgb(0xfb, 0x49, 0x34),
    surface: rgb(0x3c, 0x38, 0x36),
    diff_add_bg: rgb(0x34, 0x38, 0x1b),
    diff_del_bg: rgb(0x3c, 0x1f, 0x1e),
};

// Catppuccin ships four flavors; each maps the same roles (Overlay0 border,
// Peach accent, Teal interactive, Green/Red, Surface0 fill) onto that flavor's
// palette. Diff backgrounds are hand-tinted from each flavor's Base.
static CATPPUCCIN_MOCHA: FixedTheme = FixedTheme {
    id: "catppuccin-mocha",
    name: "Catppuccin Mocha",
    border: rgb(0x6c, 0x70, 0x86),
    accent: rgb(0xfa, 0xb3, 0x87),
    interactive: rgb(0x94, 0xe2, 0xd5),
    success: rgb(0xa6, 0xe3, 0xa1),
    danger: rgb(0xf3, 0x8b, 0xa8),
    surface: rgb(0x31, 0x32, 0x44),
    diff_add_bg: rgb(0x27, 0x3c, 0x2e),
    diff_del_bg: rgb(0x43, 0x27, 0x2e),
};

static CATPPUCCIN_MACCHIATO: FixedTheme = FixedTheme {
    id: "catppuccin-macchiato",
    name: "Catppuccin Macchiato",
    border: rgb(0x6e, 0x73, 0x8d),
    accent: rgb(0xf5, 0xa9, 0x7f),
    interactive: rgb(0x8b, 0xd5, 0xca),
    success: rgb(0xa6, 0xda, 0x95),
    danger: rgb(0xed, 0x87, 0x96),
    surface: rgb(0x36, 0x3a, 0x4f),
    diff_add_bg: rgb(0x2d, 0x45, 0x3a),
    diff_del_bg: rgb(0x49, 0x30, 0x3a),
};

static CATPPUCCIN_FRAPPE: FixedTheme = FixedTheme {
    id: "catppuccin-frappe",
    name: "Catppuccin Frappe",
    border: rgb(0x73, 0x79, 0x94),
    accent: rgb(0xef, 0x9f, 0x76),
    interactive: rgb(0x81, 0xc8, 0xbe),
    success: rgb(0xa6, 0xd1, 0x89),
    danger: rgb(0xe7, 0x82, 0x84),
    surface: rgb(0x41, 0x45, 0x59),
    diff_add_bg: rgb(0x39, 0x52, 0x46),
    diff_del_bg: rgb(0x55, 0x3d, 0x46),
};

// Latte is Catppuccin's light flavor: it sets light-tuned roles, but Iris does
// not own the full-screen background, so it looks correct only on a light
// terminal. Diff backgrounds are pale tints of the light Base.
static CATPPUCCIN_LATTE: FixedTheme = FixedTheme {
    id: "catppuccin-latte",
    name: "Catppuccin Latte",
    border: rgb(0x9c, 0xa0, 0xb0),
    accent: rgb(0xfe, 0x64, 0x0b),
    interactive: rgb(0x17, 0x92, 0x99),
    success: rgb(0x40, 0xa0, 0x2b),
    danger: rgb(0xd2, 0x0f, 0x39),
    surface: rgb(0xcc, 0xd0, 0xda),
    diff_add_bg: rgb(0xd1, 0xe9, 0xcd),
    diff_del_bg: rgb(0xef, 0xce, 0xd7),
};

static NORD: FixedTheme = FixedTheme {
    id: "nord",
    name: "Nord",
    border: rgb(0x4c, 0x56, 0x6a),
    accent: rgb(0xd0, 0x87, 0x70),
    interactive: rgb(0x88, 0xc0, 0xd0),
    success: rgb(0xa3, 0xbe, 0x8c),
    danger: rgb(0xbf, 0x61, 0x6a),
    surface: rgb(0x3b, 0x42, 0x52),
    diff_add_bg: rgb(0x33, 0x3f, 0x36),
    diff_del_bg: rgb(0x43, 0x33, 0x38),
};

static TOKYO_NIGHT: FixedTheme = FixedTheme {
    id: "tokyo-night",
    name: "Tokyo Night",
    border: rgb(0x41, 0x48, 0x68),
    accent: rgb(0xff, 0x9e, 0x64),
    interactive: rgb(0x7a, 0xa2, 0xf7),
    success: rgb(0x9e, 0xce, 0x6a),
    danger: rgb(0xf7, 0x76, 0x8e),
    surface: rgb(0x24, 0x28, 0x3b),
    diff_add_bg: rgb(0x20, 0x30, 0x2b),
    diff_del_bg: rgb(0x37, 0x22, 0x2b),
};

static DRACULA: FixedTheme = FixedTheme {
    id: "dracula",
    name: "Dracula",
    border: rgb(0x62, 0x72, 0xa4),
    accent: rgb(0xff, 0xb8, 0x6c),
    interactive: rgb(0x8b, 0xe9, 0xfd),
    success: rgb(0x50, 0xfa, 0x7b),
    danger: rgb(0xff, 0x55, 0x55),
    surface: rgb(0x44, 0x47, 0x5a),
    diff_add_bg: rgb(0x21, 0x3a, 0x2b),
    diff_del_bg: rgb(0x45, 0x25, 0x2c),
};

static ROSE_PINE: FixedTheme = FixedTheme {
    id: "rose-pine",
    name: "Rose Pine",
    border: rgb(0x6e, 0x6a, 0x86),
    accent: rgb(0xf6, 0xc1, 0x77),
    interactive: rgb(0x9c, 0xcf, 0xd8),
    success: rgb(0x31, 0x74, 0x8f),
    danger: rgb(0xeb, 0x6f, 0x92),
    surface: rgb(0x1f, 0x1d, 0x2e),
    diff_add_bg: rgb(0x1b, 0x2e, 0x33),
    diff_del_bg: rgb(0x37, 0x22, 0x2e),
};

static SOLARIZED: FixedTheme = FixedTheme {
    id: "solarized",
    name: "Solarized Dark",
    border: rgb(0x58, 0x6e, 0x75),
    accent: rgb(0xb5, 0x89, 0x00),
    interactive: rgb(0x2a, 0xa1, 0x98),
    success: rgb(0x85, 0x99, 0x00),
    danger: rgb(0xdc, 0x32, 0x2f),
    surface: rgb(0x07, 0x36, 0x42),
    diff_add_bg: rgb(0x0a, 0x3a, 0x2e),
    diff_del_bg: rgb(0x3a, 0x1f, 0x22),
};

static EVERFOREST: FixedTheme = FixedTheme {
    id: "everforest",
    name: "Everforest Dark",
    border: rgb(0x85, 0x92, 0x89),
    accent: rgb(0xdb, 0xbc, 0x7f),
    interactive: rgb(0x83, 0xc0, 0x92),
    success: rgb(0xa7, 0xc0, 0x80),
    danger: rgb(0xe6, 0x7e, 0x80),
    surface: rgb(0x3d, 0x48, 0x4d),
    diff_add_bg: rgb(0x3a, 0x40, 0x2f),
    diff_del_bg: rgb(0x4b, 0x2f, 0x33),
};

/// All selectable theme ids, in picker order (adaptive default first).
pub(crate) fn available() -> &'static [&'static str] {
    &[
        "terminal",
        "gruvbox",
        "catppuccin-latte",
        "catppuccin-frappe",
        "catppuccin-macchiato",
        "catppuccin-mocha",
        "nord",
        "tokyo-night",
        "dracula",
        "rose-pine",
        "solarized",
        "everforest",
    ]
}

/// Resolve a known theme id to its definition. Returns `None` for unknown ids so
/// the config layer can distinguish a typo from an intentional `terminal`
/// selection and emit a diagnostic; callers that want the adaptive default on
/// miss should use [`set_active`] or `resolve(id).unwrap_or(default())`.
pub(crate) fn resolve(id: &str) -> Option<&'static dyn Theme> {
    match id {
        "terminal" => Some(&TERMINAL),
        "gruvbox" => Some(&GRUVBOX),
        "catppuccin-latte" => Some(&CATPPUCCIN_LATTE),
        "catppuccin-frappe" => Some(&CATPPUCCIN_FRAPPE),
        "catppuccin-macchiato" => Some(&CATPPUCCIN_MACCHIATO),
        "catppuccin-mocha" => Some(&CATPPUCCIN_MOCHA),
        // Legacy alias: `catppuccin` shipped as Mocha before the flavors split.
        "catppuccin" => Some(&CATPPUCCIN_MOCHA),
        "nord" => Some(&NORD),
        "tokyo-night" => Some(&TOKYO_NIGHT),
        "dracula" => Some(&DRACULA),
        "rose-pine" => Some(&ROSE_PINE),
        "solarized" => Some(&SOLARIZED),
        "everforest" => Some(&EVERFOREST),
        _ => None,
    }
}

/// The adaptive default theme (terminal-relative).
pub(crate) fn default() -> &'static dyn Theme {
    &TERMINAL
}

/// Process-wide active theme. Defaults to the adaptive terminal theme.
static ACTIVE: RwLock<&'static dyn Theme> = RwLock::new(&TERMINAL);

/// The currently active theme. Recovers from lock poisoning rather than
/// panicking: theme state is cosmetic and must not crash the UI.
pub(crate) fn active() -> &'static dyn Theme {
    *ACTIVE.read().unwrap_or_else(|e| e.into_inner())
}

/// Set the active theme by id. Unknown ids log a warning and fall back to the
/// adaptive default instead of silently masking a typo.
pub(crate) fn set_active(id: &str) {
    let theme = resolve(id).unwrap_or_else(|| {
        tracing::warn!(theme = id, "unknown theme id; using adaptive default");
        default()
    });
    *ACTIVE.write().unwrap_or_else(|e| e.into_inner()) = theme;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_returns_requested_theme() {
        assert_eq!(resolve("gruvbox").unwrap().id(), "gruvbox");
        assert_eq!(
            resolve("catppuccin-mocha").unwrap().id(),
            "catppuccin-mocha"
        );
        assert_eq!(resolve("nord").unwrap().id(), "nord");
        assert_eq!(resolve("tokyo-night").unwrap().id(), "tokyo-night");
    }

    #[test]
    fn all_four_catppuccin_flavors_resolve() {
        for id in [
            "catppuccin-latte",
            "catppuccin-frappe",
            "catppuccin-macchiato",
            "catppuccin-mocha",
        ] {
            let t = resolve(id).unwrap();
            assert_eq!(t.id(), id);
            // Flavors are distinct fixed-RGB palettes, not the same table.
            assert!(matches!(t.accent(), Color::Rgb(..)));
        }
        // The four flavors carry different accent (Peach) values.
        let accents: std::collections::HashSet<_> = [
            "catppuccin-latte",
            "catppuccin-frappe",
            "catppuccin-macchiato",
            "catppuccin-mocha",
        ]
        .into_iter()
        .map(|id| format!("{:?}", resolve(id).unwrap().accent()))
        .collect();
        assert_eq!(accents.len(), 4);
    }

    #[test]
    fn legacy_catppuccin_id_aliases_mocha() {
        // Configs saved before the flavor split used the bare `catppuccin` id.
        assert_eq!(resolve("catppuccin").unwrap().id(), "catppuccin-mocha");
    }

    #[test]
    fn unknown_id_is_none_and_default_is_terminal() {
        assert!(resolve("does-not-exist").is_none());
        assert_eq!(default().id(), "terminal");
    }

    #[test]
    fn available_ids_all_resolve_to_themselves() {
        for id in available() {
            assert_eq!(resolve(id).unwrap().id(), *id);
        }
    }

    #[test]
    fn terminal_theme_matches_palette_roles() {
        let t = resolve("terminal").unwrap();
        assert_eq!(t.border(), palette::BORDER);
        assert_eq!(t.accent(), palette::ORANGE);
        assert_eq!(t.interactive(), palette::CYAN);
        assert_eq!(t.success(), palette::GREEN);
        assert_eq!(t.danger(), palette::RED);
        assert_eq!(t.surface(), palette::SURFACE);
        assert_eq!(t.diff_add_bg(), palette::DIFF_ADD_BG);
        assert_eq!(t.diff_del_bg(), palette::DIFF_DEL_BG);
    }

    #[test]
    fn named_themes_use_fixed_rgb_not_ansi_slots() {
        // Opt-in themes deliberately override the terminal palette.
        assert_ne!(resolve("gruvbox").unwrap().success(), palette::GREEN);
        assert!(matches!(resolve("nord").unwrap().border(), Color::Rgb(..)));
    }
}
