use std::env;
use std::io::{IsTerminal, Write};
use std::process::{Command, ExitCode};
use std::time::Duration;

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use nexus::{Agent, ChatProvider};
use reqwest::blocking::Client;
use tokio_util::sync::CancellationToken;

mod approval;
mod cli;
mod config;
mod errors;
mod handles;
mod mimir;
mod nexus;
mod process_group;
mod session;
mod signals;
mod telemetry;
mod tool_display;
mod tools;
mod ui;
mod wayland;

fn main() -> ExitCode {
    telemetry::init();
    signals::install();
    match dispatch() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            if let Some(auth) = error.downcast_ref::<errors::AuthError>() {
                // Prefer the provider that actually failed (carried on the typed
                // error) so the hint never points at the wrong provider; fall
                // back to the configured default when it is unknown.
                let provider = auth
                    .provider()
                    .map(str::to_string)
                    .unwrap_or_else(configured_provider);
                eprintln!(
                    "hint: run `{}` login {provider} to authenticate",
                    command_name(),
                );
            }
            ExitCode::from(errors::exit_code(&error))
        }
    }
}

fn dispatch() -> Result<()> {
    match env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => run_agent(false),
        [flag] if flag == "--plain" => run_agent(true),
        [command, session_id] if command == "resume" => resume_agent(session_id, false),
        [command, session_id, flag] if command == "resume" && flag == "--plain" => {
            resume_agent(session_id, true)
        }
        [command, provider] if command == "login" && provider == "openai-codex" => {
            login_openai_codex(LoginMethod::Browser)
        }
        [command, provider] if command == "login" && provider == "openai" => {
            login_api_key(mimir::selection::ProviderId::OpenAi)
        }
        [command, provider] if command == "login" && provider == "openai-compatible" => {
            login_api_key(mimir::selection::ProviderId::OpenAiCompatible)
        }
        [command, provider] if command == "login" && provider == "antigravity" => {
            login_antigravity()
        }
        [command, provider] if command == "login" && provider == "anthropic" => login_anthropic(),
        [command, provider, flag]
            if command == "login" && provider == "openai-codex" && flag == "--browser" =>
        {
            login_openai_codex(LoginMethod::Browser)
        }
        [command, provider, flag]
            if command == "login" && provider == "openai-codex" && flag == "--device-code" =>
        {
            login_openai_codex(LoginMethod::DeviceCode)
        }
        [command, provider, flag]
            if command == "login" && provider == "anthropic" && flag == "--api-key" =>
        {
            login_api_key(mimir::selection::ProviderId::Anthropic)
        }
        [command, provider, flag]
            if command == "login" && provider == "openai" && flag == "--api-key" =>
        {
            login_api_key(mimir::selection::ProviderId::OpenAi)
        }
        [command, provider, flag]
            if command == "login" && provider == "openai-compatible" && flag == "--api-key" =>
        {
            login_api_key(mimir::selection::ProviderId::OpenAiCompatible)
        }
        [command] if command == "update" => update_agent(),
        [command] if command == "help" || command == "--help" || command == "-h" => {
            print_help();
            Ok(())
        }
        _ => {
            print_help();
            Err(errors::UsageError::new("unknown command").into())
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum LoginMethod {
    Browser,
    DeviceCode,
}

/// Provider used when the settings file selects none. Stays `openai-codex` for
/// backward compatibility; `anthropic` and `antigravity` are opt-in via
/// `defaultProvider` in settings.
const DEFAULT_PROVIDER: &str = "openai-codex";

/// Best-effort provider id for the auth re-login hint, so an Anthropic or
/// Antigravity auth failure does not tell the user to log into OpenAI. Reads
/// `defaultProvider` from settings; falls back to the default when settings
/// cannot be read (the hint is advisory, never fatal).
fn configured_provider() -> String {
    env::current_dir()
        .ok()
        .and_then(|cwd| config::Settings::load(&cwd).ok())
        .and_then(|settings| settings.default_provider)
        .map(|provider| provider.trim().to_string())
        .filter(|provider| !provider.is_empty())
        .unwrap_or_else(|| DEFAULT_PROVIDER.to_string())
}

fn run_agent(force_plain: bool) -> Result<()> {
    let cwd = env::current_dir()?;
    let settings = config::Settings::load(&cwd)?;
    // Materialize the shipped fragment defaults into ~/.iris/fragments (if
    // absent) so users can edit/reorder them on disk; best-effort.
    wayland::system_prompt::ensure_default_fragments();
    // Harness-owned assembly: the fragment/slot baukasten composes the prompt
    // from fragment files plus dynamic context (project docs, date, cwd) and the
    // live tool registry. Fresh and resume call the same function.
    let tools = tools::built_in_tools();
    let system_prompt = wayland::system_prompt::assemble(&cwd, &tools);
    // One resolution point owns provider/model/reasoning precedence; capability
    // validation then rejects a configured reasoning level the model cannot do.
    let selection = mimir::selection::ModelSelection::resolve(&settings)?;
    mimir::model_capabilities::validate(&selection)?;
    let session_id = session::new_session_id();
    let provider = build_provider(&selection, &system_prompt, &session_id)?;
    let agent =
        Agent::new(provider, tools).with_max_tool_roundtrips(settings.max_tool_roundtrips());
    // Transcript persistence is best-effort: if the log cannot be opened (e.g.
    // no writable session dir), warn and continue in-memory rather than fail.
    let session = match session::SessionLog::create_with_id(&cwd, &session_id) {
        Ok(log) => {
            tracing::info!(id = %log.id(), path = %log.path().display(), "session transcript");
            Some(log)
        }
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "session persistence disabled");
            None
        }
    };
    // Resume foundation: surface prior persisted sessions for this workspace.
    // The /resume UI is a later milestone; this only proves the store reads
    // back and signals that persistence is durable and resumable.
    log_resumable_sessions(&cwd);
    // The Tier-2 harness owns the execution surface (workspace + tool state),
    // persistence, and the auto-compaction policy, wrapping the bare in-memory
    // agent. When the context token total exceeds the budget at a turn
    // boundary, the harness compacts before the provider request.
    let budget = Some(settings.context_token_budget());
    let mut harness =
        wayland::Harness::new(agent, cwd.clone(), tools::ToolState::new(), session, budget);
    // Tier-3 mode-switch state: `/model` `/reasoning` rebuild a provider from the
    // same system prompt via `build_provider` and install it at a turn boundary.
    let build_session_id = session_id.clone();
    let build = move |selection: &mimir::selection::ModelSelection, prompt: &str| {
        build_provider(selection, prompt, &build_session_id)
    };
    let mut switch = Some(cli::ModelSwitch::new(
        selection,
        system_prompt,
        &build,
        settings.enabled_models.clone(),
    ));
    cli::run_interactive(&mut harness, &mut switch, force_plain)
}

/// Resume an existing session by id: load its transcript from the store,
/// reconstruct the provider-visible messages, seed the agent with them, and
/// continue appending future turns to the same log. Errors clearly when the id
/// is unknown or the session cannot be read.
fn resume_agent(session_id: &str, force_plain: bool) -> Result<()> {
    let cwd = env::current_dir()?;
    let store = session::SessionStore::open_default()?;
    let meta = store.find(session_id)?.ok_or_else(|| {
        errors::UsageError::new(format!(
            "no session found with id '{session_id}'; run with no arguments to start a new session"
        ))
    })?;
    let stored = store.open(&meta)?;
    let resumed = stored.messages.len();
    // The rebuilt context's token total from the reconstruction path -- the same
    // number the live session reports via `session::context_tokens`, so it is
    // stable across resume. The harness compares it against the budget at the
    // next turn boundary.
    let context_tokens = stored.context_tokens;

    let settings = config::Settings::load(&cwd)?;
    let budget = Some(settings.context_token_budget());
    // Resume assembles instructions through the same harness-owned baukasten as
    // a fresh session, so a resumed turn gets identical fragment/context output.
    wayland::system_prompt::ensure_default_fragments();
    let tools = tools::built_in_tools();
    let system_prompt = wayland::system_prompt::assemble(&cwd, &tools);
    let selection = mimir::selection::ModelSelection::resolve(&settings)?;
    mimir::model_capabilities::validate(&selection)?;
    let session_id = meta.id.clone();
    let provider = build_provider(&selection, &system_prompt, &session_id)?;
    let agent = Agent::resumed(provider, tools, stored.messages)
        .with_max_tool_roundtrips(settings.max_tool_roundtrips());

    // Reopen the same transcript for append so continued turns extend it rather
    // than starting a new file. Best-effort, like new-session persistence: if
    // the reopen fails, warn and continue in-memory.
    let session = match session::SessionLog::resume(&meta.path) {
        Ok(log) => Some(log),
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "resume persistence disabled");
            None
        }
    };
    tracing::info!(id = %meta.id, messages = resumed, context_tokens, "resumed session");

    let mut harness = wayland::Harness::resumed(
        agent,
        cwd.clone(),
        tools::ToolState::new(),
        session,
        resumed,
        budget,
    );
    let build_session_id = session_id.clone();
    let build = move |selection: &mimir::selection::ModelSelection, prompt: &str| {
        build_provider(selection, prompt, &build_session_id)
    };
    let mut switch = Some(cli::ModelSwitch::new(
        selection,
        system_prompt,
        &build,
        settings.enabled_models.clone(),
    ));
    cli::run_interactive(&mut harness, &mut switch, force_plain)
}

/// Log the most recent prior session for `cwd` (if any) via the read side of
/// the session store. Best-effort and invisible by default: a read failure is
/// debug-logged, never fatal. This is the seam the future `/resume` command
/// will build on.
fn log_resumable_sessions(cwd: &Path) {
    let store = match session::SessionStore::open_default() {
        Ok(store) => store,
        Err(error) => {
            tracing::debug!(error = %format!("{error:#}"), "session store unavailable");
            return;
        }
    };
    let metas = match store.list() {
        Ok(metas) => metas,
        Err(error) => {
            tracing::debug!(error = %format!("{error:#}"), "could not list prior sessions");
            return;
        }
    };
    let cwd_str = cwd.to_string_lossy();
    // list() is newest-first, so the first match is the latest session here.
    let mut here = metas.into_iter().filter(|meta| meta.cwd == cwd_str);
    let Some(latest) = here.next() else {
        return;
    };
    let also = here.count();
    match store.open(&latest) {
        Ok(prior) => tracing::info!(
            id = %prior.meta.id,
            created_ms = prior.meta.created_ms,
            updated_ms = prior.meta.updated_ms,
            messages = prior.messages.len(),
            also_resumable = also,
            "prior session available for this workspace"
        ),
        Err(error) => {
            tracing::debug!(error = %format!("{error:#}"), "could not read latest prior session")
        }
    }
}

/// Build the selected provider as a boxed trait object so a single
/// `Agent<Box<dyn ChatProvider>>` can back any provider chosen at runtime.
/// Precedence and the unsupported-provider error now live in `mimir::selection`
/// (`ModelSelection::resolve`), so this only maps the resolved [`ProviderId`] to
/// its concrete adapter. Reused at startup and on every `/model` `/reasoning`
/// switch (rebuilds with the new selection + the same system prompt).
fn build_provider(
    selection: &mimir::selection::ModelSelection,
    system_prompt: &str,
    session_id: &str,
) -> Result<Box<dyn ChatProvider>> {
    use mimir::selection::ProviderId;
    let model = selection.model.as_str();
    let base_url = selection.base_url.as_str();
    let reasoning = selection.reasoning;
    let provider: Box<dyn ChatProvider> = match selection.provider {
        ProviderId::OpenAiCodex => Box::new(
            mimir::providers::openai_codex_responses::OpenAiCodexResponsesProvider::new(
                model,
                base_url,
                reasoning,
                system_prompt,
                session_id,
                selection.cache_retention,
                selection.retry_policy,
            )?,
        ),
        ProviderId::OpenAi => {
            let auth = mimir::auth::storage::AuthStore::from_env()?;
            let api_key = mimir::auth::api_key::api_key_for_provider(ProviderId::OpenAi, &auth)?;
            Box::new(
                mimir::providers::openai_compatible_chat::OpenAiCompatibleChatProvider::new(
                    mimir::providers::openai_compatible_chat::OpenAiCompatibleChatConfig {
                        provider: ProviderId::OpenAi,
                        model,
                        base_url,
                        reasoning,
                        system_prompt,
                        api_key,
                        supports_reasoning: true,
                        api_key_required: true,
                        retry_policy: selection.retry_policy,
                    },
                )?,
            )
        }
        ProviderId::Anthropic => Box::new(
            mimir::providers::anthropic_messages::AnthropicProvider::new(
                model,
                base_url,
                reasoning,
                system_prompt,
                selection.cache_retention,
                selection.context_management.clone(),
                selection.retry_policy,
            )?,
        ),
        ProviderId::Antigravity => {
            Box::new(mimir::providers::antigravity::AntigravityProvider::new(
                model,
                base_url,
                reasoning,
                system_prompt,
            )?)
        }
        ProviderId::OpenAiCompatible => {
            let auth = mimir::auth::storage::AuthStore::from_env()?;
            let api_key =
                mimir::auth::api_key::api_key_for_provider(ProviderId::OpenAiCompatible, &auth)?;
            Box::new(
                mimir::providers::openai_compatible_chat::OpenAiCompatibleChatProvider::new(
                    mimir::providers::openai_compatible_chat::OpenAiCompatibleChatConfig {
                        provider: ProviderId::OpenAiCompatible,
                        model,
                        base_url,
                        reasoning,
                        system_prompt,
                        api_key,
                        supports_reasoning: selection.open_ai_compatible.reasoning,
                        api_key_required: selection.open_ai_compatible.api_key_required,
                        retry_policy: selection.retry_policy,
                    },
                )?,
            )
        }
    };
    Ok(provider)
}

const COMMAND_NAME: &str = "iris";
const UPDATE_REPO: &str = "https://github.com/5omeOtherGuy/iris-agent.git";
const UPDATE_ARGS: &[&str] = &["install", "--git", UPDATE_REPO, "--locked", "--force"];

fn command_name() -> &'static str {
    COMMAND_NAME
}

fn update_args() -> &'static [&'static str] {
    UPDATE_ARGS
}

fn update_agent() -> Result<()> {
    println!("Updating Iris from {UPDATE_REPO} ...");
    let status = Command::new("cargo")
        .args(update_args())
        .status()
        .context("failed to run cargo; install Rust/Cargo or update with cargo install manually")?;
    if !status.success() {
        bail!("cargo install failed with {status}");
    }
    Ok(())
}

fn login_openai_codex(method: LoginMethod) -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let cancel = CancellationToken::new();

    match method {
        LoginMethod::Browser => {
            mimir::auth::openai_codex::login_browser(&client, &cancel, |auth| {
                println!("OpenAI Codex browser login");
                println!("Open: {}", auth.url);
                crate::ui::login::open_in_browser(&auth.url);
                println!("Waiting for callback at {} ...", auth.redirect_uri);
            })?
        }
        LoginMethod::DeviceCode => mimir::auth::openai_codex::login_device_code(&client, |code| {
            println!("OpenAI Codex device-code login");
            println!("Open: {}", code.verification_uri);
            crate::ui::login::open_in_browser(&code.verification_uri);
            println!("Code: {}", code.user_code);
            println!("Waiting for authorization...");
        })?,
    }

    println!("Logged in to openai-codex.");
    Ok(())
}

fn login_antigravity() -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let cancel = CancellationToken::new();
    mimir::auth::antigravity::login_browser(&client, &cancel, |url| {
        println!("Antigravity (Google account) login");
        println!("Open: {url}");
        crate::ui::login::open_in_browser(url);
        println!("Waiting for callback...");
    })?;
    println!("Logged in to antigravity.");
    Ok(())
}

fn login_api_key(provider: mimir::selection::ProviderId) -> Result<()> {
    let key = read_api_key(provider)?;
    if key.is_empty() {
        bail!("API key is blank");
    }
    let auth = mimir::auth::storage::AuthStore::from_env()?;
    auth.set_api_key_credentials(provider.as_str(), &key)?;
    save_default_after_api_key_login(provider);
    println!("Stored API key for {}.", provider.display_name());
    Ok(())
}

fn read_api_key(provider: mimir::selection::ProviderId) -> Result<String> {
    print!("Enter API key for {}: ", provider.display_name());
    std::io::stdout().flush()?;
    if !std::io::stdin().is_terminal() {
        let mut key = String::new();
        std::io::stdin().read_line(&mut key)?;
        return Ok(key.trim().to_string());
    }

    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = ratatui::crossterm::terminal::disable_raw_mode();
        }
    }

    ratatui::crossterm::terminal::enable_raw_mode()?;
    let guard = RawModeGuard;
    let mut key = String::new();
    let result: Result<()> = loop {
        match ratatui::crossterm::event::read()? {
            ratatui::crossterm::event::Event::Key(event)
                if event.kind == ratatui::crossterm::event::KeyEventKind::Press =>
            {
                match event.code {
                    ratatui::crossterm::event::KeyCode::Enter => break Ok(()),
                    ratatui::crossterm::event::KeyCode::Backspace => {
                        key.pop();
                    }
                    ratatui::crossterm::event::KeyCode::Char('c')
                        if event
                            .modifiers
                            .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        break Err(anyhow!("API key entry cancelled"));
                    }
                    ratatui::crossterm::event::KeyCode::Char(ch) => key.push(ch),
                    _ => {}
                }
            }
            ratatui::crossterm::event::Event::Paste(text) => key.push_str(&text),
            _ => {}
        }
    };
    drop(guard);
    println!();
    result?;
    Ok(key.trim().to_string())
}

fn save_default_after_api_key_login(provider: mimir::selection::ProviderId) {
    if !matches!(
        provider,
        mimir::selection::ProviderId::OpenAi | mimir::selection::ProviderId::Anthropic
    ) {
        return;
    }
    let already_default = env::current_dir()
        .ok()
        .and_then(|cwd| config::Settings::load(&cwd).ok())
        .and_then(|settings| settings.default_provider)
        .is_some_and(|default| default.trim() == provider.as_str());
    if !already_default {
        let _ = config::save_default_model(provider.as_str(), provider.default_model());
    }
}

fn login_anthropic() -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let cancel = CancellationToken::new();
    // A background stdin reader feeds a pasted authorization code / full redirect
    // URL to the callback wait, so login works even when the browser is on
    // another machine or the loopback callback cannot be received.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() && !line.trim().is_empty() {
            let _ = tx.send(line);
        }
    });
    mimir::auth::anthropic::login_browser(&client, &cancel, Some(&rx), |url| {
        println!("Anthropic browser login");
        println!("Open: {url}");
        crate::ui::login::open_in_browser(url);
        println!(
            "Waiting for the browser callback... or paste the authorization code / full redirect URL and press Enter."
        );
    })?;
    println!("Logged in to anthropic.");
    Ok(())
}

fn print_help() {
    eprintln!("Usage:");
    eprintln!("  iris                              Start interactive agent");
    eprintln!("  iris --plain                      Start in the plain, ANSI-free text UI");
    eprintln!("  iris resume <session-id>          Resume a prior session by id");
    eprintln!("  iris login openai-codex           Login with browser OAuth (default)");
    eprintln!("  iris login openai-codex --browser Login with browser OAuth");
    eprintln!("  iris login openai-codex --device-code Login with device-code OAuth");
    eprintln!("  iris login openai                 Store an OpenAI API key");
    eprintln!("  iris login openai-compatible      Store an OpenAI-compatible API key");
    eprintln!("  iris login antigravity            Login with Google account OAuth");
    eprintln!("  iris login anthropic              Login with Anthropic OAuth (browser)");
    eprintln!("  iris login anthropic --api-key    Store an Anthropic API key");
    eprintln!("  iris update                       Update Iris from GitHub");
    eprintln!();
    eprintln!("Accessibility (environment):");
    eprintln!(
        "  IRIS_PLAIN=1                      Use the plain, ANSI-free text UI (like --plain)"
    );
    eprintln!("  NO_COLOR                          Disable color; routes to the plain text UI");
    eprintln!("  IRIS_REDUCED_MOTION=1             Freeze the working-indicator animation");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_name_is_iris() {
        assert_eq!(command_name(), "iris");
    }

    #[test]
    fn update_command_installs_locked_remote_with_force() {
        assert_eq!(
            UPDATE_REPO,
            "https://github.com/5omeOtherGuy/iris-agent.git"
        );
        assert_eq!(
            update_args(),
            &["install", "--git", UPDATE_REPO, "--locked", "--force"]
        );
    }
}
