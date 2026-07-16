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
    let mut sink = RecordingSink::default();
    let err = run_with_retry(
        PROVIDER_ID,
        &crate::mimir::retry::RetryPolicy::default(),
        &cancel,
        &mut sink,
        |_force| Ok(()),
        |&(), _sink| Attempt::Reauth(anyhow!("HTTP 401 token rejected")),
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
fn resolves_codex_websocket_url_from_backend_url() -> Result<()> {
    assert_eq!(
        resolve_codex_ws_url("https://chatgpt.com/backend-api")?.as_str(),
        "wss://chatgpt.com/backend-api/codex/responses"
    );
    assert_eq!(
        resolve_codex_ws_url("http://localhost:8080/backend-api/codex")?.as_str(),
        "ws://localhost:8080/backend-api/codex/responses"
    );
    assert_eq!(
        resolve_codex_ws_url("https://chatgpt.com/backend-api/codex/responses")?.as_str(),
        "wss://chatgpt.com/backend-api/codex/responses"
    );
    Ok(())
}

#[test]
fn codex_ws_headers_use_oauth_metadata_without_content_type() -> Result<()> {
    let token = AccessToken {
        bearer: "secret-token".to_string(),
        account_id: "acct_123".to_string(),
    };
    let headers = ws_headers_for_test(&token, "session-1")?;
    assert_eq!(
        headers.get(AUTHORIZATION.as_str()).unwrap(),
        "Bearer secret-token"
    );
    assert_eq!(headers.get("chatgpt-account-id").unwrap(), "acct_123");
    assert_eq!(headers.get("originator").unwrap(), "iris");
    assert_eq!(headers.get(USER_AGENT.as_str()).unwrap(), "iris-agent");
    assert_eq!(
        headers.get("OpenAI-Beta").unwrap(),
        "responses_websockets=2026-02-06"
    );
    assert_eq!(headers.get("session-id").unwrap(), "session-1");
    assert_eq!(
        headers.get("x-client-request-id").unwrap(),
        "iris-session-1"
    );
    assert!(headers.get(CONTENT_TYPE.as_str()).is_none());
    Ok(())
}

#[test]
fn websocket_setup_timeout_retries_before_visible_output() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancel = CancellationToken::new();

    let (policy, error) = runtime
        .block_on(await_ws_setup(
            "connect",
            std::time::Duration::from_millis(1),
            &cancel,
            false,
            std::future::pending::<Result<()>>(),
        ))
        .unwrap_err();

    assert_eq!(policy, WsFallback::RetryWebSocket);
    assert!(error.to_string().contains("timed out"), "{error}");
}

#[test]
fn websocket_setup_timeout_after_visible_output_is_fatal() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancel = CancellationToken::new();

    let (policy, error) = runtime
        .block_on(await_ws_setup(
            "pong",
            std::time::Duration::from_millis(1),
            &cancel,
            true,
            std::future::pending::<Result<()>>(),
        ))
        .unwrap_err();

    assert_eq!(policy, WsFallback::Fatal);
    assert!(error.to_string().contains("timed out"), "{error}");
}

#[test]
fn websocket_setup_cancel_stops_before_connect_or_send_complete() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let (policy, error) = runtime
        .block_on(await_ws_setup(
            "send",
            std::time::Duration::from_secs(30),
            &cancel,
            false,
            std::future::pending::<Result<()>>(),
        ))
        .unwrap_err();

    assert_eq!(policy, WsFallback::Fatal);
    assert!(error.to_string().contains("cancelled"), "{error}");
}

#[test]
fn websocket_setup_failure_is_classified_and_redacted() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancel = CancellationToken::new();

    let (policy, error) = runtime
        .block_on(await_ws_setup(
            "connect",
            std::time::Duration::from_secs(1),
            &cancel,
            false,
            async { Err::<(), _>(anyhow!("handshake failed /home/alice sk-secret prompt")) },
        ))
        .unwrap_err();
    let message = error.to_string();

    assert_eq!(policy, WsFallback::RetryWebSocket);
    assert!(
        message.contains("classification=websocket_setup_error"),
        "{message}"
    );
    assert!(message.contains("phase=connect"), "{message}");
    assert!(!message.contains("/home/alice"), "{message}");
    assert!(!message.contains("sk-secret"), "{message}");
    assert!(!message.contains("prompt"), "{message}");
}

#[test]
fn websocket_idle_fallback_metadata_is_safe_and_complete() {
    let fallback = ws_transport_fallback(
        "gpt-test /home/alice sk-secret",
        "read_idle",
        "awaiting_next_frame",
        300_000,
        3,
        Some("response.created /tmp/secret"),
    );

    assert_eq!(fallback.provider, PROVIDER_ID);
    assert_eq!(fallback.model, "redacted");
    assert_eq!(fallback.from_transport, "websocket");
    assert_eq!(fallback.to_transport, "https_sse");
    assert_eq!(fallback.reason, "read_idle");
    assert_eq!(fallback.phase, "awaiting_next_frame");
    assert_eq!(fallback.idle_ms, 300_000);
    assert_eq!(fallback.ws_attempt, 3);
    assert_eq!(fallback.reconnect_count, 2);
    assert_eq!(fallback.last_event, None);
}

#[test]
fn websocket_read_idle_before_visible_output_retries_with_diagnostics() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancel = CancellationToken::new();

    let (policy, error) = runtime
        .block_on(await_ws_message(
            Some(std::time::Duration::from_millis(1)),
            &cancel,
            false,
            "awaiting_first_frame",
            std::future::pending::<()>(),
        ))
        .unwrap_err();
    let message = error.to_string();

    assert_eq!(policy, WsFallback::RetryWebSocket);
    assert!(
        message.contains("classification=provider_transport_idle"),
        "{message}"
    );
    assert!(message.contains("transport=websocket"), "{message}");
    assert!(message.contains("phase=awaiting_first_frame"), "{message}");
    assert!(message.contains("visible_output=false"), "{message}");
}

#[test]
fn websocket_read_idle_after_visible_output_is_fatal() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancel = CancellationToken::new();

    let (policy, error) = runtime
        .block_on(await_ws_message(
            Some(std::time::Duration::from_millis(1)),
            &cancel,
            true,
            "awaiting_next_frame",
            std::future::pending::<()>(),
        ))
        .unwrap_err();
    let message = error.to_string();

    assert_eq!(policy, WsFallback::Fatal);
    assert!(message.contains("phase=awaiting_next_frame"), "{message}");
    assert!(message.contains("visible_output=true"), "{message}");
}

#[test]
fn websocket_raw_activity_resets_the_sliding_idle_timer() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancel = CancellationToken::new();

    runtime.block_on(async {
        for frame in 0..3 {
            let received = await_ws_message(
                Some(std::time::Duration::from_millis(30)),
                &cancel,
                false,
                "awaiting_next_frame",
                async move {
                    tokio::time::sleep(std::time::Duration::from_millis(15)).await;
                    frame
                },
            )
            .await
            .expect("each raw frame arrives within its own sliding window");
            assert_eq!(received, frame);
        }
    });
}

#[test]
fn disabled_websocket_idle_timeout_still_honors_cancellation() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();

    let (policy, error) = runtime
        .block_on(async move {
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                trigger.cancel();
            });
            await_ws_message(
                None,
                &cancel,
                false,
                "awaiting_first_frame",
                std::future::pending::<()>(),
            )
            .await
        })
        .unwrap_err();

    assert_eq!(policy, WsFallback::Fatal);
    assert!(error.to_string().contains("cancelled"), "{error}");
}

#[test]
fn async_sse_decoder_handles_split_chunks_and_multiline_events() -> Result<()> {
    let mut decoder = CodexSseDecoder::default();
    let mut events = Vec::new();

    decoder.push(
        b"event: response.output_text.delta\ndata: {\"type\":",
        |data| {
            events.push(data.to_string());
            Ok(())
        },
    )?;
    decoder.push(
        b"\"response.output_text.delta\",\ndata: \"delta\":\"hi\"}\n\n",
        |data| {
            events.push(data.to_string());
            Ok(())
        },
    )?;
    decoder.finish(|data| {
        events.push(data.to_string());
        Ok(())
    })?;

    assert_eq!(
        events,
        ["{\"type\":\"response.output_text.delta\",\n\"delta\":\"hi\"}"]
    );
    Ok(())
}

#[test]
fn websocket_special_recovery_uses_typed_provider_fields_not_message_text() {
    let previous = ws_provider_error(
        &json!({
            "type": "error",
            "error": {"code": "previous_response_not_found"}
        }),
        "error",
        "safe diagnostics".to_string(),
    );
    assert_eq!(
        classify_ws_error(&previous, false),
        WsFallback::RetryFullWebSocket
    );

    let misleading = ws_provider_error(
        &json!({
            "type": "error",
            "error": {"code": "other"}
        }),
        "error",
        "message mentions previous_response_not_found and 401".to_string(),
    );
    assert_eq!(classify_ws_error(&misleading, false), WsFallback::Fatal);

    let bad_request = ws_provider_error(
        &json!({
            "type": "response.failed",
            "response": {"status": 400, "error": {"code": "invalid_request"}}
        }),
        "response.failed",
        "safe diagnostics".to_string(),
    );
    assert_eq!(classify_ws_error(&bad_request, false), WsFallback::Fatal);

    let rate_limited = ws_provider_error(
        &json!({
            "type": "response.failed",
            "response": {"status": 429, "error": {"code": "rate_limited"}}
        }),
        "response.failed",
        "safe diagnostics".to_string(),
    );
    assert_eq!(
        classify_ws_error(&rate_limited, false),
        WsFallback::RetryWebSocket
    );

    let unauthorized = ws_provider_error(
        &json!({
            "type": "response.failed",
            "response": {"status": "403", "error": {"code": "forbidden"}}
        }),
        "response.failed",
        "safe diagnostics".to_string(),
    );
    assert_eq!(
        classify_ws_error(&unauthorized, false),
        WsFallback::ForceRefresh
    );

    for (status, expected) in [
        (400, WsFallback::Fatal),
        (429, WsFallback::RetryWebSocket),
        (503, WsFallback::RetryWebSocket),
    ] {
        let response = tokio_tungstenite::tungstenite::http::Response::builder()
            .status(status)
            .body(None)
            .unwrap();
        let error: anyhow::Error =
            tokio_tungstenite::tungstenite::Error::Http(Box::new(response)).into();
        assert_eq!(classify_ws_error(&error, false), expected);
    }
}

#[test]
fn websocket_reconnect_forces_full_context_on_the_new_connection() {
    let mut recovery = WsRecoveryState::default();

    let decision = recovery.on_failure(WsFallback::RetryWebSocket, 3);
    assert_eq!(
        decision,
        WsRecoveryDecision::Retry {
            reconnect_count: 1,
            force_full: true,
            force_refresh: false,
        }
    );

    let full = json!({"model": "gpt-test", "input": [{"type": "message"}]});
    let continuation = CodexContinuation {
        last_full_body: full.clone(),
        last_response_id: "response_from_dropped_socket".to_string(),
        last_response_items: Vec::new(),
    };
    let force_full = match decision {
        WsRecoveryDecision::Retry { force_full, .. } => force_full,
        _ => unreachable!(),
    };
    let frame = build_ws_create_frame(&full, Some(&continuation), force_full);
    assert!(frame.get("previous_response_id").is_none());
    assert_eq!(frame["input"], full["input"]);
}

#[test]
fn websocket_retry_budget_counts_special_recovery_and_exhausts_once() {
    let mut recovery = WsRecoveryState::default();

    assert_eq!(
        recovery.on_failure(WsFallback::RetryFullWebSocket, 2),
        WsRecoveryDecision::Retry {
            reconnect_count: 1,
            force_full: true,
            force_refresh: false,
        }
    );
    assert_eq!(
        recovery.on_failure(WsFallback::ForceRefresh, 2),
        WsRecoveryDecision::Retry {
            reconnect_count: 2,
            force_full: true,
            force_refresh: true,
        }
    );
    assert_eq!(
        recovery.on_failure(WsFallback::RetryWebSocket, 2),
        WsRecoveryDecision::FallbackSse,
        "special retries consume rather than reset the transient budget"
    );
    assert_eq!(
        recovery.on_failure(WsFallback::RetryWebSocket, 2),
        WsRecoveryDecision::FallbackSse,
        "exhaustion remains terminal and cannot ping-pong back to WebSocket"
    );
}

#[test]
fn cancellation_during_websocket_backoff_is_prompt() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let started = std::time::Instant::now();

    let error = runtime
        .block_on(sleep_ws_backoff(
            std::time::Duration::from_secs(60),
            &cancel,
        ))
        .unwrap_err();

    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert!(error.to_string().contains("cancelled"), "{error}");
}

#[test]
fn websocket_close_diagnostics_keep_code_and_only_safe_reason() {
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    let safe = websocket_close_error(
        Some(CloseFrame {
            code: CloseCode::Away,
            reason: "server_restart".into(),
        }),
        Some("response.created".to_string()),
    )
    .to_string();
    assert!(safe.contains("close_code=1001"), "{safe}");
    assert!(safe.contains("close_reason=server_restart"), "{safe}");
    assert!(safe.contains("last_event=response.created"), "{safe}");

    let hostile = websocket_close_error(
        Some(CloseFrame {
            code: CloseCode::Error,
            reason: "leak /home/alice sk-secret prompt".into(),
        }),
        None,
    )
    .to_string();
    assert!(hostile.contains("close_code=1011"), "{hostile}");
    assert!(!hostile.contains("/home/alice"), "{hostile}");
    assert!(!hostile.contains("sk-secret"), "{hostile}");
    assert!(!hostile.contains("prompt"), "{hostile}");
}

#[test]
fn websocket_create_frame_omits_stream_and_keeps_store_false() {
    let full = json!({
        "model": "gpt-test",
        "store": false,
        "stream": true,
        "background": false,
        "input": [{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],
        "tools": [],
        "reasoning": {"effort":"low"},
        "prompt_cache_key": "session-1"
    });

    let frame = build_ws_create_frame(&full, None, false);

    assert_eq!(frame["type"], "response.create");
    assert_eq!(frame["store"], false);
    assert_eq!(frame["model"], "gpt-test");
    assert!(frame.get("stream").is_none());
    assert!(frame.get("background").is_none());
    assert_eq!(frame["reasoning"]["effort"], "low");
    assert_eq!(frame["prompt_cache_key"], "session-1");
}

#[test]
fn websocket_continuation_sends_only_suffix_when_prefix_matches() {
    let prior = json!({
        "model": "gpt-test",
        "store": false,
        "input": [
            {"type":"message","role":"user","content":[{"type":"input_text","text":"one"}]}
        ],
        "tools": []
    });
    let assistant = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"two"}]});
    let next_user =
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"three"}]});
    let current = json!({
        "model": "gpt-test",
        "store": false,
        "stream": true,
        "input": [prior["input"][0].clone(), assistant, next_user],
        "tools": []
    });
    let continuation = CodexContinuation {
        last_full_body: prior,
        last_response_id: "resp_1".to_string(),
        last_response_items: vec![assistant],
    };

    let frame = build_ws_create_frame(&current, Some(&continuation), false);

    assert_eq!(frame["previous_response_id"], "resp_1");
    assert_eq!(frame["input"], Value::Array(vec![next_user]));
}

#[test]
fn websocket_continuation_normalizes_server_output_items_for_prefix_match() {
    let prior_user =
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"one"}]});
    let prior = json!({
        "model": "gpt-test",
        "store": false,
        "input": [prior_user],
        "tools": []
    });
    let response = json!({
        "id": "resp_1",
        "output": [
            {
                "id": "rs_1",
                "type": "reasoning",
                "status": "completed",
                "encrypted_content": " encrypted-reasoning ",
                "summary": [{"type":"summary_text","text":"thinking"}]
            },
            {
                "id": "msg_1",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type":"output_text","text":"two","annotations":[]}]
            },
            {
                "id": "fc_1",
                "type": "function_call",
                "status": "completed",
                "call_id": "call_1",
                "name": "read",
                "arguments": "{ \"path\" : \"src/main.rs\" }"
            }
        ]
    });
    let normalized = normalize_response_items_for_continuation(&response).unwrap();
    let reasoning = json!({
        "type": "reasoning",
        "encrypted_content": "encrypted-reasoning",
        "summary": []
    });
    let assistant = json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"two"}]});
    let tool_call = json!({
        "type": "function_call",
        "call_id": "call_1",
        "name": "read",
        "arguments": json!({"path":"src/main.rs"}).to_string()
    });
    let tool_result = json!({
        "type": "function_call_output",
        "call_id": "call_1",
        "output": "ok"
    });
    let current = json!({
        "model": "gpt-test",
        "store": false,
        "stream": true,
        "input": [prior_user, reasoning, assistant, tool_call, tool_result],
        "tools": []
    });
    let continuation = CodexContinuation {
        last_full_body: prior,
        last_response_id: "resp_1".to_string(),
        last_response_items: normalized,
    };

    let frame = build_ws_create_frame(&current, Some(&continuation), false);

    assert_eq!(frame["previous_response_id"], "resp_1");
    assert_eq!(frame["input"], Value::Array(vec![tool_result]));
}

#[test]
fn websocket_continuation_falls_back_to_full_on_shape_mismatch() {
    let prior = json!({"model":"gpt-test","store":false,"input":[],"tools":[]});
    let current_input = vec![
        json!({"type":"message","role":"user","content":[{"type":"input_text","text":"new"}]}),
    ];
    let current =
        json!({"model":"other","store":false,"stream":true,"input":current_input,"tools":[]});
    let continuation = CodexContinuation {
        last_full_body: prior,
        last_response_id: "resp_1".to_string(),
        last_response_items: vec![],
    };

    let frame = build_ws_create_frame(&current, Some(&continuation), false);

    assert!(frame.get("previous_response_id").is_none());
    assert_eq!(frame["input"], current["input"]);
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
    assert!(instructions.contains("Available tools: read, bash"));
    assert!(instructions.contains(", ls,"));
    assert!(instructions.contains("Use only these tools"));
    assert!(instructions.contains("Current working directory: /tmp/iris"));
    assert_eq!(request["input"].as_array().unwrap().len(), 2);
    assert_eq!(request["input"][0]["role"], "user");
    assert_eq!(request["input"][0]["content"][0]["type"], "input_text");
    assert_eq!(request["input"][0]["content"][0]["text"], "hello");
    assert_eq!(request["input"][1]["role"], "assistant");
    assert_eq!(request["input"][1]["content"][0]["type"], "output_text");
    let read = &request["tools"][0];
    assert_eq!(read["name"], "read");
    assert!(
        read["description"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    );
    assert_eq!(read["parameters"]["type"], "object");
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
                provider_blocks: Vec::new(),
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
                provider_blocks: Vec::new(),
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
fn remote_v2_compaction_probe_is_flag_gated_and_uses_a_trigger_item() {
    assert!(!openai_native_probe_enabled(None));
    assert!(openai_native_probe_enabled(Some("1")));
    assert!(openai_native_probe_enabled(Some("true")));
    assert!(!openai_native_probe_enabled(Some("0")));

    let request = build_codex_native_compaction_request(
        "gpt-5.4-mini",
        "IRIS PROMPT",
        &[Message::user("old context")],
        PromptCacheRetention::Short,
    );
    let input = request["input"].as_array().unwrap();
    assert_eq!(
        input.last().unwrap(),
        &json!({ "type": "compaction_trigger" })
    );
    assert!(request["tools"].as_array().unwrap().is_empty());

    let response = json!({
        "output": [{ "type": "compaction", "encrypted_content": "opaque" }]
    });
    assert_eq!(
        extract_codex_compaction_block(&response, "gpt-5.4-mini"),
        Some(json!({
            "adapter": "openai-codex-responses",
            "model": "gpt-5.4-mini",
            "block": { "type": "compaction", "encrypted_content": "opaque" }
        }))
    );
}

#[test]
fn remote_v2_compaction_probe_captures_one_deduplicated_stream_item() {
    let stream = concat!(
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"opaque\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"compaction\",\"encrypted_content\":\"opaque\"}]}}\n\n",
        "data: [DONE]\n\n"
    );
    let output = parse_codex_compaction_probe_reader(
        std::io::Cursor::new(stream),
        "gpt-5.4-mini",
        &CancellationToken::new(),
    )
    .unwrap();
    let block = output.block;
    assert_eq!(block["adapter"], "openai-codex-responses");
    assert_eq!(block["model"], "gpt-5.4-mini");
    assert_eq!(block["block"]["encrypted_content"], "opaque");
}

#[test]
fn native_compaction_parser_captures_usage_and_merge_accounts_for_both_calls() {
    let stream = concat!(
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"encrypted_content\":\"opaque\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"compaction\",\"encrypted_content\":\"opaque\"}],\"usage\":{\"input_tokens\":100,\"output_tokens\":5,\"total_tokens\":105}}}\n\n",
        "data: [DONE]\n\n"
    );
    let native = parse_codex_compaction_probe_reader(
        std::io::Cursor::new(stream),
        "gpt-test-usage",
        &CancellationToken::new(),
    )
    .unwrap();
    let summary = ProviderUsage {
        provider: PROVIDER_ID.to_string(),
        model: "gpt-test-usage".to_string(),
        input_tokens: 80,
        output_tokens: 20,
        cache_read_input_tokens: 10,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 4,
        total_tokens: 100,
        cache_creation: None,
    };
    let merged = merge_openai_compaction_usage(native.usage, Some(summary)).unwrap();
    assert_eq!(merged.input_tokens, 180);
    assert_eq!(merged.output_tokens, 25);
    assert_eq!(merged.total_tokens, 205);
    assert_eq!(merged.reasoning_output_tokens, 4);
}

#[test]
fn native_compaction_failure_classification_does_not_cache_overflow() {
    assert_eq!(
        classify_codex_native_failure(400, r#"{"error":{"code":"context_length_exceeded"}}"#),
        CodexNativeFailure::Fatal,
    );
    assert_eq!(
        classify_codex_native_failure(400, r#"{"error":{"code":"unsupported_feature"}}"#),
        CodexNativeFailure::Unsupported,
    );
    assert_eq!(
        classify_codex_native_failure(401, ""),
        CodexNativeFailure::Reauth,
    );
    assert_eq!(
        classify_codex_native_failure(429, ""),
        CodexNativeFailure::Retry,
    );
}

#[test]
fn native_compaction_capability_has_no_input_floor_and_honors_rejection_cache() {
    let model = "gpt-test-native-capability-cache";
    assert_eq!(
        codex_native_compaction_capability(model),
        ProviderCompactionCapability::OpaqueBlocks,
    );
    NATIVE_COMPACTION_UNSUPPORTED_MODELS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap()
        .insert(model.to_string());
    assert_eq!(
        codex_native_compaction_capability(model),
        ProviderCompactionCapability::None,
    );
}

#[test]
fn native_compaction_portable_summary_prompt_is_text_only_and_preserves_focus() {
    let prompt = native_compaction_summary_instructions("retain issue 42 and src/lib.rs");
    assert!(prompt.contains("self-contained handoff summary"));
    assert!(prompt.contains("do not call tools"));
    assert!(prompt.contains("retain issue 42 and src/lib.rs"));
}

#[test]
fn native_compaction_summary_directive_is_the_final_user_turn() {
    // Regression: a covered range ending on an unanswered user question must
    // not be the last turn the summary worker sees, or the model answers it
    // instead of summarizing.
    let covered = vec![
        Message::user("what does the API use?"),
        Message::assistant("the API uses low"),
        Message::user("but what does OpenAI use, not what does iris use"),
    ];
    let request = native_compaction_summary_messages(&covered, "keep exact effort names");
    assert_eq!(request.len(), covered.len() + 1);
    let last = request.last().expect("directive appended");
    assert_eq!(last.role, Role::User);
    assert!(last.content.contains("self-contained handoff summary"));
    assert!(last.content.contains("keep exact effort names"));
}

#[test]
fn cross_provider_request_uses_portable_summary_and_ignores_anthropic_block() {
    let message =
        Message::user("portable cross-provider summary").with_provider_blocks(vec![json!({
            "adapter": "anthropic-messages",
            "model": "claude-opus-4-6",
            "block": { "type": "compaction", "content": "opaque-anthropic-summary" }
        })]);
    let request = build_codex_request(
        "gpt-5.4-mini",
        "IRIS PROMPT",
        &[message],
        &Tools::new(Vec::new()),
        None,
        None,
        None,
        PromptCacheRetention::Short,
    );
    let encoded = serde_json::to_string(&request).unwrap();
    assert!(encoded.contains("portable cross-provider summary"));
    assert!(!encoded.contains("opaque-anthropic-summary"));
    assert!(!encoded.contains("providerBlocks"));
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

    let max = build_codex_request(
        "gpt-5.6-sol",
        &instructions,
        &messages,
        &tools,
        Some(ReasoningEffort::Max),
        Some("session-1"),
        None,
        PromptCacheRetention::Short,
    );
    assert_eq!(
        max["reasoning"],
        json!({ "effort": "max", "summary": "auto" }),
        "GPT-5.6 native max must reach the Codex Responses request unchanged"
    );

    let unsupported_max = build_codex_request(
        "gpt-5.5",
        &instructions,
        &messages,
        &tools,
        Some(ReasoningEffort::Max),
        Some("session-1"),
        None,
        PromptCacheRetention::Short,
    );
    assert!(
        unsupported_max.get("reasoning").is_none(),
        "provider adapters must not silently send unsupported wire levels"
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
fn completed_reasoning_summary_overrides_output_item_placeholder() -> Result<()> {
    let stream = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Answer\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"enc\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"**Planning**\\n\\n<!-- -->\"}]}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[{\"type\":\"reasoning\",\"encrypted_content\":\"enc\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"**Planning**\\n\\nReal summary.\"}]}]}}\n\n",
    );

    let turn = parse_response_stream(stream)?;

    assert_eq!(turn.reasoning[0].text, "**Planning**\n\nReal summary.");
    Ok(())
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
fn parses_cache_write_tokens_from_input_tokens_details() -> Result<()> {
    // GPT-5.6+ reports prompt-cache writes alongside reads. Both must be
    // parsed independently from `input_tokens_details`.
    let stream = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":100,\"output_tokens\":20,\"total_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":64,\"cache_write_tokens\":36},\"output_tokens_details\":{\"reasoning_tokens\":7}}}}\n\n",
    );

    let turn = parse_response_stream(stream)?;
    let usage = turn.usage.expect("usage");
    assert_eq!(usage.cache_read_input_tokens, 64);
    assert_eq!(usage.cache_write_input_tokens, 36);
    Ok(())
}

#[test]
fn cache_write_tokens_absent_defaults_to_zero() -> Result<()> {
    // Older model families never send the field; parse must fall back to 0.
    let stream = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":100,\"output_tokens\":20,\"total_tokens\":120,\"input_tokens_details\":{\"cached_tokens\":64}}}}\n\n",
    );

    let turn = parse_response_stream(stream)?;
    let usage = turn.usage.expect("usage");
    assert_eq!(usage.cache_write_input_tokens, 0);
    Ok(())
}

#[test]
fn cache_write_tokens_passed_through_verbatim_even_when_exceeding_prompt() -> Result<()> {
    // Real-world observed shape: reads + writes can exceed prompt_tokens.
    // We record faithfully and never clamp or "fix" the provider's numbers.
    let stream = concat!(
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"usage\":{\"input_tokens\":4583,\"output_tokens\":20,\"total_tokens\":4603,\"input_tokens_details\":{\"cached_tokens\":3945,\"cache_write_tokens\":4580}}}}\n\n",
    );

    let turn = parse_response_stream(stream)?;
    let usage = turn.usage.expect("usage");
    assert_eq!(usage.input_tokens, 4583);
    assert_eq!(usage.cache_read_input_tokens, 3945);
    assert_eq!(usage.cache_write_input_tokens, 4580);
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

#[test]
fn developer_context_keeps_its_native_responses_role() {
    let origin = ModelOrigin::new(PROVIDER_ID, API_ID, "gpt-test");
    let item = codex_input_item(&Message::developer("skill catalog"), &origin).unwrap();

    assert_eq!(item["type"], json!("message"));
    assert_eq!(item["role"], json!("developer"));
    assert_eq!(item["content"][0]["type"], json!("input_text"));
    assert_eq!(item["content"][0]["text"], json!("skill catalog"));
}

// -- issue #475 / ADR-0061: structured-output compaction summary request
// plumbing --------------------------------------------------------------

#[test]
fn native_summary_request_sets_json_schema_format_and_no_tools() {
    use crate::wayland::structured_summary::canonical_compaction_schema;

    let request = build_codex_summary_request(
        "gpt-test",
        "summarize sessions",
        &[Message::user("F range=msg_1..msg_5\nU hi")],
        PromptCacheRetention::Short,
    );

    assert_eq!(request["tools"], json!([]));
    assert_eq!(request["text"]["verbosity"], json!("low"));
    assert_eq!(request["text"]["format"]["type"], json!("json_schema"));
    assert_eq!(
        request["text"]["format"]["name"],
        json!("compaction_summary")
    );
    assert_eq!(request["text"]["format"]["strict"], json!(true));
    assert_eq!(
        request["text"]["format"]["schema"],
        canonical_compaction_schema()
    );
    // ADR-0061: the ChatGPT backend-api `/codex/responses` OAuth lane 400s on
    // a top-level `max_output_tokens` (`Unsupported parameter`), unlike the
    // OpenAI platform Responses API #475's literal JSON was modeled on.
    // Token bounding relies on `text.verbosity` plus the model's own output
    // cap instead, so the summary request must never add it.
    assert!(
        request.get("max_output_tokens").is_none(),
        "must omit max_output_tokens (ADR-0061 probe: 400 Unsupported parameter)"
    );
    // Unrelated request shaping is unchanged: still the normal Codex envelope.
    assert_eq!(request["model"], json!("gpt-test"));
    assert_eq!(request["store"], json!(false));
    assert_eq!(request["stream"], json!(true));
    assert_eq!(request["instructions"], json!("summarize sessions"));
    assert_eq!(request["input"].as_array().unwrap().len(), 1);
}

#[test]
fn forced_tool_fallback_summary_request_forces_the_single_virtual_tool() {
    use crate::wayland::structured_summary::{VIRTUAL_TOOL_NAME, canonical_compaction_schema};

    let request = build_codex_summary_fallback_request(
        "gpt-test",
        "summarize sessions",
        &[Message::user("hi")],
        PromptCacheRetention::Short,
    );

    let tools = request["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], json!("function"));
    assert_eq!(tools[0]["name"], json!(VIRTUAL_TOOL_NAME));
    assert_eq!(tools[0]["strict"], json!(true));
    assert_eq!(tools[0]["parameters"], canonical_compaction_schema());
    assert_eq!(
        request["tool_choice"],
        json!({ "type": "function", "name": VIRTUAL_TOOL_NAME })
    );
}

#[test]
fn summary_requests_reuse_the_unchanged_oauth_headers_and_endpoint() -> Result<()> {
    // Neither summary request builder touches headers or endpoint resolution
    // -- both still flow through the same `codex_headers`/`resolve_codex_url`
    // every other Codex request uses. Assert that path directly so a future
    // change to header/endpoint construction cannot silently diverge for the
    // summary path.
    let token = AccessToken {
        bearer: "secret-token".to_string(),
        account_id: "acct_123".to_string(),
    };
    let headers = codex_headers(&token)?;
    assert_eq!(
        headers.get(AUTHORIZATION.as_str()).unwrap(),
        "Bearer secret-token"
    );
    assert_eq!(headers.get("chatgpt-account-id").unwrap(), "acct_123");
    assert_eq!(headers.get("originator").unwrap(), "iris");
    assert_eq!(headers.get(USER_AGENT.as_str()).unwrap(), "iris-agent");
    assert_eq!(
        headers.get("OpenAI-Beta").unwrap(),
        "responses=experimental"
    );
    assert_eq!(
        headers.get(CONTENT_TYPE.as_str()).unwrap(),
        "application/json"
    );
    assert_eq!(
        resolve_codex_url("https://chatgpt.com/backend-api")?.path(),
        "/backend-api/codex/responses"
    );
    Ok(())
}

#[test]
fn provider_builds_native_and_fallback_summary_requests_from_its_own_state() -> Result<()> {
    let provider = OpenAiCodexResponsesProvider::new(
        "gpt-test",
        "https://chatgpt.com/backend-api",
        None,
        "summarize sessions",
        "prompt-cache-key",
        PromptCacheRetention::Short,
        crate::mimir::retry::RetryPolicy::default(),
        crate::mimir::selection::CodexTransport::Sse,
        Some(std::time::Duration::from_secs(300)),
    )?;
    let messages = [Message::user("hi")];

    let native = provider.build_summary_request(&messages);
    assert_eq!(native["tools"], json!([]));
    assert_eq!(native["text"]["format"]["type"], json!("json_schema"));

    let fallback = provider.build_summary_fallback_request(&messages);
    assert_eq!(
        fallback["tool_choice"]["name"],
        json!(crate::wayland::structured_summary::VIRTUAL_TOOL_NAME)
    );
    Ok(())
}

#[test]
fn extracts_a_native_summary_from_a_real_response_completed_sse_shape() -> Result<()> {
    use crate::wayland::structured_summary::extract_native_summary;

    let summary_json = json!({
        "goal": "Ship #475 structured summaries",
        "state": ["renderer written"],
        "decisions": ["native first, forced-tool fallback second"],
        "key_facts": ["src/wayland/structured_summary/ holds the new modules"],
        "next_steps": ["wire provider request plumbing"],
        "preserved_identifiers": []
    })
    .to_string();
    let escaped = summary_json.replace('\\', "\\\\").replace('"', "\\\"");
    let stream = format!(
        "event: response.output_item.done\n\
         data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"message\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{escaped}\"}}]}}}}\n\n\
         event: response.completed\n\
         data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\"}}}}\n\n"
    );

    let turn = parse_response_stream(&stream)?;
    let summary = extract_native_summary(&turn).expect("schema-valid native summary");
    assert_eq!(summary.goal, "Ship #475 structured summaries");
    assert_eq!(summary.next_steps, vec!["wire provider request plumbing"]);
    Ok(())
}

#[test]
fn extracts_a_forced_tool_summary_from_a_real_function_call_sse_shape() -> Result<()> {
    use crate::wayland::structured_summary::{VIRTUAL_TOOL_NAME, extract_forced_tool_summary};

    let arguments = json!({
        "goal": "Ship #475 structured summaries",
        "state": [],
        "decisions": [],
        "key_facts": [],
        "next_steps": ["wire provider request plumbing"],
        "preserved_identifiers": []
    })
    .to_string();
    let escaped = arguments.replace('\\', "\\\\").replace('"', "\\\"");
    let stream = format!(
        "event: response.output_item.done\n\
         data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"{VIRTUAL_TOOL_NAME}\",\"arguments\":\"{escaped}\"}}}}\n\n\
         event: response.completed\n\
         data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\"}}}}\n\n"
    );

    let turn = parse_response_stream(&stream)?;
    let summary = extract_forced_tool_summary(&turn).expect("schema-valid forced-tool summary");
    assert_eq!(summary.goal, "Ship #475 structured summaries");
    Ok(())
}

#[test]
fn rejects_a_forced_tool_response_with_an_extra_tool_call() -> Result<()> {
    use crate::wayland::structured_summary::{
        SummaryExtractionError, VIRTUAL_TOOL_NAME, extract_forced_tool_summary,
    };

    let arguments = json!({
        "goal": "g", "state": [], "decisions": [], "key_facts": [], "next_steps": [],
        "preserved_identifiers": []
    })
    .to_string();
    let escaped = arguments.replace('\\', "\\\\").replace('"', "\\\"");
    let stream = format!(
        "event: response.output_item.done\n\
         data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"{VIRTUAL_TOOL_NAME}\",\"arguments\":\"{escaped}\"}}}}\n\n\
         event: response.output_item.done\n\
         data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"read\",\"arguments\":\"{{}}\"}}}}\n\n\
         event: response.completed\n\
         data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\"}}}}\n\n"
    );

    let turn = parse_response_stream(&stream)?;
    let error = extract_forced_tool_summary(&turn).unwrap_err();
    assert_eq!(
        error,
        SummaryExtractionError::UnexpectedToolCalls(vec!["read".to_string()])
    );
    Ok(())
}
