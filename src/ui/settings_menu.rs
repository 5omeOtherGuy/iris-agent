//! The settings panel — the faceplate (Tier 3, presentation-only, harness-free).
//!
//! `/settings` is ONE flat control surface, not a category tree: every setting
//! is a row on a silkscreened panel (dim uppercase section headers), adjusted
//! **in place** with `←`/`→` like clicking a physical detent. No sub-menu is
//! ever opened to change a value. Each row is one of four control archetypes —
//! a closed set, like the four tool families:
//!
//! - **switch** — a fixed vocabulary printed as a labeled detent track
//!   (`○ strict  ◉ auto  ○ never`); `←`/`→` click between positions and CLAMP
//!   at the ends (a real switch never wraps). Bools are two-position switches.
//!   When the track does not fit the width, it degrades to its **rotary** form:
//!   position dots + the selected value (`○○◉  auto`).
//! - **dial** — a numeric on a 10-detent LED ladder (`●●●●●●○○○○  232k tokens`),
//!   the house meter idiom; `←`/`→` step to the neighbouring detent, `↵` opens
//!   an inline register for a precise value. The printed number is always the
//!   TRUE persisted value (numbers are honest); the fill is its detent.
//! - **register** — free text edited inline on the row (`▋` caret); `↵` edits,
//!   `↵` again saves, `esc` cancels. An empty buffer clears the key when the
//!   field allows it.
//! - **port** — a `▸` row that opens a deeper surface (model picker, project
//!   permissions, scoped models, login). The loop returns to the panel when
//!   that surface closes, so the panel is home.
//!
//! Every widget here is pure: it turns a [`ModalKey`] into a [`ModalOutcome`],
//! and the loop ([`crate::ui::picker::apply_action`]) performs the disk writes
//! at the safe inter-turn boundary. A change is acknowledged mechanically: the
//! adjusted element renders bright for two ticks (the §6 detent flash), gated
//! by reduced motion. A dependent control (the microcompaction watermark) goes
//! dark while its master switch is off — inert hardware, still operable.
//!
//! All writes go to the user-global settings file via `config::save_*`;
//! global-vs-project scope governs only load/merge precedence in
//! [`crate::config::Settings::merged_with`]. Two fields are GLOBAL-ONLY so a
//! cloned project cannot lower posture: `defaultApproval` and
//! `promptCacheRetention`. Deliberately absent from the faceplate (service
//! hatch: `settings.json` only): `bashToolMode`, `maxToolRoundtrips`, retry
//! tuning, and the OpenAI-compatible endpoint block.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::mimir::selection::ReasoningEffort;
use crate::ui::modal::{ModalAction, ModalKey, ModalOutcome, dim};

/// Crate rev printed on the masthead (the panel's silkscreen, same source as
/// the start page and the exit receipt).
const REV: &str = env!("CARGO_PKG_VERSION");

/// Label column width: two-cell indent + label, control column after. Wide
/// enough that the longest label (`microcompaction`, 15) keeps a real gutter.
/// Shared with the model picker's reasoning track so the two surfaces sit on
/// one grid.
pub(crate) const LABEL_W: usize = 18;

/// Detent-flash duration in loop ticks (the §6 two-tick acknowledgment).
const FLASH_TICKS: u8 = 2;

/// Default line budget when the render path supplies none: matches the legacy
/// 16-row docked-menu cap minus its two inset rows.
const DEFAULT_LINE_BUDGET: usize = 14;

/// A persisted setting adjusted in place on the panel. Pruned relative to the
/// full `Settings` struct — see the module doc for the service-hatch list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    AltScreen,
    ScrollSpeed,
    ReducedMotion,
    Theme,
    DefaultApproval,
    ContextTokenBudget,
    CompactionSummarizer,
    Microcompaction,
    MicrocompactionWatermark,
    PromptCacheRetention,
    VerifyCommand,
    VerifyMaxAttempts,
    WorktreeRoot,
}

/// One row of the faceplate. `Field` rows edit persisted settings; the rest
/// are the session-live controls (reasoning, skip-approvals) and the ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowId {
    Model,
    Reasoning,
    Scope,
    Providers,
    SkipApprovals,
    Permissions,
    Field(Field),
}

/// A silkscreened section: dim uppercase title + its rows, in panel order.
struct Section {
    title: &'static str,
    rows: &'static [RowId],
}

/// The faceplate, top to bottom: what runs → what it may do → what it
/// remembers → how it self-checks → the panel itself → where it works.
const SECTIONS: &[Section] = &[
    Section {
        title: "ENGINE",
        rows: &[
            RowId::Model,
            RowId::Reasoning,
            RowId::Scope,
            RowId::Providers,
        ],
    },
    Section {
        title: "SAFETY",
        rows: &[
            RowId::Field(Field::DefaultApproval),
            RowId::SkipApprovals,
            RowId::Permissions,
        ],
    },
    Section {
        title: "MEMORY",
        rows: &[
            RowId::Field(Field::ContextTokenBudget),
            RowId::Field(Field::CompactionSummarizer),
            RowId::Field(Field::Microcompaction),
            RowId::Field(Field::MicrocompactionWatermark),
            RowId::Field(Field::PromptCacheRetention),
        ],
    },
    Section {
        title: "CHECKS",
        rows: &[
            RowId::Field(Field::VerifyCommand),
            RowId::Field(Field::VerifyMaxAttempts),
        ],
    },
    Section {
        title: "PANEL",
        rows: &[
            RowId::Field(Field::Theme),
            RowId::Field(Field::AltScreen),
            RowId::Field(Field::ScrollSpeed),
            RowId::Field(Field::ReducedMotion),
        ],
    },
    Section {
        title: "GIT",
        rows: &[RowId::Field(Field::WorktreeRoot)],
    },
];

/// Ten-detent ladders for the dials. Ten positions so every dial IS the house
/// 10-dot meter: one click, one LED.
const BUDGET_LADDER: [u64; 10] = [
    64_000, 96_000, 128_000, 160_000, 200_000, 232_000, 300_000, 400_000, 600_000, 1_000_000,
];
const WATERMARK_LADDER: [u64; 10] = [
    8_000, 12_000, 16_000, 24_000, 32_000, 48_000, 64_000, 96_000, 128_000, 192_000,
];
const UNIT_LADDER: [u64; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

/// Hard bounds for typed dial entry, matching the `config::save_*` clamps.
fn dial_bounds(field: Field) -> (u64, u64) {
    match field {
        Field::ContextTokenBudget | Field::MicrocompactionWatermark => (1_000, 100_000_000),
        Field::ScrollSpeed => (1, 100),
        Field::VerifyMaxAttempts => (1, 10),
        _ => (0, u64::MAX),
    }
}

fn ladder(field: Field) -> &'static [u64] {
    match field {
        Field::ContextTokenBudget => &BUDGET_LADDER,
        Field::MicrocompactionWatermark => &WATERMARK_LADDER,
        _ => &UNIT_LADDER,
    }
}

/// Compact dial value in the ONE house token format (`232k`, `12.5k`, `1m`) —
/// the same formatter the meter, divider, and receipt print, so a number
/// never reads differently on the panel than in the transcript.
fn compact_value(value: u64) -> String {
    crate::ui::tui::compact_count(value)
}

/// Current persisted values plus the live session state the panel controls
/// (reasoning levels for the active model, skip-approvals). Read once by the
/// loop from [`crate::config::Settings`]; pure data, so the panel stays
/// harness-free and unit-testable.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    /// Qualified `provider/model` id of the persisted default.
    pub(crate) default_model: String,
    /// Reasoning levels the ACTIVE model supports, panel order.
    pub(crate) reasoning_levels: Vec<(ReasoningEffort, &'static str)>,
    /// The active reasoning level (clamped to the model).
    pub(crate) reasoning: ReasoningEffort,
    /// `/scoped-models` summary: `all models` or `3 of 7 enabled`.
    pub(crate) scope_summary: String,
    /// Authenticated provider count for the providers port.
    pub(crate) providers_connected: usize,
    pub(crate) default_approval: String,
    pub(crate) skip_permissions: bool,
    pub(crate) context_token_budget: u64,
    pub(crate) compaction_summarizer: String,
    pub(crate) microcompaction: bool,
    pub(crate) microcompaction_watermark: u64,
    pub(crate) prompt_cache_retention: String,
    pub(crate) verify_command: Option<String>,
    pub(crate) verify_max_attempts: u32,
    pub(crate) theme: String,
    pub(crate) alt_screen: String,
    pub(crate) scroll_speed: u16,
    pub(crate) reduced_motion: bool,
    pub(crate) worktree_root: Option<String>,
}

impl Snapshot {
    fn switch_options(&self, field: Field) -> &'static [&'static str] {
        match field {
            Field::AltScreen => &["auto", "always", "never"],
            Field::DefaultApproval => &["strict", "auto", "never"],
            Field::PromptCacheRetention => &["none", "short", "long"],
            Field::CompactionSummarizer => &["excerpts", "provider", "subagent"],
            Field::Theme => crate::ui::theme::available(),
            Field::Microcompaction | Field::ReducedMotion => &["off", "on"],
            _ => &[],
        }
    }

    fn switch_value(&self, field: Field) -> String {
        match field {
            Field::AltScreen => self.alt_screen.clone(),
            Field::DefaultApproval => self.default_approval.clone(),
            Field::PromptCacheRetention => self.prompt_cache_retention.clone(),
            Field::CompactionSummarizer => self.compaction_summarizer.clone(),
            Field::Theme => self.theme.clone(),
            Field::Microcompaction => on_off(self.microcompaction),
            Field::ReducedMotion => on_off(self.reduced_motion),
            _ => String::new(),
        }
    }

    fn set_switch_value(&mut self, field: Field, value: &str) {
        match field {
            Field::AltScreen => self.alt_screen = value.to_string(),
            Field::DefaultApproval => self.default_approval = value.to_string(),
            Field::PromptCacheRetention => self.prompt_cache_retention = value.to_string(),
            Field::CompactionSummarizer => self.compaction_summarizer = value.to_string(),
            Field::Theme => self.theme = value.to_string(),
            Field::Microcompaction => self.microcompaction = value == "on",
            Field::ReducedMotion => self.reduced_motion = value == "on",
            _ => {}
        }
    }

    fn dial_value(&self, field: Field) -> u64 {
        match field {
            Field::ContextTokenBudget => self.context_token_budget,
            Field::MicrocompactionWatermark => self.microcompaction_watermark,
            Field::ScrollSpeed => u64::from(self.scroll_speed),
            Field::VerifyMaxAttempts => u64::from(self.verify_max_attempts),
            _ => 0,
        }
    }

    fn set_dial_value(&mut self, field: Field, value: u64) {
        match field {
            Field::ContextTokenBudget => self.context_token_budget = value,
            Field::MicrocompactionWatermark => self.microcompaction_watermark = value,
            Field::ScrollSpeed => self.scroll_speed = value.min(u64::from(u16::MAX)) as u16,
            Field::VerifyMaxAttempts => self.verify_max_attempts = value.min(10) as u32,
            _ => {}
        }
    }

    fn register_value(&self, field: Field) -> Option<String> {
        match field {
            Field::VerifyCommand => self.verify_command.clone(),
            Field::WorktreeRoot => self.worktree_root.clone(),
            _ => None,
        }
    }

    fn set_register_value(&mut self, field: Field, value: Option<String>) {
        match field {
            Field::VerifyCommand => self.verify_command = value,
            Field::WorktreeRoot => self.worktree_root = value,
            _ => {}
        }
    }
}

fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_string()
}

/// How a row is adjusted/activated: drives both key handling and the footer's
/// keymap-honest verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Archetype {
    Switch,
    Dial,
    Register,
    Port,
}

fn archetype(row: RowId) -> Archetype {
    match row {
        RowId::Model | RowId::Scope | RowId::Providers | RowId::Permissions => Archetype::Port,
        RowId::Reasoning | RowId::SkipApprovals => Archetype::Switch,
        RowId::Field(field) => match field {
            Field::AltScreen
            | Field::DefaultApproval
            | Field::PromptCacheRetention
            | Field::CompactionSummarizer
            | Field::Theme
            | Field::Microcompaction
            | Field::ReducedMotion => Archetype::Switch,
            Field::ContextTokenBudget
            | Field::MicrocompactionWatermark
            | Field::ScrollSpeed
            | Field::VerifyMaxAttempts => Archetype::Dial,
            Field::VerifyCommand | Field::WorktreeRoot => Archetype::Register,
        },
    }
}

fn label(row: RowId) -> &'static str {
    match row {
        RowId::Model => "model",
        RowId::Reasoning => "reasoning",
        RowId::Scope => "model scope",
        RowId::Providers => "providers",
        RowId::SkipApprovals => "skip approvals",
        RowId::Permissions => "permissions",
        RowId::Field(field) => match field {
            Field::AltScreen => "alt screen",
            Field::ScrollSpeed => "scroll speed",
            Field::ReducedMotion => "reduced motion",
            Field::Theme => "theme",
            Field::DefaultApproval => "approvals",
            Field::ContextTokenBudget => "compact at",
            Field::CompactionSummarizer => "summarizer",
            Field::Microcompaction => "microcompaction",
            Field::MicrocompactionWatermark => "watermark",
            Field::PromptCacheRetention => "prompt cache",
            Field::VerifyCommand => "verify",
            Field::VerifyMaxAttempts => "attempts",
            Field::WorktreeRoot => "worktree root",
        },
    }
}

/// A dial's unit, printed dim after the honest value. Empty = bare number.
fn dial_unit(field: Field) -> &'static str {
    match field {
        Field::ContextTokenBudget | Field::MicrocompactionWatermark => " tokens",
        Field::ScrollSpeed => " lines",
        _ => "",
    }
}

/// The display list: every rendered row of the panel body, in order. Controls
/// carry their flat control index (the cursor space); headers and blanks are
/// skipped by navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DisplayRow {
    Header(&'static str),
    Blank,
    Control { index: usize, row: RowId },
}

fn display_rows() -> Vec<DisplayRow> {
    let mut out = Vec::new();
    let mut index = 0usize;
    for (i, section) in SECTIONS.iter().enumerate() {
        if i > 0 {
            out.push(DisplayRow::Blank);
        }
        out.push(DisplayRow::Header(section.title));
        for &row in section.rows {
            out.push(DisplayRow::Control { index, row });
            index += 1;
        }
    }
    out
}

fn controls() -> Vec<RowId> {
    SECTIONS
        .iter()
        .flat_map(|section| section.rows.iter().copied())
        .collect()
}

/// An in-place edit on a register (or a dial's typed precise value).
#[derive(Debug, Clone)]
struct Edit {
    buffer: String,
    numeric: bool,
    error: Option<&'static str>,
}

/// The settings panel modal. Owns its snapshot as display truth while open:
/// adjustments update it locally and emit the matching save action; the loop
/// keeps the panel (no rebuild) on success so detents click without jank.
#[derive(Debug, Clone)]
pub(crate) struct SettingsPanel {
    snap: Snapshot,
    controls: Vec<RowId>,
    cursor: usize,
    edit: Option<Edit>,
    /// Control index whose value renders bright, and ticks remaining.
    flash: Option<(usize, u8)>,
}

impl SettingsPanel {
    pub(crate) fn new(snapshot: Snapshot) -> Self {
        SettingsPanel {
            snap: snapshot,
            controls: controls(),
            cursor: 0,
            edit: None,
            flash: None,
        }
    }

    /// Re-open with the cursor on `row` (the port-return path).
    pub(crate) fn with_selected(snapshot: Snapshot, row: RowId) -> Self {
        let mut panel = SettingsPanel::new(snapshot);
        if let Some(pos) = panel.controls.iter().position(|&r| r == row) {
            panel.cursor = pos;
        }
        panel
    }

    fn selected(&self) -> RowId {
        self.controls[self.cursor]
    }

    /// Decay the detent flash one tick. Returns true while a flash is live so
    /// the loop keeps repainting until the element settles.
    pub(crate) fn tick(&mut self) -> bool {
        match self.flash.as_mut() {
            Some((_, ticks)) => {
                *ticks = ticks.saturating_sub(1);
                if *ticks == 0 {
                    self.flash = None;
                }
                true
            }
            None => false,
        }
    }

    /// Arm the two-tick acknowledgment on the adjusted control. Reduced motion
    /// settles instantly (§6: every motion degrades to its settled state).
    fn arm_flash(&mut self) {
        if !self.snap.reduced_motion {
            self.flash = Some((self.cursor, FLASH_TICKS));
        }
    }

    /// Arm the flash from outside: the loop rebuilds the panel after a model
    /// cycle (the model list lives beyond the snapshot) and still owes the
    /// row its mechanical acknowledgment.
    pub(crate) fn flash_selected(&mut self) {
        self.arm_flash();
    }

    /// Paste into an active edit (the loop routes `Event::Paste` here).
    /// Registers are single-line controls: interior line breaks collapse to
    /// spaces so a multi-line paste can never embed a newline in a saved value.
    pub(crate) fn push_str(&mut self, text: &str) {
        if let Some(edit) = self.edit.as_mut() {
            let mut flat = text.replace(['\r', '\n'], " ");
            flat.truncate(flat.trim_end().len());
            edit.buffer.push_str(&flat);
            edit.error = None;
        }
    }

    pub(crate) fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        if self.edit.is_some() {
            return self.handle_edit_key(key);
        }
        match key {
            ModalKey::Up => {
                self.cursor = if self.cursor == 0 {
                    self.controls.len() - 1
                } else {
                    self.cursor - 1
                };
                ModalOutcome::Redraw
            }
            ModalKey::Down => {
                self.cursor = if self.cursor + 1 >= self.controls.len() {
                    0
                } else {
                    self.cursor + 1
                };
                ModalOutcome::Redraw
            }
            ModalKey::Left => self.adjust(false),
            ModalKey::Right => self.adjust(true),
            ModalKey::Enter => self.activate(),
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    /// Click the selected control one detent left/right. Emits the save action
    /// when the position actually changes; a clamped end is a silent no-op
    /// (the switch is already against its stop).
    fn adjust(&mut self, forward: bool) -> ModalOutcome {
        match self.selected() {
            // The model row is a port AND a rotary: ←/→ cycles the scoped
            // models exactly like Ctrl+P (the loop rebuilds the panel on the
            // new model); ↵ still opens the full picker.
            RowId::Model => ModalOutcome::Emit(ModalAction::CycleModel { forward }),
            RowId::Reasoning => {
                let levels = &self.snap.reasoning_levels;
                let pos = levels
                    .iter()
                    .position(|(level, _)| *level == self.snap.reasoning)
                    .unwrap_or(0);
                let next = step_clamped(pos, levels.len(), forward);
                if next == pos {
                    return ModalOutcome::Ignore;
                }
                let level = levels[next].0;
                self.snap.reasoning = level;
                self.arm_flash();
                ModalOutcome::Emit(ModalAction::AdjustEffort(level))
            }
            RowId::SkipApprovals => {
                let target = forward;
                if self.snap.skip_permissions == target {
                    return ModalOutcome::Ignore;
                }
                self.snap.skip_permissions = target;
                self.arm_flash();
                ModalOutcome::Emit(ModalAction::ToggleSkipPermissions)
            }
            RowId::Field(field) => match archetype(RowId::Field(field)) {
                Archetype::Switch => {
                    let options = self.snap.switch_options(field);
                    if options.is_empty() {
                        return ModalOutcome::Ignore;
                    }
                    let current = self.snap.switch_value(field);
                    // A hand-edited value outside the vocabulary sits between
                    // detents: the first click snaps into the scale (right →
                    // first position, left → last), like a dial's off-ladder
                    // snap. A known position steps one detent and clamps.
                    let next = match options.iter().position(|o| *o == current) {
                        Some(pos) => {
                            let next = step_clamped(pos, options.len(), forward);
                            if next == pos {
                                return ModalOutcome::Ignore;
                            }
                            next
                        }
                        None if forward => 0,
                        None => options.len() - 1,
                    };
                    let value = options[next];
                    self.snap.set_switch_value(field, value);
                    self.arm_flash();
                    ModalOutcome::Emit(ModalAction::SaveSetting {
                        field,
                        value: Some(save_token(field, value)),
                    })
                }
                Archetype::Dial => {
                    let value = self.snap.dial_value(field);
                    let Some(next) = next_detent(ladder(field), value, forward) else {
                        return ModalOutcome::Ignore;
                    };
                    self.snap.set_dial_value(field, next);
                    self.arm_flash();
                    ModalOutcome::Emit(ModalAction::SaveSetting {
                        field,
                        value: Some(next.to_string()),
                    })
                }
                _ => ModalOutcome::Ignore,
            },
            _ => ModalOutcome::Ignore,
        }
    }

    /// `↵` acts by archetype: ports open their surface, registers and dials
    /// enter inline edit. Switches only move with `←`/`→` (pressing a slide
    /// switch does nothing).
    fn activate(&mut self) -> ModalOutcome {
        match self.selected() {
            RowId::Model => ModalOutcome::Emit(ModalAction::OpenModelPicker),
            RowId::Scope => ModalOutcome::Emit(ModalAction::OpenScopedModels),
            RowId::Providers => ModalOutcome::Emit(ModalAction::OpenLoginMethod),
            RowId::Permissions => ModalOutcome::Emit(ModalAction::OpenTrustMenu),
            RowId::Field(field) => match archetype(RowId::Field(field)) {
                Archetype::Register => {
                    self.edit = Some(Edit {
                        buffer: self.snap.register_value(field).unwrap_or_default(),
                        numeric: false,
                        error: None,
                    });
                    ModalOutcome::Redraw
                }
                Archetype::Dial => {
                    self.edit = Some(Edit {
                        buffer: self.snap.dial_value(field).to_string(),
                        numeric: true,
                        error: None,
                    });
                    ModalOutcome::Redraw
                }
                _ => ModalOutcome::Ignore,
            },
            _ => ModalOutcome::Ignore,
        }
    }

    fn handle_edit_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::Enter => self.commit_edit(),
            ModalKey::Esc | ModalKey::CtrlC => {
                self.edit = None;
                ModalOutcome::Redraw
            }
            ModalKey::Backspace => {
                if let Some(edit) = self.edit.as_mut() {
                    edit.buffer.pop();
                    edit.error = None;
                }
                ModalOutcome::Redraw
            }
            ModalKey::Char(ch) => {
                if let Some(edit) = self.edit.as_mut() {
                    edit.buffer.push(ch);
                    edit.error = None;
                }
                ModalOutcome::Redraw
            }
            _ => ModalOutcome::Ignore,
        }
    }

    /// Validate the buffer and, when valid, save + settle the row. A numeric
    /// buffer clamps to the field's hard bounds; an empty buffer clears the
    /// key when the field allows it, else it is rejected inline.
    fn commit_edit(&mut self) -> ModalOutcome {
        let RowId::Field(field) = self.selected() else {
            self.edit = None;
            return ModalOutcome::Redraw;
        };
        let Some(edit) = self.edit.as_mut() else {
            return ModalOutcome::Ignore;
        };
        let trimmed = edit.buffer.trim().to_string();
        if edit.numeric {
            if trimmed.is_empty() {
                edit.error = Some("enter a number");
                return ModalOutcome::Redraw;
            }
            match trimmed.parse::<u64>() {
                Ok(value) => {
                    let (min, max) = dial_bounds(field);
                    let value = value.clamp(min, max);
                    self.snap.set_dial_value(field, value);
                    self.edit = None;
                    self.arm_flash();
                    ModalOutcome::Emit(ModalAction::SaveSetting {
                        field,
                        value: Some(value.to_string()),
                    })
                }
                Err(_) => {
                    edit.error = Some("whole numbers only");
                    ModalOutcome::Redraw
                }
            }
        } else {
            let value = (!trimmed.is_empty()).then_some(trimmed);
            self.snap.set_register_value(field, value.clone());
            self.edit = None;
            self.arm_flash();
            ModalOutcome::Emit(ModalAction::SaveSetting { field, value })
        }
    }

    // --- rendering ---

    pub(crate) fn render(&self, width: u16) -> Vec<Line<'static>> {
        self.render_budgeted(usize::from(width), DEFAULT_LINE_BUDGET)
    }

    /// Render within `budget` total lines (masthead + body window + footer).
    /// The body windows over the display rows to keep the cursor visible; a
    /// scrolled panel appends the house `(n/N)` position row.
    pub(crate) fn render_budgeted(&self, width: usize, budget: usize) -> Vec<Line<'static>> {
        let avail = width.max(20);
        let rows = display_rows();
        // Fixed lines outside the body window: masthead, the blank under it,
        // and overlay_menu's blank + footer.
        let fixed = 4usize;
        let budget = budget.max(fixed + 3);
        let mut window = budget - fixed;
        let scrolled = rows.len() > window;
        if scrolled {
            window = window.saturating_sub(1).max(1);
        }
        let cursor_pos = rows
            .iter()
            .position(
                |row| matches!(row, DisplayRow::Control { index, .. } if *index == self.cursor),
            )
            .unwrap_or(0);
        let offset = crate::ui::selector::scroll_offset(cursor_pos, window);

        let mut body: Vec<(Line<'static>, bool)> = Vec::new();
        body.push((self.masthead(avail), false));
        body.push((Line::default(), false));
        for row in rows.iter().skip(offset).take(window) {
            match row {
                DisplayRow::Blank => body.push((Line::default(), false)),
                DisplayRow::Header(title) => {
                    body.push((Line::from(Span::styled((*title).to_string(), dim())), false));
                }
                DisplayRow::Control { index, row } => {
                    let selected = *index == self.cursor;
                    body.push((self.control_line(*row, selected, avail), selected));
                }
            }
        }
        if scrolled {
            body.push((
                Line::from(Span::styled(
                    crate::ui::selector::position_label(self.cursor, self.controls.len()),
                    dim(),
                )),
                false,
            ));
        }
        crate::ui::tui::overlay_menu(None, body, Some(&self.footer()), avail)
    }

    /// `SETTINGS` bold + the crate rev right-bound — the faceplate masthead,
    /// aligned to the panel measure like the start page's silkscreen.
    fn masthead(&self, avail: usize) -> Line<'static> {
        let measure = self.measure(avail);
        let title = "SETTINGS";
        let rev = format!("iris {REV}");
        let gap = measure.saturating_sub(title.chars().count() + rev.chars().count());
        if gap == 0 {
            return Line::from(Span::styled(
                title.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        Line::from(vec![
            Span::styled(
                title.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(gap)),
            Span::styled(rev, dim()),
        ])
    }

    /// The panel measure: the widest control row at this width, so the
    /// masthead's rev right-aligns to the grid the controls establish.
    fn measure(&self, avail: usize) -> usize {
        self.controls
            .iter()
            .map(|&row| line_width(&self.control_line(row, false, avail)))
            .max()
            .unwrap_or(0)
            .clamp(24.min(avail), avail)
    }

    /// One control row: `  label            <control>`, two-cell indent, the
    /// control column at `LABEL_W`. The selected row's label is bold (the
    /// surface fill comes from `overlay_menu`).
    fn control_line(&self, row: RowId, selected: bool, avail: usize) -> Line<'static> {
        let flashing = self
            .flash
            .is_some_and(|(index, _)| self.controls.get(index) == Some(&row));
        let mut spans: Vec<Span<'static>> = Vec::new();
        let name = label(row);
        let label_style = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        // The watermark is inert hardware while its master switch is off.
        let inert =
            row == RowId::Field(Field::MicrocompactionWatermark) && !self.snap.microcompaction;
        spans.push(Span::styled(
            format!("  {name:<width$}", width = LABEL_W),
            if inert {
                label_style.patch(dim())
            } else {
                label_style
            },
        ));
        let editing = selected && self.edit.is_some();
        if editing {
            self.push_edit_spans(&mut spans);
        } else {
            self.push_control_spans(row, &mut spans, flashing, inert, avail);
        }
        Line::from(spans)
    }

    fn push_edit_spans(&self, spans: &mut Vec<Span<'static>>) {
        let Some(edit) = self.edit.as_ref() else {
            return;
        };
        spans.push(Span::raw(edit.buffer.clone()));
        spans.push(Span::styled(
            "\u{258b}".to_string(),
            Style::default().fg(crate::ui::palette::orange()),
        ));
        if let Some(error) = edit.error {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("{} {error}", crate::ui::symbols::ERROR),
                Style::default().fg(crate::ui::palette::red()),
            ));
        }
    }

    fn push_control_spans(
        &self,
        row: RowId,
        spans: &mut Vec<Span<'static>>,
        flashing: bool,
        inert: bool,
        avail: usize,
    ) {
        match row {
            RowId::Model => {
                let (provider, model) = self
                    .snap
                    .default_model
                    .split_once('/')
                    .unwrap_or(("", self.snap.default_model.as_str()));
                spans.push(port_marker());
                spans.push(Span::raw(model.to_string()));
                if !provider.is_empty() {
                    spans.push(Span::styled(
                        format!(" {} {provider}", crate::ui::symbols::SEP),
                        dim(),
                    ));
                }
            }
            RowId::Scope => {
                spans.push(port_marker());
                spans.push(Span::styled(self.snap.scope_summary.clone(), dim()));
            }
            RowId::Providers => {
                spans.push(port_marker());
                spans.push(Span::styled(
                    match self.snap.providers_connected {
                        0 => "none connected".to_string(),
                        1 => "1 connected".to_string(),
                        n => format!("{n} connected"),
                    },
                    dim(),
                ));
            }
            RowId::Permissions => {
                spans.push(port_marker());
                spans.push(Span::styled("per-tool + bash grants".to_string(), dim()));
            }
            RowId::Reasoning => {
                let options: Vec<&str> = self
                    .snap
                    .reasoning_levels
                    .iter()
                    .map(|(_, label)| *label)
                    .collect();
                let pos = self
                    .snap
                    .reasoning_levels
                    .iter()
                    .position(|(level, _)| *level == self.snap.reasoning);
                let current = pos.map(|p| options[p]).unwrap_or("");
                spans.extend(switch_spans(
                    &options,
                    pos,
                    current,
                    flashing,
                    false,
                    false,
                    avail.saturating_sub(LABEL_W + 2),
                ));
            }
            RowId::SkipApprovals => {
                let pos = usize::from(self.snap.skip_permissions);
                spans.extend(switch_spans(
                    &["off", "on"],
                    Some(pos),
                    if self.snap.skip_permissions {
                        "on"
                    } else {
                        "off"
                    },
                    flashing,
                    self.snap.skip_permissions,
                    false,
                    avail.saturating_sub(LABEL_W + 2),
                ));
                // Caution silkscreen: printed under the guard switch, always
                // visible, `┊`-joined metadata (never key-hint `·`).
                spans.push(Span::styled(
                    format!("  dangerous {} session only", crate::ui::symbols::SEP),
                    dim(),
                ));
            }
            RowId::Field(field) => match archetype(row) {
                Archetype::Switch => {
                    let options = self.snap.switch_options(field);
                    let current = self.snap.switch_value(field);
                    let pos = options.iter().position(|o| *o == current);
                    spans.extend(switch_spans(
                        options,
                        pos,
                        &current,
                        flashing,
                        false,
                        inert,
                        avail.saturating_sub(LABEL_W + 2),
                    ));
                }
                Archetype::Dial => {
                    let value = self.snap.dial_value(field);
                    push_dial(
                        spans,
                        ladder(field),
                        value,
                        dial_unit(field),
                        flashing,
                        inert,
                    );
                }
                Archetype::Register => match self.snap.register_value(field) {
                    Some(value) => spans.push(Span::raw(value)),
                    None => spans.push(Span::styled(
                        match field {
                            Field::WorktreeRoot => "../wt (default)".to_string(),
                            _ => "not set".to_string(),
                        },
                        dim(),
                    )),
                },
                // Field rows are never ports (the ports are the non-Field
                // RowIds, matched above).
                Archetype::Port => {}
            },
        }
    }

    /// Keymap-honest footer: the verbs for the selected row's archetype only.
    fn footer(&self) -> String {
        if let Some(edit) = self.edit.as_ref() {
            return if edit.numeric {
                "\u{21b5} save \u{00b7} esc cancel".to_string()
            } else {
                "\u{21b5} save \u{00b7} esc cancel \u{00b7} empty clears".to_string()
            };
        }
        let verbs = if self.selected() == RowId::Model {
            // The hybrid row: rotary cycle + port open.
            "\u{2190}\u{2192} cycle \u{00b7} \u{21b5} open"
        } else {
            match archetype(self.selected()) {
                Archetype::Switch => "\u{2190}\u{2192} set",
                Archetype::Dial => "\u{2190}\u{2192} adjust \u{00b7} \u{21b5} type",
                Archetype::Register => "\u{21b5} edit",
                Archetype::Port => "\u{21b5} open",
            }
        };
        format!("\u{2191}\u{2193} select \u{00b7} {verbs} \u{00b7} esc close")
    }
}

/// Shared port marker: the `▸` continuation glyph, dim, in the control column.
fn port_marker() -> Span<'static> {
    Span::styled(format!("{} ", crate::ui::symbols::COLLAPSED), dim())
}

/// Clamped detent step: a switch never wraps.
fn step_clamped(pos: usize, len: usize, forward: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if forward {
        (pos + 1).min(len - 1)
    } else {
        pos.saturating_sub(1)
    }
}

/// The neighbouring ladder detent from `value`: right clicks into the smallest
/// detent above the true value, left into the largest below — an off-ladder
/// value snaps into the ladder on its first click. `None` at the stop.
fn next_detent(ladder: &[u64], value: u64, forward: bool) -> Option<u64> {
    if forward {
        ladder.iter().copied().find(|&detent| detent > value)
    } else {
        ladder.iter().copied().rev().find(|&detent| detent < value)
    }
}

/// Nearest ladder position for the LED fill (display only; the printed value
/// stays the true value).
fn nearest_position(ladder: &[u64], value: u64) -> usize {
    ladder
        .iter()
        .enumerate()
        .min_by_key(|&(_, &detent)| detent.abs_diff(value))
        .map(|(index, _)| index)
        .unwrap_or(0)
}

/// A labeled detent track (`○ strict  ◉ auto  ○ never`), or its rotary form
/// (`○○◉  auto`) when the track does not fit `track_avail` (the row width
/// left after the caller's label column). `danger` paints the selected mark
/// red (the guarded switch); `inert` dims the whole control.
///
/// `pos: None` = a hand-edited value outside the vocabulary: the switch sits
/// BETWEEN detents — no position lights, and the raw `current` value is
/// printed after the track so the display never claims a position the config
/// does not hold (numbers/positions are honest). The first `←`/`→` snaps it
/// into the scale.
///
/// `pub(crate)` because the switch IS the house multi-option control: the
/// model picker prints its reasoning track through this same function so the
/// two surfaces cannot drift.
#[allow(clippy::too_many_arguments)]
pub(crate) fn switch_spans(
    options: &[&str],
    pos: Option<usize>,
    current: &str,
    flashing: bool,
    danger: bool,
    inert: bool,
    track_avail: usize,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let selected_color = if danger {
        crate::ui::palette::red()
    } else {
        crate::ui::palette::orange()
    };
    let mark_style = if inert {
        dim()
    } else {
        Style::default().fg(selected_color)
    };
    let mut label_style = if inert { dim() } else { Style::default() };
    if flashing {
        label_style = label_style.add_modifier(Modifier::BOLD);
    }
    // Track width: `◉ label` per option, two spaces between. Width alone
    // decides the form — a printed scale whenever it fits, the rotary window
    // when it does not (the session bar's drop-rule honesty, §9.1).
    let track: usize = options
        .iter()
        .map(|option| option.chars().count() + 2)
        .sum::<usize>()
        + options.len().saturating_sub(1) * 2;
    if track <= track_avail {
        for (i, option) in options.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            if Some(i) == pos {
                spans.push(Span::styled(
                    format!("{} ", crate::ui::symbols::ACTIVE),
                    mark_style,
                ));
                spans.push(Span::styled((*option).to_string(), label_style));
            } else {
                spans.push(Span::styled(
                    format!("{} {option}", crate::ui::symbols::EMPTY),
                    dim(),
                ));
            }
        }
        if pos.is_none() {
            // Between detents: print what the config actually holds.
            spans.push(Span::raw("  "));
            spans.push(Span::styled(current.to_string(), label_style));
        }
    } else {
        // Rotary form: position dots + the TRUE current value.
        for (i, _) in options.iter().enumerate() {
            spans.push(if Some(i) == pos {
                Span::styled(crate::ui::symbols::ACTIVE.to_string(), mark_style)
            } else {
                Span::styled(crate::ui::symbols::EMPTY.to_string(), dim())
            });
        }
        spans.push(Span::raw("  "));
        spans.push(Span::styled(current.to_string(), label_style));
    }
    spans
}

/// A 10-detent LED fader (the house meter: filled `●`, orange edge, dim `○`)
/// plus the honest printed value.
fn push_dial(
    spans: &mut Vec<Span<'static>>,
    ladder: &[u64],
    value: u64,
    unit: &str,
    flashing: bool,
    inert: bool,
) {
    let pos = nearest_position(ladder, value);
    let edge_style = if inert {
        dim()
    } else {
        let style = Style::default().fg(crate::ui::palette::orange());
        if flashing {
            style.add_modifier(Modifier::BOLD)
        } else {
            style
        }
    };
    let fill_style = if inert { dim() } else { Style::default() };
    for (i, _) in ladder.iter().enumerate() {
        spans.push(if i < pos {
            Span::styled(crate::ui::symbols::RUNNING.to_string(), fill_style)
        } else if i == pos {
            Span::styled(crate::ui::symbols::RUNNING.to_string(), edge_style)
        } else {
            Span::styled(crate::ui::symbols::EMPTY.to_string(), dim())
        });
    }
    spans.push(Span::raw("  "));
    let mut value_style = if inert { dim() } else { Style::default() };
    if flashing {
        value_style = value_style.add_modifier(Modifier::BOLD);
    }
    spans.push(Span::styled(compact_value(value), value_style));
    if !unit.is_empty() {
        spans.push(Span::styled(unit.to_string(), dim()));
    }
}

/// The token persisted for a switch position. Bool switches show `off`/`on`
/// but persist `false`/`true`.
fn save_token(field: Field, value: &str) -> String {
    match field {
        Field::Microcompaction | Field::ReducedMotion => (value == "on").to_string(),
        _ => value.to_string(),
    }
}

/// Sum of a line's span widths (all panel content is single-width).
fn line_width(line: &Line<'static>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            default_model: "openai-codex/gpt-5.5".to_string(),
            reasoning_levels: vec![
                (ReasoningEffort::Minimal, "minimal"),
                (ReasoningEffort::Low, "low"),
                (ReasoningEffort::Medium, "medium"),
                (ReasoningEffort::High, "high"),
                (ReasoningEffort::XHigh, "xhigh"),
            ],
            reasoning: ReasoningEffort::Medium,
            scope_summary: "all models".to_string(),
            providers_connected: 2,
            default_approval: "strict".to_string(),
            skip_permissions: false,
            context_token_budget: 232_000,
            compaction_summarizer: "subagent".to_string(),
            microcompaction: false,
            microcompaction_watermark: 32_000,
            prompt_cache_retention: "short".to_string(),
            verify_command: None,
            verify_max_attempts: 3,
            theme: "terminal".to_string(),
            alt_screen: "auto".to_string(),
            scroll_speed: 3,
            reduced_motion: false,
            worktree_root: None,
        }
    }

    fn panel() -> SettingsPanel {
        SettingsPanel::new(snapshot())
    }

    fn select(panel: &mut SettingsPanel, row: RowId) {
        let pos = panel
            .controls
            .iter()
            .position(|&r| r == row)
            .expect("row on the panel");
        panel.cursor = pos;
    }

    fn text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn faceplate_lists_every_section_and_prunes_the_service_hatch() {
        let lines = panel().render_budgeted(80, 60);
        let rendered = text(&lines);
        for section in ["ENGINE", "SAFETY", "MEMORY", "CHECKS", "PANEL", "GIT"] {
            assert!(rendered.contains(section), "{section} missing:\n{rendered}");
        }
        // Pruned to settings.json (service hatch): never on the faceplate.
        assert!(!rendered.to_lowercase().contains("bash tool"));
        assert!(!rendered.to_lowercase().contains("round-trips"));
        assert!(!rendered.to_lowercase().contains("roundtrips"));
        // Masthead: identity + rev, like the start-page silkscreen.
        assert!(rendered.contains("SETTINGS"));
        assert!(rendered.contains(&format!("iris {REV}")));
    }

    #[test]
    fn switch_clicks_one_detent_and_clamps_at_the_stop() {
        let mut panel = panel();
        select(&mut panel, RowId::Field(Field::DefaultApproval));
        // strict -> auto.
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::DefaultApproval,
                value: Some("auto".to_string()),
            })
        );
        // auto -> never.
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::DefaultApproval,
                value: Some("never".to_string()),
            })
        );
        // Against the stop: a real switch never wraps.
        assert_eq!(panel.handle_key(ModalKey::Right), ModalOutcome::Ignore);
        // And back one detent.
        assert_eq!(
            panel.handle_key(ModalKey::Left),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::DefaultApproval,
                value: Some("auto".to_string()),
            })
        );
    }

    #[test]
    fn bool_switches_persist_true_false_not_on_off() {
        let mut panel = panel();
        select(&mut panel, RowId::Field(Field::Microcompaction));
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::Microcompaction,
                value: Some("true".to_string()),
            })
        );
        // Already on: pushing further is a no-op, pulling back turns it off.
        assert_eq!(panel.handle_key(ModalKey::Right), ModalOutcome::Ignore);
        assert_eq!(
            panel.handle_key(ModalKey::Left),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::Microcompaction,
                value: Some("false".to_string()),
            })
        );
    }

    #[test]
    fn skip_approvals_is_positional_and_only_fires_on_a_real_flip() {
        let mut panel = panel();
        select(&mut panel, RowId::SkipApprovals);
        // Already off: pulling left is a no-op (never a blind toggle).
        assert_eq!(panel.handle_key(ModalKey::Left), ModalOutcome::Ignore);
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::ToggleSkipPermissions)
        );
        // The panel's own display flipped with it.
        assert!(panel.snap.skip_permissions);
    }

    #[test]
    fn dial_snaps_an_off_ladder_value_into_the_ladder() {
        let mut panel = panel();
        select(&mut panel, RowId::Field(Field::ContextTokenBudget));
        // 232k sits ON the ladder; right clicks into 300k.
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::ContextTokenBudget,
                value: Some("300000".to_string()),
            })
        );
        // An off-ladder value (90k, e.g. hand-edited json) snaps to the
        // neighbouring detent on the first click.
        panel.snap.context_token_budget = 90_000;
        assert_eq!(
            panel.handle_key(ModalKey::Left),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::ContextTokenBudget,
                value: Some("64000".to_string()),
            })
        );
        // At the bottom stop the dial no-ops.
        panel.snap.context_token_budget = 64_000;
        assert_eq!(panel.handle_key(ModalKey::Left), ModalOutcome::Ignore);
    }

    #[test]
    fn dial_enter_types_a_precise_value_and_clamps_to_bounds() {
        let mut panel = panel();
        select(&mut panel, RowId::Field(Field::ScrollSpeed));
        assert_eq!(panel.handle_key(ModalKey::Enter), ModalOutcome::Redraw);
        // Seeded with the current value; type a replacement.
        for _ in 0..1 {
            panel.handle_key(ModalKey::Backspace);
        }
        panel.handle_key(ModalKey::Char('9'));
        panel.handle_key(ModalKey::Char('9'));
        panel.handle_key(ModalKey::Char('9'));
        assert_eq!(
            panel.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::ScrollSpeed,
                value: Some("100".to_string()),
            })
        );
        // Non-numeric input is rejected inline, panel stays in edit.
        panel.handle_key(ModalKey::Enter);
        panel.handle_key(ModalKey::Char('x'));
        assert_eq!(panel.handle_key(ModalKey::Enter), ModalOutcome::Redraw);
        assert!(panel.edit.as_ref().is_some_and(|e| e.error.is_some()));
        // Esc cancels the edit without saving.
        assert_eq!(panel.handle_key(ModalKey::Esc), ModalOutcome::Redraw);
        assert!(panel.edit.is_none());
    }

    #[test]
    fn register_edits_inline_and_empty_clears() {
        let mut panel = panel();
        select(&mut panel, RowId::Field(Field::VerifyCommand));
        assert_eq!(panel.handle_key(ModalKey::Enter), ModalOutcome::Redraw);
        panel.push_str("  cargo test  ");
        assert_eq!(
            panel.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::VerifyCommand,
                value: Some("cargo test".to_string()),
            })
        );
        // Re-enter, clear to empty: clears the key.
        panel.handle_key(ModalKey::Enter);
        for _ in 0.."cargo test".len() {
            panel.handle_key(ModalKey::Backspace);
        }
        assert_eq!(
            panel.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::VerifyCommand,
                value: None,
            })
        );
    }

    #[test]
    fn the_model_row_is_a_rotary_port_hybrid() {
        // ←/→ cycles the scoped models like Ctrl+P; ↵ opens the full picker.
        let mut panel = panel();
        select(&mut panel, RowId::Model);
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::CycleModel { forward: true })
        );
        assert_eq!(
            panel.handle_key(ModalKey::Left),
            ModalOutcome::Emit(ModalAction::CycleModel { forward: false })
        );
        // The footer names both verbs (keymap honesty for the hybrid).
        assert!(panel.footer().contains("\u{2190}\u{2192} cycle"));
        assert!(panel.footer().contains("\u{21b5} open"));
    }

    #[test]
    fn ports_open_their_surfaces() {
        let mut panel = panel();
        select(&mut panel, RowId::Model);
        assert_eq!(
            panel.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::OpenModelPicker)
        );
        select(&mut panel, RowId::Scope);
        assert_eq!(
            panel.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::OpenScopedModels)
        );
        select(&mut panel, RowId::Providers);
        assert_eq!(
            panel.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::OpenLoginMethod)
        );
        select(&mut panel, RowId::Permissions);
        assert_eq!(
            panel.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::OpenTrustMenu)
        );
        // Pure ports never respond to ←→ (nothing to slide) — the model
        // row's cycling is the deliberate hybrid exception.
        assert_eq!(panel.handle_key(ModalKey::Right), ModalOutcome::Ignore);
    }

    #[test]
    fn reasoning_clicks_through_the_active_models_levels() {
        let mut panel = panel();
        select(&mut panel, RowId::Reasoning);
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::AdjustEffort(ReasoningEffort::High))
        );
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::AdjustEffort(ReasoningEffort::XHigh))
        );
        assert_eq!(panel.handle_key(ModalKey::Right), ModalOutcome::Ignore);
    }

    #[test]
    fn theme_is_a_live_rotary_over_every_theme_id() {
        let mut panel = panel();
        select(&mut panel, RowId::Field(Field::Theme));
        // terminal (index 0) -> gruvbox (index 1): each click saves.
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::Theme,
                value: Some(crate::ui::theme::available()[1].to_string()),
            })
        );
        // Rendered as the rotary form (6 ids never fit a labeled track).
        let lines = panel.render_budgeted(80, 60);
        let rendered = text(&lines);
        assert!(
            rendered.contains("gruvbox"),
            "selected value printed:\n{rendered}"
        );
    }

    #[test]
    fn a_change_flashes_two_ticks_then_settles() {
        let mut panel = panel();
        select(&mut panel, RowId::Field(Field::DefaultApproval));
        panel.handle_key(ModalKey::Right);
        assert!(panel.flash.is_some(), "detent flash armed");
        assert!(panel.tick(), "first tick still settling");
        assert!(panel.tick(), "second tick settles");
        assert!(!panel.tick(), "settled: no more repaints");
        assert!(panel.flash.is_none());
    }

    #[test]
    fn reduced_motion_never_flashes() {
        let mut snap = snapshot();
        snap.reduced_motion = true;
        let mut panel = SettingsPanel::new(snap);
        select(&mut panel, RowId::Field(Field::DefaultApproval));
        panel.handle_key(ModalKey::Right);
        assert!(panel.flash.is_none(), "reduced motion settles instantly");
    }

    #[test]
    fn watermark_goes_inert_while_microcompaction_is_off() {
        let panel = panel();
        assert!(!panel.snap.microcompaction);
        let line = panel.control_line(RowId::Field(Field::MicrocompactionWatermark), false, 80);
        // Inert hardware: every span dims, including the LED edge.
        assert!(
            line.spans
                .iter()
                .all(|span| span.style.add_modifier.contains(Modifier::DIM)
                    || span.content.trim().is_empty()),
            "inert row renders fully dim: {line:?}"
        );
        // Still operable: adjusting emits a save.
        let mut panel = self::panel();
        select(&mut panel, RowId::Field(Field::MicrocompactionWatermark));
        assert!(matches!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::MicrocompactionWatermark,
                ..
            })
        ));
    }

    #[test]
    fn footer_verbs_are_keymap_honest_per_archetype() {
        let mut panel = panel();
        select(&mut panel, RowId::Field(Field::DefaultApproval));
        assert!(panel.footer().contains("\u{2190}\u{2192} set"));
        assert!(!panel.footer().contains("\u{21b5}"));
        select(&mut panel, RowId::Field(Field::ContextTokenBudget));
        assert!(panel.footer().contains("adjust"));
        assert!(panel.footer().contains("\u{21b5} type"));
        select(&mut panel, RowId::Field(Field::VerifyCommand));
        assert!(panel.footer().contains("\u{21b5} edit"));
        select(&mut panel, RowId::Model);
        assert!(panel.footer().contains("\u{21b5} open"));
        // Editing swaps to the edit verbs.
        select(&mut panel, RowId::Field(Field::VerifyCommand));
        panel.handle_key(ModalKey::Enter);
        assert!(panel.footer().contains("\u{21b5} save"));
        assert!(panel.footer().contains("esc cancel"));
    }

    #[test]
    fn narrow_width_degrades_the_track_to_its_rotary_form() {
        let panel = panel();
        // Wide: the reasoning row prints every level.
        let wide = text(&[panel.control_line(RowId::Reasoning, false, 80)]);
        assert!(wide.contains("minimal") && wide.contains("xhigh"));
        // Narrow: position dots + the selected value only.
        let narrow = text(&[panel.control_line(RowId::Reasoning, false, 46)]);
        assert!(narrow.contains("medium"));
        assert!(!narrow.contains("minimal"), "rotary form: {narrow}");
    }

    #[test]
    fn windowing_keeps_the_cursor_visible_with_a_position_row() {
        let mut panel = panel();
        // Walk to the last control (worktree root) under a tight budget.
        select(&mut panel, RowId::Field(Field::WorktreeRoot));
        let lines = panel.render_budgeted(80, 14);
        let rendered = text(&lines);
        assert!(
            rendered.contains("worktree root"),
            "cursor row visible:\n{rendered}"
        );
        let total = panel.controls.len();
        assert!(
            rendered.contains(&format!("({total}/{total})")),
            "house position row while scrolled:\n{rendered}"
        );
        // Untruncated on a tall terminal: no position row, every section shown.
        let full = text(&panel.render_budgeted(80, 60));
        assert!(!full.contains(&format!("({total}/{total})")));
        assert!(full.contains("ENGINE") && full.contains("GIT"));
    }

    #[test]
    fn navigation_wraps_over_controls_and_skips_silkscreen() {
        let mut panel = panel();
        assert_eq!(panel.selected(), RowId::Model);
        panel.handle_key(ModalKey::Up);
        assert_eq!(
            panel.selected(),
            RowId::Field(Field::WorktreeRoot),
            "wraps to the last control, never a header"
        );
        panel.handle_key(ModalKey::Down);
        assert_eq!(panel.selected(), RowId::Model);
    }

    #[test]
    fn ladder_stepping_is_mechanical() {
        assert_eq!(next_detent(&BUDGET_LADDER, 232_000, true), Some(300_000));
        assert_eq!(next_detent(&BUDGET_LADDER, 232_000, false), Some(200_000));
        // Off-ladder snaps to the neighbour, both directions.
        assert_eq!(next_detent(&BUDGET_LADDER, 90_000, true), Some(96_000));
        assert_eq!(next_detent(&BUDGET_LADDER, 90_000, false), Some(64_000));
        // Stops.
        assert_eq!(next_detent(&BUDGET_LADDER, 1_000_000, true), None);
        assert_eq!(next_detent(&BUDGET_LADDER, 64_000, false), None);
        // Fill position is the nearest detent.
        assert_eq!(nearest_position(&BUDGET_LADDER, 232_000), 5);
        assert_eq!(nearest_position(&BUDGET_LADDER, 90_000), 1);
    }

    #[test]
    fn dial_values_print_in_the_one_house_token_format() {
        assert_eq!(compact_value(232_000), "232k");
        assert_eq!(compact_value(1_000_000), "1m");
        assert_eq!(compact_value(3), "3");
        // Honest compacting: a non-round value keeps its decimal, exactly as
        // the divider and receipt would print it — never silently truncated.
        assert_eq!(compact_value(12_500), "12.5k");
    }

    #[test]
    fn port_return_reopens_on_the_port_row() {
        let panel = SettingsPanel::with_selected(snapshot(), RowId::Scope);
        assert_eq!(panel.selected(), RowId::Scope);
    }

    #[test]
    fn an_off_vocabulary_value_sits_between_detents_and_prints_raw() {
        // A hand-edited config value outside the switch vocabulary (e.g. the
        // "catppuccin" theme alias, or an unknown approval token) must never
        // light a position it does not hold — the old tree showed the raw
        // value, and the faceplate must stay at least as honest.
        let mut snap = snapshot();
        snap.default_approval = "on-request".to_string();
        let panel = SettingsPanel::new(snap);
        let line = panel.control_line(RowId::Field(Field::DefaultApproval), false, 80);
        let rendered = text(std::slice::from_ref(&line));
        assert!(
            rendered.contains("on-request"),
            "raw value printed: {rendered}"
        );
        assert!(
            !line
                .spans
                .iter()
                .any(|span| span.content.contains(crate::ui::symbols::ACTIVE)),
            "no detent lights for a value between detents: {rendered}"
        );
        // Same law on a rotary (the theme row): true value, no lit dot.
        let mut snap = snapshot();
        snap.theme = "catppuccin".to_string();
        let panel = SettingsPanel::new(snap);
        let line = panel.control_line(RowId::Field(Field::Theme), false, 80);
        let rendered = text(std::slice::from_ref(&line));
        assert!(rendered.contains("catppuccin"), "{rendered}");
        assert!(
            !line
                .spans
                .iter()
                .any(|span| span.content.contains(crate::ui::symbols::ACTIVE)),
            "rotary shows no position for an off-vocabulary value: {rendered}"
        );
    }

    #[test]
    fn an_off_vocabulary_value_snaps_into_the_scale_on_first_click() {
        let mut snap = snapshot();
        snap.default_approval = "on-request".to_string();
        let mut panel = SettingsPanel::new(snap);
        select(&mut panel, RowId::Field(Field::DefaultApproval));
        // Right snaps to the first detent, like a dial's off-ladder snap.
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::DefaultApproval,
                value: Some("strict".to_string()),
            })
        );
        // And left from between detents snaps to the last one.
        let mut snap = snapshot();
        snap.default_approval = "on-request".to_string();
        let mut panel = SettingsPanel::new(snap);
        select(&mut panel, RowId::Field(Field::DefaultApproval));
        assert_eq!(
            panel.handle_key(ModalKey::Left),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::DefaultApproval,
                value: Some("never".to_string()),
            })
        );
    }

    #[test]
    fn a_multi_line_paste_flattens_into_the_single_line_register() {
        let mut panel = panel();
        select(&mut panel, RowId::Field(Field::VerifyCommand));
        panel.handle_key(ModalKey::Enter);
        panel.push_str("cargo fmt\ncargo test\r\n");
        assert_eq!(
            panel.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::VerifyCommand,
                value: Some("cargo fmt cargo test".to_string()),
            })
        );
    }
}
