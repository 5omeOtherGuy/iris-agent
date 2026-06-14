use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const AUTH_PROVIDER: &str = "openai-codex";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ACCOUNT_ID_CLAIM: &str = "https://api.openai.com/auth";

#[derive(Debug, Clone)]
pub(crate) struct OpenAiCodexTokenStore {
    path: PathBuf,
}

impl OpenAiCodexTokenStore {
    pub(crate) fn from_env() -> Result<Self> {
        let home = env::var("HOME").context("HOME is not set")?;
        let path = env::var("IRIS_AUTH_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Path::new(&home).join(".iris/auth.json"));
        Ok(Self { path })
    }

    pub(crate) fn access_token(&self, client: &Client) -> Result<AccessToken> {
        let mut auth = AuthFile::read(&self.path)?;
        let mut credentials = auth.openai_codex().with_context(|| {
            format!(
                "failed to load {AUTH_PROVIDER} credentials from {}",
                self.path.display()
            )
        })?;

        if credentials.expires <= now_millis() {
            credentials = refresh_access_token(client, &credentials.refresh)?;
            auth.set_openai_codex(credentials.clone())?;
            auth.write(&self.path)?;
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct OAuthCredentials {
    access: String,
    refresh: String,
    expires: u128,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthFile {
    #[serde(flatten)]
    providers: serde_json::Map<String, Value>,
}

impl AuthFile {
    fn read(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
    }

    fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let raw = serde_json::to_string_pretty(self)?;
        let tmp_path = path.with_extension("tmp");
        fs::write(&tmp_path, format!("{raw}\n"))
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        restrict_file_permissions(&tmp_path)?;
        fs::rename(&tmp_path, path).with_context(|| format!("failed to replace {}", path.display()))
    }

    fn openai_codex(&self) -> Result<OAuthCredentials> {
        let value = self
            .providers
            .get(AUTH_PROVIDER)
            .ok_or_else(|| anyhow!("missing {AUTH_PROVIDER} credentials"))?
            .clone();
        if value.get("type").and_then(Value::as_str) != Some("oauth") {
            bail!("{AUTH_PROVIDER} credentials are not OAuth credentials");
        }
        serde_json::from_value(value).context("malformed openai-codex OAuth credentials")
    }

    fn set_openai_codex(&mut self, credentials: OAuthCredentials) -> Result<()> {
        let mut value =
            serde_json::to_value(credentials).context("failed to serialize OAuth credentials")?;
        if let Value::Object(object) = &mut value {
            object.insert("type".to_string(), Value::String("oauth".to_string()));
        }
        self.providers.insert(AUTH_PROVIDER.to_string(), value);
        Ok(())
    }
}

fn restrict_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to restrict permissions for {}", path.display()))?;
    }
    Ok(())
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

    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        bail!("OpenAI Codex token refresh failed ({status}): {body}");
    }

    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: String,
        refresh_token: String,
        expires_in: u128,
    }

    let token: TokenResponse = response
        .json()
        .context("failed to parse token refresh response")?;
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
    fn reads_openai_codex_credentials_from_iris_auth_shape() -> Result<()> {
        let auth: AuthFile = serde_json::from_str(&format!(
            r#"{{"openai-codex":{{"type":"oauth","access":"{}","refresh":"r","expires":9999999999999,"accountId":"acc_test"}}}}"#,
            jwt("acc_test")
        ))?;
        let credentials = auth.openai_codex()?;
        assert_eq!(extract_account_id(&credentials.access)?, "acc_test");
        assert_eq!(credentials.refresh, "r");
        Ok(())
    }

    #[test]
    fn reports_malformed_openai_codex_credentials() -> Result<()> {
        let auth: AuthFile = serde_json::from_str(
            r#"{"openai-codex":{"type":"oauth","access":"aaa.bbb.ccc","expires":1}}"#,
        )?;
        let error = auth.openai_codex().unwrap_err().to_string();
        assert!(error.contains("malformed openai-codex OAuth credentials"));
        Ok(())
    }

    #[test]
    fn writes_auth_file_atomically_with_restricted_permissions() -> Result<()> {
        let dir = unique_test_dir()?;
        let path = dir.join("auth.json");
        let mut auth = AuthFile::default();
        auth.set_openai_codex(OAuthCredentials {
            access: jwt("acc_test"),
            refresh: "refresh".to_string(),
            expires: 9999999999999,
            extra: serde_json::Map::new(),
        })?;

        auth.write(&path)?;

        let written = fs::read_to_string(&path)?;
        assert!(written.contains("openai-codex"));
        assert!(!path.with_extension("tmp").exists());
        #[cfg(unix)]
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    fn unique_test_dir() -> Result<PathBuf> {
        let path = env::temp_dir().join(format!("iris-oauth-test-{}", now_millis()));
        fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn jwt(account_id: &str) -> String {
        let payload =
            format!(r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#);
        format!("aaa.{}.bbb", URL_SAFE_NO_PAD.encode(payload.as_bytes()))
    }
}
