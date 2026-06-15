use super::*;
use std::cell::Cell;

fn fake_token() -> AccessToken {
    AccessToken {
        bearer: "bearer-value".to_string(),
        account_id: "acct".to_string(),
    }
}

#[derive(Default)]
struct RecordingSink {
    deltas: Vec<String>,
}

impl TurnSink for RecordingSink {
    fn on_text_delta(&mut self, delta: &str) {
        self.deltas.push(delta.to_string());
    }
}

fn is_auth_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<AuthError>().is_some()
}

#[test]
fn retry_loop_exhausts_transient_then_returns_error() {
    let sends = Cell::new(0u32);
    let sleeps = Cell::new(0u32);
    let result = run_retry_loop(
        |_force| Ok(fake_token()),
        |_token| {
            sends.set(sends.get() + 1);
            Attempt::Retry(anyhow!("503"), None)
        },
        |_delay| sleeps.set(sleeps.get() + 1),
    );
    assert!(result.is_err());
    assert!(!is_auth_error(&result.unwrap_err()));
    // One initial attempt plus MAX retries, sleeping before each retry.
    assert_eq!(sends.get(), MAX_TRANSIENT_RETRIES + 1);
    assert_eq!(sleeps.get(), MAX_TRANSIENT_RETRIES);
}

#[test]
fn retry_loop_reauths_exactly_once_then_succeeds() {
    let forces: Cell<Vec<bool>> = Cell::new(Vec::new());
    let sends = Cell::new(0u32);
    let result = run_retry_loop(
        |force| {
            let mut seen = forces.take();
            seen.push(force);
            forces.set(seen);
            Ok(fake_token())
        },
        |_token| {
            sends.set(sends.get() + 1);
            if sends.get() == 1 {
                Attempt::Reauth(anyhow!("401"))
            } else {
                Attempt::Done(AssistantTurn::text("ok"))
            }
        },
        |_delay| {},
    );
    assert!(result.is_ok());
    assert_eq!(forces.take(), vec![false, true]);
    assert_eq!(sends.get(), 2);
}

#[test]
fn retry_loop_second_auth_rejection_returns_auth_error() {
    let result = run_retry_loop(
        |_force| Ok(fake_token()),
        |_token| Attempt::Reauth(anyhow!("401")),
        |_delay| {},
    );
    let error = result.unwrap_err();
    assert!(is_auth_error(&error));
}

#[test]
fn retry_loop_force_refresh_failure_returns_auth_error() {
    let sends = Cell::new(0u32);
    let result = run_retry_loop(
        |force| {
            if force {
                Err(anyhow!("refresh failed"))
            } else {
                Ok(fake_token())
            }
        },
        |_token| {
            sends.set(sends.get() + 1);
            Attempt::Reauth(anyhow!("401"))
        },
        |_delay| {},
    );
    assert!(is_auth_error(&result.unwrap_err()));
    // First attempt sent (got 401); refresh then failed before any resend.
    assert_eq!(sends.get(), 1);
}

#[test]
fn retry_loop_reauth_does_not_reset_transient_budget() {
    // Retry, Retry, Reauth, Retry, Retry: with the budget retained the
    // fifth send exhausts MAX_TRANSIENT_RETRIES (=3) and returns.
    let sends = Cell::new(0u32);
    let result = run_retry_loop(
        |_force| Ok(fake_token()),
        |_token| {
            sends.set(sends.get() + 1);
            match sends.get() {
                3 => Attempt::Reauth(anyhow!("401")),
                _ => Attempt::Retry(anyhow!("503"), None),
            }
        },
        |_delay| {},
    );
    assert!(result.is_err());
    assert!(!is_auth_error(&result.unwrap_err()));
    assert_eq!(sends.get(), 5);
}

#[test]
fn retry_loop_passes_retry_after_delay_to_sleeper() {
    let delays: Cell<Vec<Duration>> = Cell::new(Vec::new());
    let sends = Cell::new(0u32);
    let _ = run_retry_loop(
        |_force| Ok(fake_token()),
        |_token| {
            sends.set(sends.get() + 1);
            if sends.get() == 1 {
                Attempt::Retry(anyhow!("429"), Some(Duration::from_secs(2)))
            } else {
                Attempt::Done(AssistantTurn::text("ok"))
            }
        },
        |delay| {
            let mut seen = delays.take();
            seen.push(delay);
            delays.set(seen);
        },
    );
    let seen = delays.take();
    assert_eq!(seen.len(), 1);
    // Retry-After of 2s, plus up to 250ms of jitter.
    assert!(seen[0] >= Duration::from_secs(2));
    assert!(seen[0] < Duration::from_secs(2) + Duration::from_millis(250));
}

#[test]
fn classifies_http_status_into_retry_policy() {
    assert_eq!(classify_http_status(401), HttpClass::Reauth);
    assert_eq!(classify_http_status(403), HttpClass::Reauth);
    assert_eq!(classify_http_status(429), HttpClass::Retry);
    assert_eq!(classify_http_status(408), HttpClass::Retry);
    assert_eq!(classify_http_status(503), HttpClass::Retry);
    assert_eq!(classify_http_status(500), HttpClass::Retry);
    assert_eq!(classify_http_status(400), HttpClass::Fatal);
    assert_eq!(classify_http_status(404), HttpClass::Fatal);
    assert_eq!(classify_http_status(422), HttpClass::Fatal);
}

#[test]
fn backoff_delay_grows_exponentially_within_jitter_bounds() {
    let base = Duration::from_millis(500);
    let jitter = Duration::from_millis(250);
    // retry 1 -> base, retry 2 -> 2x base, retry 3 -> 4x base, each + jitter.
    for (retry, expected) in [(1u32, 500u64), (2, 1000), (3, 2000)] {
        let delay = backoff_delay(retry, None, base);
        assert!(delay >= Duration::from_millis(expected), "retry {retry}");
        assert!(
            delay < Duration::from_millis(expected) + jitter,
            "retry {retry}"
        );
    }
}

#[test]
fn backoff_delay_is_clamped_to_max() {
    let delay = backoff_delay(20, None, Duration::from_millis(500));
    assert!(delay <= MAX_BACKOFF + Duration::from_millis(250));
}

#[test]
fn backoff_delay_honors_retry_after_hint() {
    let delay = backoff_delay(1, Some(Duration::from_secs(2)), Duration::from_millis(500));
    assert!(delay >= Duration::from_secs(2));
    assert!(delay < Duration::from_secs(2) + Duration::from_millis(250));
}

#[test]
fn parse_retry_after_reads_integer_seconds() {
    let mut headers = HeaderMap::new();
    headers.insert(RETRY_AFTER, HeaderValue::from_static("7"));
    assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(7)));
}

#[test]
fn parse_retry_after_ignores_non_integer() {
    let mut headers = HeaderMap::new();
    headers.insert(
        RETRY_AFTER,
        HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
    );
    assert_eq!(parse_retry_after(&headers), None);
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
    let request = build_codex_request(
        "gpt-test",
        &[Message::user("hello"), Message::assistant("hi")],
    );
    assert_eq!(request["model"], "gpt-test");
    assert_eq!(request["stream"], true);
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
    let request = build_codex_request(
        "gpt-test",
        &[
            Message {
                role: Role::AssistantToolCall,
                content: json!({ "path": "src/main.rs" }).to_string(),
                tool_call_id: Some("call_1".to_string()),
                tool_name: Some("read".to_string()),
            },
            Message {
                role: Role::Tool,
                content: json!({ "ok": true, "content": "file text" }).to_string(),
                tool_call_id: Some("call_1".to_string()),
                tool_name: Some("read".to_string()),
            },
        ],
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

    let turn =
        parse_response_stream_reader(BufReader::with_capacity(1, stream.as_bytes()), &mut sink)?;

    assert_eq!(turn.text.as_deref(), Some("Hello"));
    assert_eq!(sink.deltas, vec!["Hel", "lo"]);
    Ok(())
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

    let err = parse_response_stream_reader(stream.as_bytes(), &mut sink).unwrap_err();

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
    let turn = parse_response_stream_reader(stream.as_bytes(), &mut sink)?;

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
