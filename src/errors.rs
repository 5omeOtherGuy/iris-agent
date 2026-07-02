//! Provider-neutral typed errors carried across runtime boundaries.
//!
//! Leaf functions use `anyhow` with `.context()`; at the boundaries that need
//! distinct user-facing handling or process exit codes we attach one of these
//! typed errors so callers can classify by `downcast_ref` without parsing
//! error strings. These types are intentionally free of any provider, auth, or
//! transport payload detail (see AGENTS.md ownership boundary).

/// Authentication could not be established or was rejected by the provider.
///
/// Surfaced to the user with a re-login hint and mapped to a dedicated exit
/// code. The wrapped message already includes the underlying cause chain. The
/// optional `provider` records which provider failed so the presentation layer
/// (CLI/TUI) can format the right re-login hint -- the runtime never embeds the
/// CLI command itself, keeping this type free of Tier-4 command detail.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct AuthError {
    message: String,
    provider: Option<String>,
}

impl AuthError {
    #[cfg(test)]
    pub(crate) fn new(cause: impl std::fmt::Display) -> Self {
        Self {
            message: cause.to_string(),
            provider: None,
        }
    }

    /// Records the failing provider id so callers can render a
    /// provider-specific re-login hint at the UI boundary.
    pub(crate) fn for_provider(provider: impl Into<String>, cause: impl std::fmt::Display) -> Self {
        Self {
            message: cause.to_string(),
            provider: Some(provider.into()),
        }
    }

    /// The failing provider id, when known.
    pub(crate) fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }
}

/// The provider rejected a request because the conversation exceeded the
/// model's context window (issue #211).
///
/// Providers attach this after classifying their own native error signal (an
/// HTTP 400 body naming a token/context limit), so the harness can recover by
/// compacting and retrying without ever parsing provider error strings itself.
/// The wrapped message is the provider's already-sanitized diagnostics line,
/// never the raw response body.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct ContextOverflowError {
    message: String,
}

impl ContextOverflowError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Whether an error (anywhere in its chain) is a provider context-window
/// overflow, so the harness can compact and retry instead of surfacing it.
pub(crate) fn is_context_overflow(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ContextOverflowError>().is_some()
}

/// Whether a provider error body names a context-window/token-limit overflow.
///
/// Called by provider adapters on the RAW (unsanitized) error body -- only the
/// boolean ever escapes, so no body text is surfaced. The patterns cover the
/// providers Iris routes to today: Anthropic ("prompt is too long", "input
/// length and `max_tokens` exceed context limit"), OpenAI ("context_length_exceeded",
/// "maximum context length", "exceeds the context window"), and Gemini ("input
/// token count ... exceeds the maximum"). Deliberately a handful of patterns,
/// not pi's 24: unknown phrasings fail open to the ordinary error path.
pub(crate) fn body_indicates_context_overflow(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "prompt is too long",
        "exceed context limit",
        "context_length_exceeded",
        "maximum context length",
        "exceeds the context window",
        "context window exceeded",
        "exceeds the maximum number of tokens",
    ];
    if PATTERNS.iter().any(|pattern| lower.contains(pattern)) {
        return true;
    }
    // Gemini phrases the limit as "the input token count (N) exceeds ...".
    lower.contains("input token count") && lower.contains("exceeds")
}

/// The command line was malformed (unknown command or bad arguments).
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(crate) struct UsageError {
    message: String,
}

impl UsageError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Map a top-level error to a process exit code, classifying by typed cause.
///
/// `2` usage, `3` auth, `1` everything else. The message is printed by the
/// caller; this only chooses the code.
pub(crate) fn exit_code(error: &anyhow::Error) -> u8 {
    if error.downcast_ref::<UsageError>().is_some() {
        2
    } else if error.downcast_ref::<AuthError>().is_some() {
        3
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn classifies_usage_error_as_code_two() {
        let error = anyhow::Error::new(UsageError::new("unknown command"));
        assert_eq!(exit_code(&error), 2);
    }

    #[test]
    fn classifies_auth_error_as_code_three() {
        let error = anyhow::Error::new(AuthError::new("token refresh failed"));
        assert_eq!(exit_code(&error), 3);
    }

    #[test]
    fn classifies_other_error_as_code_one() {
        let error = anyhow!("disk full");
        assert_eq!(exit_code(&error), 1);
    }

    #[test]
    fn auth_error_preserves_cause_message() {
        let error = AuthError::new("HTTP 401: invalid token");
        assert_eq!(error.to_string(), "HTTP 401: invalid token");
    }

    #[test]
    fn classifies_through_context_wrapping() {
        let auth = anyhow::Error::new(AuthError::new("expired")).context("startup");
        assert_eq!(exit_code(&auth), 3);
        let usage = anyhow::Error::new(UsageError::new("bad")).context("parsing args");
        assert_eq!(exit_code(&usage), 2);
    }

    #[test]
    fn context_overflow_is_detected_through_context_wrapping() {
        let error =
            anyhow::Error::new(ContextOverflowError::new("prompt too long")).context("turn");
        assert!(is_context_overflow(&error));
        assert!(!is_context_overflow(&anyhow!("disk full")));
        assert!(!is_context_overflow(&anyhow::Error::new(AuthError::new(
            "401"
        ))));
    }

    #[test]
    fn overflow_body_patterns_cover_the_supported_providers() {
        // Anthropic
        assert!(body_indicates_context_overflow(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 250000 tokens > 200000 maximum"}}"#
        ));
        assert!(body_indicates_context_overflow(
            "input length and `max_tokens` exceed context limit"
        ));
        // OpenAI
        assert!(body_indicates_context_overflow(
            r#"{"error":{"code":"context_length_exceeded","message":"..."}}"#
        ));
        assert!(body_indicates_context_overflow(
            "This model's maximum context length is 128000 tokens."
        ));
        assert!(body_indicates_context_overflow(
            "Your input exceeds the context window of this model."
        ));
        // Gemini
        assert!(body_indicates_context_overflow(
            "The input token count (1200000) exceeds the maximum number of tokens allowed (1048576)."
        ));

        // Ordinary errors fail open to the normal error path.
        assert!(!body_indicates_context_overflow("rate limit exceeded"));
        assert!(!body_indicates_context_overflow(
            r#"{"error":{"type":"invalid_request_error","message":"missing field"}}"#
        ));
        assert!(!body_indicates_context_overflow(""));
    }
}
