use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::Value;

use crate::auth::device_code::{DeviceCodePoll, poll_device_code};
use crate::auth::storage::{AuthStore, OAuthCredentials};

const AUTH_PROVIDER: &str = "openai-codex";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEVICE_USER_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
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
    let credentials =
        exchange_authorization_code(client, &code.authorization_code, &code.code_verifier)?;
    AuthStore::from_env()?.set_oauth_credentials(AUTH_PROVIDER, credentials)
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
        bail!("OpenAI Codex device code request failed ({status}): {body}");
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
        _ => Ok(DeviceCodePoll::Failed(format!(
            "OpenAI Codex device auth failed ({status}): {body}"
        ))),
    }
}

fn exchange_authorization_code(
    client: &Client,
    code: &str,
    code_verifier: &str,
) -> Result<OAuthCredentials> {
    let response = client
        .post(TOKEN_URL)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", code_verifier),
            ("redirect_uri", DEVICE_REDIRECT_URI),
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
        let body = response.text().unwrap_or_default();
        bail!("OpenAI Codex token {operation} failed ({status}): {body}");
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
