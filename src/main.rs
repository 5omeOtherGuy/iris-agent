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
            login_openai_codex()
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

fn run_agent() -> Result<()> {
    let provider = providers::openai_codex_responses::OpenAiCodexResponsesProvider::from_env()?;
    Agent::new(provider).run()
}

fn login_openai_codex() -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .build()?;
    auth::openai_codex::login_device_code(&client, |code| {
        println!("OpenAI Codex login");
        println!("Open: {}", code.verification_uri);
        println!("Code: {}", code.user_code);
        println!("Waiting for authorization...");
    })?;
    println!("Logged in to openai-codex.");
    Ok(())
}

fn print_help() {
    eprintln!("Usage:");
    eprintln!("  iris-agent                 Start interactive agent");
    eprintln!("  iris-agent login openai-codex  Login with ChatGPT Codex subscription");
}
