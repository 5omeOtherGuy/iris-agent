//! Antigravity provider (Google-account OAuth -> Gemini via Code Assist).
//! Mirrors `anthropic_messages.rs`: the request is built eagerly, then a
//! blocking reqwest + SSE parse runs through the shared `transport` channel +
//! one-shot reauth glue. The wire surface is Code Assist's
//! `v1internal:streamGenerateContent?alt=sse`: the Gemini request is wrapped in
//! an Antigravity envelope and the SSE candidates are assembled into an
//! `AssistantTurn`.
//!
//! ponytail: MVP wire surface only -- no generationConfig/thinking, no
//! multimodal, no transient backoff. Add them if a real need shows up.

use std::io::BufReader;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::transport::{
    Attempt, HttpClass, TurnSink, classify_http_status, for_each_sse_event, run_with_reauth,
    spawn_stream,
};
use crate::mimir::auth::antigravity::AntigravityTokenStore;
use crate::mimir::selection::ReasoningEffort;
use crate::nexus::{AssistantTurn, ChatProvider, Message, ProviderStream, Role, ToolCall, Tools};

const USER_AGENT_VALUE: &str = "antigravity/1.0.2 linux/amd64";

#[derive(Debug, Clone)]
pub(crate) struct AntigravityProvider {
    client: Client,
    model: String,
    base_url: String,
    reasoning: Option<ReasoningEffort>,
    system_prompt: String,
    tokens: AntigravityTokenStore,
}

impl AntigravityProvider {
    /// Build from the resolved model/base-url/reasoning selection (precedence is
    /// owned by `mimir::selection`). `system_prompt` is the harness-assembled
    /// instruction string; the provider forwards it as the request's
    /// `systemInstruction`.
    pub(crate) fn new(
        model: &str,
        base_url: &str,
        reasoning: Option<ReasoningEffort>,
        system_prompt: &str,
    ) -> Result<Self> {
        Ok(Self {
            // Shared process-wide client: warm pooled connections (HTTP/2 +
            // keep-alive) survive across turns and model switches, so a turn
            // does not pay a fresh TLS handshake after an idle gap.
            client: super::transport::shared_client(),
            model: model.to_string(),
            base_url: base_url.to_string(),
            reasoning,
            system_prompt: system_prompt.to_string(),
            tokens: AntigravityTokenStore::from_env()?,
        })
    }
}

impl ChatProvider for AntigravityProvider {
    fn respond_stream<'a>(
        &'a self,
        messages: &'a [Message],
        tools: &'a Tools,
        cancel: &'a CancellationToken,
    ) -> Result<ProviderStream<'a>> {
        // Build the inner Gemini request eagerly so the blocking task captures
        // only an owned `Value` and a cloned provider, never a borrow of
        // `self`/`messages`/`tools`. The envelope (which needs project id) is
        // assembled per-attempt from this inner request.
        let inner = build_inner_request(
            &self.model,
            &self.system_prompt,
            messages,
            tools,
            self.reasoning,
        );
        let wire_slot = wire_model_slot(&self.model).to_string();
        let provider = self.clone();
        let cancel = cancel.clone();
        Ok(spawn_stream(
            move |sink, cancel| {
                run_with_reauth(
                    "antigravity",
                    cancel,
                    |force| {
                        if force {
                            provider.tokens.force_refresh(&provider.client)
                        } else {
                            provider.tokens.access_token(&provider.client)
                        }
                    },
                    |token| provider.send_once(token, &inner, &wire_slot, sink, cancel),
                )
            },
            cancel,
        ))
    }
}

impl AntigravityProvider {
    fn send_once(
        &self,
        token: &crate::mimir::auth::antigravity::AntigravityToken,
        inner: &Value,
        wire_slot: &str,
        sink: &mut dyn TurnSink,
        cancel: &CancellationToken,
    ) -> Attempt {
        let envelope = wrap_request(&token.project_id, wire_slot, inner.clone());
        let headers = match antigravity_headers(&token.bearer) {
            Ok(headers) => headers,
            Err(error) => return Attempt::Fatal(error),
        };
        let url = format!("{}/v1internal:streamGenerateContent?alt=sse", self.base_url);
        let response = match self
            .client
            .post(&url)
            .headers(headers)
            .json(&envelope)
            .send()
        {
            Ok(response) => response,
            Err(error) => {
                return Attempt::Fatal(
                    anyhow::Error::new(error).context("failed to send Antigravity request"),
                );
            }
        };

        let status = response.status();
        if status.is_success() {
            let mut parser = GeminiStreamParser::default();
            if let Err(error) = for_each_sse_event(BufReader::new(response), cancel, |data| {
                sink.on_activity()?;
                parser.ingest_event(data, sink)
            }) {
                return Attempt::Fatal(error);
            }
            return match parser.finish() {
                Ok(turn) => Attempt::Done(Box::new(turn)),
                Err(error) => Attempt::Fatal(error),
            };
        }

        let body = response.text().unwrap_or_default();
        let diagnostic = match crate::telemetry::sanitize_external_body(&body) {
            Some(detail) => format!("Antigravity request failed ({status}): {detail}"),
            None => format!("Antigravity request failed ({status})"),
        };
        let error = super::classified_http_error(status.as_u16(), &body, diagnostic);
        match classify_http_status(status.as_u16()) {
            HttpClass::Reauth => Attempt::Reauth(error),
            // Antigravity uses the reauth-only loop and does not retry transient
            // failures; `classify_http_status` never returns `Retry`, so this is
            // only here for exhaustiveness over the shared `HttpClass`.
            HttpClass::Retry | HttpClass::Fatal => Attempt::Fatal(error),
        }
    }
}

fn antigravity_headers(token: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers.insert(USER_AGENT, HeaderValue::from_static(USER_AGENT_VALUE));
    Ok(headers)
}

/// Translate a stable Pi-visible model id to the Antigravity backend wire slot.
/// The backend periodically renames its slots; unknown ids pass through.
fn wire_model_slot(model: &str) -> &str {
    match model {
        "gemini-3.5-flash" => "gemini-3.5-flash-low",
        "gemini-3.1-pro" => "gemini-3.1-pro-low",
        "gemini-3-flash" => "gemini-3-flash",
        other => other,
    }
}

/// Wrap the inner Gemini request in the Antigravity Code Assist envelope.
fn wrap_request(project_id: &str, wire_slot: &str, inner: Value) -> Value {
    json!({
        "project": project_id,
        "model": wire_slot,
        "request": inner,
        "requestType": "agent",
        "userAgent": "antigravity",
        "requestId": format!("agent-{}", unique_id()),
    })
}

fn build_inner_request(
    model: &str,
    system_prompt: &str,
    messages: &[Message],
    tools: &Tools,
    reasoning: Option<ReasoningEffort>,
) -> Value {
    let mut request = json!({
        "contents": build_contents(messages),
        "sessionId": format!("pi-{}", unique_id()),
    });
    if !system_prompt.is_empty() {
        request["systemInstruction"] = json!({
            "role": "user",
            "parts": [{ "text": system_prompt }],
        });
    }
    let declarations = tool_declarations(tools);
    if !declarations.is_empty() {
        request["tools"] = json!([{ "functionDeclarations": declarations }]);
        request["toolConfig"] = json!({ "functionCallingConfig": { "mode": "AUTO" } });
    }
    // generationConfig.thinkingConfig is added only when a preference is set, so
    // the default (None) request is byte-identical to today's (no
    // generationConfig at all).
    if let Some(thinking) = antigravity_thinking(model, reasoning) {
        request["generationConfig"] = json!({ "thinkingConfig": thinking });
    }
    request
}

/// Map a normalized reasoning level to the Gemini `thinkingConfig`, or `None` to
/// omit it. Flash tiers accept `minimal|low|medium|high`; Pro tiers reject
/// `minimal` on the wire, so their semantic levels collapse to `low|high`
/// (`minimal|low -> low`, `medium|high -> high`) like gemini-pi's
/// `PRO_THINKING`. `xhigh` is not exposed for Antigravity and defensively clamps
/// to `high` if a carried value reaches the sender.
fn antigravity_thinking(model: &str, reasoning: Option<ReasoningEffort>) -> Option<Value> {
    let level = antigravity_thinking_level(model, reasoning?)?;
    Some(json!({ "includeThoughts": true, "thinkingLevel": level }))
}

fn antigravity_thinking_level(model: &str, reasoning: ReasoningEffort) -> Option<&'static str> {
    if crate::mimir::model_capabilities::is_antigravity_pro_model(model) {
        return match reasoning {
            ReasoningEffort::Off => None,
            ReasoningEffort::Minimal | ReasoningEffort::Low => Some("low"),
            ReasoningEffort::Medium | ReasoningEffort::High | ReasoningEffort::XHigh => {
                Some("high")
            }
        };
    }
    match reasoning {
        ReasoningEffort::Off => None,
        ReasoningEffort::Minimal => Some("minimal"),
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High | ReasoningEffort::XHigh => Some("high"),
    }
}

fn tool_declarations(tools: &Tools) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name(),
                "description": tool.description(),
                "parameters": sanitize_schema(tool.parameters()),
            })
        })
        .collect()
}

/// Recursively drop JSON-Schema meta keys Gemini's OpenAPI subset rejects.
///
/// ponytail: light sanitize (only the keys seen to 400 today); expand the drop
/// set if Gemini starts rejecting other schema constructs.
fn sanitize_schema(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(key, _)| {
                    !matches!(key.as_str(), "$schema" | "$defs" | "additionalProperties")
                })
                .map(|(key, val)| (key, sanitize_schema(val)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(sanitize_schema).collect()),
        other => other,
    }
}

/// Map Nexus messages onto Gemini `contents`, coalescing consecutive entries
/// that share a wire role ("user" or "model") into a single entry so the parts
/// accumulate. Tool calls are model-role `functionCall` parts; tool results are
/// user-role `functionResponse` parts.
fn build_contents(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for message in messages {
        if message.role == Role::AssistantReasoning {
            continue;
        }
        let (role, part) = match message.role {
            // Gemini has no developer role in `contents`; keep it as a user
            // context part rather than raising it into system instructions.
            Role::Developer => ("user", json!({ "text": message.content })),
            Role::User => ("user", json!({ "text": message.content })),
            Role::Assistant => ("model", json!({ "text": message.content })),
            Role::AssistantToolCall => {
                let mut function_call = json!({
                    "name": message.tool_name.as_deref().unwrap_or_default(),
                    "args": serde_json::from_str::<Value>(&message.content)
                        .unwrap_or_else(|_| json!({})),
                });
                insert_optional_id(&mut function_call, message.tool_call_id.as_deref());
                let mut part = json!({ "functionCall": function_call });
                // Echo the captured `thoughtSignature` (stored opaquely as the
                // message's continuity) as a sibling of `functionCall` in the
                // same part, exactly where Gemini returned it.
                if let Some(signature) = message
                    .continuity
                    .as_deref()
                    .map(str::trim)
                    .filter(|signature| !signature.is_empty())
                    && let Some(map) = part.as_object_mut()
                {
                    map.insert("thoughtSignature".to_string(), json!(signature));
                }
                ("model", part)
            }
            Role::Tool => {
                let mut function_response = json!({
                    "name": message.tool_name.as_deref().unwrap_or_default(),
                    "response": { "output": message.content },
                });
                insert_optional_id(&mut function_response, message.tool_call_id.as_deref());
                ("user", json!({ "functionResponse": function_response }))
            }
            Role::AssistantReasoning => unreachable!("reasoning rows are skipped above"),
        };
        push_part(&mut out, role, part);
    }
    out
}

fn insert_optional_id(object: &mut Value, id: Option<&str>) {
    if let Some(id) = id.map(str::trim).filter(|id| !id.is_empty())
        && let Some(map) = object.as_object_mut()
    {
        map.insert("id".to_string(), json!(id));
    }
}

fn push_part(out: &mut Vec<Value>, role: &str, part: Value) {
    if let Some(last) = out.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(parts) = last.get_mut("parts").and_then(Value::as_array_mut)
    {
        parts.push(part);
        return;
    }
    out.push(json!({ "role": role, "parts": [part] }));
}

/// Process-unique id (nanos since epoch + a monotonic counter) for request and
/// session ids. No uuid dependency; uniqueness within a process is sufficient.
fn unique_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{count:x}")
}

/// Incremental SSE assembler for Gemini `streamGenerateContent` events.
#[derive(Default)]
struct GeminiStreamParser {
    text: String,
    tool_calls: Vec<ToolCall>,
    generated_calls: u64,
}

impl GeminiStreamParser {
    fn ingest_event(&mut self, data: &str, sink: &mut dyn TurnSink) -> Result<()> {
        if data == "[DONE]" {
            return Ok(());
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|e| anyhow!("failed to parse Antigravity SSE: {e}"))?;
        // Each event may be a single object, an array, or wrapped in `response`.
        match value {
            Value::Array(items) => {
                for item in items {
                    self.ingest_payload(&unwrap_response(item), sink)?;
                }
            }
            other => self.ingest_payload(&unwrap_response(other), sink)?,
        }
        Ok(())
    }

    fn ingest_payload(&mut self, payload: &Value, sink: &mut dyn TurnSink) -> Result<()> {
        if let Some(message) = payload
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
        {
            return Err(anyhow!("{message}"));
        }
        if let Some(reason) = payload
            .get("promptFeedback")
            .and_then(|feedback| feedback.get("blockReason"))
            .and_then(Value::as_str)
        {
            return Err(anyhow!("Antigravity response blocked: {reason}"));
        }

        let Some(parts) = payload
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
            .and_then(|candidate| candidate.get("content"))
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array)
        else {
            return Ok(());
        };

        for part in parts {
            if part.get("thought").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if let Some(call) = part.get("functionCall") {
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        self.generated_calls += 1;
                        format!("call_{}", self.generated_calls)
                    });
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments = call.get("args").cloned().unwrap_or_else(|| json!({}));
                // Gemini 3 attaches `thoughtSignature` as a sibling of
                // `functionCall` on the part (only on the first of parallel
                // calls). It must be echoed back verbatim in history or the next
                // request is rejected with a 400, so capture it on the call.
                let thought_signature = part
                    .get("thoughtSignature")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments,
                    thought_signature,
                });
            } else if let Some(text) = part.get("text").and_then(Value::as_str) {
                self.text.push_str(text);
                sink.on_text_delta(text)?;
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<AssistantTurn> {
        if self.text.is_empty() && self.tool_calls.is_empty() {
            return Err(anyhow!(
                "Antigravity response did not include assistant text or tool calls"
            ));
        }
        Ok(AssistantTurn {
            text: (!self.text.is_empty()).then_some(self.text),
            reasoning: Vec::new(),
            tool_calls: self.tool_calls,
            response_id: None,
            usage: None,
            completion_reason: None,
        })
    }
}

/// Unwrap a `{ "response": INNER }` envelope, returning INNER (or the value
/// unchanged when there is no wrapper).
fn unwrap_response(value: Value) -> Value {
    match value {
        Value::Object(mut map) => map.remove("response").unwrap_or(Value::Object(map)),
        other => other,
    }
}

#[cfg(test)]
fn parse_antigravity_sse(body: &str) -> Result<AssistantTurn> {
    struct NoopSink;
    impl TurnSink for NoopSink {
        fn on_text_delta(&mut self, _delta: &str) -> Result<()> {
            Ok(())
        }
    }
    let mut parser = GeminiStreamParser::default();
    let mut sink = NoopSink;
    for_each_sse_event(body.as_bytes(), &CancellationToken::new(), |data| {
        parser.ingest_event(data, &mut sink)
    })?;
    parser.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::{Message, Tools};

    #[test]
    fn text_parts_assemble_into_turn() {
        let body = "\
data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello \"}]}}]}

data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"world\"}]}}]}

";
        let turn = parse_antigravity_sse(body).expect("stream should parse");
        assert_eq!(turn.text.as_deref(), Some("Hello world"));
        assert!(turn.tool_calls.is_empty());
    }

    #[test]
    fn function_call_parses_args_and_generates_id_when_missing() {
        let body = "\
data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"read\",\"args\":{\"path\":\"a.rs\"}}}]}}]}

";
        let turn = parse_antigravity_sse(body).expect("stream should parse");
        assert_eq!(turn.tool_calls.len(), 1);
        let call = &turn.tool_calls[0];
        assert_eq!(call.name, "read");
        assert_eq!(
            call.id, "call_1",
            "generated id when functionCall.id absent"
        );
        assert_eq!(call.arguments, json!({ "path": "a.rs" }));
    }

    #[test]
    fn function_call_captures_thought_signature_when_present() {
        let body = "\
data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"ls\",\"args\":{}},\"thoughtSignature\":\"sig-abc\"}]}}]}

";
        let turn = parse_antigravity_sse(body).expect("stream should parse");
        let call = &turn.tool_calls[0];
        assert_eq!(call.thought_signature.as_deref(), Some("sig-abc"));
    }

    #[test]
    fn tool_call_continuity_round_trips_thought_signature_into_request() {
        // A captured signature rides on the tool call's `thought_signature` and
        // is stored as the assistant-tool-call message's continuity.
        let call = ToolCall {
            id: "fc_1".to_string(),
            name: "ls".to_string(),
            arguments: json!({ "path": "." }),
            thought_signature: Some("sig-xyz".to_string()),
        };
        let message = Message::assistant_tool_call(&call);
        assert_eq!(message.continuity.as_deref(), Some("sig-xyz"));

        let contents = build_contents(std::slice::from_ref(&message));
        let part = &contents[0]["parts"][0];
        // Echoed as a sibling of `functionCall`, in the exact part, per the
        // Gemini thought-signature contract.
        assert_eq!(part["functionCall"]["name"], json!("ls"));
        assert_eq!(part["thoughtSignature"], json!("sig-xyz"));
    }

    #[test]
    fn tool_call_without_signature_omits_thought_signature_field() {
        let call = ToolCall {
            id: "fc_1".to_string(),
            name: "ls".to_string(),
            arguments: json!({}),
            thought_signature: None,
        };
        let contents = build_contents(&[Message::assistant_tool_call(&call)]);
        let part = &contents[0]["parts"][0];
        assert!(
            part.get("thoughtSignature").is_none(),
            "no signature -> no thoughtSignature key"
        );
    }

    #[test]
    fn function_call_without_args_defaults_to_empty_object() {
        let body = "\
data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"list\",\"id\":\"fc_7\"}}]}}]}

";
        let turn = parse_antigravity_sse(body).expect("stream should parse");
        let call = &turn.tool_calls[0];
        assert_eq!(call.id, "fc_7", "provided functionCall.id is used");
        assert_eq!(call.arguments, json!({}));
    }

    #[test]
    fn response_wrapper_is_unwrapped() {
        let body = "\
data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}}

";
        let turn = parse_antigravity_sse(body).expect("stream should parse");
        assert_eq!(turn.text.as_deref(), Some("hi"));
    }

    #[test]
    fn thought_parts_are_skipped() {
        let body = "\
data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"secret\",\"thought\":true},{\"text\":\"shown\"}]}}]}

";
        let turn = parse_antigravity_sse(body).expect("stream should parse");
        assert_eq!(turn.text.as_deref(), Some("shown"));
    }

    #[test]
    fn top_level_error_is_error() {
        let body = "\
data: {\"error\":{\"message\":\"quota exceeded\"}}

";
        let error = parse_antigravity_sse(body).unwrap_err().to_string();
        assert!(error.contains("quota exceeded"), "got: {error}");
    }

    #[test]
    fn request_envelope_has_agent_metadata_and_maps_tool_result() {
        let messages = vec![
            Message::user("hi"),
            Message {
                role: Role::Tool,
                content: "result body".to_string(),
                tool_call_id: Some("fc_1".to_string()),
                tool_name: Some("read".to_string()),
                continuity: None,
                provider_turn_id: None,
                redacted: false,
                origin: None,
                provider_blocks: Vec::new(),
            },
        ];
        let inner = build_inner_request(
            "gemini-3.5-flash",
            "IRIS PROMPT",
            &messages,
            &Tools::new(Vec::new()),
            None,
        );
        let envelope = wrap_request("proj-1", wire_model_slot("gemini-3.5-flash"), inner);

        assert_eq!(envelope["requestType"], json!("agent"));
        assert_eq!(envelope["userAgent"], json!("antigravity"));
        assert_eq!(envelope["project"], json!("proj-1"));
        assert_eq!(
            envelope["model"],
            json!("gemini-3.5-flash-low"),
            "wire slot mapped"
        );

        let request = &envelope["request"];
        assert_eq!(
            request["systemInstruction"]["parts"][0]["text"],
            json!("IRIS PROMPT")
        );
        assert!(request.get("tools").is_none(), "empty tools omitted");
        assert!(
            request.get("toolConfig").is_none(),
            "no toolConfig without tools"
        );

        // The user "hi" text and the tool result share the "user" wire role,
        // so they coalesce into one content entry (text part, then the
        // functionResponse part).
        let contents = request["contents"].as_array().expect("contents array");
        let user_entry = contents.last().expect("user content");
        assert_eq!(user_entry["role"], json!("user"));
        let parts = user_entry["parts"].as_array().expect("parts array");
        let fr = &parts.last().expect("functionResponse part")["functionResponse"];
        assert_eq!(fr["name"], json!("read"));
        assert_eq!(fr["id"], json!("fc_1"));
        assert_eq!(fr["response"]["output"], json!("result body"));
    }

    #[test]
    fn reasoning_none_omits_generation_config_some_adds_thinking() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());

        // None: no generationConfig at all (byte-identical to today's wire).
        let none = build_inner_request("gemini-3.5-flash", "P", &messages, &tools, None);
        assert!(
            none.get("generationConfig").is_none(),
            "None omits generationConfig"
        );

        // Medium: thinkingConfig with includeThoughts + thinkingLevel.
        let medium = build_inner_request(
            "gemini-3.5-flash",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::Medium),
        );
        assert_eq!(
            medium["generationConfig"]["thinkingConfig"],
            json!({ "includeThoughts": true, "thinkingLevel": "medium" })
        );

        // xhigh clamps to high on the Flash tier.
        let xhigh = build_inner_request(
            "gemini-3.5-flash",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::XHigh),
        );
        assert_eq!(
            xhigh["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            json!("high")
        );

        // Off omits generationConfig entirely.
        let off = build_inner_request(
            "gemini-3.5-flash",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::Off),
        );
        assert!(
            off.get("generationConfig").is_none(),
            "Off omits generationConfig"
        );
    }

    #[test]
    fn pro_reasoning_maps_to_only_low_or_high_wire_levels() {
        let messages = [Message::user("hi")];
        let tools = Tools::new(Vec::new());

        let minimal = build_inner_request(
            "gemini-3.1-pro",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::Minimal),
        );
        assert_eq!(
            minimal["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            json!("low"),
            "Pro rejects wire minimal; semantic minimal maps to low"
        );

        let medium = build_inner_request(
            "gemini-3.1-pro",
            "P",
            &messages,
            &tools,
            Some(ReasoningEffort::Medium),
        );
        assert_eq!(
            medium["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            json!("high"),
            "Pro exposes only low/high wire levels"
        );
    }

    #[test]
    fn consecutive_same_role_messages_coalesce() {
        let messages = vec![
            Message::user("a"),
            Message::user("b"),
            Message::assistant("c"),
            Message {
                role: Role::AssistantToolCall,
                content: "{\"path\":\"x\"}".to_string(),
                tool_call_id: Some("fc_1".to_string()),
                tool_name: Some("read".to_string()),
                continuity: None,
                provider_turn_id: None,
                redacted: false,
                origin: None,
                provider_blocks: Vec::new(),
            },
        ];
        let contents = build_contents(&messages);
        assert_eq!(
            contents.len(),
            2,
            "user*2 coalesce, model+toolcall coalesce"
        );
        assert_eq!(contents[0]["role"], json!("user"));
        assert_eq!(contents[0]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(contents[1]["role"], json!("model"));
        let model_parts = contents[1]["parts"].as_array().unwrap();
        assert_eq!(
            model_parts.len(),
            2,
            "text + functionCall in one model entry"
        );
        assert_eq!(model_parts[1]["functionCall"]["id"], json!("fc_1"));
        assert_eq!(
            model_parts[1]["functionCall"]["args"],
            json!({ "path": "x" })
        );
    }

    #[test]
    fn tools_present_adds_declarations_and_tool_config() {
        struct FakeTool;
        impl crate::nexus::Tool for FakeTool {
            fn name(&self) -> &str {
                "read"
            }
            fn description(&self) -> &str {
                "read a file"
            }
            fn parameters(&self) -> Value {
                json!({
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "type": "object",
                    "additionalProperties": false,
                    "properties": { "path": { "type": "string" } }
                })
            }
            fn execute<'a>(
                &'a self,
                _args: &'a Value,
                _env: &'a crate::nexus::ToolEnv<'_>,
                _cancel: CancellationToken,
            ) -> crate::nexus::ToolFuture<'a> {
                unimplemented!()
            }
        }
        let tools = Tools::new(vec![Box::new(FakeTool)]);
        let inner =
            build_inner_request("gemini-3.5-flash", "", &[Message::user("hi")], &tools, None);
        let decl = &inner["tools"][0]["functionDeclarations"][0];
        assert_eq!(decl["name"], json!("read"));
        let params = &decl["parameters"];
        assert!(params.get("$schema").is_none(), "$schema sanitized out");
        assert!(
            params.get("additionalProperties").is_none(),
            "additionalProperties sanitized out"
        );
        assert_eq!(params["properties"]["path"]["type"], json!("string"));
        assert_eq!(
            inner["toolConfig"]["functionCallingConfig"]["mode"],
            json!("AUTO")
        );
        assert!(
            inner.get("systemInstruction").is_none(),
            "empty system prompt omitted"
        );
    }

    #[test]
    fn developer_context_maps_to_a_user_part() {
        let contents =
            build_contents(&[Message::developer("skill catalog"), Message::user("task")]);

        assert_eq!(contents.len(), 1, "consecutive user parts coalesce");
        assert_eq!(contents[0]["role"], json!("user"));
        assert_eq!(contents[0]["parts"][0]["text"], json!("skill catalog"));
        assert_eq!(contents[0]["parts"][1]["text"], json!("task"));
    }
}
