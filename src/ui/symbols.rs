//! Canonical Iris symbol vocabulary — the single source of truth for the state
//! glyphs and transcript markers defined in `docs/TUI_DESIGN_LANGUAGE.md`
//! §"Symbol Vocabulary" and the design-system skill (`StateSymbol`).
//!
//! Each glyph has exactly one job. `●` is the live/running LED and meter fill;
//! settled states get their own glyph so a header stays legible without color
//! (every colored state is paired with one of these symbols *and* a label). Box
//! -drawing frame characters (`┌ ┐ └ ┘ │ ─ ├ ┤`) are structural and stay inline
//! in the panel renderers; only the state/marker vocabulary is centralized here.
//!
//! Boundary: single glyphs shared across renderers live here and call sites
//! reference the constants. Composed frame/animation strings (e.g.
//! `WORKING_FRAMES` in `src/ui/tui.rs`) are compositions, not vocabulary, and
//! stay local; single-call-site decorations that appear in exactly one file
//! (e.g. the `◉`/`○` radio marks in `src/ui/modal.rs`) may also stay local.

/// `›` — the user transcript marker: the one turn the transcript marks, so the
/// eye can scan back to what was asked. The agent speaks unmarked. Never a state
/// dot.
pub(crate) const USER: &str = "\u{203a}";

/// `◉` — active / selected mode (composer top-frame mode glyph, picker rows).
pub(crate) const ACTIVE: &str = "\u{25c9}";

/// `●` — running LED / live activity / meter fill. The only animated glyph.
pub(crate) const RUNNING: &str = "\u{25cf}";

/// `◆` — done / approved (settled success).
pub(crate) const DONE: &str = "\u{25c6}";

/// `◇` — preview / pending (an edit awaiting apply).
pub(crate) const PREVIEW: &str = "\u{25c7}";

/// `■` — error / denied (settled failure).
pub(crate) const ERROR: &str = "\u{25a0}";

/// `▲` — review / warning (a gated action awaiting the user's decision).
pub(crate) const REVIEW: &str = "\u{25b2}";

/// `□` — skipped / cancelled / neutral.
pub(crate) const CANCELLED: &str = "\u{25a1}";

/// `○` — queued / empty meter slot.
pub(crate) const EMPTY: &str = "\u{25cb}";

/// `▾` — expanded disclosure (full output shown).
pub(crate) const EXPANDED: &str = "\u{25be}";

/// `▸` — collapsed disclosure (capped preview; hidden lines elided).
pub(crate) const COLLAPSED: &str = "\u{25b8}";

/// `+` — added line (diff).
pub(crate) const ADDED: &str = "+";

/// `−` (U+2212 MINUS SIGN) — removed line (diff). Not ASCII `-`.
pub(crate) const REMOVED: &str = "\u{2212}";

/// `┊` — soft metadata separator (working indicator, turn divider, workspace
/// label, reasoning left rail). Not an ASCII pipe.
pub(crate) const SEP: &str = "\u{250a}";

/// `·` (U+00B7) — unlit meter cell: a slot that could light but is dark right
/// now. Shared by the LED chase's dark cells (`●···`, composed locally in
/// `WORKING_FRAMES`) and the flow meter's unlit cells, so the two instruments
/// speak one vocabulary (§5).
pub(crate) const UNLIT: &str = "\u{b7}";

/// ` ▏▎▍▌▋▊▉█` — the flow meter's left-anchored eighth-block fill ramp
/// (U+2588–U+258F), indexed by how many of a cell's eight quanta are lit.
/// Index 0 is never rendered: a cell with no lit eighths shows [`UNLIT`] (or
/// the peak tick), not a blank.
pub(crate) const FLOW_FILL: [char; 9] = [
    ' ', '\u{258f}', '\u{258e}', '\u{258d}', '\u{258c}', '\u{258b}', '\u{258a}', '\u{2589}',
    '\u{2588}',
];

/// `▏` — the flow meter's peak-hold tick: the 1/8-fill glyph doubling (dim, in
/// an unlit cell) as the mark for the decaying high-water quantum (§5).
pub(crate) const FLOW_PEAK: char = FLOW_FILL[1];

/// `⇡` — commits ahead of the last-fetched upstream (git dropdown status
/// line). One job only; `↑` remains input-token telemetry.
pub(crate) const AHEAD: &str = "\u{21e1}";

/// `⇣` — commits behind the last-fetched upstream. One job only; `↓` remains
/// output-token telemetry.
pub(crate) const BEHIND: &str = "\u{21e3}";

/// `±` — uncommitted modification relative to committed state: diff modified
/// rows, the session-bar dirty count, and user-attributed dirty files. One
/// meaning everywhere.
pub(crate) const DIRTY: &str = "\u{b1}";
