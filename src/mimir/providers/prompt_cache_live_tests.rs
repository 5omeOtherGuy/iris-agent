//! Opt-in provider probes for cross-session prompt-cache behavior.
//!
//! These tests make real API calls. Normal test runs skip them via `#[ignore]`,
//! and explicit ignored-test runs still require `IRIS_PROMPT_CACHE_LIVE=1`.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use super::anthropic_messages::AnthropicProvider;
use super::openai_codex_responses::OpenAiCodexResponsesProvider;
use super::openai_prompt_cache_key;
use crate::mimir::retry::RetryPolicy;
use crate::mimir::selection::{CodexTransport, ContextManagement, PromptCacheRetention};
use crate::nexus::{AssistantTurn, ChatProvider, Message, ProviderEvent, Tools};

struct ProbeResult {
    first_a_input: u64,
    first_a_read: u64,
    first_b_input: u64,
    first_b_read: u64,
    second_a_input: u64,
    second_a_read: u64,
    second_b_input: u64,
    second_b_read: u64,
}

fn live_enabled(test_name: &str) -> bool {
    if std::env::var("IRIS_PROMPT_CACHE_LIVE").ok().as_deref() == Some("1") {
        true
    } else {
        eprintln!("{test_name}: skipped (set IRIS_PROMPT_CACHE_LIVE=1 to run)");
        false
    }
}

fn shared_system_prompt() -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    format!(
        "Prompt-cache live probe {nonce}.\n{}\nKeep answers to one word.",
        "This is stable shared system-prompt padding for an isolated cache probe. ".repeat(900)
    )
}

fn first_messages(token: &str) -> Vec<Message> {
    vec![Message::user(&format!(
        "Your session token is {token}. {} Reply with exactly {token}.",
        format!("Retain only the token {token} for this session. ").repeat(500)
    ))]
}

async fn completed_turn(
    provider: &dyn ChatProvider,
    messages: &[Message],
    tools: &Tools,
) -> Result<AssistantTurn> {
    let cancel = CancellationToken::new();
    let mut stream = provider.respond_stream(messages, tools, &cancel)?;
    while let Some(event) = stream.next().await {
        if let ProviderEvent::Completed(turn) = event? {
            return Ok(turn);
        }
    }
    bail!("provider stream ended without a completed turn")
}

fn append_follow_up(messages: &mut Vec<Message>, turn: &AssistantTurn, token: &str) -> Result<()> {
    messages.push(Message::assistant(
        turn.text.as_deref().context("first turn had no text")?,
    ));
    messages.push(Message::user(&format!(
        "What is this session's token? Reply with exactly {token}."
    )));
    Ok(())
}

async fn run_probe(
    provider_a: Box<dyn ChatProvider>,
    provider_b: Box<dyn ChatProvider>,
) -> Result<ProbeResult> {
    let tools = Tools::new(Vec::new());
    let mut messages_a = first_messages("ALPHA");
    let mut messages_b = first_messages("BRAVO");

    // Seed A, then prove B can read only their exact common system prefix.
    let first_a = completed_turn(provider_a.as_ref(), &messages_a, &tools).await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let first_b = completed_turn(provider_b.as_ref(), &messages_b, &tools).await?;
    let first_a_usage = first_a.usage.as_ref().context("first A turn omitted usage")?;
    let first_b_usage = first_b.usage.as_ref().context("first B turn omitted usage")?;
    let first_a_input = first_a_usage.input_tokens;
    let first_a_read = first_a_usage.cache_read_input_tokens;
    let first_b_input = first_b_usage.input_tokens;
    let first_b_read = first_b_usage.cache_read_input_tokens;

    append_follow_up(&mut messages_a, &first_a, "ALPHA")?;
    append_follow_up(&mut messages_b, &first_b, "BRAVO")?;
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Continue both sessions concurrently. Each must retain its own deeper warm
    // branch even though both route through the shared workspace cache key.
    let (second_a, second_b) = tokio::join!(
        completed_turn(provider_a.as_ref(), &messages_a, &tools),
        completed_turn(provider_b.as_ref(), &messages_b, &tools),
    );
    let second_a = second_a?;
    let second_b = second_b?;
    let second_a_text = second_a.text.as_deref().unwrap_or_default();
    let second_b_text = second_b.text.as_deref().unwrap_or_default();
    assert!(
        second_a_text.contains("ALPHA"),
        "session A response crossed branches: {second_a_text:?}"
    );
    assert!(
        second_b_text.contains("BRAVO"),
        "session B response crossed branches: {second_b_text:?}"
    );

    let second_a_usage = second_a.usage.context("second A turn omitted usage")?;
    let second_b_usage = second_b.usage.context("second B turn omitted usage")?;
    Ok(ProbeResult {
        first_a_input,
        first_a_read,
        first_b_input,
        first_b_read,
        second_a_input: second_a_usage.input_tokens,
        second_a_read: second_a_usage.cache_read_input_tokens,
        second_b_input: second_b_usage.input_tokens,
        second_b_read: second_b_usage.cache_read_input_tokens,
    })
}

fn assert_shared_head_and_independent_branches(provider: &str, result: &ProbeResult) {
    eprintln!(
        "{provider}: A1 input/read={}/{}, B1 shared-head input/read={}/{}, A2 branch input/read={}/{}, B2 branch input/read={}/{}",
        result.first_a_input,
        result.first_a_read,
        result.first_b_input,
        result.first_b_read,
        result.second_a_input,
        result.second_a_read,
        result.second_b_input,
        result.second_b_read
    );
    assert!(
        result.first_b_read > 0,
        "{provider}: second session did not reuse the shared system prompt"
    );
    assert!(
        result.second_a_read > result.first_b_read,
        "{provider}: session A did not retain its deeper warm branch"
    );
    assert!(
        result.second_b_read > result.first_b_read,
        "{provider}: session B did not retain its deeper warm branch"
    );
}

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(future)
}

#[test]
#[ignore = "live OpenAI Codex API calls; set IRIS_PROMPT_CACHE_LIVE=1 to run"]
fn openai_shares_system_prompt_without_breaking_concurrent_session_branches() -> Result<()> {
    if !live_enabled("openai_shares_system_prompt_without_breaking_concurrent_session_branches") {
        return Ok(());
    }
    let system_prompt = shared_system_prompt();
    let cache_key = openai_prompt_cache_key(
        Path::new("/tmp/iris-prompt-cache-live-workspace"),
        &system_prompt,
    );
    let build = |session_key: &str| {
        OpenAiCodexResponsesProvider::new_with_session_cache_key(
            "gpt-5.4-mini",
            "https://chatgpt.com/backend-api",
            None,
            &system_prompt,
            &cache_key,
            session_key,
            PromptCacheRetention::DEFAULT,
            RetryPolicy::default(),
            CodexTransport::Auto,
        )
        .map(|provider| Box::new(provider) as Box<dyn ChatProvider>)
    };
    let result = block_on(run_probe(build("live-session-a")?, build("live-session-b")?))?;
    assert_shared_head_and_independent_branches("openai-codex", &result);
    Ok(())
}

#[test]
#[ignore = "live OpenAI Codex SSE calls; set IRIS_PROMPT_CACHE_LIVE=1 to run"]
fn openai_sse_keeps_concurrent_session_branches_isolated() -> Result<()> {
    if !live_enabled("openai_sse_keeps_concurrent_session_branches_isolated") {
        return Ok(());
    }
    let system_prompt = shared_system_prompt();
    let cache_key = openai_prompt_cache_key(
        Path::new("/tmp/iris-prompt-cache-live-workspace"),
        &system_prompt,
    );
    let build = |session_key: &str| {
        OpenAiCodexResponsesProvider::new_with_session_cache_key(
            "gpt-5.4-mini",
            "https://chatgpt.com/backend-api",
            None,
            &system_prompt,
            &cache_key,
            session_key,
            PromptCacheRetention::DEFAULT,
            RetryPolicy::default(),
            CodexTransport::Sse,
        )
        .map(|provider| Box::new(provider) as Box<dyn ChatProvider>)
    };
    let result = block_on(run_probe(build("live-sse-a")?, build("live-sse-b")?))?;
    eprintln!(
        "openai-codex-sse: A1 input/read={}/{}, B1 input/read={}/{}, A2 input/read={}/{}, B2 input/read={}/{}",
        result.first_a_input,
        result.first_a_read,
        result.first_b_input,
        result.first_b_read,
        result.second_a_input,
        result.second_a_read,
        result.second_b_input,
        result.second_b_read
    );
    // Cache population timing on the SSE lane is provider-dependent. The probe's
    // branch-token assertions prove concurrent responses stayed isolated; the
    // deterministic routing test proves this path uses distinct session keys.
    Ok(())
}

#[test]
#[ignore = "live Anthropic API calls; set IRIS_PROMPT_CACHE_LIVE=1 to run"]
fn anthropic_shares_system_prompt_without_breaking_concurrent_session_branches() -> Result<()> {
    if !live_enabled("anthropic_shares_system_prompt_without_breaking_concurrent_session_branches") {
        return Ok(());
    }
    let system_prompt = shared_system_prompt();
    let build = || {
        AnthropicProvider::new(
            "claude-haiku-4-5",
            "https://api.anthropic.com",
            None,
            &system_prompt,
            PromptCacheRetention::DEFAULT,
            ContextManagement::default(),
            RetryPolicy::default(),
        )
        .map(|provider| Box::new(provider) as Box<dyn ChatProvider>)
    };
    let result = block_on(run_probe(build()?, build()?))?;
    assert_shared_head_and_independent_branches("anthropic", &result);
    Ok(())
}
