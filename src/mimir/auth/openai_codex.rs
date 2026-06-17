use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use reqwest::Url;
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::mimir::auth::device_code::{DeviceCodePoll, poll_device_code};
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
const BROWSER_CALLBACK_ADDR: &str = "127.0.0.1:1455";
const SCOPE: &str = "openid profile email offline_access";
const DEVICE_CODE_TIMEOUT_SECONDS: u64 = 15 * 60;
const ACCOUNT_ID_CLAIM: &str = "https://api.openai.com/auth";

#[derive(Debug, Clone)]
pub(crate) struct OpenAiCodexTokenStore {
    storage: AuthStore,
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
            credentials = refresh_access_token(client, &credentials.refresh)?;
            self.storage
                .set_oauth_credentials(AUTH_PROVIDER, credentials.clone())?;
        }

        let account_id = extract_account_id(&credentials.access)?;
        Ok(AccessToken {
            bearer: credentials.access,
            account_id,
        })
    }

    /// Refresh the access token unconditionally and persist the result.
    ///
    /// Used when the provider receives an auth rejection (HTTP 401/403) even
    /// though the locally cached token had not yet expired, so a single forced
    /// refresh can recover a server-side-invalidated token.
    pub(crate) fn force_refresh(&self, client: &Client) -> Result<AccessToken> {
        let credentials = self.storage.oauth_credentials(AUTH_PROVIDER)?;
        let refreshed = refresh_access_token(client, &credentials.refresh)?;
        self.storage
            .set_oauth_credentials(AUTH_PROVIDER, refreshed.clone())?;
        let account_id = extract_account_id(&refreshed.access)?;
        Ok(AccessToken {
            bearer: refreshed.access,
            account_id,
        })
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

pub(crate) fn login_browser(client: &Client, on_auth: impl FnOnce(BrowserLoginInfo)) -> Result<()> {
    let listener = TcpListener::bind(BROWSER_CALLBACK_ADDR)
        .context("failed to start OpenAI Codex OAuth callback server")?;
    let pkce = create_pkce();
    let state = random_url_token(16);
    let url = authorization_url(&pkce.challenge, &state)?;

    on_auth(BrowserLoginInfo {
        url,
        redirect_uri: BROWSER_REDIRECT_URI.to_string(),
    });

    let code = wait_for_browser_code(listener, &state)?;
    let credentials =
        exchange_authorization_code(client, &code, &pkce.verifier, BROWSER_REDIRECT_URI)?;
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

fn random_url_token(byte_len: usize) -> String {
    let mut bytes = vec![0_u8; byte_len];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
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

fn wait_for_browser_code(listener: TcpListener, expected_state: &str) -> Result<String> {
    for stream in listener.incoming() {
        let mut stream = stream.context("failed to receive OpenAI Codex OAuth callback")?;
        let request = read_http_request(&mut stream)?;
        match parse_callback_request(&request, expected_state)? {
            Some(code) => {
                write_callback_response(
                    &mut stream,
                    200,
                    "OpenAI authentication completed. You can close this window.",
                )?;
                return Ok(code);
            }
            None => {
                write_callback_response(&mut stream, 404, "Not found.")?;
            }
        }
    }
    bail!("OpenAI Codex OAuth callback server stopped before receiving a code")
}

fn read_http_request(stream: &mut TcpStream) -> Result<String> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let len = stream
            .read(&mut buffer)
            .context("failed to read OpenAI Codex OAuth callback")?;
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
        .context("failed to write OpenAI Codex OAuth callback response")
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
    if url.path() != "/auth/callback" {
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
        match telemetry::sanitize_external_body(&body) {
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
            match telemetry::sanitize_external_body(&body) {
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
    fn parses_browser_callback_request() -> Result<()> {
        let request =
            "GET /auth/callback?code=abc123&state=state123 HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(
            parse_callback_request(request, "state123")?,
            Some("abc123".to_string())
        );
        Ok(())
    }

    #[test]
    fn parses_absolute_browser_callback_request() -> Result<()> {
        let request = "GET http://localhost:1455/auth/callback?code=abc123&state=state123 HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(
            parse_callback_request(request, "state123")?,
            Some("abc123".to_string())
        );
        Ok(())
    }

    #[test]
    fn ignores_unrelated_browser_requests() -> Result<()> {
        let request = "GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(parse_callback_request(request, "state123")?, None);
        Ok(())
    }

    #[test]
    fn rejects_browser_callback_state_mismatch() {
        let request =
            "GET /auth/callback?code=abc123&state=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let error = parse_callback_request(request, "state123")
            .unwrap_err()
            .to_string();
        assert!(error.contains("state mismatch"));
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
