use std::cell::RefCell;
use std::io::IsTerminal;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;

use crate::config;
use crate::mimir::model_capabilities;
use crate::mimir::selection::{self, ModelSelection, ProviderId, ReasoningEffort};
use crate::nexus::{AgentObserver, ApprovalGate, ChatProvider, Message, Role};
use crate::session::SessionLog;
use crate::ui::tui::TuiUi;
use crate::ui::{Ui, UiBridge, UiEvent, slash};
use crate::wayland::Harness;

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
            scoped,
        }
    }

    /// The active resolved selection (provider/model/base-url/reasoning).
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
    committed: bool,
}

impl SessionIdGuard {
    pub(crate) fn swap(cell: Rc<RefCell<String>>, next: String) -> Self {
        let previous = cell.replace(next);
        Self {
            cell,
            previous,
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
    pub(crate) resumed: usize,
}

/// Builds a [`LoadedSource`] for a requested [`SessionSource`]. The app
/// (`main.rs`) owns session-store access and the shared session-id cell the
/// provider builder reads, so it can generate/select the id, open or create the
/// log, and load messages; the loop only asks for the swap.
pub(crate) type SessionLoader<'a> = dyn Fn(&SessionSource) -> Result<LoadedSource> + 'a;

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
        "/accept" => Some(match harness.accept_checkpoint() {
            Some(summary) => vec![summary],
            None => vec!["no unsettled Iris changes to accept".to_string()],
        }),
        "/checkpoint" => Some(match harness.save_checkpoint() {
            Some(summary) => vec![summary],
            None => vec!["no Iris changes to checkpoint".to_string()],
        }),
        "/rollback" => Some(handle_rollback(rest, harness)),
        _ => None,
    }
}

/// `/rollback` (no args) lists the restore points; `/rollback <n>` restores that
/// point. Only Iris-authored work and the user's index are affected.
fn handle_rollback<P: ChatProvider>(rest: &str, harness: &mut Harness<P>) -> Vec<String> {
    let points = harness.checkpoint_restore_points();
    if points.is_empty() {
        return vec!["no unsettled Iris changes to roll back".to_string()];
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
    if rest.is_empty() {
        return vec!["usage: /reasoning <off|minimal|low|medium|high|xhigh>".to_string()];
    }
    let level = match ReasoningEffort::parse(rest) {
        Ok(level) => level,
        Err(error) => return vec![format!("{error:#}")],
    };
    let provider = switch.selection.provider;
    let model = switch.selection.model.clone();
    let clamped = model_capabilities::clamp(provider, &model, level);
    let mut lines = Vec::new();
    if clamped != level {
        lines.push(format!(
            "reasoning '{}' is not supported by {}/{}; using '{}'",
            level.as_str(),
            provider.as_str(),
            model,
            clamped.as_str(),
        ));
    }
    let mut candidate = switch.selection.clone();
    candidate.reasoning = Some(clamped);
    lines.extend(apply_selection(candidate, harness, switch));
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
    let reasoning =
        if provider == ProviderId::OpenAiCompatible && !current.open_ai_compatible.reasoning {
            None
        } else {
            current
                .reasoning
                .map(|level| model_capabilities::clamp(provider, model, level))
        };
    ModelSelection {
        provider,
        model: model.to_string(),
        base_url,
        reasoning,
        cache_retention: current.cache_retention,
        context_management: current.context_management.clone(),
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

/// Validate, rebuild the provider, install it at the safe boundary, and record
/// the audit event. Any failure (unsupported reasoning, build/auth error) leaves
/// the active selection and provider untouched.
pub(crate) fn apply_selection<P: ChatProvider>(
    candidate: ModelSelection,
    harness: &mut Harness<P>,
    switch: &mut ModelSwitch<'_, P>,
) -> Vec<String> {
    if let Err(error) = model_capabilities::validate(&candidate) {
        return vec![format!("{error:#}")];
    }
    let provider = match (switch.build)(&candidate, &switch.system_prompt) {
        Ok(provider) => provider,
        Err(error) => return vec![format!("could not switch: {error:#}")],
    };
    harness.replace_provider(provider);
    let reasoning = candidate.reasoning.map(ReasoningEffort::as_str);
    if let Err(error) =
        harness.record_selection_event(candidate.provider.as_str(), &candidate.model, reasoning)
    {
        tracing::warn!(error = %format!("{error:#}"), "failed to record model selection event");
    }
    let confirm = format!(
        "switched to {}/{} (reasoning: {})",
        candidate.provider.as_str(),
        candidate.model,
        candidate
            .reasoning
            .map(ReasoningEffort::as_str)
            .unwrap_or("none"),
    );
    switch.selection = candidate;
    vec![confirm]
}

/// Slash commands that require the interactive TUI (pickers/modals, or --
/// `/debug` -- the rendered screen itself). In the non-TTY text path these are
/// reported as unavailable instead of being sent to the model; `/model`,
/// `/reasoning`, `/copy`, and `/session` keep working as text commands.
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

/// `/copy`: put the last assistant reply on the system clipboard and report
/// what happened. Shared by the TUI and text front-ends.
pub(crate) fn copy_command_lines<P: ChatProvider>(harness: &Harness<P>) -> Vec<String> {
    let Some(text) = last_assistant_text(harness.messages()) else {
        return vec!["no assistant reply to copy yet.".to_string()];
    };
    match crate::ui::clipboard::copy(text) {
        Ok(crate::ui::clipboard::CopyMethod::NativeTool) => {
            vec!["copied last assistant reply to the clipboard.".to_string()]
        }
        Ok(crate::ui::clipboard::CopyMethod::Osc52) => vec![
            "sent last assistant reply to the clipboard via OSC 52 (requires terminal support)."
                .to_string(),
        ],
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
        lines.push(format!(
            "model: {}/{} (reasoning: {})",
            selection.provider.as_str(),
            selection.model,
            selection
                .reasoning
                .map(ReasoningEffort::as_str)
                .unwrap_or("none"),
        ));
    }
    lines
}

/// Read-only `/model` view: current provider/model/reasoning + supported levels.
fn current_selection_lines(selection: &ModelSelection) -> Vec<String> {
    let levels = model_capabilities::join_levels(model_capabilities::supported_levels(
        selection.provider,
        &selection.model,
    ));
    vec![
        format!(
            "{}/{} (reasoning: {})",
            selection.provider.as_str(),
            selection.model,
            selection
                .reasoning
                .map(ReasoningEffort::as_str)
                .unwrap_or("none"),
        ),
        format!("supported reasoning levels: {levels}"),
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
    swap: &SessionLoader<'_>,
    startup_modal: Option<crate::ui::modal::Modal>,
) -> Result<()> {
    if !prefers_text_ui(force_plain) {
        match TuiUi::new() {
            Ok(tui) => return run_tui(harness, tui, switch, swap, startup_modal),
            Err(error) => {
                if startup_modal.is_some() {
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
    run_session(harness, &mut ui, switch)
}

/// Whether the interactive entry point will fall back to the plain, ANSI-free
/// text UI rather than the terminal-surface TUI: the plain renderer was
/// requested (`--plain`, `IRIS_PLAIN`, `NO_COLOR`) or either stdio end is not a
/// terminal. `main.rs` consults this so `iris resume` (no id) prints a plain
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
    startup_modal: Option<crate::ui::modal::Modal>,
) -> Result<()> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let result = crate::ui::tui_loop::run(harness, &runtime, tui, switch, swap, startup_modal);
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
    done.store(true, Ordering::Relaxed);
    let _ = watcher.join();
    // Bound shutdown so an orphaned blocking provider request cannot hang exit.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
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
        if prompt == "/copy" {
            for line in copy_command_lines(harness) {
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
    use std::cell::RefCell;

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
            context_management: selection::ContextManagement::default(),
            retry_policy: crate::mimir::retry::RetryPolicy::default(),
            open_ai_compatible: selection::OpenAiCompatibleConfig::default(),
        }
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
            copy_command_lines(&harness),
            vec!["no assistant reply to copy yet.".to_string()]
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
    fn checkpoint_command_routes_and_ignores_others() {
        let (mut harness, _dir) = fake_harness();
        // Non-checkpoint lines fall through so the caller sends them as prompts.
        assert!(handle_checkpoint_command("hello there", &mut harness).is_none());
        assert!(handle_checkpoint_command("/model", &mut harness).is_none());
        // With no active task, each command reports the empty state (never panics
        // or sends the line to the model).
        assert_eq!(
            handle_checkpoint_command("/accept", &mut harness).unwrap(),
            vec!["no unsettled Iris changes to accept".to_string()]
        );
        assert_eq!(
            handle_checkpoint_command("/rollback", &mut harness).unwrap(),
            vec!["no unsettled Iris changes to roll back".to_string()]
        );
        assert_eq!(
            handle_checkpoint_command("/checkpoint", &mut harness).unwrap(),
            vec!["no Iris changes to checkpoint".to_string()]
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

    #[test]
    fn reasoning_command_sets_and_clamps_level() {
        let (mut harness, _dir) = fake_harness();
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

        // Shipped adaptive Anthropic models now accept xhigh (it maps to the
        // "max" effort): switching to Sonnet 4.6 and asking for xhigh sticks.
        handle_model_command(
            "/model anthropic/claude-sonnet-4-6",
            &mut harness,
            &mut switch,
        );
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
                .any(|l| l.contains("not supported") && l.contains("using 'high'")),
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
