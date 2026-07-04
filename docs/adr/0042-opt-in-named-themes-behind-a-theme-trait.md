# ADR-0042: Opt-in named color themes behind a Theme trait, terminal-relative by default

**Date**: 2026-07-04
**Status**: proposed
**Deciders**: Iris maintainers

## Context

Iris' palette is a fixed set of ANSI-slot constants in `src/ui/palette.rs`, and
`docs/TUI_DESIGN_LANGUAGE.md` §2 makes that a design law: "Every role binds to
an ANSI named slot so Iris inherits the user's own light/dark terminal theme."
The whole render layer reaches the palette through ~9 semantic roles (BORDER,
ORANGE/accent, CYAN/interactive, GREEN/success, RED/danger, SURFACE, diff
backgrounds); the many `Color::` references in `terminal_surface.rs` are an
ANSI-serialization map, not UI colors. So the themeable surface is genuinely
small and already centralized.

Users want popular fixed palettes — Gruvbox, Catppuccin, Nord, Tokyo Night.
Those are RGB palettes that deliberately *override* the terminal theme, which is
the opposite of Iris' terminal-relative default. We need named themes without
breaking the default that respects the user's terminal, `NO_COLOR`, light mode,
and the two laws of color (color is a point signal; never color alone).

## Decision

Introduce a `Theme` trait as the single indirection point behind
`src/ui/palette.rs`. Roles become methods on the active theme instead of module
constants. Themes cover only the eight existing palette roles
(border, accent, interactive, success, danger, surface, and the two diff-row
backgrounds); they do **not** own a canvas background, ink, or muted color —
painting a full background would violate color law 1 ("never color a whole
region"). Ship the current ANSI-slot palette as the default `TerminalTheme`
impl (unchanged behavior), plus a set of opt-in fixed-RGB themes
(`gruvbox`, `catppuccin`, `nord`, `tokyo-night`, `dracula`, `rose-pine`,
`solarized`, `everforest`) selected by a `theme` config field. Themes are defined in-tree; we do **not** depend on `ratatui-themekit` or
`ratatui-themes`. `NO_COLOR` and `--plain` continue to force the terminal/no-color
path regardless of the configured theme.

## Alternatives Considered

### Alternative 1: Depend on `ratatui-themekit`
- **Pros**: Ships 11 themes incl. gruvbox/catppuccin/nord/tokyo-night; a
  `Theme` trait, a `BUILTIN_THEMES` registry, `resolve_theme(id)`, serde custom
  themes, and `NO_COLOR` support — most of this ADR's surface, prebuilt.
- **Cons**: Pre-1.0 (v0.6.1), ~3 months old, ~1k downloads, 0 stars, single
  author. Its builders (`ThemeExt`, line/block/status compositors) are a second
  styling API competing with Iris' own render code; 15 required trait methods,
  much of it we don't use. A core UI subsystem would ride on a fragile dep.
- **Why not**: Reuse-before-handroll (AGENTS.md) weighs adoption against
  maturity and fit. Our themeable surface is ~9 roles; the trait + palettes are
  ~150 lines. The dependency's blast radius and churn risk exceed the code it
  saves.

### Alternative 2: Keep ANSI-only; never ship fixed-RGB themes
- **Pros**: Zero new code; honors the terminal-relative law purely; always
  correct under `NO_COLOR` and any terminal theme.
- **Cons**: Does not deliver the requested Gruvbox/Catppuccin/Nord/Tokyo Night
  palettes; users who want a self-contained look can't get one.
- **Why not**: The request is explicitly for named themes; "use your terminal
  theme" is not an answer for users who want Iris to look like Catppuccin
  regardless of terminal.

### Alternative 3: Runtime CSS/token loading from `tokens/colors.css`
- **Pros**: One source of truth already exists for tokens; could load palettes
  from data files.
- **Why not**: Over-engineered for a handful of palettes; adds a parser and file I/O on
  the UI path for values that are static and few. A `Theme` impl per palette is
  simpler and type-checked.

## Consequences

### Positive
- Named themes ship without breaking the terminal-relative default — the ANSI
  path stays the default and the `NO_COLOR`/`--plain` behavior is unchanged.
- One indirection point (`Theme` behind `palette.rs`) keeps the render layer
  untouched: call sites move from `palette::GREEN` to `theme.success()`.
- No new dependency; palettes are type-checked in-tree and easy to extend.
- Foundation for a later `/theme` picker and config persistence (out of scope
  here).

### Negative
- Fixed-RGB themes deliberately ignore the user's terminal theme — a documented
  exception to §2's terminal-relative law, allowed only for opt-in themes.
- Palette access changes from constants to a passed/borrowed theme handle;
  every current `palette::ROLE` site must thread the active theme.

### Risks
- **Color laws regression**: a fixed-RGB theme could tempt full-region fills.
  Mitigation: themes only supply role colors; the two laws (point signal,
  symbol+label, monochrome-legible) are enforced by render code, not the theme.
- **Contrast on light terminals**: named themes recolor point signals plus the
  one permitted `surface` fill and diff-row tints, but keep the terminal's own
  background (color law 1 forbids painting the canvas). A fixed *dark* theme's
  surface/diff tints can therefore read poorly on a *light* terminal.
  Mitigation: the default stays terminal-relative and adaptive; named themes are
  opt-in and documented as non-adaptive point-signal recolorings, not full
  self-contained backgrounds.
- **Threading churn**: converting constant call sites to a theme handle touches
  several files. Mitigation: keep the trait behind `palette.rs` and expose a
  process-wide active theme accessor so call-site changes are mechanical.
