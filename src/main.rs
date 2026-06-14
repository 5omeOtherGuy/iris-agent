use std::env;
use std::time::Duration;

use anyhow::{Result, bail};
use nexus::Agent;
use reqwest::blocking::Client;

mod auth;
mod nexus;
mod providers;

fn main() -> Result<()> {
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
            bail!("unknown command")
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum LoginMethod {
    Browser,
    DeviceCode,
}

fn run_agent() -> Result<()> {
    let provider = providers::openai_codex_responses::OpenAiCodexResponsesProvider::from_env()?;
    Agent::new(provider, env::current_dir()?).run()
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
