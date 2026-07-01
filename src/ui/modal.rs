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
    Tab,
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
    /// Apply this scope to the live session immediately (every scoped edit).
    /// `None` clears the scope (cycle all authenticated models).
    ApplyScoped(Option<Vec<String>>),
    /// Persist the scope to settings (Ctrl+S); the picker stays open.
    SaveScoped(Option<Vec<String>>),
    /// Apply this effort/thinking level.
    SetEffort(ReasoningEffort),
    /// Settings menu -> open the thinking-level submenu.
    OpenEffortPicker,
    /// `/login` method chosen -> open the matching provider selector.
    ChooseLoginMethod(LoginMethod),
    /// Begin an OAuth/subscription login for this provider.
    BeginLogin(ProviderId),
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
    Scoped(ScopedModels),
    Effort(EffortPicker),
    Settings(SettingsMenu),
    LoginMethod(MethodSelect),
    Providers(ProviderSelect),
    LoginDialog(LoginDialog),
}

impl Modal {
    pub(crate) fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match self {
            Modal::Model(picker) => picker.handle_key(key),
            Modal::Scoped(picker) => picker.handle_key(key),
            Modal::Effort(picker) => picker.handle_key(key),
            Modal::Settings(menu) => menu.handle_key(key),
            Modal::LoginMethod(menu) => menu.handle_key(key),
            Modal::Providers(picker) => picker.handle_key(key),
            Modal::LoginDialog(dialog) => dialog.handle_key(key),
        }
    }

    pub(crate) fn render(&self, width: u16) -> Vec<Line<'static>> {
        match self {
            Modal::Model(picker) => picker.render(width),
            Modal::Scoped(picker) => picker.render(width),
            Modal::Effort(picker) => picker.render(width),
            Modal::Settings(menu) => menu.render(width),
            Modal::LoginMethod(menu) => menu.render(width),
            Modal::Providers(picker) => picker.render(width),
            Modal::LoginDialog(dialog) => dialog.render(width),
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

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// Render the shared search line + windowed rows for a [`Selector`] as overlay
/// rows: `(line, selected)` pairs for [`overlay_box`], which gives the selected
/// row the surface fill (never a colored accent). The selected label is bold;
/// metadata stays muted; an enabled/disabled mark uses the `◉`/`○` glyphs from
/// the closed vocabulary (never `[x]`). `empty` is the no-match message.
fn selector_rows(selector: &Selector, empty: &str) -> Vec<(Line<'static>, bool)> {
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
                if enabled { "◉ " } else { "○ " },
                if enabled {
                    Style::default().fg(crate::ui::palette::ORANGE)
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
                Span::styled("◉ ", Style::default().fg(crate::ui::palette::ORANGE))
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
            self.display_effort().as_str()
        );
        crate::ui::tui::overlay_box(
            Some("Select model"),
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
        crate::ui::tui::overlay_box(
            Some("Scoped models"),
            rows,
            Some(&footer),
            usize::from(width),
        )
    }
}

// --- effort picker ---

#[derive(Debug, Clone)]
pub(crate) struct EffortPicker {
    selector: Selector,
    levels: Vec<ReasoningEffort>,
}

impl EffortPicker {
    pub(crate) fn new(levels: Vec<ReasoningEffort>, current: ReasoningEffort) -> Self {
        let items: Vec<SelectorItem> = levels
            .iter()
            .map(|level| {
                let mut item =
                    SelectorItem::new(level.as_str(), level.as_str()).detail(level.description());
                if *level == current {
                    item = item.trailing("current");
                }
                item
            })
            .collect();
        let mut selector = Selector::new(items, false, true, 8);
        selector.select_id(current.as_str());
        EffortPicker { selector, levels }
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
            ModalKey::Enter => match self.selected_level() {
                Some(level) => ModalOutcome::Emit(ModalAction::SetEffort(level)),
                None => ModalOutcome::Ignore,
            },
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    fn selected_level(&self) -> Option<ReasoningEffort> {
        let id = self.selector.selected_id()?;
        self.levels
            .iter()
            .copied()
            .find(|level| level.as_str() == id)
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let rows = selector_rows(&self.selector, "No levels");
        crate::ui::tui::overlay_box(
            Some("Reasoning effort"),
            rows,
            Some("↑↓ move · ↵ select · esc cancel"),
            usize::from(width),
        )
    }
}

// --- settings menu ---

#[derive(Debug, Clone)]
pub(crate) struct SettingsMenu {
    selector: Selector,
}

impl SettingsMenu {
    pub(crate) fn new(current_effort: ReasoningEffort) -> Self {
        let item = SelectorItem::new("thinking", "Thinking level")
            .detail(format!("current: {}", current_effort.as_str()));
        SettingsMenu {
            selector: Selector::new(vec![item], false, false, 8),
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
                Some("thinking") => ModalOutcome::Emit(ModalAction::OpenEffortPicker),
                _ => ModalOutcome::Ignore,
            },
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let rows = selector_rows(&self.selector, "No settings");
        crate::ui::tui::overlay_box(
            Some("Settings"),
            rows,
            Some("↑↓ move · ↵ select · esc cancel"),
            usize::from(width),
        )
    }
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
        crate::ui::tui::overlay_box(
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
            ProviderPurpose::Login => ModalOutcome::Emit(ModalAction::BackToLoginMethod),
            ProviderPurpose::Logout => ModalOutcome::Close,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let rows = selector_rows(&self.selector, &self.empty);
        let title = match self.purpose {
            ProviderPurpose::Login => "Select provider",
            ProviderPurpose::Logout => "Logout",
        };
        crate::ui::tui::overlay_box(
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
        crate::ui::tui::overlay_box(Some(&title), rows, Some("esc cancel"), usize::from(width))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat(provider: ProviderId, id: &str) -> CatalogModel {
        CatalogModel {
            provider,
            id: id.to_string(),
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
        // Bordered box, uppercase title, ◉ current marker, provider meta.
        assert!(text.contains("SELECT MODEL"), "{text}");
        assert!(text.contains('┌') && text.contains('└'), "{text}");
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
                    .any(|span| span.style.bg == Some(crate::ui::palette::SURFACE))
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
        assert!(text.contains("←→ effort (xhigh)"), "{text}");
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
    fn effort_picker_selects_level() {
        let levels = vec![
            ReasoningEffort::Off,
            ReasoningEffort::Low,
            ReasoningEffort::High,
        ];
        let mut picker = EffortPicker::new(levels, ReasoningEffort::Low);
        // Preselected on Low; move up to Off and select.
        picker.handle_key(ModalKey::Up);
        match picker.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::SetEffort(level)) => {
                assert_eq!(level, ReasoningEffort::Off);
            }
            other => panic!("expected SetEffort, got {other:?}"),
        }
    }

    #[test]
    fn modal_renders_through_component_trait_with_width_clamp() {
        use crate::ui::tui::Component;
        let modal = Modal::Effort(EffortPicker::new(
            vec![ReasoningEffort::Low, ReasoningEffort::High],
            ReasoningEffort::Low,
        ));
        // The Component impl forwards to Modal::render after clamping usize->u16.
        assert_eq!(Component::render(&modal, 40), Modal::render(&modal, 40));
        // An out-of-u16-range width clamps to u16::MAX rather than overflowing.
        assert_eq!(
            Component::render(&modal, usize::from(u16::MAX) + 100),
            Modal::render(&modal, u16::MAX)
        );
    }

    #[test]
    fn settings_menu_opens_effort_picker() {
        let mut menu = SettingsMenu::new(ReasoningEffort::Medium);
        assert_eq!(
            menu.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::OpenEffortPicker)
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
        // Strip the box chrome (│ + one padding cell each side) before joining.
        let joined: String = texts
            .iter()
            .filter_map(|text| {
                let inner = text.strip_prefix('│')?.strip_suffix('│')?;
                Some(inner.trim_matches(' ').to_string())
            })
            .collect();
        assert!(
            joined.contains(url),
            "wrapped rows must contain the full URL: {joined}"
        );
    }
}
