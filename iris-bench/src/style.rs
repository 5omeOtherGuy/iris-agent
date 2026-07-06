//! Local visual tokens for the iris-bench TUI — the bench control surface's
//! copy of the Iris design language.
//!
//! iris-bench is a separate crate that drives the agent ONLY through the
//! `iris_agent::harness` façade (see `Cargo.toml`); it deliberately does not
//! reach into the agent's `pub(crate)` UI internals (`src/ui/palette.rs`,
//! `src/ui/symbols.rs`). So rather than import those, this module *re-states*
//! the canonical color roles and state glyphs from
//! `docs/TUI_DESIGN_LANGUAGE.md` (§2 Color, §5 Symbol vocabulary), keeping the
//! benchmark UI visually part of Iris while preserving the façade boundary.
//! Keep these values in sync with that document.
//!
//! The two laws of color hold here as in the agent: color is a *point* signal
//! paired with a symbol + label, never a fill behind a region; and every state
//! stays legible in monochrome, because the glyph — not the hue — carries it.

use ratatui::style::{Color, Style};

// ── Foundation — grey does the structural work (§2.1) ─────────────────────

/// `border` — rules & frames. ANSI Gray, so it inherits the user's own
/// light/dark terminal theme.
pub const BORDER: Color = Color::Gray;

/// `muted` — metadata, hints, markers, elisions, empty meter slots. ANSI
/// DarkGray.
pub const MUTED: Color = Color::DarkGray;

// ── Signal — sparse, role-assigned (§2.2) ─────────────────────────────────

/// `accent` (orange) — active mode, the running LED, the meter edge dot,
/// warnings. Bound to the terminal's yellow/accent slot, exactly as the agent
/// binds `ORANGE` in `palette.rs`.
pub const ACCENT: Color = Color::Yellow;

/// `interactive` (cyan) — the title / current focus.
pub const INTERACTIVE: Color = Color::Cyan;

/// `success` (green) — DONE, a passed check, measured savings.
pub const SUCCESS: Color = Color::Green;

/// `danger` (red) — ERROR / a harness failure / a token regression.
pub const DANGER: Color = Color::Red;

// ── Symbol vocabulary — one job per glyph (§5) ────────────────────────────

/// `●` running / live LED — the only animated glyph.
pub const RUNNING: &str = "\u{25cf}";
/// `◆` done / a passed success-check.
pub const DONE: &str = "\u{25c6}";
/// `▲` warning / review — a cell that ran but did not pass its check.
pub const WARN: &str = "\u{25b2}";
/// `■` error / denied — a cell that could not run at all.
pub const ERROR: &str = "\u{25a0}";
/// `○` queued / empty meter slot.
pub const QUEUED: &str = "\u{25cb}";

/// `┊` soft metadata separator. NOT an ASCII pipe.
pub const SEP: &str = "\u{250a}";
/// `─` rule / frame line.
pub const RULE: &str = "\u{2500}";
/// `↑` input-token telemetry.
pub const IN_TOK: &str = "\u{2191}";
/// `…` elision. NOT three ASCII dots.
pub const ELLIPSIS: &str = "\u{2026}";
/// `−` Unicode minus — a reduction (negative delta). NOT ASCII `-`.
pub const MINUS: &str = "\u{2212}";

/// The 4-cell LED-chase working-indicator strip. The lit cell bounces across
/// it (§6, §7.7); position comes from [`chase_pos`].
pub const CHASE_LEN: usize = 4;

/// User preferences that gate color and motion. Read once from the environment
/// so the render path is a pure function of them: this is what lets the
/// monochrome test (§12) and reduced-motion (§6) be real rather than aspirational.
#[derive(Clone, Copy, Debug, Default)]
pub struct Prefs {
    /// `NO_COLOR` set: render every span without a foreground color, proving
    /// the UI is fully legible from symbol + label + position alone.
    pub mono: bool,
    /// `IRIS_REDUCED_MOTION` set: hold the working indicator static instead of
    /// running the LED chase.
    pub reduced_motion: bool,
}

impl Prefs {
    /// Read `NO_COLOR` and `IRIS_REDUCED_MOTION` from the environment. Any
    /// non-empty value counts as set (the `NO_COLOR` convention).
    pub fn from_env() -> Self {
        let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
        Prefs {
            mono: set("NO_COLOR"),
            reduced_motion: set("IRIS_REDUCED_MOTION"),
        }
    }

    /// A foreground style for `color`, or a bare style when in monochrome mode.
    /// Every colored span in the TUI flows through here.
    pub fn fg(self, color: Color) -> Style {
        if self.mono {
            Style::default()
        } else {
            Style::default().fg(color)
        }
    }
}

/// The lit-cell position (`0..CHASE_LEN`) for the LED chase at `elapsed_ms`.
/// Ping-pongs so the head reverses at the ends instead of wrapping, matching
/// the agent's working indicator. Advances one cell per `step_ms`.
pub fn chase_pos(elapsed_ms: u128, step_ms: u128) -> usize {
    let step = step_ms.max(1);
    let period = (2 * (CHASE_LEN - 1)) as u128; // 0,1,2,3,2,1,-> repeat
    let phase = (elapsed_ms / step) % period;
    let n = (CHASE_LEN - 1) as u128;
    (if phase <= n { phase } else { period - phase }) as usize
}

/// Render the 4-cell working strip with the lit head at `pos` (dim `·` cells,
/// one bright accent `●`). `pos >= CHASE_LEN` (e.g. reduced motion) pins the
/// head at the first cell — the static `●···` readout.
pub fn chase_cells(pos: usize) -> [&'static str; CHASE_LEN] {
    let head = if pos >= CHASE_LEN { 0 } else { pos };
    let mut cells = ["\u{b7}"; CHASE_LEN]; // middle dot ·
    cells[head] = RUNNING;
    cells
}

/// A compact human token count: `900`, `1.4k`, `14.2k`, `1.2M`. Mirrors the
/// agent's `↑14.2k` telemetry formatting.
pub fn humanize_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// A signed percentage in Iris punctuation: `−18.4%` for a reduction (Unicode
/// minus), `+4.2%`, `±0.0%`. Used for the live defaults-vs-baseline delta.
pub fn signed_pct(pct: f64) -> String {
    if pct < -0.05 {
        format!("{}{:.1}%", MINUS, -pct)
    } else if pct > 0.05 {
        format!("+{pct:.1}%")
    } else {
        "\u{b1}0.0%".to_string() // ±0.0%
    }
}

/// Truncate `s` to `max` display cells, appending `…` when clipped. Widths are
/// counted in chars (every glyph this UI shows is single-width).
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push_str(ELLIPSIS);
    out
}

/// Truncate from the left, keeping the tail (for long paths): `…runs.jsonl`.
pub fn truncate_left(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let keep = max.saturating_sub(1);
    let tail: String = s.chars().skip(count - keep).collect();
    format!("{ELLIPSIS}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_matches_iris_telemetry() {
        assert_eq!(humanize_tokens(38), "38");
        assert_eq!(humanize_tokens(999), "999");
        assert_eq!(humanize_tokens(1_400), "1.4k");
        assert_eq!(humanize_tokens(14_200), "14.2k");
        assert_eq!(humanize_tokens(1_200_000), "1.2M");
    }

    #[test]
    fn signed_pct_uses_unicode_minus() {
        assert_eq!(signed_pct(-18.4), "−18.4%");
        assert!(!signed_pct(-18.4).contains('-')); // ASCII hyphen banned
        assert_eq!(signed_pct(4.2), "+4.2%");
        assert_eq!(signed_pct(0.0), "±0.0%");
    }

    #[test]
    fn chase_ping_pongs_without_wrapping() {
        // step 250ms: 0,1,2,3 then back 2,1, then 0,1,2,3 ...
        let seq: Vec<usize> = (0..8).map(|i| chase_pos(i * 250, 250)).collect();
        assert_eq!(seq, vec![0, 1, 2, 3, 2, 1, 0, 1]);
        // Never leaves the strip.
        for ms in 0..5_000 {
            assert!(chase_pos(ms, 130) < CHASE_LEN);
        }
    }

    #[test]
    fn chase_cells_light_one_head() {
        let c = chase_cells(2);
        assert_eq!(c[2], RUNNING);
        assert_eq!(c.iter().filter(|x| **x == RUNNING).count(), 1);
        // Out-of-range pins to the first cell (reduced motion).
        assert_eq!(chase_cells(99)[0], RUNNING);
    }

    #[test]
    fn truncate_adds_ellipsis_only_when_clipped() {
        assert_eq!(truncate("rename", 10), "rename");
        assert_eq!(truncate("chained_provider_fix", 8), "chained…");
        assert_eq!(
            truncate_left("target/iris-bench-runs.jsonl", 12),
            "…-runs.jsonl"
        );
        assert_eq!(truncate_left("short.jsonl", 20), "short.jsonl");
    }

    #[test]
    fn mono_pref_drops_foreground() {
        let color = Prefs {
            mono: true,
            reduced_motion: false,
        };
        assert_eq!(color.fg(ACCENT), Style::default());
        let normal = Prefs::default();
        assert_eq!(normal.fg(ACCENT), Style::default().fg(ACCENT));
    }
}
