use std::env;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use nexus::Agent;
use reqwest::blocking::Client;

mod approval;
mod auth;
mod cli;
mod config;
mod errors;
mod nexus;
mod process_group;
mod providers;
mod session;
mod signals;
mod telemetry;
mod tool_display;
mod tools;
mod ui;

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
        [command, provider] if command == "login" && provider == "openai-codex" => {
            login_openai_codex(LoginMethod::Browser)
        }
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

const SUPPORTED_PROVIDER: &str = "openai-codex";

fn run_agent() -> Result<()> {
    let cwd = env::current_dir()?;
    let settings = config::Settings::load(&cwd)?;
    if let Some(provider) = settings
        .default_provider
        .as_deref()
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
        && provider != SUPPORTED_PROVIDER
    {
        return Err(errors::UsageError::new(format!(
            "unsupported provider '{provider}' in settings; only '{SUPPORTED_PROVIDER}' is supported"
        ))
        .into());
    }
    let provider = providers::openai_codex_responses::OpenAiCodexResponsesProvider::new(
        settings.default_model.as_deref(),
        settings.base_url.as_deref(),
        &cwd,
    )?;
    let mut agent = Agent::new(provider, cwd.clone(), tools::built_in_tools());
    // Transcript persistence is best-effort: if the log cannot be opened (e.g.
    // no writable session dir), warn and continue in-memory rather than fail.
    match session::SessionLog::create(&cwd) {
        Ok(log) => {
            tracing::info!(path = %log.path().display(), "session transcript");
            agent.attach_session_log(log);
        }
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "session persistence disabled");
        }
    }
    let mut ui = ui::text::TextUi::stdio();
    cli::run_session(&mut agent, &mut ui)
}

fn login_openai_codex(method: LoginMethod) -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;

    match method {
        LoginMethod::Browser => auth::openai_codex::login_browser(&client, |auth| {
            println!("OpenAI Codex browser login");
            println!("Open: {}", auth.url);
            println!("Waiting for callback at {} ...", auth.redirect_uri);
        })?,
        LoginMethod::DeviceCode => auth::openai_codex::login_device_code(&client, |code| {
            println!("OpenAI Codex device-code login");
            println!("Open: {}", code.verification_uri);
            println!("Code: {}", code.user_code);
            println!("Waiting for authorization...");
        })?,
    }

    println!("Logged in to openai-codex.");
    Ok(())
}

fn print_help() {
    eprintln!("Usage:");
    eprintln!("  iris-agent                              Start interactive agent");
    eprintln!("  iris-agent login openai-codex           Login with browser OAuth (default)");
    eprintln!("  iris-agent login openai-codex --browser Login with browser OAuth");
    eprintln!("  iris-agent login openai-codex --device-code Login with device-code OAuth");
}
