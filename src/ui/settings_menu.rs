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
//! - **port** — a `▸` row that **expands in place** to `▾` + indented child
//!   rows inside the same panel (model picker, model scope, providers, project
//!   permissions). One hatch open at a time (accordion); `↵` on the header or
//!   `esc` anywhere folds it. The panel never leaves — a port opens a hatch, not
//!   a door.
//!
//! Every widget here is pure: it turns a [`ModalKey`] into a [`ModalOutcome`],
//! and the loop ([`crate::ui::picker::apply_action`]) performs the disk writes
//! at the safe inter-turn boundary. A change is acknowledged mechanically: the
//! adjusted element renders bright for two ticks (the §6 detent flash), gated
//! by reduced motion. Dependent controls go dark while their master switch is
//! off — inert hardware, still operable: the AUTO COMPACT thresholds, tail,
//! reactive, summarizer, and worker-input knobs follow `automatic`; the
//! tool-result aggressiveness, cache timing, fold trigger, retain, and
//! keep-tool-uses knobs follow `tool result compaction`.
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

use crate::mimir::model_capabilities;
use crate::mimir::selection::{ProviderId, ReasoningEffort};
use crate::ui::modal::{ModalAction, ModalKey, ModalOutcome, dim};
use crate::wayland::trust::ProjectPolicyEdit;

/// Crate rev printed on the masthead (the panel's silkscreen, same source as
/// the start page and the exit receipt).
const REV: &str = env!("CARGO_PKG_VERSION");

/// Label column width: two-cell indent + label, control column after. Wide
/// enough that most labels keep a real gutter; the one long outlier (`tool
/// result compaction`, deliberately spelled out to disambiguate it from AUTO
/// COMPACT) overhangs the column rather than widening the shared grid. Shared
/// with the model picker's reasoning track so the two surfaces sit on one grid.
pub(crate) const LABEL_W: usize = 18;

/// Detent-flash duration in loop ticks (the §6 two-tick acknowledgment).
const FLASH_TICKS: u8 = 2;

/// Default line budget when the render path supplies none: matches the legacy
/// 16-row docked-menu cap minus its two inset rows.
const DEFAULT_LINE_BUDGET: usize = 14;

/// The per-tool grants the permissions hatch can toggle. Matches the ADR-0027
/// per-tool approval defaults; `bash` is intentionally absent (bash grants are
/// per-command, minted at the approval prompt).
const POLICY_TOOLS: &[&str] = &["write", "edit"];

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
    CompactionEnabled,
    CompactionWarn,
    CompactionStart,
    CompactionHard,
    CompactionKeepRecentTokens,
    CompactionHardWait,
    CompactionReactive,
    CompactionSummarizer,
    CompactionWorkerInput,
    Microcompaction,
    MicrocompactionWatermark,
    CompactionAggressiveness,
    CompactionCacheTiming,
    SemanticRetainPerPath,
    ToolClearingKeepRecent,
    PromptCacheRetention,
    VerifyCommand,
    VerifyMaxAttempts,
    WorktreeRoot,
}

/// One top-level control of the faceplate. `Field` rows edit persisted settings;
/// the rest are the session-live controls (reasoning, skip-approvals) and the
/// ports (model, scope, providers, permissions).
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

/// A selectable row of the panel, keyed on identity (not position) so a hatch
/// opening above the cursor never silently moves the selection, and a flash
/// armed on a row survives the list reflowing (§2.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PanelRow {
    /// A top-level control (in `SECTIONS` order).
    Top(RowId),
    /// A model-hatch candidate, carrying its qualified `provider/model` id.
    ModelChild(String),
    /// A scope-hatch candidate, carrying its qualified id.
    ScopeChild(String),
    /// A providers-hatch row, carrying the provider id.
    ProviderChild(String),
    /// A permissions per-tool switch, carrying the tool name.
    PolicyTool(String),
    /// A permissions stored-bash-grant row (exact), carrying the command.
    PolicyBashExact(String),
    /// A permissions stored-bash-grant row (prefix), carrying the prefix.
    PolicyBashPrefix(String),
}

/// Which hatch a slash entry pre-expands, and where its cursor lands (§4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HatchTarget {
    /// `/model`, bare `/reasoning`, ctrl+p picker key: model hatch, active row.
    Model,
    /// `/scoped-models`: scope hatch, first child.
    Scope,
    /// `/trust`, bare `/permissions`: permissions hatch, first child.
    Permissions,
    /// `/login`: providers hatch, first uncredentialed row (else first).
    Login,
    /// `/logout`: providers hatch, first credentialed row (else first).
    Logout,
}

/// The panel's restorable view: the open hatch, the identity-keyed cursor, and
/// any live scope filter. Captured before a dialog-guard replaces the panel or
/// a snapshot refresh rebuilds it, then re-applied so the operator lands back
/// exactly where they were (§2.5, §5).
#[derive(Debug, Clone)]
pub(crate) struct PanelView {
    expanded: Option<RowId>,
    cursor: PanelRow,
    filter: String,
    /// The cursor's index in the flattened selectable list at capture time, so a
    /// refresh that removes the cursor's row (a revoked bash grant) can land on
    /// the row that took its slot instead of jumping to the port header.
    cursor_index: usize,
}

impl PanelView {
    /// The row the cursor rested on — the flash target for an in-place refresh.
    pub(crate) fn cursor(&self) -> PanelRow {
        self.cursor.clone()
    }

    /// The hatch that was open when this view was captured. Inspection peer of
    /// [`Self::cursor`], used by the loop tests to assert which hatch a slash
    /// entry pre-expanded and that a guard round trip returns expanded.
    #[cfg(test)]
    pub(crate) fn expanded(&self) -> Option<RowId> {
        self.expanded
    }
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
        title: "AUTO COMPACT",
        rows: &[
            RowId::Field(Field::CompactionEnabled),
            RowId::Field(Field::CompactionWarn),
            RowId::Field(Field::CompactionStart),
            RowId::Field(Field::CompactionHard),
            RowId::Field(Field::CompactionKeepRecentTokens),
            RowId::Field(Field::CompactionHardWait),
            RowId::Field(Field::CompactionReactive),
            RowId::Field(Field::CompactionSummarizer),
            RowId::Field(Field::CompactionWorkerInput),
        ],
    },
    Section {
        title: "MEMORY",
        rows: &[
            RowId::Field(Field::ContextTokenBudget),
            RowId::Field(Field::Microcompaction),
            RowId::Field(Field::CompactionAggressiveness),
            RowId::Field(Field::CompactionCacheTiming),
            RowId::Field(Field::MicrocompactionWatermark),
            RowId::Field(Field::SemanticRetainPerPath),
            RowId::Field(Field::ToolClearingKeepRecent),
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
/// Protected-tail detents for `compaction.keepRecentTokens` (retain tail).
const TAIL_LADDER: [u64; 10] = [
    2_000, 4_000, 6_000, 8_000, 12_000, 16_000, 24_000, 32_000, 48_000, 64_000,
];
/// Hard-tier bounded-wait detents for `compaction.hardWaitMs`, in 30 s steps
/// from 30 s to the 300 s (5 min) cap. The default 120000 ms is step 4.
const HARD_WAIT_LADDER: [u64; 10] = [
    30_000, 60_000, 90_000, 120_000, 150_000, 180_000, 210_000, 240_000, 270_000, 300_000,
];
/// Whole-percent detents for the warn/start/hard threshold dials. The printed
/// value is always the honest percent; the ladder only drives the LED fill and
/// the neighbouring-detent step.
const PERCENT_LADDER: [u64; 10] = [10, 20, 30, 40, 50, 60, 70, 80, 90, 99];
const UNIT_LADDER: [u64; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

/// Hard bounds for typed dial entry, matching the `config::save_*` clamps.
fn dial_bounds(field: Field) -> (u64, u64) {
    match field {
        Field::ContextTokenBudget
        | Field::MicrocompactionWatermark
        | Field::CompactionKeepRecentTokens => (1_000, 100_000_000),
        Field::SemanticRetainPerPath | Field::ToolClearingKeepRecent => (1, 1_000),
        // Thresholds are whole percents; ordering (`warn < start < hard`) is
        // enforced by the config save, which restores the persisted value on a
        // rejected combination.
        Field::CompactionWarn | Field::CompactionStart | Field::CompactionHard => (1, 99),
        // Hard wait clamps to the ladder span; the config save clamps to the
        // same 300000 ms cap independently.
        Field::CompactionHardWait => (30_000, 300_000),
        Field::ScrollSpeed => (1, 100),
        Field::VerifyMaxAttempts => (1, 10),
        _ => (0, u64::MAX),
    }
}

fn ladder(field: Field) -> &'static [u64] {
    match field {
        Field::ContextTokenBudget => &BUDGET_LADDER,
        Field::MicrocompactionWatermark => &WATERMARK_LADDER,
        Field::CompactionKeepRecentTokens => &TAIL_LADDER,
        Field::CompactionWarn | Field::CompactionStart | Field::CompactionHard => &PERCENT_LADDER,
        Field::CompactionHardWait => &HARD_WAIT_LADDER,
        _ => &UNIT_LADDER,
    }
}

/// Compact dial value in the ONE house token format (`232k`, `12.5k`, `1m`) —
/// the same formatter the meter, divider, and receipt print, so a number
/// never reads differently on the panel than in the transcript.
fn compact_value(value: u64) -> String {
    crate::ui::tui::compact_count(value)
}

/// One authenticated model, presentation-ready for the ENGINE › model hatch:
/// default-first order, with the reasoning levels the row's live effort track
/// clicks through (§5).
#[derive(Debug, Clone)]
pub(crate) struct ModelChoice {
    pub(crate) qualified: String,
    pub(crate) display: String,
    pub(crate) provider_label: String,
    pub(crate) provider: ProviderId,
    pub(crate) model_id: String,
    pub(crate) levels: Vec<(ReasoningEffort, &'static str)>,
    pub(crate) is_current: bool,
    pub(crate) is_default: bool,
}

/// One scope candidate (ENGINE › model scope hatch), registry order.
#[derive(Debug, Clone)]
pub(crate) struct ScopeChoice {
    pub(crate) qualified: String,
    pub(crate) provider_label: String,
}

/// One provider row (ENGINE › providers hatch): a no-secret credential badge
/// and the login methods that exist for it.
#[derive(Debug, Clone)]
pub(crate) struct ProviderStatus {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) badge: String,
    pub(crate) oauth_capable: bool,
    pub(crate) api_key_capable: bool,
    pub(crate) credentialed: bool,
}

/// The SAFETY › permissions hatch payload (ADR-0027), disk-free and secret-free.
#[derive(Debug, Clone, Default)]
pub(crate) struct PolicySnapshot {
    pub(crate) granted_tools: Vec<String>,
    pub(crate) bash_exact: Vec<String>,
    pub(crate) bash_prefix: Vec<String>,
    pub(crate) sandbox: Option<String>,
}

/// The auto-compaction ladder resolved against the active model window
/// (`Harness::context_diagnostics`), for the AUTO COMPACT section's dim
/// resolved-value line. Reuses the harness's trigger resolution rather than
/// recomputing the arithmetic on the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedLadder {
    pub(crate) warn: u64,
    pub(crate) start: u64,
    pub(crate) hard: u64,
    /// The tail size the ladder actually applies after clamping to the model
    /// window (`keep_recent_tokens.min(window / 4)`).
    pub(crate) effective_tail: u64,
    /// The configured tail before that clamp, so the panel can show a
    /// `configured tail X -> effective tail Y` note when they differ.
    pub(crate) configured_tail: u64,
    pub(crate) effective_window: u64,
}

/// Current persisted values plus the live session state the panel controls
/// (reasoning levels for the active model, skip-approvals) and the hatch
/// payloads. Read once by the loop from [`crate::config::Settings`]; pure data,
/// so the panel stays harness-free and unit-testable.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    /// Qualified `provider/model` id of the persisted default.
    pub(crate) default_model: String,
    /// Reasoning levels the ACTIVE model supports, panel order.
    pub(crate) reasoning_levels: Vec<(ReasoningEffort, &'static str)>,
    /// The active reasoning level (clamped to the model).
    pub(crate) reasoning: ReasoningEffort,
    /// ENGINE › model hatch: authenticated catalog, default-first.
    pub(crate) catalog: Vec<ModelChoice>,
    /// ENGINE › scope hatch candidates, registry order.
    pub(crate) scope_candidates: Vec<ScopeChoice>,
    /// The live scope: `None` = all enabled (existing collapse_full rule).
    pub(crate) scope_enabled: Option<Vec<String>>,
    /// The persisted scope, so the hatch prints `· unsaved` while it differs.
    pub(crate) scope_persisted: Option<Vec<String>>,
    /// ENGINE › providers hatch.
    pub(crate) providers: Vec<ProviderStatus>,
    /// SAFETY › permissions hatch.
    pub(crate) policy: PolicySnapshot,
    pub(crate) default_approval: String,
    pub(crate) skip_permissions: bool,
    pub(crate) context_token_budget: u64,
    /// Full-context auto-compaction master switch (`compaction.enabled`).
    pub(crate) compaction_enabled: bool,
    /// The warn/start/hard trigger fractions, as whole percents for the dials.
    pub(crate) compaction_warn_pct: u64,
    pub(crate) compaction_start_pct: u64,
    pub(crate) compaction_hard_pct: u64,
    /// The configured protected-tail size (`compaction.keepRecentTokens`).
    pub(crate) compaction_keep_recent_tokens: u64,
    /// The hard-tier bounded wait in milliseconds (`compaction.hardWaitMs`).
    pub(crate) compaction_hard_wait_ms: u64,
    /// Reactive deterministic-recovery toggle (`compaction.reactive`).
    pub(crate) compaction_reactive: bool,
    /// Background worker input mode (`compaction.worker.input`).
    pub(crate) compaction_worker_input: String,
    /// The ladder resolved against the active model window, for the dim
    /// resolved-value line. `None` when the harness has no live diagnostics.
    pub(crate) resolved_ladder: Option<ResolvedLadder>,
    pub(crate) compaction_summarizer: String,
    /// The resolved tool-result-compaction master switch (`toolResultCompaction
    /// .enabled`, or the legacy `microcompaction` alias). Field name kept for the
    /// MEMORY row's identity; the label reads `tool result compaction`.
    pub(crate) microcompaction: bool,
    /// The resolved compaction trigger tokens (`toolResultCompaction
    /// .triggerTokens`, or the legacy watermark). The `fold trigger` dial.
    pub(crate) microcompaction_watermark: u64,
    pub(crate) compaction_aggressiveness: String,
    pub(crate) compaction_cache_timing: String,
    pub(crate) semantic_retain_per_path: u64,
    pub(crate) tool_clearing_keep_recent: u64,
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
            Field::CompactionAggressiveness => {
                &["conservative", "balanced", "aggressive", "custom"]
            }
            Field::CompactionCacheTiming => {
                &["breakOnly", "cacheAware", "pressureOnly", "immediate"]
            }
            Field::Theme => crate::ui::theme::available(),
            Field::Microcompaction
            | Field::CompactionEnabled
            | Field::CompactionReactive
            | Field::ReducedMotion => &["off", "on"],
            Field::CompactionWorkerInput => &["transcript", "investigator"],
            _ => &[],
        }
    }

    fn switch_value(&self, field: Field) -> String {
        match field {
            Field::AltScreen => self.alt_screen.clone(),
            Field::DefaultApproval => self.default_approval.clone(),
            Field::PromptCacheRetention => self.prompt_cache_retention.clone(),
            Field::CompactionSummarizer => self.compaction_summarizer.clone(),
            Field::CompactionAggressiveness => self.compaction_aggressiveness.clone(),
            Field::CompactionCacheTiming => self.compaction_cache_timing.clone(),
            Field::Theme => self.theme.clone(),
            Field::Microcompaction => on_off(self.microcompaction),
            Field::CompactionEnabled => on_off(self.compaction_enabled),
            Field::CompactionReactive => on_off(self.compaction_reactive),
            Field::CompactionWorkerInput => self.compaction_worker_input.clone(),
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
            Field::CompactionAggressiveness => self.compaction_aggressiveness = value.to_string(),
            Field::CompactionCacheTiming => self.compaction_cache_timing = value.to_string(),
            Field::Theme => self.theme = value.to_string(),
            Field::Microcompaction => self.microcompaction = value == "on",
            Field::CompactionEnabled => self.compaction_enabled = value == "on",
            Field::CompactionReactive => self.compaction_reactive = value == "on",
            Field::CompactionWorkerInput => self.compaction_worker_input = value.to_string(),
            Field::ReducedMotion => self.reduced_motion = value == "on",
            _ => {}
        }
    }

    fn dial_value(&self, field: Field) -> u64 {
        match field {
            Field::ContextTokenBudget => self.context_token_budget,
            Field::MicrocompactionWatermark => self.microcompaction_watermark,
            Field::CompactionWarn => self.compaction_warn_pct,
            Field::CompactionStart => self.compaction_start_pct,
            Field::CompactionHard => self.compaction_hard_pct,
            Field::CompactionKeepRecentTokens => self.compaction_keep_recent_tokens,
            Field::CompactionHardWait => self.compaction_hard_wait_ms,
            Field::SemanticRetainPerPath => self.semantic_retain_per_path,
            Field::ToolClearingKeepRecent => self.tool_clearing_keep_recent,
            Field::ScrollSpeed => u64::from(self.scroll_speed),
            Field::VerifyMaxAttempts => u64::from(self.verify_max_attempts),
            _ => 0,
        }
    }

    fn set_dial_value(&mut self, field: Field, value: u64) {
        match field {
            Field::ContextTokenBudget => self.context_token_budget = value,
            Field::MicrocompactionWatermark => self.microcompaction_watermark = value,
            Field::CompactionWarn => self.compaction_warn_pct = value.clamp(1, 99),
            Field::CompactionStart => self.compaction_start_pct = value.clamp(1, 99),
            Field::CompactionHard => self.compaction_hard_pct = value.clamp(1, 99),
            Field::CompactionKeepRecentTokens => self.compaction_keep_recent_tokens = value.max(1),
            Field::CompactionHardWait => {
                self.compaction_hard_wait_ms = value.clamp(30_000, 300_000)
            }
            Field::SemanticRetainPerPath => self.semantic_retain_per_path = value.max(1),
            Field::ToolClearingKeepRecent => self.tool_clearing_keep_recent = value.max(1),
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

/// How a top row is adjusted/activated: drives both key handling and the
/// footer's keymap-honest verb.
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
            | Field::CompactionAggressiveness
            | Field::CompactionCacheTiming
            | Field::CompactionEnabled
            | Field::CompactionReactive
            | Field::CompactionWorkerInput
            | Field::Theme
            | Field::Microcompaction
            | Field::ReducedMotion => Archetype::Switch,
            Field::ContextTokenBudget
            | Field::MicrocompactionWatermark
            | Field::CompactionWarn
            | Field::CompactionStart
            | Field::CompactionHard
            | Field::CompactionKeepRecentTokens
            | Field::CompactionHardWait
            | Field::SemanticRetainPerPath
            | Field::ToolClearingKeepRecent
            | Field::ScrollSpeed
            | Field::VerifyMaxAttempts => Archetype::Dial,
            Field::VerifyCommand | Field::WorktreeRoot => Archetype::Register,
        },
    }
}

/// The four ports whose `↵` expands a hatch (the model row is also a rotary).
fn is_port(row: RowId) -> bool {
    matches!(archetype(row), Archetype::Port)
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
            Field::ContextTokenBudget => "context cap",
            Field::CompactionEnabled => "automatic",
            Field::CompactionWarn => "warn at",
            Field::CompactionStart => "start at",
            Field::CompactionHard => "hard at",
            Field::CompactionKeepRecentTokens => "retain tail",
            Field::CompactionHardWait => "hard wait",
            Field::CompactionReactive => "reactive",
            Field::CompactionSummarizer => "summarizer",
            Field::CompactionWorkerInput => "worker input",
            Field::Microcompaction => "tool result compaction",
            Field::CompactionAggressiveness => "aggressiveness",
            Field::CompactionCacheTiming => "cache timing",
            Field::MicrocompactionWatermark => "fold trigger",
            Field::SemanticRetainPerPath => "retain/path",
            Field::ToolClearingKeepRecent => "keep tool uses",
            Field::PromptCacheRetention => "prompt cache",
            Field::VerifyCommand => "verify",
            Field::VerifyMaxAttempts => "attempts",
            Field::WorktreeRoot => "worktree root",
        },
    }
}

/// A dial's printed value, in the field's own idiom. Most dials print the house
/// count token (`232k`); the hard-wait dial prints a duration (`2m`, `90s`) so
/// the milliseconds read as a wall-clock the operator judges the turn-block by.
fn dial_value_label(field: Field, value: u64) -> String {
    match field {
        Field::CompactionHardWait => hard_wait_label(value),
        _ => compact_value(value),
    }
}

/// A hard-wait duration in the terse house idiom: whole minutes print `Nm`
/// (`120000` -> `2m`), otherwise bare seconds `Ns` (`90000` -> `90s`).
fn hard_wait_label(ms: u64) -> String {
    let seconds = ms / 1_000;
    if seconds != 0 && seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// A dial's unit, printed dim after the honest value. Empty = bare number.
fn dial_unit(field: Field) -> &'static str {
    match field {
        Field::ContextTokenBudget
        | Field::MicrocompactionWatermark
        | Field::CompactionKeepRecentTokens => " tokens",
        Field::CompactionWarn | Field::CompactionStart | Field::CompactionHard => "%",
        Field::ScrollSpeed => " lines",
        _ => "",
    }
}

/// One rendered row of the panel body, in order. `Control` rows carry their
/// [`PanelRow`] identity (the cursor space); headers, blanks, and read-only
/// silkscreen lines (the sandbox posture, empty-state notes) are skipped by
/// navigation.
enum DisplayRow {
    Header(&'static str),
    Blank,
    Control(PanelRow),
    ReadOnly(Line<'static>),
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
/// keeps the panel (no rebuild) on success so detents click without jank. A
/// port's `↵` expands a hatch in place (accordion) rather than opening a door.
#[derive(Debug, Clone)]
pub(crate) struct SettingsPanel {
    snap: Snapshot,
    cursor: PanelRow,
    /// The one open hatch, if any. Accordion: opening a port collapses any other.
    expanded: Option<RowId>,
    /// Live type-to-filter for the scope hatch (§3.2); empty when inactive.
    scope_filter: String,
    /// The model hatch's target effort, clamped to each candidate for display
    /// but never mutated by navigation (§3.1). Reset to the active level on
    /// every (re)build.
    model_target: ReasoningEffort,
    edit: Option<Edit>,
    /// The row whose value renders bright, and ticks remaining.
    flash: Option<(PanelRow, u8)>,
}

impl SettingsPanel {
    pub(crate) fn new(snapshot: Snapshot) -> Self {
        let model_target = snapshot.reasoning;
        SettingsPanel {
            snap: snapshot,
            cursor: PanelRow::Top(RowId::Model),
            expanded: None,
            scope_filter: String::new(),
            model_target,
            edit: None,
            flash: None,
        }
    }

    /// Open with a hatch pre-expanded and the cursor on its most useful child —
    /// the slash-command entry path (§4.1).
    pub(crate) fn with_expanded(snapshot: Snapshot, target: HatchTarget) -> Self {
        let mut panel = SettingsPanel::new(snapshot);
        let port = match target {
            HatchTarget::Model => RowId::Model,
            HatchTarget::Scope => RowId::Scope,
            HatchTarget::Permissions => RowId::Permissions,
            HatchTarget::Login | HatchTarget::Logout => RowId::Providers,
        };
        panel.expanded = Some(port);
        panel.cursor = panel.entry_cursor(target);
        panel
    }

    /// The child the entry path lands on when a hatch opens pre-expanded.
    fn entry_cursor(&self, target: HatchTarget) -> PanelRow {
        let children = self.children_of(self.expanded);
        match target {
            HatchTarget::Model => self
                .snap
                .catalog
                .iter()
                .find(|m| m.is_current)
                .map(|m| PanelRow::ModelChild(m.qualified.clone()))
                .or_else(|| children.first().cloned())
                .unwrap_or(PanelRow::Top(RowId::Model)),
            HatchTarget::Login => self
                .snap
                .providers
                .iter()
                .find(|p| !p.credentialed)
                .map(|p| PanelRow::ProviderChild(p.id.clone()))
                .or_else(|| children.first().cloned())
                .unwrap_or(PanelRow::Top(RowId::Providers)),
            HatchTarget::Logout => self
                .snap
                .providers
                .iter()
                .find(|p| p.credentialed)
                .map(|p| PanelRow::ProviderChild(p.id.clone()))
                .or_else(|| children.first().cloned())
                .unwrap_or(PanelRow::Top(RowId::Providers)),
            HatchTarget::Scope | HatchTarget::Permissions => children
                .first()
                .cloned()
                .unwrap_or_else(|| PanelRow::Top(self.expanded.unwrap_or(RowId::Scope))),
        }
    }

    /// Capture the restorable view (§2.5, §5).
    pub(crate) fn view(&self) -> PanelView {
        let rows = self.selectable();
        let cursor_index = rows.iter().position(|row| *row == self.cursor).unwrap_or(0);
        PanelView {
            expanded: self.expanded,
            cursor: self.cursor.clone(),
            filter: self.scope_filter.clone(),
            cursor_index,
        }
    }

    /// Re-apply a captured view onto a freshly built panel, landing the cursor
    /// on the next grant when its row has vanished (a revoked bash grant), and
    /// re-arming the live scope filter. Model target resets to the fresh level.
    pub(crate) fn restore(&mut self, view: PanelView) {
        self.expanded = view.expanded;
        if self.expanded == Some(RowId::Scope) {
            self.scope_filter = view.filter;
        }
        let rows = self.selectable();
        self.cursor = if rows.contains(&view.cursor) {
            view.cursor
        } else {
            self.landing_after_vanished(&rows, view.cursor_index)
        };
        self.model_target = self.snap.reasoning;
    }

    /// Where the cursor lands when its row vanished under it — a revoked bash
    /// grant. The removed row's old slot now holds its successor (the list
    /// shifted up), so land there; fall to the predecessor when the removed
    /// grant was the hatch's last; land on the port header only when no grant
    /// remains to hold the cursor (the per-tool switches and the next section's
    /// controls are never a landing, so an emptied grant list yields the header).
    fn landing_after_vanished(&self, rows: &[PanelRow], removed_index: usize) -> PanelRow {
        let is_grant = |row: &&PanelRow| {
            matches!(
                row,
                PanelRow::PolicyBashExact(_) | PanelRow::PolicyBashPrefix(_)
            )
        };
        let next = rows.get(removed_index).filter(is_grant);
        let prev = removed_index
            .checked_sub(1)
            .and_then(|i| rows.get(i))
            .filter(is_grant);
        next.or(prev)
            .cloned()
            .or_else(|| self.expanded.map(PanelRow::Top))
            .or_else(|| rows.first().cloned())
            .unwrap_or(PanelRow::Top(RowId::Model))
    }

    fn selected(&self) -> PanelRow {
        self.cursor.clone()
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

    /// Arm the two-tick acknowledgment on `row`. Reduced motion settles instantly
    /// (§6: every motion degrades to its settled state).
    fn arm_flash(&mut self, row: PanelRow) {
        if !self.snap.reduced_motion {
            self.flash = Some((row, FLASH_TICKS));
        } else {
            self.flash = None;
        }
    }

    /// Apply a live accessibility switch to an already-open faceplate. Any
    /// detent acknowledgment settles in the same interaction that enables
    /// reduced motion.
    pub(crate) fn set_reduced_motion(&mut self, reduced_motion: bool) {
        self.snap.reduced_motion = reduced_motion;
        if reduced_motion {
            self.flash = None;
        }
    }

    /// Arm the flash from outside (the loop rebuilds the panel after a snapshot
    /// refresh and still owes the acted-on row its mechanical acknowledgment).
    pub(crate) fn flash_row(&mut self, row: PanelRow) {
        self.arm_flash(row);
    }

    /// Paste into whichever single-line field has focus (the loop routes
    /// `Event::Paste` here): an active register/dial edit, else the scope
    /// hatch's type-to-filter. Interior line breaks collapse to spaces so a
    /// multi-line paste can never embed a newline in a saved value or a filter.
    pub(crate) fn push_str(&mut self, text: &str) {
        let flat = flatten_paste(text);
        if let Some(edit) = self.edit.as_mut() {
            edit.buffer.push_str(&flat);
            edit.error = None;
        } else if self.in_scope_hatch() {
            self.scope_filter.push_str(&flat);
            self.clamp_scope_cursor();
        }
    }

    // --- navigation ---

    /// The selectable rows, flattened with the open hatch's children spliced in.
    fn selectable(&self) -> Vec<PanelRow> {
        let mut out = Vec::new();
        self.walk_rows(|row| {
            if let DisplayRow::Control(panel_row) = row {
                out.push(panel_row);
            }
        });
        out
    }

    /// The children of an open port, in display order (with the scope filter
    /// applied). Empty for `None` or a non-port.
    fn children_of(&self, port: Option<RowId>) -> Vec<PanelRow> {
        match port {
            Some(RowId::Model) => self
                .snap
                .catalog
                .iter()
                .map(|m| PanelRow::ModelChild(m.qualified.clone()))
                .collect(),
            Some(RowId::Scope) => self
                .scope_children()
                .into_iter()
                .map(PanelRow::ScopeChild)
                .collect(),
            Some(RowId::Providers) => self
                .snap
                .providers
                .iter()
                .map(|p| PanelRow::ProviderChild(p.id.clone()))
                .collect(),
            Some(RowId::Permissions) => {
                let mut out: Vec<PanelRow> = POLICY_TOOLS
                    .iter()
                    .map(|tool| PanelRow::PolicyTool((*tool).to_string()))
                    .collect();
                out.extend(
                    self.snap
                        .policy
                        .bash_exact
                        .iter()
                        .map(|cmd| PanelRow::PolicyBashExact(cmd.clone())),
                );
                out.extend(
                    self.snap
                        .policy
                        .bash_prefix
                        .iter()
                        .map(|pfx| PanelRow::PolicyBashPrefix(pfx.clone())),
                );
                out
            }
            _ => Vec::new(),
        }
    }

    fn move_cursor(&mut self, down: bool) {
        let rows = self.selectable();
        if rows.is_empty() {
            return;
        }
        let pos = rows.iter().position(|row| *row == self.cursor).unwrap_or(0);
        let next = if down {
            (pos + 1) % rows.len()
        } else {
            (pos + rows.len() - 1) % rows.len()
        };
        self.cursor = rows[next].clone();
    }

    pub(crate) fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        if self.edit.is_some() {
            return self.handle_edit_key(key);
        }
        match key {
            ModalKey::Up => {
                self.move_cursor(false);
                ModalOutcome::Redraw
            }
            ModalKey::Down => {
                self.move_cursor(true);
                ModalOutcome::Redraw
            }
            ModalKey::Left => self.adjust(false),
            ModalKey::Right => self.adjust(true),
            ModalKey::Enter => self.activate(),
            ModalKey::Esc => self.escape(),
            ModalKey::CtrlC => ModalOutcome::Close,
            other => self.hatch_key(other),
        }
    }

    /// `esc`: clear an active scope filter first, collapse a hatch second, close
    /// the panel last (§2.3).
    fn escape(&mut self) -> ModalOutcome {
        if self.expanded == Some(RowId::Scope) && !self.scope_filter.is_empty() {
            self.scope_filter.clear();
            self.clamp_scope_cursor();
            return ModalOutcome::Redraw;
        }
        if self.expanded.is_some() {
            self.collapse();
            return ModalOutcome::Redraw;
        }
        ModalOutcome::Close
    }

    /// Fold the open hatch: cursor lands on its port header, filter clears, the
    /// header flashes.
    fn collapse(&mut self) {
        if let Some(port) = self.expanded.take() {
            self.scope_filter.clear();
            self.cursor = PanelRow::Top(port);
            self.arm_flash(PanelRow::Top(port));
        }
    }

    /// Expand `port` (accordion: any other hatch folds), or fold it if already
    /// open. Either way the header flashes and the cursor stays on it.
    fn toggle_hatch(&mut self, port: RowId) -> ModalOutcome {
        if self.expanded == Some(port) {
            self.collapse();
        } else {
            self.expanded = Some(port);
            self.scope_filter.clear();
            if port == RowId::Model {
                self.model_target = self.snap.reasoning;
            }
            self.cursor = PanelRow::Top(port);
            self.arm_flash(PanelRow::Top(port));
        }
        ModalOutcome::Redraw
    }

    /// Click the selected control one detent left/right. Emits the save action
    /// when the position actually changes; a clamped end is a silent no-op.
    fn adjust(&mut self, forward: bool) -> ModalOutcome {
        match self.selected() {
            // The model row is a port AND a rotary: ←/→ cycles the scoped
            // models exactly like Ctrl+P; ↵ expands the hatch.
            PanelRow::Top(RowId::Model) => ModalOutcome::Emit(ModalAction::CycleModel { forward }),
            PanelRow::Top(RowId::Reasoning) => self.adjust_reasoning(forward),
            PanelRow::Top(RowId::SkipApprovals) => {
                if self.snap.skip_permissions == forward {
                    return ModalOutcome::Ignore;
                }
                self.snap.skip_permissions = forward;
                self.arm_flash(self.cursor.clone());
                ModalOutcome::Emit(ModalAction::ToggleSkipPermissions)
            }
            PanelRow::Top(RowId::Field(field)) => self.adjust_field(field, forward),
            // Ports without a slide, and non-switch children, ignore ←/→.
            PanelRow::Top(_) => ModalOutcome::Ignore,
            PanelRow::ModelChild(id) => self.adjust_model_effort(&id, forward),
            PanelRow::PolicyTool(tool) => self.adjust_policy_tool(&tool, forward),
            _ => ModalOutcome::Ignore,
        }
    }

    fn adjust_reasoning(&mut self, forward: bool) -> ModalOutcome {
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
        self.arm_flash(PanelRow::Top(RowId::Reasoning));
        ModalOutcome::Emit(ModalAction::AdjustEffort(level))
    }

    fn adjust_field(&mut self, field: Field, forward: bool) -> ModalOutcome {
        match archetype(RowId::Field(field)) {
            Archetype::Switch => {
                let options = self.snap.switch_options(field);
                if options.is_empty() {
                    return ModalOutcome::Ignore;
                }
                let current = self.snap.switch_value(field);
                // A hand-edited value outside the vocabulary sits between
                // detents: the first click snaps into the scale (right → first
                // position, left → last), like a dial's off-ladder snap.
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
                self.arm_flash(self.cursor.clone());
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
                self.arm_flash(self.cursor.clone());
                ModalOutcome::Emit(ModalAction::SaveSetting {
                    field,
                    value: Some(next.to_string()),
                })
            }
            _ => ModalOutcome::Ignore,
        }
    }

    /// Model-hatch effort: click the target one detent within the highlighted
    /// candidate's level stops (clamp, never wrap). Navigation never mutates the
    /// target; only this does (§3.1, criterion 9/10). The reasoning row flashes.
    fn adjust_model_effort(&mut self, qualified: &str, forward: bool) -> ModalOutcome {
        let Some(model) = self.snap.catalog.iter().find(|m| m.qualified == qualified) else {
            return ModalOutcome::Ignore;
        };
        let from = model_capabilities::clamp(model.provider, &model.model_id, self.model_target);
        let pos = model
            .levels
            .iter()
            .position(|(level, _)| *level == from)
            .unwrap_or(0);
        let next = step_clamped(pos, model.levels.len(), forward);
        if next == pos {
            return ModalOutcome::Ignore;
        }
        self.model_target = model.levels[next].0;
        self.arm_flash(PanelRow::Top(RowId::Reasoning));
        ModalOutcome::Redraw
    }

    /// A permissions per-tool switch (`ask · always`): position IS state, so
    /// `←`/`→` emits the matching grant/revoke immediately and clamps at stops.
    /// The panel clicks its own display detent first (like the other switches),
    /// so a second click against the stop is a silent no-op before the loop's
    /// snapshot refresh lands.
    fn adjust_policy_tool(&mut self, tool: &str, forward: bool) -> ModalOutcome {
        let granted = self.snap.policy.granted_tools.iter().any(|t| t == tool);
        let target = forward; // right = always (granted), left = ask (revoked)
        if granted == target {
            return ModalOutcome::Ignore;
        }
        let edit = if target {
            self.snap.policy.granted_tools.push(tool.to_string());
            ProjectPolicyEdit::GrantTool(tool.to_string())
        } else {
            self.snap.policy.granted_tools.retain(|t| t != tool);
            ProjectPolicyEdit::RevokeTool(tool.to_string())
        };
        self.arm_flash(self.cursor.clone());
        ModalOutcome::Emit(ModalAction::EditPolicy(edit))
    }

    /// `↵` acts by row: ports expand/fold their hatch; registers and dials enter
    /// inline edit; child rows fire their per-port verb.
    fn activate(&mut self) -> ModalOutcome {
        match self.selected() {
            PanelRow::Top(row) if is_port(row) => self.toggle_hatch(row),
            PanelRow::Top(RowId::Field(field)) => match archetype(RowId::Field(field)) {
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
            PanelRow::ModelChild(id) => self.select_model(&id, true),
            PanelRow::ScopeChild(id) => {
                self.scope_toggle(&id);
                ModalOutcome::Emit(ModalAction::ApplyScoped(self.scope()))
            }
            PanelRow::ProviderChild(id) => self.provider_primary(&id),
            PanelRow::PolicyBashExact(cmd) => ModalOutcome::Emit(ModalAction::EditPolicy(
                ProjectPolicyEdit::RevokeBashExact(cmd),
            )),
            PanelRow::PolicyBashPrefix(pfx) => ModalOutcome::Emit(ModalAction::EditPolicy(
                ProjectPolicyEdit::RevokeBashPrefix(pfx),
            )),
            // Switches (reasoning, skip approvals, per-tool grants) only move
            // with ←/→ — pressing a slide switch does nothing.
            _ => ModalOutcome::Ignore,
        }
    }

    /// The per-hatch extra keys, routed by the cursor's context.
    fn hatch_key(&mut self, key: ModalKey) -> ModalOutcome {
        match self.selected() {
            PanelRow::ModelChild(id) => match key {
                ModalKey::Char('s') | ModalKey::Char('S') => self.select_model(&id, false),
                _ => ModalOutcome::Ignore,
            },
            PanelRow::ProviderChild(id) => self.provider_key(&id, key),
            _ if self.in_scope_hatch() => self.scope_key(key),
            _ => ModalOutcome::Ignore,
        }
    }

    fn in_scope_hatch(&self) -> bool {
        self.expanded == Some(RowId::Scope)
            && matches!(
                self.cursor,
                PanelRow::ScopeChild(_) | PanelRow::Top(RowId::Scope)
            )
    }

    // --- model hatch ---

    fn select_model(&mut self, qualified: &str, save_default: bool) -> ModalOutcome {
        let Some(model) = self.snap.catalog.iter().find(|m| m.qualified == qualified) else {
            return ModalOutcome::Ignore;
        };
        let effort = model_capabilities::clamp(model.provider, &model.model_id, self.model_target);
        ModalOutcome::Emit(ModalAction::SelectModel {
            id: qualified.to_string(),
            effort,
            save_default,
        })
    }

    /// The candidate whose levels the reasoning row live-tracks: the highlighted
    /// model child, or `None` (revert to the active model's truth).
    fn highlighted_model(&self) -> Option<&ModelChoice> {
        if self.expanded != Some(RowId::Model) {
            return None;
        }
        match &self.cursor {
            PanelRow::ModelChild(id) => self.snap.catalog.iter().find(|m| &m.qualified == id),
            _ => None,
        }
    }

    // --- providers hatch ---

    fn provider(&self, id: &str) -> Option<&ProviderStatus> {
        self.snap.providers.iter().find(|p| p.id == id)
    }

    /// `↵` fires the provider's primary method: OAuth/subscription when it
    /// supports one, else the API-key dialog (§3.3).
    fn provider_primary(&self, id: &str) -> ModalOutcome {
        let Some(status) = self.provider(id) else {
            return ModalOutcome::Ignore;
        };
        if status.oauth_capable {
            match ProviderId::parse(id) {
                Ok(provider) => ModalOutcome::Emit(ModalAction::BeginLogin(provider)),
                Err(_) => ModalOutcome::Ignore,
            }
        } else if status.api_key_capable {
            ModalOutcome::Emit(ModalAction::OpenApiKeyDialog(id.to_string()))
        } else {
            ModalOutcome::Ignore
        }
    }

    fn provider_key(&mut self, id: &str, key: ModalKey) -> ModalOutcome {
        let Some(status) = self.provider(id) else {
            return ModalOutcome::Ignore;
        };
        match key {
            ModalKey::Char('a') | ModalKey::Char('A') if status.api_key_capable => {
                ModalOutcome::Emit(ModalAction::OpenApiKeyDialog(id.to_string()))
            }
            ModalKey::Char('x') | ModalKey::Char('X') if status.credentialed => {
                ModalOutcome::Emit(ModalAction::Logout(id.to_string()))
            }
            _ => ModalOutcome::Ignore,
        }
    }

    // --- scope hatch ---

    /// The live scope to apply/persist.
    fn scope(&self) -> Option<Vec<String>> {
        self.snap.scope_enabled.clone()
    }

    fn scope_is_enabled(&self, id: &str) -> bool {
        match &self.snap.scope_enabled {
            None => true,
            Some(list) => list.iter().any(|e| e == id),
        }
    }

    /// Scope children: enabled ids first (configured order), then the remaining
    /// candidates in registry order — with the live filter applied (§3.2).
    fn scope_children(&self) -> Vec<String> {
        let filter = self.scope_filter.to_ascii_lowercase();
        let matches = |id: &str| filter.is_empty() || id.to_ascii_lowercase().contains(&filter);
        let mut out: Vec<String> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        if let Some(enabled) = &self.snap.scope_enabled {
            for id in enabled {
                if self
                    .snap
                    .scope_candidates
                    .iter()
                    .any(|c| &c.qualified == id)
                    && matches(id)
                {
                    out.push(id.clone());
                }
                seen.push(id.clone());
            }
        }
        for candidate in &self.snap.scope_candidates {
            if seen.contains(&candidate.qualified) {
                continue;
            }
            if matches(&candidate.qualified) {
                out.push(candidate.qualified.clone());
            }
        }
        out
    }

    /// Keep the scope cursor on a still-visible child (after a filter change);
    /// fall back to the first child, then the header.
    fn clamp_scope_cursor(&mut self) {
        if let PanelRow::ScopeChild(id) = &self.cursor {
            let children = self.scope_children();
            if !children.iter().any(|c| c == id) {
                self.cursor = children
                    .first()
                    .cloned()
                    .map(PanelRow::ScopeChild)
                    .unwrap_or(PanelRow::Top(RowId::Scope));
            }
        }
    }

    fn scope_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::CtrlA => {
                let ids = self.scope_matching_or_all();
                self.scope_set_many(&ids, true);
                ModalOutcome::Emit(ModalAction::ApplyScoped(self.scope()))
            }
            ModalKey::CtrlX => {
                let ids = self.scope_matching_or_all();
                self.scope_set_many(&ids, false);
                ModalOutcome::Emit(ModalAction::ApplyScoped(self.scope()))
            }
            ModalKey::CtrlP => {
                if let PanelRow::ScopeChild(id) = &self.cursor
                    && let Some((provider, _)) = id.split_once('/')
                    && let Ok(provider) = ProviderId::parse(provider)
                {
                    self.scope_toggle_provider(provider);
                    ModalOutcome::Emit(ModalAction::ApplyScoped(self.scope()))
                } else {
                    ModalOutcome::Ignore
                }
            }
            ModalKey::CtrlS => {
                // Persist clears the unsaved tag: mirror the live scope onto the
                // panel's persisted-scope truth so the header settles.
                self.snap.scope_persisted = self.snap.scope_enabled.clone();
                ModalOutcome::Emit(ModalAction::SaveScoped(self.scope()))
            }
            ModalKey::AltUp => {
                self.scope_reorder(true);
                ModalOutcome::Emit(ModalAction::ApplyScoped(self.scope()))
            }
            ModalKey::AltDown => {
                self.scope_reorder(false);
                ModalOutcome::Emit(ModalAction::ApplyScoped(self.scope()))
            }
            ModalKey::Backspace => {
                if self.scope_filter.pop().is_some() {
                    self.clamp_scope_cursor();
                    ModalOutcome::Redraw
                } else {
                    ModalOutcome::Ignore
                }
            }
            ModalKey::Char(c) => {
                self.scope_filter.push(c);
                self.clamp_scope_cursor();
                ModalOutcome::Redraw
            }
            _ => ModalOutcome::Ignore,
        }
    }

    /// Fold an explicit list covering every candidate back to `None`, matching
    /// pi-mono. An explicit empty list stays `Some([])` — a deliberate "nothing
    /// enabled".
    fn scope_collapse_full(&mut self) {
        if let Some(list) = &self.snap.scope_enabled
            && !list.is_empty()
            && list.len() >= self.snap.scope_candidates.len()
            && self
                .snap
                .scope_candidates
                .iter()
                .all(|c| list.iter().any(|e| e == &c.qualified))
        {
            self.snap.scope_enabled = None;
        }
    }

    fn scope_toggle(&mut self, id: &str) {
        let mut list = self.snap.scope_enabled.clone().unwrap_or_default();
        if let Some(pos) = list.iter().position(|e| e == id) {
            list.remove(pos);
        } else {
            list.push(id.to_string());
        }
        self.snap.scope_enabled = Some(list);
        self.scope_collapse_full();
    }

    /// Enable/disable a whole set of ids (Ctrl+A / Ctrl+X / provider toggle).
    fn scope_set_many(&mut self, ids: &[String], enable: bool) {
        let mut list = match &self.snap.scope_enabled {
            None => {
                if enable {
                    return; // enable-all from all-enabled is a no-op
                }
                self.snap
                    .scope_candidates
                    .iter()
                    .map(|c| c.qualified.clone())
                    .collect()
            }
            Some(list) => list.clone(),
        };
        for id in ids {
            let pos = list.iter().position(|e| e == id);
            match (enable, pos) {
                (true, None) => list.push(id.clone()),
                (false, Some(p)) => {
                    list.remove(p);
                }
                _ => {}
            }
        }
        self.snap.scope_enabled = Some(list);
        self.scope_collapse_full();
    }

    fn scope_toggle_provider(&mut self, provider: ProviderId) {
        let prefix = format!("{}/", provider.as_str());
        let ids: Vec<String> = self
            .snap
            .scope_candidates
            .iter()
            .filter(|c| c.qualified.starts_with(&prefix))
            .map(|c| c.qualified.clone())
            .collect();
        let all_on = ids.iter().all(|id| self.scope_is_enabled(id));
        self.scope_set_many(&ids, !all_on);
    }

    fn scope_reorder(&mut self, up: bool) {
        let PanelRow::ScopeChild(id) = self.cursor.clone() else {
            return;
        };
        let Some(list) = self.snap.scope_enabled.as_mut() else {
            return;
        };
        let Some(pos) = list.iter().position(|e| e == &id) else {
            return;
        };
        let swap = if up {
            if pos == 0 {
                return;
            }
            pos - 1
        } else {
            if pos + 1 >= list.len() {
                return;
            }
            pos + 1
        };
        list.swap(pos, swap);
    }

    /// The filtered ids when a filter is active, otherwise every candidate id.
    fn scope_matching_or_all(&self) -> Vec<String> {
        if self.scope_filter.is_empty() {
            self.snap
                .scope_candidates
                .iter()
                .map(|c| c.qualified.clone())
                .collect()
        } else {
            self.scope_children()
        }
    }

    /// The candidate ids, for the one scope normalization the header count and
    /// the `· unsaved` tag both route through — so they can never diverge.
    fn scope_candidate_ids(&self) -> Vec<&str> {
        self.snap
            .scope_candidates
            .iter()
            .map(|c| c.qualified.as_str())
            .collect()
    }

    /// The enabled count printed in the header, normalized against the current
    /// candidate set: a stale persisted scope carrying ids no longer in the
    /// catalog must not inflate the numerator (`2 of 1 enabled`). Reuses the
    /// `scope_unsaved` normalization so the count and the tag stay in lockstep.
    fn scope_enabled_count(&self) -> usize {
        let candidates = self.scope_candidate_ids();
        match normalize_scope(&self.snap.scope_enabled, &candidates) {
            None => self.snap.scope_candidates.len(),
            Some(list) => list.len(),
        }
    }

    /// Whether the live scope differs from the persisted one (the `· unsaved`
    /// tag). Both are normalized (dropped to candidates, full → None) so an
    /// equivalent scope never reads as unsaved.
    fn scope_unsaved(&self) -> bool {
        let candidates = self.scope_candidate_ids();
        normalize_scope(&self.snap.scope_enabled, &candidates)
            != normalize_scope(&self.snap.scope_persisted, &candidates)
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
        let PanelRow::Top(RowId::Field(field)) = self.selected() else {
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
                    self.arm_flash(self.cursor.clone());
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
            self.arm_flash(self.cursor.clone());
            ModalOutcome::Emit(ModalAction::SaveSetting { field, value })
        }
    }

    // --- rendering ---

    pub(crate) fn render(&self, width: u16) -> Vec<Line<'static>> {
        self.render_budgeted(usize::from(width), DEFAULT_LINE_BUDGET)
    }

    /// Build the display list (headers, blanks, controls with the open hatch's
    /// children spliced in, and read-only silkscreen lines), calling `emit` for
    /// each row in order. Shared by `selectable` (identity only) and the render
    /// path (which also needs the read-only lines and blanks).
    fn walk_rows<F: FnMut(DisplayRow)>(&self, mut emit: F) {
        for (i, section) in SECTIONS.iter().enumerate() {
            if i > 0 {
                emit(DisplayRow::Blank);
            }
            emit(DisplayRow::Header(section.title));
            for &row in section.rows {
                emit(DisplayRow::Control(PanelRow::Top(row)));
                if self.expanded == Some(row) {
                    // The scope filter echoes directly under the port header so a
                    // live type-to-filter is visible, not a silent narrowing.
                    if let Some(echo) = self.scope_filter_echo(row) {
                        emit(DisplayRow::ReadOnly(echo));
                    }
                    for child in self.children_of(Some(row)) {
                        emit(DisplayRow::Control(child));
                    }
                    for line in self.hatch_notes(row) {
                        emit(DisplayRow::ReadOnly(line));
                    }
                }
            }
            for line in self.section_footnotes(section.title) {
                emit(DisplayRow::ReadOnly(line));
            }
        }
    }

    /// Dim silkscreen lines printed under a section. AUTO COMPACT shows the
    /// ladder resolved against the active model window — reusing the harness
    /// trigger resolution (`/context` diagnostics), never recomputing it here —
    /// so configured fractions/tail read next to their effective token values.
    fn section_footnotes(&self, title: &str) -> Vec<Line<'static>> {
        if title != "AUTO COMPACT" {
            return Vec::new();
        }
        let Some(ladder) = self.snap.resolved_ladder else {
            return Vec::new();
        };
        let mut lines = vec![Line::from(Span::styled(
            format!(
                "  active model: warn {} / start {} / hard {} / tail {}",
                compact_value(ladder.warn),
                compact_value(ladder.start),
                compact_value(ladder.hard),
                compact_value(ladder.effective_tail),
            ),
            dim(),
        ))];
        // The tail is clamped to a quarter of the model window; when a small
        // window shrinks it, show the configured -> effective adjustment.
        if ladder.effective_tail != ladder.configured_tail {
            lines.push(Line::from(Span::styled(
                format!(
                    "  configured tail {} \u{2192} effective tail {}",
                    compact_value(ladder.configured_tail),
                    compact_value(ladder.effective_tail),
                ),
                dim(),
            )));
        }
        lines
    }

    /// The dim filter-echo line printed directly under the scope port header
    /// while a type-to-filter query is live (§3.2): the `filter` label on the
    /// child grid, the query text, and the house caret — so the operator sees
    /// what is narrowing the checklist rather than a silent reflow. `None`
    /// unless this is the scope hatch with a non-empty query.
    fn scope_filter_echo(&self, row: RowId) -> Option<Line<'static>> {
        if row != RowId::Scope || self.scope_filter.is_empty() {
            return None;
        }
        Some(Line::from(vec![
            Span::styled(
                format!("    {:<width$}", "filter", width = LABEL_W - 2),
                dim(),
            ),
            Span::raw(self.scope_filter.clone()),
            Span::styled(
                crate::ui::symbols::CARET.to_string(),
                Style::default().fg(crate::ui::palette::orange()),
            ),
        ]))
    }

    /// Non-selectable silkscreen lines printed inside a hatch: the empty-catalog
    /// notes (every hatch prints a quiet row, never nothing), the scope empty
    /// filter note, the permissions empty-grants note, and the read-only sandbox
    /// posture.
    fn hatch_notes(&self, row: RowId) -> Vec<Line<'static>> {
        match row {
            RowId::Model if self.snap.catalog.is_empty() => {
                vec![child_note("no models \u{2014} connect a provider")]
            }
            RowId::Providers if self.snap.providers.is_empty() => {
                vec![child_note("no providers")]
            }
            RowId::Scope if self.scope_children().is_empty() => {
                vec![child_note("no matching models")]
            }
            RowId::Permissions => {
                let mut notes = Vec::new();
                if self.snap.policy.bash_exact.is_empty() && self.snap.policy.bash_prefix.is_empty()
                {
                    notes.push(child_note("no bash grants"));
                }
                if let Some(sandbox) = &self.snap.policy.sandbox {
                    notes.push(Line::from(vec![
                        Span::styled(format!("    {:<width$}", "sandbox", width = LABEL_W), dim()),
                        Span::styled(sandbox.clone(), dim()),
                    ]));
                }
                notes
            }
            _ => Vec::new(),
        }
    }

    /// Render within `budget` total lines (masthead + body window + footer).
    /// The body windows over the display rows to keep the cursor visible; a
    /// scrolled panel appends the house `(n/N)` position row.
    pub(crate) fn render_budgeted(&self, width: usize, budget: usize) -> Vec<Line<'static>> {
        let avail = width.max(20);
        let mut rows: Vec<DisplayRow> = Vec::new();
        self.walk_rows(|row| rows.push(row));
        let selectable = self.selectable();
        let total = selectable.len().max(1);
        let cursor_index = selectable
            .iter()
            .position(|row| *row == self.cursor)
            .unwrap_or(0);
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
            .position(|row| matches!(row, DisplayRow::Control(pr) if *pr == self.cursor))
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
                DisplayRow::ReadOnly(line) => body.push((line.clone(), false)),
                DisplayRow::Control(panel_row) => {
                    let selected = *panel_row == self.cursor;
                    body.push((self.control_line(panel_row, selected, avail), selected));
                }
            }
        }
        if scrolled {
            body.push((
                Line::from(Span::styled(
                    crate::ui::selector::position_label(cursor_index, total),
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

    /// The panel measure: the widest top control row at this width, so the
    /// masthead's rev right-aligns to the grid the controls establish.
    fn measure(&self, avail: usize) -> usize {
        SECTIONS
            .iter()
            .flat_map(|section| section.rows.iter())
            .map(|&row| line_width(&self.control_line(&PanelRow::Top(row), false, avail)))
            .max()
            .unwrap_or(0)
            .clamp(24.min(avail), avail)
    }

    /// One panel row. Top control rows are `  label  <control>` (two-cell
    /// indent, control column at `LABEL_W`); child rows print at a four-cell
    /// indent. The selected row's label is bold (the surface fill comes from
    /// `overlay_menu`).
    fn control_line(&self, row: &PanelRow, selected: bool, avail: usize) -> Line<'static> {
        match row {
            PanelRow::Top(top) => self.top_line(*top, selected, avail),
            PanelRow::ModelChild(id) => self.model_child_line(id, selected),
            PanelRow::ScopeChild(id) => self.scope_child_line(id, selected),
            PanelRow::ProviderChild(id) => self.provider_child_line(id, selected),
            PanelRow::PolicyTool(tool) => self.policy_tool_line(tool, selected, avail),
            PanelRow::PolicyBashExact(cmd) => bash_grant_line(&format!("bash: {cmd}"), selected),
            PanelRow::PolicyBashPrefix(pfx) => {
                bash_grant_line(&format!("bash prefix: {pfx}"), selected)
            }
        }
    }

    /// Whether a row's control is inert (dark but operable) because its master
    /// switch is off. The AUTO COMPACT knobs follow `automatic`
    /// (`compaction.enabled`); the tool-result knobs follow `tool result
    /// compaction` (`toolResultCompaction.enabled`).
    fn is_inert(&self, row: RowId) -> bool {
        match row {
            RowId::Field(
                Field::CompactionWarn
                | Field::CompactionStart
                | Field::CompactionHard
                | Field::CompactionKeepRecentTokens
                | Field::CompactionHardWait
                | Field::CompactionReactive
                | Field::CompactionSummarizer
                | Field::CompactionWorkerInput,
            ) => !self.snap.compaction_enabled,
            RowId::Field(
                Field::CompactionAggressiveness
                | Field::CompactionCacheTiming
                | Field::MicrocompactionWatermark
                | Field::SemanticRetainPerPath
                | Field::ToolClearingKeepRecent,
            ) => !self.snap.microcompaction,
            _ => false,
        }
    }

    fn top_line(&self, row: RowId, selected: bool, avail: usize) -> Line<'static> {
        let flashing = self
            .flash
            .as_ref()
            .is_some_and(|(r, _)| *r == PanelRow::Top(row));
        let mut spans: Vec<Span<'static>> = Vec::new();
        let name = label(row);
        let label_style = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        // The compaction groups' knobs are inert hardware while their master
        // switch is off — still operable, just dark. The AUTO COMPACT dials/
        // switches follow `automatic`; the tool-result knobs follow `tool result
        // compaction`.
        let inert = self.is_inert(row);
        // Pad to the shared label column, but always leave at least one space
        // so an over-long label (`tool result compaction`) never abuts its
        // control; shorter labels are unchanged (name + spaces to `LABEL_W`).
        let pad = LABEL_W.saturating_sub(name.chars().count()).max(1);
        spans.push(Span::styled(
            format!("  {name}{}", " ".repeat(pad)),
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
            self.push_top_control_spans(row, &mut spans, flashing, inert, avail);
        }
        Line::from(spans)
    }

    fn push_edit_spans(&self, spans: &mut Vec<Span<'static>>) {
        let Some(edit) = self.edit.as_ref() else {
            return;
        };
        spans.push(Span::raw(edit.buffer.clone()));
        spans.push(Span::styled(
            crate::ui::symbols::CARET.to_string(),
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

    fn push_top_control_spans(
        &self,
        row: RowId,
        spans: &mut Vec<Span<'static>>,
        flashing: bool,
        inert: bool,
        avail: usize,
    ) {
        // The label span (with its one-space gutter) is already pushed. Its
        // width is `LABEL_W + 2` for a short label, but an over-long one
        // (`tool result compaction`, 22 > LABEL_W) overhangs the fixed column,
        // leaving fewer cells than `LABEL_W + 2` implies. Size width-sensitive
        // controls against what the label actually consumed so a control never
        // prints past a narrow panel edge.
        let used_width: usize = spans.iter().map(|span| span.content.chars().count()).sum();
        let control_avail = avail.saturating_sub(used_width);
        match row {
            RowId::Model => {
                // The collapsed value prints the ACTIVE session engine, not the
                // persisted default — honest after an `s` session-only pick
                // (§10.1). When they agree (the common case; ←/→ cycling persists
                // on every click) it reads exactly as before; when they diverge
                // the active model carries a quiet dim `· session` tag.
                let (qualified, session_only) =
                    match self.snap.catalog.iter().find(|m| m.is_current) {
                        Some(model) => (model.qualified.as_str(), !model.is_default),
                        None => (self.snap.default_model.as_str(), false),
                    };
                let (provider, model) = qualified.split_once('/').unwrap_or(("", qualified));
                spans.push(self.port_marker(row));
                spans.push(Span::raw(model.to_string()));
                if !provider.is_empty() {
                    spans.push(Span::styled(
                        format!(" {} {provider}", crate::ui::symbols::SEP),
                        dim(),
                    ));
                }
                if session_only {
                    spans.push(Span::styled(" \u{00b7} session".to_string(), dim()));
                }
            }
            RowId::Scope => {
                spans.push(self.port_marker(row));
                let summary = if self.snap.scope_enabled.is_none() {
                    "all enabled".to_string()
                } else {
                    format!(
                        "{} of {} enabled",
                        self.scope_enabled_count(),
                        self.snap.scope_candidates.len()
                    )
                };
                let text = if self.scope_unsaved() {
                    format!("{summary} \u{00b7} unsaved")
                } else {
                    summary
                };
                spans.push(Span::styled(text, dim()));
            }
            RowId::Providers => {
                spans.push(self.port_marker(row));
                let connected = self
                    .snap
                    .providers
                    .iter()
                    .filter(|p| p.credentialed)
                    .count();
                spans.push(Span::styled(
                    match connected {
                        0 => "none connected".to_string(),
                        1 => "1 connected".to_string(),
                        n => format!("{n} connected"),
                    },
                    dim(),
                ));
            }
            RowId::Permissions => {
                spans.push(self.port_marker(row));
                spans.push(Span::styled("per-tool + bash grants".to_string(), dim()));
            }
            RowId::Reasoning => self.push_reasoning_spans(spans, flashing, avail),
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
                    control_avail,
                ));
                // Caution silkscreen: printed after the guard switch,
                // `┊`-joined metadata (never key-hint `·`). At a narrow width
                // it drops whole fields — ` ┊ saved default` first, then
                // `dangerous` — never truncating mid-word (a clipped `defau`
                // reads as a dead control, the footer drop-rule honesty). The
                // bypass persists via #520's permission-mode default, so the
                // qualifier reads `saved default`, not `session only`.
                let used: usize = spans.iter().map(|span| span.content.chars().count()).sum();
                let caution_avail = avail.saturating_sub(used);
                let full = format!("  dangerous {} saved default", crate::ui::symbols::SEP);
                let short = "  dangerous";
                if full.chars().count() <= caution_avail {
                    spans.push(Span::styled(full, dim()));
                } else if short.chars().count() <= caution_avail {
                    spans.push(Span::styled(short.to_string(), dim()));
                }
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
                        control_avail,
                    ));
                }
                Archetype::Dial => {
                    let value = self.snap.dial_value(field);
                    push_dial(
                        spans,
                        ladder(field),
                        value,
                        &dial_value_label(field, value),
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
                Archetype::Port => {}
            },
        }
    }

    /// The reasoning row IS the model hatch's effort track: while a candidate is
    /// highlighted it live-renders that candidate's levels with the target
    /// clamped to them; otherwise it prints the active model's real track (§3.1).
    fn push_reasoning_spans(&self, spans: &mut Vec<Span<'static>>, flashing: bool, avail: usize) {
        let track_avail = avail.saturating_sub(LABEL_W + 2);
        if let Some(model) = self.highlighted_model() {
            let effort =
                model_capabilities::clamp(model.provider, &model.model_id, self.model_target);
            let options: Vec<&str> = model.levels.iter().map(|(_, label)| *label).collect();
            let pos = model.levels.iter().position(|(level, _)| *level == effort);
            let current = pos.map(|p| options[p]).unwrap_or("");
            spans.extend(switch_spans(
                &options,
                pos,
                current,
                flashing,
                false,
                false,
                track_avail,
            ));
        } else {
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
                track_avail,
            ));
        }
    }

    fn model_child_line(&self, qualified: &str, selected: bool) -> Line<'static> {
        let name_w = self
            .snap
            .catalog
            .iter()
            .map(|m| m.display.chars().count())
            .max()
            .unwrap_or(0);
        let model = self.snap.catalog.iter().find(|m| m.qualified == qualified);
        let base = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        match model {
            Some(model) => {
                let marker = if model.is_current {
                    Span::styled(
                        format!("    {} ", crate::ui::symbols::ACTIVE),
                        Style::default().fg(crate::ui::palette::orange()),
                    )
                } else {
                    Span::raw("      ")
                };
                spans.push(marker);
                spans.push(Span::styled(format!("{:<name_w$}", model.display), base));
                spans.push(Span::raw("  "));
                spans.push(Span::styled(model.provider_label.clone(), dim()));
                if model.is_default {
                    spans.push(Span::styled("  default", dim()));
                }
            }
            None => spans.push(Span::styled(format!("    {qualified}"), base)),
        }
        Line::from(spans)
    }

    fn scope_child_line(&self, qualified: &str, selected: bool) -> Line<'static> {
        let base = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        // `enabled = None` ("all enabled") renders as the no-checkmark-column
        // form: the qualified id plus the provider, no `◉`/`○`.
        if self.snap.scope_enabled.is_none() {
            spans.push(Span::styled(format!("    {qualified}"), base));
            if let Some(candidate) = self
                .snap
                .scope_candidates
                .iter()
                .find(|c| c.qualified == qualified)
            {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(candidate.provider_label.clone(), dim()));
            }
        } else {
            let enabled = self.scope_is_enabled(qualified);
            let mark = if enabled {
                Span::styled(
                    format!("    {} ", crate::ui::symbols::ACTIVE),
                    Style::default().fg(crate::ui::palette::orange()),
                )
            } else {
                Span::styled(format!("    {} ", crate::ui::symbols::EMPTY), dim())
            };
            spans.push(mark);
            spans.push(Span::styled(qualified.to_string(), base));
        }
        Line::from(spans)
    }

    fn provider_child_line(&self, id: &str, selected: bool) -> Line<'static> {
        let name_w = self
            .snap
            .providers
            .iter()
            .map(|p| p.name.chars().count())
            .max()
            .unwrap_or(0);
        let base = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let mut spans: Vec<Span<'static>> = Vec::new();
        if let Some(status) = self.provider(id) {
            let mark = if status.credentialed {
                Span::styled(
                    format!("    {} ", crate::ui::symbols::ACTIVE),
                    Style::default().fg(crate::ui::palette::orange()),
                )
            } else {
                Span::styled(format!("    {} ", crate::ui::symbols::EMPTY), dim())
            };
            spans.push(mark);
            spans.push(Span::styled(format!("{:<name_w$}", status.name), base));
            spans.push(Span::raw("  "));
            spans.push(Span::styled(status.badge.clone(), dim()));
        } else {
            spans.push(Span::styled(format!("    {id}"), base));
        }
        Line::from(spans)
    }

    fn policy_tool_line(&self, tool: &str, selected: bool, avail: usize) -> Line<'static> {
        let flashing = self
            .flash
            .as_ref()
            .is_some_and(|(r, _)| matches!(r, PanelRow::PolicyTool(t) if t == tool));
        let granted = self.snap.policy.granted_tools.iter().any(|t| t == tool);
        let base = if selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        // Four-cell indent + a `LABEL_W - 2` label field puts the switch track on
        // the panel's control column (§2.4).
        let mut spans = vec![Span::styled(
            format!("    {tool:<width$}", width = LABEL_W - 2),
            base,
        )];
        let pos = usize::from(granted);
        spans.extend(switch_spans(
            &["ask", "always"],
            Some(pos),
            if granted { "always" } else { "ask" },
            flashing,
            false,
            false,
            avail.saturating_sub(LABEL_W + 2),
        ));
        Line::from(spans)
    }

    fn port_marker(&self, row: RowId) -> Span<'static> {
        let glyph = if self.expanded == Some(row) {
            crate::ui::symbols::EXPANDED
        } else {
            crate::ui::symbols::COLLAPSED
        };
        Span::styled(format!("{glyph} "), dim())
    }

    /// Keymap-honest footer: the verbs for the selected row only. Child rows
    /// print their per-port verbs; while a hatch is open, `esc` collapses (never
    /// "close") on every row (§2.3, §3).
    fn footer(&self) -> String {
        if let Some(edit) = self.edit.as_ref() {
            return if edit.numeric {
                "\u{21b5} save \u{00b7} esc cancel".to_string()
            } else {
                "\u{21b5} save \u{00b7} esc cancel \u{00b7} empty clears".to_string()
            };
        }
        match &self.cursor {
            PanelRow::Top(row) => self.top_footer(*row),
            PanelRow::ModelChild(_) => {
                "\u{2190}\u{2192} reasoning \u{00b7} \u{21b5} set default \u{00b7} s session \u{00b7} esc collapse"
                    .to_string()
            }
            PanelRow::ScopeChild(_) => {
                // `type to filter` is always live in the hatch; while a filter is
                // active `esc` clears it first, then the collapse layering applies.
                let mut parts = vec![
                    "\u{21b5} toggle",
                    "ctrl+a all",
                    "ctrl+x none",
                    "ctrl+p provider",
                    "alt+\u{2191}\u{2193} reorder",
                    "ctrl+s save",
                    "type to filter",
                ];
                if !self.scope_filter.is_empty() {
                    parts.push("esc clear");
                }
                parts.push("esc collapse");
                parts.join(" \u{00b7} ")
            }
            PanelRow::ProviderChild(id) => self.provider_footer(id),
            PanelRow::PolicyTool(_) => "\u{2190}\u{2192} set \u{00b7} esc collapse".to_string(),
            PanelRow::PolicyBashExact(_) | PanelRow::PolicyBashPrefix(_) => {
                "\u{21b5} revoke \u{00b7} esc collapse".to_string()
            }
        }
    }

    fn top_footer(&self, row: RowId) -> String {
        let hatch = self.expanded.is_some();
        let esc = if hatch { "esc collapse" } else { "esc close" };
        let expanded_here = self.expanded == Some(row);
        let verbs = if row == RowId::Model {
            if expanded_here {
                "\u{2190}\u{2192} cycle \u{00b7} \u{21b5} collapse".to_string()
            } else {
                "\u{2190}\u{2192} cycle \u{00b7} \u{21b5} open".to_string()
            }
        } else if is_port(row) {
            if expanded_here {
                "\u{21b5} collapse".to_string()
            } else {
                "\u{21b5} open".to_string()
            }
        } else {
            match archetype(row) {
                Archetype::Switch => "\u{2190}\u{2192} set".to_string(),
                Archetype::Dial => "\u{2190}\u{2192} adjust \u{00b7} \u{21b5} type".to_string(),
                Archetype::Register => "\u{21b5} edit".to_string(),
                Archetype::Port => String::new(),
            }
        };
        format!("\u{2191}\u{2193} select \u{00b7} {verbs} \u{00b7} {esc}")
    }

    /// The providers footer advertises only the verbs that exist for this row:
    /// `x logout` only when credentialed, `a api key` only when the provider
    /// takes one (§3.3, criterion 16).
    fn provider_footer(&self, id: &str) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if let Some(status) = self.provider(id) {
            if status.oauth_capable || status.api_key_capable {
                parts.push("\u{21b5} login");
            }
            if status.api_key_capable {
                parts.push("a api key");
            }
            if status.credentialed {
                parts.push("x logout");
            }
        }
        parts.push("esc collapse");
        parts.join(" \u{00b7} ")
    }
}

/// A dim, four-cell-indented silkscreen note printed inside a hatch (empty
/// states); not selectable.
fn child_note(text: &str) -> Line<'static> {
    Line::from(Span::styled(format!("    {text}"), dim()))
}

/// A revoke-only bash-grant row: `    <label>  ↵ revoke`, the hint muted.
fn bash_grant_line(label: &str, selected: bool) -> Line<'static> {
    let base = if selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Line::from(vec![
        Span::styled(format!("    {label}"), base),
        Span::styled(format!("  {} revoke", crate::ui::symbols::SEP), dim()),
    ])
}

/// Flatten a bracketed paste to a single line for a register or the scope
/// filter: each line break collapses to ONE space (`\r\n` is one break, not
/// two), trailing whitespace is trimmed. Neither field may ever embed a
/// newline.
fn flatten_paste(text: &str) -> String {
    let mut flat = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
    flat.truncate(flat.trim_end().len());
    flat
}

/// Normalize a scope for the `· unsaved` comparison: keep only ids that are
/// current candidates (preserving order), then fold a full set to `None`.
fn normalize_scope(scope: &Option<Vec<String>>, candidates: &[&str]) -> Option<Vec<String>> {
    let list = scope.as_ref()?;
    let kept: Vec<String> = list
        .iter()
        .filter(|id| candidates.contains(&id.as_str()))
        .cloned()
        .collect();
    if !kept.is_empty()
        && kept.len() >= candidates.len()
        && candidates.iter().all(|c| kept.iter().any(|k| k == c))
    {
        return None;
    }
    Some(kept)
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
/// permissions per-tool grants and the model hatch's reasoning track print
/// through this same function so the surfaces cannot drift.
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
    value_label: &str,
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
    spans.push(Span::styled(value_label.to_string(), value_style));
    if !unit.is_empty() {
        spans.push(Span::styled(unit.to_string(), dim()));
    }
}

/// The token persisted for a switch position. Bool switches show `off`/`on`
/// but persist `false`/`true`.
fn save_token(field: Field, value: &str) -> String {
    match field {
        Field::Microcompaction
        | Field::CompactionEnabled
        | Field::CompactionReactive
        | Field::ReducedMotion => (value == "on").to_string(),
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

    fn model_choice(
        provider: ProviderId,
        model_id: &str,
        is_current: bool,
        is_default: bool,
    ) -> ModelChoice {
        let qualified = format!("{}/{}", provider.as_str(), model_id);
        let levels = model_capabilities::level_options(provider, model_id)
            .iter()
            .map(|option| (option.level, option.label))
            .collect();
        ModelChoice {
            qualified,
            display: crate::mimir::model_catalog::display_name(&format!(
                "{}/{}",
                provider.as_str(),
                model_id
            )),
            provider_label: provider.display_name().to_string(),
            provider,
            model_id: model_id.to_string(),
            levels,
            is_current,
            is_default,
        }
    }

    fn scope_choice(provider: ProviderId, model_id: &str) -> ScopeChoice {
        ScopeChoice {
            qualified: format!("{}/{}", provider.as_str(), model_id),
            provider_label: provider.display_name().to_string(),
        }
    }

    fn provider_status(id: &str, oauth: bool, api_key: bool, credentialed: bool) -> ProviderStatus {
        ProviderStatus {
            id: id.to_string(),
            name: id.to_string(),
            badge: if credentialed {
                if oauth { "subscription" } else { "api key" }.to_string()
            } else {
                "\u{2014}".to_string()
            },
            oauth_capable: oauth,
            api_key_capable: api_key,
            credentialed,
        }
    }

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
            catalog: vec![
                model_choice(ProviderId::OpenAiCodex, "gpt-5.5", true, true),
                model_choice(ProviderId::Anthropic, "claude-sonnet-4-6", false, false),
                model_choice(ProviderId::Antigravity, "gemini-3.5-flash", false, false),
            ],
            scope_candidates: vec![
                scope_choice(ProviderId::OpenAiCodex, "gpt-5.5"),
                scope_choice(ProviderId::Anthropic, "claude-sonnet-4-6"),
                scope_choice(ProviderId::Antigravity, "gemini-3.5-flash"),
            ],
            scope_enabled: None,
            scope_persisted: None,
            providers: vec![
                provider_status("openai-codex", true, false, true),
                provider_status("anthropic", true, true, false),
                provider_status("openai", false, true, false),
            ],
            policy: PolicySnapshot {
                granted_tools: vec![],
                bash_exact: vec![],
                bash_prefix: vec![],
                sandbox: None,
            },
            default_approval: "strict".to_string(),
            skip_permissions: false,
            context_token_budget: 232_000,
            compaction_enabled: true,
            compaction_warn_pct: 60,
            compaction_start_pct: 72,
            compaction_hard_pct: 90,
            compaction_keep_recent_tokens: 8_000,
            compaction_hard_wait_ms: 120_000,
            compaction_reactive: true,
            compaction_worker_input: "transcript".to_string(),
            resolved_ladder: Some(ResolvedLadder {
                warn: 139_200,
                start: 167_040,
                hard: 208_800,
                effective_tail: 8_000,
                configured_tail: 8_000,
                effective_window: 232_000,
            }),
            compaction_summarizer: "subagent".to_string(),
            microcompaction: false,
            microcompaction_watermark: 32_000,
            compaction_aggressiveness: "conservative".to_string(),
            compaction_cache_timing: "cacheAware".to_string(),
            semantic_retain_per_path: 1,
            tool_clearing_keep_recent: 8,
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

    fn select(panel: &mut SettingsPanel, row: PanelRow) {
        panel.cursor = row;
    }

    fn select_top(panel: &mut SettingsPanel, row: RowId) {
        panel.cursor = PanelRow::Top(row);
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

    fn expand(panel: &mut SettingsPanel, port: RowId) {
        select_top(panel, port);
        panel.handle_key(ModalKey::Enter);
    }

    #[test]
    fn faceplate_lists_every_section_and_prunes_the_service_hatch() {
        let lines = panel().render_budgeted(80, 60);
        let rendered = text(&lines);
        for section in [
            "ENGINE",
            "SAFETY",
            "AUTO COMPACT",
            "MEMORY",
            "CHECKS",
            "PANEL",
            "GIT",
        ] {
            assert!(rendered.contains(section), "{section} missing:\n{rendered}");
        }
        assert!(!rendered.to_lowercase().contains("bash tool"));
        assert!(!rendered.to_lowercase().contains("round-trips"));
        assert!(!rendered.to_lowercase().contains("roundtrips"));
        assert!(rendered.contains("SETTINGS"));
        assert!(rendered.contains(&format!("iris {REV}")));
    }

    #[test]
    fn auto_compact_section_prints_the_new_rows_and_renamed_tool_result_labels() {
        let rendered = text(&panel().render_budgeted(120, 80));
        // The AUTO COMPACT rows.
        assert!(rendered.contains("automatic"), "{rendered}");
        assert!(rendered.contains("warn at"), "{rendered}");
        assert!(rendered.contains("start at"), "{rendered}");
        assert!(rendered.contains("hard at"), "{rendered}");
        assert!(rendered.contains("retain tail"), "{rendered}");
        assert!(rendered.contains("hard wait"), "{rendered}");
        assert!(rendered.contains("reactive"), "{rendered}");
        assert!(rendered.contains("worker input"), "{rendered}");
        // Percent dials print the honest percent + `%` unit.
        assert!(rendered.contains("60%"), "{rendered}");
        assert!(rendered.contains("72%"), "{rendered}");
        assert!(rendered.contains("90%"), "{rendered}");
        // The hard-wait dial prints its default (120000 ms) as a duration.
        assert!(rendered.contains("2m"), "{rendered}");
        // The renamed context-cap row and tool-result labels.
        assert!(rendered.contains("context cap"), "{rendered}");
        assert!(rendered.contains("tool result compaction"), "{rendered}");
        assert!(rendered.contains("fold trigger"), "{rendered}");
        assert!(rendered.contains("keep tool uses"), "{rendered}");
        // The stale labels are gone.
        assert!(!rendered.contains("compact at"), "{rendered}");
        assert!(!rendered.contains("trigger at"), "{rendered}");
        assert!(!rendered.contains("keep recent"), "{rendered}");
    }

    #[test]
    fn overlong_label_control_never_overflows_a_narrow_panel() {
        // `tool result compaction` (22 chars) overhangs the fixed LABEL_W (18)
        // column, so a control sized against `LABEL_W + 2` is told it has more
        // room than the label actually left. At a narrow width the switch must
        // fall back to its rotary form instead of printing the full scale past
        // the panel edge (the drop-rule honesty, §9.1).
        let panel = panel();
        let avail = 32;
        let line = panel.control_line(
            &PanelRow::Top(RowId::Field(Field::Microcompaction)),
            false,
            avail,
        );
        let rendered = text(std::slice::from_ref(&line));
        assert!(
            rendered.contains("tool result compaction"),
            "the long label still renders: {rendered:?}"
        );
        assert!(
            line_width(&line) <= avail,
            "the control overflows the {avail}-cell panel: width {} in {rendered:?}",
            line_width(&line),
        );
    }

    #[test]
    fn resolved_ladder_line_prints_effective_tokens_and_tail_adjustment() {
        // A large window: tail is unclamped, so no configured->effective note.
        let rendered = text(&panel().render_budgeted(120, 80));
        assert!(
            rendered.contains("active model: warn 139k / start 167k / hard 208k / tail 8k"),
            "{rendered}"
        );
        assert!(!rendered.contains("configured tail"), "{rendered}");

        // A small window clamps the tail to a quarter of the window: the panel
        // shows the configured -> effective adjustment.
        let mut snap = snapshot();
        snap.resolved_ladder = Some(ResolvedLadder {
            warn: 9_600,
            start: 11_520,
            hard: 14_400,
            effective_tail: 4_000,
            configured_tail: 8_000,
            effective_window: 16_000,
        });
        let rendered = text(&SettingsPanel::new(snap).render_budgeted(120, 80));
        assert!(
            rendered.contains("configured tail 8k \u{2192} effective tail 4k"),
            "{rendered}"
        );
    }

    #[test]
    fn auto_compact_knobs_go_dark_while_automatic_is_off() {
        let mut snap = snapshot();
        snap.compaction_enabled = false;
        let panel = SettingsPanel::new(snap);
        // The dependent knobs are inert; the master switch never is.
        assert!(panel.is_inert(RowId::Field(Field::CompactionWarn)));
        assert!(panel.is_inert(RowId::Field(Field::CompactionKeepRecentTokens)));
        assert!(panel.is_inert(RowId::Field(Field::CompactionHardWait)));
        assert!(panel.is_inert(RowId::Field(Field::CompactionReactive)));
        assert!(panel.is_inert(RowId::Field(Field::CompactionSummarizer)));
        assert!(panel.is_inert(RowId::Field(Field::CompactionWorkerInput)));
        assert!(!panel.is_inert(RowId::Field(Field::CompactionEnabled)));
    }

    #[test]
    fn percent_dial_clicks_one_detent_and_emits_the_percent_value() {
        let mut panel = panel();
        select_top(&mut panel, RowId::Field(Field::CompactionWarn));
        // warn starts at 60%; one click right lands on the next percent detent.
        let outcome = panel.handle_key(ModalKey::Right);
        assert_eq!(panel.snap.compaction_warn_pct, 70);
        match outcome {
            ModalOutcome::Emit(ModalAction::SaveSetting { field, value }) => {
                assert_eq!(field, Field::CompactionWarn);
                assert_eq!(value.as_deref(), Some("70"));
            }
            other => panic!("expected a save emit, got {other:?}"),
        }
    }

    #[test]
    fn switch_clicks_one_detent_and_clamps_at_the_stop() {
        let mut panel = panel();
        select_top(&mut panel, RowId::Field(Field::DefaultApproval));
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::DefaultApproval,
                value: Some("auto".to_string()),
            })
        );
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::DefaultApproval,
                value: Some("never".to_string()),
            })
        );
        assert_eq!(panel.handle_key(ModalKey::Right), ModalOutcome::Ignore);
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
        select_top(&mut panel, RowId::Field(Field::Microcompaction));
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::Microcompaction,
                value: Some("true".to_string()),
            })
        );
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
        select_top(&mut panel, RowId::SkipApprovals);
        assert_eq!(panel.handle_key(ModalKey::Left), ModalOutcome::Ignore);
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::ToggleSkipPermissions)
        );
        assert!(panel.snap.skip_permissions);
    }

    #[test]
    fn dial_snaps_an_off_ladder_value_into_the_ladder() {
        let mut panel = panel();
        select_top(&mut panel, RowId::Field(Field::ContextTokenBudget));
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::ContextTokenBudget,
                value: Some("300000".to_string()),
            })
        );
        panel.snap.context_token_budget = 90_000;
        assert_eq!(
            panel.handle_key(ModalKey::Left),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::ContextTokenBudget,
                value: Some("64000".to_string()),
            })
        );
        panel.snap.context_token_budget = 64_000;
        assert_eq!(panel.handle_key(ModalKey::Left), ModalOutcome::Ignore);
    }

    #[test]
    fn dial_enter_types_a_precise_value_and_clamps_to_bounds() {
        let mut panel = panel();
        select_top(&mut panel, RowId::Field(Field::ScrollSpeed));
        assert_eq!(panel.handle_key(ModalKey::Enter), ModalOutcome::Redraw);
        panel.handle_key(ModalKey::Backspace);
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
        panel.handle_key(ModalKey::Enter);
        panel.handle_key(ModalKey::Char('x'));
        assert_eq!(panel.handle_key(ModalKey::Enter), ModalOutcome::Redraw);
        assert!(panel.edit.as_ref().is_some_and(|e| e.error.is_some()));
        assert_eq!(panel.handle_key(ModalKey::Esc), ModalOutcome::Redraw);
        assert!(panel.edit.is_none());
    }

    #[test]
    fn hard_wait_dial_steps_persist_milliseconds_and_render_duration() {
        let mut panel = panel();
        select_top(&mut panel, RowId::Field(Field::CompactionHardWait));
        // The default lands on step 4 (120000 ms); one click right is step 5.
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::CompactionHardWait,
                value: Some("150000".to_string()),
            })
        );
        assert_eq!(panel.snap.compaction_hard_wait_ms, 150_000);
        // Non-whole-minute detents print bare seconds; whole minutes print `Nm`.
        assert!(text(&panel.render_budgeted(120, 80)).contains("150s"));
        panel.snap.compaction_hard_wait_ms = 300_000;
        // At the 300000 ms cap the dial is pinned; right is a no-op, and the
        // value renders as `5m`.
        assert_eq!(panel.handle_key(ModalKey::Right), ModalOutcome::Ignore);
        assert!(text(&panel.render_budgeted(120, 80)).contains("5m"));
    }

    #[test]
    fn hard_wait_dial_enter_clamps_a_typed_value_to_the_cap() {
        let mut panel = panel();
        select_top(&mut panel, RowId::Field(Field::CompactionHardWait));
        assert_eq!(panel.handle_key(ModalKey::Enter), ModalOutcome::Redraw);
        for _ in 0.."120000".len() {
            panel.handle_key(ModalKey::Backspace);
        }
        for ch in "999999".chars() {
            panel.handle_key(ModalKey::Char(ch));
        }
        // A hand-typed value past the cap clamps to 300000 ms before it persists.
        assert_eq!(
            panel.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::CompactionHardWait,
                value: Some("300000".to_string()),
            })
        );
        assert_eq!(panel.snap.compaction_hard_wait_ms, 300_000);
    }

    #[test]
    fn register_edits_inline_and_empty_clears() {
        let mut panel = panel();
        select_top(&mut panel, RowId::Field(Field::VerifyCommand));
        assert_eq!(panel.handle_key(ModalKey::Enter), ModalOutcome::Redraw);
        panel.push_str("  cargo test  ");
        assert_eq!(
            panel.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::VerifyCommand,
                value: Some("cargo test".to_string()),
            })
        );
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
        let mut panel = panel();
        select_top(&mut panel, RowId::Model);
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::CycleModel { forward: true })
        );
        assert_eq!(
            panel.handle_key(ModalKey::Left),
            ModalOutcome::Emit(ModalAction::CycleModel { forward: false })
        );
        assert!(panel.footer().contains("\u{2190}\u{2192} cycle"));
        assert!(panel.footer().contains("\u{21b5} open"));
    }

    #[test]
    fn the_collapsed_model_row_prints_the_active_engine_and_tags_a_session_pick() {
        // Common case: the active session model IS the persisted default, so the
        // row reads exactly as before — no `· session` tag.
        let panel = panel();
        let rendered = text(&panel.render_budgeted(120, 80));
        let model_line = rendered
            .lines()
            .find(|line| line.contains("gpt-5.5"))
            .expect("model row");
        assert!(model_line.contains("openai-codex"), "{model_line}");
        assert!(
            !model_line.contains("session"),
            "no tag when active == default: {model_line}"
        );

        // Session-only pick: the active engine diverges from the persisted
        // default, so the active model + provider print with a quiet `· session`.
        let mut snap = snapshot();
        snap.catalog = vec![
            model_choice(ProviderId::OpenAiCodex, "gpt-5.5", false, true),
            model_choice(ProviderId::Anthropic, "claude-sonnet-4-6", true, false),
        ];
        let panel = SettingsPanel::new(snap);
        let rendered = text(&panel.render_budgeted(120, 80));
        let model_line = rendered
            .lines()
            .find(|line| line.contains("claude-sonnet-4-6"))
            .expect("model row");
        assert!(
            model_line.contains("anthropic"),
            "active provider: {model_line}"
        );
        assert!(
            model_line.contains("\u{00b7} session"),
            "session tag: {model_line}"
        );
        assert!(
            !model_line.contains("gpt-5.5"),
            "prints the active engine, not the persisted default: {model_line}"
        );
    }

    // --- criterion 1: expand each port in place ---
    #[test]
    fn enter_on_each_port_expands_in_place_with_the_cursor_on_the_header() {
        for port in [
            RowId::Model,
            RowId::Scope,
            RowId::Providers,
            RowId::Permissions,
        ] {
            let mut panel = panel();
            select_top(&mut panel, port);
            assert_eq!(panel.handle_key(ModalKey::Enter), ModalOutcome::Redraw);
            assert_eq!(panel.expanded, Some(port), "{port:?} expanded");
            // Cursor stays on the header; the header flash is armed.
            assert_eq!(panel.cursor, PanelRow::Top(port));
            assert_eq!(
                panel.flash.as_ref().map(|(r, _)| r.clone()),
                Some(PanelRow::Top(port))
            );
            // Children are in the flattened selectable list.
            assert!(
                panel
                    .selectable()
                    .iter()
                    .any(|r| !matches!(r, PanelRow::Top(_))),
                "{port:?} children present"
            );
            // The expanded marker (▾) renders on the header row.
            let rendered = text(&panel.render_budgeted(100, 60));
            assert!(
                rendered.contains(crate::ui::symbols::EXPANDED),
                "{port:?} shows ▾:\n{rendered}"
            );
        }
    }

    #[test]
    fn expand_arms_no_flash_under_reduced_motion() {
        let mut snap = snapshot();
        snap.reduced_motion = true;
        let mut panel = SettingsPanel::new(snap);
        expand(&mut panel, RowId::Model);
        assert!(panel.flash.is_none(), "reduced motion settles instantly");
    }

    // --- criterion 2: collapse verbs + two-step esc ---
    #[test]
    fn enter_on_expanded_header_collapses_and_esc_from_a_child_collapses() {
        let mut panel = panel();
        expand(&mut panel, RowId::Model);
        // ↵ on the expanded header collapses.
        assert_eq!(panel.handle_key(ModalKey::Enter), ModalOutcome::Redraw);
        assert!(panel.expanded.is_none());
        assert_eq!(panel.cursor, PanelRow::Top(RowId::Model));

        // esc from a child collapses (cursor → header).
        expand(&mut panel, RowId::Providers);
        panel.handle_key(ModalKey::Down); // onto a provider child
        assert!(matches!(panel.cursor, PanelRow::ProviderChild(_)));
        assert_eq!(panel.handle_key(ModalKey::Esc), ModalOutcome::Redraw);
        assert!(panel.expanded.is_none());
        assert_eq!(panel.cursor, PanelRow::Top(RowId::Providers));

        // esc with no hatch open closes the panel; the old two-step holds:
        // hatch open + esc esc = collapse, then close.
        expand(&mut panel, RowId::Scope);
        assert_eq!(panel.handle_key(ModalKey::Esc), ModalOutcome::Redraw); // collapse
        assert_eq!(panel.handle_key(ModalKey::Esc), ModalOutcome::Close); // close
    }

    // --- criterion 3: accordion ---
    #[test]
    fn expanding_a_second_port_collapses_the_first_in_the_same_keypress() {
        let mut panel = panel();
        expand(&mut panel, RowId::Scope);
        assert_eq!(panel.expanded, Some(RowId::Scope));
        expand(&mut panel, RowId::Providers);
        assert_eq!(panel.expanded, Some(RowId::Providers));
        // At most one ▾ ever renders.
        let rendered = text(&panel.render_budgeted(100, 60));
        assert_eq!(
            rendered.matches(crate::ui::symbols::EXPANDED).count(),
            1,
            "exactly one hatch open:\n{rendered}"
        );
    }

    // --- criterion 4: traversal wraps, silkscreen skipped ---
    #[test]
    fn navigation_wraps_over_the_flattened_list_and_skips_silkscreen() {
        let mut panel = panel();
        assert_eq!(panel.selected(), PanelRow::Top(RowId::Model));
        panel.handle_key(ModalKey::Up);
        assert_eq!(
            panel.selected(),
            PanelRow::Top(RowId::Field(Field::WorktreeRoot)),
            "wraps to the last control, never a header"
        );
        panel.handle_key(ModalKey::Down);
        assert_eq!(panel.selected(), PanelRow::Top(RowId::Model));
        // Header → children → next control while expanded.
        expand(&mut panel, RowId::Model);
        panel.handle_key(ModalKey::Down);
        assert!(matches!(panel.cursor, PanelRow::ModelChild(_)));
    }

    #[test]
    fn the_sandbox_line_is_never_selectable() {
        let mut snap = snapshot();
        snap.policy.sandbox = Some("workspace-write".to_string());
        let mut panel = SettingsPanel::new(snap);
        expand(&mut panel, RowId::Permissions);
        // The permissions children are policy rows only — the sandbox posture is
        // never among them (it is a read-only silkscreen line).
        assert!(
            panel
                .children_of(Some(RowId::Permissions))
                .iter()
                .all(|r| matches!(
                    r,
                    PanelRow::PolicyTool(_)
                        | PanelRow::PolicyBashExact(_)
                        | PanelRow::PolicyBashPrefix(_)
                )),
            "permissions children are policy rows"
        );
        // The sandbox line renders (dim, read-only) but no selectable row draws
        // the posture — it can never take the cursor.
        let rendered = text(&panel.render_budgeted(100, 60));
        assert!(rendered.contains("workspace-write"), "{rendered}");
        assert!(
            !panel.selectable().iter().any(|row| {
                text(&[panel.control_line(row, false, 100)]).contains("workspace-write")
            }),
            "sandbox posture is never on a selectable row"
        );
    }

    // --- criterion 5: identity-keyed cursor survives a reflow ---
    #[test]
    fn expanding_a_port_above_the_cursor_keeps_the_selection_row() {
        let mut panel = panel();
        // Cursor on a lower row (providers header).
        select_top(&mut panel, RowId::Providers);
        // Expand the model hatch above it (via a fresh panel + accordion is not
        // it; simulate the reflow by expanding model then confirming providers
        // header is still the cursor is wrong — instead expand model while the
        // cursor is on providers by setting expanded directly through the API).
        panel.expanded = Some(RowId::Model);
        // A reflow (children spliced above) must not move the identity cursor.
        assert_eq!(panel.cursor, PanelRow::Top(RowId::Providers));
        // And the row is still found in the flattened list.
        assert!(
            panel
                .selectable()
                .contains(&PanelRow::Top(RowId::Providers))
        );
    }

    #[test]
    fn a_flash_survives_a_reflow() {
        let mut panel = panel();
        select_top(&mut panel, RowId::Field(Field::DefaultApproval));
        panel.handle_key(ModalKey::Right);
        let flashed = panel.flash.as_ref().map(|(r, _)| r.clone());
        assert_eq!(
            flashed,
            Some(PanelRow::Top(RowId::Field(Field::DefaultApproval)))
        );
        // Reflow: open a hatch elsewhere. The flash still targets the same row.
        panel.expanded = Some(RowId::Model);
        let rendered = text(&panel.render_budgeted(100, 60));
        assert!(rendered.contains("auto"), "{rendered}");
    }

    // --- criterion 6: windowing ---
    #[test]
    fn windowing_keeps_the_cursor_visible_and_counts_the_flattened_list() {
        let mut panel = panel();
        expand(&mut panel, RowId::Model);
        // Move to the last model child under a tight budget.
        panel.handle_key(ModalKey::Down);
        panel.handle_key(ModalKey::Down);
        panel.handle_key(ModalKey::Down);
        let total = panel.selectable().len();
        let lines = panel.render_budgeted(80, 12);
        let rendered = text(&lines);
        // The cursor's model row is visible.
        assert!(
            rendered.contains("Gemini"),
            "cursor row visible:\n{rendered}"
        );
        // Position row counts the flattened selectable list, masthead pinned.
        let idx = panel
            .selectable()
            .iter()
            .position(|r| *r == panel.cursor)
            .unwrap();
        assert!(
            rendered.contains(&format!("({}/{})", idx + 1, total)),
            "flattened (n/N):\n{rendered}"
        );
        assert!(
            rendered.contains("SETTINGS"),
            "masthead pinned:\n{rendered}"
        );
    }

    // --- criterion 8: model rows ordered, marked, tagged ---
    #[test]
    fn model_hatch_marks_active_tags_default_and_mutes_provider() {
        let mut panel = panel();
        expand(&mut panel, RowId::Model);
        let rendered = text(&panel.render_budgeted(100, 60));
        // Default-first: gpt-5.5 (current+default) is the first child.
        assert!(rendered.contains("GPT 5.5"), "{rendered}");
        assert!(
            rendered.contains(crate::ui::symbols::ACTIVE),
            "active mark:\n{rendered}"
        );
        assert!(rendered.contains("default"), "default tag:\n{rendered}");
        assert!(
            rendered.contains("OpenAI Codex"),
            "provider column:\n{rendered}"
        );
    }

    // --- criterion 9: navigation clamps display without mutating the target ---
    #[test]
    fn arrowing_over_a_low_cap_model_clamps_display_but_preserves_the_target() {
        // Set target to xhigh; gemini caps at high.
        let mut snap = snapshot();
        snap.reasoning = ReasoningEffort::XHigh;
        let mut panel = SettingsPanel::new(snap);
        panel.model_target = ReasoningEffort::XHigh;
        expand(&mut panel, RowId::Model);
        // Onto gemini (caps below xhigh): the reasoning row shows the clamp.
        select(
            &mut panel,
            PanelRow::ModelChild("antigravity/gemini-3.5-flash".to_string()),
        );
        let capped = model_capabilities::clamp(
            ProviderId::Antigravity,
            "gemini-3.5-flash",
            ReasoningEffort::XHigh,
        );
        assert_ne!(capped, ReasoningEffort::XHigh, "gemini caps below xhigh");
        // The target is untouched by navigation.
        assert_eq!(panel.model_target, ReasoningEffort::XHigh);
    }

    // --- criterion 10: model select emits + hatch stays open ---
    #[test]
    fn model_child_enter_and_s_emit_select_with_the_displayed_effort() {
        let mut panel = panel();
        expand(&mut panel, RowId::Model);
        select(
            &mut panel,
            PanelRow::ModelChild("anthropic/claude-sonnet-4-6".to_string()),
        );
        match panel.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::SelectModel {
                id, save_default, ..
            }) => {
                assert_eq!(id, "anthropic/claude-sonnet-4-6");
                assert!(save_default, "Enter persists the default");
            }
            other => panic!("expected SelectModel, got {other:?}"),
        }
        // The hatch does not slam itself shut.
        assert_eq!(panel.expanded, Some(RowId::Model));
        match panel.handle_key(ModalKey::Char('s')) {
            ModalOutcome::Emit(ModalAction::SelectModel { save_default, .. }) => {
                assert!(!save_default, "s applies for this session only");
            }
            other => panic!("expected SelectModel, got {other:?}"),
        }
    }

    #[test]
    fn model_child_left_right_clicks_the_effort_and_clamps_at_stops() {
        let mut panel = panel();
        panel.model_target = ReasoningEffort::Medium;
        expand(&mut panel, RowId::Model);
        // gpt-5.5 supports up to xhigh: click right toward higher effort.
        select(
            &mut panel,
            PanelRow::ModelChild("openai-codex/gpt-5.5".to_string()),
        );
        assert_eq!(panel.handle_key(ModalKey::Right), ModalOutcome::Redraw);
        assert_eq!(panel.model_target, ReasoningEffort::High);
        assert_eq!(panel.handle_key(ModalKey::Right), ModalOutcome::Redraw);
        assert_eq!(panel.model_target, ReasoningEffort::XHigh);
        // Against the top stop: clamp, no wrap.
        assert_eq!(panel.handle_key(ModalKey::Right), ModalOutcome::Ignore);
    }

    // --- criterion 11: reasoning row reverts to active truth on the header ---
    #[test]
    fn reasoning_row_reverts_to_the_active_model_when_not_on_a_child() {
        let mut panel = panel();
        expand(&mut panel, RowId::Model);
        // Cursor on the header (not a child): the reasoning row uses the active
        // model's levels + level, not a candidate's.
        assert!(panel.highlighted_model().is_none());
        let rendered = text(&panel.render_budgeted(120, 60));
        assert!(rendered.contains("medium"), "active truth:\n{rendered}");
    }

    // --- reasoning row (top-level switch) still clicks ---
    #[test]
    fn reasoning_clicks_through_the_active_models_levels() {
        let mut panel = panel();
        select_top(&mut panel, RowId::Reasoning);
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

    // --- criterion 13: scope toggle + header count + unsaved tag ---
    #[test]
    fn scope_toggle_emits_apply_and_the_header_tracks_count_and_unsaved() {
        let mut panel = panel();
        expand(&mut panel, RowId::Scope);
        // all enabled while None.
        let rendered = text(&panel.render_budgeted(100, 60));
        assert!(rendered.contains("all enabled"), "{rendered}");
        // From all-enabled, toggling a row starts a one-item explicit list (the
        // pi-mono semantic: only that row enabled) — ApplyScoped, unsaved.
        panel.handle_key(ModalKey::Down);
        match panel.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::ApplyScoped(Some(ids))) => {
                assert_eq!(ids.len(), 1, "explicit list with just the toggled row");
            }
            other => panic!("expected ApplyScoped(Some), got {other:?}"),
        }
        let rendered = text(&panel.render_budgeted(100, 60));
        assert!(rendered.contains("1 of 3 enabled"), "{rendered}");
        assert!(rendered.contains("unsaved"), "{rendered}");
        // ctrl+s persists and clears the tag.
        match panel.handle_key(ModalKey::CtrlS) {
            ModalOutcome::Emit(ModalAction::SaveScoped(_)) => {}
            other => panic!("expected SaveScoped, got {other:?}"),
        }
        assert!(!panel.scope_unsaved(), "ctrl+s clears the unsaved tag");
    }

    // --- criterion 14: ctrl+a/x/p, collapse_full, explicit empty ---
    #[test]
    fn scope_bulk_ops_collapse_full_and_survive_explicit_empty() {
        let mut panel = panel();
        expand(&mut panel, RowId::Scope);
        // ctrl+x clears to an explicit empty set (Some([])).
        match panel.handle_key(ModalKey::CtrlX) {
            ModalOutcome::Emit(ModalAction::ApplyScoped(scope)) => {
                assert_eq!(scope.as_deref(), Some(&[][..]));
            }
            other => panic!("expected ApplyScoped(Some([])), got {other:?}"),
        }
        // Some([]) renders (nothing enabled), does not collapse to None.
        assert_eq!(panel.snap.scope_enabled.as_deref(), Some(&[][..]));
        // ctrl+a re-enables everything, which collapses to None (all enabled).
        match panel.handle_key(ModalKey::CtrlA) {
            ModalOutcome::Emit(ModalAction::ApplyScoped(None)) => {}
            other => panic!("expected ApplyScoped(None), got {other:?}"),
        }
        assert!(
            panel.snap.scope_enabled.is_none(),
            "full set collapses to None"
        );
    }

    // --- criterion 14: ctrl+p toggles a whole provider (incl. the filtered set) ---
    #[test]
    fn scope_ctrl_p_toggles_the_whole_provider_and_bulk_ops_honor_the_filter() {
        let mut snap = snapshot();
        // Two anthropic candidates so a provider-wide toggle is observable.
        snap.scope_candidates = vec![
            scope_choice(ProviderId::OpenAiCodex, "gpt-5.5"),
            scope_choice(ProviderId::Anthropic, "claude-sonnet-4-6"),
            scope_choice(ProviderId::Anthropic, "claude-haiku-4-5"),
            scope_choice(ProviderId::Antigravity, "gemini-3.5-flash"),
        ];
        let mut panel = SettingsPanel::new(snap);
        expand(&mut panel, RowId::Scope);
        // From all-enabled (None), ctrl+p on an anthropic row disables the whole
        // provider and leaves the others enabled.
        select(
            &mut panel,
            PanelRow::ScopeChild("anthropic/claude-sonnet-4-6".to_string()),
        );
        match panel.handle_key(ModalKey::CtrlP) {
            ModalOutcome::Emit(ModalAction::ApplyScoped(Some(ids))) => {
                assert!(
                    !ids.iter().any(|id| id.starts_with("anthropic/")),
                    "both anthropic models dropped: {ids:?}"
                );
                assert!(
                    ids.contains(&"openai-codex/gpt-5.5".to_string())
                        && ids.contains(&"antigravity/gemini-3.5-flash".to_string()),
                    "other providers untouched: {ids:?}"
                );
            }
            other => panic!("expected ApplyScoped(Some), got {other:?}"),
        }
        // ctrl+p again re-enables the whole provider — back to the full set, so
        // it collapses to None (all enabled).
        match panel.handle_key(ModalKey::CtrlP) {
            ModalOutcome::Emit(ModalAction::ApplyScoped(scope)) => {
                assert!(scope.is_none(), "re-enabling the provider re-fills the set");
            }
            other => panic!("expected ApplyScoped, got {other:?}"),
        }
        // Filtered-set scoping: a live filter narrows ctrl+x to the matching rows.
        for c in "haiku".chars() {
            panel.handle_key(ModalKey::Char(c));
        }
        match panel.handle_key(ModalKey::CtrlX) {
            ModalOutcome::Emit(ModalAction::ApplyScoped(Some(ids))) => {
                assert!(
                    !ids.contains(&"anthropic/claude-haiku-4-5".to_string()),
                    "only the filtered row was disabled: {ids:?}"
                );
                assert!(
                    ids.contains(&"anthropic/claude-sonnet-4-6".to_string()),
                    "unfiltered rows stay enabled: {ids:?}"
                );
            }
            other => panic!("expected ApplyScoped(Some), got {other:?}"),
        }
    }

    // --- criterion 14: alt+↑↓ reorders an enabled id and re-applies ---
    #[test]
    fn scope_reorder_moves_an_enabled_id_and_emits_apply() {
        let mut snap = snapshot();
        snap.scope_enabled = Some(vec![
            "openai-codex/gpt-5.5".to_string(),
            "anthropic/claude-sonnet-4-6".to_string(),
        ]);
        let mut panel = SettingsPanel::new(snap);
        expand(&mut panel, RowId::Scope);
        // Cursor on the first enabled row; alt+down swaps it below the second and
        // re-applies the live scope.
        select(
            &mut panel,
            PanelRow::ScopeChild("openai-codex/gpt-5.5".to_string()),
        );
        match panel.handle_key(ModalKey::AltDown) {
            ModalOutcome::Emit(ModalAction::ApplyScoped(Some(ids))) => {
                assert_eq!(
                    ids,
                    vec![
                        "anthropic/claude-sonnet-4-6".to_string(),
                        "openai-codex/gpt-5.5".to_string(),
                    ]
                );
            }
            other => panic!("expected ApplyScoped(Some), got {other:?}"),
        }
        // The identity-keyed cursor rode the move — still on the same model.
        assert_eq!(
            panel.cursor,
            PanelRow::ScopeChild("openai-codex/gpt-5.5".to_string())
        );
        // Reorder is bounded: on the top enabled row alt+up cannot climb further,
        // so the order is unchanged (still re-applied as the live scope).
        select(
            &mut panel,
            PanelRow::ScopeChild("anthropic/claude-sonnet-4-6".to_string()),
        );
        match panel.handle_key(ModalKey::AltUp) {
            ModalOutcome::Emit(ModalAction::ApplyScoped(Some(ids))) => {
                assert_eq!(
                    ids,
                    vec![
                        "anthropic/claude-sonnet-4-6".to_string(),
                        "openai-codex/gpt-5.5".to_string(),
                    ],
                    "top row cannot climb further"
                );
            }
            other => panic!("expected ApplyScoped(Some), got {other:?}"),
        }
    }

    // --- criterion 15: type-to-filter, esc clears filter first ---
    #[test]
    fn scope_type_to_filter_narrows_children_and_esc_clears_it_first() {
        let mut panel = panel();
        expand(&mut panel, RowId::Scope);
        for c in "gemini".chars() {
            panel.handle_key(ModalKey::Char(c));
        }
        assert_eq!(panel.scope_children().len(), 1, "filtered to gemini");
        // esc clears the filter first, does not collapse.
        assert_eq!(panel.handle_key(ModalKey::Esc), ModalOutcome::Redraw);
        assert!(panel.scope_filter.is_empty());
        assert_eq!(panel.expanded, Some(RowId::Scope), "still open");
        // A second esc collapses.
        assert_eq!(panel.handle_key(ModalKey::Esc), ModalOutcome::Redraw);
        assert!(panel.expanded.is_none());
        // Filter does not leak across collapse.
        expand(&mut panel, RowId::Scope);
        assert!(panel.scope_filter.is_empty());
    }

    // --- criterion 16/17/18: providers footer + emits ---
    #[test]
    fn providers_hatch_advertises_only_real_verbs_and_emits_correctly() {
        let mut panel = panel();
        expand(&mut panel, RowId::Providers);
        // openai-codex: oauth-capable, credentialed, no api-key path.
        select(
            &mut panel,
            PanelRow::ProviderChild("openai-codex".to_string()),
        );
        let footer = panel.footer();
        assert!(footer.contains("\u{21b5} login"), "{footer}");
        assert!(footer.contains("x logout"), "credentialed: {footer}");
        assert!(!footer.contains("a api key"), "no api-key path: {footer}");
        match panel.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::BeginLogin(ProviderId::OpenAiCodex)) => {}
            other => panic!("expected BeginLogin, got {other:?}"),
        }
        // openai: api-key only, uncredentialed → ↵ opens the api-key dialog.
        select(&mut panel, PanelRow::ProviderChild("openai".to_string()));
        let footer = panel.footer();
        assert!(footer.contains("a api key"), "{footer}");
        assert!(!footer.contains("x logout"), "uncredentialed: {footer}");
        match panel.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::OpenApiKeyDialog(id)) => assert_eq!(id, "openai"),
            other => panic!("expected OpenApiKeyDialog, got {other:?}"),
        }
        // anthropic: both — `a` forces the api-key path.
        select(&mut panel, PanelRow::ProviderChild("anthropic".to_string()));
        match panel.handle_key(ModalKey::Char('a')) {
            ModalOutcome::Emit(ModalAction::OpenApiKeyDialog(id)) => assert_eq!(id, "anthropic"),
            other => panic!("expected OpenApiKeyDialog, got {other:?}"),
        }
    }

    #[test]
    fn provider_x_logs_out_only_a_credentialed_row() {
        let mut panel = panel();
        expand(&mut panel, RowId::Providers);
        // openai-codex is credentialed → x emits Logout.
        select(
            &mut panel,
            PanelRow::ProviderChild("openai-codex".to_string()),
        );
        match panel.handle_key(ModalKey::Char('x')) {
            ModalOutcome::Emit(ModalAction::Logout(id)) => assert_eq!(id, "openai-codex"),
            other => panic!("expected Logout, got {other:?}"),
        }
        // anthropic is uncredentialed → x is a no-op.
        select(&mut panel, PanelRow::ProviderChild("anthropic".to_string()));
        assert_eq!(panel.handle_key(ModalKey::Char('x')), ModalOutcome::Ignore);
    }

    // --- criterion 19: per-tool switches emit grant/revoke, clamp ---
    #[test]
    fn permissions_tool_switches_emit_grant_and_revoke_and_clamp() {
        let mut panel = panel();
        expand(&mut panel, RowId::Permissions);
        select(&mut panel, PanelRow::PolicyTool("write".to_string()));
        // ask → always grants; against the stop is a no-op.
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::EditPolicy(ProjectPolicyEdit::GrantTool(
                "write".to_string()
            )))
        );
        assert_eq!(panel.handle_key(ModalKey::Right), ModalOutcome::Ignore);
        // A granted tool: left revokes.
        let mut snap = snapshot();
        snap.policy.granted_tools = vec!["write".to_string()];
        let mut panel = SettingsPanel::new(snap);
        expand(&mut panel, RowId::Permissions);
        select(&mut panel, PanelRow::PolicyTool("write".to_string()));
        assert_eq!(
            panel.handle_key(ModalKey::Left),
            ModalOutcome::Emit(ModalAction::EditPolicy(ProjectPolicyEdit::RevokeTool(
                "write".to_string()
            )))
        );
        // The track renders `ask · always`.
        let rendered = text(&panel.render_budgeted(120, 60));
        assert!(
            rendered.contains("ask") && rendered.contains("always"),
            "{rendered}"
        );
    }

    // --- criterion 20: bash revoke rows, empty state, sandbox ---
    #[test]
    fn permissions_bash_rows_revoke_and_empty_state_prints_a_quiet_row() {
        let mut snap = snapshot();
        snap.policy.bash_exact = vec!["cargo test".to_string()];
        snap.policy.bash_prefix = vec!["git ".to_string()];
        snap.policy.sandbox = Some("workspace-write".to_string());
        let mut panel = SettingsPanel::new(snap);
        expand(&mut panel, RowId::Permissions);
        select(
            &mut panel,
            PanelRow::PolicyBashExact("cargo test".to_string()),
        );
        assert_eq!(
            panel.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::EditPolicy(ProjectPolicyEdit::RevokeBashExact(
                "cargo test".to_string()
            )))
        );
        select(&mut panel, PanelRow::PolicyBashPrefix("git ".to_string()));
        assert_eq!(
            panel.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::EditPolicy(
                ProjectPolicyEdit::RevokeBashPrefix("git ".to_string())
            ))
        );
        // Empty state: the quiet row prints, not nothing.
        let mut empty = self::panel();
        expand(&mut empty, RowId::Permissions);
        let rendered = text(&empty.render_budgeted(120, 60));
        assert!(rendered.contains("no bash grants"), "{rendered}");
    }

    #[test]
    fn theme_is_a_live_rotary_over_every_theme_id() {
        let mut panel = panel();
        select_top(&mut panel, RowId::Field(Field::Theme));
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::Theme,
                value: Some(crate::ui::theme::available()[1].to_string()),
            })
        );
        let rendered = text(&panel.render_budgeted(80, 60));
        assert!(rendered.contains("gruvbox"), "{rendered}");
    }

    #[test]
    fn a_change_flashes_two_ticks_then_settles() {
        let mut panel = panel();
        select_top(&mut panel, RowId::Field(Field::DefaultApproval));
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
        select_top(&mut panel, RowId::Field(Field::DefaultApproval));
        panel.handle_key(ModalKey::Right);
        assert!(panel.flash.is_none(), "reduced motion settles instantly");
    }

    #[test]
    fn enabling_reduced_motion_clears_an_existing_flash() {
        let mut panel = panel();
        select_top(&mut panel, RowId::Field(Field::DefaultApproval));
        panel.handle_key(ModalKey::Right);
        assert!(panel.flash.is_some());

        panel.set_reduced_motion(true);

        assert!(panel.snap.reduced_motion);
        assert!(
            panel.flash.is_none(),
            "transition settles in the same frame"
        );
        assert!(!panel.tick(), "no trailing redraw remains");
    }

    #[test]
    fn watermark_goes_inert_while_microcompaction_is_off() {
        let panel = panel();
        assert!(!panel.snap.microcompaction);
        let line = panel.control_line(
            &PanelRow::Top(RowId::Field(Field::MicrocompactionWatermark)),
            false,
            80,
        );
        assert!(
            line.spans
                .iter()
                .all(|span| span.style.add_modifier.contains(Modifier::DIM)
                    || span.content.trim().is_empty()),
            "inert row renders fully dim: {line:?}"
        );
        let mut panel = self::panel();
        select_top(&mut panel, RowId::Field(Field::MicrocompactionWatermark));
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
        select_top(&mut panel, RowId::Field(Field::DefaultApproval));
        assert!(panel.footer().contains("\u{2190}\u{2192} set"));
        assert!(!panel.footer().contains("\u{21b5}"));
        select_top(&mut panel, RowId::Field(Field::ContextTokenBudget));
        assert!(panel.footer().contains("adjust"));
        assert!(panel.footer().contains("\u{21b5} type"));
        select_top(&mut panel, RowId::Field(Field::VerifyCommand));
        assert!(panel.footer().contains("\u{21b5} edit"));
        select_top(&mut panel, RowId::Model);
        assert!(panel.footer().contains("\u{21b5} open"));
        select_top(&mut panel, RowId::Field(Field::VerifyCommand));
        panel.handle_key(ModalKey::Enter);
        assert!(panel.footer().contains("\u{21b5} save"));
        assert!(panel.footer().contains("esc cancel"));
    }

    #[test]
    fn hatch_open_footer_says_collapse_not_close() {
        let mut panel = panel();
        expand(&mut panel, RowId::Scope);
        // On the header, on a child, and on a distant top row: all say collapse.
        assert!(panel.footer().contains("esc collapse"));
        panel.handle_key(ModalKey::Down);
        assert!(panel.footer().contains("esc collapse"));
        select_top(&mut panel, RowId::Field(Field::Theme));
        assert!(
            panel.footer().contains("esc collapse"),
            "{}",
            panel.footer()
        );
    }

    #[test]
    fn narrow_width_degrades_the_track_to_its_rotary_form() {
        let panel = panel();
        let wide = text(&[panel.control_line(&PanelRow::Top(RowId::Reasoning), false, 80)]);
        assert!(wide.contains("minimal") && wide.contains("xhigh"));
        let narrow = text(&[panel.control_line(&PanelRow::Top(RowId::Reasoning), false, 46)]);
        assert!(narrow.contains("medium"));
        assert!(!narrow.contains("minimal"), "rotary form: {narrow}");
    }

    #[test]
    fn ladder_stepping_is_mechanical() {
        assert_eq!(next_detent(&BUDGET_LADDER, 232_000, true), Some(300_000));
        assert_eq!(next_detent(&BUDGET_LADDER, 232_000, false), Some(200_000));
        assert_eq!(next_detent(&BUDGET_LADDER, 90_000, true), Some(96_000));
        assert_eq!(next_detent(&BUDGET_LADDER, 90_000, false), Some(64_000));
        assert_eq!(next_detent(&BUDGET_LADDER, 1_000_000, true), None);
        assert_eq!(next_detent(&BUDGET_LADDER, 64_000, false), None);
        assert_eq!(nearest_position(&BUDGET_LADDER, 232_000), 5);
        assert_eq!(nearest_position(&BUDGET_LADDER, 90_000), 1);
    }

    #[test]
    fn dial_values_print_in_the_one_house_token_format() {
        assert_eq!(compact_value(232_000), "232k");
        assert_eq!(compact_value(1_000_000), "1m");
        assert_eq!(compact_value(3), "3");
        assert_eq!(compact_value(12_500), "12.5k");
    }

    // --- entry cursors (§4.1) ---
    #[test]
    fn with_expanded_places_the_cursor_per_the_entry_table() {
        let model = SettingsPanel::with_expanded(snapshot(), HatchTarget::Model);
        assert_eq!(model.expanded, Some(RowId::Model));
        assert_eq!(
            model.cursor,
            PanelRow::ModelChild("openai-codex/gpt-5.5".to_string()),
            "model entry lands on the active model"
        );

        let scope = SettingsPanel::with_expanded(snapshot(), HatchTarget::Scope);
        assert_eq!(scope.expanded, Some(RowId::Scope));
        assert!(matches!(scope.cursor, PanelRow::ScopeChild(_)));

        let perms = SettingsPanel::with_expanded(snapshot(), HatchTarget::Permissions);
        assert_eq!(perms.expanded, Some(RowId::Permissions));
        assert_eq!(perms.cursor, PanelRow::PolicyTool("write".to_string()));

        // Login lands on the first uncredentialed provider (anthropic).
        let login = SettingsPanel::with_expanded(snapshot(), HatchTarget::Login);
        assert_eq!(
            login.cursor,
            PanelRow::ProviderChild("anthropic".to_string())
        );
        // Logout lands on the first credentialed provider (openai-codex).
        let logout = SettingsPanel::with_expanded(snapshot(), HatchTarget::Logout);
        assert_eq!(
            logout.cursor,
            PanelRow::ProviderChild("openai-codex".to_string())
        );
    }

    #[test]
    fn view_round_trips_cursor_and_expansion() {
        let mut panel = panel();
        expand(&mut panel, RowId::Providers);
        select(&mut panel, PanelRow::ProviderChild("openai".to_string()));
        let view = panel.view();
        // Rebuild on a fresh snapshot and restore.
        let mut rebuilt = SettingsPanel::new(snapshot());
        rebuilt.restore(view);
        assert_eq!(rebuilt.expanded, Some(RowId::Providers));
        assert_eq!(
            rebuilt.cursor,
            PanelRow::ProviderChild("openai".to_string())
        );
    }

    #[test]
    fn restore_lands_on_the_header_when_the_cursor_row_vanished() {
        let mut snap = snapshot();
        snap.policy.bash_exact = vec!["cargo test".to_string()];
        let mut panel = SettingsPanel::new(snap);
        expand(&mut panel, RowId::Permissions);
        select(
            &mut panel,
            PanelRow::PolicyBashExact("cargo test".to_string()),
        );
        let view = panel.view();
        // Revoking the last remaining grant empties the grant list; the quiet
        // `no bash grants` note is unselectable, so the cursor falls to the header.
        let mut rebuilt = SettingsPanel::new(snapshot());
        rebuilt.restore(view);
        assert_eq!(rebuilt.expanded, Some(RowId::Permissions));
        assert_eq!(rebuilt.cursor, PanelRow::Top(RowId::Permissions));
    }

    // --- finding 3: a revoked grant lands the cursor on the next grant ---
    #[test]
    fn restore_lands_on_the_next_grant_when_one_of_several_is_revoked() {
        let mut snap = snapshot();
        snap.policy.bash_exact = vec!["cargo test".to_string(), "npm run".to_string()];
        let mut panel = SettingsPanel::new(snap);
        expand(&mut panel, RowId::Permissions);
        select(
            &mut panel,
            PanelRow::PolicyBashExact("cargo test".to_string()),
        );
        let view = panel.view();
        // Revoking the first of two grants: the cursor lands on the (former)
        // second grant that took its slot, not the port header.
        let mut snap2 = snapshot();
        snap2.policy.bash_exact = vec!["npm run".to_string()];
        let mut rebuilt = SettingsPanel::new(snap2);
        rebuilt.restore(view);
        assert_eq!(rebuilt.expanded, Some(RowId::Permissions));
        assert_eq!(
            rebuilt.cursor,
            PanelRow::PolicyBashExact("npm run".to_string())
        );

        // Revoking the LAST of two grants: nothing took its slot (the next
        // selectable row sits outside the hatch), so the cursor falls back to
        // the previous grant.
        let mut snap3 = snapshot();
        snap3.policy.bash_exact = vec!["cargo test".to_string(), "npm run".to_string()];
        let mut panel = SettingsPanel::new(snap3);
        expand(&mut panel, RowId::Permissions);
        select(&mut panel, PanelRow::PolicyBashExact("npm run".to_string()));
        let view = panel.view();
        let mut snap4 = snapshot();
        snap4.policy.bash_exact = vec!["cargo test".to_string()];
        let mut rebuilt = SettingsPanel::new(snap4);
        rebuilt.restore(view);
        assert_eq!(
            rebuilt.cursor,
            PanelRow::PolicyBashExact("cargo test".to_string())
        );
    }

    #[test]
    fn an_off_vocabulary_value_sits_between_detents_and_prints_raw() {
        let mut snap = snapshot();
        snap.default_approval = "on-request".to_string();
        let panel = SettingsPanel::new(snap);
        let line = panel.control_line(
            &PanelRow::Top(RowId::Field(Field::DefaultApproval)),
            false,
            80,
        );
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
    }

    #[test]
    fn an_off_vocabulary_value_snaps_into_the_scale_on_first_click() {
        let mut snap = snapshot();
        snap.default_approval = "on-request".to_string();
        let mut panel = SettingsPanel::new(snap);
        select_top(&mut panel, RowId::Field(Field::DefaultApproval));
        assert_eq!(
            panel.handle_key(ModalKey::Right),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::DefaultApproval,
                value: Some("strict".to_string()),
            })
        );
        let mut snap = snapshot();
        snap.default_approval = "on-request".to_string();
        let mut panel = SettingsPanel::new(snap);
        select_top(&mut panel, RowId::Field(Field::DefaultApproval));
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
        select_top(&mut panel, RowId::Field(Field::VerifyCommand));
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

    // --- adversarial: empty states ---
    #[test]
    fn empty_states_do_not_panic_and_render_quiet_rows() {
        let mut snap = snapshot();
        snap.catalog.clear();
        snap.scope_candidates.clear();
        snap.providers.clear();
        snap.scope_enabled = Some(Vec::new());
        let mut panel = SettingsPanel::new(snap);
        for port in [
            RowId::Model,
            RowId::Scope,
            RowId::Providers,
            RowId::Permissions,
        ] {
            expand(&mut panel, port);
            // Rendering never panics on zero children / a narrow width.
            let _ = panel.render_budgeted(24, 12);
            let _ = panel.render_budgeted(200, 60);
        }
        // Navigation over an empty hatch does not crash and lands on a real
        // selectable row (the next control, since the hatch has no children).
        expand(&mut panel, RowId::Model);
        panel.handle_key(ModalKey::Down);
        assert!(panel.selectable().contains(&panel.cursor));
        assert_eq!(panel.cursor, PanelRow::Top(RowId::Reasoning));
    }

    // --- finding 1: a ghost persisted scope must not inflate the header count ---
    #[test]
    fn a_stale_scope_id_outside_the_candidate_set_does_not_inflate_the_count() {
        let mut snap = snapshot();
        // One candidate, but the live scope also carries a ghost id no longer in
        // the catalog. The header must read `1 of 1 enabled`, not `2 of 1`; the
        // child list already drops the ghost.
        snap.scope_candidates = vec![scope_choice(ProviderId::OpenAiCodex, "gpt-5.5")];
        snap.scope_enabled = Some(vec![
            "anthropic/claude-ghost".to_string(),
            "openai-codex/gpt-5.5".to_string(),
        ]);
        let panel = SettingsPanel::new(snap);
        let rendered = text(&panel.render_budgeted(100, 60));
        assert!(rendered.contains("1 of 1 enabled"), "{rendered}");
        assert!(!rendered.contains("2 of 1"), "{rendered}");
        assert_eq!(
            panel.scope_children().len(),
            1,
            "child list drops the ghost"
        );
    }

    // --- finding 2: the scope filter is visible (echo line + footer verbs) ---
    #[test]
    fn the_scope_filter_echoes_the_query_and_caret_under_the_header() {
        let mut panel = panel();
        expand(&mut panel, RowId::Scope);
        for c in "gem".chars() {
            panel.handle_key(ModalKey::Char(c));
        }
        let rendered = text(&panel.render_budgeted(100, 60));
        assert!(rendered.contains("filter"), "filter label:\n{rendered}");
        assert!(rendered.contains("gem"), "query echoed:\n{rendered}");
        assert!(
            rendered.contains(crate::ui::symbols::CARET),
            "caret:\n{rendered}"
        );
    }

    #[test]
    fn the_scope_footer_names_type_to_filter_and_esc_clear_by_state() {
        let mut panel = panel();
        expand(&mut panel, RowId::Scope);
        panel.handle_key(ModalKey::Down); // onto a scope child
        assert!(
            panel.footer().contains("type to filter"),
            "verb present when idle: {}",
            panel.footer()
        );
        assert!(
            !panel.footer().contains("esc clear"),
            "no clear verb when the filter is empty: {}",
            panel.footer()
        );
        for c in "gem".chars() {
            panel.handle_key(ModalKey::Char(c));
        }
        assert!(
            panel.footer().contains("type to filter"),
            "{}",
            panel.footer()
        );
        assert!(
            panel.footer().contains("esc clear"),
            "clear verb while a filter is active: {}",
            panel.footer()
        );
    }

    #[test]
    fn esc_clears_the_scope_filter_and_the_echo_disappears() {
        let mut panel = panel();
        expand(&mut panel, RowId::Scope);
        for c in "gem".chars() {
            panel.handle_key(ModalKey::Char(c));
        }
        assert!(
            text(&panel.render_budgeted(100, 60)).contains(crate::ui::symbols::CARET),
            "echo visible while filtering"
        );
        // First esc clears the filter (does not collapse) and the echo vanishes.
        assert_eq!(panel.handle_key(ModalKey::Esc), ModalOutcome::Redraw);
        assert!(panel.scope_filter.is_empty());
        assert_eq!(panel.expanded, Some(RowId::Scope), "still open");
        let rendered = text(&panel.render_budgeted(100, 60));
        assert!(
            !rendered.contains(crate::ui::symbols::CARET),
            "echo gone after clear:\n{rendered}"
        );
    }

    #[test]
    fn a_bracketed_paste_into_the_scope_hatch_lands_in_the_filter() {
        let mut panel = panel();
        expand(&mut panel, RowId::Scope);
        panel.push_str("gemini");
        assert_eq!(panel.scope_filter, "gemini");
        assert_eq!(
            panel.scope_children().len(),
            1,
            "the paste narrowed the list"
        );
    }

    // --- finding 4: empty-catalog hatches print quiet notes, never nothing ---
    #[test]
    fn an_empty_model_catalog_prints_the_connect_note() {
        let mut snap = snapshot();
        snap.catalog.clear();
        let mut panel = SettingsPanel::new(snap);
        expand(&mut panel, RowId::Model);
        let rendered = text(&panel.render_budgeted(100, 60));
        assert!(
            rendered.contains("no models \u{2014} connect a provider"),
            "{rendered}"
        );
        // The note is silkscreen: nothing selectable joined the hatch.
        assert!(
            panel
                .selectable()
                .iter()
                .all(|row| matches!(row, PanelRow::Top(_))),
            "note is unselectable"
        );
    }

    #[test]
    fn zero_known_providers_prints_the_no_providers_note() {
        let mut snap = snapshot();
        snap.providers.clear();
        let mut panel = SettingsPanel::new(snap);
        expand(&mut panel, RowId::Providers);
        let rendered = text(&panel.render_budgeted(100, 60));
        assert!(rendered.contains("no providers"), "{rendered}");
        assert!(
            panel
                .selectable()
                .iter()
                .all(|row| matches!(row, PanelRow::Top(_))),
            "note is unselectable"
        );
    }

    // --- finding 5: \r\n is one line break, not two spaces ---
    #[test]
    fn a_crlf_paste_collapses_to_one_space() {
        let mut panel = panel();
        select_top(&mut panel, RowId::Field(Field::VerifyCommand));
        panel.handle_key(ModalKey::Enter);
        panel.push_str("a\r\nb");
        assert_eq!(
            panel.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::VerifyCommand,
                value: Some("a b".to_string()),
            })
        );
    }

    // --- finding 6: the caution silkscreen drops whole fields, never mid-word ---
    #[test]
    fn the_caution_silkscreen_drops_whole_fields_never_mid_word() {
        let panel = panel();
        let row = PanelRow::Top(RowId::SkipApprovals);
        // Wide: the whole caution prints.
        let wide = text(&[panel.control_line(&row, false, 80)]);
        assert!(
            wide.contains("dangerous") && wide.contains("saved default"),
            "{wide}"
        );
        // Mid: ` ┊ saved default` drops first, whole.
        let mid = text(&[panel.control_line(&row, false, 46)]);
        assert!(mid.contains("dangerous"), "{mid}");
        assert!(!mid.contains("saved"), "{mid}");
        // Narrow (the offending ~40 cols): `dangerous` drops too — never `danger`.
        let narrow = text(&[panel.control_line(&row, false, 40)]);
        assert!(!narrow.contains("dangerous"), "{narrow}");
        assert!(!narrow.contains("danger"), "{narrow}");
        // Whole-field honesty: the row always fits the width it was given, so
        // the overlay's hard truncation never gets to clip a word.
        for avail in [40usize, 42, 46, 57, 80] {
            let line = panel.control_line(&row, false, avail);
            assert!(line_width(&line) <= avail, "fits at {avail}");
        }
    }
}
