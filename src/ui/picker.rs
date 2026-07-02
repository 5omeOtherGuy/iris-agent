//! Picker orchestration (Tier 3): turns live model/effort/scope state into
//! [`Modal`]s and applies the [`ModalAction`]s they emit, at the safe inter-turn
//! boundary. The pure decision helpers (what to open, which model an exact
//! `/model` resolves to, the next cycle target) are split out so they are unit
//! tested without a provider or harness; the thin `apply_*` wrappers gather the
//! auth/catalog snapshot and reuse [`crate::cli`]'s `candidate_for` /
//! `apply_selection` so a picker switches a provider exactly like `/model`.

use crate::cli::{self, ModelSwitch};
use crate::config;
use crate::mimir::auth::storage::AuthStore;
use crate::mimir::model_capabilities;
use crate::mimir::model_catalog::{self, CatalogModel, ExactMatch};
use crate::mimir::selection::{ProviderId, ReasoningEffort};
use crate::nexus::ChatProvider;
use crate::session::{self, ResumableSession, SessionStore};
use crate::ui::modal::{
    EffortPicker, Modal, ModalAction, ModelPicker, ScopedModels, SessionPicker, SessionRow,
    SettingsMenu, TrustMenu,
};
use crate::wayland::Harness;
use crate::wayland::approvals;
use crate::wayland::trust::{self, TrustDecision};

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
    let rows = session_rows(&entries, session::current_ms());
    Some(Modal::Session(SessionPicker::new(rows)))
}

/// Turn resumable-session metadata into display rows (id, preview, relative
/// age), preserving the newest-first input order. Pure, so the `/resume` picker
/// construction is unit-tested without the session store.
pub(crate) fn session_rows(entries: &[ResumableSession], now_ms: u128) -> Vec<SessionRow> {
    entries
        .iter()
        .map(|entry| SessionRow {
            id: entry.meta.id.clone(),
            preview: entry.preview.clone(),
            age: session::relative_age(now_ms, entry.meta.updated_ms),
        })
        .collect()
}

/// Build the `/trust` modal from the current recorded decision for the cwd. An
/// undecided project shows the untrusted row as current (the effective state).
pub(crate) fn open_trust() -> Modal {
    let trusted = std::env::current_dir()
        .ok()
        .map(|cwd| trust::decision_for(&cwd) == TrustDecision::Trusted)
        .unwrap_or(false);
    Modal::Trust(TrustMenu::new(trusted))
}

/// Apply a `/trust` decision: persist it (keyed by canonical cwd), re-assemble
/// the system prompt under the new trust, and rebuild the provider at the safe
/// inter-turn boundary so the change takes effect this session. The provider is
/// rebuilt first; the decision is persisted and the success notice shown only
/// after the rebuild succeeds. A rebuild failure restores the prior prompt and
/// is reported honestly (no success-looking notice), failing closed on untrust.
fn apply_trust<P: ChatProvider>(
    trusted: bool,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> Vec<String> {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => return vec![format!("could not resolve working directory: {error:#}")],
    };
    // Re-assemble the prompt under the requested trust and rebuild the provider
    // with it BEFORE committing anything. Mirrors `/model` / `/reasoning`: build
    // first, commit only after the rebuild succeeds. If the rebuild fails we
    // restore the prior prompt and report the failure honestly -- for an untrust
    // that means failing closed: never leave a success-looking notice while the
    // old trusted prompt is still live.
    let tools = crate::tools::built_in_tools();
    let prompt = crate::wayland::system_prompt::assemble(&cwd, &tools, trusted);
    let previous_prompt = switch.system_prompt().to_string();
    switch.set_system_prompt(prompt);
    let candidate = switch.selection().clone();
    let rebuild = cli::apply_selection(candidate, harness, switch);
    let (rebuilt, mut lines) = finalize_trust(trusted, rebuild);
    if !rebuilt {
        // Rebuild failed: restore the prompt so the session keeps matching the
        // still-live provider, and do not persist the decision.
        switch.set_system_prompt(previous_prompt);
        return lines;
    }
    // Rebuild succeeded; commit the decision so future sessions honor it too.
    if let Err(error) = trust::set_decision(&cwd, trusted) {
        lines.push(format!("could not save trust decision: {error:#}"));
    }
    lines
}

/// Finalize a `/trust` change after the provider rebuild attempt. `rebuild` is
/// the output of [`cli::apply_selection`]; a line containing "could not" marks a
/// failed rebuild. Returns `(rebuilt, lines)`: on failure `rebuilt` is `false`
/// and `lines` carries only the failure -- no trust notice, so the session never
/// claims the prompt changed (fail closed); on success `rebuilt` is `true` with
/// the trust confirmation. The generic "switched to <model>" line is dropped
/// either way, as it would be misleading for a trust change.
fn finalize_trust(trusted: bool, rebuild: Vec<String>) -> (bool, Vec<String>) {
    let failures: Vec<String> = rebuild
        .into_iter()
        .filter(|line| line.contains("could not"))
        .collect();
    if !failures.is_empty() {
        return (false, failures);
    }
    let notice = if trusted {
        "Project trusted; repo fragments now load.".to_string()
    } else {
        "Project not trusted; repo fragments are skipped.".to_string()
    };
    (true, vec![notice])
}

/// Build the `/settings` modal (effort picker + project approvals entries).
pub(crate) fn open_settings<P>(switch: &ModelSwitch<'_, P>, workspace: &std::path::Path) -> Modal {
    let current = switch
        .selection()
        .reasoning
        .unwrap_or(ReasoningEffort::DEFAULT);
    let grants = approvals::grants_for(workspace).len();
    Modal::Settings(SettingsMenu::new(current, grants))
}

/// Build the project-approvals review/revoke submenu from the stored grants.
fn approvals_menu<P: ChatProvider>(harness: &Harness<P>) -> Modal {
    let grants = approvals::grants_for(harness.workspace())
        .iter()
        .map(|grant| (grant.key(), grant.label()))
        .collect();
    Modal::Approvals(crate::ui::modal::ApprovalsMenu::new(grants))
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
        ModalAction::SetTrust(trusted) => {
            ActionResult::Close(apply_trust(trusted, harness, switch))
        }
        ModalAction::SetEffort(level) => ActionResult::Close(apply_effort(level, harness, switch)),
        ModalAction::OpenEffortPicker => {
            ActionResult::Replace(Box::new(effort_picker(switch)), Vec::new())
        }
        ModalAction::OpenApprovals => {
            ActionResult::Replace(Box::new(approvals_menu(harness)), Vec::new())
        }
        ModalAction::RevokeApproval(key) => {
            let line = match approvals::Grant::parse(&key) {
                Some(grant) => match approvals::revoke_grant(harness.workspace(), &grant) {
                    Ok(()) => format!("revoked project approval: {}", grant.label()),
                    Err(error) => format!("could not revoke project approval: {error:#}"),
                },
                None => format!("unknown approval entry: {key}"),
            };
            // Rebuild the submenu so the list reflects the revocation.
            ActionResult::Replace(Box::new(approvals_menu(harness)), vec![line])
        }
        // Login navigation/side effects are not picker concerns.
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
    fn session_rows_carry_id_preview_and_relative_age() {
        use crate::session::{ResumableSession, SessionMeta};
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
        let rows = session_rows(&entries, minute * 160);
        assert_eq!(rows.len(), 2, "order preserved (newest first)");
        assert_eq!(rows[0].id, "newest");
        assert_eq!(rows[0].preview, "recent task");
        assert_eq!(rows[0].age, "1h ago");
        assert_eq!(rows[1].id, "older");
        assert_eq!(rows[1].age, "2h ago");
    }

    #[test]
    fn finalize_trust_fails_closed_when_rebuild_fails() {
        // A failed rebuild must not commit and must not emit a trust notice --
        // otherwise an untrust would keep the old trusted prompt live while
        // claiming success. Only the honest failure is surfaced.
        let (rebuilt, lines) = finalize_trust(false, vec!["could not switch: boom".to_string()]);
        assert!(!rebuilt, "a failed rebuild must not be committed");
        assert!(
            lines.iter().any(|l| l.contains("could not switch")),
            "the rebuild failure is surfaced: {lines:?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("not trusted")),
            "no success-looking notice on a failed untrust: {lines:?}"
        );
        // A trust that fails to rebuild is equally uncommitted and silent.
        let (rebuilt, lines) = finalize_trust(true, vec!["could not switch: boom".to_string()]);
        assert!(!rebuilt);
        assert!(
            !lines.iter().any(|l| l.contains("repo fragments now load")),
            "{lines:?}"
        );
    }

    #[test]
    fn finalize_trust_reports_notice_after_a_clean_rebuild() {
        // A clean rebuild commits and shows the trust notice, dropping the
        // generic "switched to <model>" confirmation.
        let (rebuilt, lines) = finalize_trust(
            true,
            vec!["switched to anthropic/claude (reasoning: none)".to_string()],
        );
        assert!(rebuilt);
        assert_eq!(
            lines,
            vec!["Project trusted; repo fragments now load.".to_string()]
        );
        let (rebuilt, lines) =
            finalize_trust(false, vec!["switched to x/y (reasoning: none)".to_string()]);
        assert!(rebuilt);
        assert_eq!(
            lines,
            vec!["Project not trusted; repo fragments are skipped.".to_string()]
        );
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
}
