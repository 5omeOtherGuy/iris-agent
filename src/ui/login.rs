//! `/login` and `/logout` orchestration (Tier 3).
//!
//! Builds the provider lists for the auth-method/provider selectors (with
//! no-secret status badges), runs the existing blocking OAuth helpers behind a
//! [`LoginBackend`] seam so the TUI loop can drive them on a blocking task and
//! tests can inject a fake, and applies `/logout` by removing only credentials
//! stored by `/login`.
//!
//! Scope: Iris authenticates every provider by OAuth/subscription, so the
//! "Use an API key" branch shows `No API key providers available.` and no
//! API-key input dialog is built (a deliberate deferral -- see the task report).
//! Anthropic runs a real OAuth PKCE browser login (with a manual paste
//! fallback), the same shape as the other subscription providers.

use anyhow::Result;
use reqwest::blocking::Client;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::mimir::auth;
use crate::mimir::auth::storage::{AuthStore, CredentialKind};
use crate::mimir::model_catalog;
use crate::mimir::selection::ProviderId;
use crate::ui::modal::{
    LoginMethod, MethodSelect, Modal, ProviderPurpose, ProviderRow, ProviderSelect,
};

/// Either open a modal, or show status lines (e.g. nothing to do).
pub(crate) enum LoginStep {
    Open(Modal),
    Lines(Vec<String>),
}

/// Open the `/login` method selector (subscription vs API key).
pub(crate) fn open_login() -> Modal {
    Modal::LoginMethod(MethodSelect::new())
}

/// Build the provider selector for a chosen auth method.
pub(crate) fn provider_select(method: LoginMethod, auth: &AuthStore) -> LoginStep {
    match method {
        LoginMethod::Subscription => {
            let providers = subscription_providers(auth);
            LoginStep::Open(Modal::Providers(ProviderSelect::new(
                ProviderPurpose::Login,
                providers,
                "No subscription providers available.",
            )))
        }
        // Iris has no API-key-backed providers today.
        LoginMethod::ApiKey => {
            LoginStep::Lines(vec!["No API key providers available.".to_string()])
        }
    }
}

/// OAuth/subscription providers with their no-secret status badge.
fn subscription_providers(auth: &AuthStore) -> Vec<ProviderRow> {
    let mut rows: Vec<ProviderRow> = ProviderId::ALL
        .iter()
        .map(|provider| ProviderRow {
            id: provider.as_str().to_string(),
            name: provider.display_name().to_string(),
            badge: model_catalog::provider_status(auth, *provider)
                .badge()
                .to_string(),
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// Open the `/logout` selector over only providers with stored credentials.
/// Returns status lines (not a picker) when nothing is stored, matching
/// pi-mono's "only /login credentials are removed" message.
pub(crate) fn open_logout(auth: &AuthStore) -> LoginStep {
    let stored = match auth.stored_credentials() {
        Ok(stored) => stored,
        Err(error) => {
            return LoginStep::Lines(vec![format!("could not read credentials: {error:#}")]);
        }
    };
    if stored.is_empty() {
        return LoginStep::Lines(vec![
            "No stored credentials to remove. /logout only removes credentials saved by /login; \
             environment variables and external config are unchanged."
                .to_string(),
        ]);
    }
    let providers: Vec<ProviderRow> = stored
        .into_iter()
        .map(|cred| ProviderRow {
            name: provider_display_name(&cred.provider_id),
            badge: match cred.kind {
                CredentialKind::OAuth => "subscription".to_string(),
                CredentialKind::ApiKey => "API key".to_string(),
                CredentialKind::Unknown => "stored".to_string(),
            },
            id: cred.provider_id,
        })
        .collect();
    LoginStep::Open(Modal::Providers(ProviderSelect::new(
        ProviderPurpose::Logout,
        providers,
        "No providers logged in. Use /login first.",
    )))
}

/// Apply a `/logout`: remove only the stored credential for `provider_id`.
/// Environment variables and external (Claude Code) config are untouched.
pub(crate) fn apply_logout(provider_id: &str, auth: &AuthStore) -> Vec<String> {
    // Capture the kind before removal so the message phrasing is right.
    let kind = auth
        .stored_credentials()
        .ok()
        .and_then(|stored| {
            stored
                .into_iter()
                .find(|cred| cred.provider_id == provider_id)
                .map(|cred| cred.kind)
        })
        .unwrap_or(CredentialKind::Unknown);
    match auth.remove_credentials(provider_id) {
        Ok(true) => {
            let name = provider_display_name(provider_id);
            vec![match kind {
                CredentialKind::ApiKey => format!(
                    "Removed stored API key for {name}. Environment variables and external config are unchanged."
                ),
                _ => format!("Logged out of {name}"),
            }]
        }
        Ok(false) => vec![format!("No stored credentials for {provider_id}")],
        Err(error) => vec![format!("Logout failed: {error:#}")],
    }
}

/// Friendly provider name for a stored id (falls back to the raw id).
fn provider_display_name(provider_id: &str) -> String {
    ProviderId::parse(provider_id)
        .map(|provider| provider.display_name().to_string())
        .unwrap_or_else(|_| provider_id.to_string())
}

// --- login backend seam ---

/// A live dialog update emitted during an OAuth login. Iris drives the in-TUI
/// flow with the browser-callback helpers, so only the auth-URL and progress
/// states occur here; device-code login stays a CLI option
/// (`iris login openai-codex --device-code`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoginUpdate {
    /// Show an authorization URL and a click hint.
    AuthUrl { url: String, hint: String },
    /// Append a progress line.
    Progress(String),
}

/// What a completed login produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoginOutcome {
    /// Credentials were stored.
    LoggedIn,
}

/// Runs a blocking OAuth login for a provider, pushing dialog updates. Abstracted
/// so the TUI loop drives the real flow on a blocking task while tests inject a
/// deterministic fake. `cancel` lets the loop abort the blocking callback wait
/// (releasing the port); `manual_rx` feeds a pasted authorization code / redirect
/// URL for providers that support a manual fallback (Anthropic).
pub(crate) trait LoginBackend: Send + Sync + 'static {
    fn login(
        &self,
        provider: ProviderId,
        cancel: &CancellationToken,
        manual_rx: Option<&Receiver<String>>,
        on_update: &dyn Fn(LoginUpdate),
    ) -> Result<LoginOutcome>;
}

/// The production backend: drives the existing `mimir::auth` browser OAuth
/// helpers, which store credentials in `auth.json` on success.
pub(crate) struct OAuthLoginBackend;

impl LoginBackend for OAuthLoginBackend {
    fn login(
        &self,
        provider: ProviderId,
        cancel: &CancellationToken,
        manual_rx: Option<&Receiver<String>>,
        on_update: &dyn Fn(LoginUpdate),
    ) -> Result<LoginOutcome> {
        // The URL is auto-opened (see `apply_login_update`); the hint covers the
        // headless / no-opener fallback. Ctrl/Cmd+click is unreliable here
        // because the bordered modal cannot carry an OSC-8 hyperlink.
        let open_hint = "Opening your browser. If it does not open, copy the URL above to sign in.";
        // A generous timeout covers the browser round-trip plus the bounded
        // callback wait the helpers enforce internally.
        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()?;
        match provider {
            ProviderId::OpenAiCodex => {
                auth::openai_codex::login_browser(&client, cancel, |info| {
                    on_update(LoginUpdate::AuthUrl {
                        url: info.url.clone(),
                        hint: open_hint.to_string(),
                    });
                    on_update(LoginUpdate::Progress(format!(
                        "Waiting for callback at {} ...",
                        info.redirect_uri
                    )));
                })?;
                Ok(LoginOutcome::LoggedIn)
            }
            ProviderId::Antigravity => {
                auth::antigravity::login_browser(&client, cancel, |url| {
                    on_update(LoginUpdate::AuthUrl {
                        url: url.to_string(),
                        hint: open_hint.to_string(),
                    });
                    on_update(LoginUpdate::Progress("Waiting for callback...".to_string()));
                })?;
                Ok(LoginOutcome::LoggedIn)
            }
            ProviderId::Anthropic => {
                auth::anthropic::login_browser(&client, cancel, manual_rx, |url| {
                    on_update(LoginUpdate::AuthUrl {
                        url: url.to_string(),
                        hint: open_hint.to_string(),
                    });
                    on_update(LoginUpdate::Progress(
                        "Waiting for the browser callback, or paste the code below...".to_string(),
                    ));
                })?;
                Ok(LoginOutcome::LoggedIn)
            }
        }
    }
}

/// Best-effort: open `url` in the user's default browser. The OAuth dialogs also
/// display the URL, so failures (headless host, no opener on PATH) are ignored
/// rather than surfaced -- the user can still copy the URL manually. The child is
/// fully detached from our stdio so it cannot corrupt the raw-mode TUI.
pub(crate) fn open_in_browser(url: &str) {
    let (program, args) = browser_open_command(std::env::consts::OS, url);
    let _ = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Platform command + args that open `url` in the default browser. Split out as a
/// pure function so the per-OS argument shape is unit-testable without spawning.
fn browser_open_command(os: &str, url: &str) -> (&'static str, Vec<String>) {
    match os {
        "macos" => ("open", vec![url.to_owned()]),
        // `start` is a cmd builtin; the empty "" is the window-title argument so
        // a quoted URL is not consumed as the window title.
        "windows" => (
            "cmd",
            vec![
                "/C".to_owned(),
                "start".to_owned(),
                String::new(),
                url.to_owned(),
            ],
        ),
        // Linux/BSD and anything else: the freedesktop opener.
        _ => ("xdg-open", vec![url.to_owned()]),
    }
}

/// The status line shown after a login completes.
pub(crate) fn login_complete_lines(provider: ProviderId, outcome: &LoginOutcome) -> Vec<String> {
    match outcome {
        LoginOutcome::LoggedIn => vec![format!("Logged in to {}", provider.display_name())],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mimir::auth::storage::OAuthCredentials;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_auth() -> (AuthStore, PathBuf) {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "iris-login-test-{nanos}-{}-{seq}/auth.json",
            std::process::id()
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        (AuthStore::from_path(path.clone()), path)
    }

    const SECRET: &str = "sk-super-secret-token";

    fn oauth() -> OAuthCredentials {
        OAuthCredentials {
            access: SECRET.to_string(),
            refresh: SECRET.to_string(),
            expires: 9_999_999_999_999,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn subscription_list_marks_configured_and_unconfigured_without_secrets() {
        let (auth, path) = temp_auth();
        auth.set_oauth_credentials("openai-codex", oauth()).unwrap();
        let rows = subscription_providers(&auth);
        let codex = rows.iter().find(|r| r.id == "openai-codex").unwrap();
        assert_eq!(codex.badge, "✓ configured");
        let antigravity = rows.iter().find(|r| r.id == "antigravity").unwrap();
        assert_eq!(antigravity.badge, "unconfigured");
        // No secret material leaks into any row (name or badge).
        assert!(
            rows.iter()
                .all(|r| !r.badge.contains(SECRET) && !r.name.contains(SECRET))
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn browser_open_command_is_platform_correct_and_carries_full_url() {
        let url = "https://accounts.google.com/o/oauth2/v2/auth?client_id=abc&scope=x+y";
        let (mac_prog, mac_args) = browser_open_command("macos", url);
        assert_eq!(mac_prog, "open");
        assert_eq!(mac_args, vec![url.to_string()]);

        let (win_prog, win_args) = browser_open_command("windows", url);
        assert_eq!(win_prog, "cmd");
        // Empty title arg guards against a quoted URL being read as the title.
        assert_eq!(
            win_args,
            vec![
                "/C".to_string(),
                "start".to_string(),
                String::new(),
                url.to_string()
            ]
        );

        let (nix_prog, nix_args) = browser_open_command("linux", url);
        assert_eq!(nix_prog, "xdg-open");
        assert_eq!(nix_args, vec![url.to_string()]);
        // The full URL is always the final argument, never truncated.
        assert_eq!(nix_args.last().map(String::as_str), Some(url));
    }

    #[test]
    fn api_key_method_reports_no_providers() {
        let (auth, path) = temp_auth();
        match provider_select(LoginMethod::ApiKey, &auth) {
            LoginStep::Lines(lines) => {
                assert_eq!(lines, vec!["No API key providers available.".to_string()])
            }
            _ => panic!("expected lines"),
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn logout_lists_only_stored_and_removes_only_those() {
        let (auth, path) = temp_auth();
        auth.set_oauth_credentials("anthropic", oauth()).unwrap();
        // Logout list contains the stored provider.
        match open_logout(&auth) {
            LoginStep::Open(Modal::Providers(_)) => {}
            _ => panic!("expected provider selector"),
        }
        // Removing it reports the OAuth phrasing and clears the store.
        let lines = apply_logout("anthropic", &auth);
        assert!(lines[0].contains("Logged out of Anthropic"), "{lines:?}");
        assert!(!auth.has_credentials("anthropic").unwrap());
        // Now empty -> status, not a picker.
        match open_logout(&auth) {
            LoginStep::Lines(lines) => assert!(lines[0].contains("No stored credentials")),
            _ => panic!("expected empty status"),
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    /// A deterministic backend that records the requested provider and returns a
    /// preset outcome, proving the login flow without any network.
    struct FakeBackend {
        seen: Arc<Mutex<Vec<ProviderId>>>,
        outcome: LoginOutcome,
    }

    impl LoginBackend for FakeBackend {
        fn login(
            &self,
            provider: ProviderId,
            _cancel: &CancellationToken,
            _manual_rx: Option<&Receiver<String>>,
            on_update: &dyn Fn(LoginUpdate),
        ) -> Result<LoginOutcome> {
            on_update(LoginUpdate::AuthUrl {
                url: "https://example/auth".to_string(),
                hint: "Ctrl+click to open".to_string(),
            });
            self.seen.lock().unwrap().push(provider);
            Ok(self.outcome.clone())
        }
    }

    #[test]
    fn fake_backend_drives_updates_and_outcome() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = FakeBackend {
            seen: seen.clone(),
            outcome: LoginOutcome::LoggedIn,
        };
        let updates = Arc::new(Mutex::new(Vec::new()));
        let updates_c = updates.clone();
        let outcome = backend
            .login(
                ProviderId::OpenAiCodex,
                &CancellationToken::new(),
                None,
                &move |u| updates_c.lock().unwrap().push(u),
            )
            .unwrap();
        assert_eq!(outcome, LoginOutcome::LoggedIn);
        assert_eq!(seen.lock().unwrap().as_slice(), &[ProviderId::OpenAiCodex]);
        assert_eq!(updates.lock().unwrap().len(), 1);
        assert_eq!(
            login_complete_lines(ProviderId::OpenAiCodex, &outcome),
            vec!["Logged in to OpenAI".to_string()]
        );
    }
}
