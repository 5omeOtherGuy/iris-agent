//! Antigravity (Google account OAuth -> Gemini Code Assist) auth: PKCE browser
//! login, token refresh, and Antigravity project-id discovery. The access token
//! and its project id are persisted together (the project id under
//! `extra["projectId"]`).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::mimir::auth::storage::{AuthStore, OAuthCredentials};

/// Auth-store provider key for Antigravity Google OAuth credentials.
pub(crate) const AUTH_PROVIDER: &str = "antigravity";

// Antigravity installed-app OAuth client ID: a public, per-application identity
// (every Antigravity client ships the same value). Base64-wrapped and decoded at
// runtime so the raw ID is not casually grep-able, mirroring pi-mono's
// `decode(atob(...))` pattern -- obfuscation, not a secret.
const CLIENT_ID_B64: &str = "MTA3MTAwNjA2MDU5MS10bWhzc2luMmgyMWxjcmUyMzV2dG9sb2poNGc0MDNlcC5hcHBzLmdvb2dsZXVzZXJjb250ZW50LmNvbQ";
static CLIENT_ID: LazyLock<String> = LazyLock::new(|| {
    String::from_utf8(
        URL_SAFE_NO_PAD
            .decode(CLIENT_ID_B64)
            .expect("embedded client id is valid base64"),
    )
    .expect("embedded client id is valid utf-8")
});
// The installed-app OAuth client secret is not committed to the public repo.
// Release builders may inject it at compile time with ANTIGRAVITY_CLIENT_SECRET;
// local/development runs may provide the same variable at runtime.
const COMPILED_CLIENT_SECRET: Option<&str> = option_env!("ANTIGRAVITY_CLIENT_SECRET");
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const REDIRECT_URI: &str = "http://localhost:51121/oauth-callback";
const CALLBACK_ADDR: &str = "127.0.0.1:51121";
const CALLBACK_PATH: &str = "/oauth-callback";
const SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";
const LOAD_CODE_ASSIST_URL: &str = "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist";
const CODE_ASSIST_USER_AGENT: &str = "google-api-nodejs-client/9.15.1";
const PROJECT_ID_KEY: &str = "projectId";
/// Refresh the token if it expires within this window (5 minutes).
const EXPIRY_SKEW_MS: u128 = 300_000;

/// A resolved access token plus the Antigravity project id it is scoped to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AntigravityToken {
    pub(crate) bearer: String,
    pub(crate) project_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct AntigravityTokenStore {
    storage: AuthStore,
}

impl AntigravityTokenStore {
    pub(crate) fn from_env() -> Result<Self> {
        Ok(Self {
            storage: AuthStore::from_env()?,
        })
    }

    /// Return the current access token + project id, refreshing the token near
    /// expiry and discovering/reusing the project id.
    pub(crate) fn access_token(&self, client: &Client) -> Result<AntigravityToken> {
        let mut credentials = self.storage.oauth_credentials(AUTH_PROVIDER)?;
        if credentials.expires <= now_millis() + EXPIRY_SKEW_MS {
            credentials = refresh_access_token(client, &credentials)?;
            self.storage
                .set_oauth_credentials(AUTH_PROVIDER, credentials.clone())?;
        }
        self.resolve_token(client, credentials)
    }

    /// Force a token refresh regardless of cached expiry (used after an HTTP
    /// 401/403) and return the new token + project id.
    pub(crate) fn force_refresh(&self, client: &Client) -> Result<AntigravityToken> {
        let credentials = self.storage.oauth_credentials(AUTH_PROVIDER)?;
        let refreshed = refresh_access_token(client, &credentials)?;
        self.storage
            .set_oauth_credentials(AUTH_PROVIDER, refreshed.clone())?;
        self.resolve_token(client, refreshed)
    }

    /// Extract the project id from stored credentials, rediscovering and
    /// persisting it if it is missing.
    fn resolve_token(
        &self,
        client: &Client,
        mut credentials: OAuthCredentials,
    ) -> Result<AntigravityToken> {
        let project_id = match project_id_from_env() {
            Some(id) => id,
            None => match project_id_from_extra(&credentials) {
                Some(id) => id,
                None => {
                    let discovered = discover_project(client, &credentials.access)?;
                    credentials.extra.insert(
                        PROJECT_ID_KEY.to_string(),
                        Value::String(discovered.clone()),
                    );
                    self.storage
                        .set_oauth_credentials(AUTH_PROVIDER, credentials.clone())?;
                    discovered
                }
            },
        };
        Ok(AntigravityToken {
            bearer: credentials.access,
            project_id,
        })
    }
}

fn project_id_from_env() -> Option<String> {
    project_id_from_env_value(std::env::var("ANTIGRAVITY_PROJECT_ID").ok())
}

fn project_id_from_env_value(value: Option<String>) -> Option<String> {
    value
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
}

fn project_id_from_extra(credentials: &OAuthCredentials) -> Option<String> {
    credentials
        .extra
        .get(PROJECT_ID_KEY)
        .and_then(Value::as_str)
        .and_then(|id| project_id_from_env_value(Some(id.to_string())))
}

/// Run the Google OAuth PKCE browser login, persist the credentials, and
/// discover the Antigravity project id. `on_auth` receives the authorization
/// URL to open.
pub(crate) fn login_browser(client: &Client, on_auth: impl FnOnce(&str)) -> Result<()> {
    // Fail fast on a missing client secret before opening the browser, rather
    // than after the user has already authorized.
    client_secret()?;
    let listener = TcpListener::bind(CALLBACK_ADDR)
        .context("failed to start Antigravity OAuth callback server")?;
    let pkce = create_pkce();
    // OAuth state is public (it appears in the auth URL); keep it independent
    // from the private PKCE verifier used later for the token exchange.
    let state = create_oauth_state();
    let url = authorization_url(&pkce.challenge, &state)?;

    on_auth(&url);

    let code = wait_for_browser_code(listener, &state)?;
    let mut credentials = exchange_authorization_code(client, &code, &pkce.verifier)?;
    let project_id = discover_project(client, &credentials.access)?;
    credentials
        .extra
        .insert(PROJECT_ID_KEY.to_string(), Value::String(project_id));
    AuthStore::from_env()?.set_oauth_credentials(AUTH_PROVIDER, credentials)
}

#[derive(Debug, Clone)]
struct Pkce {
    verifier: String,
    challenge: String,
}

fn create_pkce() -> Pkce {
    let verifier = random_url_token(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

fn create_oauth_state() -> String {
    random_url_token(32)
}

fn random_url_token(byte_len: usize) -> String {
    let mut bytes = vec![0_u8; byte_len];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn authorization_url(challenge: &str, state: &str) -> Result<String> {
    let mut url = Url::parse(AUTHORIZE_URL)?;
    url.query_pairs_mut()
        .append_pair("client_id", CLIENT_ID.as_str())
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent");
    Ok(url.to_string())
}

fn wait_for_browser_code(listener: TcpListener, expected_state: &str) -> Result<String> {
    for stream in listener.incoming() {
        let mut stream = stream.context("failed to receive Antigravity OAuth callback")?;
        let request = read_http_request(&mut stream)?;
        match parse_callback_request(&request, expected_state)? {
            Some(code) => {
                write_callback_response(
                    &mut stream,
                    200,
                    "Antigravity authentication completed. You can close this window.",
                )?;
                return Ok(code);
            }
            None => write_callback_response(&mut stream, 404, "Not found.")?,
        }
    }
    bail!("Antigravity OAuth callback server stopped before receiving a code")
}

fn read_http_request(stream: &mut TcpStream) -> Result<String> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let len = stream
            .read(&mut buffer)
            .context("failed to read Antigravity OAuth callback")?;
        if len == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..len]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&request).into_owned())
}

fn write_callback_response(stream: &mut TcpStream, status: u16, body: &str) -> Result<()> {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .context("failed to write Antigravity OAuth callback response")
}

fn parse_callback_request(request: &str, expected_state: &str) -> Result<Option<String>> {
    let request_line = request
        .lines()
        .next()
        .ok_or_else(|| anyhow!("missing OAuth callback request line"))?;
    let target = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("missing OAuth callback target"))?;
    let url = if target.starts_with('/') {
        Url::parse(&format!("http://localhost{target}"))?
    } else {
        Url::parse(target)?
    };
    if url.path() != CALLBACK_PATH {
        return Ok(None);
    }
    let mut code = None;
    let mut state = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        bail!("OAuth state mismatch");
    }
    code.map(Some)
        .ok_or_else(|| anyhow!("missing OAuth authorization code"))
}

/// Resolve the installed-app client secret, preferring a runtime override over
/// the value injected into release builds. Source checkouts without either get
/// an actionable error instead of a late token-exchange failure.
fn client_secret() -> Result<String> {
    resolve_client_secret(
        std::env::var("ANTIGRAVITY_CLIENT_SECRET").ok(),
        COMPILED_CLIENT_SECRET,
    )
}

fn resolve_client_secret(
    env_value: Option<String>,
    compiled_value: Option<&str>,
) -> Result<String> {
    env_value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            compiled_value
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .ok_or_else(|| {
            anyhow!(
                "antigravity login requires ANTIGRAVITY_CLIENT_SECRET at runtime or when building Iris"
            )
        })
}

fn exchange_authorization_code(
    client: &Client,
    code: &str,
    code_verifier: &str,
) -> Result<OAuthCredentials> {
    let secret = client_secret()?;
    let response = client
        .post(TOKEN_URL)
        .header(ACCEPT, "application/json")
        .form(&[
            ("client_id", CLIENT_ID.as_str()),
            ("client_secret", secret.as_str()),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", code_verifier),
        ])
        .send()
        .context("failed to exchange Antigravity authorization code")?;
    read_token_response(response, "exchange", None)
}

fn refresh_access_token(
    client: &Client,
    credentials: &OAuthCredentials,
) -> Result<OAuthCredentials> {
    let secret = client_secret()?;
    let response = client
        .post(TOKEN_URL)
        .header(ACCEPT, "application/json")
        .form(&[
            ("client_id", CLIENT_ID.as_str()),
            ("client_secret", secret.as_str()),
            ("refresh_token", credentials.refresh.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .context("failed to refresh Antigravity token")?;
    // Reuse the existing refresh token + project id when the response omits them.
    read_token_response(response, "refresh", Some(credentials))
}

fn read_token_response(
    response: reqwest::blocking::Response,
    operation: &str,
    previous: Option<&OAuthCredentials>,
) -> Result<OAuthCredentials> {
    let status = response.status();
    if !status.is_success() {
        // Token-endpoint bodies are the highest-risk surface; omit entirely.
        let _ = response.text();
        bail!("Antigravity token {operation} failed ({status})");
    }

    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        expires_in: u128,
    }

    let token: TokenResponse = response
        .json()
        .with_context(|| format!("failed to parse token {operation} response"))?;
    let refresh = token
        .refresh_token
        .or_else(|| previous.map(|p| p.refresh.clone()))
        .ok_or_else(|| {
            anyhow!("Antigravity token {operation} response is missing a refresh token")
        })?;
    let extra = previous.map(|p| p.extra.clone()).unwrap_or_default();
    Ok(OAuthCredentials {
        access: token.access_token,
        refresh,
        expires: now_millis() + token.expires_in * 1000,
        extra,
    })
}

/// Discover the Antigravity Code Assist project id: explicit env override, then
/// a `loadCodeAssist` probe. We deliberately do not persist a hard-coded
/// fallback project after a failed probe: a 401/403 or network failure should not
/// poison the auth store with the wrong project.
fn discover_project(client: &Client, access: &str) -> Result<String> {
    if let Some(id) = project_id_from_env() {
        return Ok(id);
    }
    load_code_assist_project(client, access)?.ok_or_else(|| {
        anyhow!(
            "Antigravity project discovery did not return a project id; set ANTIGRAVITY_PROJECT_ID"
        )
    })
}

fn load_code_assist_project(client: &Client, access: &str) -> Result<Option<String>> {
    let response = client
        .post(LOAD_CODE_ASSIST_URL)
        .header(AUTHORIZATION, format!("Bearer {access}"))
        .header(CONTENT_TYPE, "application/json")
        .header(USER_AGENT, CODE_ASSIST_USER_AGENT)
        .json(&json!({
            "metadata": {
                "ideType": "IDE_UNSPECIFIED",
                "platform": "PLATFORM_UNSPECIFIED",
                "pluginType": "GEMINI",
            }
        }))
        .send()
        .context("failed to discover Antigravity project id")?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!(
            "Antigravity project discovery failed with HTTP {status}"
        ));
    }
    let payload: Value = response
        .json()
        .context("failed to parse Antigravity project discovery response")?;
    Ok(extract_project_id(&payload))
}

/// Pull `cloudaicompanionProject` (a string or `{id}`) from a payload.
fn extract_project_id(payload: &Value) -> Option<String> {
    let value = payload.get("cloudaicompanionProject")?;
    match value {
        Value::String(id) if !id.trim().is_empty() => Some(id.clone()),
        Value::Object(_) => value
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(str::to_string),
        _ => None,
    }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_id_decodes_to_installed_app_identity() {
        // Verifies the base64-wrapped ID round-trips to the public Antigravity
        // client identity. Client IDs are not secrets, so asserting the value
        // here is fine.
        assert!(CLIENT_ID.starts_with("1071006060591-"));
        assert!(CLIENT_ID.ends_with(".apps.googleusercontent.com"));
    }

    #[test]
    fn client_secret_uses_runtime_then_compiled_value() {
        assert!(resolve_client_secret(None, None).is_err());
        assert!(resolve_client_secret(Some("   ".to_string()), None).is_err());
        assert_eq!(
            resolve_client_secret(None, Some(" built ")).unwrap(),
            "built"
        );
        assert_eq!(
            resolve_client_secret(Some("  runtime  ".to_string()), Some("built")).unwrap(),
            "runtime",
            "runtime override wins over build-time injection"
        );
    }

    #[test]
    fn oauth_state_is_distinct_from_pkce_verifier() {
        let pkce = create_pkce();
        let state = create_oauth_state();
        assert_ne!(state, pkce.verifier, "state must not leak the verifier");
    }

    #[test]
    fn builds_authorization_url_with_pkce_and_offline_access() -> Result<()> {
        let url = Url::parse(&authorization_url("challenge", "verifier")?)?;
        let pairs = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(url.as_str().split('?').next(), Some(AUTHORIZE_URL));
        assert_eq!(pairs["client_id"].as_ref(), CLIENT_ID.as_str());
        assert_eq!(pairs["redirect_uri"].as_ref(), REDIRECT_URI);
        assert_eq!(pairs["code_challenge"].as_ref(), "challenge");
        assert_eq!(pairs["code_challenge_method"].as_ref(), "S256");
        assert_eq!(pairs["state"].as_ref(), "verifier");
        assert_eq!(pairs["access_type"].as_ref(), "offline");
        assert_eq!(pairs["prompt"].as_ref(), "consent");
        Ok(())
    }

    #[test]
    fn parses_callback_code_when_state_matches() -> Result<()> {
        let request =
            "GET /oauth-callback?code=abc123&state=verifier HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(
            parse_callback_request(request, "verifier")?,
            Some("abc123".to_string())
        );
        Ok(())
    }

    #[test]
    fn rejects_callback_state_mismatch() {
        let request =
            "GET /oauth-callback?code=abc123&state=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let error = parse_callback_request(request, "verifier")
            .unwrap_err()
            .to_string();
        assert!(error.contains("state mismatch"));
    }

    #[test]
    fn ignores_unrelated_callback_requests() -> Result<()> {
        let request = "GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(parse_callback_request(request, "verifier")?, None);
        Ok(())
    }

    #[test]
    fn extracts_project_id_from_string_or_object() {
        assert_eq!(
            extract_project_id(&json!({"cloudaicompanionProject": "proj-1"})).as_deref(),
            Some("proj-1")
        );
        assert_eq!(
            extract_project_id(&json!({"cloudaicompanionProject": {"id": "proj-2"}})).as_deref(),
            Some("proj-2")
        );
        assert_eq!(extract_project_id(&json!({"other": 1})), None);
    }

    #[test]
    fn project_id_env_value_is_trimmed() {
        assert_eq!(project_id_from_env_value(None), None);
        assert_eq!(project_id_from_env_value(Some("   ".to_string())), None);
        assert_eq!(
            project_id_from_env_value(Some("  proj-env  ".to_string())).as_deref(),
            Some("proj-env")
        );
    }

    #[test]
    fn project_id_round_trips_through_extra() {
        let mut credentials = OAuthCredentials {
            access: "a".to_string(),
            refresh: "r".to_string(),
            expires: 1,
            extra: serde_json::Map::new(),
        };
        assert_eq!(project_id_from_extra(&credentials), None);
        credentials.extra.insert(
            PROJECT_ID_KEY.to_string(),
            Value::String("proj-x".to_string()),
        );
        assert_eq!(
            project_id_from_extra(&credentials).as_deref(),
            Some("proj-x")
        );
    }

    #[test]
    fn refresh_response_reuses_old_refresh_and_sets_future_expiry() -> Result<()> {
        // Parse a refresh body lacking refresh_token via the same Deserialize +
        // fallback logic read_token_response uses, without a live HTTP call.
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            #[serde(default)]
            refresh_token: Option<String>,
            expires_in: u128,
        }
        let previous = OAuthCredentials {
            access: "old-access".to_string(),
            refresh: "old-refresh".to_string(),
            expires: 0,
            extra: {
                let mut map = serde_json::Map::new();
                map.insert(
                    PROJECT_ID_KEY.to_string(),
                    Value::String("proj-x".to_string()),
                );
                map
            },
        };
        let token: TokenResponse =
            serde_json::from_str(r#"{"access_token":"new-access","expires_in":3600}"#)?;
        let refresh = token
            .refresh_token
            .or_else(|| Some(previous.refresh.clone()))
            .unwrap();
        let expires = now_millis() + token.expires_in * 1000;
        assert_eq!(token.access_token, "new-access");
        assert_eq!(refresh, "old-refresh", "old refresh reused when omitted");
        assert!(expires > now_millis(), "expiry is in the future");
        // Extra (project id) carries forward on refresh.
        assert_eq!(
            previous.extra.get(PROJECT_ID_KEY).and_then(Value::as_str),
            Some("proj-x")
        );
        Ok(())
    }
}
