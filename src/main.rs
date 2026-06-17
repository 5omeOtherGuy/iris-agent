use std::env;
use std::process::ExitCode;
use std::time::Duration;

use std::path::Path;

use anyhow::Result;
use nexus::{Agent, ChatProvider};
use reqwest::blocking::Client;

mod approval;
mod cli;
mod config;
mod errors;
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
            if error.downcast_ref::<errors::AuthError>().is_some() {
                eprintln!("hint: run `iris-agent login openai-codex` to authenticate");
            }
            ExitCode::from(errors::exit_code(&error))
        }
    }
}

fn dispatch() -> Result<()> {
    match env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => run_agent(),
        [command, session_id] if command == "resume" => resume_agent(session_id),
        [command, provider] if command == "login" && provider == "openai-codex" => {
            login_openai_codex(LoginMethod::Browser)
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

fn run_agent() -> Result<()> {
    let cwd = env::current_dir()?;
    let settings = config::Settings::load(&cwd)?;
    let provider = build_provider(resolve_provider_id(&settings), &settings, &cwd)?;
    let agent = Agent::new(provider, tools::built_in_tools());
    // Transcript persistence is best-effort: if the log cannot be opened (e.g.
    // no writable session dir), warn and continue in-memory rather than fail.
    let session = match session::SessionLog::create(&cwd) {
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
    // The Tier-2 harness owns the execution surface (workspace + tool state) and
    // persistence, wrapping the bare in-memory agent.
    let mut harness = wayland::Harness::new(agent, cwd.clone(), tools::ToolState::new(), session);
    let mut ui = ui::tui::stdio();
    cli::run_session(&mut harness, ui.as_mut())
}

/// Resume an existing session by id: load its transcript from the store,
/// reconstruct the provider-visible messages, seed the agent with them, and
/// continue appending future turns to the same log. Errors clearly when the id
/// is unknown or the session cannot be read.
fn resume_agent(session_id: &str) -> Result<()> {
    let cwd = env::current_dir()?;
    let store = session::SessionStore::open_default()?;
    let meta = store.find(session_id)?.ok_or_else(|| {
        errors::UsageError::new(format!(
            "no session found with id '{session_id}'; run with no arguments to start a new session"
        ))
    })?;
    let stored = store.open(&meta)?;
    let resumed = stored.messages.len();

    let settings = config::Settings::load(&cwd)?;
    let provider = build_provider(resolve_provider_id(&settings), &settings, &cwd)?;
    let agent = Agent::resumed(provider, tools::built_in_tools(), stored.messages);

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
    tracing::info!(id = %meta.id, messages = resumed, "resumed session");

    let mut harness = wayland::Harness::resumed(
        agent,
        cwd.clone(),
        tools::ToolState::new(),
        session,
        resumed,
    );
    let mut ui = ui::tui::stdio();
    cli::run_session(&mut harness, ui.as_mut())
}

/// Resolve the configured provider id from settings, falling back to the
/// backward-compatible default when none is set.
fn resolve_provider_id(settings: &config::Settings) -> &str {
    settings
        .default_provider
        .as_deref()
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
        .unwrap_or(DEFAULT_PROVIDER)
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

/// Build the configured provider as a boxed trait object so a single
/// `Agent<Box<dyn ChatProvider>>` can back any provider chosen at runtime. Each
/// provider resolves its own default model/base URL from the passed-through
/// settings (env still wins inside the provider).
fn build_provider(
    provider_id: &str,
    settings: &config::Settings,
    cwd: &Path,
) -> Result<Box<dyn ChatProvider>> {
    let model = settings.default_model.as_deref();
    let base_url = settings.base_url.as_deref();
    let provider: Box<dyn ChatProvider> = match provider_id {
        "openai-codex" => Box::new(
            mimir::providers::openai_codex_responses::OpenAiCodexResponsesProvider::new(
                model, base_url, cwd,
            )?,
        ),
        "anthropic" => Box::new(
            mimir::providers::anthropic_messages::AnthropicProvider::new(model, base_url, cwd)?,
        ),
        "antigravity" => Box::new(mimir::providers::antigravity::AntigravityProvider::new(
            model, base_url, cwd,
        )?),
        other => {
            return Err(errors::UsageError::new(format!(
                "unsupported provider '{other}' in settings; supported: openai-codex, anthropic, antigravity"
            ))
            .into());
        }
    };
    Ok(provider)
}

fn login_openai_codex(method: LoginMethod) -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;

    match method {
        LoginMethod::Browser => mimir::auth::openai_codex::login_browser(&client, |auth| {
            println!("OpenAI Codex browser login");
            println!("Open: {}", auth.url);
            println!("Waiting for callback at {} ...", auth.redirect_uri);
        })?,
        LoginMethod::DeviceCode => mimir::auth::openai_codex::login_device_code(&client, |code| {
            println!("OpenAI Codex device-code login");
            println!("Open: {}", code.verification_uri);
            println!("Code: {}", code.user_code);
            println!("Waiting for authorization...");
        })?,
    }

    println!("Logged in to openai-codex.");
    Ok(())
}

fn login_antigravity() -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    mimir::auth::antigravity::login_browser(&client, |url| {
        println!("Antigravity (Google account) login");
        println!("Open: {url}");
        println!("Waiting for callback...");
    })?;
    println!("Logged in to antigravity.");
    Ok(())
}

fn login_anthropic() -> Result<()> {
    // ponytail: no dedicated Anthropic login. The Claude Code subscription lane
    // reuses an existing Claude Code OAuth login; Iris reads that credential.
    // Add a manual-code-paste OAuth flow here if standalone login is needed.
    println!("Anthropic uses your existing Claude Code login.");
    println!("Sign in once with the Claude Code CLI; Iris reads its OAuth token.");
    Ok(())
}

fn print_help() {
    eprintln!("Usage:");
    eprintln!("  iris-agent                              Start interactive agent");
    eprintln!("  iris-agent resume <session-id>          Resume a prior session by id");
    eprintln!("  iris-agent login openai-codex           Login with browser OAuth (default)");
    eprintln!("  iris-agent login openai-codex --browser Login with browser OAuth");
    eprintln!("  iris-agent login openai-codex --device-code Login with device-code OAuth");
    eprintln!("  iris-agent login antigravity            Login with Google account OAuth");
    eprintln!("  iris-agent login anthropic              Show Claude Code login instructions");
}
