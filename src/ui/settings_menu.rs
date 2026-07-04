//! Settings menu tree (Tier 3, presentation-only, harness-free).
//!
//! The `/settings` surface is a category list; each category opens a submenu of
//! rows that either (a) hand off to an existing modal (model picker, effort
//! picker, `/trust`, `/scoped-models`, `/login`), (b) open a small enum picker,
//! (c) open a free-text/numeric entry, or (d) toggle a bool in place. Every
//! widget here is pure: it turns a [`ModalKey`] into a [`ModalOutcome`], and the
//! loop ([`crate::ui::picker::apply_action`]) performs the disk writes and opens
//! existing modals at the safe inter-turn boundary. Navigation between settings
//! surfaces reuses the existing "Replace the modal" pattern
//! ([`crate::ui::picker::ActionResult::Replace`]).
//!
//! All writes go to the user-global settings file via `config::save_*`;
//! global-vs-project scope governs only load/merge precedence in
//! [`crate::config::Settings::merged_with`]. Two fields are GLOBAL-ONLY so a
//! cloned project cannot lower posture: `defaultApproval` and
//! `promptCacheRetention`.

use ratatui::text::{Line, Span};

use crate::ui::modal::{ModalAction, ModalKey, ModalOutcome, dim, selector_rows};
use crate::ui::selector::{Selector, SelectorItem};

/// A top-level settings category. Selecting one opens its submenu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Category {
    ModelReasoning,
    Display,
    Approvals,
    Runtime,
    Verification,
    Providers,
}

impl Category {
    /// Every category, in menu order.
    pub(crate) const ALL: [Category; 6] = [
        Category::ModelReasoning,
        Category::Display,
        Category::Approvals,
        Category::Runtime,
        Category::Verification,
        Category::Providers,
    ];

    /// Stable selector key.
    fn id(self) -> &'static str {
        match self {
            Category::ModelReasoning => "model",
            Category::Display => "display",
            Category::Approvals => "approvals",
            Category::Runtime => "runtime",
            Category::Verification => "verification",
            Category::Providers => "providers",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Category::ModelReasoning => "Model & reasoning",
            Category::Display => "Display",
            Category::Approvals => "Approvals & trust",
            Category::Runtime => "Runtime",
            Category::Verification => "Verification",
            Category::Providers => "Providers & scope",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Category::ModelReasoning => "default model, reasoning effort",
            Category::Display => "screen mode, scrolling, accessibility",
            Category::Approvals => "startup posture, project permissions",
            Category::Runtime => "context budget, tool loop, prompt cache",
            Category::Verification => "post-change checks",
            Category::Providers => "scoped models, worktrees, login",
        }
    }
}

/// A single persisted setting reachable from a submenu. Each field knows its
/// parent [`Category`] (for back-navigation and post-save refresh) and its
/// input kind (enum options / numeric bounds / free text / bool).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    AltScreen,
    ScrollSpeed,
    ReducedMotion,
    DefaultApproval,
    ContextTokenBudget,
    MaxToolRoundtrips,
    PromptCacheRetention,
    VerifyCommand,
    VerifyMaxAttempts,
    WorktreeRoot,
}

/// How a [`Field`] is edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FieldKind {
    /// One of a fixed closed vocabulary.
    Enum { options: &'static [&'static str] },
    /// A positive integer clamped to `[min, max]`; `allow_empty` clears the key.
    Numeric {
        min: u64,
        max: u64,
        allow_empty: bool,
    },
    /// Free text; `allow_empty` clears the key.
    Text { allow_empty: bool },
    /// A boolean toggled in place from the submenu (no separate widget).
    Bool,
}

impl Field {
    pub(crate) fn category(self) -> Category {
        match self {
            Field::AltScreen | Field::ScrollSpeed | Field::ReducedMotion => Category::Display,
            Field::DefaultApproval => Category::Approvals,
            Field::ContextTokenBudget | Field::MaxToolRoundtrips | Field::PromptCacheRetention => {
                Category::Runtime
            }
            Field::VerifyCommand | Field::VerifyMaxAttempts => Category::Verification,
            Field::WorktreeRoot => Category::Providers,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Field::AltScreen => "Alt-screen",
            Field::ScrollSpeed => "Scroll speed",
            Field::ReducedMotion => "Reduced motion",
            Field::DefaultApproval => "Default approval",
            Field::ContextTokenBudget => "Context token budget",
            Field::MaxToolRoundtrips => "Max tool round-trips",
            Field::PromptCacheRetention => "Prompt cache retention",
            Field::VerifyCommand => "Verify command",
            Field::VerifyMaxAttempts => "Verify max attempts",
            Field::WorktreeRoot => "Worktree root",
        }
    }

    pub(crate) fn kind(self) -> FieldKind {
        match self {
            Field::AltScreen => FieldKind::Enum {
                options: &["auto", "always", "never"],
            },
            Field::DefaultApproval => FieldKind::Enum {
                options: &["strict", "auto", "never"],
            },
            Field::PromptCacheRetention => FieldKind::Enum {
                options: &["none", "short", "long"],
            },
            Field::ScrollSpeed => FieldKind::Numeric {
                min: 1,
                max: 100,
                allow_empty: false,
            },
            Field::ContextTokenBudget => FieldKind::Numeric {
                min: 1_000,
                max: 100_000_000,
                allow_empty: false,
            },
            Field::MaxToolRoundtrips => FieldKind::Numeric {
                min: 1,
                max: 1_000,
                allow_empty: true,
            },
            Field::VerifyMaxAttempts => FieldKind::Numeric {
                min: 1,
                max: 10,
                allow_empty: false,
            },
            Field::VerifyCommand | Field::WorktreeRoot => FieldKind::Text { allow_empty: true },
            Field::ReducedMotion => FieldKind::Bool,
        }
    }
}

/// Current persisted values, read once by the loop from [`crate::config::Settings`]
/// (plus the live selection for model/reasoning) so the menus can show the
/// current value as muted metadata. Pure data, so the menu builders stay
/// harness-free and unit-testable.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub(crate) default_model: String,
    pub(crate) default_reasoning: String,
    pub(crate) alt_screen: String,
    pub(crate) scroll_speed: u16,
    pub(crate) reduced_motion: bool,
    pub(crate) default_approval: String,
    pub(crate) context_token_budget: u64,
    pub(crate) max_tool_roundtrips: Option<usize>,
    pub(crate) prompt_cache_retention: String,
    pub(crate) verify_command: Option<String>,
    pub(crate) verify_max_attempts: u32,
    pub(crate) worktree_root: Option<String>,
}

impl Snapshot {
    /// The current value of `field` as a display string (bools as `on`/`off`,
    /// cleared/absent as a muted placeholder).
    fn value(&self, field: Field) -> String {
        match field {
            Field::AltScreen => self.alt_screen.clone(),
            Field::ScrollSpeed => self.scroll_speed.to_string(),
            Field::ReducedMotion => on_off(self.reduced_motion),
            Field::DefaultApproval => self.default_approval.clone(),
            Field::ContextTokenBudget => self.context_token_budget.to_string(),
            Field::MaxToolRoundtrips => match self.max_tool_roundtrips {
                Some(cap) => cap.to_string(),
                None => "unbounded".to_string(),
            },
            Field::PromptCacheRetention => self.prompt_cache_retention.clone(),
            Field::VerifyCommand => self
                .verify_command
                .clone()
                .unwrap_or_else(|| "not set".to_string()),
            Field::VerifyMaxAttempts => self.verify_max_attempts.to_string(),
            Field::WorktreeRoot => self
                .worktree_root
                .clone()
                .unwrap_or_else(|| "../wt (default)".to_string()),
        }
    }

    /// The pre-filled text for an entry dialog: the raw current value, or empty
    /// when the key is cleared/unset (so the placeholder does not seed the input).
    fn entry_seed(&self, field: Field) -> String {
        match field {
            Field::ScrollSpeed => self.scroll_speed.to_string(),
            Field::ContextTokenBudget => self.context_token_budget.to_string(),
            Field::MaxToolRoundtrips => self
                .max_tool_roundtrips
                .map(|c| c.to_string())
                .unwrap_or_default(),
            Field::VerifyMaxAttempts => self.verify_max_attempts.to_string(),
            Field::VerifyCommand => self.verify_command.clone().unwrap_or_default(),
            Field::WorktreeRoot => self.worktree_root.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }
}

fn on_off(value: bool) -> String {
    if value {
        "on".to_string()
    } else {
        "off".to_string()
    }
}

/// One submenu row: a display item plus the action its Enter emits.
#[derive(Debug, Clone)]
struct Row {
    item: SelectorItem,
    action: ModalAction,
}

/// A field row whose current value is shown as muted detail. Bool fields toggle
/// in place (Enter emits the flipped [`ModalAction::SaveSetting`]); enum/entry
/// fields open the matching widget.
fn field_row(field: Field, snapshot: &Snapshot) -> Row {
    let action = match field.kind() {
        FieldKind::Bool => {
            let current = matches!(field, Field::ReducedMotion if snapshot.reduced_motion);
            ModalAction::SaveSetting {
                field,
                value: Some((!current).to_string()),
            }
        }
        FieldKind::Enum { .. } => ModalAction::OpenSettingsEnum(field),
        FieldKind::Numeric { .. } | FieldKind::Text { .. } => ModalAction::OpenSettingsEntry(field),
    };
    Row {
        item: SelectorItem::new(field.label(), field.label()).detail(snapshot.value(field)),
        action,
    }
}

/// A row that hands off to an existing modal.
fn open_row(id: &str, label: &str, detail: Option<&str>, action: ModalAction) -> Row {
    let mut item = SelectorItem::new(id, label);
    if let Some(detail) = detail {
        item = item.detail(detail);
    }
    Row { item, action }
}

/// The rows shown for a category, populated with current values.
fn rows_for(category: Category, snapshot: &Snapshot) -> Vec<Row> {
    match category {
        Category::ModelReasoning => vec![
            open_row(
                "model",
                "Default model",
                Some(&snapshot.default_model),
                ModalAction::OpenModelPicker,
            ),
            open_row(
                "reasoning",
                "Default reasoning",
                Some(&snapshot.default_reasoning),
                ModalAction::OpenEffortPicker,
            ),
        ],
        Category::Display => vec![
            field_row(Field::AltScreen, snapshot),
            field_row(Field::ScrollSpeed, snapshot),
            field_row(Field::ReducedMotion, snapshot),
        ],
        Category::Approvals => vec![
            field_row(Field::DefaultApproval, snapshot),
            open_row(
                "trust",
                "Project permissions",
                Some("per-tool + bash grants"),
                ModalAction::OpenTrustMenu,
            ),
        ],
        Category::Runtime => vec![
            field_row(Field::ContextTokenBudget, snapshot),
            field_row(Field::MaxToolRoundtrips, snapshot),
            field_row(Field::PromptCacheRetention, snapshot),
        ],
        Category::Verification => vec![
            field_row(Field::VerifyCommand, snapshot),
            field_row(Field::VerifyMaxAttempts, snapshot),
        ],
        Category::Providers => vec![
            open_row(
                "scoped",
                "Scoped models",
                Some("cycle scope"),
                ModalAction::OpenScopedModels,
            ),
            field_row(Field::WorktreeRoot, snapshot),
            open_row(
                "login",
                "Login / providers",
                Some("add or remove a provider"),
                ModalAction::OpenLoginMethod,
            ),
        ],
    }
}

// --- category list (top level) ---

#[derive(Debug, Clone)]
pub(crate) struct SettingsMenu {
    selector: Selector,
}

impl SettingsMenu {
    pub(crate) fn new() -> Self {
        let items: Vec<SelectorItem> = Category::ALL
            .iter()
            .map(|category| {
                SelectorItem::new(category.id(), category.title()).detail(category.description())
            })
            .collect();
        SettingsMenu {
            selector: Selector::new(items, false, true, 8),
        }
    }

    fn selected_category(&self) -> Option<Category> {
        let id = self.selector.selected_id()?;
        Category::ALL.iter().copied().find(|c| c.id() == id)
    }

    pub(crate) fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::Up => {
                self.selector.up();
                ModalOutcome::Redraw
            }
            ModalKey::Down => {
                self.selector.down();
                ModalOutcome::Redraw
            }
            ModalKey::Enter | ModalKey::Right => match self.selected_category() {
                Some(category) => ModalOutcome::Emit(ModalAction::OpenSettingsCategory(category)),
                None => ModalOutcome::Ignore,
            },
            ModalKey::Esc | ModalKey::CtrlC => ModalOutcome::Close,
            _ => ModalOutcome::Ignore,
        }
    }

    pub(crate) fn render(&self, width: u16) -> Vec<Line<'static>> {
        let rows = selector_rows(&self.selector, "No settings");
        crate::ui::tui::overlay_box(
            Some("Settings"),
            rows,
            Some("\u{2191}\u{2193} move \u{00b7} \u{21b5} open \u{00b7} esc close"),
            usize::from(width),
        )
    }
}

// --- category submenu ---

#[derive(Debug, Clone)]
pub(crate) struct SubMenu {
    category: Category,
    selector: Selector,
    rows: Vec<Row>,
}

impl SubMenu {
    pub(crate) fn new(category: Category, snapshot: &Snapshot) -> Self {
        let rows = rows_for(category, snapshot);
        let items = rows.iter().map(|row| row.item.clone()).collect();
        SubMenu {
            category,
            selector: Selector::new(items, false, true, 8),
            rows,
        }
    }

    fn selected_action(&self) -> Option<ModalAction> {
        let id = self.selector.selected_id()?;
        self.rows
            .iter()
            .find(|row| row.item.id == id)
            .map(|row| row.action.clone())
    }

    pub(crate) fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::Up => {
                self.selector.up();
                ModalOutcome::Redraw
            }
            ModalKey::Down => {
                self.selector.down();
                ModalOutcome::Redraw
            }
            ModalKey::Enter | ModalKey::Right => match self.selected_action() {
                Some(action) => ModalOutcome::Emit(action),
                None => ModalOutcome::Ignore,
            },
            // Back out to the category list.
            ModalKey::Esc | ModalKey::CtrlC | ModalKey::Left => {
                ModalOutcome::Emit(ModalAction::OpenSettingsRoot)
            }
            _ => ModalOutcome::Ignore,
        }
    }

    pub(crate) fn render(&self, width: u16) -> Vec<Line<'static>> {
        let rows = selector_rows(&self.selector, "No settings");
        crate::ui::tui::overlay_box(
            Some(self.category.title()),
            rows,
            Some(
                "\u{2191}\u{2193} move \u{00b7} \u{21b5} select \u{00b7} \u{2190} back \u{00b7} esc close",
            ),
            usize::from(width),
        )
    }
}

// --- enum picker ---

#[derive(Debug, Clone)]
pub(crate) struct EnumMenu {
    field: Field,
    selector: Selector,
}

impl EnumMenu {
    pub(crate) fn new(field: Field, snapshot: &Snapshot) -> Self {
        let options = match field.kind() {
            FieldKind::Enum { options } => options,
            // A non-enum field never reaches here (the submenu only routes enum
            // fields to this widget); fall back to an empty list defensively.
            _ => &[],
        };
        let current = snapshot.value(field);
        let items: Vec<SelectorItem> = options
            .iter()
            .map(|option| {
                let mut item = SelectorItem::new(*option, *option);
                if *option == current {
                    item = item.trailing("current");
                }
                item
            })
            .collect();
        let mut selector = Selector::new(items, false, true, 8);
        selector.select_id(&current);
        EnumMenu { field, selector }
    }

    pub(crate) fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
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
                Some(value) => ModalOutcome::Emit(ModalAction::SaveSetting {
                    field: self.field,
                    value: Some(value.to_string()),
                }),
                None => ModalOutcome::Ignore,
            },
            ModalKey::Esc | ModalKey::CtrlC | ModalKey::Left => {
                ModalOutcome::Emit(ModalAction::OpenSettingsCategory(self.field.category()))
            }
            _ => ModalOutcome::Ignore,
        }
    }

    pub(crate) fn render(&self, width: u16) -> Vec<Line<'static>> {
        let rows = selector_rows(&self.selector, "No options");
        let title = format!(
            "{} \u{00b7} {}",
            self.field.category().title(),
            self.field.label()
        );
        crate::ui::tui::overlay_box(
            Some(&title),
            rows,
            Some("\u{2191}\u{2193} move \u{00b7} \u{21b5} select \u{00b7} \u{2190} back"),
            usize::from(width),
        )
    }
}

// --- text / numeric entry ---

#[derive(Debug, Clone)]
pub(crate) struct EntryDialog {
    field: Field,
    input: String,
    error: Option<String>,
}

impl EntryDialog {
    pub(crate) fn new(field: Field, snapshot: &Snapshot) -> Self {
        EntryDialog {
            field,
            input: snapshot.entry_seed(field),
            error: None,
        }
    }

    pub(crate) fn push_str(&mut self, text: &str) {
        self.input.push_str(text.trim_end_matches(['\r', '\n']));
        self.error = None;
    }

    /// Validate the buffer and, when valid, emit the save. Numeric fields reject
    /// non-numbers and clamp to their bounds; an empty buffer clears the key when
    /// the field allows it, otherwise it is rejected with an inline error.
    fn confirm(&mut self) -> ModalOutcome {
        let trimmed = self.input.trim();
        match self.field.kind() {
            FieldKind::Numeric {
                min,
                max,
                allow_empty,
            } => {
                if trimmed.is_empty() {
                    if allow_empty {
                        return self.emit(None);
                    }
                    self.error = Some("enter a number".to_string());
                    return ModalOutcome::Redraw;
                }
                match trimmed.parse::<u64>() {
                    Ok(value) => self.emit(Some(value.clamp(min, max).to_string())),
                    Err(_) => {
                        self.error = Some("must be a whole number".to_string());
                        ModalOutcome::Redraw
                    }
                }
            }
            FieldKind::Text { allow_empty } => {
                if trimmed.is_empty() {
                    if allow_empty {
                        return self.emit(None);
                    }
                    self.error = Some("cannot be empty".to_string());
                    return ModalOutcome::Redraw;
                }
                self.emit(Some(trimmed.to_string()))
            }
            // Enum/Bool fields never open an entry dialog.
            FieldKind::Enum { .. } | FieldKind::Bool => ModalOutcome::Ignore,
        }
    }

    fn emit(&self, value: Option<String>) -> ModalOutcome {
        ModalOutcome::Emit(ModalAction::SaveSetting {
            field: self.field,
            value,
        })
    }

    pub(crate) fn handle_key(&mut self, key: ModalKey) -> ModalOutcome {
        match key {
            ModalKey::Enter => self.confirm(),
            ModalKey::Backspace => {
                self.input.pop();
                self.error = None;
                ModalOutcome::Redraw
            }
            ModalKey::Char(ch) => {
                self.input.push(ch);
                self.error = None;
                ModalOutcome::Redraw
            }
            ModalKey::Esc | ModalKey::CtrlC => {
                ModalOutcome::Emit(ModalAction::OpenSettingsCategory(self.field.category()))
            }
            _ => ModalOutcome::Ignore,
        }
    }

    pub(crate) fn render(&self, width: u16) -> Vec<Line<'static>> {
        let allow_empty = matches!(
            self.field.kind(),
            FieldKind::Numeric {
                allow_empty: true,
                ..
            } | FieldKind::Text { allow_empty: true }
        );
        let prompt = match self.field.kind() {
            FieldKind::Numeric { min, max, .. } => {
                format!("Enter a number ({min}\u{2013}{max}), then Enter.")
            }
            _ => "Enter a value, then Enter.".to_string(),
        };
        let mut rows: Vec<(Line<'static>, bool)> = vec![
            (Line::from(Span::styled(prompt, dim())), false),
            (Line::from(Span::raw(format!("> {}", self.input))), false),
        ];
        if let Some(error) = &self.error {
            rows.push((Line::from(Span::styled(error.clone(), dim())), false));
        }
        let footer = if allow_empty {
            "\u{21b5} save \u{00b7} \u{2190} back \u{00b7} empty clears"
        } else {
            "\u{21b5} save \u{00b7} \u{2190} back"
        };
        let title = format!(
            "{} \u{00b7} {}",
            self.field.category().title(),
            self.field.label()
        );
        crate::ui::tui::overlay_box(Some(&title), rows, Some(footer), usize::from(width))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            default_model: "openai-codex/gpt-5.5".to_string(),
            default_reasoning: "medium".to_string(),
            alt_screen: "auto".to_string(),
            scroll_speed: 3,
            reduced_motion: false,
            default_approval: "strict".to_string(),
            context_token_budget: 128_000,
            max_tool_roundtrips: None,
            prompt_cache_retention: "short".to_string(),
            verify_command: None,
            verify_max_attempts: 3,
            worktree_root: None,
        }
    }

    #[test]
    fn category_menu_opens_the_selected_category() {
        let mut menu = SettingsMenu::new();
        // First row is Model & reasoning.
        assert_eq!(
            menu.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::OpenSettingsCategory(Category::ModelReasoning))
        );
        // Move to Display and open it.
        menu.handle_key(ModalKey::Down);
        assert_eq!(
            menu.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::OpenSettingsCategory(Category::Display))
        );
        // Esc at the top closes.
        assert_eq!(menu.handle_key(ModalKey::Esc), ModalOutcome::Close);
    }

    #[test]
    fn submenu_leaves_emit_the_intended_actions() {
        let snap = snapshot();
        // Display: alt-screen -> enum, scroll -> entry, reduced-motion -> toggle.
        let mut display = SubMenu::new(Category::Display, &snap);
        assert_eq!(
            display.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::OpenSettingsEnum(Field::AltScreen))
        );
        display.handle_key(ModalKey::Down);
        assert_eq!(
            display.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::OpenSettingsEntry(Field::ScrollSpeed))
        );
        display.handle_key(ModalKey::Down);
        // Reduced motion currently off -> toggling saves "true".
        assert_eq!(
            display.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::ReducedMotion,
                value: Some("true".to_string()),
            })
        );
        // Left backs out to the category list.
        assert_eq!(
            display.handle_key(ModalKey::Left),
            ModalOutcome::Emit(ModalAction::OpenSettingsRoot)
        );
    }

    #[test]
    fn submenu_open_rows_reuse_existing_modals() {
        let snap = snapshot();
        let mut model = SubMenu::new(Category::ModelReasoning, &snap);
        assert_eq!(
            model.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::OpenModelPicker)
        );
        model.handle_key(ModalKey::Down);
        assert_eq!(
            model.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::OpenEffortPicker)
        );

        let mut providers = SubMenu::new(Category::Providers, &snap);
        assert_eq!(
            providers.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::OpenScopedModels)
        );
        // Last row is login.
        providers.handle_key(ModalKey::Down);
        providers.handle_key(ModalKey::Down);
        assert_eq!(
            providers.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::OpenLoginMethod)
        );
    }

    #[test]
    fn enum_menu_emits_the_selected_token_and_backs_out() {
        let snap = snapshot();
        let mut menu = EnumMenu::new(Field::DefaultApproval, &snap);
        // Current is "strict" (preselected); move down to "auto".
        menu.handle_key(ModalKey::Down);
        assert_eq!(
            menu.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::DefaultApproval,
                value: Some("auto".to_string()),
            })
        );
        // Esc backs out to the parent category, not Close.
        assert_eq!(
            menu.handle_key(ModalKey::Esc),
            ModalOutcome::Emit(ModalAction::OpenSettingsCategory(Category::Approvals))
        );
    }

    #[test]
    fn numeric_entry_rejects_non_numbers_and_clamps() {
        let snap = snapshot();
        let mut entry = EntryDialog::new(Field::ScrollSpeed, &snap);
        // Non-number is rejected (no emit, error shown).
        entry.input = "abc".to_string();
        assert_eq!(entry.handle_key(ModalKey::Enter), ModalOutcome::Redraw);
        assert!(entry.error.is_some());
        // Above the max clamps to 100.
        entry.input = "9999".to_string();
        assert_eq!(
            entry.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::ScrollSpeed,
                value: Some("100".to_string()),
            })
        );
        // A good in-range value is accepted verbatim.
        entry.input = "12".to_string();
        assert_eq!(
            entry.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::ScrollSpeed,
                value: Some("12".to_string()),
            })
        );
    }

    #[test]
    fn numeric_entry_empty_clears_only_when_allowed() {
        let snap = snapshot();
        // MaxToolRoundtrips allows empty -> clears to None (unbounded).
        let mut clearable = EntryDialog::new(Field::MaxToolRoundtrips, &snap);
        clearable.input = "   ".to_string();
        assert_eq!(
            clearable.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::MaxToolRoundtrips,
                value: None,
            })
        );
        // ScrollSpeed does not allow empty -> rejected.
        let mut required = EntryDialog::new(Field::ScrollSpeed, &snap);
        required.input = String::new();
        assert_eq!(required.handle_key(ModalKey::Enter), ModalOutcome::Redraw);
        assert!(required.error.is_some());
    }

    #[test]
    fn text_entry_empty_clears_and_backs_out() {
        let snap = snapshot();
        let mut entry = EntryDialog::new(Field::VerifyCommand, &snap);
        entry.push_str("  cargo test  ");
        assert_eq!(
            entry.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::VerifyCommand,
                value: Some("cargo test".to_string()),
            })
        );
        // Clearing the text emits a None (clear the key).
        entry.input = String::new();
        assert_eq!(
            entry.handle_key(ModalKey::Enter),
            ModalOutcome::Emit(ModalAction::SaveSetting {
                field: Field::VerifyCommand,
                value: None,
            })
        );
        // Esc backs out to the Verification category.
        assert_eq!(
            entry.handle_key(ModalKey::Esc),
            ModalOutcome::Emit(ModalAction::OpenSettingsCategory(Category::Verification))
        );
    }

    #[test]
    fn every_field_maps_back_to_a_category_that_lists_it() {
        // A leaf's field.category() must contain a row that targets it, so
        // post-save refresh returns to a menu that includes the field.
        let snap = snapshot();
        let fields = [
            Field::AltScreen,
            Field::ScrollSpeed,
            Field::ReducedMotion,
            Field::DefaultApproval,
            Field::ContextTokenBudget,
            Field::MaxToolRoundtrips,
            Field::PromptCacheRetention,
            Field::VerifyCommand,
            Field::VerifyMaxAttempts,
            Field::WorktreeRoot,
        ];
        for field in fields {
            let rows = rows_for(field.category(), &snap);
            let found = rows.iter().any(|row| match &row.action {
                ModalAction::OpenSettingsEnum(f) | ModalAction::OpenSettingsEntry(f) => *f == field,
                ModalAction::SaveSetting { field: f, .. } => *f == field,
                _ => false,
            });
            assert!(found, "{field:?} not present in its category submenu");
        }
    }
}
