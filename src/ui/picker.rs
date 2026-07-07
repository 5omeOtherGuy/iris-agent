//! Picker orchestration (Tier 3): turns live model/effort/scope state into
//! [`Modal`]s and applies the [`ModalAction`]s they emit, at the safe inter-turn
//! boundary. The pure decision helpers (what to open, which model an exact
//! `/model` resolves to, the next cycle target) are split out so they are unit
//! tested without a provider or harness; the thin `apply_*` wrappers gather the
//! auth/catalog snapshot and reuse [`crate::cli`]'s `candidate_for` /
//! `apply_selection` so a picker switches a provider exactly like `/model`.

use crate::cli::{self, ModelSwitch};
use crate::config;
use crate::git::status::GitStatus;
use crate::mimir::auth::storage::AuthStore;
use crate::mimir::model_capabilities;
use crate::mimir::model_catalog::{self, CatalogModel, ExactMatch};
use crate::mimir::selection::{ProviderId, ReasoningEffort};
use crate::nexus::ChatProvider;
use crate::session::{self, ResumableSession, SessionStore};
use crate::ui::modal::{
    EffortPicker, Modal, ModalAction, ModelPicker, ScopedModels, SessionPicker, SessionRow,
    TaskPicker, TrustMenu,
};
use crate::ui::settings_menu::{self, SettingsMenu};
use crate::ui::task_view::TaskCard;
use crate::wayland::Harness;
use crate::wayland::git_safety::{ActiveTaskDisplay, AdoptedTask, GitSafety, RecoverableTask};
use std::collections::BTreeSet;

/// Result of a `/model` command: open a picker, or show status/confirmation
/// lines (after an exact-match switch or when nothing is available).
pub(crate) enum ModelCommand {
    Open(Modal),
    Lines(Vec<String>),
}

/// What the loop does with the modal after an action is applied.
pub(crate) enum ActionResult {
    /// Dismiss the modal; show these lines.
    Close(Vec<String>),
    /// Keep the modal open (scoped apply/save); show these lines.
    Keep(Vec<String>),
    /// Replace the modal (settings menu -> effort submenu); show these lines.
    Replace(Box<Modal>, Vec<String>),
}

/// The active model's qualified `provider/model` id.
fn current_qualified<P>(switch: &ModelSwitch<'_, P>) -> String {
    let selection = switch.selection();
    format!("{}/{}", selection.provider.as_str(), selection.model)
}

/// Resolve the configured scope ids against the authenticated catalog, keeping
/// configured order and dropping any model whose provider is no longer
/// authenticated. Returns `None` when the resolved set is empty; callers inspect
/// the original `scoped` to tell "no scope configured" (fall back to all) apart
/// from "scope configured but nothing currently available" (stay in scope).
pub(crate) fn resolve_scoped(
    scoped: Option<&[String]>,
    available: &[CatalogModel],
) -> Option<Vec<CatalogModel>> {
    let ids = scoped?;
    let resolved: Vec<CatalogModel> = ids
        .iter()
        .filter_map(|id| available.iter().find(|model| &model.qualified() == id))
        .cloned()
        .collect();
    (!resolved.is_empty()).then_some(resolved)
}

/// Snapshot the authenticated catalog. A failure to read the auth store is
/// treated as "no models" rather than panicking.
fn available_now() -> Vec<CatalogModel> {
    let settings = std::env::current_dir()
        .ok()
        .and_then(|cwd| config::Settings::load(&cwd).ok())
        .unwrap_or_default();
    match AuthStore::from_env() {
        Ok(auth) => model_catalog::available_models(&auth, &settings),
        Err(_) => Vec::new(),
    }
}

/// Decide what `/model <arg>` does given a snapshot. Pure, so it is unit-tested
/// without a harness. `available` is the all-scope authenticated set; `scoped` is
/// the resolved scope (or `None`).
fn decide_model_command(
    arg: &str,
    available: &[CatalogModel],
    scoped: &Option<Vec<CatalogModel>>,
    current: &str,
) -> ModelDecision {
    if available.is_empty() {
        return ModelDecision::Status(vec![
            "No models available. Use /login to add providers.".to_string(),
        ]);
    }
    let arg = arg.trim();
    if arg.is_empty() {
        return ModelDecision::Open(String::new());
    }
    // Exact match runs against the active candidate set (scoped if a scope is
    // active, otherwise all available); ambiguity falls back to the picker.
    let _ = current;
    let candidates = scoped.as_deref().unwrap_or(available);
    match model_catalog::exact_match(candidates, arg) {
        ExactMatch::One(model) => ModelDecision::Switch(model),
        ExactMatch::Ambiguous | ExactMatch::None => ModelDecision::Open(arg.to_string()),
    }
}

/// Internal decision before the picker is materialized.
enum ModelDecision {
    Open(String),
    Switch(CatalogModel),
    Status(Vec<String>),
}

/// Handle a `/model [arg]` command in the TUI: open the searchable picker or, for
/// an unambiguous exact id, switch immediately (bypassing the picker).
pub(crate) fn model_command<P: ChatProvider>(
    arg: &str,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> ModelCommand {
    let available = available_now();
    let scoped = resolve_scoped(switch.scoped(), &available);
    let current = current_qualified(switch);
    match decide_model_command(arg, &available, &scoped, &current) {
        ModelDecision::Status(lines) => ModelCommand::Lines(lines),
        ModelDecision::Switch(model) => ModelCommand::Lines(apply_model(model, harness, switch)),
        ModelDecision::Open(_search) => {
            // The redesigned picker is not searchable, so an unresolved `/model
            // <arg>` opens the full list rather than a hidden filtered view.
            let default = config::default_model_qualified().unwrap_or_else(|| current.clone());
            let effort = switch
                .selection()
                .reasoning
                .unwrap_or(ReasoningEffort::DEFAULT);
            ModelCommand::Open(Modal::Model(ModelPicker::new(
                available, &current, &default, effort,
            )))
        }
    }
}

/// Build the `/scoped-models` modal, or report no available models.
pub(crate) fn open_scoped<P>(switch: &ModelSwitch<'_, P>) -> ModelCommand {
    let available = available_now();
    if available.is_empty() {
        return ModelCommand::Lines(vec!["No models available".to_string()]);
    }
    let enabled = switch.scoped().map(<[String]>::to_vec);
    ModelCommand::Open(Modal::Scoped(ScopedModels::new(available, enabled)))
}

/// Build the `/resume` picker for the given workspace, or `None` when no prior
/// session exists there. Reads the store's cheap listing (newest first) plus a
/// first-user-message preview per session, then formats a human-relative age
/// against `now_ms` into display rows. Pure row-building is split into
/// [`session_rows`] so it is unit-tested without disk.
pub(crate) fn open_resume(cwd: &std::path::Path) -> Option<Modal> {
    let store = SessionStore::open_default().ok()?;
    let entries = store.resumable_for_cwd(&cwd.to_string_lossy()).ok()?;
    if entries.is_empty() {
        return None;
    }
    let linked = resume_task_linked_session_ids(cwd);
    let rows = session_rows(&entries, session::current_ms(), &linked);
    Some(Modal::Session(SessionPicker::new(rows)))
}

fn resume_task_linked_session_ids(cwd: &std::path::Path) -> BTreeSet<String> {
    if !config::Settings::load(cwd)
        .map(|settings| settings.tasks())
        .unwrap_or(false)
    {
        return BTreeSet::new();
    }
    GitSafety::new_with_workflow(cwd, true).task_linked_session_ids()
}

/// Turn resumable-session metadata into display rows (id, preview, relative
/// age), preserving the newest-first input order. Pure, so the `/resume` picker
/// construction is unit-tested without the session store.
pub(crate) fn session_rows(
    entries: &[ResumableSession],
    now_ms: u128,
    task_linked_sessions: &BTreeSet<String>,
) -> Vec<SessionRow> {
    entries
        .iter()
        .map(|entry| SessionRow {
            id: entry.meta.id.clone(),
            preview: entry.preview.clone(),
            age: session::relative_age(now_ms, entry.meta.updated_ms),
            task_linked: task_linked_sessions.contains(&entry.meta.id),
        })
        .collect()
}

/// Build the unified `/tasks` surface from the harness (ADR-0031): the active
/// (live, unsettled) task as a non-selectable header, plus the recoverable/legacy
/// tasks as selectable rows. The active card is enriched with the git-status
/// snapshot's attribution counts + age when the snapshot's task id matches.
/// `None` when there is neither an active nor a recoverable task in this
/// workspace. Live foreign (leased) tasks are already excluded by the git-safety
/// seam.
pub(crate) fn build_tasks_modal<P: ChatProvider>(
    harness: &Harness<P>,
    git: Option<&GitStatus>,
) -> Option<Modal> {
    let recoverable = harness.recoverable_tasks();
    let active = harness
        .active_task()
        .map(|display| active_card(&display, git));
    if active.is_none() && recoverable.is_empty() {
        return None;
    }
    let cards: Vec<TaskCard> = recoverable.iter().map(TaskCard::from_recoverable).collect();
    Some(Modal::Tasks(TaskPicker::new(active, cards)))
}

/// Project the active task, enriched from the git-status snapshot's attribution
/// split (iris/user file counts) and age -- but only when the snapshot's task id
/// matches this active task, so a stale snapshot never mislabels the header
/// (ADR-0031 counts stay honest). Otherwise the counts/age are left unknown.
fn active_card(display: &ActiveTaskDisplay, git: Option<&GitStatus>) -> TaskCard {
    let matched = git
        .and_then(|status| status.task.as_ref().map(|task| (status, task)))
        .filter(|(_, task)| task.task_id == display.task_id);
    match matched {
        Some((status, task)) => TaskCard::active(
            display,
            Some(task.age),
            Some(status.iris_unsettled),
            Some(status.user_dirty),
        ),
        None => TaskCard::active(display, None, None, None),
    }
}

/// Decide the adoption UX for an adopted task (#288, ADR-0031): the notice lines
/// (body + linked-session summary) and, only when exactly one session is linked,
/// the id of that session to offer as an explicit "also resume" second action.
/// Adopting NEVER implicitly resumes a session; zero or multiple linked sessions
/// yield `None` (multiple are never guessed between). Pure, so the offer policy
/// is unit-tested without the loop.
pub(crate) fn adopt_notice(adopted: &AdoptedTask) -> (Vec<String>, Option<String>) {
    let short = short_id(&adopted.task_id);
    let body = crate::ui::task_view::body_preview(adopted.body.as_deref());
    let mut lines = vec![format!("Adopted task {short}: {body}")];
    let resume = match adopted.sessions.as_slice() {
        [] => {
            lines.push("No linked sessions recorded.".to_string());
            None
        }
        [one] => {
            lines.push(format!(
                "1 linked session ({}) -- task resumed; confirm to also resume it.",
                short_id(one)
            ));
            Some(one.clone())
        }
        many => {
            lines.push(format!(
                "{} linked sessions -- task resumed; resume one explicitly with /resume.",
                many.len()
            ));
            None
        }
    };
    (lines, resume)
}

/// A short, display-friendly prefix of an opaque id (task or session): the first
/// 8 chars, so notices stay readable without dropping uniqueness for the user.
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// The explicit "also resume its session" offer shown after adopting a task with
/// exactly one linked session (#288, ADR-0031). A single-row [`SessionPicker`]:
/// Enter resumes that session (reusing the `/resume` swap path), Esc declines
/// and leaves the task adopted. Reuses the session picker so resume goes through
/// the exact same inter-turn swap as `/resume`.
pub(crate) fn resume_offer(session_id: &str) -> Modal {
    let row = SessionRow {
        id: session_id.to_string(),
        preview: "resume the session that worked this task".to_string(),
        age: String::new(),
        task_linked: false,
    };
    Modal::Session(SessionPicker::new(vec![row]))
}

/// Explicit "resume this linked task too" offer shown after resuming a session
/// whose id appears in exactly one recoverable task record. The regular task
/// picker is reused so Enter goes through the same adoption path and keeps the
/// adoption notice copy unchanged.
pub(crate) fn linked_task_offer(task: &RecoverableTask) -> Modal {
    Modal::Tasks(TaskPicker::new(
        None,
        vec![TaskCard::from_recoverable(task)],
    ))
}

/// Build the `/trust` project-permissions modal from the harness-owned policy
/// snapshot (ADR-0027). The modal is a snapshot; every applied edit rebuilds it.
pub(crate) fn open_trust<P: ChatProvider>(harness: &Harness<P>) -> Modal {
    let record = harness.project_policy_record();
    Modal::Trust(TrustMenu::new(
        &record.allow_tools.iter().cloned().collect::<Vec<_>>(),
        &record.allow_bash.iter().cloned().collect::<Vec<_>>(),
        &record.allow_bash_prefix.iter().cloned().collect::<Vec<_>>(),
        record.sandbox.clone(),
    ))
}

/// Build the `/settings` modal: the top-level category list. Submenus are opened
/// lazily (with a fresh settings snapshot) as the user navigates, reusing the
/// existing "Replace the modal" pattern.
pub(crate) fn open_settings<P>(_switch: &ModelSwitch<'_, P>) -> Modal {
    Modal::Settings(SettingsMenu::new())
}

/// Snapshot the current persisted settings (plus the live model default) so the
/// settings menus can show each field's current value as muted metadata. Reads
/// the merged global+project config for `cwd`; a read failure degrades to
/// built-in defaults rather than failing the menu.
fn settings_snapshot<P: ChatProvider>(
    harness: &Harness<P>,
    switch: &ModelSwitch<'_, P>,
) -> settings_menu::Snapshot {
    let settings = std::env::current_dir()
        .ok()
        .and_then(|cwd| config::Settings::load(&cwd).ok())
        .unwrap_or_default();
    let tui = settings.tui_settings();
    settings_menu::Snapshot {
        default_model: config::default_model_qualified()
            .unwrap_or_else(|| current_qualified(switch)),
        default_reasoning: settings
            .default_reasoning
            .clone()
            .unwrap_or_else(|| ReasoningEffort::DEFAULT.as_str().to_string()),
        alt_screen: tui
            .and_then(|t| t.alt_screen.clone())
            .unwrap_or_else(|| "auto".to_string()),
        scroll_speed: tui.and_then(|t| t.scroll_speed).unwrap_or(3),
        reduced_motion: tui.and_then(|t| t.reduced_motion).unwrap_or(false),
        default_approval: settings
            .default_approval
            .clone()
            .unwrap_or_else(|| "strict".to_string()),
        skip_permissions: harness.skip_permissions(),
        context_token_budget: settings.context_token_budget(),
        compaction_summarizer: settings
            .compaction_summarizer
            .clone()
            .unwrap_or_else(|| "provider".to_string()),
        microcompaction: settings.microcompaction(),
        bash_tool_mode: settings.bash_tool_mode(),
        max_tool_roundtrips: settings.max_tool_roundtrips(),
        prompt_cache_retention: settings
            .prompt_cache_retention
            .clone()
            .unwrap_or_else(|| "short".to_string()),
        verify_command: settings
            .verify
            .as_ref()
            .and_then(|v| v.command.clone())
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty()),
        verify_max_attempts: settings.verification().map(|v| v.max_attempts).unwrap_or(3),
        worktree_root: settings.worktree_root.clone(),
        theme: tui
            .and_then(|t| t.theme.clone())
            .unwrap_or_else(|| crate::ui::theme::default().id().to_string()),
    }
}

/// Persist a single settings field to the user-global file. The menu widgets
/// pre-validate/clamp the value, so the parse here is a safety net; the typed
/// `config::save_*` also clamp defensively. `value` is `None` for the
/// empty-clears fields (unbounded round-trips, unset command/worktree root).
fn save_setting_field(field: settings_menu::Field, value: Option<&str>) -> anyhow::Result<()> {
    use settings_menu::Field;
    let parse_bool = |v: Option<&str>| v == Some("true");
    match field {
        Field::AltScreen => config::save_alt_screen(value.unwrap_or("auto")),
        Field::ScrollSpeed => config::save_scroll_speed(value.unwrap_or("3").parse()?),
        Field::ReducedMotion => config::save_reduced_motion(parse_bool(value)),
        Field::DefaultApproval => config::save_default_approval(value.unwrap_or("strict")),
        Field::ContextTokenBudget => {
            config::save_context_token_budget(value.unwrap_or("0").parse()?)
        }
        Field::CompactionSummarizer => {
            config::save_compaction_summarizer(value.unwrap_or("provider"))
        }
        Field::Microcompaction => config::save_microcompaction(parse_bool(value)),
        Field::BashToolMode => config::save_bash_tool_mode(parse_bool(value)),
        Field::MaxToolRoundtrips => config::save_max_tool_roundtrips(match value {
            Some(v) => Some(v.parse::<usize>()?),
            None => None,
        }),
        Field::PromptCacheRetention => {
            config::save_prompt_cache_retention(value.unwrap_or("short"))
        }
        Field::VerifyCommand => config::save_verify_command(value),
        Field::VerifyMaxAttempts => config::save_verify_max_attempts(value.unwrap_or("3").parse()?),
        Field::WorktreeRoot => config::save_worktree_root(value),
        Field::Theme => {
            let id = value.unwrap_or("terminal");
            crate::ui::theme::set_active(id);
            config::save_theme(id)
        }
    }
}

/// Build the effort/thinking picker for the current model (settings submenu).
fn effort_picker<P>(switch: &ModelSwitch<'_, P>) -> Modal {
    let selection = switch.selection();
    if selection.provider == ProviderId::OpenAiCompatible && !selection.open_ai_compatible.reasoning
    {
        return Modal::Effort(EffortPicker::new(
            vec![ReasoningEffort::Off],
            ReasoningEffort::Off,
        ));
    }
    let levels =
        model_capabilities::supported_levels(selection.provider, &selection.model).to_vec();
    let current = model_capabilities::clamp(
        selection.provider,
        &selection.model,
        selection.reasoning.unwrap_or(ReasoningEffort::DEFAULT),
    );
    Modal::Effort(EffortPicker::new(levels, current))
}

/// Apply a model/scoped/effort/settings [`ModalAction`]. Login actions are
/// handled by the loop via [`crate::ui::login`], not here.
pub(crate) fn apply_action<P: ChatProvider>(
    action: ModalAction,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> ActionResult {
    match action {
        ModalAction::SelectModel {
            id,
            effort,
            save_default,
        } => match parse_qualified(&id) {
            Some(model) => ActionResult::Close(apply_model_effort(
                model,
                effort,
                save_default,
                harness,
                switch,
            )),
            None => ActionResult::Close(vec![format!("unknown model: {id}")]),
        },
        ModalAction::ApplyScoped(ids) => {
            // Every scoped edit updates the live cycle scope immediately; only
            // Ctrl+S persists it.
            switch.set_scoped(ids.map(|ids| collapse_scope(ids, switch)));
            ActionResult::Keep(Vec::new())
        }
        ModalAction::SaveScoped(ids) => {
            let ids = ids.map(|ids| collapse_scope(ids, switch));
            switch.set_scoped(ids.clone());
            let mut lines = Vec::new();
            if let Err(error) = config::save_enabled_models(ids.as_deref()) {
                lines.push(format!("could not save scoped models: {error:#}"));
            } else {
                lines.push("Model selection saved to settings".to_string());
            }
            ActionResult::Keep(lines)
        }
        ModalAction::ToggleSkipPermissions => {
            let enabled = !harness.skip_permissions();
            harness.set_skip_permissions(enabled);
            let snap = settings_snapshot(harness, switch);
            ActionResult::Replace(
                Box::new(Modal::SettingsSub(settings_menu::SubMenu::new(
                    settings_menu::Category::Approvals,
                    &snap,
                ))),
                vec![if enabled {
                    "Dangerously skip permissions enabled for this session".to_string()
                } else {
                    "Dangerously skip permissions disabled for this session".to_string()
                }],
            )
        }
        ModalAction::EditPolicy(edit) => {
            // Wayland owns the policy store edit and live Nexus refresh; re-open
            // the modal on the refreshed policy so row states reflect it.
            let lines = match harness.apply_project_policy_edit(&edit) {
                Ok(notice) => vec![notice],
                Err(error) => vec![format!("could not save project policy: {error:#}")],
            };
            ActionResult::Replace(Box::new(open_trust(harness)), lines)
        }
        ModalAction::SetEffort(level) => ActionResult::Close(apply_effort(level, harness, switch)),
        ModalAction::OpenEffortPicker => {
            ActionResult::Replace(Box::new(effort_picker(switch)), Vec::new())
        }
        // --- settings menu tree ---
        ModalAction::OpenSettingsRoot => {
            ActionResult::Replace(Box::new(open_settings(switch)), Vec::new())
        }
        ModalAction::OpenSettingsCategory(category) => {
            let snap = settings_snapshot(harness, switch);
            ActionResult::Replace(
                Box::new(Modal::SettingsSub(settings_menu::SubMenu::new(
                    category, &snap,
                ))),
                Vec::new(),
            )
        }
        ModalAction::OpenSettingsEnum(field) => {
            let snap = settings_snapshot(harness, switch);
            ActionResult::Replace(
                Box::new(Modal::SettingsEnum(settings_menu::EnumMenu::new(
                    field, &snap,
                ))),
                Vec::new(),
            )
        }
        ModalAction::OpenSettingsEntry(field) => {
            let snap = settings_snapshot(harness, switch);
            ActionResult::Replace(
                Box::new(Modal::SettingsEntry(settings_menu::EntryDialog::new(
                    field, &snap,
                ))),
                Vec::new(),
            )
        }
        ModalAction::SaveSetting { field, value } => {
            let lines = match save_setting_field(field, value.as_deref()) {
                Ok(()) => {
                    // Some settings are read live by the harness, not just at
                    // startup: mirror the persisted value onto the running
                    // harness so the toggle takes effect at the next turn
                    // boundary in this session (DoD, ADR-0048/#378), the same
                    // way `EditPolicy` refreshes live Nexus state on save.
                    if field == settings_menu::Field::Microcompaction {
                        harness.set_microcompaction(value.as_deref() == Some("true"));
                    }
                    Vec::new()
                }
                Err(error) => vec![format!("could not save setting: {error:#}")],
            };
            // Re-open the parent category submenu on the refreshed values.
            let snap = settings_snapshot(harness, switch);
            ActionResult::Replace(
                Box::new(Modal::SettingsSub(settings_menu::SubMenu::new(
                    field.category(),
                    &snap,
                ))),
                lines,
            )
        }
        ModalAction::OpenModelPicker => {
            let available = available_now();
            if available.is_empty() {
                return ActionResult::Close(vec![
                    "No models available. Use /login to add providers.".to_string(),
                ]);
            }
            let current = current_qualified(switch);
            let default = config::default_model_qualified().unwrap_or_else(|| current.clone());
            let effort = switch
                .selection()
                .reasoning
                .unwrap_or(ReasoningEffort::DEFAULT);
            ActionResult::Replace(
                Box::new(Modal::Model(ModelPicker::new(
                    available, &current, &default, effort,
                ))),
                Vec::new(),
            )
        }
        ModalAction::OpenTrustMenu => {
            ActionResult::Replace(Box::new(open_trust(harness)), Vec::new())
        }
        ModalAction::OpenScopedModels => match open_scoped(switch) {
            ModelCommand::Open(modal) => ActionResult::Replace(Box::new(modal), Vec::new()),
            ModelCommand::Lines(lines) => ActionResult::Close(lines),
        },
        // Login navigation/side effects (incl. OpenLoginMethod) are handled by
        // the loop, which owns the auth store / login backend.
        other => ActionResult::Close(vec![format!("unhandled action: {other:?}")]),
    }
}

/// Fold an explicit enabled list back to `None` when it covers every available
/// model (pi-mono's "all enabled" -> clear scope). An empty list also clears.
fn collapse_scope<P>(ids: Vec<String>, _switch: &ModelSwitch<'_, P>) -> Vec<String> {
    let available = available_now();
    let total = available.len();
    if ids.is_empty() || (total > 0 && ids.len() >= total) {
        // Returning an empty vec signals "clear" to ModelSwitch::set_scoped.
        return Vec::new();
    }
    ids
}

/// Cycle the active model forward/backward over the scope (Ctrl+P /
/// Shift+Ctrl+P).
pub(crate) fn cycle_model<P: ChatProvider>(
    forward: bool,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> Vec<String> {
    let available = available_now();
    if available.is_empty() {
        return vec!["No models available".to_string()];
    }
    let scoped_ids = switch.scoped();
    let scoped_active = scoped_ids.is_some();
    let candidates = match resolve_scoped(scoped_ids, &available) {
        Some(scoped) => scoped,
        // Scope is configured but none of its models are currently available:
        // stay in scope (report) rather than silently cycling all models.
        None if scoped_active => {
            return vec!["No scoped models are currently available".to_string()];
        }
        None => available,
    };
    if candidates.len() <= 1 {
        return vec![if scoped_active {
            "Only one model in scope".to_string()
        } else {
            "Only one model available".to_string()
        }];
    }
    let current = current_qualified(switch);
    let pos = candidates
        .iter()
        .position(|model| model.qualified() == current);
    let next = next_cycle_index(candidates.len(), pos, forward);
    apply_model(candidates[next].clone(), harness, switch)
}

/// Next index when cycling a candidate list of length `len` (>= 1). `current` is
/// the active model's position, or `None` when it is outside the list - in which
/// case forward starts at the first row and backward at the last so no candidate
/// is skipped.
fn next_cycle_index(len: usize, current: Option<usize>, forward: bool) -> usize {
    match current {
        Some(idx) if forward => (idx + 1) % len,
        Some(idx) => (idx + len - 1) % len,
        None if forward => 0,
        None => len - 1,
    }
}

/// Cycle the thinking/effort level for the current model (Shift+Tab).
pub(crate) fn cycle_effort<P: ChatProvider>(
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> Vec<String> {
    let selection = switch.selection();
    let provider = selection.provider;
    let model = selection.model.clone();
    if !model_capabilities::supports_thinking(provider, &model)
        || (provider == ProviderId::OpenAiCompatible && !selection.open_ai_compatible.reasoning)
    {
        return vec!["Current model does not support thinking".to_string()];
    }
    let current = selection.reasoning.unwrap_or(ReasoningEffort::DEFAULT);
    let Some(next) = model_capabilities::cycle_effort(provider, &model, current, true) else {
        return vec!["Current model does not support thinking".to_string()];
    };
    apply_effort(next, harness, switch)
}

/// Apply a model switch (picker/cycle/exact) and persist it as the default.
fn apply_model<P: ChatProvider>(
    model: CatalogModel,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> Vec<String> {
    let candidate = cli::candidate_for(switch.selection(), model.provider, &model.id);
    // candidate_for clamps the carried reasoning to the new model; capture it so
    // the persisted default reasoning stays valid for the persisted default model
    // (resolve() trusts settings without re-clamping at startup).
    let reasoning = candidate.reasoning;
    let mut lines = cli::apply_selection(candidate, harness, switch);
    // Persist the new default best-effort; a write failure is surfaced but never
    // blocks the in-session switch.
    if let Err(error) = config::save_default_model(model.provider.as_str(), &model.id) {
        lines.push(format!("(default not saved: {error:#})"));
    }
    if let Some(reasoning) = reasoning
        && let Err(error) = config::save_default_reasoning(reasoning.as_str())
    {
        lines.push(format!("(reasoning not saved: {error:#})"));
    }
    lines
}

/// Apply a model switch together with a chosen effort (both clamped to the
/// model) in a single selection. `save_default` persists the pair as the global
/// default (Enter); when false the change applies for this session only (`s`).
fn apply_model_effort<P: ChatProvider>(
    model: CatalogModel,
    effort: ReasoningEffort,
    save_default: bool,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> Vec<String> {
    let clamped = model_capabilities::clamp(model.provider, &model.id, effort);
    let mut candidate = cli::candidate_for(switch.selection(), model.provider, &model.id);
    candidate.reasoning = Some(clamped);
    let mut lines = cli::apply_selection(candidate, harness, switch);
    if save_default {
        if let Err(error) = config::save_default_model(model.provider.as_str(), &model.id) {
            lines.push(format!("(default not saved: {error:#})"));
        }
        if let Err(error) = config::save_default_reasoning(clamped.as_str()) {
            lines.push(format!("(reasoning not saved: {error:#})"));
        }
    }
    lines
}

/// Apply an effort change (clamped to the model) and persist it as the default.
fn apply_effort<P: ChatProvider>(
    level: ReasoningEffort,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> Vec<String> {
    let selection = switch.selection();
    let clamped = model_capabilities::clamp(selection.provider, &selection.model, level);
    let mut candidate = selection.clone();
    candidate.reasoning = Some(clamped);
    let mut lines = cli::apply_selection(candidate, harness, switch);
    if let Err(error) = config::save_default_reasoning(clamped.as_str()) {
        lines.push(format!("(reasoning not saved: {error:#})"));
    }
    lines
}

/// Split a `provider/model` id back into a [`CatalogModel`].
fn parse_qualified(id: &str) -> Option<CatalogModel> {
    let (provider, model) = id.split_once('/')?;
    let provider = crate::mimir::selection::ProviderId::parse(provider).ok()?;
    Some(CatalogModel {
        provider,
        id: model.to_string(),
        ctx_label: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mimir::selection::ProviderId;

    fn model(provider: ProviderId, id: &str) -> CatalogModel {
        CatalogModel {
            provider,
            id: id.to_string(),
            ctx_label: None,
        }
    }

    fn available() -> Vec<CatalogModel> {
        vec![
            model(ProviderId::OpenAiCodex, "gpt-5.5"),
            model(ProviderId::Anthropic, "claude-sonnet-4-6"),
        ]
    }

    #[test]
    fn session_rows_carry_id_preview_relative_age_and_task_marker() {
        use crate::session::{ResumableSession, SessionMeta};
        use std::collections::BTreeSet;
        use std::path::PathBuf;
        let minute = 60_000u128;
        let entries = vec![
            ResumableSession {
                meta: SessionMeta {
                    id: "newest".to_string(),
                    path: PathBuf::from("/tmp/newest.jsonl"),
                    cwd: "/proj".to_string(),
                    created_ms: minute * 100,
                    updated_ms: minute * 100,
                },
                preview: "recent task".to_string(),
            },
            ResumableSession {
                meta: SessionMeta {
                    id: "older".to_string(),
                    path: PathBuf::from("/tmp/older.jsonl"),
                    cwd: "/proj".to_string(),
                    created_ms: minute * 40,
                    updated_ms: minute * 40,
                },
                preview: "older task".to_string(),
            },
        ];
        // now = 160 minutes: newest is 60m ago, older is 120m (2h) ago.
        let linked = BTreeSet::from(["older".to_string()]);
        let rows = session_rows(&entries, minute * 160, &linked);
        assert_eq!(rows.len(), 2, "order preserved (newest first)");
        assert_eq!(rows[0].id, "newest");
        assert_eq!(rows[0].preview, "recent task");
        assert_eq!(rows[0].age, "1h ago");
        assert!(!rows[0].task_linked, "unjoined session is unmarked");
        assert_eq!(rows[1].id, "older");
        assert_eq!(rows[1].age, "2h ago");
        assert!(rows[1].task_linked, "joined session is marked");
    }

    #[test]
    fn empty_available_reports_no_models() {
        match decide_model_command("", &[], &None, "openai-codex/gpt-5.5") {
            ModelDecision::Status(lines) => assert!(lines[0].contains("No models available")),
            _ => panic!("expected status"),
        }
    }

    #[test]
    fn exact_arg_switches_and_unknown_arg_opens_with_search() {
        // Exact qualified id -> switch.
        match decide_model_command(
            "anthropic/claude-sonnet-4-6",
            &available(),
            &None,
            "openai-codex/gpt-5.5",
        ) {
            ModelDecision::Switch(m) => assert_eq!(m.id, "claude-sonnet-4-6"),
            _ => panic!("expected switch"),
        }
        // Unknown -> open with the search term carried in.
        match decide_model_command("bad-prefix", &available(), &None, "openai-codex/gpt-5.5") {
            ModelDecision::Open(search) => assert_eq!(search, "bad-prefix"),
            _ => panic!("expected open"),
        }
        // No arg -> open with empty search.
        match decide_model_command("", &available(), &None, "openai-codex/gpt-5.5") {
            ModelDecision::Open(search) => assert!(search.is_empty()),
            _ => panic!("expected open"),
        }
    }

    #[test]
    fn resolve_scoped_drops_unavailable_and_preserves_order() {
        let scoped = vec![
            "anthropic/claude-sonnet-4-6".to_string(),
            "antigravity/gemini-3.5-flash".to_string(), // not in available
            "openai-codex/gpt-5.5".to_string(),
        ];
        let resolved = resolve_scoped(Some(&scoped), &available()).expect("some");
        assert_eq!(
            resolved
                .iter()
                .map(CatalogModel::qualified)
                .collect::<Vec<_>>(),
            vec![
                "anthropic/claude-sonnet-4-6".to_string(),
                "openai-codex/gpt-5.5".to_string(),
            ]
        );
        // None scope, and a scope with nothing available, both yield None.
        assert!(resolve_scoped(None, &available()).is_none());
        assert!(resolve_scoped(Some(&["x/y".to_string()]), &available()).is_none());
    }

    #[test]
    fn next_cycle_index_handles_current_inside_and_outside() {
        // Current inside the list: wrap forward and backward.
        assert_eq!(next_cycle_index(3, Some(0), true), 1);
        assert_eq!(next_cycle_index(3, Some(2), true), 0);
        assert_eq!(next_cycle_index(3, Some(0), false), 2);
        // Current outside the candidate set (e.g. active model not in scope):
        // forward picks the first, backward the last - never skipping index 0.
        assert_eq!(next_cycle_index(2, None, true), 0);
        assert_eq!(next_cycle_index(2, None, false), 1);
    }

    #[test]
    fn exact_match_uses_scoped_candidates_when_scoped_active() {
        // Scope contains only anthropic; a bare "gpt-5.5" is not in scope, so it
        // does not exact-match and opens the picker instead.
        let scoped = Some(vec![model(ProviderId::Anthropic, "claude-sonnet-4-6")]);
        match decide_model_command(
            "gpt-5.5",
            &available(),
            &scoped,
            "anthropic/claude-sonnet-4-6",
        ) {
            ModelDecision::Open(search) => assert_eq!(search, "gpt-5.5"),
            _ => panic!("expected open (gpt-5.5 not in scope)"),
        }
    }

    // --- adoption offer (#288, ADR-0031); row/projection rendering is covered
    // by the `ui::task_view` unit tests. ---

    fn adopted(id: &str, body: Option<&str>, sessions: &[&str]) -> AdoptedTask {
        AdoptedTask {
            task_id: id.to_string(),
            body: body.map(str::to_string),
            sessions: sessions.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn adopt_offer_only_for_exactly_one_linked_session() {
        // Exactly one linked session: an explicit resume offer is surfaced.
        let (lines, resume) = adopt_notice(&adopted("t1", Some("work"), &["sessone1"]));
        assert_eq!(resume.as_deref(), Some("sessone1"));
        assert!(lines.iter().any(|l| l.contains("work")), "body shown");
        assert!(
            lines.iter().any(|l| l.contains("1 linked session")),
            "the single-session line is shown: {lines:?}"
        );

        // Zero linked sessions (legacy): adopt only, never resume.
        let (lines, resume) = adopt_notice(&adopted("t2", None, &[]));
        assert!(resume.is_none(), "zero sessions never offers a resume");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("(no description recorded)")),
            "unknown body placeholder shown for a legacy resumed task: {lines:?}"
        );

        // Multiple linked sessions: never guessed between, so no resume offer.
        let (_, resume) = adopt_notice(&adopted("t3", Some("work"), &["sa", "sb"]));
        assert!(
            resume.is_none(),
            "multiple linked sessions are never guessed between"
        );
    }

    #[test]
    fn active_card_enriches_counts_only_on_matching_snapshot_id() {
        use crate::git::status::{GitStatus, TaskSummary};
        use std::time::Duration;
        let display = ActiveTaskDisplay {
            task_id: "activetask01".to_string(),
            body: Some("do the thing".to_string()),
            sessions: vec!["s1".to_string()],
            approved_paths: vec!["src/main.rs".to_string()],
            all_dirty_approved: false,
        };
        // Matching snapshot id: counts + age come from the git status snapshot.
        let matching = GitStatus {
            iris_unsettled: 3,
            user_dirty: 2,
            task: Some(TaskSummary {
                task_id: "activetask01".to_string(),
                age: Duration::from_secs(120),
            }),
            ..Default::default()
        };
        let card = active_card(&display, Some(&matching));
        assert_eq!(card.iris_files, Some(3));
        assert_eq!(card.user_files, Some(2));
        assert_eq!(card.age_label(), "2m ago");

        // Stale snapshot (different task id): counts/age left unknown so the
        // header never mislabels a different task's numbers as this task's.
        let stale = GitStatus {
            iris_unsettled: 9,
            user_dirty: 9,
            task: Some(TaskSummary {
                task_id: "someothertask".to_string(),
                age: Duration::from_secs(1),
            }),
            ..Default::default()
        };
        let card = active_card(&display, Some(&stale));
        assert_eq!(card.iris_files, None);
        assert_eq!(card.user_files, None);
        assert_eq!(card.age_label(), "");

        // No snapshot at all: also unknown, never fabricated.
        let card = active_card(&display, None);
        assert_eq!(card.iris_files, None);
        assert_eq!(card.user_files, None);
    }
}
