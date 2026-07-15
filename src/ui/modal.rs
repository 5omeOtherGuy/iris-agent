//! Picker/dialog state machine (Tier 3, presentation-only).
//!
//! Every `/model`, `/scoped-models`, `/settings`, `/login`, and `/logout`
//! surface is a [`Modal`] rendered above the editor. A modal owns its
//! [`Selector`] (or, for the OAuth dialog, just display lines), turns key
//! events into a [`ModalOutcome`], and renders itself into ratatui `Line`s. It
//! performs no side effects: confirming a row returns a [`ModalAction`] the event
//! loop applies at the safe inter-turn boundary (model/effort switch, settings
//! save, login/logout), so a picker can never switch a provider mid-stream.
//!
//! The data a modal needs (available models, auth status, provider list) is
//! gathered by the loop/cli layer and passed in at construction, keeping disk and
//! auth lookups out of per-keystroke handling and out of this presentation code.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::mimir::selection::ReasoningEffort;
use crate::ui::selector::{Selector, SelectorItem};
use crate::wayland::trust::ProjectPolicyEdit;

/// Max rows shown in above-editor menus before windowing. Keep enough room for
/// footer controls plus the editor/status chrome on a compact terminal.
const MODEL_ROWS: usize = 5;
const SKILL_ROWS: usize = 7;

/// A key the loop forwards to the active modal. A neutral subset of crossterm
/// keys so modal handling is unit-testable without constructing terminal events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalKey {
    Up,
    Down,
    Left,
    Right,
    Tab,
    BackTab,
    Enter,
    Esc,
    CtrlC,
    CtrlA,
    CtrlX,
    CtrlP,
    CtrlS,
    AltUp,
    AltDown,
    Backspace,
    Char(char),
}

/// A side effect or navigation step the loop performs after a modal key. The
/// loop owns the harness, model-switch state, settings, and auth store; the
/// modal only names the intent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModalAction {
    /// Switch to this `provider/model` id with the chosen reasoning effort. The
    /// model picker emits this on Enter (`save_default: true`, persist as the
    /// default) or `s` (`save_default: false`, this session only).
    SelectModel {
        id: String,
        effort: ReasoningEffort,
        save_default: bool,
    },
    ConfirmModelSwitch {
        id: String,
        effort: ReasoningEffort,
        save_default: bool,
        compact_first: bool,
    },
    /// Apply this scope to the live session immediately (every scoped edit).
    /// `None` clears the scope (cycle all authenticated models).
    ApplyScoped(Option<Vec<String>>),
    /// Persist the scope to settings (Ctrl+S); the picker stays open.
    SaveScoped(Option<Vec<String>>),
    /// Settings panel: the reasoning switch clicked to a new detent. Applies to
    /// the live session AND persists as the default; the panel stays open.
    AdjustEffort(ReasoningEffort),
    /// Settings panel: the model row clicked ←/→ — cycle through the scoped
    /// models exactly like Ctrl+P. The loop rebuilds the panel on the new
    /// model (the catalog lives beyond the panel's snapshot).
    CycleModel {
        forward: bool,
    },
    /// Persist a settings field to the user-global file (`None` clears the key).
    /// The loop maps the field to its `config::save_*`; the panel stays open
    /// and keeps its own display state (it already clicked the detent).
    SaveSetting {
        field: crate::ui::settings_menu::Field,
        value: Option<String>,
    },
    /// Settings -> toggle the dangerous approval-gate bypass. Persisted as the
    /// default permission mode (#520) via `cli::apply_permission_mode`, applied
    /// live at the same time; the faceplate skip-approvals row is the trigger.
    ToggleSkipPermissions,
    /// Accept or decline native jj integration for the active canonical workspace.
    SetNativeJj(bool),
    /// Edit this project's persistent permission policy (ADR-0027). The loop
    /// persists the edit to the HOME-owned store and refreshes the live agent's
    /// in-memory policy at the safe inter-turn boundary.
    EditPolicy(ProjectPolicyEdit),
    /// Resume the persisted session with this id, swapping the live session at
    /// the safe inter-turn boundary (reloads messages, session log, and harness
    /// state). Emitted by the `/resume` picker on Enter.
    ResumeSession(String),
    /// Adopt the recoverable task with this id at the safe inter-turn boundary
    /// (#288, ADR-0031): rehydrate its checkpoint chain so settlement operates on
    /// the real chain. Never implicitly resumes a session. Emitted by the
    /// `/tasks` picker on Enter.
    AdoptTask(String),
    /// Show the deterministic linked-session detail for this task id in the
    /// unified task modal (ADR-0031 session lookup). The loop fetches the
    /// bounded, cwd-scoped extraction and re-opens the modal in its detail view.
    /// Display-only audit; never affects adoption, recovery, or enforcement.
    ViewTaskSessions(String),
    /// Active `/tasks` card: accept Iris's current task changes.
    AcceptTask,
    /// Active `/tasks` card: render Iris's current task diff.
    ShowTaskDiff,
    /// Active `/tasks` card: list rollback points.
    ListTaskRollback,
    /// Begin an OAuth/subscription login for this provider (providers hatch).
    BeginLogin(crate::mimir::selection::ProviderId),
    /// Open an API-key entry dialog for this provider id (providers hatch).
    OpenApiKeyDialog(String),
    /// Store the API key currently buffered in the active API-key dialog.
    SaveApiKey(String),
    /// Remove the stored credential for this provider id (providers hatch).
    Logout(String),
    /// Insert one path-qualified skill mention into the composer. The path
    /// disambiguates duplicate skill names exactly as Codex's structured
    /// selector does.
    InsertSkillMention {
        name: String,
        path: String,
    },
    /// Confirm replacing an unfinished saved goal.
    ReplaceGoal(String),
    /// Save an edited objective while preserving the current goal id and usage.
    EditGoal(String),
    ResolveUserQuestion(crate::nexus::InteractionOutcome),
}

/// What the loop does after a modal handled a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModalOutcome {
    /// Event ignored; no redraw.
    Ignore,
    /// Internal state changed; redraw the modal.
    Redraw,
    /// Cancel/dismiss the modal and restore the editor.
    Close,
    /// Perform this action (the loop decides whether the modal then closes).
    Emit(ModalAction),
}

/// The active picker/dialog. The four settings sub-surfaces (model picker,
/// scope, providers, permissions) are no longer modals of their own: they are
/// hatches that expand inside [`Modal::Settings`] (§10.1). The dialog-guards
/// (`SwitchContext`, `LoginDialog`, `ApiKeyDialog`) still overlay it.
#[derive(Debug, Clone)]
pub(crate) enum Modal {
    JjSetup(JjSetupPrompt),
    GoalReplace(GoalReplacePrompt),
    GoalEdit(GoalEditDialog),
    SwitchContext(SwitchContextPrompt),
    // Boxed (like Tasks) so the panel's snapshot does not dominate the enum.
    Settings(Box<crate::ui::settings_menu::SettingsPanel>),
    Session(SessionPicker),
    Tasks(TaskPicker),
    /// Codex-compatible native skills picker (#521): a composer affordance that
    /// inserts a path-qualified `skill://` mention. Not a settings surface.
    Skills(SkillPicker),
    AskUserQuestion(crate::ui::ask_user_question::AskUserDialog),
    LoginDialog(LoginDialog),
    ApiKeyDialog(ApiKeyDialog),
}

impl Modal {
    pub(crate) fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match self {
            Modal::JjSetup(prompt) => prompt.handle_key(key),
            Modal::GoalReplace(prompt) => prompt.handle_key(key),
            Modal::GoalEdit(dialog) => dialog.handle_key(key),
            Modal::SwitchContext(prompt) => prompt.handle_key(key),
            Modal::Settings(panel) => panel.handle_key(key),
            Modal::Session(picker) => picker.handle_key(key),
            Modal::Tasks(picker) => picker.handle_key(key),
            Modal::Skills(picker) => picker.handle_key(key),
            Modal::AskUserQuestion(dialog) => match dialog.handle_key(key) {
                Some(outcome) => {
                    ModalOutcome::Emit(ModalAction::ResolveUserQuestion(outcome.into()))
                }
                None => ModalOutcome::Redraw,
            },
            Modal::LoginDialog(dialog) => dialog.handle_key(key),
            Modal::ApiKeyDialog(dialog) => dialog.handle_key(key),
        }
    }

    pub(crate) fn paste_text(&mut self, text: &str) -> ModalOutcome {
        match self {
            Modal::JjSetup(_) | Modal::GoalReplace(_) => ModalOutcome::Ignore,
            Modal::GoalEdit(dialog) => {
                dialog.push_str(text);
                ModalOutcome::Redraw
            }
            Modal::LoginDialog(dialog) if dialog.accepts_manual_input() => {
                dialog.push_str(text);
                ModalOutcome::Redraw
            }
            Modal::ApiKeyDialog(dialog) => {
                dialog.push_str(text);
                ModalOutcome::Redraw
            }
            Modal::Settings(panel) => {
                panel.push_str(text);
                ModalOutcome::Redraw
            }
            Modal::AskUserQuestion(dialog) => {
                dialog.paste(text);
                ModalOutcome::Redraw
            }
            _ => ModalOutcome::Ignore,
        }
    }

    /// Advance modal-owned animation one loop tick (today: the settings
    /// panel's detent flash). Returns true while something is still settling,
    /// so the loop keeps repainting on the tick grid until it does.
    pub(crate) fn tick(&mut self) -> bool {
        match self {
            Modal::Settings(panel) => panel.tick(),
            _ => false,
        }
    }

    /// Apply the live motion posture to modal-owned state. Today only the
    /// settings faceplate owns reactive motion.
    pub(crate) fn set_reduced_motion(&mut self, reduced_motion: bool) {
        if let Modal::Settings(panel) = self {
            panel.set_reduced_motion(reduced_motion);
        }
    }

    /// Render with an explicit line budget (the docked-menu region's height).
    /// Only the settings panel windows itself to the viewport; every other
    /// modal is already bounded by its own row cap and ignores the budget.
    pub(crate) fn render_budgeted(&self, width: usize, budget: usize) -> Vec<Line<'static>> {
        match self {
            Modal::Settings(panel) => panel.render_budgeted(width, budget),
            _ => Modal::render(self, u16::try_from(width).unwrap_or(u16::MAX)),
        }
    }

    pub(crate) fn render(&self, width: u16) -> Vec<Line<'static>> {
        match self {
            Modal::JjSetup(prompt) => prompt.render(width),
            Modal::GoalReplace(prompt) => prompt.render(width),
            Modal::GoalEdit(dialog) => dialog.render(width),
            Modal::SwitchContext(prompt) => prompt.render(width),
            Modal::Settings(panel) => panel.render(width),
            Modal::Session(picker) => picker.render(width),
            Modal::Tasks(picker) => picker.render(width),
            Modal::Skills(picker) => picker.render(width),
            Modal::AskUserQuestion(dialog) => dialog.render(width),
            Modal::LoginDialog(dialog) => dialog.render(width),
            Modal::ApiKeyDialog(dialog) => dialog.render(width),
        }
    }
}

// --- skills picker ---

#[derive(Debug, Clone)]
pub(crate) struct SkillPicker {
    selector: Selector,
}

impl SkillPicker {
    pub(crate) fn new(skills: &[crate::wayland::skills::SkillMetadata]) -> Self {
        let items = skills
            .iter()
            .map(|skill| {
                let scope = match skill.scope {
                    crate::wayland::skills::SkillScope::Repo => "repo",
                    crate::wayland::skills::SkillScope::User => "user",
                    crate::wayland::skills::SkillScope::System => "system",
                    crate::wayland::skills::SkillScope::Admin => "admin",
                };
                SelectorItem::new(
                    skill.path.display().to_string(),
                    skill.display_name().to_string(),
                )
                .detail(skill.display_description().to_string())
                .trailing(scope)
            })
            .collect();
        Self {
            selector: Selector::new(items, true, true, SKILL_ROWS),
        }
    }

    fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::Up => {
                self.selector.up();
                ModalOutcome::Redraw
            }
            ModalKey::Down => {
                self.selector.down();
                ModalOutcome::Redraw
            }
            ModalKey::Char(character) => {
                self.selector.push_char(character);
                ModalOutcome::Redraw
            }
            ModalKey::Backspace => {
                self.selector.backspace();
                ModalOutcome::Redraw
            }
            ModalKey::CtrlC if self.selector.clear_search() => ModalOutcome::Redraw,
            ModalKey::Enter => match self.selector.selected() {
                Some(skill) => ModalOutcome::Emit(ModalAction::InsertSkillMention {
                    name: skill.label.clone(),
                    path: skill.id.clone(),
                }),
                None => ModalOutcome::Ignore,
            },
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        crate::ui::tui::overlay_menu(
            Some("Select skill"),
            selector_rows(&self.selector, "No matching skills"),
            Some("type to filter · ↑↓ move · ↵ mention · esc cancel"),
            usize::from(width),
        )
    }
}

/// The modal is a docked overlay [`Component`]: the composer chrome composites
/// it through the same render contract as every other surface (see
/// `ui::tui::overlay`). Width arrives as `usize` from the component path and is
/// clamped back to the `u16` the picker renderers use.
impl crate::ui::tui::Component for Modal {
    fn render(&self, width: usize) -> Vec<Line<'static>> {
        Modal::render(self, u16::try_from(width).unwrap_or(u16::MAX))
    }
}

// --- shared rendering helpers ---

pub(crate) fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// Render the shared search line + windowed rows for a [`Selector`] as overlay
/// rows: `(line, selected)` pairs for [`overlay_menu`], which gives the selected
/// row the surface fill (never a colored accent). The selected label is bold;
/// metadata stays muted. `empty` is the no-match message.
pub(crate) fn selector_rows(selector: &Selector, empty: &str) -> Vec<(Line<'static>, bool)> {
    let mut out: Vec<(Line<'static>, bool)> = Vec::new();
    if selector.searchable() {
        let search = selector.search().unwrap_or("");
        out.push((
            Line::from(vec![
                Span::styled("> ", dim()),
                Span::raw(search.to_string()),
            ]),
            false,
        ));
    }
    if selector.is_empty() {
        out.push((Line::from(Span::styled(empty.to_string(), dim())), false));
        return out;
    }
    for row in selector.visible() {
        let mut spans = Vec::new();
        let base = if row.selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        spans.push(Span::styled(row.item.label.clone(), base));
        if let Some(detail) = &row.item.detail {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(detail.clone(), dim()));
        }
        if let Some(trailing) = &row.item.trailing {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(trailing.clone(), dim()));
        }
        out.push((Line::from(spans), row.selected));
    }
    if selector.is_scrolled() {
        out.push((
            Line::from(Span::styled(selector.position_label(), dim())),
            false,
        ));
    }
    out
}

#[derive(Debug, Clone)]
pub(crate) struct GoalReplacePrompt {
    objective: String,
    selector: Selector,
}

impl GoalReplacePrompt {
    pub(crate) fn new(objective: String) -> Self {
        Self {
            objective,
            selector: Selector::new(
                vec![
                    SelectorItem::new("cancel", "Keep the current goal"),
                    SelectorItem::new("replace", "Replace the current goal")
                        .detail("resets goal usage and elapsed-time accounting"),
                ],
                false,
                true,
                2,
            ),
        }
    }

    fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::Up => {
                self.selector.up();
                ModalOutcome::Redraw
            }
            ModalKey::Down => {
                self.selector.down();
                ModalOutcome::Redraw
            }
            ModalKey::Enter => match self.selector.selected_id() {
                Some("replace") => {
                    ModalOutcome::Emit(ModalAction::ReplaceGoal(self.objective.clone()))
                }
                Some("cancel") => ModalOutcome::Close,
                _ => ModalOutcome::Ignore,
            },
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let preview: String = self.objective.chars().take(120).collect();
        let mut rows = vec![(
            Line::from(Span::styled(format!("New objective: {preview}"), dim())),
            false,
        )];
        rows.extend(selector_rows(&self.selector, "No choices"));
        crate::ui::tui::overlay_menu(
            Some("Replace unfinished goal?"),
            rows,
            Some("↑↓ move · ↵ choose · esc cancel"),
            usize::from(width),
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GoalEditDialog {
    objective: String,
}

impl GoalEditDialog {
    pub(crate) fn new(objective: String) -> Self {
        Self { objective }
    }

    fn push_str(&mut self, text: &str) {
        self.objective.push_str(text);
    }

    fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::Char(character) => {
                self.objective.push(character);
                ModalOutcome::Redraw
            }
            ModalKey::Backspace => {
                self.objective.pop();
                ModalOutcome::Redraw
            }
            ModalKey::Enter if !self.objective.trim().is_empty() => {
                ModalOutcome::Emit(ModalAction::EditGoal(self.objective.clone()))
            }
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        crate::ui::tui::overlay_menu(
            Some("Edit goal objective"),
            vec![(Line::from(self.objective.clone()), true)],
            Some("type to edit · ↵ save · esc cancel"),
            usize::from(width),
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) struct JjSetupPrompt {
    selector: Selector,
}

impl JjSetupPrompt {
    pub(crate) fn new() -> Self {
        Self {
            selector: Selector::new(
                vec![
                    SelectorItem::new("enable", "Enable native jj integration")
                        .detail("track jj operations and enable jj-aware recovery"),
                    SelectorItem::new("decline", "Keep native jj integration off")
                        .detail("use reduced file-only mutation guarantees"),
                ],
                false,
                true,
                2,
            ),
        }
    }

    fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::Up => {
                self.selector.up();
                ModalOutcome::Redraw
            }
            ModalKey::Down => {
                self.selector.down();
                ModalOutcome::Redraw
            }
            ModalKey::Enter => match self.selector.selected_id() {
                Some("enable") => ModalOutcome::Emit(ModalAction::SetNativeJj(true)),
                Some("decline") => ModalOutcome::Emit(ModalAction::SetNativeJj(false)),
                _ => ModalOutcome::Ignore,
            },
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Emit(ModalAction::SetNativeJj(false)),
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let mut rows = vec![
            (
                Line::from(Span::styled("Iris found a compatible jj workspace.", dim())),
                false,
            ),
            (
                Line::from(Span::styled(
                    "Native mode runs jj snapshots and tracks operation boundaries.",
                    dim(),
                )),
                false,
            ),
            (
                Line::from(Span::styled(
                    "It halts after external operations and enables jj-aware",
                    dim(),
                )),
                false,
            ),
            (
                Line::from(Span::styled(
                    "rollback/restoration. Off uses reduced file-only protection.",
                    dim(),
                )),
                false,
            ),
        ];
        rows.extend(selector_rows(&self.selector, "No choices"));
        crate::ui::tui::overlay_menu(
            Some("Enable native jj integration?"),
            rows,
            Some("↑↓ move · ↵ choose · esc keep off"),
            usize::from(width),
        )
    }
}

pub(crate) fn jj_setup() -> Modal {
    Modal::JjSetup(JjSetupPrompt::new())
}

#[derive(Debug, Clone)]
pub(crate) struct SwitchContextPrompt {
    id: String,
    effort: ReasoningEffort,
    save_default: bool,
    model: String,
    context_tokens: u64,
    selector: Selector,
}

impl SwitchContextPrompt {
    pub(crate) fn new(
        id: String,
        effort: ReasoningEffort,
        save_default: bool,
        model: String,
        context_tokens: u64,
    ) -> Self {
        let items = vec![
            SelectorItem::new("summary", "Compact first, then switch")
                .detail("send the new model a shorter handoff summary"),
            SelectorItem::new("full", "Switch with full context")
                .detail("re-read the current context on the new model"),
            SelectorItem::new("cancel", "Cancel switch"),
        ];
        Self {
            id,
            effort,
            save_default,
            model,
            context_tokens,
            selector: Selector::new(items, false, true, 3),
        }
    }

    fn emit(&self, compact_first: bool) -> ModalOutcome {
        ModalOutcome::Emit(ModalAction::ConfirmModelSwitch {
            id: self.id.clone(),
            effort: self.effort,
            save_default: self.save_default,
            compact_first,
        })
    }

    fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::Up => {
                self.selector.up();
                ModalOutcome::Redraw
            }
            ModalKey::Down => {
                self.selector.down();
                ModalOutcome::Redraw
            }
            ModalKey::Enter => match self.selector.selected_id() {
                Some("summary") => self.emit(true),
                Some("full") => self.emit(false),
                Some("cancel") => ModalOutcome::Close,
                _ => ModalOutcome::Ignore,
            },
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let rows = {
            let mut rows = vec![
                (
                    Line::from(Span::styled(
                        format!(
                            "Switching to {} will carry ~{} context tokens.",
                            self.model, self.context_tokens
                        ),
                        dim(),
                    )),
                    false,
                ),
                (
                    Line::from(Span::styled(
                        "Choose whether to summarize before switching.",
                        dim(),
                    )),
                    false,
                ),
            ];
            rows.extend(selector_rows(&self.selector, "No choices"));
            rows
        };
        crate::ui::tui::overlay_menu(
            Some("Large context switch"),
            rows,
            Some("↑↓ move · ↵ choose · esc cancel"),
            usize::from(width),
        )
    }
}

// --- session resume picker ---

/// One row for the `/resume` picker: the session id (stable selector key), a
/// one-line first-user-message preview, and a human-relative age. Built by the
/// orchestration layer from persisted session metadata so this presentation
/// code stays disk-free and unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionRow {
    pub(crate) id: String,
    pub(crate) preview: String,
    pub(crate) age: String,
    pub(crate) task_linked: bool,
}

/// Searchable list of resumable sessions for the current workspace. Confirming a
/// row emits [`ModalAction::ResumeSession`]; the loop swaps the live session at
/// the safe inter-turn boundary. Newest-first order is preserved from the input
/// rows.
#[derive(Debug, Clone)]
pub(crate) struct SessionPicker {
    selector: Selector,
}

impl SessionPicker {
    pub(crate) fn new(rows: Vec<SessionRow>) -> Self {
        let items: Vec<SelectorItem> = rows
            .into_iter()
            .map(|row| {
                let label = if row.preview.is_empty() {
                    "(no messages yet)".to_string()
                } else {
                    row.preview
                };
                let item = SelectorItem::new(row.id, label).detail(row.age);
                if row.task_linked {
                    item.trailing("\u{25c7}")
                } else {
                    item
                }
            })
            .collect();
        SessionPicker {
            selector: Selector::new(items, true, false, MODEL_ROWS),
        }
    }

    fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::Up => {
                self.selector.up();
                ModalOutcome::Redraw
            }
            ModalKey::Down => {
                self.selector.down();
                ModalOutcome::Redraw
            }
            ModalKey::Enter => match self.selector.selected_id() {
                Some(id) => ModalOutcome::Emit(ModalAction::ResumeSession(id.to_string())),
                None => ModalOutcome::Ignore,
            },
            ModalKey::Esc => ModalOutcome::Close,
            ModalKey::CtrlC => {
                if self.selector.clear_search() {
                    ModalOutcome::Redraw
                } else {
                    ModalOutcome::Close
                }
            }
            ModalKey::Backspace => {
                if self.selector.backspace() {
                    ModalOutcome::Redraw
                } else {
                    ModalOutcome::Ignore
                }
            }
            ModalKey::Char(c) => {
                self.selector.push_char(c);
                ModalOutcome::Redraw
            }
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let rows = selector_rows(&self.selector, "No sessions to resume");
        crate::ui::tui::overlay_menu(
            Some("Resume session"),
            rows,
            Some("↑↓ move · type to filter · ↵ resume · esc cancel"),
            usize::from(width),
        )
    }
}

// --- unified task modal (/tasks) ---

use crate::ui::task_view::{TaskCard, TaskKind};

/// The linked-session detail sub-view of the task modal (ADR-0031 session
/// lookup): bounded, cwd-scoped display lines for one task, fetched by the loop
/// (the deterministic `sessions_for_task`/`extract_session` path) and shown in
/// place of the list. Display-only audit; never an adoption/recovery input.
#[derive(Debug, Clone)]
struct TaskDetail {
    task_short: String,
    lines: Vec<String>,
}

/// The unified task surface (`/tasks`, ADR-0031): an optional non-selectable
/// header for the ACTIVE (live, unsettled) task, then the recoverable/legacy
/// tasks. Enter adopts a selected recoverable task only when no active task is
/// present; with an active task, recovery rows are shown as non-selectable
/// blocked rows. The right arrow requests the target task's linked-session
/// detail ([`ModalAction::ViewTaskSessions`]); left/esc leaves the detail view.
/// The active task is shown but never adoptable; settlement stays on the
/// existing `/git` / `/accept` / `/rollback` / `/diff` paths (not duplicated
/// here).
/// Built disk-free by the orchestration layer; input order is preserved.
#[derive(Debug, Clone)]
pub(crate) struct TaskPicker {
    // Boxed so the (rarely constructed) task modal does not dominate the `Modal`
    // enum's size; `has_recoverable` tracks selectable recovery rows.
    active: Option<Box<TaskCard>>,
    has_recoverable: bool,
    blocked_recoverable: Vec<TaskCard>,
    selector: Selector,
    detail: Option<Box<TaskDetail>>,
}

impl TaskPicker {
    /// Build the unified modal from the active card (if any) and the recoverable
    /// cards. The startup/session-swap recovery path passes `active: None`.
    pub(crate) fn new(active: Option<TaskCard>, recoverable: Vec<TaskCard>) -> Self {
        let adoption_blocked = active.is_some();
        let items: Vec<SelectorItem> = if adoption_blocked {
            Vec::new()
        } else {
            recoverable
                .iter()
                // Defensive: only adoptable (recoverable/legacy) cards become
                // selectable rows, so an active task handed in by mistake can
                // never be adopted (ADR-0031: active is shown, never adopted).
                .filter(|card| card.is_adoptable())
                .map(|card| {
                    let label = if matches!(card.kind, TaskKind::Legacy) {
                        format!("{}  (legacy)", card.body_preview())
                    } else {
                        card.body_preview()
                    };
                    SelectorItem::new(card.task_id.clone(), label)
                        .detail(card.age_label())
                        .trailing(card.session_summary())
                })
                .collect()
        };
        let has_recoverable = !items.is_empty();
        TaskPicker {
            active: active.map(Box::new),
            has_recoverable,
            blocked_recoverable: if adoption_blocked {
                recoverable
            } else {
                Vec::new()
            },
            selector: Selector::new(items, true, false, MODEL_ROWS),
            detail: None,
        }
    }

    /// The task whose linked-session detail the right arrow targets: the selected
    /// recoverable row if any, otherwise the active task (so an active-only
    /// modal can still show its sessions). `None` when neither exists.
    fn detail_target(&self) -> Option<String> {
        self.selector
            .selected_id()
            .map(str::to_string)
            .or_else(|| self.active.as_ref().map(|card| card.task_id.clone()))
    }

    /// Enter the linked-session detail view with the loop-fetched bounded lines.
    pub(crate) fn show_detail(&mut self, task_id: &str, lines: Vec<String>) {
        self.detail = Some(Box::new(TaskDetail {
            task_short: task_id.chars().take(8).collect(),
            lines,
        }));
    }

    fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        // Detail view: read-only. Left/backspace step back to the list; esc
        // dismisses the whole modal (the modal-wide esc convention), matching
        // the detail footer's "back / close" hints.
        if self.detail.is_some() {
            return match key {
                ModalKey::Left | ModalKey::Backspace => {
                    self.detail = None;
                    ModalOutcome::Redraw
                }
                ModalKey::Esc => ModalOutcome::Close,
                _ => ModalOutcome::Ignore,
            };
        }
        match key {
            ModalKey::Up => {
                self.selector.up();
                ModalOutcome::Redraw
            }
            ModalKey::Down => {
                self.selector.down();
                ModalOutcome::Redraw
            }
            // Adopt the selected recoverable task; an active-only modal has no
            // selectable row, so Enter is a no-op there (the active task is not
            // adoptable -- ADR-0031).
            ModalKey::Enter => match self.selector.selected_id() {
                Some(id) => ModalOutcome::Emit(ModalAction::AdoptTask(id.to_string())),
                None => ModalOutcome::Ignore,
            },
            // Inspect the target task's linked sessions (display-only detail).
            ModalKey::Right => match self.detail_target() {
                Some(id) => ModalOutcome::Emit(ModalAction::ViewTaskSessions(id)),
                None => ModalOutcome::Ignore,
            },
            ModalKey::Char('a') | ModalKey::Char('A') if self.active.is_some() => {
                ModalOutcome::Emit(ModalAction::AcceptTask)
            }
            ModalKey::Char('d') | ModalKey::Char('D') if self.active.is_some() => {
                ModalOutcome::Emit(ModalAction::ShowTaskDiff)
            }
            ModalKey::Char('r') | ModalKey::Char('R') if self.active.is_some() => {
                ModalOutcome::Emit(ModalAction::ListTaskRollback)
            }
            ModalKey::Esc => ModalOutcome::Close,
            ModalKey::CtrlC => {
                if self.selector.clear_search() {
                    ModalOutcome::Redraw
                } else {
                    ModalOutcome::Close
                }
            }
            ModalKey::Backspace => {
                if self.selector.backspace() {
                    ModalOutcome::Redraw
                } else {
                    ModalOutcome::Ignore
                }
            }
            ModalKey::Char(c) => {
                self.selector.push_char(c);
                ModalOutcome::Redraw
            }
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        if let Some(detail) = &self.detail {
            let rows: Vec<(Line<'static>, bool)> = detail
                .lines
                .iter()
                .map(|line| (Line::from(Span::styled(line.clone(), dim())), false))
                .collect();
            return crate::ui::tui::overlay_menu(
                Some(&format!("Sessions \u{00b7} task {}", detail.task_short)),
                rows,
                Some("\u{2190} back \u{00b7} esc close"),
                usize::from(width),
            );
        }
        let mut rows: Vec<(Line<'static>, bool)> = Vec::new();
        if let Some(active) = &self.active {
            rows.extend(active_header_rows(active));
            if self.has_recoverable || !self.blocked_recoverable.is_empty() {
                rows.push((Line::from(String::new()), false));
            }
        }
        if self.blocked_recoverable.is_empty() {
            rows.extend(selector_rows(&self.selector, "No tasks to resume"));
        } else {
            rows.extend(blocked_recoverable_rows(&self.blocked_recoverable));
        }
        crate::ui::tui::overlay_menu(
            Some("Tasks"),
            rows,
            Some(self.footer_hint()),
            usize::from(width),
        )
    }

    /// Footer key hints, adapting to whether any recoverable row is selectable.
    fn footer_hint(&self) -> &'static str {
        if self.active.is_some() {
            "a accept \u{00b7} d diff \u{00b7} r undo \u{00b7} \u{2192} sessions \u{00b7} esc close"
        } else if self.has_recoverable {
            "\u{2191}\u{2193} move \u{00b7} type to filter \u{00b7} \u{21b5} resume task \u{00b7} \u{2192} sessions \u{00b7} esc cancel"
        } else {
            "\u{2192} sessions \u{00b7} esc close"
        }
    }
}

fn blocked_recoverable_rows(cards: &[TaskCard]) -> Vec<(Line<'static>, bool)> {
    let mut rows = vec![(
        Line::from(Span::styled(
            "accept or undo the active task before resuming another",
            dim(),
        )),
        false,
    )];
    for card in cards {
        let label = if matches!(card.kind, TaskKind::Legacy) {
            format!("{}  (legacy)", card.body_preview())
        } else {
            card.body_preview()
        };
        let mut parts = Vec::new();
        let age = card.age_label();
        if !age.is_empty() {
            parts.push(age);
        }
        parts.push(card.session_summary());
        rows.push((
            Line::from(vec![
                Span::styled(label, dim()),
                Span::raw("  "),
                Span::styled(parts.join(" \u{00b7} "), dim()),
            ]),
            false,
        ));
    }
    rows
}

/// The non-selectable header rows for the active (live, unsettled) task: an
/// identity line (`\u{25c7} <id8>  <body>`), a dim meta line (attribution counts,
/// linked-session count, and age; each part shown only when known), and a dim
/// hint pointing at the existing settlement paths (never duplicated here).
fn active_header_rows(active: &TaskCard) -> Vec<(Line<'static>, bool)> {
    let identity = Line::from(vec![
        Span::styled(format!("{} ", crate::ui::symbols::PREVIEW), dim()),
        Span::raw(active.short_id()),
        Span::raw("  "),
        Span::raw(active.body_preview()),
    ]);
    let mut meta_parts = vec!["active".to_string()];
    if let Some(iris) = active.iris_files {
        meta_parts.push(format!("{iris} iris"));
    }
    if let Some(user) = active.user_files {
        meta_parts.push(format!("{user} yours"));
    }
    meta_parts.push(active.session_summary());
    let age = active.age_label();
    if !age.is_empty() {
        meta_parts.push(age);
    }
    let meta = Line::from(Span::styled(meta_parts.join(" \u{00b7} "), dim()));
    let approved = Line::from(Span::styled(
        format!("approved: {}", active.approved_scope_label()),
        dim(),
    ));
    let hint = Line::from(Span::styled(
        "actions: a accept \u{00b7} d diff \u{00b7} r undo \u{00b7} /checkpoint saves rollback points"
            .to_string(),
        dim(),
    ));
    vec![
        (identity, false),
        (meta, false),
        (approved, false),
        (hint, false),
    ]
}

// --- OAuth login dialog (display-only) ---

#[derive(Debug, Clone)]
pub(crate) struct LoginDialog {
    provider_name: String,
    lines: Vec<String>,
    /// Whether the dialog accepts a pasted authorization code / redirect URL
    /// (Anthropic, whose browser callback may need a manual fallback).
    manual: bool,
    /// Buffered manual-paste input (only collected when `manual`).
    input: String,
}

impl LoginDialog {
    pub(crate) fn new(provider_name: &str, manual: bool) -> Self {
        LoginDialog {
            provider_name: provider_name.to_string(),
            lines: vec!["Starting login...".to_string()],
            manual,
            input: String::new(),
        }
    }

    /// Replace the dialog body (auth URL, device code, progress).
    pub(crate) fn set_lines(&mut self, lines: Vec<String>) {
        self.lines = lines;
    }

    /// Append a progress line.
    pub(crate) fn push_line(&mut self, line: String) {
        self.lines.push(line);
    }

    /// Whether this dialog collects a pasted authorization code / redirect URL.
    pub(crate) fn accepts_manual_input(&self) -> bool {
        self.manual
    }

    /// Append a typed character to the manual-paste buffer.
    pub(crate) fn push_char(&mut self, ch: char) {
        if self.manual {
            self.input.push(ch);
        }
    }

    /// Append bracketed paste text to the manual-paste buffer.
    pub(crate) fn push_str(&mut self, text: &str) {
        if self.manual {
            self.input.push_str(text.trim_end_matches(['\r', '\n']));
        }
    }

    /// Delete the last character of the manual-paste buffer.
    pub(crate) fn backspace(&mut self) {
        self.input.pop();
    }

    /// Take and clear the current manual-paste buffer (on submit).
    pub(crate) fn take_input(&mut self) -> String {
        std::mem::take(&mut self.input)
    }

    fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            // Cancel aborts the in-flight login (the loop drops the task).
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let mut rows: Vec<(Line<'static>, bool)> = Vec::new();
        // Wrap each body line (notably the long OAuth URL) to the box's inner
        // width so it is shown in full and stays copyable, instead of being
        // clipped by the non-wrapping Paragraph that draws the modal.
        let wrap_at = usize::from(width).saturating_sub(4).max(1);
        for line in &self.lines {
            for row in crate::ui::tui::wrap_to_width(line, wrap_at) {
                rows.push((Line::from(Span::raw(row)), false));
            }
        }
        if self.manual {
            rows.push((
                Line::from(Span::styled(
                    "Or paste the authorization code / full redirect URL, then Enter:",
                    dim(),
                )),
                false,
            ));
            for row in crate::ui::tui::wrap_to_width(&format!("> {}", self.input), wrap_at) {
                rows.push((Line::from(Span::raw(row)), false));
            }
        }
        let title = format!("Login — {}", self.provider_name);
        crate::ui::tui::overlay_menu(Some(&title), rows, Some("esc cancel"), usize::from(width))
    }
}

#[derive(Clone)]
pub(crate) struct ApiKeyDialog {
    provider_id: String,
    provider_name: String,
    input: String,
}

impl std::fmt::Debug for ApiKeyDialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyDialog")
            .field("provider_id", &self.provider_id)
            .field("provider_name", &self.provider_name)
            .field("input", &"<redacted>")
            .finish()
    }
}

impl ApiKeyDialog {
    pub(crate) fn new(provider_id: &str, provider_name: &str) -> Self {
        Self {
            provider_id: provider_id.to_string(),
            provider_name: provider_name.to_string(),
            input: String::new(),
        }
    }

    pub(crate) fn take_input(&mut self) -> String {
        std::mem::take(&mut self.input)
    }

    pub(crate) fn push_str(&mut self, text: &str) {
        self.input.push_str(text.trim_end_matches(['\r', '\n']));
    }

    fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::Enter => {
                ModalOutcome::Emit(ModalAction::SaveApiKey(self.provider_id.clone()))
            }
            ModalKey::Backspace => {
                self.input.pop();
                ModalOutcome::Redraw
            }
            ModalKey::Char(ch) => {
                self.input.push(ch);
                ModalOutcome::Redraw
            }
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let masked = "•".repeat(self.input.chars().count());
        let rows = vec![
            (
                Line::from(Span::styled("Paste the API key, then press Enter.", dim())),
                false,
            ),
            (Line::from(Span::raw(format!("> {masked}"))), false),
        ];
        let title = format!("API key — {}", self.provider_name);
        crate::ui::tui::overlay_menu(
            Some(&title),
            rows,
            Some("↵ save · esc cancel"),
            usize::from(width),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::wayland::skills::{SkillMetadata, SkillScope};

    #[test]
    fn jj_setup_explains_effects_and_emits_explicit_decisions() {
        let mut prompt = JjSetupPrompt::new();
        let rendered = prompt
            .render(100)
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("snapshots"), "{rendered}");
        assert!(rendered.contains("external operations"), "{rendered}");
        assert!(rendered.contains("rollback/restoration"), "{rendered}");
        assert_eq!(
            prompt.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SetNativeJj(true))
        );
        assert_eq!(
            prompt.handle_key(ModalKey::Esc),
            ModalOutcome::Emit(ModalAction::SetNativeJj(false))
        );
    }

    #[test]
    fn skill_picker_filters_and_emits_path_qualified_selection() {
        let skills = vec![
            SkillMetadata {
                name: "alpha".to_string(),
                description: "Alpha workflow".to_string(),
                short_description: None,
                interface: None,
                dependencies: None,
                path: PathBuf::from("/skills/alpha/SKILL.md"),
                scope: SkillScope::Repo,
                policy: Default::default(),
            },
            SkillMetadata {
                name: "beta".to_string(),
                description: "Beta workflow".to_string(),
                short_description: None,
                interface: None,
                dependencies: None,
                path: PathBuf::from("/skills/beta/SKILL.md"),
                scope: SkillScope::User,
                policy: Default::default(),
            },
        ];
        let mut picker = SkillPicker::new(&skills);

        assert_eq!(picker.handle_key(ModalKey::Char('b')), ModalOutcome::Redraw);
        assert_eq!(
            picker.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::InsertSkillMention {
                name: "beta".to_string(),
                path: "/skills/beta/SKILL.md".to_string(),
            })
        );
        let text = picker
            .render(80)
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("beta"));
        assert!(!text.contains("alpha"));
    }

    #[test]
    fn goal_replacement_defaults_to_cancel_and_requires_explicit_selection() {
        let mut prompt = GoalReplacePrompt::new("new objective".to_string());
        assert_eq!(prompt.handle_key(ModalKey::Enter), ModalOutcome::Close);
        prompt.handle_key(ModalKey::Down);
        assert_eq!(
            prompt.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::ReplaceGoal("new objective".to_string()))
        );
    }

    #[test]
    fn goal_edit_dialog_emits_the_edited_objective() {
        let mut dialog = GoalEditDialog::new("old".to_string());
        dialog.handle_key(ModalKey::Backspace);
        dialog.handle_key(ModalKey::Char('w'));
        assert_eq!(
            dialog.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::EditGoal("olw".to_string()))
        );
    }

    #[test]
    fn modal_renders_through_component_trait_with_width_clamp() {
        use crate::ui::tui::Component;
        let modal = Modal::LoginDialog(LoginDialog::new("openai-codex", false));
        // The Component impl forwards to Modal::render after clamping usize->u16.
        assert_eq!(Component::render(&modal, 40), Modal::render(&modal, 40));
        // An out-of-u16-range width clamps to u16::MAX rather than overflowing.
        assert_eq!(
            Component::render(&modal, usize::from(u16::MAX) + 100),
            Modal::render(&modal, u16::MAX)
        );
    }

    #[test]
    fn login_dialog_cancel_closes() {
        let mut dialog = LoginDialog::new("openai-codex", false);
        dialog.set_lines(vec!["Open: https://example".to_string()]);
        assert_eq!(dialog.handle_key(ModalKey::CtrlC), ModalOutcome::Close);
    }

    #[test]
    fn api_key_dialog_masks_secret_and_emits_save_without_secret() {
        let mut dialog = ApiKeyDialog::new("openai", "OpenAI API");
        for ch in "sk-secret".chars() {
            dialog.handle_key(ModalKey::Char(ch));
        }
        let rendered = dialog
            .render(80)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!rendered.contains("sk-secret"), "{rendered}");
        assert!(rendered.contains("•••••••••"), "{rendered}");
        match dialog.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::SaveApiKey(provider_id)) => {
                assert_eq!(provider_id, "openai");
            }
            other => panic!("expected SaveApiKey, got {other:?}"),
        }
        assert_eq!(dialog.take_input(), "sk-secret");
        assert!(!format!("{dialog:?}").contains("sk-secret"));
    }

    #[test]
    fn manual_login_dialog_buffers_edits_and_takes_input() {
        let mut dialog = LoginDialog::new("anthropic", true);
        assert!(dialog.accepts_manual_input());
        // Backspacing an empty buffer is a no-op (no underflow/panic).
        dialog.backspace();
        for ch in "abX".chars() {
            dialog.push_char(ch);
        }
        dialog.backspace(); // drops 'X'
        dialog.push_char('c');
        assert_eq!(dialog.take_input(), "abc");
        // take_input clears the buffer.
        assert_eq!(dialog.take_input(), "");
    }

    #[test]
    fn non_manual_login_dialog_ignores_typed_characters() {
        let mut dialog = LoginDialog::new("openai-codex", false);
        assert!(!dialog.accepts_manual_input());
        dialog.push_char('x');
        assert_eq!(dialog.take_input(), "");
    }

    #[test]
    fn login_dialog_wraps_long_url_within_width_without_dropping_characters() {
        let url = "https://accounts.google.com/o/oauth2/v2/auth?client_id=1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com&scope=a+b+c";
        let line = format!("Open: {url}");
        let mut dialog = LoginDialog::new("antigravity", false);
        dialog.set_lines(vec![line]);

        let width = 40_u16;
        let rendered = dialog.render(width);
        let texts: Vec<String> = rendered
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();

        // Every rendered row fits the modal width (no clipping by the box). Test
        // data is ASCII, so char count equals display columns.
        for text in &texts {
            assert!(
                text.chars().count() <= usize::from(width),
                "row exceeds width: {text:?}"
            );
        }
        // The complete URL survives wrapping (char-wrapped, contiguous), so the
        // copy/paste fallback is the full, working URL rather than a clipped one.
        // Frameless rows carry no box chrome; just trim the row padding.
        let joined: String = texts
            .iter()
            .map(|text| text.trim_matches(' ').to_string())
            .collect();
        assert!(
            joined.contains(url),
            "wrapped rows must contain the full URL: {joined}"
        );
    }

    fn session_rows() -> Vec<SessionRow> {
        vec![
            SessionRow {
                id: "aaaa".to_string(),
                preview: "fix the login bug".to_string(),
                age: "5m ago".to_string(),
                task_linked: true,
            },
            SessionRow {
                id: "bbbb".to_string(),
                preview: "add rate limiting".to_string(),
                age: "2h ago".to_string(),
                task_linked: false,
            },
        ]
    }

    #[test]
    fn session_picker_enter_emits_resume_for_selected_id() {
        let mut picker = SessionPicker::new(session_rows());
        // First row is selected by default (newest first).
        match picker.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::ResumeSession(id)) => assert_eq!(id, "aaaa"),
            other => panic!("expected ResumeSession, got {other:?}"),
        }
        // Down then Enter resumes the second session.
        picker.handle_key(ModalKey::Down);
        match picker.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::ResumeSession(id)) => assert_eq!(id, "bbbb"),
            other => panic!("expected ResumeSession, got {other:?}"),
        }
    }

    #[test]
    fn session_picker_search_filters_and_esc_closes() {
        let mut picker = SessionPicker::new(session_rows());
        for c in "rate".chars() {
            picker.handle_key(ModalKey::Char(c));
        }
        match picker.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::ResumeSession(id)) => assert_eq!(id, "bbbb"),
            other => panic!("expected the filtered row, got {other:?}"),
        }
        assert_eq!(picker.handle_key(ModalKey::Esc), ModalOutcome::Close);
    }

    #[test]
    fn session_picker_marks_rows_linked_to_unreviewed_tasks() {
        let picker = SessionPicker::new(session_rows());
        let rendered: String = picker
            .render(80)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("fix the login bug") && rendered.contains("\u{25c7}"),
            "linked row should show the task marker: {rendered}"
        );
        let rate_row = rendered
            .lines()
            .find(|line| line.contains("add rate limiting"))
            .unwrap_or("");
        assert!(
            !rate_row.contains("\u{25c7}"),
            "unlinked row must not show the marker: {rate_row}"
        );
    }

    fn recoverable_cards() -> Vec<TaskCard> {
        use crate::wayland::git_safety::RecoverableTask;
        use std::time::Duration;
        vec![
            TaskCard::from_recoverable(&RecoverableTask::for_test(
                "taskaaaa",
                Duration::from_secs(300),
                Some("fix the parser"),
                &["s1", "s2"],
            )),
            TaskCard::from_recoverable(&RecoverableTask::for_test_legacy(
                "taskbbbb",
                Duration::from_secs(7200),
            )),
        ]
    }

    fn active_card() -> TaskCard {
        use crate::wayland::git_safety::ActiveTaskDisplay;
        use std::time::Duration;
        TaskCard::active(
            &ActiveTaskDisplay {
                task_id: "activeee1".to_string(),
                body: Some("refactor the loop".to_string()),
                sessions: vec!["live-session".to_string()],
                approved_paths: vec!["src/main.rs".to_string()],
                all_dirty_approved: false,
            },
            Some(Duration::from_secs(60)),
            Some(2),
            Some(1),
        )
    }

    #[test]
    fn task_picker_enter_emits_adopt_for_selected_id() {
        let mut picker = TaskPicker::new(None, recoverable_cards());
        // First row selected by default.
        match picker.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::AdoptTask(id)) => assert_eq!(id, "taskaaaa"),
            other => panic!("expected AdoptTask, got {other:?}"),
        }
        // Down then Enter adopts the second task; only the chosen id is emitted.
        picker.handle_key(ModalKey::Down);
        match picker.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::AdoptTask(id)) => assert_eq!(id, "taskbbbb"),
            other => panic!("expected AdoptTask, got {other:?}"),
        }
    }

    #[test]
    fn task_picker_search_filters_and_esc_closes() {
        let mut picker = TaskPicker::new(None, recoverable_cards());
        for c in "parser".chars() {
            picker.handle_key(ModalKey::Char(c));
        }
        match picker.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::AdoptTask(id)) => assert_eq!(id, "taskaaaa"),
            other => panic!("expected the filtered row, got {other:?}"),
        }
        assert_eq!(picker.handle_key(ModalKey::Esc), ModalOutcome::Close);
    }

    #[test]
    fn right_arrow_requests_linked_session_detail_for_target() {
        // A recoverable list: the right arrow targets the selected row.
        let mut picker = TaskPicker::new(None, recoverable_cards());
        match picker.handle_key(ModalKey::Right) {
            ModalOutcome::Emit(ModalAction::ViewTaskSessions(id)) => assert_eq!(id, "taskaaaa"),
            other => panic!("expected ViewTaskSessions, got {other:?}"),
        }
        // An active-only modal (no selectable rows): the right arrow targets the
        // active task; Enter is a no-op (the active task is never adoptable).
        let mut active_only = TaskPicker::new(Some(active_card()), Vec::new());
        assert_eq!(
            active_only.handle_key(ModalKey::Enter),
            ModalOutcome::Ignore
        );
        match active_only.handle_key(ModalKey::Right) {
            ModalOutcome::Emit(ModalAction::ViewTaskSessions(id)) => assert_eq!(id, "activeee1"),
            other => panic!("expected ViewTaskSessions for the active task, got {other:?}"),
        }
    }

    #[test]
    fn detail_view_is_read_only_and_returns_to_the_list() {
        let mut picker = TaskPicker::new(Some(active_card()), recoverable_cards());
        picker.show_detail(
            "taskaaaa",
            vec!["session abc".to_string(), "  > hello".to_string()],
        );
        // In the detail view, adoption keys do nothing.
        assert_eq!(picker.handle_key(ModalKey::Enter), ModalOutcome::Ignore);
        assert_eq!(picker.handle_key(ModalKey::Down), ModalOutcome::Ignore);
        // Left returns to the list without closing the modal.
        assert_eq!(picker.handle_key(ModalKey::Left), ModalOutcome::Redraw);
        // Back in the list, Enter is still blocked because an active task exists.
        assert_eq!(picker.handle_key(ModalKey::Enter), ModalOutcome::Ignore);
        // Esc in the detail view dismisses the whole modal (matches the footer).
        picker.show_detail("taskaaaa", vec!["session abc".to_string()]);
        assert_eq!(picker.handle_key(ModalKey::Esc), ModalOutcome::Close);
    }

    #[test]
    fn task_picker_with_active_task_blocks_recoverable_adoption() {
        let mut picker = TaskPicker::new(Some(active_card()), recoverable_cards());
        assert_eq!(
            picker.handle_key(ModalKey::Enter),
            ModalOutcome::Ignore,
            "resume rows are not selectable while a task is active"
        );
        let text: String = picker
            .render(80)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("accept or undo the active task"),
            "blocked rows explain the active-task requirement: {text}"
        );
        assert!(
            !text.contains("adopt"),
            "footer omits the old adopt action while resume is blocked: {text}"
        );
        assert_eq!(
            picker.handle_key(ModalKey::Char('a')),
            ModalOutcome::Emit(ModalAction::AcceptTask)
        );
        assert_eq!(
            picker.handle_key(ModalKey::Char('d')),
            ModalOutcome::Emit(ModalAction::ShowTaskDiff)
        );
        assert_eq!(
            picker.handle_key(ModalKey::Char('r')),
            ModalOutcome::Emit(ModalAction::ListTaskRollback)
        );
    }

    #[test]
    fn active_header_renders_identity_meta_and_task_actions() {
        let picker = TaskPicker::new(Some(active_card()), recoverable_cards());
        let text: String = picker
            .render(80)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("activeee"), "active id shown: {text}");
        assert!(
            text.contains("refactor the loop"),
            "active body shown: {text}"
        );
        assert!(
            text.contains("2 iris") && text.contains("1 yours"),
            "counts: {text}"
        );
        assert!(text.contains("1 session"), "linked-session count: {text}");
        assert!(
            text.contains("approved: src/main.rs"),
            "approved scope shown: {text}"
        );
        assert!(
            text.contains("a accept") && text.contains("d diff") && text.contains("r undo"),
            "active task actions shown: {text}"
        );
        // The resumable list is shown below the active header.
        assert!(text.contains("fix the parser"), "resume row shown: {text}");
        assert!(text.contains("(legacy)"), "legacy marker shown: {text}");
        assert!(
            text.contains("accept or undo the active task"),
            "resume rows are blocked while active: {text}"
        );
    }
}
