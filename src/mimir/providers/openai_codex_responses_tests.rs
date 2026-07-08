use super::*;
use crate::mimir::selection::PromptCacheRetention;
use crate::nexus::ModelOrigin;
use std::path::Path;

#[derive(Default)]
struct RecordingSink {
    deltas: Vec<String>,
    reasoning_deltas: Vec<String>,
    raw_reasoning_deltas: Vec<String>,
    section_breaks: usize,
    tool_input_deltas: Vec<(String, String)>,
}

impl TurnSink for RecordingSink {
    fn on_text_delta(&mut self, delta: &str) -> Result<()> {
        self.deltas.push(delta.to_string());
        Ok(())
    }

    fn on_reasoning_delta(&mut self, delta: &str) -> Result<()> {
        self.reasoning_deltas.push(delta.to_string());
        Ok(())
    }

    fn on_raw_reasoning_delta(&mut self, delta: &str) -> Result<()> {
        self.raw_reasoning_deltas.push(delta.to_string());
        Ok(())
    }

    fn on_reasoning_section_break(&mut self) -> Result<()> {
        self.section_breaks += 1;
        Ok(())
    }

    fn on_tool_input_delta(&mut self, call_id: &str, delta: &str) -> Result<()> {
        self.tool_input_deltas
            .push((call_id.to_string(), delta.to_string()));
        Ok(())
    }
}

fn test_system_prompt() -> String {
    // The harness-owned baukasten is the single source of the instruction string
    // providers forward. Use the hermetic in-memory-defaults assembler (no HOME,
    // no disk, no project docs), which is all this request-shaping test needs.
    crate::wayland::system_prompt::assemble_defaults(
        Path::new("/tmp/iris"),
        &crate::tools::built_in_tools(),
    )
}

#[test]
fn codex_retry_auth_error_preserves_provider_and_safe_cause() {
    let cancel = CancellationToken::new();
    let err = run_with_retry(
        PROVIDER_ID,
        &crate::mimir::retry::RetryPolicy::default(),
        &cancel,
        |_force| Ok(()),
        |&()| Attempt::Reauth(anyhow!("HTTP 401 token rejected")),
    )
    .unwrap_err();

    let message = err.to_string();
    assert!(message.contains(PROVIDER_ID), "{message}");
    assert!(message.contains("HTTP 401 token rejected"), "{message}");
    assert_eq!(
        err.downcast_ref::<crate::errors::AuthError>()
            .and_then(crate::errors::AuthError::provider),
        Some(PROVIDER_ID)
    );
}

#[test]
fn streamed_failure_redacts_free_text_message() {
    let stream = concat!(
        "event: response.failed\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"type\":\"server_error\",\"code\":\"rate_limited\",\"message\":\"leak /home/alice/project secret sk-abc prompt text\"}}}\n\n",
    );

    let err = parse_response_stream(stream).unwrap_err().to_string();

    assert!(err.contains("Codex response failed"), "{err}");
    assert!(err.contains("type=server_error"), "{err}");
    assert!(err.contains("code=rate_limited"), "{err}");
    assert!(!err.contains("/home/alice"), "{err}");
    assert!(!err.contains("sk-abc"), "{err}");
    assert!(!err.contains("prompt text"), "{err}");
}

#[test]
fn non_success_body_diagnostics_redact_free_text_message() {
    let body = r#"{"error":{"type":"invalid_request_error","code":"bad_request","message":"leak /tmp/work prompt sk-secret"}}"#;

    let diag = CodexDiagnostics {
        status: 400,
        error_type: extract_error_field(body, "type"),
        error_code: extract_error_field(body, "code"),
        model: "gpt-test".to_string(),
        endpoint: "/codex/responses",
        last_event_type: None,
    }
    .to_string();

    assert!(diag.contains("error_type=invalid_request_error"), "{diag}");
    assert!(diag.contains("error_code=bad_request"), "{diag}");
    assert!(!diag.contains("/tmp/work"), "{diag}");
    assert!(!diag.contains("sk-secret"), "{diag}");
    assert!(!diag.contains("prompt"), "{diag}");
}

#[test]
fn hostile_type_and_code_tokens_are_omitted() {
    let body =
        r#"{"error":{"type":"leak_/home/alice","code":"sk-secret","message":"prompt text"}}"#;
    let diag = CodexDiagnostics {
        status: 400,
        error_type: extract_error_field(body, "type"),
        error_code: extract_error_field(body, "code"),
        model: "gpt-test".to_string(),
        endpoint: "/codex/responses",
        last_event_type: None,
    }
    .to_string();

    assert!(!diag.contains("/home/alice"), "{diag}");
    assert!(!diag.contains("sk-secret"), "{diag}");
    assert!(!diag.contains("prompt text"), "{diag}");
    assert!(!diag.contains("error_type="), "{diag}");
    assert!(!diag.contains("error_code="), "{diag}");
}

#[test]
fn malformed_stream_before_visible_text_is_retryable_protocol_anomaly() {
    let err = parse_response_stream("data: {not json}\n\n").unwrap_err();

    assert!(protocol_anomaly_retryable(&err, false), "{err}");
}

#[test]
fn malformed_stream_after_visible_text_is_not_retryable() {
    let stream = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        "data: {not json}\n\n",
    );
    let err = parse_response_stream(stream).unwrap_err();

    assert!(!protocol_anomaly_retryable(&err, true), "{err}");
}

struct FailingReader;

impl std::io::Read for FailingReader {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset",
        ))
    }
}

impl std::io::BufRead for FailingReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset",
        ))
    }

    fn consume(&mut self, _amt: usize) {}
}

#[test]
fn stream_read_error_before_visible_text_is_retryable() {
    let mut sink = RecordingSink::default();
    let err = parse_response_stream_reader(
        FailingReader,
        &mut sink,
        &CancellationToken::new(),
        "gpt-test",
    )
    .unwrap_err();

    assert!(protocol_anomaly_retryable(&err, false), "{err}");
}

#[test]
fn protocol_anomaly_redacts_hostile_last_event_type() {
    let stream = "data: {\"type\":\"sk-secret prompt /home/alice\"}\n\n";
    let err = parse_response_stream(stream).unwrap_err().to_string();

    assert!(!err.contains("sk-secret"), "{err}");
    assert!(!err.contains("prompt"), "{err}");
    assert!(!err.contains("/home/alice"), "{err}");
}

#[test]
fn resolves_codex_responses_url() -> Result<()> {
    assert_eq!(
        resolve_codex_url("https://chatgpt.com/backend-api")?.as_str(),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        resolve_codex_url("https://chatgpt.com/backend-api/codex")?.as_str(),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        resolve_codex_url("https://chatgpt.com/backend-api/codex/responses")?.as_str(),
        "https://chatgpt.com/backend-api/codex/responses"
    );
    Ok(())
}

#[test]
fn rejects_invalid_codex_base_url() {
    assert!(resolve_codex_url("not a url").is_err());
}

#[test]
fn builds_codex_request_from_conversation() {
    let instructions = test_system_prompt();
    let request = build_codex_request(
        "gpt-test",
        &instructions,
        &[Message::user("hello"), Message::assistant("hi")],
        &crate::tools::built_in_tools(),
        None,
        Some("session-1"),
        None,
        PromptCacheRetention::Short,
    );
    assert_eq!(request["model"], "gpt-test");
    assert_eq!(request["stream"], true);
    let instructions = request["instructions"].as_str().unwrap();
    assert!(instructions.contains("You are iris, a coding assistant"));
    assert!(instructions.contains("- read:"));
    assert!(instructions.contains("- ls:"));
    assert!(instructions.contains("No other tools are available"));
    assert!(instructions.contains("Current working directory: /tmp/iris"));
    assert_eq!(request["input"].as_array().unwrap().len(), 2);
    assert_eq!(request["input"][0]["role"], "user");
    assert_eq!(request["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(request["input"][0]["content"][0]["text"], "hello");
    assert_eq!(request["input"][1]["role"], "assistant");
    assert_eq!(request["input"][1]["content"][0]["type"], "output_text");
    assert_eq!(request["tools"][0]["name"], "read");
}

#[test]
fn builds_codex_request_from_tool_messages() {
    let instructions = test_system_prompt();
    let request = build_codex_request(
        "gpt-test",
        &instructions,
        &[
            Message {
                role: Role::AssistantToolCall,
                content: json!({ "path": "src/main.rs" }).to_string(),
                tool_call_id: Some("call_1".to_string()),
                tool_name: Some("read".to_string()),
                continuity: None,
                provider_turn_id: None,
                redacted: false,
                origin: None,
            },
            Message {
                role: Role::Tool,
                content: json!({ "ok": true, "content": "file text" }).to_string(),
                tool_call_id: Some("call_1".to_string()),
                tool_name: Some("read".to_string()),
                continuity: None,
                provider_turn_id: None,
                redacted: false,
                origin: None,
            },
        ],
        &crate::tools::built_in_tools(),
        None,
        Some("session-1"),
        None,
        PromptCacheRetention::Short,
    );

    assert_eq!(request["input"][0]["type"], "function_call");
    assert_eq!(request["input"][0]["call_id"], "call_1");
    assert_eq!(request["input"][0]["name"], "read");
    assert_eq!(request["input"][1]["type"], "function_call_output");
    assert_eq!(request["input"][1]["call_id"], "call_1");
    assert!(
        request["input"][1]["output"]
            .as_str()
            .unwrap()
            .contains("file text")
    );
}

#[test]
fn parses_streamed_output_text_delta_events() -> Result<()> {
    let stream = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    );
    let mut sink = RecordingSink::default();

    let turn = parse_response_stream_reader(
        BufReader::with_capacity(1, stream.as_bytes()),
        &mut sink,
        &CancellationToken::new(),
        "gpt-test",
    )?;

    assert_eq!(turn.text.as_deref(), Some("Hello"));
    assert_eq!(sink.deltas, vec!["Hel", "lo"]);
    Ok(())
}

/// Sink that fails on the first delta, standing in for a consumer that dropped
/// the [`ProviderStream`] (cancellation).
struct DroppedConsumerSink;
impl TurnSink for DroppedConsumerSink {
    fn on_text_delta(&mut self, _delta: &str) -> Result<()> {
        Err(anyhow!("response stream dropped by consumer"))
    }
}

#[test]
fn cancelled_token_stops_the_sse_read_early() {
    // With the turn already cancelled, the SSE read loop bails before parsing
    // rather than draining the streaming response.
    let stream = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    );
    let mut sink = RecordingSink::default();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let result = parse_response_stream_reader(
        BufReader::with_capacity(1, stream.as_bytes()),
        &mut sink,
        &cancel,
        "gpt-test",
    );

    let error = result.unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
}

#[test]
fn dropped_consumer_stops_the_sse_read_early() {
    // When the sink reports the consumer is gone, the SSE read loop must abort
    // immediately instead of draining the rest of the response (the live path
    // would otherwise keep a spawn_blocking thread downloading on a cancelled
    // turn).
    let stream = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    );
    let mut sink = DroppedConsumerSink;

    let result = parse_response_stream_reader(
        BufReader::with_capacity(1, stream.as_bytes()),
        &mut sink,
        &CancellationToken::new(),
        "gpt-test",
    );

    let error = result.unwrap_err();
    assert!(error.to_string().contains("dropped by consumer"), "{error}");
}

#[test]
fn streamed_failure_preserves_prior_deltas() {
    let stream = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        "event: response.failed\n",
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"boom\"}}}\n\n",
    );
    let mut sink = RecordingSink::default();

    let err = parse_response_stream_reader(
        stream.as_bytes(),
        &mut sink,
        &CancellationToken::new(),
        "gpt-test",
    )
    .unwrap_err();

    assert!(err.to_string().contains("Codex response failed"));
    assert_eq!(sink.deltas, vec!["partial"]);
}

#[test]
fn parses_streamed_output_item_done_events() -> Result<()> {
    let stream = concat!(
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello\"}]}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    );

    assert_eq!(
        parse_response_stream(stream)?.text.as_deref(),
        Some("Hello")
    );
    Ok(())
}

#[test]
fn parses_streamed_tool_call() -> Result<()> {
    let stream = concat!(
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"src/main.rs\\\"}\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    );

    let mut sink = RecordingSink::default();
    let turn = parse_response_stream_reader(
        stream.as_bytes(),
        &mut sink,
        &CancellationToken::new(),
        "gpt-test",
    )?;

    assert!(sink.deltas.is_empty());
    assert!(turn.text.is_none());
    assert_eq!(turn.tool_calls[0].id, "call_1");
    assert_eq!(turn.tool_calls[0].name, "read");
    assert_eq!(turn.tool_calls[0].arguments["path"], "src/main.rs");
    Ok(())
}

#[test]
fn parses_responses_output_text() -> Result<()> {
    let response = json!({
        "output": [{
            "type": "message",
            "content": [{ "type": "output_text", "text": "Hello" }]
        }]
    });
    assert_eq!(
        parse_response_json(response)?.text.as_deref(),
        Some("Hello")
    );
    Ok(())
}

#[test]
fn parses_responses_tool_call() -> Result<()> {
    let response = json!({
        "output": [{
            "type": "function_call",
            "call_id": "call_1",
            "name": "read",
            "arguments": { "path": "src/main.rs" }
        }]
    });

    let turn = parse_response_json(response)?;

    assert!(turn.text.is_none());
    assert_eq!(turn.tool_calls[0].id, "call_1");
    assert_eq!(turn.tool_calls[0].name, "read");
    assert_eq!(turn.tool_calls[0].arguments["path"], "src/main.rs");
    Ok(())
}

#[test]
fn rejects_response_without_text() {
    let response = json!({ "output": [{ "type": "message", "content": [] }] });
    let error = parse_response_json(response).unwrap_err().to_string();
    assert!(error.contains("did not include assistant text or tool calls"));
}

#[test]
fn reasoning_none_produces_todays_exact_body() {
    // The default (no preference) request must be byte-identical to today's: no
    // `reasoning` key and no prompt-cache request keys; `text.verbosity` still
    // "low".
    let instructions = test_system_prompt();
    let tools = crate::tools::built_in_tools();
    let messages = [Message::user("hello")];
    let none = build_codex_request(
        "gpt-test",
        &instructions,
        &messages,
        &tools,
        None,
        Some("session-1"),
        None,
        PromptCacheRetention::None,
    );
    let expected = json!({
        "model": "gpt-test",
        "store": false,
        "stream": true,
        "instructions": instructions,
        "input": none["input"].clone(),
        "tools": none["tools"].clone(),
        "text": { "verbosity": "low" },
    });
    assert_eq!(none, expected);
    assert!(none.get("reasoning").is_none(), "None must omit reasoning");
    assert!(none.get("prompt_cache_key").is_none());
    assert!(none.get("prompt_cache_retention").is_none());
}

#[test]
fn reasoning_level_adds_effort_and_off_omits() {
    let instructions = test_system_prompt();
    let tools = crate::tools::built_in_tools();
    let messages = [Message::user("hello")];

    let minimal = build_codex_request(
        "gpt-test",
        &instructions,
        &messages,
        &tools,
        Some(ReasoningEffort::Minimal),
        Some("session-1"),
        None,
        PromptCacheRetention::Short,
    );
    assert_eq!(
        minimal["reasoning"],
        json!({ "effort": "low", "summary": "auto" }),
        "Codex metadata maps semantic minimal to wire low"
    );

    let high = build_codex_request(
        "gpt-test",
        &instructions,
        &messages,
        &tools,
        Some(ReasoningEffort::High),
        Some("session-1"),
        None,
        PromptCacheRetention::Short,
    );
    assert_eq!(
        high["reasoning"],
        json!({ "effort": "high", "summary": "auto" })
    );
    assert_eq!(high["include"], json!(["reasoning.encrypted_content"]));
    // The rest of the body is unchanged from the None case.
    assert_eq!(high["text"], json!({ "verbosity": "low" }));

    let xhigh = build_codex_request(
        "gpt-test",
        &instructions,
        &messages,
        &tools,
        Some(ReasoningEffort::XHigh),
        Some("session-1"),
        None,
        PromptCacheRetention::Short,
    );
    assert_eq!(
        xhigh["reasoning"],
        json!({ "effort": "xhigh", "summary": "auto" })
    );

    // Off has no disable field on gpt-5.5, so it omits reasoning entirely.
    let off = build_codex_request(
        "gpt-test",
        &instructions,
        &messages,
        &tools,
        Some(ReasoningEffort::Off),
        Some("session-1"),
        None,
        PromptCacheRetention::Short,
    );
    assert!(off.get("reasoning").is_none(), "Off omits reasoning");
}

/// The request must ask for a reasoning summary; without it the Responses API
/// streams no summary deltas and the live thinking rail never fires (ADR-0050).
#[test]
fn reasoning_request_asks_for_summary_so_live_thinking_can_stream() {
    let instructions = test_system_prompt();
    let messages = [Message::user("hello")];
    let req = build_codex_request(
        "gpt-test",
        &instructions,
        &messages,
        &crate::tools::built_in_tools(),
        Some(ReasoningEffort::Medium),
        Some("session-1"),
        None,
        PromptCacheRetention::Short,
    );
    // Drives the live thinking rail: the summary deltas only stream when asked.
    assert_eq!(
        req["reasoning"]["summary"],
        json!("auto"),
        "the request must ask for a reasoning summary, or the Responses API streams no summary deltas and the live thinking rail never fires (ADR-0050)"
    );
}

#[test]
fn prompt_cache_key_is_clamped_to_64_unicode_scalars_and_omitted_when_disabled() {
    let instructions = test_system_prompt();
    let tools = crate::tools::built_in_tools();
    let messages = [Message::user("hello")];
    let long_key = format!("{}tail", "å".repeat(70));

    let cached = build_codex_request(
        "gpt-test",
        &instructions,
        &messages,
        &tools,
        None,
        Some(&long_key),
        None,
        PromptCacheRetention::Short,
    );
    let key = cached["prompt_cache_key"].as_str().expect("cache key");
    assert_eq!(key.chars().count(), 64);
    assert_eq!(key, "å".repeat(64));

    let long = build_codex_request(
        "gpt-test",
        &instructions,
        &messages,
        &tools,
        None,
        Some("session1"),
        None,
        PromptCacheRetention::Long,
    );
    assert_eq!(long["prompt_cache_key"], json!("session1"));
    assert_eq!(long["prompt_cache_retention"], json!("24h"));
    assert!(cached.get("prompt_cache_retention").is_none());

    let disabled = build_codex_request(
        "gpt-test",
        &instructions,
        &messages,
        &tools,
        None,
        Some("session-1"),
        None,
        PromptCacheRetention::None,
    );
    assert!(disabled.get("prompt_cache_key").is_none());
    assert!(disabled.get("prompt_cache_retention").is_none());
}

#[test]
fn codex_long_cache_and_previous_response_fields_are_explicit_opt_ins() {
    let instructions = test_system_prompt();
    let tools = crate::tools::built_in_tools();
    let messages = [Message::user("hello")];

    let request = build_codex_request(
        "gpt-test",
        &instructions,
        &messages,
        &tools,
        Some(ReasoningEffort::High),
        Some("session-1"),
        Some("resp_previous"),
        PromptCacheRetention::Long,
    );

    assert_eq!(request["prompt_cache_key"], json!("session-1"));
    assert_eq!(request["prompt_cache_retention"], json!("24h"));
    assert_eq!(request["previous_response_id"], json!("resp_previous"));
    assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));

    let omitted = build_codex_request(
        "gpt-test",
        &instructions,
        &messages,
        &tools,
        None,
        Some("session-1"),
        Some("   "),
        PromptCacheRetention::Short,
    );
    assert!(omitted.get("previous_response_id").is_none());
    assert!(omitted.get("prompt_cache_retention").is_none());
}

#[test]
fn reasoning_continuity_replay_requests_encrypted_reasoning() {
    let instructions = test_system_prompt();
    let tools = crate::tools::built_in_tools();
    let origin = ModelOrigin::new("openai-codex", "openai-codex-responses", "gpt-test");
    let messages = [
        Message::user("hello"),
        Message::assistant_reasoning("", "encrypted-reasoning", true, origin),
    ];

    let request = build_codex_request(
        "gpt-test",
        &instructions,
        &messages,
        &tools,
        None,
        Some("session-1"),
        None,
        PromptCacheRetention::Short,
    );

    assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
    let input = request["input"].as_array().expect("input array");
    assert!(input.iter().any(|item| {
        item["type"] == json!("reasoning")
            && item["encrypted_content"] == json!("encrypted-reasoning")
    }));
}

#[test]
fn parses_usage_response_id_and_encrypted_reasoning_from_stream() -> Result<()> {
    let stream = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"enc-1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"thought\"}]}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":100,\"output_tokens\":20,\"total_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":64},\"output_tokens_details\":{\"reasoning_tokens\":7}}}}\n\n",
    );

    let turn = parse_response_stream(stream)?;

    assert_eq!(turn.text.as_deref(), Some("Hi"));
    assert_eq!(turn.response_id.as_deref(), Some("resp_1"));
    let usage = turn.usage.expect("usage");
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.output_tokens, 20);
    assert_eq!(usage.cache_read_input_tokens, 64);
    assert_eq!(usage.cache_write_input_tokens, 0);
    assert_eq!(usage.reasoning_output_tokens, 7);
    assert_eq!(usage.total_tokens, 120);
    assert_eq!(turn.reasoning.len(), 1);
    assert_eq!(turn.reasoning[0].text, "thought");
    assert_eq!(turn.reasoning[0].continuity.as_deref(), Some("enc-1"));
    assert!(!turn.reasoning[0].redacted);
    Ok(())
}

#[test]
fn encrypted_reasoning_without_summary_is_continuity_not_redaction() {
    let origin = ModelOrigin::new("openai-codex", "openai-codex-responses", "gpt-test");
    let block = extract_reasoning_block(
        &json!({
            "type": "reasoning",
            "encrypted_content": "enc-only"
        }),
        &origin,
    )
    .expect("reasoning block");

    assert_eq!(block.text, "");
    assert_eq!(block.continuity.as_deref(), Some("enc-only"));
    assert!(!block.redacted);
}

#[test]
fn streams_reasoning_summary_deltas_and_section_breaks() -> Result<()> {
    // Summary deltas are forwarded display-only; a section break is emitted for
    // each new summary part AFTER the first (which opens the trace silently).
    let stream = concat!(
        "event: response.reasoning_summary_part.added\n",
        "data: {\"type\":\"response.reasoning_summary_part.added\",\"summary_index\":0}\n\n",
        "event: response.reasoning_summary_text.delta\n",
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"First \",\"summary_index\":0}\n\n",
        "event: response.reasoning_summary_text.delta\n",
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thought.\",\"summary_index\":0}\n\n",
        "event: response.reasoning_summary_part.added\n",
        "data: {\"type\":\"response.reasoning_summary_part.added\",\"summary_index\":1}\n\n",
        "event: response.reasoning_summary_text.delta\n",
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Second.\",\"summary_index\":1}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Answer\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    );
    let mut sink = RecordingSink::default();
    let turn = parse_response_stream_reader(
        BufReader::new(stream.as_bytes()),
        &mut sink,
        &CancellationToken::new(),
        "gpt-test",
    )?;
    assert_eq!(sink.reasoning_deltas, vec!["First ", "thought.", "Second."]);
    assert_eq!(
        sink.section_breaks, 1,
        "only the part.added after visible reasoning breaks"
    );
    // Display-only: summary text is never folded into the assistant answer.
    assert_eq!(turn.text.as_deref(), Some("Answer"));
    Ok(())
}

#[test]
fn streams_raw_reasoning_text_deltas_live_on_explicit_raw_channel() -> Result<()> {
    // Raw reasoning deltas use their own display-only channel so they are never
    // silently reclassified as reasoning-summary deltas.
    let stream = concat!(
        "event: response.reasoning_summary_text.delta\n",
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"summary\",\"summary_index\":0}\n\n",
        "event: response.reasoning_text.delta\n",
        "data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"raw cot\",\"content_index\":0}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Answer\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    );
    let mut sink = RecordingSink::default();
    let turn = parse_response_stream_reader(
        BufReader::new(stream.as_bytes()),
        &mut sink,
        &CancellationToken::new(),
        "gpt-test",
    )?;
    assert_eq!(sink.reasoning_deltas, vec!["summary"]);
    assert_eq!(sink.raw_reasoning_deltas, vec!["raw cot"]);
    assert_eq!(turn.text.as_deref(), Some("Answer"));
    Ok(())
}

#[test]
fn visible_reasoning_summary_disables_silent_retry() -> Result<()> {
    // Shown reasoning counts as visible output, so a later mid-stream protocol
    // anomaly is fatal (a retry would duplicate what the user saw).
    let mut parser = ResponseStreamParser::new("gpt-test");
    let mut sink = RecordingSink::default();
    parser.ingest_event(
        "{\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking\",\"summary_index\":0}",
        &mut sink,
    )?;
    assert!(parser.emitted_visible_output(), "summary is visible output");
    assert!(!parser.emitted_visible_text, "but not via assistant text");
    let anomaly = anyhow::Error::new(CodexStreamProtocolAnomaly::invalid_json(None));
    assert!(
        !protocol_anomaly_retryable(&anomaly, parser.emitted_visible_output()),
        "a protocol anomaly after visible reasoning must not be silently retried"
    );

    let mut parser = ResponseStreamParser::new("gpt-test");
    parser.ingest_event(
        "{\"type\":\"response.reasoning_text.delta\",\"delta\":\"raw\",\"content_index\":0}",
        &mut sink,
    )?;
    assert!(
        parser.emitted_visible_output(),
        "raw reasoning is visible output"
    );
    assert!(
        !protocol_anomaly_retryable(&anomaly, parser.emitted_visible_output()),
        "a protocol anomaly after visible raw reasoning must not be silently retried"
    );
    Ok(())
}

#[test]
fn streams_custom_tool_call_input_deltas() -> Result<()> {
    // Freeform/custom tool-call input fragments are forwarded display-only
    // (ADR-0039), carrying the streaming correlation id. They are never folded
    // into the assembled turn's text or tool calls.
    let stream = concat!(
        "event: response.custom_tool_call_input.delta\n",
        "data: {\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"call_1\",\"delta\":\"*** Begin Patch\"}\n\n",
        "event: response.custom_tool_call_input.delta\n",
        "data: {\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"call_1\",\"delta\":\"*** End Patch\"}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Done\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    );
    let mut sink = RecordingSink::default();
    let turn = parse_response_stream_reader(
        BufReader::new(stream.as_bytes()),
        &mut sink,
        &CancellationToken::new(),
        "gpt-test",
    )?;
    assert_eq!(
        sink.tool_input_deltas,
        vec![
            ("call_1".to_string(), "*** Begin Patch".to_string()),
            ("call_1".to_string(), "*** End Patch".to_string()),
        ]
    );
    // Display-only: the fragments are never folded into the assistant answer or
    // into the assembled tool calls.
    assert_eq!(turn.text.as_deref(), Some("Done"));
    assert!(
        turn.tool_calls.is_empty(),
        "deltas do not become tool calls"
    );
    Ok(())
}

#[test]
fn json_argument_deltas_are_not_streamed() -> Result<()> {
    // Only freeform/custom tool input streams. JSON-argument (`function`) tools
    // keep buffering their arguments until completion, so their argument deltas
    // are ignored (no display event).
    let stream = concat!(
        "event: response.function_call_arguments.delta\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"call_1\",\"delta\":\"{\\\"path\\\":\"}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Done\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
    );
    let mut sink = RecordingSink::default();
    parse_response_stream_reader(
        BufReader::new(stream.as_bytes()),
        &mut sink,
        &CancellationToken::new(),
        "gpt-test",
    )?;
    assert!(
        sink.tool_input_deltas.is_empty(),
        "JSON-argument tool deltas must stay buffered, not streamed"
    );
    Ok(())
}

#[test]
fn visible_tool_input_disables_silent_retry() -> Result<()> {
    // A shown freeform tool-input preview counts as visible output, so a later
    // mid-stream protocol anomaly is fatal (a retry would duplicate what the
    // user saw) -- exactly like assistant text or a reasoning summary.
    let mut parser = ResponseStreamParser::new("gpt-test");
    let mut sink = RecordingSink::default();
    parser.ingest_event(
        "{\"type\":\"response.custom_tool_call_input.delta\",\"item_id\":\"call_1\",\"delta\":\"*** Begin Patch\"}",
        &mut sink,
    )?;
    assert!(
        parser.emitted_visible_output(),
        "streamed tool input is visible output"
    );
    assert!(!parser.emitted_visible_text, "but not via assistant text");
    let anomaly = anyhow::Error::new(CodexStreamProtocolAnomaly::invalid_json(None));
    assert!(
        !protocol_anomaly_retryable(&anomaly, parser.emitted_visible_output()),
        "a protocol anomaly after visible tool input must not be silently retried"
    );
    Ok(())
}

#[test]
fn reasoning_display_text_is_summary_only_never_raw_content() {
    // Display/stored reasoning text is the human-readable summary ONLY; the raw
    // `content` chain-of-thought is never surfaced (ADR-0049).
    let text = extract_reasoning_text(&json!({
        "type": "reasoning",
        "summary": [{ "type": "summary_text", "text": "summary" }],
        "content": [{ "type": "reasoning_text", "text": "raw cot" }]
    }));
    assert_eq!(text, "summary");
}
