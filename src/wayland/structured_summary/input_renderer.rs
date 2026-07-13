//! Deterministic parent-owned input renderer (issue #475, ADR-0061): turns a
//! planned compaction range into compact `F/U/A/R/TC/TR` line-oriented text
//! for the model input side. Never renders verbose JSON session snapshots.

use crate::nexus::{Message, Role};
use serde_json::Value;

/// Per-line char cap for `U`/`A`/`R` bodies: generous enough to keep a
/// needle-bearing sentence intact, bounded so one oversized message cannot
/// blow up the rendered input's token cost.
const MAX_LINE_CHARS: usize = 400;
/// Per-field char cap for a compacted tool-call argument value.
const MAX_FIELD_CHARS: usize = 200;
/// Per-line char cap for a compacted tool-result preview.
const MAX_PREVIEW_CHARS: usize = 200;
/// Char cap for an unknown tool's raw JSON args/result preview.
const MAX_UNKNOWN_JSON_CHARS: usize = 300;

/// The planned compaction range this renderer turns into compact text: the
/// covered [`Message`] slice (the real "planner's covered range" -- Wayland
/// has no separate `CompactionSnapshot` type; `CompactionPlan` addresses a
/// range of the same `messages` slice compaction already operates on) plus
/// the parent-derived facts that ride alongside it: durable entry ids,
/// deterministic carry paths (ADR-0044), and the covered range's token
/// estimate. All parent-derived, never provider-supplied.
pub(crate) struct CompactInputRange<'a> {
    pub(crate) from_id: &'a str,
    pub(crate) to_id: &'a str,
    pub(crate) covered: &'a [Message],
    pub(crate) carry_paths: &'a [String],
    pub(crate) original_tokens: u64,
}

/// Render `range` into compact `F/U/A/R/TC/TR` line-oriented text.
///
/// Renderer rules (issue #475):
/// - `F` lines carry parent-derived facts only (range/count/token estimate,
///   carried paths): never provider- or model-supplied.
/// - `U`/`A` lines are the visible user/assistant text, whitespace-collapsed
///   and length-capped.
/// - `R` lines carry non-redacted `assistant_reasoning` summaries verbatim
///   (capped); a redacted reasoning message renders the literal
///   `R [redacted]` and never attempts to reconstruct hidden text.
/// - `TC`/`TR` lines compact known tool args/results into short fields and
///   deterministically cap unknown JSON shapes.
/// - `Role::Developer` messages (system-injected content such as skills
///   instructions; see `wayland::last_skills_instructions`) are stripped
///   entirely: they are persistence/context-assembly plumbing, not
///   conversation content, and are never one of the six required line kinds.
/// - Provider envelopes, encrypted continuity blobs, raw request/response
///   bodies, and auth material never enter this function: it only ever reads
///   `Message::content`/`role`/`tool_name`/`redacted`, never `continuity`,
///   `origin`, `provider_turn_id`, or `provider_blocks`.
pub(crate) fn render_compact_input(range: &CompactInputRange<'_>) -> String {
    let mut lines = Vec::with_capacity(range.covered.len() + 1 + range.carry_paths.len());
    lines.push(format!(
        "F range={}..{} count={} tokens\u{2248}{}",
        range.from_id,
        range.to_id,
        range.covered.len(),
        range.original_tokens
    ));
    for path in range.carry_paths {
        lines.push(format!("F carry_path={path}"));
    }
    for message in range.covered {
        if let Some(line) = render_message_line(message) {
            lines.push(line);
        }
    }
    lines.join("\n")
}

fn render_message_line(message: &Message) -> Option<String> {
    match message.role {
        // System-injected context assembly, not conversation content or a
        // required line kind; stripped (see the rule list above).
        Role::Developer => None,
        Role::User => Some(format!(
            "U {}",
            compact_oneline(&message.content, MAX_LINE_CHARS)
        )),
        Role::Assistant => {
            let text = message.content.trim();
            if text.is_empty() {
                None
            } else {
                Some(format!("A {}", compact_oneline(text, MAX_LINE_CHARS)))
            }
        }
        Role::AssistantReasoning => Some(if message.redacted {
            // Never reconstruct or leak hidden/redacted reasoning text, even
            // if a persisted `content` value were somehow non-empty.
            "R [redacted]".to_string()
        } else {
            format!("R {}", compact_oneline(&message.content, MAX_LINE_CHARS))
        }),
        Role::AssistantToolCall => {
            let name = message.tool_name.as_deref().unwrap_or("unknown_tool");
            Some(render_tool_call_line(name, &message.content))
        }
        Role::Tool => {
            let name = message.tool_name.as_deref().unwrap_or("unknown_tool");
            Some(render_tool_result_line(name, &message.content))
        }
    }
}

/// Collapse whitespace (including newlines) to single spaces and cap length,
/// so one line of rendered text always maps to one physical output line.
fn compact_oneline(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, max_chars)
}

/// Char-boundary-safe truncation with a trailing ellipsis marker, mirroring
/// `wayland`'s own `truncate_chars` but kept local so this renderer stays a
/// self-contained pure module.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let kept: String = text.chars().take(max).collect();
        format!("{kept}...")
    }
}

/// Quote a compacted field value if it contains whitespace, so `TC`/`TR`
/// fields stay unambiguous key=value pairs.
fn quote_if_needed(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

fn field(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    let value = map.get(key)?;
    let rendered = match value {
        Value::String(s) => compact_oneline(s, MAX_FIELD_CHARS),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    Some(format!("{key}={}", quote_if_needed(&rendered)))
}

/// Render one `AssistantToolCall`'s args into a `TC <tool> <fields...>` line.
/// Known tools compact their high-value fields (paths, commands, patterns
/// preserved exactly, up to the per-field cap); an unknown tool or malformed
/// argument JSON falls back to a deterministically capped raw preview.
fn render_tool_call_line(tool_name: &str, args_json: &str) -> String {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(args_json) else {
        return format!(
            "TC {tool_name} args={}",
            quote_if_needed(&compact_oneline(args_json, MAX_UNKNOWN_JSON_CHARS))
        );
    };
    let fields = match tool_name {
        "read" => compact_fields(&map, &["path", "offset", "limit"]),
        "write" => {
            let mut fields = compact_fields(&map, &["path"]);
            if let Some(content) = map.get("content").and_then(Value::as_str) {
                fields.push(format!("bytes={}", content.len()));
            }
            fields
        }
        "edit" => {
            let mut fields = compact_fields(&map, &["file_path"]);
            if let Some(old) = map.get("old_string").and_then(Value::as_str) {
                fields.push(format!("old_len={}", old.chars().count()));
            }
            if let Some(new) = map.get("new_string").and_then(Value::as_str) {
                fields.push(format!("new_len={}", new.chars().count()));
            }
            fields
        }
        "ls" => compact_fields(&map, &["path", "recursive", "depth", "long"]),
        "grep" => compact_fields(&map, &["pattern", "path", "glob"]),
        "bash" => compact_fields(&map, &["command", "session"]),
        _ => {
            return format!(
                "TC {tool_name} args={}",
                quote_if_needed(&compact_oneline(
                    &Value::Object(map).to_string(),
                    MAX_UNKNOWN_JSON_CHARS
                ))
            );
        }
    };
    if fields.is_empty() {
        format!("TC {tool_name}")
    } else {
        format!("TC {tool_name} {}", fields.join(" "))
    }
}

fn compact_fields(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Vec<String> {
    keys.iter().filter_map(|key| field(map, key)).collect()
}

/// Render one `Tool` result's content into a `TR <status> ...` line. Tool
/// results are the ADR-0021 wire envelope (`{"ok": bool, "content"|"error":
/// ..., "metadata": {...}}`); malformed or legacy content that does not match
/// the envelope shape falls back to a deterministically capped raw preview.
fn render_tool_result_line(_tool_name: &str, content: &str) -> String {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(content) else {
        return format!(
            "TR unknown preview=\"{}\"",
            compact_oneline(content, MAX_UNKNOWN_JSON_CHARS)
        );
    };
    let Some(ok) = map.get("ok").and_then(Value::as_bool) else {
        return format!(
            "TR unknown preview=\"{}\"",
            compact_oneline(&Value::Object(map).to_string(), MAX_UNKNOWN_JSON_CHARS)
        );
    };
    if ok {
        let preview = map
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let target = map
            .get("metadata")
            .and_then(|metadata| metadata.get("target"))
            .and_then(Value::as_str);
        match target {
            Some(target) => format!(
                "TR ok target={} preview=\"{}\"",
                quote_if_needed(target),
                compact_oneline(preview, MAX_PREVIEW_CHARS)
            ),
            None => format!(
                "TR ok preview=\"{}\"",
                compact_oneline(preview, MAX_PREVIEW_CHARS)
            ),
        }
    } else {
        let error = map.get("error").and_then(Value::as_str).unwrap_or_default();
        format!(
            "TR error preview=\"{}\"",
            compact_oneline(error, MAX_PREVIEW_CHARS)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::{ModelOrigin, ToolCall};
    use serde_json::json;

    fn origin() -> ModelOrigin {
        ModelOrigin::new("anthropic", "messages", "claude-haiku-4-5")
    }

    fn tool_call(name: &str, args: Value) -> Message {
        Message::assistant_tool_call(&ToolCall {
            id: "call_1".to_string(),
            name: name.to_string(),
            arguments: args,
            thought_signature: None,
        })
    }

    #[test]
    fn renders_the_six_required_line_labels() {
        let messages = vec![
            Message::user("implement issue #475 typed compaction summaries"),
            Message::assistant_reasoning(
                "native structured output first, forced-tool fallback second",
                "continuity-token",
                false,
                origin(),
            ),
            Message::assistant("Agreed: native first, forced-tool fallback second."),
            tool_call(
                "read",
                json!({ "path": "src/mimir/providers/anthropic_messages.rs" }),
            ),
            Message::tool_result(
                "call_1",
                "read",
                &json!({
                    "ok": true,
                    "content": "output_config.format json_schema path",
                    "metadata": { "target": "src/mimir/providers/anthropic_messages.rs" }
                })
                .to_string(),
            ),
        ];
        let range = CompactInputRange {
            from_id: "msg_1",
            to_id: "msg_5",
            covered: &messages,
            carry_paths: &["src/telemetry/sink.rs".to_string()],
            original_tokens: 42_000,
        };
        let rendered = render_compact_input(&range);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines[0].starts_with("F range=msg_1..msg_5 count=5 tokens\u{2248}42000"));
        assert_eq!(lines[1], "F carry_path=src/telemetry/sink.rs");
        assert!(lines[2].starts_with("U implement issue #475"));
        assert!(lines[3].starts_with("R native structured output first"));
        assert!(lines[4].starts_with("A Agreed: native first"));
        assert!(lines[5].starts_with("TC read path=src/mimir/providers/anthropic_messages.rs"));
        assert!(lines[6].starts_with("TR ok target=src/mimir/providers/anthropic_messages.rs"));
    }

    #[test]
    fn redacted_reasoning_never_leaks_and_is_never_reconstructed() {
        let redacted =
            Message::assistant_reasoning("SHOULD NEVER APPEAR", "continuity", true, origin());
        let range = CompactInputRange {
            from_id: "a",
            to_id: "b",
            covered: std::slice::from_ref(&redacted),
            carry_paths: &[],
            original_tokens: 10,
        };
        let rendered = render_compact_input(&range);
        assert_eq!(rendered.lines().nth(1).unwrap(), "R [redacted]");
        assert!(!rendered.contains("SHOULD NEVER APPEAR"));
    }

    #[test]
    fn non_redacted_reasoning_is_included_as_r_line() {
        let reasoning = Message::assistant_reasoning(
            "Determined durable append remains parent-owned",
            "continuity",
            false,
            origin(),
        );
        let range = CompactInputRange {
            from_id: "a",
            to_id: "b",
            covered: std::slice::from_ref(&reasoning),
            carry_paths: &[],
            original_tokens: 10,
        };
        let rendered = render_compact_input(&range);
        assert!(rendered.contains("R Determined durable append remains parent-owned"));
    }

    #[test]
    fn strips_provider_envelope_auth_and_continuity_metadata() {
        let mut reasoning = Message::assistant_reasoning(
            "decision text",
            "opaque-continuity-token-should-not-leak",
            false,
            ModelOrigin::new("openai", "codex_responses", "gpt-5.4-mini"),
        );
        reasoning.provider_turn_id = Some("turn_should_not_leak".to_string());
        reasoning.provider_blocks = vec![json!({"raw": "encrypted continuity blob"})];
        let range = CompactInputRange {
            from_id: "a",
            to_id: "b",
            covered: std::slice::from_ref(&reasoning),
            carry_paths: &[],
            original_tokens: 10,
        };
        let rendered = render_compact_input(&range);
        assert!(rendered.contains("R decision text"));
        assert!(!rendered.contains("opaque-continuity-token-should-not-leak"));
        assert!(!rendered.contains("turn_should_not_leak"));
        assert!(!rendered.contains("encrypted continuity blob"));
        assert!(!rendered.contains("openai"));
        assert!(!rendered.contains("gpt-5.4-mini"));
    }

    #[test]
    fn developer_messages_are_stripped() {
        let developer = Message::developer("<skills_instructions>...</skills_instructions>");
        let range = CompactInputRange {
            from_id: "a",
            to_id: "b",
            covered: std::slice::from_ref(&developer),
            carry_paths: &[],
            original_tokens: 10,
        };
        let rendered = render_compact_input(&range);
        // Only the mandatory `F` header line remains.
        assert_eq!(rendered.lines().count(), 1);
    }

    #[test]
    fn preserves_high_value_needles() {
        let messages = vec![
            Message::user(
                "Keep the token DEPLOY-KEY-AB12CD34 I asked to preserve; fix issue #475 in \
                 src/wayland/compaction.rs",
            ),
            tool_call(
                "bash",
                json!({ "command": "cargo test --locked compaction" }),
            ),
            Message::tool_result(
                "call_1",
                "bash",
                &json!({
                    "ok": true,
                    "content": "test result: ok. 42 passed; 0 failed; 0 ignored",
                })
                .to_string(),
            ),
            Message::tool_result(
                "call_2",
                "bash",
                &json!({ "ok": false, "error": "error[E0433]: failed to resolve" }).to_string(),
            ),
        ];
        let range = CompactInputRange {
            from_id: "a",
            to_id: "b",
            covered: &messages,
            carry_paths: &[],
            original_tokens: 10,
        };
        let rendered = render_compact_input(&range);
        assert!(rendered.contains("DEPLOY-KEY-AB12CD34"));
        assert!(rendered.contains("#475"));
        assert!(rendered.contains("src/wayland/compaction.rs"));
        assert!(rendered.contains("cargo test --locked compaction"));
        assert!(rendered.contains("42 passed; 0 failed; 0 ignored"));
        assert!(rendered.contains("error[E0433]: failed to resolve"));
    }

    #[test]
    fn unknown_tool_args_and_results_get_a_deterministic_capped_preview() {
        let messages = vec![
            tool_call("web_search", json!({ "query": "x".repeat(500) })),
            Message::tool_result("call_1", "web_search", &"y".repeat(500)),
        ];
        let range = CompactInputRange {
            from_id: "a",
            to_id: "b",
            covered: &messages,
            carry_paths: &[],
            original_tokens: 10,
        };
        let rendered = render_compact_input(&range);
        for line in rendered.lines() {
            assert!(
                line.chars().count() <= MAX_UNKNOWN_JSON_CHARS + 32,
                "line not deterministically capped: {} chars",
                line.chars().count()
            );
        }
    }

    /// Fixture (issue #475 DoD): the line-oriented renderer must be smaller
    /// than a compact JSON rendering of the same snapshot and still preserve
    /// the required needle strings.
    #[test]
    fn line_rendering_is_smaller_than_compact_json_for_the_same_snapshot() {
        let messages = vec![
            Message::user(
                "Implement issue #475 typed compaction summaries; keep the token \
                 DEPLOY-KEY-AB12CD34 I asked to preserve.",
            ),
            Message::assistant_reasoning(
                "Determined durable append remains parent-owned; worker returns text only.",
                "continuity",
                false,
                origin(),
            ),
            Message::assistant(
                "Agreed to use parent revalidation by durable ids and native structured output.",
            ),
            tool_call(
                "read",
                json!({ "path": "src/mimir/providers/anthropic_messages.rs" }),
            ),
            Message::tool_result(
                "call_1",
                "read",
                &json!({
                    "ok": true,
                    "content": "output_config.format json_schema path",
                    "metadata": { "target": "src/mimir/providers/anthropic_messages.rs" }
                })
                .to_string(),
            ),
        ];
        let range = CompactInputRange {
            from_id: "msg_120",
            to_id: "msg_184",
            covered: &messages,
            carry_paths: &["src/wayland/mod.rs".to_string()],
            original_tokens: 42_000,
        };
        let line_rendered = render_compact_input(&range);

        // A compact JSON rendering of the same snapshot: every field this
        // renderer reads, JSON-encoded with no pretty-printing.
        let json_rendered = serde_json::to_string(&json!({
            "range": { "from": range.from_id, "to": range.to_id, "tokens": range.original_tokens },
            "carry_paths": range.carry_paths,
            "messages": messages
                .iter()
                .map(|m| json!({
                    "role": m.role.as_str(),
                    "tool_name": m.tool_name,
                    "redacted": m.redacted,
                    "content": m.content,
                }))
                .collect::<Vec<_>>(),
        }))
        .unwrap();

        assert!(
            line_rendered.len() < json_rendered.len(),
            "line-rendered ({} bytes) must be smaller than compact JSON ({} bytes)",
            line_rendered.len(),
            json_rendered.len()
        );
        for needle in [
            "#475",
            "DEPLOY-KEY-AB12CD34",
            "src/mimir/providers/anthropic_messages.rs",
        ] {
            assert!(line_rendered.contains(needle), "missing needle: {needle}");
            assert!(json_rendered.contains(needle), "missing needle: {needle}");
        }
    }
}
