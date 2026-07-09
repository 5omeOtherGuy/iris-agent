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

use crate::mimir::model_capabilities;
use crate::mimir::model_catalog::{self, CatalogModel};
use crate::mimir::selection::{ProviderId, ReasoningEffort};
use crate::ui::selector::{Selector, SelectorItem};
use crate::wayland::trust::ProjectPolicyEdit;

/// Max rows shown in above-editor menus before windowing. Keep enough room for
/// footer controls plus the editor/status chrome on a compact terminal.
const MODEL_ROWS: usize = 5;
const SCOPED_ROWS: usize = 5;
const PROVIDER_ROWS: usize = 5;

/// A key the loop forwards to the active modal. A neutral subset of crossterm
/// keys so modal handling is unit-testable without constructing terminal events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalKey {
    Up,
    Down,
    Left,
    Right,
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

/// The `/login` first-step choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoginMethod {
    Subscription,
    ApiKey,
}

/// Whether a provider selector configures (`/login`) or removes (`/logout`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderPurpose {
    Login,
    ApiKeyLogin,
    Logout,
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
    /// Persist a settings field to the user-global file (`None` clears the key).
    /// The loop maps the field to its `config::save_*`; the panel stays open
    /// and keeps its own display state (it already clicked the detent).
    SaveSetting {
        field: crate::ui::settings_menu::Field,
        value: Option<String>,
    },
    /// Settings -> open the existing `/model` picker (default model).
    OpenModelPicker,
    /// Settings -> toggle this session's dangerous approval-gate bypass.
    ToggleSkipPermissions,
    /// Settings -> open the existing `/trust` project-permissions modal.
    OpenTrustMenu,
    /// Settings -> open the existing `/scoped-models` picker.
    OpenScopedModels,
    /// Settings -> open the existing `/login` method selector.
    OpenLoginMethod,
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
    /// `/login` method chosen -> open the matching provider selector.
    ChooseLoginMethod(LoginMethod),
    /// Begin an OAuth/subscription login for this provider.
    BeginLogin(ProviderId),
    /// Open an API-key entry dialog for this provider id.
    OpenApiKeyDialog(String),
    /// Store the API key currently buffered in the active API-key dialog.
    SaveApiKey(String),
    /// Remove the stored credential for this provider id.
    Logout(String),
    /// Provider-select cancel during `/login` returns to the method selector.
    BackToLoginMethod,
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

/// The active picker/dialog.
#[derive(Debug, Clone)]
pub(crate) enum Modal {
    Model(ModelPicker),
    SwitchContext(SwitchContextPrompt),
    Scoped(ScopedModels),
    // Boxed (like Tasks) so the panel's snapshot does not dominate the enum.
    Settings(Box<crate::ui::settings_menu::SettingsPanel>),
    Trust(TrustMenu),
    Session(SessionPicker),
    Tasks(TaskPicker),
    LoginMethod(MethodSelect),
    Providers(ProviderSelect),
    LoginDialog(LoginDialog),
    ApiKeyDialog(ApiKeyDialog),
}

impl Modal {
    pub(crate) fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match self {
            Modal::Model(picker) => picker.handle_key(key),
            Modal::SwitchContext(prompt) => prompt.handle_key(key),
            Modal::Scoped(picker) => picker.handle_key(key),
            Modal::Settings(panel) => panel.handle_key(key),
            Modal::Trust(menu) => menu.handle_key(key),
            Modal::Session(picker) => picker.handle_key(key),
            Modal::Tasks(picker) => picker.handle_key(key),
            Modal::LoginMethod(menu) => menu.handle_key(key),
            Modal::Providers(picker) => picker.handle_key(key),
            Modal::LoginDialog(dialog) => dialog.handle_key(key),
            Modal::ApiKeyDialog(dialog) => dialog.handle_key(key),
        }
    }

    pub(crate) fn paste_text(&mut self, text: &str) -> ModalOutcome {
        match self {
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
            Modal::Model(picker) => picker.render(width),
            Modal::SwitchContext(prompt) => prompt.render(width),
            Modal::Scoped(picker) => picker.render(width),
            Modal::Settings(panel) => panel.render(width),
            Modal::Trust(menu) => menu.render(width),
            Modal::Session(picker) => picker.render(width),
            Modal::Tasks(picker) => picker.render(width),
            Modal::LoginMethod(menu) => menu.render(width),
            Modal::Providers(picker) => picker.render(width),
            Modal::LoginDialog(dialog) => dialog.render(width),
            Modal::ApiKeyDialog(dialog) => dialog.render(width),
        }
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
/// metadata stays muted; an enabled/disabled mark uses the `◉`/`○` glyphs from
/// the closed vocabulary (never `[x]`). `empty` is the no-match message.
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
        if let Some(enabled) = row.item.enabled {
            spans.push(Span::styled(
                if enabled {
                    format!("{} ", crate::ui::symbols::ACTIVE)
                } else {
                    format!("{} ", crate::ui::symbols::EMPTY)
                },
                if enabled {
                    Style::default().fg(crate::ui::palette::orange())
                } else {
                    dim()
                },
            ));
        }
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

// --- model picker ---

#[derive(Debug, Clone)]
pub(crate) struct ModelPicker {
    selector: Selector,
    /// Authenticated models, persisted-default first then by provider name.
    models: Vec<CatalogModel>,
    /// Qualified id of the active session model (gets the checkmark).
    current: String,
    /// Qualified id of the persisted default model (gets the "Default" label).
    default: String,
    /// The reasoning effort to apply, kept clamped to the selected model.
    effort: ReasoningEffort,
}

impl ModelPicker {
    /// Build the picker. `available` is the authenticated catalog; `current` is
    /// the active session model's qualified id (marked with the checkmark);
    /// `default` is the persisted default's qualified id (labeled "Default" and
    /// sorted to the top); `effort` is the active reasoning level (clamped to the
    /// selected row).
    pub(crate) fn new(
        available: Vec<CatalogModel>,
        current: &str,
        default: &str,
        effort: ReasoningEffort,
    ) -> Self {
        let models = order_by_default(available, default);
        let items: Vec<SelectorItem> = models
            .iter()
            .map(|model| SelectorItem::new(model.qualified(), model.id.clone()))
            .collect();
        // Non-searchable: the mockup drives the picker with Up/Down + Left/Right +
        // Enter/s hotkeys; `/model <id>` still resolves an exact id directly.
        let mut selector = Selector::new(items, false, true, MODEL_ROWS);
        selector.select_id(current);
        ModelPicker {
            selector,
            models,
            current: current.to_string(),
            default: default.to_string(),
            effort,
        }
    }

    /// The catalog model under the cursor, if any.
    fn selected_model(&self) -> Option<&CatalogModel> {
        let id = self.selector.selected_id()?;
        self.models.iter().find(|model| model.qualified() == id)
    }

    /// The effort shown and applied for the selected model: the user's target
    /// (`self.effort`) clamped to that model's supported levels. Navigation never
    /// mutates the target, so arrowing past a low-cap model does not truncate it.
    fn display_effort(&self) -> ReasoningEffort {
        match self.selected_model() {
            Some(model) => model_capabilities::clamp(model.provider, &model.id, self.effort),
            None => self.effort,
        }
    }

    fn display_effort_label(&self) -> &'static str {
        match self.selected_model() {
            Some(model) => {
                model_capabilities::display_level(model.provider, &model.id, self.display_effort())
            }
            None => self.display_effort().as_str(),
        }
    }

    /// Emit a model+effort selection. `save_default` persists it as the default
    /// (Enter) versus applying it for this session only (`s`). The emitted effort
    /// is `display_effort()` (the target clamped to the chosen model), which is
    /// exactly what the user sees and what is safe to persist: startup `resolve`
    /// trusts the stored reasoning without re-clamping, so the saved level must be
    /// valid for the saved model.
    fn select(&self, save_default: bool) -> ModalOutcome {
        match self.selector.selected_id() {
            Some(id) => ModalOutcome::Emit(ModalAction::SelectModel {
                id: id.to_string(),
                effort: self.display_effort(),
                save_default,
            }),
            None => ModalOutcome::Ignore,
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
            // Left/Right adjust the inline reasoning effort within the selected
            // model's supported levels.
            ModalKey::Left | ModalKey::Right => {
                let forward = matches!(key, ModalKey::Right);
                if let Some((provider, id)) =
                    self.selected_model().map(|m| (m.provider, m.id.clone()))
                {
                    // Cycle from the value currently shown (target clamped to
                    // this model) so adjusting on a capped model is intuitive.
                    let from = model_capabilities::clamp(provider, &id, self.effort);
                    if let Some(next) =
                        model_capabilities::cycle_effort(provider, &id, from, forward)
                    {
                        self.effort = next;
                    }
                }
                ModalOutcome::Redraw
            }
            // Enter persists the choice as the default; `s` applies it for this
            // session only.
            ModalKey::Enter => self.select(true),
            ModalKey::Char('s') | ModalKey::Char('S') => self.select(false),
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        // The Picker idiom from the design system: `◉` marks the current
        // model (orange), the label is bold on the highlighted (surface-fill)
        // row, and the provider is the muted right column. The default model
        // carries a quiet `default` tag instead of a column of labels.
        let name_w = self
            .models
            .iter()
            .map(|m| model_catalog::display_name(&m.qualified()).len())
            .max()
            .unwrap_or(0);
        let mut rows: Vec<(Line<'static>, bool)> = Vec::new();
        for row in self.selector.visible() {
            let qualified = row.item.id.clone();
            let model = self.models.iter().find(|m| m.qualified() == qualified);
            let base = if row.selected {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let marker = if qualified == self.current {
                Span::styled(
                    format!("{} ", crate::ui::symbols::ACTIVE),
                    Style::default().fg(crate::ui::palette::orange()),
                )
            } else {
                Span::raw("  ")
            };
            let name = model_catalog::display_name(&qualified);
            let mut spans = vec![marker, Span::styled(format!("{name:<name_w$}"), base)];
            if let Some(m) = model {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(m.provider.display_name().to_string(), dim()));
            }
            if qualified == self.default {
                spans.push(Span::styled("  default", dim()));
            }
            rows.push((Line::from(spans), row.selected));
        }
        if self.selector.is_scrolled() {
            rows.push((
                Line::from(Span::styled(self.selector.position_label(), dim())),
                false,
            ));
        }
        let footer = format!(
            "↑↓ move · ←→ effort ({}) · ↵ select · s session · esc cancel",
            self.display_effort_label()
        );
        // ONE selector for the adjacent pair: rows pick the model, ←→ clicks
        // the reasoning detent. `/model` and a bare `/reasoning` both open it.
        crate::ui::tui::overlay_menu(
            Some("Model & reasoning"),
            rows,
            Some(&footer),
            usize::from(width),
        )
    }
}

/// Order the list: the persisted default first (labeled "Default"), then the rest
/// by provider name, preserving registry order within a provider.
fn order_by_default(models: Vec<CatalogModel>, default: &str) -> Vec<CatalogModel> {
    let mut ordered: Vec<CatalogModel> = Vec::with_capacity(models.len());
    if let Some(found) = models.iter().find(|model| model.qualified() == default) {
        ordered.push(found.clone());
    }
    let mut rest: Vec<CatalogModel> = models
        .into_iter()
        .filter(|model| model.qualified() != default)
        .collect();
    // Stable sort by provider name keeps within-provider registry order.
    rest.sort_by(|a, b| a.provider.as_str().cmp(b.provider.as_str()));
    ordered.extend(rest);
    ordered
}

// --- scoped-models picker ---

#[derive(Debug, Clone)]
pub(crate) struct ScopedModels {
    selector: Selector,
    /// All authenticated candidates, registry order.
    candidates: Vec<CatalogModel>,
    /// Explicit enabled ids in order, or `None` = all enabled (no filter).
    enabled: Option<Vec<String>>,
    dirty: bool,
}

impl ScopedModels {
    pub(crate) fn new(candidates: Vec<CatalogModel>, enabled: Option<Vec<String>>) -> Self {
        let mut picker = ScopedModels {
            selector: Selector::new(Vec::new(), true, true, SCOPED_ROWS),
            candidates,
            enabled,
            dirty: false,
        };
        picker.collapse_full();
        picker.rebuild();
        picker
    }

    /// Fold an explicit list that covers every candidate back to `None`
    /// ("all enabled"), matching pi-mono. An explicit empty list stays `Some([])`
    /// - a deliberate "nothing enabled".
    fn collapse_full(&mut self) {
        if let Some(list) = &self.enabled
            && !list.is_empty()
            && list.len() >= self.candidates.len()
            && self
                .candidates
                .iter()
                .all(|m| list.iter().any(|e| e == &m.qualified()))
        {
            self.enabled = None;
        }
    }

    /// Display order: enabled ids first (in their configured order), then the
    /// remaining candidates in registry order. With `enabled = None` every row
    /// shows no checkmark column.
    fn rebuild(&mut self) {
        let mut items: Vec<SelectorItem> = Vec::with_capacity(self.candidates.len());
        let mut seen: Vec<String> = Vec::new();
        if let Some(enabled) = &self.enabled {
            for id in enabled {
                if let Some(model) = self.candidates.iter().find(|m| &m.qualified() == id) {
                    items.push(self.item(model, true));
                    seen.push(id.clone());
                }
            }
        }
        for model in &self.candidates {
            let qualified = model.qualified();
            if seen.contains(&qualified) {
                continue;
            }
            let enabled = self.enabled.as_ref().map(|_| false);
            items.push(match enabled {
                Some(_) => self.item(model, false),
                None => {
                    SelectorItem::new(qualified, model.id.clone()).detail(model.provider.as_str())
                }
            });
        }
        self.selector.replace_items(items);
    }

    fn item(&self, model: &CatalogModel, enabled: bool) -> SelectorItem {
        SelectorItem::new(model.qualified(), model.id.clone())
            .detail(model.provider.as_str())
            .enabled(enabled)
    }

    /// The current scope to apply/persist: the explicit enabled list, or `None`.
    fn scope(&self) -> Option<Vec<String>> {
        self.enabled.clone()
    }

    fn is_enabled(&self, id: &str) -> bool {
        match &self.enabled {
            None => true,
            Some(list) => list.iter().any(|e| e == id),
        }
    }

    fn toggle(&mut self, id: &str) {
        let mut list = match &self.enabled {
            // Toggling while all-enabled creates a one-item explicit list.
            None => Vec::new(),
            Some(list) => list.clone(),
        };
        if let Some(pos) = list.iter().position(|e| e == id) {
            list.remove(pos);
        } else {
            list.push(id.to_string());
        }
        self.enabled = Some(list);
        self.collapse_full();
        self.dirty = true;
        self.rebuild();
        self.selector.select_id(id);
    }

    /// Enable/disable a whole set of ids. `enable` true adds any missing; false
    /// removes them.
    fn set_many(&mut self, ids: &[String], enable: bool) {
        let mut list = match &self.enabled {
            None => {
                // From "all enabled", an enable-all is a no-op; a clear starts
                // from the full set so we can remove from it.
                if enable {
                    return;
                }
                self.candidates
                    .iter()
                    .map(CatalogModel::qualified)
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
        self.enabled = Some(list);
        self.collapse_full();
        self.dirty = true;
        self.rebuild();
    }

    fn toggle_provider(&mut self, provider: ProviderId) {
        let ids: Vec<String> = self
            .candidates
            .iter()
            .filter(|m| m.provider == provider)
            .map(CatalogModel::qualified)
            .collect();
        let all_on = ids.iter().all(|id| self.is_enabled(id));
        self.set_many(&ids, !all_on);
    }

    fn reorder(&mut self, up: bool) {
        let Some(id) = self.selector.selected_id().map(str::to_string) else {
            return;
        };
        // Reorder only applies to an explicit list and an enabled row.
        let Some(list) = self.enabled.as_mut() else {
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
        self.dirty = true;
        self.rebuild();
        self.selector.select_id(&id);
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
            ModalKey::Enter => match self.selector.selected_id().map(str::to_string) {
                Some(id) => {
                    self.toggle(&id);
                    ModalOutcome::Emit(ModalAction::ApplyScoped(self.scope()))
                }
                None => ModalOutcome::Ignore,
            },
            ModalKey::CtrlA => {
                let ids = self.matching_or_all();
                self.set_many(&ids, true);
                ModalOutcome::Emit(ModalAction::ApplyScoped(self.scope()))
            }
            ModalKey::CtrlX => {
                let ids = self.matching_or_all();
                self.set_many(&ids, false);
                ModalOutcome::Emit(ModalAction::ApplyScoped(self.scope()))
            }
            ModalKey::CtrlP => match self.selector.selected() {
                Some(item) => {
                    let provider = item
                        .id
                        .split_once('/')
                        .and_then(|(p, _)| ProviderId::parse(p).ok());
                    if let Some(provider) = provider {
                        self.toggle_provider(provider);
                        ModalOutcome::Emit(ModalAction::ApplyScoped(self.scope()))
                    } else {
                        ModalOutcome::Ignore
                    }
                }
                None => ModalOutcome::Ignore,
            },
            ModalKey::AltUp => {
                self.reorder(true);
                ModalOutcome::Emit(ModalAction::ApplyScoped(self.scope()))
            }
            ModalKey::AltDown => {
                self.reorder(false);
                ModalOutcome::Emit(ModalAction::ApplyScoped(self.scope()))
            }
            ModalKey::CtrlS => {
                self.dirty = false;
                ModalOutcome::Emit(ModalAction::SaveScoped(self.scope()))
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

    /// The filtered ids when a search is active, otherwise every candidate id.
    fn matching_or_all(&self) -> Vec<String> {
        if self.selector.search().is_some_and(|s| !s.is_empty()) {
            self.selector.filtered_ids()
        } else {
            self.candidates
                .iter()
                .map(CatalogModel::qualified)
                .collect()
        }
    }

    fn enabled_count(&self) -> usize {
        match &self.enabled {
            None => self.candidates.len(),
            Some(list) => list.len(),
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let rows = selector_rows(&self.selector, "No matching models");
        let count = if self.enabled.is_none() {
            "all enabled".to_string()
        } else {
            format!("{}/{} enabled", self.enabled_count(), self.candidates.len())
        };
        let unsaved = if self.dirty { " · unsaved" } else { "" };
        let footer = format!(
            "↵ toggle · ctrl+a all · ctrl+x clear · ctrl+p provider · alt+↑↓ reorder · ctrl+s save · {count}{unsaved}"
        );
        crate::ui::tui::overlay_menu(
            Some("Scoped models"),
            rows,
            Some(&footer),
            usize::from(width),
        )
    }
}

// --- project permissions menu (/trust) ---

/// View and edit this project's persistent permission policy (ADR-0027): toggle
/// per-tool approval grants (`write`/`edit`) and revoke stored `bash` command
/// grants. Confirming a row emits [`ModalAction::EditPolicy`]; the loop
/// persists the edit and refreshes the live agent's policy. Presentation-only:
/// the caller supplies the current policy snapshot, so this stays disk-free.
#[derive(Debug, Clone)]
pub(crate) struct TrustMenu {
    selector: Selector,
    /// Row id -> the edit Enter emits for it.
    edits: Vec<(String, ProjectPolicyEdit)>,
    /// Stored sandbox posture, shown read-only (enforcement deferred).
    sandbox: Option<String>,
}

/// The per-tool grants the `/trust` editor can toggle. Matches the ADR-0027
/// per-tool approval defaults; `bash` is intentionally absent (bash grants are
/// per-command, minted at the approval prompt).
const POLICY_TOOLS: &[&str] = &["write", "edit"];

impl TrustMenu {
    pub(crate) fn new(
        granted_tools: &[String],
        bash_exact: &[String],
        bash_prefix: &[String],
        sandbox: Option<String>,
    ) -> Self {
        let mut items = Vec::new();
        let mut edits = Vec::new();
        for tool in POLICY_TOOLS {
            let granted = granted_tools.iter().any(|t| t == tool);
            let id = format!("tool:{tool}");
            let mut item = SelectorItem::new(&id, *tool).detail(if granted {
                "always allowed for this project · ↵ revoke"
            } else {
                "prompts for approval · ↵ always allow for this project"
            });
            if granted {
                item = item.trailing("granted");
            }
            items.push(item);
            let edit = if granted {
                ProjectPolicyEdit::RevokeTool((*tool).to_string())
            } else {
                ProjectPolicyEdit::GrantTool((*tool).to_string())
            };
            edits.push((id, edit));
        }
        for command in bash_exact {
            let id = format!("bash:{command}");
            items.push(
                SelectorItem::new(&id, format!("bash: {command}"))
                    .detail("↵ revoke")
                    .trailing("granted"),
            );
            edits.push((id, ProjectPolicyEdit::RevokeBashExact(command.clone())));
        }
        for prefix in bash_prefix {
            let id = format!("pfx:{prefix}");
            items.push(
                SelectorItem::new(&id, format!("bash prefix: {prefix}"))
                    .detail("↵ revoke")
                    .trailing("granted"),
            );
            edits.push((id, ProjectPolicyEdit::RevokeBashPrefix(prefix.clone())));
        }
        let selector = Selector::new(items, false, false, 12);
        TrustMenu {
            selector,
            edits,
            sandbox,
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
            ModalKey::Enter => {
                let Some(id) = self.selector.selected_id() else {
                    return ModalOutcome::Ignore;
                };
                match self.edits.iter().find(|(row, _)| row == id) {
                    Some((_, edit)) => ModalOutcome::Emit(ModalAction::EditPolicy(edit.clone())),
                    None => ModalOutcome::Ignore,
                }
            }
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let rows = selector_rows(&self.selector, "No grants");
        let hint = match &self.sandbox {
            Some(posture) => format!("sandbox: {posture} · ↑↓ move · ↵ toggle/revoke · esc close"),
            None => "↑↓ move · ↵ toggle/revoke · esc close".to_string(),
        };
        crate::ui::tui::overlay_menu(
            Some("Project permissions"),
            rows,
            Some(hint.as_str()),
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

// --- login method selector ---

#[derive(Debug, Clone)]
pub(crate) struct MethodSelect {
    selector: Selector,
}

impl MethodSelect {
    pub(crate) fn new() -> Self {
        let items = vec![
            SelectorItem::new("subscription", "Use a subscription"),
            SelectorItem::new("api_key", "Use an API key"),
        ];
        MethodSelect {
            // No wrap, no search: pi-mono's auth method selector.
            selector: Selector::new(items, false, false, 8),
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
            // j/k navigation (pi-mono).
            ModalKey::Char('k') => {
                self.selector.up();
                ModalOutcome::Redraw
            }
            ModalKey::Char('j') => {
                self.selector.down();
                ModalOutcome::Redraw
            }
            ModalKey::Enter => match self.selector.selected_id() {
                Some("subscription") => {
                    ModalOutcome::Emit(ModalAction::ChooseLoginMethod(LoginMethod::Subscription))
                }
                Some("api_key") => {
                    ModalOutcome::Emit(ModalAction::ChooseLoginMethod(LoginMethod::ApiKey))
                }
                _ => ModalOutcome::Ignore,
            },
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let rows = selector_rows(&self.selector, "No methods");
        crate::ui::tui::overlay_menu(
            Some("Login"),
            rows,
            Some("↑↓ move · ↵ select · esc cancel"),
            usize::from(width),
        )
    }
}

// --- provider selector (login / logout) ---

/// A provider row for the selector: display name, provider id, and an already-
/// computed status badge (never a secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) badge: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ProviderSelect {
    selector: Selector,
    purpose: ProviderPurpose,
    empty: String,
}

impl ProviderSelect {
    pub(crate) fn new(purpose: ProviderPurpose, providers: Vec<ProviderRow>, empty: &str) -> Self {
        let items: Vec<SelectorItem> = providers
            .into_iter()
            .map(|provider| SelectorItem::new(provider.id, provider.name).trailing(provider.badge))
            .collect();
        ProviderSelect {
            // Provider list clamps (no wrap), with search.
            selector: Selector::new(items, true, false, PROVIDER_ROWS),
            purpose,
            empty: empty.to_string(),
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
            ModalKey::Enter => match self.selector.selected_id().map(str::to_string) {
                Some(id) => match self.purpose {
                    ProviderPurpose::Login => match ProviderId::parse(&id) {
                        Ok(provider) => ModalOutcome::Emit(ModalAction::BeginLogin(provider)),
                        Err(_) => ModalOutcome::Ignore,
                    },
                    ProviderPurpose::ApiKeyLogin => {
                        ModalOutcome::Emit(ModalAction::OpenApiKeyDialog(id))
                    }
                    ProviderPurpose::Logout => ModalOutcome::Emit(ModalAction::Logout(id)),
                },
                None => ModalOutcome::Ignore,
            },
            ModalKey::Esc => self.cancel(),
            // Provider selection cancels on Ctrl+C (login returns to the method
            // selector; logout restores the editor) - no search-clearing here.
            ModalKey::CtrlC => self.cancel(),
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

    /// Login cancel returns to the method selector; logout cancel closes.
    fn cancel(&self) -> ModalOutcome {
        match self.purpose {
            ProviderPurpose::Login | ProviderPurpose::ApiKeyLogin => {
                ModalOutcome::Emit(ModalAction::BackToLoginMethod)
            }
            ProviderPurpose::Logout => ModalOutcome::Close,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let rows = selector_rows(&self.selector, &self.empty);
        let title = match self.purpose {
            ProviderPurpose::Login => "Select provider",
            ProviderPurpose::ApiKeyLogin => "Store API key",
            ProviderPurpose::Logout => "Logout",
        };
        crate::ui::tui::overlay_menu(
            Some(title),
            rows,
            Some("↑↓ move · ↵ select · esc cancel"),
            usize::from(width),
        )
    }
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
    use super::*;

    fn cat(provider: ProviderId, id: &str) -> CatalogModel {
        CatalogModel {
            provider,
            id: id.to_string(),
            ctx_label: None,
        }
    }

    fn models() -> Vec<CatalogModel> {
        vec![
            cat(ProviderId::OpenAiCodex, "gpt-5.5"),
            cat(ProviderId::Anthropic, "claude-sonnet-4-6"),
            cat(ProviderId::Antigravity, "gemini-3.5-flash"),
        ]
    }

    fn render_text(picker: &ModelPicker) -> String {
        picker
            .render(80)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn model_picker_enter_saves_default_and_s_is_session_only() {
        let mut picker = ModelPicker::new(
            models(),
            "openai-codex/gpt-5.5",
            "openai-codex/gpt-5.5",
            ReasoningEffort::Medium,
        );
        // Default (gpt-5.5) is first; move to the next row (sonnet) and pick it.
        picker.handle_key(ModalKey::Down);
        match picker.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::SelectModel {
                id,
                effort,
                save_default,
            }) => {
                assert_eq!(id, "anthropic/claude-sonnet-4-6");
                assert_eq!(effort, ReasoningEffort::Medium);
                assert!(save_default, "Enter persists the default");
            }
            other => panic!("expected SelectModel, got {other:?}"),
        }
        // `s` selects the same row for this session only (no persist).
        match picker.handle_key(ModalKey::Char('s')) {
            ModalOutcome::Emit(ModalAction::SelectModel { save_default, .. }) => {
                assert!(!save_default, "s applies for this session only");
            }
            other => panic!("expected SelectModel, got {other:?}"),
        }
    }

    #[test]
    fn model_picker_render_shows_picker_idiom_and_footer() {
        let picker = ModelPicker::new(
            models(),
            "openai-codex/gpt-5.5",
            "openai-codex/gpt-5.5",
            ReasoningEffort::High,
        );
        let text = render_text(&picker);
        // Frameless: bold uppercase title, ◉ current marker, provider meta — and
        // no box-drawing frame anywhere.
        assert!(text.contains("MODEL & REASONING"), "{text}");
        assert!(
            !text.chars().any(|c| "┌┐└┘├┤│".contains(c)),
            "no frame chars: {text}"
        );
        assert!(text.contains("◉ GPT 5.5"), "{text}");
        assert!(text.contains("OpenAI"), "{text}");
        assert!(text.contains("default"), "{text}");
        // Footer: honest key hints incl. the inline effort adjust.
        assert!(text.contains("←→ effort (high)"), "{text}");
        assert!(text.contains("↵ select"), "{text}");
        assert!(text.contains("s session"), "{text}");
        assert!(text.contains("esc cancel"), "{text}");
        // The old columns/badges and search prompt are gone.
        assert!(!text.contains("[ctx:"), "{text}");
        assert!(!text.contains("[sub]"), "{text}");
        assert!(!text.contains("Default  current"), "{text}");
        assert!(!text.contains("Switch between models"), "{text}");
        assert!(!text.contains('\u{276f}'), "{text}");
        assert!(!text.contains('\u{2714}'), "{text}");
    }

    #[test]
    fn selected_modal_rows_use_surface_fill_with_bold_label() {
        let picker = ModelPicker::new(
            models(),
            "openai-codex/gpt-5.5",
            "openai-codex/gpt-5.5",
            ReasoningEffort::High,
        );
        let lines = picker.render(80);
        let selected = lines
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .any(|span| span.style.bg == Some(crate::ui::palette::surface()))
            })
            .expect("a surface-filled selected row");

        // The selection is the surface fill + a bold label — never a
        // color-only (cyan) accent.
        assert!(
            selected
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD)),
            "selected row label should be bold: {selected:?}"
        );
        assert!(
            selected
                .spans
                .iter()
                .all(|span| span.style.fg != Some(ratatui::style::Color::Cyan)),
            "selected row must not use the cyan accent: {selected:?}"
        );
    }

    #[test]
    fn model_picker_render_matches_mockup_contract() {
        let picker = ModelPicker::new(
            vec![
                cat(ProviderId::OpenAiCodex, "gpt-5.5"),
                cat(ProviderId::Anthropic, "claude-opus-4-8"),
                cat(ProviderId::Anthropic, "claude-sonnet-4-6"),
                cat(ProviderId::Anthropic, "claude-haiku-4-5"),
            ],
            "anthropic/claude-opus-4-8",
            "anthropic/claude-opus-4-8",
            ReasoningEffort::XHigh,
        );
        let text = render_text(&picker);

        assert!(text.contains("◉ Opus 4.8"), "{text}");
        assert!(text.contains("Opus 4.8"), "{text}");
        assert!(text.contains("Sonnet 4.6"), "{text}");
        assert!(text.contains("Haiku 4.5"), "{text}");
        assert!(text.contains("GPT 5.5"), "{text}");
        assert!(text.contains("Anthropic"), "{text}");
        assert!(text.contains("OpenAI"), "{text}");
        assert!(text.contains("←→ effort (max)"), "{text}");
        assert!(!text.contains("Only showing models"), "{text}");
        assert!(!text.contains("claude-opus-4-8"), "{text}");
        assert!(!text.contains("GPT-5.5"), "{text}");
    }

    #[test]
    fn model_picker_left_right_adjust_inline_effort() {
        let mut picker = ModelPicker::new(
            models(),
            "openai-codex/gpt-5.5",
            "openai-codex/gpt-5.5",
            ReasoningEffort::Medium,
        );
        assert!(render_text(&picker).contains("effort (medium)"));
        picker.handle_key(ModalKey::Right);
        assert!(render_text(&picker).contains("effort (high)"));
        picker.handle_key(ModalKey::Left);
        assert!(render_text(&picker).contains("effort (medium)"));
    }

    #[test]
    fn model_picker_navigation_preserves_effort_target() {
        // gpt-5.5 accepts xhigh; gemini caps at high. With xhigh chosen, arrowing
        // onto gemini shows the clamped value, but arrowing back restores xhigh
        // (navigation must not truncate the target).
        let two = vec![
            cat(ProviderId::OpenAiCodex, "gpt-5.5"),
            cat(ProviderId::Antigravity, "gemini-3.5-flash"),
        ];
        let mut picker = ModelPicker::new(
            two,
            "openai-codex/gpt-5.5",
            "openai-codex/gpt-5.5",
            ReasoningEffort::XHigh,
        );
        assert!(render_text(&picker).contains("effort (xhigh)"));
        picker.handle_key(ModalKey::Down); // onto gemini (caps at high)
        assert!(render_text(&picker).contains("effort (high)"));
        picker.handle_key(ModalKey::Up); // back to gpt-5.5
        assert!(render_text(&picker).contains("effort (xhigh)"));
    }

    #[test]
    fn model_picker_marks_active_and_labels_default_separately() {
        // Session-only switch: active (sonnet) differs from the persisted
        // default (gpt-5.5). Text columns mark both independently.
        let picker = ModelPicker::new(
            models(),
            "anthropic/claude-sonnet-4-6",
            "openai-codex/gpt-5.5",
            ReasoningEffort::Medium,
        );
        let rows: Vec<String> = picker
            .render(80)
            .into_iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        let default_row = rows
            .iter()
            .find(|l| l.contains("default"))
            .expect("a default-tagged row");
        assert!(default_row.contains("GPT 5.5"), "{default_row}");
        assert!(
            !default_row.contains('◉'),
            "default is not active: {default_row}"
        );
        let active_row = rows
            .iter()
            .find(|l| l.contains('◉') && !l.contains("SELECT"))
            .expect("an active row");
        assert!(active_row.contains("Sonnet 4.6"), "{active_row}");
        assert!(!active_row.contains("default"), "{active_row}");
    }

    #[test]
    fn model_picker_is_not_searchable() {
        let mut picker = ModelPicker::new(
            models(),
            "openai-codex/gpt-5.5",
            "openai-codex/gpt-5.5",
            ReasoningEffort::Medium,
        );
        // A non-hotkey char is ignored (no filtering); Ctrl+C and Esc cancel.
        assert!(matches!(
            picker.handle_key(ModalKey::Char('z')),
            ModalOutcome::Ignore
        ));
        assert_eq!(picker.selector.filtered_count(), 3);
        assert!(matches!(
            picker.handle_key(ModalKey::CtrlC),
            ModalOutcome::Close
        ));
    }

    #[test]
    fn trust_menu_toggles_tool_grants_and_revokes_bash_grants() {
        // No grants: write/edit rows offer a grant; Enter on write grants it.
        let mut menu = TrustMenu::new(&[], &[], &[], None);
        let text: String = menu
            .render(80)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("PROJECT PERMISSIONS"), "{text}");
        assert_eq!(
            menu.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::EditPolicy(ProjectPolicyEdit::GrantTool(
                "write".to_string()
            )))
        );
        // Esc closes.
        assert_eq!(menu.handle_key(ModalKey::Esc), ModalOutcome::Close);

        // With grants: the write row revokes, and a stored bash grant lists a
        // revoke row.
        let mut menu = TrustMenu::new(
            &["write".to_string()],
            &["cargo test".to_string()],
            &["git ".to_string()],
            None,
        );
        let text: String = menu
            .render(80)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("granted"), "{text}");
        assert!(text.contains("cargo test"), "{text}");
        assert!(text.contains("git "), "{text}");
        assert_eq!(
            menu.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::EditPolicy(ProjectPolicyEdit::RevokeTool(
                "write".to_string()
            )))
        );
        // Down past edit (row 2) to the bash grant (row 3): Enter revokes it.
        menu.handle_key(ModalKey::Down);
        menu.handle_key(ModalKey::Down);
        assert_eq!(
            menu.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::EditPolicy(ProjectPolicyEdit::RevokeBashExact(
                "cargo test".to_string()
            )))
        );
        // The prefix grant is the last row.
        menu.handle_key(ModalKey::Down);
        assert_eq!(
            menu.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::EditPolicy(
                ProjectPolicyEdit::RevokeBashPrefix("git ".to_string())
            ))
        );
    }

    #[test]
    fn trust_menu_shows_stored_sandbox_posture_read_only() {
        let menu = TrustMenu::new(&[], &[], &[], Some("restricted".to_string()));
        let text: String = menu
            .render(80)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("sandbox: restricted"), "{text}");
    }

    #[test]
    fn scoped_models_toggle_creates_explicit_list_and_applies() {
        let mut picker = ScopedModels::new(models(), None);
        // All enabled -> no checkmark column.
        assert!(picker.enabled.is_none());
        // Toggling the first row creates a one-item explicit list.
        let outcome = picker.handle_key(ModalKey::Enter);
        match outcome {
            ModalOutcome::Emit(ModalAction::ApplyScoped(Some(ids))) => {
                assert_eq!(ids.len(), 1);
            }
            other => panic!("expected ApplyScoped(Some), got {other:?}"),
        }
        assert!(picker.dirty);
    }

    #[test]
    fn scoped_models_collapse_to_none_when_all_enabled() {
        // Start with two of three enabled; toggling the third on covers every
        // candidate, so the scope folds back to None ("all enabled").
        let mut picker = ScopedModels::new(
            models(),
            Some(vec![
                "openai-codex/gpt-5.5".to_string(),
                "anthropic/claude-sonnet-4-6".to_string(),
            ]),
        );
        assert!(picker.enabled.is_some());
        // Move to the third (still-disabled) row and enable it.
        picker.handle_key(ModalKey::Down);
        picker.handle_key(ModalKey::Down);
        match picker.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::ApplyScoped(scope)) => assert_eq!(scope, None),
            other => panic!("expected ApplyScoped(None), got {other:?}"),
        }
        assert!(picker.enabled.is_none(), "all-enabled collapses to None");
        // Ctrl+A from a partial scope also collapses to None.
        let mut all = ScopedModels::new(models(), Some(vec!["openai-codex/gpt-5.5".to_string()]));
        all.handle_key(ModalKey::CtrlA);
        assert!(all.enabled.is_none());
        // But an explicit clear (Ctrl+X) stays Some([]) - a deliberate empty scope.
        let mut none = ScopedModels::new(models(), None);
        none.handle_key(ModalKey::CtrlX);
        assert_eq!(none.enabled.as_deref(), Some(&[][..]));
    }

    #[test]
    fn scoped_models_ctrl_s_persists_and_clears_dirty() {
        let mut picker =
            ScopedModels::new(models(), Some(vec!["openai-codex/gpt-5.5".to_string()]));
        picker.dirty = true;
        match picker.handle_key(ModalKey::CtrlS) {
            ModalOutcome::Emit(ModalAction::SaveScoped(Some(ids))) => {
                assert_eq!(ids, vec!["openai-codex/gpt-5.5".to_string()]);
            }
            other => panic!("expected SaveScoped, got {other:?}"),
        }
        assert!(!picker.dirty, "Ctrl+S clears dirty");
    }

    #[test]
    fn scoped_models_reorder_swaps_enabled_ids() {
        let enabled = vec![
            "openai-codex/gpt-5.5".to_string(),
            "anthropic/claude-sonnet-4-6".to_string(),
        ];
        let mut picker = ScopedModels::new(models(), Some(enabled));
        // Cursor on first enabled row; Alt+Down swaps it below the second.
        picker.handle_key(ModalKey::AltDown);
        assert_eq!(
            picker.enabled.as_ref().unwrap(),
            &vec![
                "anthropic/claude-sonnet-4-6".to_string(),
                "openai-codex/gpt-5.5".to_string(),
            ]
        );
    }

    #[test]
    fn modal_renders_through_component_trait_with_width_clamp() {
        use crate::ui::tui::Component;
        let modal = Modal::LoginMethod(MethodSelect::new());
        // The Component impl forwards to Modal::render after clamping usize->u16.
        assert_eq!(Component::render(&modal, 40), Modal::render(&modal, 40));
        // An out-of-u16-range width clamps to u16::MAX rather than overflowing.
        assert_eq!(
            Component::render(&modal, usize::from(u16::MAX) + 100),
            Modal::render(&modal, u16::MAX)
        );
    }

    #[test]
    fn method_select_chooses_subscription_or_api_key() {
        let mut menu = MethodSelect::new();
        assert_eq!(
            menu.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::ChooseLoginMethod(LoginMethod::Subscription))
        );
        menu.handle_key(ModalKey::Down);
        assert_eq!(
            menu.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::ChooseLoginMethod(LoginMethod::ApiKey))
        );
    }

    #[test]
    fn provider_select_login_cancel_returns_to_method() {
        let providers = vec![ProviderRow {
            id: "openai-codex".to_string(),
            name: "openai-codex".to_string(),
            badge: "unconfigured".to_string(),
        }];
        let mut picker =
            ProviderSelect::new(ProviderPurpose::Login, providers, "No providers available");
        assert_eq!(
            picker.handle_key(ModalKey::Esc),
            ModalOutcome::Emit(ModalAction::BackToLoginMethod)
        );
        match picker.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::BeginLogin(ProviderId::OpenAiCodex)) => {}
            other => panic!("expected BeginLogin, got {other:?}"),
        }
    }

    #[test]
    fn provider_select_logout_emits_logout_and_cancel_closes() {
        let providers = vec![ProviderRow {
            id: "anthropic".to_string(),
            name: "anthropic".to_string(),
            badge: "✓ configured".to_string(),
        }];
        let mut picker = ProviderSelect::new(
            ProviderPurpose::Logout,
            providers,
            "No providers logged in.",
        );
        match picker.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::Logout(id)) => assert_eq!(id, "anthropic"),
            other => panic!("expected Logout, got {other:?}"),
        }
        assert_eq!(picker.handle_key(ModalKey::Esc), ModalOutcome::Close);
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
        dialog.set_lines(vec![line.clone()]);

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
