use anyhow::Result;
use nexus::Agent;

mod auth;
mod nexus;
mod providers;

fn main() -> Result<()> {
    let provider = providers::openai_codex_responses::OpenAiCodexResponsesProvider::from_env()?;
    Agent::new(provider).run()
}
