use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::mimir::auth::device_code::{DeviceCodePoll, poll_device_code};
use crate::mimir::auth::oauth_callback::{
    CallbackConfig, CallbackServer, LOGIN_TIMEOUT, create_pkce, random_url_token,
};
use crate::mimir::auth::storage::{AuthStore, OAuthCredentials};
use crate::telemetry;

const AUTH_PROVIDER: &str = "openai-codex";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const BROWSER_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const BROWSER_CALLBACK_PORT: u16 = 1455;
const BROWSER_CALLBACK_PATH: &str = "/auth/callback";
const PROVIDER_LABEL: &str = "OpenAI Codex";
const SCOPE: &str = "openid profile email offline_access";
const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
const ACCOUNT_ID_CLAIM: &str = "https://api.openai.com/auth";

#[derive(Debug, Clone)]
pub(crate) struct OpenAiCodexTokenStore {
    storage: AuthStore,
}

/// Serializes token refreshes process-wide. OpenAI rotates the refresh token
/// on every use, so concurrent refreshes (observed live: an 8-worker subagent
/// group spawning at once) race the rotation and the losers fail their turn
/// with an auth rejection. One caller refreshes; the rest reuse its result.
static REFRESH_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Decide, after acquiring [`REFRESH_GUARD`], whether the caller still needs
/// its own refresh or can reuse a peer's freshly persisted credentials.
///
/// `stale_access` is `None` on the expiry path (`access_token`) and the
/// server-rejected bearer on the forced path (`force_refresh`).
fn needs_refresh(stale_access: Option<&str>, current: &OAuthCredentials, now: u128) -> bool {
    match stale_access {
        None => current.expires <= now,
        Some(stale) => current.access == stale,
    }
}

impl OpenAiCodexTokenStore {
    pub(crate) fn from_env() -> Result<Self> {
        Ok(Self {
            storage: AuthStore::from_env()?,
        })
    }

    pub(crate) fn access_token(&self, client: &Client) -> Result<AccessToken> {
        let mut credentials = self.storage.oauth_credentials(AUTH_PROVIDER)?;

        if credentials.expires <= now_millis() {
            credentials = self.refresh_synchronized(client, None)?;
        }

        let account_id = extract_account_id(&credentials.access)?;
        Ok(AccessToken {
            bearer: credentials.access,
            account_id,
        })
    }

    /// Refresh the access token and persist the result.
    ///
    /// Used when the provider receives an auth rejection (HTTP 401/403) even
    /// though the locally cached token had not yet expired, so a single forced
    /// refresh can recover a server-side-invalidated token. If a concurrent
    /// caller already rotated the rejected token, its replacement is reused
    /// instead of refreshing again.
    pub(crate) fn force_refresh(&self, client: &Client) -> Result<AccessToken> {
        let stale = self.storage.oauth_credentials(AUTH_PROVIDER)?;
        let refreshed = self.refresh_synchronized(client, Some(&stale.access))?;
        let account_id = extract_account_id(&refreshed.access)?;
        Ok(AccessToken {
            bearer: refreshed.access,
            account_id,
        })
    }

    fn refresh_synchronized(
        &self,
        client: &Client,
        stale_access: Option<&str>,
    ) -> Result<OAuthCredentials> {
        let _guard = REFRESH_GUARD
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Serialize against OTHER iris processes sharing this auth file (a
        // concurrent session forcing a refresh rotates the refresh token out
        // from under us just like a concurrent worker would). Taken inside
        // REFRESH_GUARD so lock order is fixed.
        let _file_lock = self.storage.lock_for_refresh()?;
        // Re-read after the locks: a peer (thread or process) may have
        // refreshed while we waited.
        let current = self.storage.oauth_credentials(AUTH_PROVIDER)?;
        if !needs_refresh(stale_access, &current, now_millis()) {
            return Ok(current);
        }
        let refreshed = refresh_access_token(client, &current.refresh)?;
        self.storage
            .set_oauth_credentials(AUTH_PROVIDER, refreshed.clone())?;
        Ok(refreshed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AccessToken {
    pub(crate) bearer: String,
    pub(crate) account_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceCodeInfo {
    pub(crate) user_code: String,
    pub(crate) verification_uri: String,
    pub(crate) interval_seconds: u64,
    pub(crate) expires_in_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BrowserLoginInfo {
    pub(crate) url: String,
    pub(crate) redirect_uri: String,
}

pub(crate) fn login_browser(
    client: &Client,
    cancel: &CancellationToken,
    on_auth: impl FnOnce(BrowserLoginInfo),
) -> Result<()> {
    let server = CallbackServer::bind(BROWSER_CALLBACK_PORT, PROVIDER_LABEL)?;
    let pkce = create_pkce();
    let state = random_url_token(16);
    let url = authorization_url(&pkce.challenge, &state)?;

    on_auth(BrowserLoginInfo {
        url,
        redirect_uri: BROWSER_REDIRECT_URI.to_string(),
    });

    let config = CallbackConfig {
        expected_state: &state,
        callback_path: BROWSER_CALLBACK_PATH,
        success_message: "OpenAI authentication completed. You can close this window.",
        provider_label: PROVIDER_LABEL,
    };
    let code = server.wait_for_code(&config, Instant::now() + LOGIN_TIMEOUT, cancel, None)?;
    let credentials =
        exchange_authorization_code(client, &code, &pkce.verifier, BROWSER_REDIRECT_URI)?;
    // A cancel that lands after the code arrived but before persistence must not
    // store credentials behind a dismissed dialog.
    if cancel.is_cancelled() {
        bail!("OpenAI Codex login cancelled");
    }
    AuthStore::from_env()?.set_oauth_credentials(AUTH_PROVIDER, credentials)
}

pub(crate) fn login_device_code(
    client: &Client,
    on_code: impl FnOnce(DeviceCodeInfo),
) -> Result<()> {
    let device = start_device_auth(client)?;
    on_code(DeviceCodeInfo {
        user_code: device.user_code.clone(),
        verification_uri: DEVICE_VERIFICATION_URI.to_string(),
        interval_seconds: device.interval_seconds,
        expires_in_seconds: DEVICE_CODE_TIMEOUT_SECONDS,
    });
    let code = poll_device_auth(client, &device)?;
    let credentials = exchange_authorization_code(
        client,
        &code.authorization_code,
        &code.code_verifier,
        DEVICE_REDIRECT_URI,
    )?;
    AuthStore::from_env()?.set_oauth_credentials(AUTH_PROVIDER, credentials)
}

fn authorization_url(challenge: &str, state: &str) -> Result<String> {
    let mut url = Url::parse(AUTHORIZE_URL)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", BROWSER_REDIRECT_URI)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("originator", "iris");
    Ok(url.to_string())
}

#[derive(Debug, Clone)]
struct DeviceAuthInfo {
    device_auth_id: String,
    user_code: String,
    interval_seconds: u64,
}

#[derive(Debug, Clone)]
struct DeviceTokenSuccess {
    authorization_code: String,
    code_verifier: String,
}

fn start_device_auth(client: &Client) -> Result<DeviceAuthInfo> {
    let response = client
        .post(DEVICE_USER_CODE_URL)
        .header(CONTENT_TYPE, "application/json")
        .json(&serde_json::json!({ "client_id": CLIENT_ID }))
        .send()
        .context("failed to request OpenAI Codex device code")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        match telemetry::sanitize_oauth_body(&body) {
            Some(detail) => bail!("OpenAI Codex device code request failed ({status}): {detail}"),
            None => bail!("OpenAI Codex device code request failed ({status})"),
        }
    }

    #[derive(Deserialize)]
    struct DeviceAuthResponse {
        device_auth_id: String,
        user_code: String,
        interval: Option<Value>,
    }

    let body: DeviceAuthResponse = response
        .json()
        .context("failed to parse OpenAI Codex device code response")?;
    let interval_seconds = parse_interval_seconds(body.interval.as_ref())?.unwrap_or(5);
    Ok(DeviceAuthInfo {
        device_auth_id: body.device_auth_id,
        user_code: body.user_code,
        interval_seconds,
    })
}

fn poll_device_auth(client: &Client, device: &DeviceAuthInfo) -> Result<DeviceTokenSuccess> {
    poll_device_code(
        Some(device.interval_seconds),
        Some(DEVICE_CODE_TIMEOUT_SECONDS),
        || poll_device_auth_once(client, device),
    )
}

fn poll_device_auth_once(
    client: &Client,
    device: &DeviceAuthInfo,
) -> Result<DeviceCodePoll<DeviceTokenSuccess>> {
    let response = client
        .post(DEVICE_TOKEN_URL)
        .header(CONTENT_TYPE, "application/json")
        .json(&serde_json::json!({
            "device_auth_id": device.device_auth_id,
            "user_code": device.user_code,
        }))
        .send()
        .context("failed to poll OpenAI Codex device auth")?;

    if response.status().is_success() {
        #[derive(Deserialize)]
        struct DeviceTokenResponse {
            authorization_code: String,
            code_verifier: String,
        }

        let body: DeviceTokenResponse = response
            .json()
            .context("failed to parse OpenAI Codex device token response")?;
        return Ok(DeviceCodePoll::Complete(DeviceTokenSuccess {
            authorization_code: body.authorization_code,
            code_verifier: body.code_verifier,
        }));
    }

    if response.status().as_u16() == 403 || response.status().as_u16() == 404 {
        return Ok(DeviceCodePoll::Pending);
    }

    let status = response.status();
    let body = response.text().unwrap_or_default();
    match error_code(&body).as_deref() {
        Some("deviceauth_authorization_pending") => Ok(DeviceCodePoll::Pending),
        Some("slow_down") => Ok(DeviceCodePoll::SlowDown),
        _ => Ok(DeviceCodePoll::Failed(
            match telemetry::sanitize_oauth_body(&body) {
                Some(detail) => format!("OpenAI Codex device auth failed ({status}): {detail}"),
                None => format!("OpenAI Codex device auth failed ({status})"),
            },
        )),
    }
}

fn exchange_authorization_code(
    client: &Client,
    code: &str,
    code_verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthCredentials> {
    let response = client
        .post(TOKEN_URL)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", code_verifier),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .context("failed to exchange OpenAI Codex authorization code")?;

    read_token_response(response, "exchange")
}

fn refresh_access_token(client: &Client, refresh_token: &str) -> Result<OAuthCredentials> {
    let response = client
        .post(TOKEN_URL)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .context("failed to refresh OpenAI Codex token")?;

    read_token_response(response, "refresh")
}

fn read_token_response(
    response: reqwest::blocking::Response,
    operation: &str,
) -> Result<OAuthCredentials> {
    let status = response.status();
    if !status.is_success() {
        // Token-endpoint bodies are the highest-risk surface; omit entirely.
        let _ = response.text();
        bail!("OpenAI Codex token {operation} failed ({status})");
    }

    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: String,
        refresh_token: String,
        expires_in: u128,
    }

    let token: TokenResponse = response
        .json()
        .with_context(|| format!("failed to parse token {operation} response"))?;
    Ok(OAuthCredentials {
        access: token.access_token,
        refresh: token.refresh_token,
        expires: now_millis() + token.expires_in * 1000,
        extra: serde_json::Map::new(),
    })
}

fn extract_account_id(token: &str) -> Result<String> {
    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("invalid OAuth access token"))?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .context("OAuth access token payload is not base64url")?;
    let value: Value =
        serde_json::from_slice(&decoded).context("OAuth access token payload is not JSON")?;
    value
        .get(ACCOUNT_ID_CLAIM)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("OAuth access token is missing chatgpt_account_id"))
}

fn parse_interval_seconds(value: Option<&Value>) -> Result<Option<u64>> {
    match value {
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| anyhow!("invalid OpenAI Codex device code interval")),
        Some(Value::String(text)) => text
            .trim()
            .parse::<u64>()
            .map(Some)
            .context("invalid OpenAI Codex device code interval"),
        Some(_) => bail!("invalid OpenAI Codex device code interval"),
        None => Ok(None),
    }
}

fn error_code(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    match value.get("error")? {
        Value::String(code) => Some(code.clone()),
        Value::Object(error) => error
            .get("code")
            .and_then(Value::as_str)
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

    fn credentials(access: &str, expires: u128) -> OAuthCredentials {
        OAuthCredentials {
            access: access.to_string(),
            refresh: "refresh-token".to_string(),
            expires,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn refresh_after_lock_reuses_a_peer_refreshed_token() {
        // Concurrent workers (live: an 8-worker group spawn) must not race the
        // rotating refresh token; only the first caller through the lock may
        // refresh, the rest reuse its result.
        let fresh = credentials("rotated-token", u128::MAX);
        // access_token: a peer's refresh made the stored token valid again.
        assert!(!needs_refresh(None, &fresh, 1_000));
        // access_token: still expired after waiting -> refresh.
        assert!(needs_refresh(None, &credentials("old", 500), 1_000));
        // force_refresh: the rejected token is still stored -> refresh.
        assert!(needs_refresh(
            Some("rejected"),
            &credentials("rejected", u128::MAX),
            1_000
        ));
        // force_refresh: a peer already rotated the rejected token -> reuse.
        assert!(!needs_refresh(Some("rejected"), &fresh, 1_000));
    }

    #[test]
    fn extracts_account_id_from_jwt_payload() -> Result<()> {
        let payload = r#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acc_test"}}"#;
        let token = format!("aaa.{}.bbb", URL_SAFE_NO_PAD.encode(payload.as_bytes()));
        assert_eq!(extract_account_id(&token)?, "acc_test");
        Ok(())
    }

    #[test]
    fn builds_browser_authorization_url() -> Result<()> {
        let url = Url::parse(&authorization_url("challenge", "state")?)?;
        let pairs = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(url.as_str().split('?').next(), Some(AUTHORIZE_URL));
        assert_eq!(
            pairs.get("client_id").map(|value| value.as_ref()),
            Some(CLIENT_ID)
        );
        assert_eq!(
            pairs.get("redirect_uri").map(|value| value.as_ref()),
            Some(BROWSER_REDIRECT_URI)
        );
        assert_eq!(
            pairs.get("code_challenge").map(|value| value.as_ref()),
            Some("challenge")
        );
        assert_eq!(
            pairs.get("state").map(|value| value.as_ref()),
            Some("state")
        );
        Ok(())
    }

    #[test]
    fn parses_string_interval() -> Result<()> {
        assert_eq!(
            parse_interval_seconds(Some(&Value::String("7".to_string())))?,
            Some(7)
        );
        Ok(())
    }

    #[test]
    fn extracts_error_code_from_string_or_object() {
        assert_eq!(
            error_code(r#"{"error":"slow_down"}"#).as_deref(),
            Some("slow_down")
        );
        assert_eq!(
            error_code(r#"{"error":{"code":"deviceauth_authorization_pending"}}"#).as_deref(),
            Some("deviceauth_authorization_pending")
        );
    }
}
