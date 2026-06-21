//! Operator-facing observability: structured logging setup, secret-safe
//! fingerprints, and sanitization of external response bodies for logs/errors.
//!
//! Logs go to stderr so the stdout chat transcript (assistant/tool output)
//! stays clean and machine-parseable. Verbosity is controlled by the standard
//! `RUST_LOG` env var via an `EnvFilter`; the default keeps the agent quiet
//! (warnings and errors only) unless the operator opts in.

use std::io::{self, Write};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing_subscriber::EnvFilter;

static INIT: Once = Once::new();
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Key fragments whose values are redacted from any external body before it is
/// surfaced in a log line or error message. Matched case-insensitively as
/// substrings so `access_token`, `refresh_token`, `client_secret`, etc. are all
/// covered.
const SENSITIVE_KEY_FRAGMENTS: &[&str] = &[
    "token",
    "secret",
    "authorization",
    "code_verifier",
    "device_auth_id",
    "user_code",
    "password",
    "api_key",
];

const MAX_BODY_CHARS: usize = 500;

/// Whether tracing should avoid stderr because the TUI owns the terminal.
pub(crate) fn set_tui_active(active: bool) {
    TUI_ACTIVE.store(active, Ordering::Relaxed);
}

fn stderr_unless_tui_active() -> LogWriter {
    // ponytail: drop logs while the live TUI owns the terminal. Add file
    // logging if operators need diagnostics during interactive sessions.
    if TUI_ACTIVE.load(Ordering::Relaxed) {
        LogWriter::Sink(io::sink())
    } else {
        LogWriter::Stderr(io::stderr())
    }
}

enum LogWriter {
    Stderr(io::Stderr),
    Sink(io::Sink),
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            LogWriter::Stderr(writer) => writer.write(buf),
            LogWriter::Sink(writer) => writer.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            LogWriter::Stderr(writer) => writer.flush(),
            LogWriter::Sink(writer) => writer.flush(),
        }
    }
}

/// Initialize the global tracing subscriber exactly once.
///
/// Idempotent and safe to call from `main` before any logging. Reads
/// `RUST_LOG` (e.g. `RUST_LOG=iris_agent=debug`); when unset it defaults to
/// `warn`. Writes to stderr to avoid corrupting the stdout transcript, except
/// while the live TUI is active because stderr is the same terminal surface.
/// Uses `try_init` so a pre-installed subscriber (e.g. in tests) does not panic.
pub(crate) fn init() {
    INIT.call_once(|| {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(stderr_unless_tui_active)
            .with_target(true)
            .compact()
            .try_init();
    });
}

/// Render a secret as a non-reversible fingerprint safe for logs.
///
/// Returns a short SHA-256 digest prefix plus the length, never any byte of the
/// secret itself, so a debug log can answer "which token am I using / did it
/// change?" without leaking the credential.
pub(crate) fn redact_secret(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    let prefix: String = digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{prefix} len={}", secret.chars().count())
}

/// Produce a log/error-safe rendering of an external HTTP error body.
///
/// Only JSON bodies are surfaced, with sensitive values redacted recursively;
/// non-JSON bodies are omitted entirely (returns `None`) since they may contain
/// unstructured secrets, PII, or conversation-adjacent text. The JSON result is
/// truncated so messages and logs stay readable.
pub(crate) fn sanitize_external_body(body: &str) -> Option<String> {
    let mut value: Value = serde_json::from_str(body).ok()?;
    redact_json(&mut value);
    Some(truncate(&value.to_string(), MAX_BODY_CHARS))
}

fn redact_json(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *child = Value::String("<redacted>".to_string());
                } else {
                    redact_json(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_json),
        _ => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_KEY_FRAGMENTS
        .iter()
        .any(|fragment| lower.contains(fragment))
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    format!("{head}... (truncated)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_secret_never_contains_secret_bytes() {
        let secret = "sk-supersecrettokenvalue1234567890";
        let rendered = redact_secret(secret);
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains("sk-"));
        assert!(!rendered.contains("supersecret"));
    }

    #[test]
    fn redact_secret_reports_length_and_is_deterministic() {
        let rendered = redact_secret("sk-abcdefghijkl");
        assert!(rendered.contains("len=15"));
        assert!(rendered.starts_with("sha256:"));
        assert_eq!(rendered, redact_secret("sk-abcdefghijkl"));
        assert_ne!(rendered, redact_secret("sk-different-value"));
    }

    #[test]
    fn sanitize_redacts_sensitive_keys_recursively() {
        let body = r#"{"error":{"message":"bad","code":"invalid"},"access_token":"leak","nested":{"refresh_token":"leak2"}}"#;
        let rendered = sanitize_external_body(body).expect("json body");
        assert!(!rendered.contains("leak"));
        assert!(rendered.contains("<redacted>"));
        // Non-sensitive diagnostic fields are preserved.
        assert!(rendered.contains("invalid"));
        assert!(rendered.contains("bad"));
    }

    #[test]
    fn sanitize_omits_non_json_bodies() {
        assert_eq!(
            sanitize_external_body("plain text error with sk-token123"),
            None
        );
    }

    #[test]
    fn sanitize_truncates_long_bodies() {
        let big = format!("{{\"msg\":\"{}\"}}", "x".repeat(2000));
        let rendered = sanitize_external_body(&big).expect("json body");
        assert!(rendered.contains("(truncated)"));
        assert!(rendered.chars().count() < 600);
    }

    #[test]
    fn stderr_writer_is_suppressed_while_tui_is_active() {
        set_tui_active(false);
        assert!(matches!(stderr_unless_tui_active(), LogWriter::Stderr(_)));

        set_tui_active(true);
        assert!(matches!(stderr_unless_tui_active(), LogWriter::Sink(_)));

        set_tui_active(false);
    }
}
