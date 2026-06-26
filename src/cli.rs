use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;

use crate::mimir::model_capabilities;
use crate::mimir::selection::{self, ModelSelection, ProviderId, ReasoningEffort};
use crate::nexus::ChatProvider;
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
        selection::base_url_for(provider, None)
    };
    let reasoning = current
        .reasoning
        .map(|level| model_capabilities::clamp(provider, model, level));
    ModelSelection {
        provider,
        model: model.to_string(),
        base_url,
        reasoning,
        cache_retention: current.cache_retention,
        context_management: current.context_management.clone(),
        // A runtime model switch keeps the configured retry policy.
        retry_policy: current.retry_policy,
    }
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

/// Picker-only slash commands that require the interactive TUI. In the non-TTY
/// text path these are reported as unavailable instead of being sent to the
/// model; `/model` and `/reasoning` keep working as text commands.
fn picker_only_command(prompt: &str) -> Option<&'static str> {
    match prompt.split_whitespace().next().unwrap_or("") {
        "/scoped-models" => Some("/scoped-models"),
        "/settings" => Some("/settings"),
        "/login" => Some("/login"),
        "/logout" => Some("/logout"),
        _ => None,
    }
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
) -> Result<()> {
    if !use_plain_renderer(force_plain)
        && std::io::stdout().is_terminal()
        && std::io::stdin().is_terminal()
    {
        match TuiUi::new() {
            Ok(tui) => return run_tui(harness, tui, switch),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "TUI unavailable; using text UI");
            }
        }
    }
    let mut ui = crate::ui::text::TextUi::stdio();
    run_session(harness, &mut ui, switch)
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
) -> Result<()> {
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let result = crate::ui::tui_loop::run(harness, &runtime, tui, switch);
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
        // Picker-only commands need the interactive TUI; in the non-TTY text path
        // they are a no-op status rather than being sent to the model. `/model`
        // and `/reasoning` keep their existing text behavior below.
        if let Some(name) = picker_only_command(prompt) {
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
        }
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
