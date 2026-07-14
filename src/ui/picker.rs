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
use crate::mimir::model_catalog::{self, AuthStatus, CatalogModel, ExactMatch};
use crate::mimir::selection::{ProviderId, ReasoningEffort};
use crate::nexus::{ApprovalMode, ChatProvider, PermissionMode};
use crate::session::{self, ResumableSession, SessionStore};
use crate::ui::modal::{
    Modal, ModalAction, SessionPicker, SessionRow, SwitchContextPrompt, TaskPicker,
};
use crate::ui::settings_menu::{
    self, HatchTarget, ModelChoice, PanelView, PolicySnapshot, ProviderStatus, ScopeChoice,
    SettingsPanel,
};
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

fn switch_context_prompt<P: ChatProvider>(
    id: String,
    effort: ReasoningEffort,
    save_default: bool,
    harness: &Harness<P>,
    switch: &ModelSwitch<'_, P>,
) -> Option<Modal> {
    let model = parse_qualified(&id)?;
    let mut candidate = cli::candidate_for(switch.selection(), model.provider, &model.id);
    candidate.reasoning = Some(model_capabilities::clamp(model.provider, &model.id, effort));
    let context_tokens = harness.context_token_estimate();
    cli::switch_context_advisory_for(&candidate, context_tokens, harness.context_budget(), switch)
        .map(|_| {
            Modal::SwitchContext(SwitchContextPrompt::new(
                id,
                effort,
                save_default,
                candidate.model,
                context_tokens,
            ))
        })
}

fn model_command_for_candidate<P: ChatProvider>(
    model: CatalogModel,
    save_default: bool,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> ModelCommand {
    let id = model.qualified();
    let effort = cli::candidate_for(switch.selection(), model.provider, &model.id)
        .reasoning
        .unwrap_or(ReasoningEffort::DEFAULT);
    if let Some(modal) = switch_context_prompt(id, effort, save_default, harness, switch) {
        ModelCommand::Open(modal)
    } else {
        ModelCommand::Lines(apply_model(model, harness, switch))
    }
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
        ModelDecision::Switch(model) => model_command_for_candidate(model, true, harness, switch),
        ModelDecision::Open(_search) => {
            // Bare / unresolved `/model` opens the faceplate's ENGINE model
            // hatch (§4.1) — the picker is now an in-place hatch, not a door.
            ModelCommand::Open(open_settings_expanded(harness, switch, HatchTarget::Model))
        }
    }
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
    let enabled = config::Settings::load(cwd)
        .map(|settings| settings.mutation_safety() && settings.tasks())
        .unwrap_or(false);
    if !enabled {
        return BTreeSet::new();
    }
    GitSafety::new_configured(
        cwd,
        true,
        crate::wayland::trust::native_jj(cwd).unwrap_or(false),
    )
    .task_linked_session_ids()
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

/// Build the `/settings` modal: the flat settings panel (the faceplate),
/// populated with a fresh snapshot of the persisted + live values.
pub(crate) fn open_settings<P: ChatProvider>(
    harness: &Harness<P>,
    switch: &ModelSwitch<'_, P>,
) -> Modal {
    Modal::Settings(Box::new(SettingsPanel::new(settings_snapshot(
        harness, switch,
    ))))
}

/// Open the faceplate with a hatch pre-expanded — the slash-command entry path
/// (§4.1). `/model` opens ENGINE › model, `/scoped-models` opens the scope
/// hatch, `/trust` the permissions hatch, `/login`/`/logout` the providers
/// hatch (cursor per [`HatchTarget`]).
pub(crate) fn open_settings_expanded<P: ChatProvider>(
    harness: &Harness<P>,
    switch: &ModelSwitch<'_, P>,
    target: HatchTarget,
) -> Modal {
    Modal::Settings(Box::new(SettingsPanel::with_expanded(
        settings_snapshot(harness, switch),
        target,
    )))
}

/// Rebuild the faceplate from a fresh snapshot while preserving the operator's
/// view (open hatch, identity-keyed cursor, live filter) and re-arming the
/// acted-on row's detent flash (§2.5, §5). This is the seamless refresh after
/// a model select, a policy edit, a logout, or a dialog-guard round trip.
pub(crate) fn refresh_settings_panel<P: ChatProvider>(
    view: PanelView,
    flash: Option<crate::ui::settings_menu::PanelRow>,
    harness: &Harness<P>,
    switch: &ModelSwitch<'_, P>,
) -> Modal {
    let mut panel = SettingsPanel::new(settings_snapshot(harness, switch));
    panel.restore(view);
    if let Some(row) = flash {
        panel.flash_row(row);
    }
    Modal::Settings(Box::new(panel))
}

/// Snapshot the current persisted settings (plus the live session state the
/// panel controls, and the hatch payloads) so the panel shows each control's
/// real position. Reads the merged global+project config for `cwd`; a read
/// failure degrades to built-in defaults rather than failing the panel.
pub(crate) fn settings_snapshot<P: ChatProvider>(
    harness: &Harness<P>,
    switch: &ModelSwitch<'_, P>,
) -> settings_menu::Snapshot {
    let settings = config::Settings::load(harness.workspace()).unwrap_or_default();
    let web_bounds = settings.web_bounds().unwrap_or_else(|_| {
        config::Settings::default()
            .web_bounds()
            .expect("default web bounds are valid")
    });
    let tui = settings.tui_settings();
    let selection = switch.selection();
    // The reasoning switch clicks through the ACTIVE model's levels; a
    // non-reasoning OpenAI-compatible endpoint pins the switch to `off`.
    let open_ai_compatible_reasoning = selection.open_ai_compatible.reasoning;
    let reasoning_levels: Vec<(ReasoningEffort, &'static str)> =
        model_capabilities::selectable_options(
            selection.provider,
            &selection.model,
            open_ai_compatible_reasoning,
        )
        .iter()
        .map(|option| (option.level, option.label))
        .collect();
    let reasoning =
        if selection.provider == ProviderId::OpenAiCompatible && !open_ai_compatible_reasoning {
            ReasoningEffort::Off
        } else {
            model_capabilities::clamp(
                selection.provider,
                &selection.model,
                selection.reasoning.unwrap_or(ReasoningEffort::DEFAULT),
            )
        };
    let available = available_now();
    let current = current_qualified(switch);
    let default_model = config::default_model_qualified().unwrap_or_else(|| current.clone());
    let catalog = build_catalog(
        &available,
        &current,
        &default_model,
        open_ai_compatible_reasoning,
    );
    let scope_candidates: Vec<ScopeChoice> = available
        .iter()
        .map(|model| ScopeChoice {
            qualified: model.qualified(),
            provider_label: model.provider.display_name().to_string(),
        })
        .collect();
    let scope_enabled = switch.scoped().map(<[String]>::to_vec);
    let providers = build_providers();
    let record = harness.project_policy_record();
    let policy = PolicySnapshot {
        granted_tools: record.allow_tools.iter().cloned().collect(),
        bash_exact: record.allow_bash.iter().cloned().collect(),
        bash_prefix: record.allow_bash_prefix.iter().cloned().collect(),
        sandbox: record.sandbox,
    };
    // The MEMORY compaction group reads the RESOLVED tool-result-compaction
    // policy (structured block, or the legacy microcompaction alias); a
    // malformed on-disk policy falls back to the built-in default so the panel
    // still opens.
    let compaction = settings.tool_result_compaction().unwrap_or_else(|_| {
        crate::config::Settings::default()
            .tool_result_compaction()
            .unwrap()
    });
    // The AUTO COMPACT group reads the validated full-context trigger; a
    // malformed on-disk ladder falls back to the built-in default so the panel
    // still opens. Percent dials carry the fractions as whole percents.
    let trigger = settings.compaction_trigger().unwrap_or_else(|_| {
        crate::config::Settings::default()
            .compaction_trigger()
            .unwrap()
    });
    let worker_input = settings
        .compaction
        .as_ref()
        .and_then(|value| value.worker.as_ref())
        .and_then(|value| value.input.clone())
        .unwrap_or_else(|| "transcript".to_string());
    // The dim resolved line reuses the live harness ladder (`/context`
    // diagnostics) resolved against the active model window rather than
    // recomputing the model-aware arithmetic on the panel.
    let diagnostics = harness.context_diagnostics();
    let resolved_ladder = diagnostics.as_ref().map(|diag| {
        let ladder = diag.ladder;
        settings_menu::ResolvedLadder {
            warn: ladder.warn,
            start: ladder.start,
            hard: ladder.hard,
            effective_tail: ladder.keep_recent_tokens,
            configured_tail: trigger.keep_recent_tokens,
            effective_window: ladder.displayed_context_window,
        }
    });
    // The model's displayed context window (pre-clamp), from the same resolved
    // policy /context prints, caps the `context cap` dial at model truth.
    let model_context_window = diagnostics
        .as_ref()
        .and_then(|diag| diag.policy)
        .and_then(|policy| policy.window)
        .map(|window| window.displayed);
    settings_menu::Snapshot {
        default_model,
        reasoning_levels,
        reasoning,
        catalog,
        scope_candidates,
        scope_enabled,
        scope_persisted: settings.enabled_models.clone(),
        providers,
        policy,
        alt_screen: tui
            .and_then(|t| t.alt_screen.clone())
            .unwrap_or_else(|| "auto".to_string()),
        scroll_speed: tui.and_then(|t| t.scroll_speed).unwrap_or(3),
        reduced_motion: tui.and_then(|t| t.reduced_motion).unwrap_or(false),
        default_approval: cli::current_permission_token(harness).to_string(),
        skip_permissions: harness.skip_permissions(),
        context_token_budget: settings.context_token_budget(),
        compaction_enabled: trigger.enabled,
        compaction_warn_pct: (trigger.warn * 100.0).round() as u64,
        compaction_start_pct: (trigger.start * 100.0).round() as u64,
        compaction_hard_pct: (trigger.hard * 100.0).round() as u64,
        compaction_keep_recent_tokens: trigger.keep_recent_tokens,
        compaction_hard_wait_ms: trigger.hard_wait_ms,
        compaction_reactive: trigger.reactive,
        compaction_worker_input: worker_input,
        resolved_ladder,
        compaction_provider_native: match settings.compaction_provider_native() {
            Ok(crate::config::ProviderNativeMode::Auto) => "auto".to_string(),
            _ => "off".to_string(),
        },
        compaction_summarizer: settings
            .compaction_summarizer
            .clone()
            .unwrap_or_else(|| "subagent".to_string()),
        microcompaction: compaction.enabled,
        microcompaction_watermark: compaction.trigger_tokens,
        compaction_aggressiveness: compaction.aggressiveness.as_str().to_string(),
        compaction_cache_timing: compaction.cache_timing.as_str().to_string(),
        semantic_retain_per_path: compaction.semantic_dedupe.retain_per_path,
        tool_clearing_keep_recent: compaction.tool_clearing.keep_recent_tool_uses,
        semantic_dedupe_enabled: compaction.semantic_dedupe.enabled,
        tool_clearing_enabled: compaction.tool_clearing.enabled,
        model_context_window,
        prompt_cache_retention: settings
            .prompt_cache_retention
            .clone()
            .unwrap_or_else(|| "short".to_string()),
        web_search_backend: settings
            .web_search_backend
            .clone()
            .unwrap_or_else(|| "off".to_string()),
        read_web_page_backend: settings
            .read_web_page_backend
            .clone()
            .unwrap_or_else(|| "off".to_string()),
        searxng_url: settings.searxng_url.clone(),
        search_timeout_ms: web_bounds.search_timeout.as_millis() as u64,
        read_timeout_ms: web_bounds.read_timeout.as_millis() as u64,
        max_search_results: web_bounds.max_search_results as u64,
        max_search_response_bytes: web_bounds.max_search_response_bytes as u64,
        max_read_response_bytes: web_bounds.max_read_response_bytes as u64,
        max_read_output_bytes: web_bounds.max_read_output_bytes as u64,
        verify_command: settings
            .verify
            .as_ref()
            .and_then(|v| v.command.clone())
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty()),
        verify_max_attempts: settings.verification().map(|v| v.max_attempts).unwrap_or(3),
        mutation_safety: harness.mutation_safety_enabled(),
        native_jj_available: harness.mutation_safety_enabled() && harness.native_jj_available(),
        native_jj_enabled: harness.native_jj_enabled(),
        worktree_root: settings.worktree_root.clone(),
        pending_rows: Vec::new(),
        theme: tui
            .and_then(|t| t.theme.clone())
            .unwrap_or_else(|| crate::ui::theme::default().id().to_string()),
    }
}

/// Build the ENGINE › model hatch catalog: default-first (then by provider),
/// each row carrying the reasoning levels its inline effort track clicks. This
/// is the `ModelPicker` ordering + level data, moved onto the hatch (§4.2).
fn build_catalog(
    available: &[CatalogModel],
    current: &str,
    default: &str,
    open_ai_compatible_reasoning: bool,
) -> Vec<ModelChoice> {
    order_by_default(available.to_vec(), default)
        .into_iter()
        .map(|model| {
            let qualified = model.qualified();
            let levels = model_capabilities::selectable_options(
                model.provider,
                &model.id,
                open_ai_compatible_reasoning,
            )
            .iter()
            .map(|option| (option.level, option.label))
            .collect();
            ModelChoice {
                display: model_catalog::display_name(&qualified),
                provider_label: model.provider.display_name().to_string(),
                is_current: qualified == current,
                is_default: qualified == default,
                provider: model.provider,
                model_id: model.id.clone(),
                levels,
                qualified,
            }
        })
        .collect()
}

/// Order the catalog: the persisted default first, then the rest by provider
/// name, preserving registry order within a provider (the `ModelPicker` rule).
fn order_by_default(models: Vec<CatalogModel>, default: &str) -> Vec<CatalogModel> {
    let mut ordered: Vec<CatalogModel> = Vec::with_capacity(models.len());
    if let Some(found) = models.iter().find(|model| model.qualified() == default) {
        ordered.push(found.clone());
    }
    let mut rest: Vec<CatalogModel> = models
        .into_iter()
        .filter(|model| model.qualified() != default)
        .collect();
    rest.sort_by(|a, b| a.provider.as_str().cmp(b.provider.as_str()));
    ordered.extend(rest);
    ordered
}

/// Build the ENGINE › providers hatch rows for every known provider (registry
/// order): the no-secret credential badge plus the login methods that exist.
fn build_providers() -> Vec<ProviderStatus> {
    let auth = AuthStore::from_env().ok();
    ProviderId::ALL
        .iter()
        .map(|&provider| {
            let status = auth
                .as_ref()
                .map(|auth| model_catalog::provider_status(auth, provider))
                .unwrap_or(AuthStatus::Unconfigured);
            let credentialed = status.is_configured();
            let badge = match status {
                AuthStatus::StoredOAuth | AuthStatus::ClaudeCode => "subscription",
                AuthStatus::StoredApiKey | AuthStatus::EnvApiKey => "api key",
                AuthStatus::Unconfigured => "\u{2014}",
            }
            .to_string();
            ProviderStatus {
                id: provider.as_str().to_string(),
                name: provider.display_name().to_string(),
                badge,
                oauth_capable: provider_oauth_capable(provider),
                api_key_capable: provider_api_key_capable(provider),
                credentialed,
            }
        })
        .collect()
}

/// Whether a provider supports the OAuth/subscription login flow (the `↵`
/// primary method for these). Mirrors `login::subscription_providers`.
fn provider_oauth_capable(provider: ProviderId) -> bool {
    matches!(
        provider,
        ProviderId::OpenAiCodex | ProviderId::Anthropic | ProviderId::Antigravity
    )
}

/// Whether a provider accepts a stored API key (the `a` path / the `↵` primary
/// for non-OAuth providers). Mirrors `login::api_key_providers`.
fn provider_api_key_capable(provider: ProviderId) -> bool {
    matches!(
        provider,
        ProviderId::Anthropic | ProviderId::OpenAi | ProviderId::OpenAiCompatible
    )
}

/// Convert a whole-percent dial value (`"60"`) to the stored fraction (`0.60`).
/// The ordering invariant is enforced by the config save, not here.
fn percent_to_fraction(value: Option<&str>) -> anyhow::Result<f64> {
    let percent: f64 = value.unwrap_or("0").trim().parse()?;
    Ok(percent / 100.0)
}

/// Persist a single settings field to the user-global file. The menu widgets
/// pre-validate/clamp the value, so the parse here is a safety net; the typed
/// `config::save_*` also clamp defensively. `value` is `None` for the
/// empty-clears fields (unbounded round-trips, unset command/worktree root).
pub(crate) fn persist_setting_field(
    field: settings_menu::Field,
    value: Option<&str>,
    workspace: &std::path::Path,
) -> anyhow::Result<()> {
    use settings_menu::Field;
    let parse_bool = |v: Option<&str>| v == Some("true");
    match field {
        Field::AltScreen => config::save_alt_screen(value.unwrap_or("auto")),
        Field::ScrollSpeed => config::save_scroll_speed(value.unwrap_or("3").parse()?),
        Field::ReducedMotion => config::save_reduced_motion(parse_bool(value)),
        Field::DefaultApproval => config::save_default_approval(value.unwrap_or("strict")),
        Field::MutationSafety => config::save_mutation_safety(parse_bool(value)),
        Field::NativeJj => crate::wayland::trust::set_native_jj(workspace, parse_bool(value)),
        Field::ContextTokenBudget => {
            config::save_context_token_budget(value.unwrap_or("0").parse()?)
        }
        Field::CompactionEnabled => config::save_compaction_enabled(workspace, parse_bool(value)),
        Field::CompactionReactive => config::save_compaction_reactive(workspace, parse_bool(value)),
        Field::CompactionKeepRecentTokens => {
            config::save_compaction_keep_recent_tokens(workspace, value.unwrap_or("0").parse()?)
        }
        Field::CompactionHardWait => {
            config::save_compaction_hard_wait(workspace, value.unwrap_or("0").parse()?)
        }
        // The percent dials persist as fractions; the config save rejects any
        // combination that would leave the merged global+project ladder
        // unordered.
        Field::CompactionWarn => {
            config::save_compaction_threshold_warn(workspace, percent_to_fraction(value)?)
        }
        Field::CompactionStart => {
            config::save_compaction_threshold_start(workspace, percent_to_fraction(value)?)
        }
        Field::CompactionHard => {
            config::save_compaction_threshold_hard(workspace, percent_to_fraction(value)?)
        }
        Field::CompactionWorkerInput => {
            config::save_compaction_worker_input(value.unwrap_or("transcript"))
        }
        Field::CompactionSummarizer => {
            config::save_compaction_summarizer(value.unwrap_or("subagent"))
        }
        Field::CompactionProviderNative => {
            config::save_compaction_provider_native(value.unwrap_or("off"))
        }
        Field::Microcompaction => config::save_tool_result_compaction_enabled(parse_bool(value)),
        Field::MicrocompactionWatermark => {
            config::save_tool_result_compaction_trigger_tokens(value.unwrap_or("0").parse()?)
        }
        Field::CompactionAggressiveness => {
            config::save_tool_result_compaction_aggressiveness(value.unwrap_or("conservative"))
        }
        Field::CompactionCacheTiming => {
            config::save_tool_result_compaction_cache_timing(value.unwrap_or("cacheAware"))
        }
        Field::SemanticRetainPerPath => {
            config::save_tool_result_compaction_retain_per_path(value.unwrap_or("1").parse()?)
        }
        Field::ToolClearingKeepRecent => {
            config::save_tool_result_compaction_keep_recent_tool_uses(value.unwrap_or("1").parse()?)
        }
        Field::PromptCacheRetention => {
            config::save_prompt_cache_retention(value.unwrap_or("short"))
        }
        Field::WebSearchBackend => config::save_web_search_backend(value.unwrap_or("off")),
        Field::ReadWebPageBackend => config::save_read_web_page_backend(value.unwrap_or("off")),
        Field::SearxngUrl => config::save_searxng_url(value),
        Field::SearchTimeout => config::save_search_timeout_ms(value.unwrap_or("0").parse()?),
        Field::ReadTimeout => config::save_read_timeout_ms(value.unwrap_or("0").parse()?),
        Field::MaxSearchResults => config::save_max_search_results(value.unwrap_or("0").parse()?),
        Field::MaxSearchResponseBytes => {
            config::save_max_search_response_bytes(value.unwrap_or("0").parse()?)
        }
        Field::MaxReadResponseBytes => {
            config::save_max_read_response_bytes(value.unwrap_or("0").parse()?)
        }
        Field::MaxReadOutputBytes => {
            config::save_max_read_output_bytes(value.unwrap_or("0").parse()?)
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

/// Whether a saved field feeds the full-context auto-compaction trigger ladder
/// (master switch, thresholds, tail, hard-tier bounded wait, or the legacy
/// context cap that clamps the window), so its live application re-resolves the
/// ladder against the current model and installs it on the harness. Includes
/// `context cap` (`contextTokenBudget`), which the menu previously failed to
/// mirror live.
fn is_auto_compaction_trigger_field(field: settings_menu::Field) -> bool {
    use settings_menu::Field;
    matches!(
        field,
        Field::CompactionEnabled
            | Field::CompactionWarn
            | Field::CompactionStart
            | Field::CompactionHard
            | Field::CompactionKeepRecentTokens
            | Field::CompactionHardWait
            | Field::CompactionReactive
            | Field::ContextTokenBudget
    )
}

/// Reload settings, resolve the auto-compaction ladder against the active model,
/// and install it on the live harness so a threshold/tail/reactive/enabled
/// change takes effect at the next boundary. Disabling automatic compaction also
/// cancels any in-flight background job. Reuses [`crate::resolved_compaction_trigger`]
/// so the model-aware arithmetic is never duplicated.
fn apply_compaction_trigger<P: ChatProvider>(
    harness: &mut Harness<P>,
    switch: &ModelSwitch<'_, P>,
    obs: &dyn crate::nexus::AgentObserver,
) -> anyhow::Result<()> {
    let settings = config::Settings::load(harness.workspace())?;
    let selection = switch.selection().clone();
    let (budget, trigger) = crate::resolved_compaction_trigger(&settings, &selection)?;
    harness.set_compaction_trigger(budget, trigger);
    if !trigger.enabled {
        // Disabling cancels an in-flight background job; the harness emits the
        // `Cancelled` lifecycle through `obs` so the transition is recorded.
        harness.cancel_auto_compaction(obs)?;
    }
    Ok(())
}

/// The permission mode a skip-approvals toggle flips to: enabling the dangerous
/// bypass, or restoring the parked approval mode (#520). Shared by the faceplate
/// skip-approvals row and its tests.
fn toggle_skip_permissions_mode(
    skip_permissions: bool,
    approval_mode: ApprovalMode,
) -> PermissionMode {
    if skip_permissions {
        PermissionMode::Approval(approval_mode)
    } else {
        PermissionMode::DangerousSkipPermissions
    }
}

/// Apply a model/scoped/effort/settings/policy [`ModalAction`] emitted by the
/// faceplate. `view` is the panel's restorable view (open hatch + identity
/// cursor + filter), captured before the action so a snapshot-refreshing action
/// (model select, policy edit) can rebuild the panel in place without losing
/// the operator's position (§5). Login/logout side effects are handled by the
/// loop via [`crate::ui::login`], not here.
pub(crate) fn apply_action<P: ChatProvider>(
    action: ModalAction,
    view: Option<PanelView>,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
    obs: &dyn crate::nexus::AgentObserver,
) -> ActionResult {
    match action {
        ModalAction::SelectModel {
            id,
            effort,
            save_default,
        } => {
            if let Some(modal) =
                switch_context_prompt(id.clone(), effort, save_default, harness, switch)
            {
                // Advisory: the confirm prompt overlays as a dialog-guard; the
                // loop's stash reopens the faceplate expanded whichever way it
                // resolves (§2.5).
                ActionResult::Replace(Box::new(modal), Vec::new())
            } else {
                let lines = switch_model_lines(id, effort, save_default, harness, switch);
                // The row now shows the new engine; only a failed persist is
                // worth a transcript line. The hatch stays open, header flashes.
                refresh_settings(
                    view,
                    Some(model_header_flash()),
                    failures(lines),
                    harness,
                    switch,
                )
            }
        }
        ModalAction::ConfirmModelSwitch {
            id,
            effort,
            save_default,
            compact_first: _,
        } => apply_confirmed_model_switch(id, effort, save_default, harness, switch),
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
            // The panel's guarded switch already clicked its own display. Persist
            // through #520's permission-mode default: `apply_permission_mode`
            // flips the live harness AND saves `defaultApproval` (the dangerous
            // bypass becomes the persisted default, the parked preset is restored
            // and persisted when toggled back). Rebuild the faceplate in place so
            // the skip and approvals rows settle onto the new persisted posture.
            let mode =
                toggle_skip_permissions_mode(harness.skip_permissions(), harness.approval_mode());
            let flash = view.as_ref().map(|view| view.cursor());
            let lines = cli::apply_permission_mode(harness, mode);
            refresh_settings(view, flash, lines, harness, switch)
        }
        ModalAction::EditPolicy(edit) => {
            // Wayland owns the policy store edit and live Nexus refresh; rebuild
            // the panel on the refreshed policy so the hatch rows reflect it. The
            // switch flip / row removal is the feedback — only a failed write is
            // worth a line.
            let flash = view.as_ref().map(|view| view.cursor());
            let lines = match harness.apply_project_policy_edit(&edit) {
                Ok(_notice) => Vec::new(),
                Err(error) => vec![format!("could not save project policy: {error:#}")],
            };
            refresh_settings(view, flash, lines, harness, switch)
        }
        ModalAction::CycleModel { forward } => {
            // The model row clicked ←/→: cycle the scoped models exactly like
            // Ctrl+P. A large-context advisory overlays the confirm prompt (the
            // loop's stash returns home); a completed click rebuilds the panel on
            // the fresh snapshot so the row shows the new engine, and flashes it.
            let before = current_qualified(switch);
            match cycle_model(forward, harness, switch) {
                ModelCommand::Open(modal) => ActionResult::Replace(Box::new(modal), Vec::new()),
                ModelCommand::Lines(lines) => {
                    if current_qualified(switch) == before {
                        // Nothing cycled ("Only one model available", empty
                        // scope): the status line is the honest feedback.
                        ActionResult::Keep(lines)
                    } else {
                        refresh_settings(
                            view,
                            Some(model_header_flash()),
                            failures(lines),
                            harness,
                            switch,
                        )
                    }
                }
            }
        }
        ModalAction::AdjustEffort(level) => {
            // The panel's reasoning switch: apply to the live session and
            // persist as the default, exactly like the model hatch's inline
            // effort. The panel stays open and already shows the new detent,
            // so the switch chatter is suppressed — only a failed persist is
            // surfaced (numbers/notices stay honest).
            let lines = apply_effort(level, harness, switch);
            ActionResult::Keep(failures(lines))
        }
        ModalAction::SaveSetting { field, value } => {
            let requested = value.as_deref() == Some("true");
            let previous_safety = harness.mutation_safety_enabled();
            let previous_native_jj = harness.native_jj_enabled();
            let reconfigured = match field {
                settings_menu::Field::MutationSafety => {
                    harness.configure_mutation_safety(requested, previous_native_jj)
                }
                settings_menu::Field::NativeJj => {
                    if requested && !harness.native_jj_available() {
                        Err("native jj integration is unavailable in this workspace")
                    } else {
                        harness.configure_mutation_safety(previous_safety, requested)
                    }
                }
                _ => Ok(()),
            };
            if let Err(error) = reconfigured {
                return refresh_settings(view, None, vec![error.to_string()], harness, switch);
            }
            match persist_setting_field(field, value.as_deref(), harness.workspace()) {
                Ok(()) => {
                    // Some settings are read live by the harness, not just at
                    // startup: mirror the persisted value onto the running
                    // harness so the toggle takes effect at the next turn
                    // boundary in this session (DoD, ADR-0048/#378), the same
                    // way `EditPolicy` refreshes live Nexus state on save.
                    if field == settings_menu::Field::DefaultApproval {
                        if let Some(mode) = value.as_deref().and_then(PermissionMode::parse) {
                            cli::set_permission_mode(harness, mode);
                        }
                    } else if is_auto_compaction_trigger_field(field) {
                        // Threshold/tail/reactive/enabled changes take effect at
                        // the next boundary: reload, resolve the ladder against
                        // the current model, and install it live. Disabling also
                        // cancels an in-flight background job (the loop then
                        // clears the chip from the live diagnostics).
                        if let Err(error) = apply_compaction_trigger(harness, switch, obs) {
                            return refresh_settings(
                                view,
                                None,
                                vec![format!("could not apply setting: {error:#}")],
                                harness,
                                switch,
                            );
                        }
                    } else if field == settings_menu::Field::CompactionSummarizer {
                        // Summarizer changes affect only the next worker job.
                        if let Ok(settings) = config::Settings::load(harness.workspace()) {
                            harness.set_summarizer(settings.compaction_summarizer());
                        }
                    } else if field == settings_menu::Field::CompactionProviderNative {
                        // Routing changes affect only the next worker job.
                        if let Ok(settings) = config::Settings::load(harness.workspace())
                            && let Ok(mode) = settings.compaction_provider_native()
                        {
                            harness.set_provider_native(mode == config::ProviderNativeMode::Auto);
                        }
                    } else if field == settings_menu::Field::CompactionWorkerInput {
                        // Worker-input changes affect only the next worker job.
                        if let Ok(settings) = config::Settings::load(harness.workspace())
                            && let Ok(worker) = settings.compaction_worker_config()
                        {
                            harness.set_compaction_worker(worker);
                        }
                    } else if matches!(
                        field,
                        settings_menu::Field::Microcompaction
                            | settings_menu::Field::MicrocompactionWatermark
                            | settings_menu::Field::CompactionAggressiveness
                            | settings_menu::Field::CompactionCacheTiming
                            | settings_menu::Field::SemanticRetainPerPath
                            | settings_menu::Field::ToolClearingKeepRecent
                    ) && let Ok(settings) = config::Settings::load(harness.workspace())
                        && let Ok(policy) = settings.tool_result_compaction()
                        && let Err(error) =
                            cli::apply_tool_result_compaction(policy, harness, switch)
                    {
                        // The live re-provider failed (e.g. the new policy is
                        // incompatible with the model): rebuild the faceplate from
                        // disk with the error surfaced — the panel is the only
                        // settings UI, so there is no submenu to fall back to.
                        return refresh_settings(
                            view,
                            None,
                            vec![format!("could not apply setting: {error:#}")],
                            harness,
                            switch,
                        );
                    }
                    if field == settings_menu::Field::CompactionAggressiveness {
                        // Presets can change which resolved reducer passes run;
                        // reload their exact policy rather than leaving dependent
                        // controls dimmed from the prior preset while this panel
                        // remains open.
                        let flash = view.as_ref().map(|view| view.cursor());
                        return refresh_settings(view, flash, Vec::new(), harness, switch);
                    }
                    if field == settings_menu::Field::MutationSafety
                        && requested
                        && harness.native_jj_available()
                        && crate::wayland::trust::native_jj(harness.workspace()).is_none()
                    {
                        return ActionResult::Replace(
                            Box::new(crate::ui::modal::jj_setup()),
                            Vec::new(),
                        );
                    }
                    if field == settings_menu::Field::MutationSafety {
                        // Enabling the master can make native jj available in the
                        // same interaction. Rebuild the faceplate so its dependent
                        // row never keeps the availability snapshot from when the
                        // master was off.
                        return refresh_settings(view, None, Vec::new(), harness, switch);
                    }
                    // The panel is the display truth while open: it already
                    // clicked the detent, so keep it (no rebuild jank).
                    ActionResult::Keep(Vec::new())
                }
                Err(error) => {
                    if matches!(
                        field,
                        settings_menu::Field::MutationSafety | settings_menu::Field::NativeJj
                    ) {
                        let _ =
                            harness.configure_mutation_safety(previous_safety, previous_native_jj);
                    }
                    // The write failed: rebuild the panel from disk so the
                    // control settles back onto the persisted position.
                    refresh_settings(
                        view,
                        None,
                        vec![format!("could not save setting: {error:#}")],
                        harness,
                        switch,
                    )
                }
            }
        }
        // Login navigation/side effects (BeginLogin/OpenApiKeyDialog/Logout) are
        // handled by the loop, which owns the auth store / login backend.
        other => ActionResult::Close(vec![format!("unhandled action: {other:?}")]),
    }
}

/// The model header row — the flash target after a model select/cycle (§3.1).
fn model_header_flash() -> crate::ui::settings_menu::PanelRow {
    crate::ui::settings_menu::PanelRow::Top(crate::ui::settings_menu::RowId::Model)
}

/// Keep only the failure lines from a model/effort switch (a failed persist);
/// the row itself is the success feedback, so the confirmation chatter is muted.
fn failures(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .filter(|line| line.contains("not saved") || line.contains("is not supported"))
        .collect()
}

/// Rebuild the faceplate in place from a fresh snapshot, preserving the view and
/// re-arming `flash`; fall back to a fresh (unexpanded) panel if no view was
/// captured (should not happen — the panel is always front for these actions).
fn refresh_settings<P: ChatProvider>(
    view: Option<PanelView>,
    flash: Option<crate::ui::settings_menu::PanelRow>,
    lines: Vec<String>,
    harness: &Harness<P>,
    switch: &ModelSwitch<'_, P>,
) -> ActionResult {
    let modal = match view {
        Some(view) => refresh_settings_panel(view, flash, harness, switch),
        None => open_settings(harness, switch),
    };
    ActionResult::Replace(Box::new(modal), lines)
}

fn switch_model_lines<P: ChatProvider>(
    id: String,
    effort: ReasoningEffort,
    save_default: bool,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> Vec<String> {
    match parse_qualified(&id) {
        Some(model) => apply_model_effort(model, effort, save_default, harness, switch),
        None => vec![format!("unknown model: {id}")],
    }
}

fn apply_confirmed_model_switch<P: ChatProvider>(
    id: String,
    effort: ReasoningEffort,
    save_default: bool,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> ActionResult {
    ActionResult::Close(switch_model_lines(
        id,
        effort,
        save_default,
        harness,
        switch,
    ))
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
) -> ModelCommand {
    let available = available_now();
    if available.is_empty() {
        return ModelCommand::Lines(vec!["No models available".to_string()]);
    }
    let scoped_ids = switch.scoped();
    let scoped_active = scoped_ids.is_some();
    let candidates = match resolve_scoped(scoped_ids, &available) {
        Some(scoped) => scoped,
        // Scope is configured but none of its models are currently available:
        // stay in scope (report) rather than silently cycling all models.
        None if scoped_active => {
            return ModelCommand::Lines(vec![
                "No scoped models are currently available".to_string(),
            ]);
        }
        None => available,
    };
    if candidates.len() <= 1 {
        return ModelCommand::Lines(vec![if scoped_active {
            "Only one model in scope".to_string()
        } else {
            "Only one model available".to_string()
        }]);
    }
    let current = current_qualified(switch);
    let pos = candidates
        .iter()
        .position(|model| model.qualified() == current);
    let next = next_cycle_index(candidates.len(), pos, forward);
    model_command_for_candidate(candidates[next].clone(), true, harness, switch)
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
    fn model_catalog_honors_openai_compatible_reasoning_gate() {
        let models = vec![model(ProviderId::OpenAiCompatible, "custom")];
        let disabled = build_catalog(
            &models,
            "openai-compatible/custom",
            "openai-compatible/custom",
            false,
        );
        assert_eq!(disabled[0].levels, vec![(ReasoningEffort::Off, "off")]);

        let enabled = build_catalog(
            &models,
            "openai-compatible/custom",
            "openai-compatible/custom",
            true,
        );
        assert_eq!(
            enabled[0]
                .levels
                .iter()
                .map(|(level, _)| *level)
                .collect::<Vec<_>>(),
            vec![
                ReasoningEffort::Off,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ]
        );
    }

    #[test]
    fn auto_compaction_trigger_fields_are_classified_apart_from_worker_knobs() {
        use settings_menu::Field;
        for field in [
            Field::CompactionEnabled,
            Field::CompactionWarn,
            Field::CompactionStart,
            Field::CompactionHard,
            Field::CompactionKeepRecentTokens,
            Field::CompactionHardWait,
            Field::CompactionReactive,
        ] {
            assert!(is_auto_compaction_trigger_field(field), "{field:?}");
        }
        // Summarizer/worker-input feed the next worker job, not the ladder.
        assert!(!is_auto_compaction_trigger_field(
            Field::CompactionSummarizer
        ));
        assert!(!is_auto_compaction_trigger_field(
            Field::CompactionWorkerInput
        ));
        assert!(!is_auto_compaction_trigger_field(
            Field::CompactionProviderNative
        ));
        // The context cap clamps the window, so it re-resolves the ladder live.
        assert!(is_auto_compaction_trigger_field(Field::ContextTokenBudget));
    }

    #[test]
    fn percent_dial_value_converts_to_a_stored_fraction() {
        assert_eq!(percent_to_fraction(Some("60")).unwrap(), 0.60);
        assert_eq!(percent_to_fraction(Some(" 19 ")).unwrap(), 0.19);
        assert!(percent_to_fraction(Some("not-a-number")).is_err());
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
    fn skip_permissions_toggle_restores_parked_approval_mode() {
        assert_eq!(
            toggle_skip_permissions_mode(false, ApprovalMode::Auto),
            PermissionMode::DangerousSkipPermissions
        );
        assert_eq!(
            toggle_skip_permissions_mode(true, ApprovalMode::Auto),
            PermissionMode::Approval(ApprovalMode::Auto)
        );
        assert_eq!(
            toggle_skip_permissions_mode(true, ApprovalMode::NeverAsk),
            PermissionMode::Approval(ApprovalMode::NeverAsk)
        );
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
