//! Picker/dialog state machine (Tier 3, presentation-only).
//!
//! Every `/model`, `/scoped-models`, `/settings`, `/login`, and `/logout`
//! surface is a [`Modal`] that temporarily replaces the editor area. A modal owns
//! its [`Selector`] (or, for the OAuth dialog, just display lines), turns key
//! events into a [`ModalOutcome`], and renders itself into ratatui `Line`s. It
//! performs no side effects: confirming a row returns a [`ModalAction`] the event
//! loop applies at the safe inter-turn boundary (model/effort switch, settings
//! save, login/logout), so a picker can never switch a provider mid-stream.
//!
//! The data a modal needs (available models, auth status, provider list) is
//! gathered by the loop/cli layer and passed in at construction, keeping disk and
//! auth lookups out of per-keystroke handling and out of this presentation code.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::mimir::model_catalog::CatalogModel;
use crate::mimir::selection::{ProviderId, ReasoningEffort};
use crate::ui::selector::{Selector, SelectorItem};

/// Max rows shown in a model/scoped list (pi-mono shows 10 / 8).
const MODEL_ROWS: usize = 10;
const SCOPED_ROWS: usize = 8;
const PROVIDER_ROWS: usize = 8;

/// A key the loop forwards to the active modal. A neutral subset of crossterm
/// keys so modal handling is unit-testable without constructing terminal events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalKey {
    Up,
    Down,
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
    /// Switch to this `provider/model` id (model picker / exact `/model`).
    SelectModel(String),
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

    /// One-line title for the modal frame.
    pub(crate) fn title(&self) -> &'static str {
        match self {
            Modal::Model(_) => "Select model",
            Modal::Scoped(_) => "Model Configuration",
            Modal::Effort(_) => "Thinking Level",
            Modal::Settings(_) => "Settings",
            Modal::LoginMethod(_) => "Select authentication method",
            Modal::Providers(picker) => match picker.purpose {
                ProviderPurpose::Login => "Select provider to configure",
                ProviderPurpose::Logout => "Select provider to logout",
            },
            Modal::LoginDialog(_) => "Login",
        }
    }
}

// --- shared rendering helpers ---

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn accent() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

fn muted() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Render the shared search line + windowed rows for a [`Selector`] into `out`.
/// `empty` is the message shown when no rows match.
fn render_selector(selector: &Selector, empty: &str, out: &mut Vec<Line<'static>>) {
    if selector.searchable() {
        let search = selector.search().unwrap_or("");
        out.push(Line::from(vec![
            Span::styled("> ", dim()),
            Span::raw(search.to_string()),
        ]));
        out.push(Line::from(""));
    }
    if selector.is_empty() {
        out.push(Line::from(Span::styled(empty.to_string(), muted())));
        return;
    }
    for row in selector.visible() {
        let mut spans = Vec::new();
        let marker = if row.selected { "→ " } else { "  " };
        let base = if row.selected {
            accent()
        } else {
            Style::default()
        };
        spans.push(Span::styled(marker, base));
        if let Some(enabled) = row.item.enabled {
            spans.push(Span::styled(
                if enabled { "✓ " } else { "✗ " },
                if enabled {
                    Style::default().fg(Color::Green)
                } else {
                    muted()
                },
            ));
        }
        spans.push(Span::styled(row.item.label.clone(), base));
        if let Some(detail) = &row.item.detail {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(format!("[{detail}]"), dim()));
        }
        if let Some(trailing) = &row.item.trailing {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                trailing.clone(),
                Style::default().fg(Color::Green),
            ));
        }
        out.push(Line::from(spans));
    }
    if selector.is_scrolled() {
        out.push(Line::from(Span::styled(selector.position_label(), muted())));
    }
}

// --- model picker ---

/// Active scope of the model picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    All,
    Scoped,
}

#[derive(Debug, Clone)]
pub(crate) struct ModelPicker {
    selector: Selector,
    /// Ordered, authenticated models for the `all` scope.
    all: Vec<CatalogModel>,
    /// Ordered scoped models, when a scope is configured.
    scoped: Option<Vec<CatalogModel>>,
    scope: Scope,
    current: String,
    /// Warning shown when no scope exists (pi-mono's "/login to add providers").
    no_scope_hint: bool,
}

impl ModelPicker {
    /// Build the picker. `available` is the authenticated catalog (registry
    /// order); `scoped` is the ordered scoped set when configured; `current` is
    /// the qualified id of the active model; `search` pre-fills the filter.
    pub(crate) fn new(
        available: Vec<CatalogModel>,
        scoped: Option<Vec<CatalogModel>>,
        current: &str,
        search: &str,
    ) -> Self {
        let all = order_all(available, current);
        let scope = if scoped.is_some() {
            Scope::Scoped
        } else {
            Scope::All
        };
        let no_scope_hint = scoped.is_none();
        let mut picker = ModelPicker {
            selector: Selector::new(Vec::new(), true, true, MODEL_ROWS),
            all,
            scoped,
            scope,
            current: current.to_string(),
            no_scope_hint,
        };
        picker.rebuild();
        for c in search.chars() {
            picker.selector.push_char(c);
        }
        picker
    }

    fn models(&self) -> &[CatalogModel] {
        match self.scope {
            Scope::All => &self.all,
            Scope::Scoped => self.scoped.as_deref().unwrap_or(&self.all),
        }
    }

    fn rebuild(&mut self) {
        let current = self.current.clone();
        let items: Vec<SelectorItem> = self
            .models()
            .iter()
            .map(|model| {
                let qualified = model.qualified();
                let mut item = SelectorItem::new(qualified.clone(), model.id.clone())
                    .detail(model.provider.as_str());
                if qualified == current {
                    item = item.trailing("✓");
                }
                item
            })
            .collect();
        self.selector.replace_items(items);
        self.selector.select_id(&current);
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
            ModalKey::Tab if self.scoped.is_some() => {
                self.scope = match self.scope {
                    Scope::All => Scope::Scoped,
                    Scope::Scoped => Scope::All,
                };
                self.rebuild();
                ModalOutcome::Redraw
            }
            ModalKey::Enter => match self.selector.selected_id() {
                Some(id) => ModalOutcome::Emit(ModalAction::SelectModel(id.to_string())),
                None => ModalOutcome::Ignore,
            },
            ModalKey::Esc => ModalOutcome::Close,
            // Per spec, the model picker cancels on Ctrl+C (search-clearing on
            // Ctrl+C is a scoped-models-only behavior).
            ModalKey::CtrlC => ModalOutcome::Close,
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
        let mut out = Vec::new();
        if self.scoped.is_some() {
            let (all_style, scoped_style) = match self.scope {
                Scope::All => (accent(), muted()),
                Scope::Scoped => (muted(), accent()),
            };
            out.push(Line::from(vec![
                Span::styled("Scope: ", dim()),
                Span::styled("all", all_style),
                Span::styled(" | ", dim()),
                Span::styled("scoped", scoped_style),
            ]));
            out.push(Line::from(Span::styled("tab scope (all/scoped)", dim())));
            out.push(Line::from(""));
        } else if self.no_scope_hint {
            out.push(Line::from(Span::styled(
                "Only showing models from configured providers. Use /login to add providers.",
                muted(),
            )));
        }
        render_selector(&self.selector, "No matching models", &mut out);
        if let Some(item) = self.selector.selected() {
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Model Name: ", dim()),
                Span::raw(crate::mimir::model_catalog::display_name(&item.id)),
            ]));
        }
        let _ = width;
        out
    }
}

/// Order the `all` scope: current model first, then by provider name, preserving
/// registry order within a provider (pi-mono's ordering).
fn order_all(models: Vec<CatalogModel>, current: &str) -> Vec<CatalogModel> {
    let mut ordered: Vec<CatalogModel> = Vec::with_capacity(models.len());
    if let Some(found) = models.iter().find(|model| model.qualified() == current) {
        ordered.push(found.clone());
    }
    let mut rest: Vec<CatalogModel> = models
        .into_iter()
        .filter(|model| model.qualified() != current)
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
        let mut out = Vec::new();
        out.push(Line::from(Span::styled(
            "Session-only. Ctrl+S to save to settings.",
            dim(),
        )));
        render_selector(&self.selector, "No matching models", &mut out);
        let count = if self.enabled.is_none() {
            "all enabled".to_string()
        } else {
            format!("{}/{} enabled", self.enabled_count(), self.candidates.len())
        };
        let mut footer = vec![
            Span::styled(
                "Enter toggle  Ctrl+A all  Ctrl+X clear  Ctrl+P provider  Alt+Up/Down reorder  Ctrl+S save  ",
                dim(),
            ),
            Span::raw(count),
        ];
        if self.dirty {
            footer.push(Span::styled(
                "  (unsaved)",
                Style::default().fg(Color::Yellow),
            ));
        }
        out.push(Line::from(footer));
        let _ = width;
        out
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
                    item = item.trailing("✓");
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
        let mut out = vec![Line::from(Span::styled(
            "Select reasoning depth for thinking-capable models",
            dim(),
        ))];
        render_selector(&self.selector, "No levels", &mut out);
        let _ = width;
        out
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
        let mut out = vec![Line::from(Span::styled("Settings", dim()))];
        render_selector(&self.selector, "No settings", &mut out);
        let _ = width;
        out
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
        let mut out = vec![Line::from(Span::styled(
            "Select authentication method:",
            dim(),
        ))];
        render_selector(&self.selector, "No methods", &mut out);
        let _ = width;
        out
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
        let title = match self.purpose {
            ProviderPurpose::Login => "Select provider to configure:",
            ProviderPurpose::Logout => "Select provider to logout:",
        };
        let mut out = vec![Line::from(Span::styled(title, dim()))];
        render_selector(&self.selector, &self.empty, &mut out);
        let _ = width;
        out
    }
}

// --- OAuth login dialog (display-only) ---

#[derive(Debug, Clone)]
pub(crate) struct LoginDialog {
    provider_name: String,
    lines: Vec<String>,
}

impl LoginDialog {
    pub(crate) fn new(provider_name: &str) -> Self {
        LoginDialog {
            provider_name: provider_name.to_string(),
            lines: vec!["Starting login...".to_string()],
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

    fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            // Cancel aborts the in-flight login (the loop drops the task).
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        let mut out = vec![Line::from(vec![
            Span::styled("Login to ", dim()),
            Span::raw(self.provider_name.clone()),
        ])];
        for line in &self.lines {
            out.push(Line::from(Span::raw(line.clone())));
        }
        out.push(Line::from(Span::styled("Esc to cancel", dim())));
        let _ = width;
        out
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

    #[test]
    fn model_picker_enter_emits_selected_qualified_id() {
        let mut picker = ModelPicker::new(models(), None, "openai-codex/gpt-5.5", "");
        // Current is first and marked; move to the next and select it.
        picker.handle_key(ModalKey::Down);
        match picker.handle_key(ModalKey::Enter) {
            ModalOutcome::Emit(ModalAction::SelectModel(id)) => {
                assert_eq!(id, "anthropic/claude-sonnet-4-6");
            }
            other => panic!("expected SelectModel, got {other:?}"),
        }
    }

    #[test]
    fn model_picker_render_matches_pi_mono_layout() {
        // Scoped set present so the scope header renders; current is gpt-5.5.
        let scoped = vec![
            cat(ProviderId::OpenAiCodex, "gpt-5.5"),
            cat(ProviderId::Anthropic, "claude-sonnet-4-6"),
        ];
        let picker = ModelPicker::new(models(), Some(scoped), "openai-codex/gpt-5.5", "");
        let text: String = picker
            .render(80)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Scope hint on its own line, '>' search prompt, and a display-name footer.
        assert!(text.contains("tab scope (all/scoped)"), "{text}");
        assert!(text.contains("> "), "{text}");
        assert!(text.contains("gpt-5.5"), "{text}");
        assert!(text.contains("Model Name: GPT-5.5"), "{text}");
        // The old inline hint and 'Model:' label are gone.
        assert!(!text.contains("Tab to switch"), "{text}");
        assert!(!text.contains("search: "), "{text}");
    }

    #[test]
    fn model_picker_ctrl_c_cancels_even_with_active_search() {
        // Spec: the model picker cancels on Ctrl+C; it does not clear the search
        // first (that is a scoped-models-only behavior).
        let mut picker = ModelPicker::new(models(), None, "openai-codex/gpt-5.5", "clau");
        assert_eq!(picker.selector.search(), Some("clau"));
        assert!(matches!(
            picker.handle_key(ModalKey::CtrlC),
            ModalOutcome::Close
        ));
    }

    #[test]
    fn model_picker_search_prefill_and_no_match_message() {
        let picker = ModelPicker::new(models(), None, "openai-codex/gpt-5.5", "bad-prefix");
        assert_eq!(picker.selector.search(), Some("bad-prefix"));
        assert!(picker.selector.is_empty());
        let lines = picker.render(80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("No matching models"), "{text}");
    }

    #[test]
    fn model_picker_tab_toggles_scope_only_when_scoped_exists() {
        let scoped = vec![cat(ProviderId::Anthropic, "claude-sonnet-4-6")];
        let mut picker = ModelPicker::new(models(), Some(scoped), "openai-codex/gpt-5.5", "");
        // Starts in scoped scope: only one model visible.
        assert_eq!(picker.selector.filtered_count(), 1);
        picker.handle_key(ModalKey::Tab); // -> all
        assert_eq!(picker.selector.filtered_count(), 3);
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
        let mut dialog = LoginDialog::new("openai-codex");
        dialog.set_lines(vec!["Open: https://example".to_string()]);
        assert_eq!(dialog.handle_key(ModalKey::CtrlC), ModalOutcome::Close);
    }
}
