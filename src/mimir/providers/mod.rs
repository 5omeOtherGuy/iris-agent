//! Mimir provider adapters: each translates a native wire format into the
//! provider-neutral `nexus::ChatProvider` streaming contract.
//!
//! The system prompt is assembled by the Tier-2 Wayland harness
//! ([`crate::wayland::system_prompt`]) and handed to each provider's
//! constructor as a ready string; a provider only wraps it in its own envelope
//! (e.g. Anthropic prepends the required Claude Code identity block as system
//! block 0). Providers no longer build the prompt themselves, so base/runtime/
//! project instructions have a single owner.

pub(crate) mod anthropic_messages;
pub(crate) mod antigravity;
pub(crate) mod openai_codex_responses;
pub(crate) mod openai_compatible_chat;
mod transport;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use crate::nexus::{Message, ProviderErrorKind, ProviderFailure, Tools};

const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

fn classified_http_error(status: u16, body: &str, diagnostic: String) -> anyhow::Error {
    if is_context_overflow_response(status, body) {
        anyhow::Error::new(ProviderFailure::new(
            ProviderErrorKind::ContextWindowExceeded,
            diagnostic,
        ))
    } else {
        anyhow::anyhow!(diagnostic)
    }
}

fn is_context_overflow_response(status: u16, body: &str) -> bool {
    if !matches!(status, 400 | 413 | 422) {
        return false;
    }
    let body = body.to_ascii_lowercase();
    [
        "context_length_exceeded",
        "context_window_exceeded",
        "model_context_window_exceeded",
        "maximum context length",
        "context window has been exceeded",
        "prompt is too long",
        "too many tokens",
    ]
    .iter()
    .any(|needle| body.contains(needle))
}

fn clamp_openai_prompt_cache_key(key: &str) -> Option<String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(
        trimmed
            .chars()
            .take(OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH)
            .collect(),
    )
}

/// Stable-prefix fingerprint of a provider request: the request parts that
/// should stay byte-stable across turns for server-side prompt caching to reuse
/// them. `head` hashes the system instructions + tool declarations; `messages`
/// is the per-message content hash in order. A later request can reuse the
/// prefix cached by an earlier one only when `head` is unchanged AND the earlier
/// message-hash list is an exact prefix of the new one (the normal append case).
/// Any divergence -- a shrunk/rewritten history (compaction, edit, reorder) or a
/// changed instruction/tool head -- proves the cached prefix changed and the
/// next request cannot reuse it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PrefixFingerprint {
    head: u64,
    messages: Vec<u64>,
}

impl PrefixFingerprint {
    fn new(instructions: &str, tools: &Tools, messages: &[Message]) -> Self {
        let mut head = DefaultHasher::new();
        instructions.hash(&mut head);
        for tool in tools.iter() {
            tool.name().hash(&mut head);
            tool.description().hash(&mut head);
            tool.parameters().to_string().hash(&mut head);
        }
        let messages = messages.iter().map(hash_message).collect();
        Self {
            head: head.finish(),
            messages,
        }
    }

    /// Whether `self` proves the cached prefix established by `prev` changed: a
    /// different head, a shorter history, or any earlier message that no longer
    /// matches. A pure append (prev is an exact prefix of self) is NOT a change,
    /// so a normal next turn never warns.
    fn breaks(&self, prev: &PrefixFingerprint) -> bool {
        self.head != prev.head
            || self.messages.len() < prev.messages.len()
            || self.messages[..prev.messages.len()] != prev.messages[..]
    }
}

fn hash_message(message: &Message) -> u64 {
    let mut hasher = DefaultHasher::new();
    message.role.as_str().hash(&mut hasher);
    message.content.hash(&mut hasher);
    message.tool_call_id.hash(&mut hasher);
    message.tool_name.hash(&mut hasher);
    message.continuity.hash(&mut hasher);
    message.redacted.hash(&mut hasher);
    if let Some(origin) = &message.origin {
        origin.provider.hash(&mut hasher);
        origin.api.hash(&mut hasher);
        origin.model.hash(&mut hasher);
    }
    hasher.finish()
}

/// Provider-local prompt-cache prefix diagnostics. Holds the previous request's
/// stable-prefix fingerprint so an adapter can warn ONLY when it can prove the
/// cached prefix changed between turns (compaction rewrote history, or the
/// model/tools/instructions changed). A cold cache, an expired cache entry, or
/// the first request never warns, so the warning is a proven cache break rather
/// than an inference from a zero cache-read count.
#[derive(Debug, Default)]
struct PromptCachePrefix {
    last: Option<PrefixFingerprint>,
}

impl PromptCachePrefix {
    /// Record the request about to be sent and return whether to warn. Warns
    /// only when caching is enabled, a prior request established a cacheable
    /// prefix, and the new request provably breaks it. State is tracked only
    /// while caching is enabled, so toggling caching off leaves no stale prefix.
    fn observe(&mut self, caching_enabled: bool, fingerprint: PrefixFingerprint) -> bool {
        if !caching_enabled {
            self.last = None;
            return false;
        }
        let warn = self
            .last
            .as_ref()
            .is_some_and(|prev| fingerprint.breaks(prev));
        self.last = Some(fingerprint);
        warn
    }

    /// Lock-guarded [`observe`](Self::observe) that builds the fingerprint from
    /// the request inputs. Returns true when the adapter should emit a
    /// prompt-break warning for this turn.
    fn observe_locked(
        prefix: &Mutex<Self>,
        caching_enabled: bool,
        instructions: &str,
        tools: &Tools,
        messages: &[Message],
    ) -> bool {
        let fingerprint = PrefixFingerprint::new(instructions, tools, messages);
        prefix
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .observe(caching_enabled, fingerprint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::{Message, Tools};

    fn msg(content: &str) -> Message {
        Message::user(content)
    }

    #[test]
    fn append_only_growth_is_not_a_prefix_break() {
        let tools = Tools::new(Vec::new());
        let turn1 = PrefixFingerprint::new("sys", &tools, &[msg("a")]);
        // The next turn appends the assistant reply + a new user prompt; the
        // earlier messages are unchanged, so the cached prefix still applies.
        let turn2 = PrefixFingerprint::new(
            "sys",
            &tools,
            &[msg("a"), Message::assistant("reply"), msg("b")],
        );
        assert!(!turn2.breaks(&turn1), "append must not look like a break");
    }

    #[test]
    fn rewritten_or_shrunk_history_is_a_proven_break() {
        let tools = Tools::new(Vec::new());
        let turn1 = PrefixFingerprint::new("sys", &tools, &[msg("a"), msg("b"), msg("c")]);
        // Compaction replaces the head with a summary -> earlier hashes diverge.
        let compacted = PrefixFingerprint::new("sys", &tools, &[msg("summary"), msg("c")]);
        assert!(compacted.breaks(&turn1));
        // A changed instruction head is also a break even with identical messages.
        let new_head = PrefixFingerprint::new("different", &tools, &[msg("a"), msg("b"), msg("c")]);
        assert!(new_head.breaks(&turn1));
    }

    #[test]
    fn observe_warns_only_on_proven_break_when_caching_enabled() {
        let tools = Tools::new(Vec::new());
        let mut prefix = PromptCachePrefix::default();
        // First request: nothing to compare, never warns.
        assert!(!prefix.observe(true, PrefixFingerprint::new("s", &tools, &[msg("a")])));
        // Append: still a cache hit, no warning.
        assert!(!prefix.observe(
            true,
            PrefixFingerprint::new("s", &tools, &[msg("a"), msg("b")])
        ));
        // Compaction rewrites the prefix: proven break -> warn.
        assert!(prefix.observe(true, PrefixFingerprint::new("s", &tools, &[msg("sum")])));
    }

    #[test]
    fn observe_never_warns_when_caching_disabled() {
        let tools = Tools::new(Vec::new());
        let mut prefix = PromptCachePrefix::default();
        assert!(!prefix.observe(false, PrefixFingerprint::new("s", &tools, &[msg("a")])));
        // Even a clear prefix break does not warn while caching is off, and no
        // stale fingerprint is retained.
        assert!(!prefix.observe(false, PrefixFingerprint::new("s", &tools, &[msg("x")])));
        assert!(prefix.last.is_none());
    }

    #[test]
    fn role_aware_hash_distinguishes_same_text_different_role() {
        assert_ne!(
            hash_message(&Message::user("x")),
            hash_message(&Message::assistant("x"))
        );
    }

    #[test]
    fn overflow_classifier_covers_adapter_error_shapes_without_broad_matches() {
        for (adapter, body) in [
            (
                "anthropic",
                r#"{"error":{"type":"invalid_request_error","message":"prompt is too long"}}"#,
            ),
            ("codex", r#"{"error":{"code":"context_length_exceeded"}}"#),
            (
                "openai-compatible",
                r#"{"error":{"message":"maximum context length is 8192 tokens"}}"#,
            ),
            (
                "antigravity",
                r#"{"error":{"message":"too many tokens in request"}}"#,
            ),
        ] {
            assert!(is_context_overflow_response(400, body), "{adapter}");
        }
        assert!(!is_context_overflow_response(
            429,
            r#"{"error":{"message":"too many tokens"}}"#
        ));
        assert!(!is_context_overflow_response(
            400,
            r#"{"error":{"message":"context management setting is invalid"}}"#
        ));
        let error = classified_http_error(
            400,
            r#"{"error":{"code":"context_length_exceeded"}}"#,
            "safe diagnostic".to_string(),
        );
        assert_eq!(
            crate::nexus::provider_error_kind(&error),
            Some(ProviderErrorKind::ContextWindowExceeded)
        );
        assert_eq!(error.to_string(), "safe diagnostic");
        let wrapped = error.context("transport wrapper");
        assert_eq!(
            crate::nexus::provider_error_kind(&wrapped),
            Some(ProviderErrorKind::ContextWindowExceeded)
        );
    }
}
