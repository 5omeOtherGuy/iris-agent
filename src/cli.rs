use std::cell::RefCell;
use std::io::IsTerminal;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;

use crate::config;
use crate::mimir::model_capabilities;
use crate::mimir::selection::{self, ModelSelection, ProviderId, ReasoningEffort};
use crate::nexus::{
    AgentObserver, ApprovalGate, ApprovalMode, ChatProvider, Message, PermissionMode, Role,
};
use crate::session::{self, SessionLog, SessionStore};
use crate::ui::tui::TuiUi;
use crate::ui::{Ui, UiBridge, UiEvent, slash};
use crate::wayland::Harness;

pub(crate) const TASK_WORKFLOW_OFF_NOTICE: &str =
    "task workflow is off - enable with `tasks = true` or `/tasks enable`";

pub(crate) const PROVIDER_NATIVE_COMPACTION_WARNING: &str = "OpenAI native compaction is enabled. Its opaque encrypted continuation block can be reused only by the same OpenAI model. After a model switch, Iris uses a separately generated portable text summary; differences between the two may change subsequent model behavior.";

pub(crate) fn provider_native_compaction_notices(enabled: bool) -> Vec<String> {
    if enabled {
        vec![PROVIDER_NATIVE_COMPACTION_WARNING.to_string()]
    } else {
        Vec::new()
    }
}

/// Tier-3 runtime mode-switch state: the active [`ModelSelection`], the
/// assembled system prompt, and a builder that rebuilds a provider for a new
/// selection. Tier 3 owns this (not the harness): a `/model` `/reasoning` switch
/// mutates the selection, rebuilds the provider with the existing prompt, and
/// installs it through [`Harness::replace_provider`]. Threaded into both
/// front-ends so one handler serves the text loop and the TUI loop.
pub(crate) struct ModelSwitch<'a, P> {
    pub(crate) selection: ModelSelection,
    system_prompt: String,
    build: &'a dyn Fn(&ModelSelection, &str) -> Result<P>,
    /// Shared active selection cell read by the background compaction provider
    /// factory. Text/TUI model switches update it at the same boundary as the
    /// foreground provider swap; tests and generic harnesses leave it unset.
    background_selection: Option<Arc<Mutex<ModelSelection>>>,
    compaction_settings: Option<config::Settings>,
    /// Ordered `provider/model` ids that scope Ctrl+P cycling, or `None` to cycle
    /// every authenticated model. Seeded from `settings.enabled_models` and edited
    /// by `/scoped-models`; only the picker's Ctrl+S persists it back to settings.
    scoped: Option<Vec<String>>,
}

impl<'a, P> ModelSwitch<'a, P> {
    pub(crate) fn new(
        selection: ModelSelection,
        system_prompt: String,
        build: &'a dyn Fn(&ModelSelection, &str) -> Result<P>,
        scoped: Option<Vec<String>>,
    ) -> Self {
        Self {
            selection,
            system_prompt,
            build,
            background_selection: None,
            compaction_settings: None,
            scoped,
        }
    }

    pub(crate) fn set_background_selection_cell(&mut self, cell: Arc<Mutex<ModelSelection>>) {
        self.background_selection = Some(cell);
    }

    pub(crate) fn set_compaction_settings(&mut self, settings: config::Settings) {
        self.compaction_settings = Some(settings);
    }

    /// The active resolved selection (provider/model/base-url/reasoning).
    /// The system prompt the active provider was built with, for the
    /// `/context` breakdown's system+tools estimate (display-only).
    pub(crate) fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub(crate) fn selection(&self) -> &ModelSelection {
        &self.selection
    }

    /// The current Ctrl+P cycle scope (ordered qualified ids), or `None` for
    /// "cycle all authenticated models".
    pub(crate) fn scoped(&self) -> Option<&[String]> {
        self.scoped.as_deref()
    }

    /// Replace the session cycle scope. `None`/empty clears it.
    pub(crate) fn set_scoped(&mut self, scoped: Option<Vec<String>>) {
        self.scoped = scoped.filter(|ids| !ids.is_empty());
    }

    /// Rebuild a provider for the current selection and system prompt without
    /// changing the active selection. The session swap uses this after the app
    /// updates the shared session-id cell, so the resumed/new session's id keys
    /// the freshly built provider. Unlike [`apply_selection`], it neither
    /// installs the provider nor records an audit event -- the caller installs it
    /// via [`Harness::replace_provider`](crate::wayland::Harness::replace_provider).
    pub(crate) fn rebuild_provider(&self) -> Result<P> {
        (self.build)(&self.selection, &self.system_prompt)
    }
}

/// Which session an in-session swap (`/resume`, `/new`) should load. The TUI
/// loop hands one of these to the app-supplied loader, which builds the matching
/// [`LoadedSource`] (a fresh transcript, or a persisted session's messages).
#[derive(Debug, Clone)]
pub(crate) enum SessionSource {
    /// Start a brand-new session (new id, empty transcript, fresh log).
    Fresh,
    /// Resume the persisted session with this id.
    Resume(String),
}

/// Rollback guard for the shared session id that provider builders read. A
/// session swap must point the provider builder at the target id before the
/// rebuild, but if the rebuild fails the live session stays unchanged; dropping
/// this guard before commit restores the previous id so later rebuilds cannot
/// key a provider to the wrong session.
pub(crate) struct SessionIdGuard {
    cell: Rc<RefCell<String>>,
    previous: String,
    background: Option<(Arc<Mutex<String>>, String)>,
    committed: bool,
}

impl SessionIdGuard {
    #[cfg(test)]
    pub(crate) fn swap(cell: Rc<RefCell<String>>, next: String) -> Self {
        let previous = cell.replace(next);
        Self {
            cell,
            previous,
            background: None,
            committed: false,
        }
    }

    pub(crate) fn swap_with_background(
        cell: Rc<RefCell<String>>,
        background: Arc<Mutex<String>>,
        next: String,
    ) -> Self {
        let previous = cell.replace(next.clone());
        let previous_background = {
            let mut id = background
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            std::mem::replace(&mut *id, next)
        };
        Self {
            cell,
            previous,
            background: Some((background, previous_background)),
            committed: false,
        }
    }

    pub(crate) fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for SessionIdGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.cell.replace(self.previous.clone());
            if let Some((background, previous)) = &self.background {
                *background
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = previous.clone();
            }
        }
    }
}

/// The pieces the harness needs to swap to a different session at the safe
/// inter-turn boundary: a transaction guard for the provider-builder session id,
/// the reopened/new transcript log, the provider-visible messages to seed, and
/// how many of them are already persisted (0 for a fresh session, the loaded
/// count for a resume). The provider is rebuilt separately via
/// [`ModelSwitch::rebuild_provider`] so the new session's id keys it.
pub(crate) struct LoadedSource {
    pub(crate) session_id: SessionIdGuard,
    pub(crate) session_log: Option<SessionLog>,
    pub(crate) messages: Vec<Message>,
    /// Durable ids parallel to `messages` (`Some` = coverable on-disk entry,
    /// `None` = summary position or id-less legacy entry). Empty for a fresh
    /// session. Carried into `swap_session` so a resumed prefix stays
    /// compactable (#375).
    pub(crate) entry_ids: Vec<Option<String>>,
    pub(crate) resumed: usize,
    /// Normal approval preset to install for this target session. Dangerous skip
    /// is carried separately so it remains an exclusive mode.
    pub(crate) approval_mode: ApprovalMode,
    /// Persisted/default dangerous skip-permissions state for the target session.
    pub(crate) skip_permissions: bool,
}

/// Builds a [`LoadedSource`] for a requested [`SessionSource`]. The app
/// (`main.rs`) owns session-store access and the shared session-id cell the
/// provider builder reads, so it can generate/select the id, open or create the
/// log, and load messages; the loop only asks for the swap.
pub(crate) type SessionLoader<'a> = dyn Fn(&SessionSource) -> Result<LoadedSource> + 'a;

pub(crate) const APPROVAL_USAGE: &str = "strict|auto|never|dangerously-skip-permissions";

/// The operator-visible permission mode: dangerous skip is exclusive and hides
/// the normal approval preset while active.
pub(crate) fn current_permission_token<P: ChatProvider>(harness: &Harness<P>) -> &'static str {
    if harness.skip_permissions() {
        crate::nexus::DANGEROUS_SKIP_PERMISSIONS_TOKEN
    } else {
        harness.approval_mode().as_token()
    }
}

/// Apply a permission mode to the live harness without writing settings. Normal
/// modes always clear dangerous skip; dangerous mode enables it and leaves the
/// normal preset parked until a normal mode is selected.
pub(crate) fn set_permission_mode<P: ChatProvider>(harness: &mut Harness<P>, mode: PermissionMode) {
    match mode {
        PermissionMode::Approval(mode) => {
            harness.set_approval_mode(mode);
            if harness.skip_permissions() {
                harness.set_skip_permissions(false);
            }
        }
        PermissionMode::DangerousSkipPermissions => {
            if !harness.skip_permissions() {
                harness.set_skip_permissions(true);
            }
        }
    }
}

/// Apply a permission mode and persist it as the global default. Persistence
/// errors are surfaced as notice lines but never block the live mode switch.
pub(crate) fn apply_permission_mode<P: ChatProvider>(
    harness: &mut Harness<P>,
    mode: PermissionMode,
) -> Vec<String> {
    set_permission_mode(harness, mode);
    let token = current_permission_token(harness);
    let mut lines = vec![format!("approval mode set to {token}")];
    if let Err(error) = config::save_default_approval(token) {
        lines.push(format!("(default not saved: {error:#})"));
    }
    lines
}

/// Startup-only UI state: notices to show before the first input event, an
/// optional TUI modal, whether the home/start page should render, and the session
/// id when this process already resumed a transcript before entering the loop.
pub(crate) struct StartupUi {
    pub(crate) notices: Vec<String>,
    pub(crate) modal: Option<crate::ui::modal::Modal>,
    pub(crate) followup_modal: Option<crate::ui::modal::Modal>,
    pub(crate) start_page: bool,
    pub(crate) resumed_session: Option<String>,
}

/// Route a submitted line through the shared `/model` / `/reasoning` handler.
/// Returns `None` when the line is not one of those commands (the caller then
/// submits it as a normal prompt) or when no switch state is available;
/// `Some(lines)` when handled, with the info / confirmation / error lines to
/// display. Switching only happens here, at the inter-turn prompt boundary, so
/// a switch can never land mid-stream or mid-tool.
pub(crate) fn handle_model_command<P: ChatProvider>(
    line: &str,
    harness: &mut Harness<P>,
    switch: &mut Option<ModelSwitch<'_, P>>,
) -> Option<Vec<String>> {
    let switch = switch.as_mut()?;
    let trimmed = line.trim();
    let (cmd, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((cmd, rest)) => (cmd, rest.trim()),
        None => (trimmed, ""),
    };
    match cmd {
        "/model" => Some(handle_model(rest, harness, switch)),
        "/reasoning" => Some(handle_reasoning(rest, harness, switch)),
        _ => None,
    }
}

/// Route `/rollback`, `/accept`, and `/checkpoint` through the harness-owned
/// checkpoint API (issue #263, ADR-0028). Returns `None` when the line is not
/// one of these commands (the caller submits it as a normal prompt);
/// `Some(lines)` when handled, with the notices to display. `/rollback` requires
/// an explicit restore-point number: typing it is the confirmation that Iris's
/// own work at/after that point may be discarded (user paths are never touched).
pub(crate) fn handle_checkpoint_command<P: ChatProvider>(
    line: &str,
    harness: &mut Harness<P>,
) -> Option<Vec<String>> {
    let trimmed = line.trim();
    let (cmd, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((cmd, rest)) => (cmd, rest.trim()),
        None => (trimmed, ""),
    };
    match cmd {
        "/accept" => Some({
            if !harness.task_workflow_enabled() {
                return Some(vec![TASK_WORKFLOW_OFF_NOTICE.to_string()]);
            }
            // Compute the net-diff summary BEFORE accepting finishes the task
            // (issue #264): show what is being accepted, per file. Fail closed
            // (finding 2): a diff read error must NOT accept the task as if
            // there were nothing to accept.
            match harness.task_diff() {
                Err(error) => vec![format!(
                    "could not compute task diff: {error:#}; not accepting"
                )],
                Ok(diff) => {
                    let summary = diff.summary_lines();
                    match harness.accept_checkpoint() {
                        Some(outcome) => {
                            let mut lines = summary;
                            lines.push(outcome);
                            lines
                        }
                        None => vec!["no unreviewed Iris changes to accept".to_string()],
                    }
                }
            }
        }),
        "/checkpoint" => Some(if !harness.task_workflow_enabled() {
            vec![TASK_WORKFLOW_OFF_NOTICE.to_string()]
        } else {
            match harness.save_checkpoint() {
                Some(summary) => vec![summary],
                None => vec!["no Iris changes to checkpoint".to_string()],
            }
        }),
        "/rollback" => Some(if !harness.task_workflow_enabled() {
            vec![TASK_WORKFLOW_OFF_NOTICE.to_string()]
        } else {
            handle_rollback(rest, harness)
        }),
        _ => None,
    }
}

/// Shared `/tasks` routing. Returns `None` only for bare `/tasks` while the
/// workflow is enabled, so the interactive TUI can open the task surface.
/// Text mode treats that `None` as a TUI-only status line.
pub(crate) fn handle_tasks_command<P: ChatProvider>(
    line: &str,
    harness: &mut Harness<P>,
) -> Option<Vec<String>> {
    let trimmed = line.trim();
    let (cmd, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((cmd, rest)) => (cmd, rest.trim()),
        None => (trimmed, ""),
    };
    if cmd != "/tasks" {
        return None;
    }
    match rest {
        "" => {
            if harness.task_workflow_enabled() {
                None
            } else {
                Some(vec![TASK_WORKFLOW_OFF_NOTICE.to_string()])
            }
        }
        "enable" => match config::save_project_tasks(harness.workspace(), true) {
            Ok(()) => match harness.set_task_workflow_enabled(true) {
                Ok(()) => Some(vec!["task workflow enabled for this project".to_string()]),
                Err(error) => Some(vec![error.to_string()]),
            },
            Err(error) => Some(vec![format!("could not enable task workflow: {error:#}")]),
        },
        "disable" => {
            if harness.current_task_id().is_some() {
                return Some(vec![
                    "finish the current task before disabling task workflow".to_string(),
                ]);
            }
            match config::save_project_tasks(harness.workspace(), false) {
                Ok(()) => match harness.set_task_workflow_enabled(false) {
                    Ok(()) => Some(vec!["task workflow disabled for this project".to_string()]),
                    Err(error) => Some(vec![error.to_string()]),
                },
                Err(error) => Some(vec![format!("could not disable task workflow: {error:#}")]),
            }
        }
        _ => Some(vec!["usage: /tasks [enable|disable]".to_string()]),
    }
}

/// `/task` is the compact help surface for the task workflow. It is display-only
/// and shared by text/TUI so the command copy does not drift.
pub(crate) fn handle_task_command<P: ChatProvider>(
    line: &str,
    harness: &Harness<P>,
) -> Option<Vec<String>> {
    let trimmed = line.trim();
    let (cmd, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((cmd, rest)) => (cmd, rest.trim()),
        None => (trimmed, ""),
    };
    if cmd != "/task" {
        return None;
    }
    match rest {
        "" | "help" => Some(task_help_lines(harness.task_workflow_enabled())),
        _ => Some(vec!["usage: /task [help]".to_string()]),
    }
}

fn task_help_lines(enabled: bool) -> Vec<String> {
    let state = if enabled { "on" } else { "off" };
    vec![
        format!("task workflow: {state}"),
        "/tasks: review the active task, accept, inspect diff, undo, or resume an interrupted task"
            .to_string(),
        "/checkpoint: save a rollback point and keep working".to_string(),
        "/diff: inspect Iris's current task changes".to_string(),
        "/accept: accept Iris's current task changes".to_string(),
        "/rollback: list or restore a rollback point".to_string(),
    ]
}

/// Build the Tier-3 event for `/diff` (issue #264): the task's net diff as a
/// summary + colorized unified diff, or an honest notice when there are no net
/// Iris changes (or no unsettled task). Shared by the TUI and text drivers so
/// both render the same computation.
pub(crate) fn task_diff_event<P: ChatProvider>(harness: &Harness<P>) -> UiEvent {
    if !harness.task_workflow_enabled() {
        return UiEvent::Notice(TASK_WORKFLOW_OFF_NOTICE.to_string());
    }
    // Fail closed (issue #264 finding 2): a checkpoint/blob read error surfaces
    // as an honest error notice, never a misleading "no Iris changes".
    let diff = match harness.task_diff() {
        Ok(diff) => diff,
        Err(error) => {
            return UiEvent::Notice(format!("could not compute task diff: {error:#}"));
        }
    };
    if diff.is_empty() {
        return UiEvent::Notice("no Iris changes in this task".to_string());
    }
    UiEvent::TaskDiff {
        summary: diff.summary_lines(),
        diff: diff.unified(),
    }
}

/// `/rollback` (no args) lists the restore points; `/rollback <n>` restores that
/// point. Only Iris-authored work and the user's index are affected.
fn handle_rollback<P: ChatProvider>(rest: &str, harness: &mut Harness<P>) -> Vec<String> {
    let points = harness.checkpoint_restore_points();
    if points.is_empty() {
        return vec!["no unreviewed Iris changes to roll back".to_string()];
    }
    if rest.is_empty() {
        let mut lines =
            vec!["restore points (/rollback <n> discards Iris's own work at/after n):".to_string()];
        for point in &points {
            lines.push(format!("  {} - {}", point.seq, point.label));
        }
        return lines;
    }
    let seq = match rest.parse::<u64>() {
        Ok(seq) => seq,
        Err(_) => return vec!["usage: /rollback <n> (a restore point number)".to_string()],
    };
    if !points.iter().any(|point| point.seq == seq) {
        return vec![format!(
            "no restore point {seq}; run /rollback to list them"
        )];
    }
    match harness.rollback_checkpoint(seq) {
        Ok(outcome) => {
            let mut lines = vec![outcome.summary];
            lines.extend(outcome.preserved_notices);
            lines.extend(outcome.index_warning);
            lines
        }
        Err(error) => vec![format!("rollback failed: {error:#}")],
    }
}

/// `/model` (no args) shows the current selection; `/model <provider>/<model>`
/// or `/model <model>` switches by exact id.
fn handle_model<P: ChatProvider>(
    rest: &str,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> Vec<String> {
    if rest.is_empty() {
        return current_selection_lines(&switch.selection);
    }
    let candidate = match parse_model_target(rest, &switch.selection) {
        Ok(candidate) => candidate,
        Err(error) => return vec![format!("{error:#}")],
    };
    apply_selection(candidate, harness, switch)
}

/// `/reasoning <level>` sets the effort, validated/clamped against the current
/// model.
fn handle_reasoning<P: ChatProvider>(
    rest: &str,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> Vec<String> {
    let provider = switch.selection.provider;
    let model = switch.selection.model.clone();
    if rest.is_empty() {
        return vec![format!(
            "usage: /reasoning <{}>",
            model_capabilities::join_display_levels(provider, &model).replace(", ", "|")
        )];
    }
    let level = match model_capabilities::parse_level(provider, &model, rest) {
        Ok(level) => level,
        Err(error) => return vec![format!("{error:#}")],
    };
    let clamped = model_capabilities::clamp(provider, &model, level);
    let mut lines = Vec::new();
    if clamped != level {
        lines.push(format!(
            "reasoning '{}' is not supported by {}/{}; using '{}'",
            rest.trim(),
            provider.as_str(),
            model,
            model_capabilities::display_level(provider, &model, clamped),
        ));
    }
    let mut candidate = switch.selection.clone();
    candidate.reasoning = Some(clamped);
    lines.extend(apply_selection(candidate, harness, switch));
    // Persist only after a successful switch: apply_selection leaves
    // switch.selection untouched on failure and returns no switch-confirmation
    // line, so mirror the picker's non-blocking persistence path only when the
    // selection was actually installed.
    if lines.iter().any(|line| line.starts_with("switched to "))
        && let Err(error) = config::save_default_reasoning(clamped.as_str())
    {
        lines.push(format!("(reasoning not saved: {error:#})"));
    }
    lines
}

/// Parse a `/model` argument into a candidate selection. Exact ids only: an
/// unknown provider errors; an unknown model is allowed and passes through.
fn parse_model_target(rest: &str, current: &ModelSelection) -> Result<ModelSelection> {
    let (provider, model) = match rest.split_once('/') {
        Some((provider, model)) => (ProviderId::parse(provider)?, model.trim().to_string()),
        None => (current.provider, rest.trim().to_string()),
    };
    if model.is_empty() {
        bail!("usage: /model <provider>/<model> or /model <model>");
    }
    Ok(candidate_for(current, provider, &model))
}

fn carried_reasoning(
    current: &ModelSelection,
    provider: ProviderId,
    model: &str,
) -> Option<ReasoningEffort> {
    if provider == ProviderId::OpenAiCompatible && !current.open_ai_compatible.reasoning {
        None
    } else {
        current
            .reasoning
            .map(|level| model_capabilities::clamp(provider, model, level))
    }
}

/// Build a candidate selection for a (provider, model), carrying base-url and
/// reasoning forward the same way `/model` does: a model-only switch keeps the
/// resolved base url (which respected the global settings value); a provider
/// switch recomputes from the new provider's env + default, since a configured
/// base url binds to the originally selected provider and must not redirect a
/// different one. The current reasoning is clamped to the new model. Reused by
/// the model picker and Ctrl+P cycling so they switch exactly like `/model`.
pub(crate) fn candidate_for(
    current: &ModelSelection,
    provider: ProviderId,
    model: &str,
) -> ModelSelection {
    let base_url = if provider == current.provider {
        current.base_url.clone()
    } else {
        let settings_base_url = settings_base_url_for_switch(provider);
        selection::base_url_for(provider, settings_base_url.as_deref())
    };
    let reasoning = carried_reasoning(current, provider, model);
    ModelSelection {
        provider,
        model: model.to_string(),
        base_url,
        reasoning,
        cache_retention: current.cache_retention,
        codex_transport: current.codex_transport,
        context_management: current.context_management.clone(),
        legacy_context_management: current.legacy_context_management.clone(),
        tool_result_compaction: current.tool_result_compaction.clone(),
        configured_tool_result_compaction: current.configured_tool_result_compaction.clone(),
        // A runtime model switch keeps the configured retry policy and custom
        // endpoint metadata.
        retry_policy: current.retry_policy,
        open_ai_compatible: current.open_ai_compatible,
    }
}

fn settings_base_url_for_switch(provider: ProviderId) -> Option<String> {
    let settings = std::env::current_dir()
        .ok()
        .and_then(|cwd| config::Settings::load(&cwd).ok())?;
    let configured_provider = settings
        .default_provider
        .as_deref()
        .and_then(|value| ProviderId::parse(value).ok());
    (configured_provider == Some(provider))
        .then_some(settings.base_url)
        .flatten()
}

/// What a candidate selection changes relative to the active one, ordered by
/// context-cost impact (ADR-0041). Reasoning-only changes keep the request
/// prefix byte-identical; a model change resets the provider's model-keyed
/// prompt cache; a provider change additionally re-ingests the whole carried
/// context on an endpoint that has never seen it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitchScope {
    ReasoningOnly,
    Model,
    Provider,
}

fn switch_scope(current: &ModelSelection, candidate: &ModelSelection) -> SwitchScope {
    if current.provider != candidate.provider {
        SwitchScope::Provider
    } else if current.model != candidate.model {
        SwitchScope::Model
    } else {
        SwitchScope::ReasoningOnly
    }
}

/// Carried-context size above which a model/provider switch appends the
/// `/compact` advisory: a quarter of the auto-compaction budget, or a fixed
/// floor when no budget is configured. Below it the re-read is cheap enough
/// that advice would be noise.
fn switch_hint_threshold(budget: Option<u64>) -> u64 {
    budget.map_or(32_000, |budget| budget / 4)
}

/// The advisory line for a switch that will re-read a large carried context
/// uncached, or `None` when the switch is reasoning-only (prefix unchanged) or
/// the context is small enough not to matter (ADR-0041).
fn switch_context_advisory(
    scope: SwitchScope,
    context_tokens: u64,
    budget: Option<u64>,
    model: &str,
) -> Option<String> {
    if scope == SwitchScope::ReasoningOnly || context_tokens < switch_hint_threshold(budget) {
        return None;
    }
    Some(format!(
        "carrying ~{context_tokens} tokens of context to {model}; its prompt cache starts cold, \
so the next request re-reads all of it -- /compact first to hand over a short summary instead.",
    ))
}

pub(crate) fn switch_context_advisory_for<P>(
    candidate: &ModelSelection,
    context_tokens: u64,
    budget: Option<u64>,
    switch: &ModelSwitch<'_, P>,
) -> Option<String> {
    switch_context_advisory(
        switch_scope(&switch.selection, candidate),
        context_tokens,
        budget,
        &candidate.model,
    )
}

/// Validate, rebuild the provider, install it at the safe boundary, and record
/// the audit event. Any failure (unsupported reasoning, build/auth error) leaves
/// the active selection and provider untouched.
pub(crate) fn apply_selection<P: ChatProvider>(
    mut candidate: ModelSelection,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> Vec<String> {
    let scope = switch_scope(&switch.selection, &candidate);
    let carried = carried_reasoning(&switch.selection, candidate.provider, &candidate.model);
    let reasoning_fallback = if scope != SwitchScope::ReasoningOnly
        && switch.selection.reasoning != candidate.reasoning
        && candidate.reasoning == carried
    {
        switch.selection.reasoning.map(|previous| {
            let requested = model_capabilities::display_level(
                switch.selection.provider,
                &switch.selection.model,
                previous,
            );
            let fallback = candidate
                .reasoning
                .map(|level| {
                    model_capabilities::display_level(candidate.provider, &candidate.model, level)
                })
                .unwrap_or("none");
            format!(
                "reasoning '{requested}' is not supported by {}/{}; using '{fallback}'",
                candidate.provider.as_str(),
                candidate.model,
            )
        })
    } else {
        None
    };
    if let Err(error) = candidate.resolve_context_management_for_provider() {
        return vec![format!("{error:#}")];
    }
    if let Err(error) = model_capabilities::validate(&candidate) {
        return vec![format!("{error:#}")];
    }
    let provider = match (switch.build)(&candidate, &switch.system_prompt) {
        Ok(provider) => provider,
        Err(error) => return vec![format!("could not switch: {error:#}")],
    };
    harness.replace_provider(provider);
    if let Some(settings) = switch.compaction_settings.as_ref()
        && let Ok((budget, trigger)) = crate::resolved_compaction_trigger(settings, &candidate)
    {
        harness.set_compaction_trigger(budget, trigger);
    }
    // Install the new lane's cache profile for the fold scheduler (issue
    // #400) before recording the switch, so the A2/A3 break is scheduled
    // against the profile of the lane the next request actually uses.
    harness.set_cache_profile(crate::mimir::selection::cache_profile(&candidate));
    harness.set_tool_result_compaction(candidate.tool_result_compaction.clone());
    let reasoning = candidate.reasoning.map(ReasoningEffort::as_str);
    if let Err(error) =
        harness.record_selection_event(candidate.provider.as_str(), &candidate.model, reasoning)
    {
        tracing::warn!(error = %format!("{error:#}"), "failed to record model selection event");
    }
    let confirm_reasoning = candidate
        .reasoning
        .map(|level| model_capabilities::display_level(candidate.provider, &candidate.model, level))
        .unwrap_or("none");
    let confirm = format!(
        "switched to {}/{} (reasoning: {})",
        candidate.provider.as_str(),
        candidate.model,
        confirm_reasoning,
    );
    let advisory = switch_context_advisory(
        scope,
        harness.context_token_estimate(),
        harness.context_budget(),
        &candidate.model,
    );
    if let Some(cell) = &switch.background_selection {
        *cell.lock().unwrap_or_else(|poison| poison.into_inner()) = candidate.clone();
    }
    switch.selection = candidate;
    let mut lines = Vec::new();
    lines.extend(reasoning_fallback);
    lines.push(confirm);
    lines.extend(advisory);
    lines
}

/// Apply a saved compaction policy to the active session. Rebuilds the provider
/// because an Anthropic-native backend changes request JSON; local-only edits
/// use the same path so selection state cannot drift and revert on `/model`.
pub(crate) fn apply_tool_result_compaction<P: ChatProvider>(
    policy: crate::config::ToolResultCompactionPolicy,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> Result<()> {
    let mut candidate = switch.selection.clone();
    candidate.configured_tool_result_compaction = policy;
    candidate.resolve_context_management_for_provider()?;
    let provider = (switch.build)(&candidate, &switch.system_prompt)?;
    harness.replace_provider(provider);
    harness.set_tool_result_compaction(candidate.tool_result_compaction.clone());
    if let Some(cell) = &switch.background_selection {
        *cell.lock().unwrap_or_else(|poison| poison.into_inner()) = candidate.clone();
    }
    switch.selection = candidate;
    Ok(())
}

/// Slash commands that require the interactive TUI (pickers/modals, or --
/// `/debug` -- the rendered screen itself). In the non-TTY text path these are
/// reported as unavailable instead of being sent to the model; `/model`,
/// `/reasoning`, `/copy`, `/session`, and `/compact` keep working as text
/// commands.
fn tui_only_command(prompt: &str) -> Option<&'static str> {
    match prompt.split_whitespace().next().unwrap_or("") {
        "/scoped-models" => Some("/scoped-models"),
        "/settings" => Some("/settings"),
        "/trust" => Some("/trust"),
        "/resume" => Some("/resume"),
        "/new" => Some("/new"),
        "/login" => Some("/login"),
        "/logout" => Some("/logout"),
        // pi-mono spells it `/debug`; `/dbug` is accepted as an unlisted alias.
        "/debug" | "/dbug" => Some("/debug"),
        _ => None,
    }
}

/// The most recent assistant text reply in the provider-visible context, for
/// `/copy`. Skips reasoning/tool rows so what lands in the clipboard is the
/// reply the user just read, mirroring pi-mono's `getLastAssistantText`.
fn last_assistant_text(messages: &[Message]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|message| message.role == Role::Assistant && !message.content.trim().is_empty())
        .map(|message| message.content.as_str())
}

/// Which assistant output `/copy` should place on the clipboard. `Last` (the
/// default and the pre-existing behavior) is the most recent reply; `All` is
/// every assistant reply in the session, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopySelection {
    Last,
    All,
}

/// Parse the `/copy` argument. Empty or `last` selects the last reply (the
/// documented default that preserves the original `/copy` behavior); `all`
/// selects every assistant reply. Any other token is unrecognized, so the
/// caller shows usage instead of silently copying the wrong thing.
pub(crate) fn parse_copy_selection(rest: &str) -> Option<CopySelection> {
    match rest.trim() {
        "" | "last" => Some(CopySelection::Last),
        "all" => Some(CopySelection::All),
        _ => None,
    }
}

/// The text `/copy <selection>` puts on the clipboard, or `None` when there is
/// no assistant reply yet. `All` joins every non-empty assistant reply with a
/// blank line so the copied block reads as the running transcript of output.
pub(crate) fn copy_selection_text(
    messages: &[Message],
    selection: CopySelection,
) -> Option<String> {
    match selection {
        CopySelection::Last => last_assistant_text(messages).map(str::to_string),
        CopySelection::All => {
            let replies: Vec<&str> = messages
                .iter()
                .filter(|m| m.role == Role::Assistant && !m.content.trim().is_empty())
                .map(|m| m.content.as_str())
                .collect();
            (!replies.is_empty()).then(|| replies.join("\n\n"))
        }
    }
}

/// Usage lines for `/copy`, shown for `/copy help` and any unrecognized option.
fn copy_help_lines() -> Vec<String> {
    vec![
        "/copy [last|all] - copy assistant output to the clipboard".to_string(),
        "  /copy, /copy last - the most recent assistant reply (default)".to_string(),
        "  /copy all         - every assistant reply in this session".to_string(),
        "tip: in pager mode, /mouse toggles terminal-native select/copy".to_string(),
    ]
}

/// `/copy [last|all]`: put the requested assistant output on the system
/// clipboard and report what happened. Shared by the TUI and text front-ends.
/// `rest` is the text typed after `/copy`; empty preserves the original
/// last-reply behavior.
pub(crate) fn copy_command_lines<P: ChatProvider>(harness: &Harness<P>, rest: &str) -> Vec<String> {
    let Some(selection) = parse_copy_selection(rest) else {
        return copy_help_lines();
    };
    let Some(text) = copy_selection_text(harness.messages(), selection) else {
        return vec!["no assistant reply to copy yet.".to_string()];
    };
    let what = match selection {
        CopySelection::Last => "last assistant reply",
        CopySelection::All => "all assistant replies",
    };
    match crate::ui::clipboard::copy(&text) {
        Ok(crate::ui::clipboard::CopyMethod::NativeTool) => {
            vec![format!("copied {what} to the clipboard.")]
        }
        Ok(crate::ui::clipboard::CopyMethod::Osc52) => vec![format!(
            "sent {what} to the clipboard via OSC 52 (requires terminal support)."
        )],
        Err(error) => vec![format!("could not copy: {error:#}")],
    }
}

/// `/session`: read-only session facts (transcript file, id, message counts,
/// context-token estimate, active model). Mirrors pi-mono's `/session` info at
/// the level Iris actually tracks -- estimates, not provider-billed totals.
pub(crate) fn session_info_lines<P: ChatProvider>(
    harness: &Harness<P>,
    switch: &Option<ModelSwitch<'_, P>>,
) -> Vec<String> {
    let mut lines = Vec::new();
    match (harness.session_id(), harness.session_path()) {
        (Some(id), Some(path)) => {
            lines.push(format!("session: {id}"));
            lines.push(format!("file: {}", path.display()));
        }
        _ => lines.push("session: in-memory (not persisted)".to_string()),
    }
    let messages = harness.messages();
    let count = |role: Role| messages.iter().filter(|m| m.role == role).count();
    lines.push(format!(
        "messages: {} ({} user, {} assistant, {} reasoning, {} tool calls, {} tool results)",
        messages.len(),
        count(Role::User),
        count(Role::Assistant),
        count(Role::AssistantReasoning),
        count(Role::AssistantToolCall),
        count(Role::Tool),
    ));
    let budget = match harness.context_budget() {
        Some(budget) => format!("{budget} budget"),
        None => "no budget".to_string(),
    };
    lines.push(format!(
        "context: ~{} tokens estimated ({budget})",
        harness.context_token_estimate(),
    ));
    if let Some(sw) = switch.as_ref() {
        let selection = sw.selection();
        let reasoning = selection
            .reasoning
            .map(|level| {
                model_capabilities::display_level(selection.provider, &selection.model, level)
            })
            .unwrap_or("none");
        lines.push(format!(
            "model: {}/{} (reasoning: {})",
            selection.provider.as_str(),
            selection.model,
            reasoning,
        ));
    }
    lines
}

/// Read one durable compaction entry for `/compaction [n]`. `n` is the
/// 1-based generation ordinal; omission selects the latest entry.
pub(crate) fn compaction_lines<P: ChatProvider>(harness: &Harness<P>, arg: &str) -> Vec<String> {
    match selected_compaction(harness, arg) {
        Ok(entry) => format_compaction_inspection(&entry),
        Err(message) => vec![message],
    }
}

pub(crate) fn selected_compaction<P: ChatProvider>(
    harness: &Harness<P>,
    arg: &str,
) -> std::result::Result<crate::session::CompactionInspection, String> {
    let Some(path) = harness.session_path() else {
        return Err(
            "no persisted session is attached; there are no compaction entries to inspect."
                .to_string(),
        );
    };
    let generation = if arg.trim().is_empty() {
        None
    } else {
        match arg.trim().parse::<u64>().ok().filter(|value| *value > 0) {
            Some(value) => Some(value),
            None => return Err("usage: /compaction [generation]".to_string()),
        }
    };
    let entries = match crate::session::read_compaction_inspections(path) {
        Ok(entries) => entries,
        Err(error) => return Err(format!("could not inspect compaction entries: {error:#}")),
    };
    let selected = generation
        .and_then(|generation| entries.iter().find(|entry| entry.generation == generation))
        .or_else(|| generation.is_none().then(|| entries.last()).flatten());
    let Some(entry) = selected else {
        return Err(match generation {
            Some(generation) => format!("compaction generation {generation} was not found."),
            None => "no compaction entries have been applied in this session.".to_string(),
        });
    };
    Ok(entry.clone())
}

fn format_compaction_inspection(entry: &crate::session::CompactionInspection) -> Vec<String> {
    let (title, detail, summary) = compaction_panel_parts(entry);
    let mut lines = vec![title];
    lines.extend(detail);
    lines.push("  summary".to_string());
    lines.extend(summary.lines().map(|line| format!("    {line}")));
    lines
}

pub(crate) fn compaction_panel_parts(
    entry: &crate::session::CompactionInspection,
) -> (String, Vec<String>, String) {
    let title = format!(
        "compaction generation {}{}",
        entry.generation,
        entry
            .id
            .as_deref()
            .map(|id| format!(" (entry {id})"))
            .unwrap_or_default()
    );
    let mut lines = Vec::new();
    lines.push(format!("  origin             {}", entry.origin));
    lines.push(format!(
        "  covered            {}..{} ({} message(s))",
        entry.covered_from, entry.covered_to, entry.covered_messages
    ));
    lines.push(format!(
        "  tokens             ~{} original -> ~{} summary",
        entry.original_tokens_estimate, entry.summary_tokens_estimate
    ));
    lines.push(format!(
        "  carry paths        {}",
        if entry.carry_paths.is_empty() {
            "none".to_string()
        } else {
            entry.carry_paths.join(", ")
        }
    ));
    lines.push(format!(
        "  instructions       {}",
        entry.instructions.as_deref().unwrap_or("none")
    ));
    lines.push(format!(
        "  recall handle      {}",
        entry.recall_handle.as_deref().unwrap_or("unavailable")
    ));
    lines.push(format!("  provider blocks    {}", entry.provider_blocks));
    match &entry.worker_usage {
        Some(usage) => {
            // Shared share arithmetic (half-up, capped, overflow-safe); a
            // zero-input ratio stays "unknown" rather than a fabricated 0%.
            let cache =
                crate::metrics::ratio_percent(usage.cache_read_input_tokens, usage.input_tokens)
                    .map(|percent| format!("{percent}%"))
                    .unwrap_or_else(|| "unknown".to_string());
            lines.push(format!(
                "  worker             {}/{}; input {} / output {} / reasoning {} / total {}; cache read {} ({cache}) / write {}",
                usage.provider,
                usage.model,
                usage.input_tokens,
                usage.output_tokens,
                usage.reasoning_output_tokens,
                usage.total_tokens,
                usage.cache_read_input_tokens,
                usage.cache_write_input_tokens,
            ));
            if let Some(creation) = &usage.cache_creation {
                lines.push(format!(
                    "  cache creation     5m {} / 1h {}",
                    creation.ephemeral_5m_input_tokens, creation.ephemeral_1h_input_tokens
                ));
            }
        }
        None => lines.push(
            "  worker             not reported (deterministic or usage-blind lane)".to_string(),
        ),
    }
    (title, lines, entry.summary.clone())
}

/// Render the deterministic `/sessions <task-id>` lookup (ADR-0031): the
/// sessions in `cwd`'s slug directory whose logs carry the task id, each
/// followed by its deterministic extraction (user-message previews + task
/// lifecycle events, in on-disk order). No summarization and no model call --
/// display-only audit text. Opens the default session store; a store failure is
/// reported as a line rather than surfaced as an error to the caller.
pub(crate) fn sessions_for_task_lines(cwd: &std::path::Path, task_id: &str) -> Vec<String> {
    match SessionStore::open_default() {
        Ok(store) => sessions_for_task_report(&store, cwd, task_id),
        Err(error) => vec![format!("session store unavailable: {error:#}")],
    }
}

/// Format one `sessions_for_task` lookup against an explicit store, so the
/// rendering is unit-tested without env/home state.
fn sessions_for_task_report(
    store: &SessionStore,
    cwd: &std::path::Path,
    task_id: &str,
) -> Vec<String> {
    let matches = match store.sessions_for_task(cwd, task_id) {
        Ok(matches) => matches,
        Err(error) => return vec![format!("could not scan sessions: {error:#}")],
    };
    if matches.is_empty() {
        return vec![format!("no sessions found for task {task_id}")];
    }
    let mut lines = vec![format!("sessions for task {task_id}: {}", matches.len())];
    for m in &matches {
        lines.push(format!("session {}", m.id));
        match session::extract_session(&m.path) {
            Ok(extract) => {
                for item in &extract.items {
                    lines.push(render_extract_item(item));
                }
            }
            Err(error) => lines.push(format!("  (could not read session: {error:#})")),
        }
    }
    lines
}

/// One extraction item as a single display line: an indented user-message
/// preview or a bracketed task lifecycle event. Bounded by construction (the
/// extraction only ever holds previews, never full bodies).
fn render_extract_item(item: &session::ExtractItem) -> String {
    match item {
        session::ExtractItem::UserPreview(preview) => format!("  > {preview}"),
        session::ExtractItem::Lifecycle(ev) => match ev.event.as_str() {
            "opened" => format!(
                "  [task opened] {}",
                ev.body.as_deref().unwrap_or("(no description)")
            ),
            "settled" => format!(
                "  [task settled] {}",
                ev.disposition.as_deref().unwrap_or("(unknown)")
            ),
            other => format!("  [task {other}]"),
        },
    }
}

/// Read-only `/model` view: current provider/model/reasoning + supported levels.
fn current_selection_lines(selection: &ModelSelection) -> Vec<String> {
    let levels = model_capabilities::selectable_options(
        selection.provider,
        &selection.model,
        selection.open_ai_compatible.reasoning,
    )
    .iter()
    .map(|option| option.label)
    .collect::<Vec<_>>()
    .join(", ");
    let reasoning = selection
        .reasoning
        .map(|level| model_capabilities::display_level(selection.provider, &selection.model, level))
        .unwrap_or("none");
    let wire = selection
        .reasoning
        .map(|level| model_capabilities::wire_behavior(selection.provider, &selection.model, level))
        .unwrap_or("provider default (reasoning omitted)");
    vec![
        format!(
            "{}/{} (reasoning: {})",
            selection.provider.as_str(),
            selection.model,
            reasoning,
        ),
        format!("supported reasoning levels: {levels}"),
        format!("wire behavior: {wire}"),
    ]
}

/// Entry point for the interactive session. Selects the front-end: the
/// persistent terminal-surface TUI when both stdin and stdout are terminals and
/// the plain renderer was not requested, otherwise the blocking, ANSI-free text
/// UI (also used for pipes/CI). `force_plain` carries the `--plain` flag.
pub(crate) fn run_interactive<P: ChatProvider>(
    harness: &mut Harness<P>,
    switch: &mut Option<ModelSwitch<'_, P>>,
    force_plain: bool,
    tui_settings: Option<&crate::config::TuiSettings>,
    swap: &SessionLoader<'_>,
    startup: StartupUi,
) -> Result<()> {
    if !prefers_text_ui(force_plain) {
        // Screen-mode policy (ADR-0029): pager vs inline, resolved once per
        // startup. Degradation/config notices land in the transcript as
        // ordinary notices so honesty costs no new UI surface.
        let alt_screen = tui_settings.and_then(|tui| tui.alt_screen.as_deref());
        let resolution = crate::ui::screen_mode::resolve_for_startup(alt_screen);
        match TuiUi::new(resolution.mode) {
            Ok(mut tui) => {
                // Apply the persisted color theme once the TUI is live
                // (ADR-0042); an invalid id logs a warning and falls back to the
                // adaptive default. Kept inside the TUI branch so
                // NO_COLOR/`--plain`/pipe users on the plain text renderer never
                // activate a fixed-RGB theme.
                if let Some(theme) = tui_settings.and_then(|t| t.theme.as_deref()) {
                    crate::ui::theme::set_active(theme);
                }
                if let Some(speed) = tui_settings.and_then(|tui| tui.scroll_speed) {
                    tui.screen.scroll_speed = speed.clamp(1, 100);
                }
                // Reduced motion: the env flag OR the persisted preference. Env
                // still wins (it is OR-ed in the resolver).
                tui.screen
                    .set_reduced_motion(crate::config::reduced_motion_enabled(
                        tui_settings.and_then(|tui| tui.reduced_motion),
                    ));
                for notice in resolution.notices {
                    tui.screen.apply(crate::ui::UiEvent::Notice(notice));
                }
                for notice in &startup.notices {
                    tui.screen.apply(crate::ui::UiEvent::Notice(notice.clone()));
                }
                return run_tui(harness, tui, switch, swap, startup);
            }
            Err(error) => {
                if startup.modal.is_some() {
                    bail!(
                        "could not open resume picker because the TUI is unavailable: {error:#}; run `iris resume --plain` to list sessions"
                    );
                }
                tracing::warn!(error = %format!("{error:#}"), "TUI unavailable; using text UI");
            }
        }
    }
    // The text/plain path has no modal surface. Bare `iris resume` is routed to
    // a list before this point whenever the plain renderer is requested or stdio
    // is not a terminal; if TUI creation fails after requesting a startup modal,
    // the branch above errors instead of silently starting a fresh session.
    let mut ui = crate::ui::text::TextUi::stdio();
    for notice in startup.notices {
        ui.emit(crate::ui::UiEvent::Notice(notice))?;
    }
    run_session(harness, &mut ui, switch)
}

/// Whether the interactive entry point will fall back to the plain, ANSI-free
/// text UI rather than the terminal-surface TUI: the plain renderer was
/// requested (`--plain`, `IRIS_PLAIN`, or `NO_COLOR`) or either stdio end is not
/// a terminal. `main.rs` consults this so `iris resume` (no id) prints a plain
/// session list in exactly the cases the picker would be unavailable.
pub(crate) fn prefers_text_ui(force_plain: bool) -> bool {
    use_plain_renderer(force_plain)
        || !std::io::stdout().is_terminal()
        || !std::io::stdin().is_terminal()
}

/// Whether to bypass the interactive TUI for the plain, ANSI-free text UI so the
/// accessible renderer is reachable on a real terminal, not only on a pipe: true
/// when `--plain` was passed, `IRIS_PLAIN` is set, or `NO_COLOR` is present.
fn use_plain_renderer(force_plain: bool) -> bool {
    force_plain || crate::config::iris_flag_enabled("IRIS_PLAIN") || no_color_requested()
}

/// `NO_COLOR` per no-color.org: honored when set to any non-empty value.
fn no_color_requested() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
}

/// Drive the terminal-surface TUI: owns a Tier-3 current-thread runtime, hands
/// it to the async event loop, and bounds shutdown like the text driver does.
fn run_tui<P: ChatProvider>(
    harness: &mut Harness<P>,
    tui: TuiUi,
    switch: &mut Option<ModelSwitch<'_, P>>,
    swap: &SessionLoader<'_>,
    startup: StartupUi,
) -> Result<()> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let result = crate::ui::tui_loop::run(harness, &runtime, tui, switch, swap, startup);
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

/// Drive one headless `--print` turn-sequence. Owns the same Tier-3 runtime and
/// Ctrl-C watcher as the interactive driver, but runs a single `submit_turn`
/// with the caller's observer and approval gate instead of a prompt loop, so a
/// non-interactive run still races the provider stream and tools against
/// cancellation and cannot hang on exit.
pub(crate) fn run_print_turn<P: ChatProvider>(
    harness: &mut Harness<P>,
    prompt: &str,
    obs: &dyn AgentObserver,
    gate: &dyn ApprovalGate,
) -> Result<()> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    // Clear any stale interrupt before arming the watcher so a Ctrl-C from before
    // this run cannot cancel the turn immediately.
    crate::signals::reset();
    let token = CancellationToken::new();
    let done = Arc::new(AtomicBool::new(false));
    let watcher = std::thread::spawn({
        let token = token.clone();
        let done = Arc::clone(&done);
        move || watch_for_interrupt(&token, &done)
    });
    let result = runtime.block_on(harness.submit_turn(prompt, obs, gate, &token));
    if result
        .as_ref()
        .is_ok_and(|outcome| outcome.allows_print_settlement())
    {
        harness.accept_print_checkpoint();
    }
    done.store(true, Ordering::Relaxed);
    let _ = watcher.join();
    // Bound shutdown so an orphaned blocking provider request cannot hang exit.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result.map(|_| ())
}

/// Drive the interactive REPL. Owns the Tier-3 runtime: a current-thread tokio
/// runtime that `block_on`s each turn, plus a per-turn cancellation token that a
/// background watcher thread trips when the user presses Ctrl-C. The blocking
/// stdin reads and rendering stay synchronous; only the turn itself runs on the
/// runtime so provider streams and tools are raced against cancellation.
pub(crate) fn run_session<P: ChatProvider>(
    harness: &mut Harness<P>,
    ui: &mut dyn Ui,
    switch: &mut Option<ModelSwitch<'_, P>>,
) -> Result<()> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let result = run_session_inner(harness, ui, &runtime, switch);
    // Bound shutdown so an orphaned blocking provider request (the loop dropped
    // its stream on cancel) cannot hang process exit, including early error exits.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

fn run_session_inner<P: ChatProvider>(
    harness: &mut Harness<P>,
    ui: &mut dyn Ui,
    runtime: &Runtime,
    switch: &mut Option<ModelSwitch<'_, P>>,
) -> Result<()> {
    ui.emit(UiEvent::SessionStarted)?;

    while let Some(prompt) = ui.next_prompt()? {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            continue;
        }
        if slash::is_exit(prompt) {
            break;
        }
        // Read-only session commands work the same here as in the TUI and never
        // start a turn.
        let (cmd, rest) = match prompt.split_once(char::is_whitespace) {
            Some((cmd, rest)) => (cmd, rest.trim()),
            None => (prompt, ""),
        };
        if cmd == "/copy" {
            for line in copy_command_lines(harness, rest) {
                ui.emit(UiEvent::Notice(line))?;
            }
            continue;
        }
        if prompt == "/session" {
            for line in session_info_lines(harness, switch) {
                ui.emit(UiEvent::Notice(line))?;
            }
            continue;
        }
        if cmd == "/compaction" {
            for line in compaction_lines(harness, rest) {
                ui.emit(UiEvent::Notice(line))?;
            }
            continue;
        }
        // On-demand compaction at this safe inter-turn boundary. Driven like a
        // turn (runtime + Ctrl-C watcher) because the provider-backed
        // summarizer awaits a cancellable model request.
        if cmd == "/compact" {
            crate::signals::reset();
            let token = CancellationToken::new();
            let done = Arc::new(AtomicBool::new(false));
            let watcher = std::thread::spawn({
                let token = token.clone();
                let done = Arc::clone(&done);
                move || watch_for_interrupt(&token, &done)
            });
            let result = {
                let bridge = UiBridge::new(ui);
                runtime.block_on(harness.compact_now_with_focus(
                    &bridge,
                    &token,
                    (!rest.is_empty()).then_some(rest),
                ))
            };
            done.store(true, Ordering::Relaxed);
            let _ = watcher.join();
            if let Err(error) = result {
                ui.emit(UiEvent::Notice(format!("could not compact: {error:#}")))?;
            }
            continue;
        }
        // TUI-only commands (pickers/modals, `/debug`) are a no-op status in the
        // non-TTY text path rather than being sent to the model. `/model` and
        // `/reasoning` keep their existing text behavior below.
        if let Some(name) = tui_only_command(prompt) {
            ui.emit(UiEvent::Notice(format!(
                "{name} is only available in the interactive TUI"
            )))?;
            continue;
        }
        // Mode switches are handled at this safe inter-turn boundary, never sent
        // to the model. The shared handler validates/rebuilds/records and returns
        // the lines to display.
        if let Some(lines) = handle_model_command(prompt, harness, switch) {
            for line in lines {
                ui.emit(UiEvent::Notice(line))?;
            }
            continue;
        }
        if let Some(lines) = handle_task_command(prompt, harness) {
            for line in lines {
                ui.emit(UiEvent::Notice(line))?;
            }
            continue;
        }
        // Permission mode (ADR-0032 + ADR-0049). A real session control,
        // meaningful in the non-TTY path too (e.g. `never` for read-only runs or
        // dangerous skip in a sandbox), handled at this safe boundary and never
        // sent to the model.
        if prompt == "/approval" || prompt.starts_with("/approval ") {
            let rest = prompt["/approval".len()..].trim();
            let lines = if rest.is_empty() {
                vec![format!(
                    "approval mode: {} (use /approval {APPROVAL_USAGE})",
                    current_permission_token(harness)
                )]
            } else {
                match PermissionMode::parse(rest) {
                    Some(mode) => apply_permission_mode(harness, mode),
                    None => vec![format!(
                        "unknown approval mode `{rest}` (use {APPROVAL_USAGE})"
                    )],
                }
            };
            for line in lines {
                ui.emit(UiEvent::Notice(line))?;
            }
            continue;
        }
        if prompt == "/tasks" || prompt.starts_with("/tasks ") {
            let lines = handle_tasks_command(prompt, harness).unwrap_or_else(|| {
                vec!["/tasks is only available in the interactive TUI".to_string()]
            });
            for line in lines {
                ui.emit(UiEvent::Notice(line))?;
            }
            continue;
        }
        // The final task diff (issue #264): render the net diff on demand at this
        // safe boundary. Emits a colorized/plain diff event, not just notices.
        if prompt.trim() == "/diff" {
            ui.emit(task_diff_event(harness))?;
            continue;
        }
        // Checkpoint/rollback commands (issue #263) settle or restore the current
        // Iris task at this same safe boundary.
        if let Some(lines) = handle_checkpoint_command(prompt, harness) {
            for line in lines {
                ui.emit(UiEvent::Notice(line))?;
            }
            continue;
        }

        // Clear any stale interrupt BEFORE arming the watcher, so a Ctrl-C left
        // over from the idle prompt cannot cancel this fresh turn immediately.
        crate::signals::reset();
        let token = CancellationToken::new();
        let done = Arc::new(AtomicBool::new(false));
        let watcher = std::thread::spawn({
            let token = token.clone();
            let done = Arc::clone(&done);
            move || watch_for_interrupt(&token, &done)
        });

        // One bridge per turn backs both Nexus seams (observer + approval gate)
        // from two shared borrows; it drops here so `ui` is free for the
        // session-driver events below.
        let result = {
            let bridge = UiBridge::new(ui);
            runtime.block_on(harness.submit_turn(prompt, &bridge, &bridge, &token))
        };
        // Stop and join the watcher so it cannot leak past the turn or trip the
        // next one.
        done.store(true, Ordering::Relaxed);
        let _ = watcher.join();

        if let Err(error) = result {
            ui.emit(UiEvent::from_turn_error(&error))?;
        }
    }

    ui.shutdown()?;
    Ok(())
}

/// Bridge the async-signal-safe SIGINT handler (which only flips a global
/// atomic) onto a turn's [`CancellationToken`], running on its own OS thread so
/// it can trip the token even while a synchronous blocking tool occupies the
/// runtime's executor thread.
///
/// ponytail: 20ms poll. A self-pipe/eventfd would remove the poll, but for
/// human-scale Ctrl-C latency the poll is enough; it stops as soon as the turn
/// finishes (`done`).
fn watch_for_interrupt(token: &CancellationToken, done: &AtomicBool) {
    while !done.load(Ordering::Relaxed) {
        if crate::signals::interrupted() {
            token.cancel();
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Result, anyhow};
    use std::{cell::RefCell, env};

    use crate::mimir::test_support::ConfigPathGuard;
    use crate::nexus::{Agent, AssistantTurn, Message, ProviderEvent, ProviderStream, Tools};
    use crate::ui::text::TextUi;
    use crate::wayland::Harness;

    struct FakeProvider {
        responses: RefCell<Vec<Result<AssistantTurn, String>>>,
    }

    impl FakeProvider {
        fn new(responses: Vec<Result<AssistantTurn, &str>>) -> Self {
            Self {
                responses: RefCell::new(
                    responses
                        .into_iter()
                        .map(|result| result.map_err(str::to_string))
                        .rev()
                        .collect(),
                ),
            }
        }
    }

    impl ChatProvider for FakeProvider {
        fn respond_stream<'a>(
            &'a self,
            _messages: &'a [Message],
            _tools: &'a Tools,
            _cancel: &'a CancellationToken,
        ) -> Result<ProviderStream<'a>> {
            let item = match self.responses.borrow_mut().pop() {
                Some(Ok(turn)) => Ok(ProviderEvent::Completed(turn)),
                Some(Err(error)) => Err(anyhow!(error)),
                None => Err(anyhow!("unexpected call")),
            };
            Ok(Box::pin(futures::stream::once(async move { item })))
        }
    }

    /// Build an in-memory harness + a `ModelSwitch` whose builder yields a
    /// throwaway provider. The closure is returned alongside so the caller keeps
    /// it alive for the borrow inside `ModelSwitch`.
    fn fake_harness() -> (Harness<FakeProvider>, crate::tools::test_support::TestDir) {
        let dir = crate::tools::test_support::temp_dir();
        let agent = Agent::new(FakeProvider::new(vec![]), crate::tools::built_in_tools());
        let harness = Harness::new(
            agent,
            dir.path.clone(),
            crate::tools::ToolState::new(),
            None,
            None,
        );
        (harness, dir)
    }

    fn selection(provider: ProviderId, model: &str) -> ModelSelection {
        ModelSelection {
            provider,
            model: model.to_string(),
            base_url: "https://example".to_string(),
            reasoning: None,
            cache_retention: selection::PromptCacheRetention::Short,
            codex_transport: selection::CodexTransport::Auto,
            context_management: selection::ContextManagement::default(),
            legacy_context_management: selection::ContextManagement::default(),
            tool_result_compaction: crate::config::Settings::default()
                .tool_result_compaction()
                .unwrap(),
            configured_tool_result_compaction: crate::config::Settings::default()
                .tool_result_compaction()
                .unwrap(),
            retry_policy: crate::mimir::retry::RetryPolicy::default(),
            open_ai_compatible: selection::OpenAiCompatibleConfig::default(),
        }
    }

    struct EnvVarGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
            let prev = env::var(key).ok();
            // SAFETY: callers hold ConfigPathGuard, which serializes test env
            // mutation through the shared mimir env lock. This guard is declared
            // after ConfigPathGuard so it restores before that lock is released.
            unsafe { env::remove_var(key) };
            Self { key, prev }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(prev) => unsafe { env::set_var(self.key, prev) },
                None => unsafe { env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn provider_native_warning_is_emitted_only_for_explicit_opt_in() {
        assert!(provider_native_compaction_notices(false).is_empty());
        assert_eq!(
            provider_native_compaction_notices(true),
            vec![PROVIDER_NATIVE_COMPACTION_WARNING.to_string()]
        );
    }

    #[test]
    fn sessions_for_task_report_lists_matches_with_deterministic_extraction() {
        let dir = crate::tools::test_support::temp_dir();
        let mut log = SessionLog::create_in(&dir.path, std::path::Path::new("/proj")).unwrap();
        let id = log.id().to_string();
        log.append(&Message::user("please fix the login bug"))
            .unwrap();
        log.append_task_opened("task-7", Some("please fix the login bug"))
            .unwrap();
        log.append(&Message::assistant("done")).unwrap();
        log.append_task_settled("task-7", "accepted").unwrap();
        drop(log);

        let store = SessionStore::with_root(dir.path.clone());
        let lines = sessions_for_task_report(&store, std::path::Path::new("/proj"), "task-7");
        assert_eq!(
            lines,
            vec![
                "sessions for task task-7: 1".to_string(),
                format!("session {id}"),
                "  > please fix the login bug".to_string(),
                "  [task opened] please fix the login bug".to_string(),
                "  [task settled] accepted".to_string(),
            ]
        );

        // Unknown task id renders a single not-found line, not an error.
        assert_eq!(
            sessions_for_task_report(&store, std::path::Path::new("/proj"), "nope"),
            vec!["no sessions found for task nope".to_string()]
        );
    }

    #[test]
    fn session_id_guard_restores_uncommitted_swap_and_keeps_committed_one() {
        let cell = Rc::new(RefCell::new("old".to_string()));
        {
            let _guard = SessionIdGuard::swap(cell.clone(), "new".to_string());
            assert_eq!(&*cell.borrow(), "new");
        }
        assert_eq!(&*cell.borrow(), "old");

        {
            let mut guard = SessionIdGuard::swap(cell.clone(), "committed".to_string());
            guard.commit();
        }
        assert_eq!(&*cell.borrow(), "committed");
    }

    #[test]
    fn session_id_guard_keeps_background_factory_cell_transactional() {
        let cell = Rc::new(RefCell::new("old".to_string()));
        let background = Arc::new(Mutex::new("old".to_string()));
        {
            let _guard = SessionIdGuard::swap_with_background(
                cell.clone(),
                background.clone(),
                "new".to_string(),
            );
            assert_eq!(&*cell.borrow(), "new");
            assert_eq!(
                background
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .as_str(),
                "new"
            );
        }
        assert_eq!(&*cell.borrow(), "old");
        assert_eq!(
            background
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_str(),
            "old"
        );

        {
            let mut guard = SessionIdGuard::swap_with_background(
                cell.clone(),
                background.clone(),
                "committed".to_string(),
            );
            guard.commit();
        }
        assert_eq!(&*cell.borrow(), "committed");
        assert_eq!(
            background
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .as_str(),
            "committed"
        );
    }

    #[test]
    fn last_assistant_text_prefers_latest_nonempty_text_reply() {
        assert_eq!(last_assistant_text(&[]), None);
        assert_eq!(last_assistant_text(&[Message::user("question")]), None);
        let messages = vec![
            Message::user("q1"),
            Message::assistant("a1"),
            Message::user("q2"),
            Message::assistant("a2"),
            // Trailing non-text rows (reasoning, blank text) are skipped.
            Message::assistant("   "),
        ];
        assert_eq!(last_assistant_text(&messages), Some("a2"));
    }

    #[test]
    fn copy_command_reports_missing_assistant_reply() {
        let (harness, _dir) = fake_harness();
        assert_eq!(
            copy_command_lines(&harness, ""),
            vec!["no assistant reply to copy yet.".to_string()]
        );
        // The `all` variant also has nothing to copy in an empty session.
        assert_eq!(
            copy_command_lines(&harness, "all"),
            vec!["no assistant reply to copy yet.".to_string()]
        );
    }

    #[test]
    fn parse_copy_selection_maps_documented_variants() {
        assert_eq!(parse_copy_selection(""), Some(CopySelection::Last));
        assert_eq!(parse_copy_selection("  "), Some(CopySelection::Last));
        assert_eq!(parse_copy_selection("last"), Some(CopySelection::Last));
        assert_eq!(parse_copy_selection("all"), Some(CopySelection::All));
        // Unrecognized options fall through to usage, not a wrong copy.
        assert_eq!(parse_copy_selection("help"), None);
        assert_eq!(parse_copy_selection("everything"), None);
    }

    #[test]
    fn copy_selection_text_selects_last_or_all_replies() {
        let messages = vec![
            Message::user("q1"),
            Message::assistant("a1"),
            Message::user("q2"),
            Message::assistant("a2"),
            // Blank/non-text rows are skipped in both selections.
            Message::assistant("   "),
        ];
        assert_eq!(
            copy_selection_text(&messages, CopySelection::Last),
            Some("a2".to_string())
        );
        assert_eq!(
            copy_selection_text(&messages, CopySelection::All),
            Some("a1\n\na2".to_string())
        );
        assert_eq!(copy_selection_text(&[], CopySelection::Last), None);
        assert_eq!(copy_selection_text(&[], CopySelection::All), None);
    }

    #[test]
    fn copy_command_shows_usage_for_unknown_option() {
        let (harness, _dir) = fake_harness();
        let lines = copy_command_lines(&harness, "nope");
        assert_eq!(
            lines.first().map(String::as_str),
            Some("/copy [last|all] - copy assistant output to the clipboard")
        );
    }

    #[test]
    fn session_info_lines_report_in_memory_session_counts_and_model() {
        let dir = crate::tools::test_support::temp_dir();
        let messages = vec![Message::user("hi"), Message::assistant("hello")];
        let agent = Agent::resumed(
            FakeProvider::new(vec![]),
            crate::tools::built_in_tools(),
            messages,
        );
        let harness = Harness::new(
            agent,
            dir.path.clone(),
            crate::tools::ToolState::new(),
            None,
            Some(1000),
        );
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        let switch = Some(ModelSwitch::new(
            selection(ProviderId::OpenAiCodex, "gpt-5.5"),
            "PROMPT".to_string(),
            &build,
            None,
        ));
        let lines = session_info_lines(&harness, &switch);
        assert_eq!(lines[0], "session: in-memory (not persisted)");
        assert!(
            lines[1].starts_with("messages: 2 (1 user, 1 assistant"),
            "{lines:?}"
        );
        assert!(lines[2].contains("1000 budget"), "{lines:?}");
        assert!(
            lines[3].contains("openai-codex/gpt-5.5 (reasoning: none)"),
            "{lines:?}"
        );
    }

    #[test]
    fn session_info_lines_report_persisted_file_and_id() {
        let dir = crate::tools::test_support::temp_dir();
        let log = SessionLog::create_in(&dir.path, &dir.path).expect("session log");
        let id = log.id().to_string();
        let path = log.path().to_path_buf();
        let agent = Agent::new(FakeProvider::new(vec![]), crate::tools::built_in_tools());
        let harness = Harness::new(
            agent,
            dir.path.clone(),
            crate::tools::ToolState::new(),
            Some(log),
            None,
        );
        let lines = session_info_lines(&harness, &None);
        assert_eq!(lines[0], format!("session: {id}"));
        assert_eq!(lines[1], format!("file: {}", path.display()));
        assert!(lines[3].contains("no budget"), "{lines:?}");
    }

    #[test]
    fn compaction_lines_render_latest_detail_and_generation_lookup() {
        let dir = crate::tools::test_support::temp_dir();
        let mut log = SessionLog::create_in(&dir.path, &dir.path).expect("session log");
        let from = log
            .append(&Message::user(&"old context ".repeat(20)))
            .unwrap();
        let to = log.append(&Message::assistant("old answer")).unwrap();
        log.append_compaction_with_metadata(
            &from,
            &to,
            "Goal: preserve NEEDLE.\n\n[recall] recall(handle=\"abc123\").",
            &["src/lib.rs".to_string()],
            None,
            Some(12),
            crate::nexus::CompactionOrigin::Excerpts,
            None,
            Some("preserve NEEDLE"),
        )
        .unwrap();
        let agent = Agent::new(FakeProvider::new(vec![]), crate::tools::built_in_tools());
        let harness = Harness::new(
            agent,
            dir.path.clone(),
            crate::tools::ToolState::new(),
            Some(log),
            None,
        );

        let latest = compaction_lines(&harness, "").join("\n");
        assert!(latest.contains("compaction generation 1"), "{latest}");
        assert!(
            latest.contains(&format!("{from}..{to} (2 message(s))")),
            "{latest}"
        );
        assert!(latest.contains("src/lib.rs"), "{latest}");
        assert!(latest.contains("preserve NEEDLE"), "{latest}");
        assert!(latest.contains("abc123"), "{latest}");
        assert!(latest.contains("Goal: preserve NEEDLE."), "{latest}");

        assert!(compaction_lines(&harness, "2")[0].contains("was not found"));
        assert_eq!(
            compaction_lines(&harness, "nope"),
            vec!["usage: /compaction [generation]"]
        );
    }

    #[test]
    fn checkpoint_command_routes_and_ignores_others() {
        let (mut harness, _dir) = fake_harness();
        // Non-checkpoint lines fall through so the caller sends them as prompts.
        assert!(handle_checkpoint_command("hello there", &mut harness).is_none());
        assert!(handle_checkpoint_command("/model", &mut harness).is_none());
        // With no active task, each command reports the empty state (never panics
        // or sends the line to the model).
        assert_eq!(
            handle_checkpoint_command("/accept", &mut harness).unwrap(),
            vec!["no unreviewed Iris changes to accept".to_string()]
        );
        assert_eq!(
            handle_checkpoint_command("/rollback", &mut harness).unwrap(),
            vec!["no unreviewed Iris changes to roll back".to_string()]
        );
        assert_eq!(
            handle_checkpoint_command("/checkpoint", &mut harness).unwrap(),
            vec!["no Iris changes to checkpoint".to_string()]
        );
    }

    #[test]
    fn mutation_safety_master_controls_effective_task_workflow() {
        let (mut harness, _dir) = fake_harness();
        harness.set_task_workflow_enabled(true).unwrap();
        harness.configure_mutation_safety(false, false).unwrap();
        assert!(!harness.mutation_safety_enabled());
        assert!(!harness.task_workflow_enabled());

        harness.configure_mutation_safety(true, false).unwrap();
        assert!(harness.mutation_safety_enabled());
        assert!(harness.task_workflow_enabled());
    }

    #[test]
    fn task_commands_surface_workflow_off_hint_and_enable_project_setting() {
        let (mut harness, dir) = fake_harness();
        harness.set_task_workflow_enabled(false).unwrap();

        assert_eq!(
            handle_tasks_command("/tasks", &mut harness).unwrap(),
            vec![TASK_WORKFLOW_OFF_NOTICE.to_string()]
        );
        assert_eq!(
            handle_checkpoint_command("/accept", &mut harness).unwrap(),
            vec![TASK_WORKFLOW_OFF_NOTICE.to_string()]
        );
        assert!(matches!(
            task_diff_event(&harness),
            UiEvent::Notice(message) if message == TASK_WORKFLOW_OFF_NOTICE
        ));

        assert_eq!(
            handle_tasks_command("/tasks enable", &mut harness).unwrap(),
            vec!["task workflow enabled for this project".to_string()]
        );
        assert!(harness.task_workflow_enabled());
        let saved = config::Settings::load(&dir.path).unwrap();
        assert!(saved.tasks());
    }

    #[test]
    fn task_help_command_surfaces_v2_workflow_copy() {
        let (harness, _dir) = fake_harness();
        let lines = handle_task_command("/task help", &harness).expect("task help");
        assert!(lines[0].contains("task workflow:"), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|line| line.contains("/checkpoint") && line.contains("keep working")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("/tasks") && line.contains("resume an interrupted task")),
            "{lines:?}"
        );
    }

    #[test]
    fn model_transition_preserves_supported_effort_and_clamps_unsupported_effort() {
        let mut current = selection(ProviderId::OpenAiCodex, "gpt-5.5");
        current.reasoning = Some(ReasoningEffort::High);
        let preserved = candidate_for(&current, ProviderId::Anthropic, "claude-sonnet-4-6");
        assert_eq!(preserved.reasoning, Some(ReasoningEffort::High));

        current.model = "gpt-5.6-sol".to_string();
        current.reasoning = Some(ReasoningEffort::Max);
        let clamped = candidate_for(&current, ProviderId::OpenAiCodex, "gpt-5.5");
        assert_eq!(
            clamped.reasoning,
            Some(ReasoningEffort::XHigh),
            "pre-5.6 Codex models must not receive max"
        );
    }

    #[test]
    fn model_transition_reports_reasoning_fallback() {
        let (mut harness, _dir) = fake_harness();
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        let mut active = selection(ProviderId::OpenAiCodex, "gpt-5.6-sol");
        active.reasoning = Some(ReasoningEffort::Max);
        let mut switch = Some(ModelSwitch::new(active, "PROMPT".to_string(), &build, None));

        let lines = handle_model_command("/model openai-codex/gpt-5.5", &mut harness, &mut switch)
            .expect("handled");
        assert!(
            lines.iter().any(|line| {
                line.contains("reasoning 'max' is not supported") && line.contains("using 'xhigh'")
            }),
            "the clamp must be visible: {lines:?}"
        );
    }

    #[test]
    fn explicit_model_effort_does_not_report_a_carried_fallback() {
        let (mut harness, _dir) = fake_harness();
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        let mut active = selection(ProviderId::OpenAiCodex, "gpt-5.5");
        active.reasoning = Some(ReasoningEffort::High);
        let mut switch = ModelSwitch::new(active, "PROMPT".to_string(), &build, None);
        let mut candidate = candidate_for(
            switch.selection(),
            ProviderId::Anthropic,
            "claude-sonnet-4-6",
        );
        candidate.reasoning = Some(ReasoningEffort::Medium);

        let lines = apply_selection(candidate, &mut harness, &mut switch);

        assert!(
            lines.iter().all(|line| !line.contains("is not supported")),
            "an explicit supported effort is not a fallback: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("reasoning: 10,240 tokens")),
            "the selected effort is applied: {lines:?}"
        );
    }

    #[test]
    fn model_command_shows_current_selection_and_supported_levels() {
        let (mut harness, _dir) = fake_harness();
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        let mut switch = Some(ModelSwitch::new(
            selection(ProviderId::OpenAiCodex, "gpt-5.5"),
            "PROMPT".to_string(),
            &build,
            None,
        ));
        let lines = handle_model_command("/model", &mut harness, &mut switch).expect("handled");
        assert!(lines[0].contains("openai-codex/gpt-5.5"), "{lines:?}");
        assert!(lines[0].contains("reasoning: none"), "{lines:?}");
        assert!(
            lines[1].contains("off, minimal, low, medium, high, xhigh"),
            "{lines:?}"
        );
        assert_eq!(
            lines[2], "wire behavior: provider default (reasoning omitted)",
            "CLI status exposes the active capability's wire behavior"
        );
    }

    #[test]
    fn model_status_honors_openai_compatible_reasoning_gate() {
        let mut selected = selection(ProviderId::OpenAiCompatible, "custom-model");
        let disabled = current_selection_lines(&selected);
        assert_eq!(disabled[1], "supported reasoning levels: off");

        selected.open_ai_compatible.reasoning = true;
        let enabled = current_selection_lines(&selected);
        assert_eq!(
            enabled[1],
            "supported reasoning levels: off, low, medium, high"
        );
    }

    #[test]
    fn model_command_switches_provider_and_model_by_exact_id() {
        let (mut harness, _dir) = fake_harness();
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        let mut switch = Some(ModelSwitch::new(
            selection(ProviderId::OpenAiCodex, "gpt-5.5"),
            "PROMPT".to_string(),
            &build,
            None,
        ));
        let lines = handle_model_command(
            "/model anthropic/claude-sonnet-4-6",
            &mut harness,
            &mut switch,
        )
        .expect("handled");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("switched to anthropic/claude-sonnet-4-6")),
            "{lines:?}"
        );
        let switched = switch.as_ref().unwrap();
        assert_eq!(switched.selection.provider, ProviderId::Anthropic);
        assert_eq!(switched.selection.model, "claude-sonnet-4-6");
        // Provider switch recomputes base url to the new provider's default.
        assert_eq!(switched.selection.base_url, "https://api.anthropic.com");
    }

    #[test]
    fn model_command_rejects_unknown_provider_but_allows_unknown_model() {
        let (mut harness, _dir) = fake_harness();
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        let mut switch = Some(ModelSwitch::new(
            selection(ProviderId::OpenAiCodex, "gpt-5.5"),
            "PROMPT".to_string(),
            &build,
            None,
        ));
        // Unknown provider: rejected, selection unchanged.
        let bad =
            handle_model_command("/model bogus/x", &mut harness, &mut switch).expect("handled");
        assert!(
            bad.iter().any(|l| l.contains("unsupported provider")),
            "{bad:?}"
        );
        assert_eq!(switch.as_ref().unwrap().selection.model, "gpt-5.5");
        // Unknown model under a known provider passes through.
        let ok = handle_model_command("/model some-future-model", &mut harness, &mut switch)
            .expect("handled");
        assert!(ok.iter().any(|l| l.contains("some-future-model")), "{ok:?}");
        assert_eq!(
            switch.as_ref().unwrap().selection.model,
            "some-future-model"
        );
    }

    /// A harness carrying a large context (seeded via a resumed agent) with an
    /// optional auto-compaction budget, for the switch-advisory tests.
    fn seeded_harness(
        content_chars: usize,
        budget: Option<u64>,
    ) -> (Harness<FakeProvider>, crate::tools::test_support::TestDir) {
        let dir = crate::tools::test_support::temp_dir();
        let messages = vec![
            Message::user("start"),
            Message::assistant(&"R".repeat(content_chars)),
        ];
        let agent = Agent::resumed(
            FakeProvider::new(vec![]),
            crate::tools::built_in_tools(),
            messages,
        );
        let harness = Harness::new(
            agent,
            dir.path.clone(),
            crate::tools::ToolState::new(),
            None,
            budget,
        );
        (harness, dir)
    }

    #[test]
    fn model_switch_with_large_context_advises_compaction() {
        // ~50k estimated tokens, over the no-budget 32k advisory floor.
        let (mut harness, _dir) = seeded_harness(200_000, None);
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        let mut switch = Some(ModelSwitch::new(
            selection(ProviderId::OpenAiCodex, "gpt-5.5"),
            "PROMPT".to_string(),
            &build,
            None,
        ));
        let lines = handle_model_command(
            "/model anthropic/claude-sonnet-4-6",
            &mut harness,
            &mut switch,
        )
        .expect("handled");
        assert!(
            lines.iter().any(|l| l.starts_with("switched to")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("carrying ~") && l.contains("/compact")),
            "a cross-provider switch over the threshold must advise /compact: {lines:?}"
        );
    }

    #[test]
    fn reasoning_only_switch_stays_silent_about_context() {
        let (mut harness, dir) = seeded_harness(200_000, None);
        let _config = ConfigPathGuard::set(&dir.path.join("settings.json"));
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        let mut switch = Some(ModelSwitch::new(
            selection(ProviderId::OpenAiCodex, "gpt-5.5"),
            "PROMPT".to_string(),
            &build,
            None,
        ));
        let lines =
            handle_model_command("/reasoning high", &mut harness, &mut switch).expect("handled");
        assert!(
            !lines.iter().any(|l| l.contains("/compact")),
            "a reasoning-only change keeps the prefix; no advisory: {lines:?}"
        );
    }

    #[test]
    fn model_switch_with_small_context_stays_silent() {
        // ~5k estimated tokens with the default-ish 128k budget: under the
        // budget/4 threshold, so the advisory would be noise.
        let (mut harness, _dir) = seeded_harness(20_000, Some(128_000));
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        let mut switch = Some(ModelSwitch::new(
            selection(ProviderId::OpenAiCodex, "gpt-5.5"),
            "PROMPT".to_string(),
            &build,
            None,
        ));
        let lines = handle_model_command(
            "/model anthropic/claude-sonnet-4-6",
            &mut harness,
            &mut switch,
        )
        .expect("handled");
        assert!(
            !lines.iter().any(|l| l.contains("/compact")),
            "a small carried context needs no advisory: {lines:?}"
        );
    }

    #[test]
    fn text_path_compact_on_in_memory_session_reports_why() -> Result<()> {
        let (mut harness, _dir) = fake_harness();
        let mut ui = TextUi::new(
            "/compact preserve exact flags\n/quit\n".as_bytes(),
            Vec::new(),
            Vec::new(),
        );
        let mut switch = None;
        run_session(&mut harness, &mut ui, &mut switch)?;
        let (_, out, _) = ui.into_parts();
        let out = String::from_utf8(out)?;
        assert!(
            out.contains("in-memory"),
            "text-path /compact must explain the in-memory no-op: {out}"
        );
        Ok(())
    }

    #[test]
    fn reasoning_command_sets_and_clamps_level() {
        let (mut harness, dir) = fake_harness();
        let _config = ConfigPathGuard::set(&dir.path.join("settings.json"));
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        // Codex supports xhigh natively.
        let mut switch = Some(ModelSwitch::new(
            selection(ProviderId::OpenAiCodex, "gpt-5.5"),
            "PROMPT".to_string(),
            &build,
            None,
        ));
        let lines =
            handle_model_command("/reasoning xhigh", &mut harness, &mut switch).expect("handled");
        assert!(
            lines.iter().any(|l| l.contains("reasoning: xhigh")),
            "{lines:?}"
        );
        assert_eq!(
            switch.as_ref().unwrap().selection.reasoning,
            Some(ReasoningEffort::XHigh)
        );

        // Manual-budget Anthropic models expose exact thinking budgets in the UI
        // but still store the normalized xhigh level internally.
        handle_model_command(
            "/model anthropic/claude-sonnet-4-6",
            &mut harness,
            &mut switch,
        );
        let lines =
            handle_model_command("/reasoning xhigh", &mut harness, &mut switch).expect("handled");
        assert!(
            lines.iter().any(|l| l.contains("reasoning: 32,768 tokens")),
            "{lines:?}"
        );
        assert_eq!(
            switch.as_ref().unwrap().selection.reasoning,
            Some(ReasoningEffort::XHigh)
        );

        // Adaptive Anthropic models expose the provider-native `max` effort.
        handle_model_command(
            "/model anthropic/claude-sonnet-5",
            &mut harness,
            &mut switch,
        );
        let lines =
            handle_model_command("/reasoning max", &mut harness, &mut switch).expect("handled");
        assert!(
            lines.iter().any(|l| l.contains("reasoning: max")),
            "{lines:?}"
        );
        assert_eq!(
            switch.as_ref().unwrap().selection.reasoning,
            Some(ReasoningEffort::XHigh)
        );

        // An older/unknown Anthropic id still tops out at high: xhigh clamps.
        handle_model_command(
            "/model anthropic/claude-3-7-sonnet",
            &mut harness,
            &mut switch,
        );
        let lines =
            handle_model_command("/reasoning xhigh", &mut harness, &mut switch).expect("handled");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("not supported") && l.contains("using '20,480 tokens'")),
            "{lines:?}"
        );
        assert_eq!(
            switch.as_ref().unwrap().selection.reasoning,
            Some(ReasoningEffort::High)
        );
    }

    #[test]
    fn reasoning_command_rejects_unparsable_level() {
        let (mut harness, _dir) = fake_harness();
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        let mut switch = Some(ModelSwitch::new(
            selection(ProviderId::OpenAiCodex, "gpt-5.5"),
            "PROMPT".to_string(),
            &build,
            None,
        ));
        let lines =
            handle_model_command("/reasoning turbo", &mut harness, &mut switch).expect("handled");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("unsupported reasoning level")),
            "{lines:?}"
        );
        // Rejected input leaves the selection untouched.
        assert_eq!(switch.as_ref().unwrap().selection.reasoning, None);
    }

    #[test]
    fn reasoning_command_persists_default_after_successful_switch() {
        // Issue #514: a successful `/reasoning` switch must persist the clamped
        // level as the global default so it survives a restart.
        let (mut harness, dir) = fake_harness();
        let path = dir.path.join("settings.json");
        let _config = ConfigPathGuard::set(&path);
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        let mut switch = Some(ModelSwitch::new(
            selection(ProviderId::OpenAiCodex, "gpt-5.5"),
            "PROMPT".to_string(),
            &build,
            None,
        ));
        let lines =
            handle_model_command("/reasoning high", &mut harness, &mut switch).expect("handled");
        assert!(
            !lines.iter().any(|l| l.contains("not saved")),
            "a successful switch must not report a persistence error: {lines:?}"
        );
        let saved = std::fs::read_to_string(&path).expect("settings file written");
        assert!(
            saved.contains("defaultReasoning") && saved.contains("high"),
            "the clamped level must be persisted as defaultReasoning: {saved}"
        );
    }

    #[test]
    fn reasoning_command_persisted_anthropic_adaptive_level_survives_restart() {
        // Root-cause regression for the #514/#512 interaction: Anthropic
        // adaptive display labels are shifted relative to Iris's stored tokens.
        // `/reasoning high` installs internal `Medium` and writes `"medium"`;
        // startup must read that as the stored token `Medium`, not as the
        // provider-native label `medium` (which would lower it to `Low`).
        let (mut harness, dir) = fake_harness();
        let path = dir.path.join("settings.json");
        let _config = ConfigPathGuard::set(&path);
        let _model_env = EnvVarGuard::unset("IRIS_MODEL");
        config::save_default_model("anthropic", "claude-sonnet-5").unwrap();
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        let mut switch = Some(ModelSwitch::new(
            selection(ProviderId::Anthropic, "claude-sonnet-5"),
            "PROMPT".to_string(),
            &build,
            None,
        ));

        let lines =
            handle_model_command("/reasoning high", &mut harness, &mut switch).expect("handled");
        assert!(
            lines.iter().any(|line| line.contains("reasoning: high")),
            "interactive confirmation must stay provider-native: {lines:?}"
        );
        let saved = std::fs::read_to_string(&path).expect("settings file written");
        assert!(
            saved.contains(r#""defaultReasoning": "medium""#),
            "Iris stores the normalized token for Anthropic adaptive high: {saved}"
        );

        let reloaded = config::Settings::load(&dir.path).unwrap();
        let resolved = ModelSelection::resolve(&reloaded).unwrap();
        assert_eq!(resolved.provider, ProviderId::Anthropic);
        assert_eq!(resolved.model, "claude-sonnet-5");
        assert_eq!(resolved.reasoning, Some(ReasoningEffort::Medium));
        assert_eq!(
            model_capabilities::display_level(
                resolved.provider,
                &resolved.model,
                resolved.reasoning.unwrap()
            ),
            "high",
            "restart must display the same provider-native effort the user selected"
        );
    }

    #[test]
    fn reasoning_command_failed_switch_does_not_persist() {
        // A provider build failure leaves the selection untouched, so nothing
        // is persisted (mirrors the existing invalid-level behavior).
        let (mut harness, dir) = fake_harness();
        let path = dir.path.join("settings.json");
        let _config = ConfigPathGuard::set(&path);
        let build = |_s: &ModelSelection, _p: &str| Err(anyhow!("boom"));
        let mut switch = Some(ModelSwitch::new(
            selection(ProviderId::OpenAiCodex, "gpt-5.5"),
            "PROMPT".to_string(),
            &build,
            None,
        ));
        let lines =
            handle_model_command("/reasoning high", &mut harness, &mut switch).expect("handled");
        assert!(
            lines.iter().any(|l| l.contains("could not switch")),
            "{lines:?}"
        );
        assert!(
            !path.exists(),
            "a failed switch must not persist a default reasoning level"
        );
    }

    #[test]
    fn reasoning_command_failed_switch_does_not_persist_when_level_already_active() {
        // Regression for the success check: if the requested level already is
        // active, a failed provider rebuild still must not write settings.
        let (mut harness, dir) = fake_harness();
        let path = dir.path.join("settings.json");
        let _config = ConfigPathGuard::set(&path);
        let build = |_s: &ModelSelection, _p: &str| Err(anyhow!("boom"));
        let mut active = selection(ProviderId::OpenAiCodex, "gpt-5.5");
        active.reasoning = Some(ReasoningEffort::High);
        let mut switch = Some(ModelSwitch::new(active, "PROMPT".to_string(), &build, None));
        let lines =
            handle_model_command("/reasoning high", &mut harness, &mut switch).expect("handled");
        assert!(
            lines.iter().any(|l| l.contains("could not switch")),
            "{lines:?}"
        );
        assert!(
            !path.exists(),
            "a failed same-level switch must not persist a default reasoning level"
        );
    }

    #[test]
    fn non_command_and_absent_switch_are_not_handled() {
        let (mut harness, _dir) = fake_harness();
        let build = |_s: &ModelSelection, _p: &str| Ok(FakeProvider::new(vec![]));
        let mut switch = Some(ModelSwitch::new(
            selection(ProviderId::OpenAiCodex, "gpt-5.5"),
            "PROMPT".to_string(),
            &build,
            None,
        ));
        // An ordinary message is not a command -> falls through to a normal turn.
        assert!(handle_model_command("hello there", &mut harness, &mut switch).is_none());
        // No switch state -> never handled (the tests' in-memory loops pass None).
        let mut none: Option<ModelSwitch<FakeProvider>> = None;
        assert!(handle_model_command("/model", &mut harness, &mut none).is_none());
    }

    #[test]
    fn text_path_handles_session_and_copy_and_reports_tui_only_commands() -> Result<()> {
        let (mut harness, _dir) = fake_harness();
        // `/quit` exits through the registry, so no turn ever reaches the
        // provider (the fake provider would error on an unexpected call).
        let mut ui = TextUi::new(
            "/session\n/copy\n/debug\n/dbug\n/settings\n/quit\n".as_bytes(),
            Vec::new(),
            Vec::new(),
        );
        let mut switch = None;
        run_session(&mut harness, &mut ui, &mut switch)?;

        let (_, out, _) = ui.into_parts();
        let out = String::from_utf8(out)?;
        assert!(
            out.contains("note: session: in-memory (not persisted)"),
            "{out}"
        );
        assert!(out.contains("note: messages: 0 (0 user"), "{out}");
        assert!(
            out.contains("note: no assistant reply to copy yet."),
            "{out}"
        );
        // Both spellings of the debug command report the same TUI-only notice.
        assert_eq!(
            out.matches("note: /debug is only available in the interactive TUI")
                .count(),
            2,
            "{out}"
        );
        assert!(
            out.contains("note: /settings is only available in the interactive TUI"),
            "{out}"
        );
        Ok(())
    }

    #[test]
    fn provider_error_is_rendered_and_session_continues() -> Result<()> {
        let provider = FakeProvider::new(vec![Err("boom"), Ok(AssistantTurn::text("ok"))]);
        let dir = crate::tools::test_support::temp_dir();
        let agent = Agent::new(provider, crate::tools::built_in_tools());
        let mut harness = Harness::new(
            agent,
            dir.path.clone(),
            crate::tools::ToolState::new(),
            None,
            None,
        );
        let mut ui = TextUi::new("bad\nagain\n/exit\n".as_bytes(), Vec::new(), Vec::new());

        let mut switch = None;
        run_session(&mut harness, &mut ui, &mut switch)?;

        let (_, out, err) = ui.into_parts();
        assert!(String::from_utf8(out)?.contains("assistant> ok"));
        assert!(String::from_utf8(err)?.contains("provider error: boom"));
        Ok(())
    }
}
