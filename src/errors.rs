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
    pub(crate) fn new(cause: impl std::fmt::Display) -> Self {
        Self {
            message: cause.to_string(),
            provider: None,
        }
    }

    /// Like [`AuthError::new`], but records the failing provider id so callers
    /// can render a provider-specific re-login hint at the UI boundary.
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
}
