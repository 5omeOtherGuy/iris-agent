//! Anthropic "Claude Code subscription" auth: reads the Claude Code OAuth token
//! (from the Iris auth store, or bootstrapped from Claude Code's own credential
//! file) and refreshes it near expiry, persisting the rotated token back to the
//! same source so a stale refresh token never locks the user out of Claude Code.
//!
//! ponytail: only the Claude Code subscription OAuth lane (no x-api-key, no
//! login flow here, no thinking replay). Login is owned elsewhere; this module
//! only loads, refreshes, and writes back the token from whichever source held
//! it.

use std::env;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client;
use serde_json::{Map, Value, json};

use crate::mimir::auth::storage::{AuthStore, OAuthCredentials};

/// Auth-store provider key for Claude Code subscription OAuth credentials.
pub(crate) const AUTH_PROVIDER: &str = "anthropic";

const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const REFRESH_SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";
/// Refresh this far ahead of expiry so an in-flight request never races the
/// token going stale.
const REFRESH_MARGIN_MS: u128 = 300_000;

/// Where a loaded token came from, so a refreshed token is written back to the
/// same place (data-integrity critical: the refresh token rotates).
#[derive(Debug, Clone)]
enum CredentialSource {
    IrisStore,
    ClaudeCodeFile(PathBuf),
}

#[derive(Debug, Clone)]
pub(crate) struct AnthropicTokenStore {
    storage: AuthStore,
}

impl AnthropicTokenStore {
    pub(crate) fn from_env() -> Result<Self> {
        Ok(Self {
            storage: AuthStore::from_env()?,
        })
    }

    /// Return the current OAuth bearer access token, refreshing (and persisting)
    /// it first when it is at/near expiry.
    pub(crate) fn access_token(&self, client: &Client) -> Result<String> {
        let (credentials, source) = self.load()?;
        if credentials.expires <= now_millis() + REFRESH_MARGIN_MS {
            let refreshed = refresh_access_token(client, &credentials.refresh)?;
            self.persist(&source, &refreshed)?;
            return Ok(refreshed.access);
        }
        Ok(credentials.access)
    }

    /// Force a token refresh regardless of cached expiry (used after an HTTP
    /// 401/403), persist it back to its source, and return the new bearer token.
    pub(crate) fn force_refresh(&self, client: &Client) -> Result<String> {
        let (credentials, source) = self.load()?;
        let refreshed = refresh_access_token(client, &credentials.refresh)?;
        self.persist(&source, &refreshed)?;
        Ok(refreshed.access)
    }

    /// Load credentials, preferring the Iris store and bootstrapping from the
    /// Claude Code credential file when the store has none.
    fn load(&self) -> Result<(OAuthCredentials, CredentialSource)> {
        if let Ok(credentials) = self.storage.oauth_credentials(AUTH_PROVIDER) {
            return Ok((credentials, CredentialSource::IrisStore));
        }
        let path = claude_code_credentials_path()?;
        let credentials = read_claude_code_file(&path)?;
        Ok((credentials, CredentialSource::ClaudeCodeFile(path)))
    }

    fn persist(&self, source: &CredentialSource, credentials: &OAuthCredentials) -> Result<()> {
        match source {
            CredentialSource::IrisStore => self
                .storage
                .set_oauth_credentials(AUTH_PROVIDER, credentials.clone()),
            CredentialSource::ClaudeCodeFile(path) => write_claude_code_file(path, credentials),
        }
    }
}

/// Whether a Claude Code credential file exists to bootstrap from. Used by the
/// model catalog to mark Anthropic available even when Iris's own auth store has
/// no stored credential. Only checks for the file's presence -- it never reads,
/// parses, or exposes the secret.
pub(crate) fn claude_code_credentials_available() -> bool {
    claude_code_credentials_path()
        .map(|path| path.exists())
        .unwrap_or(false)
}

fn claude_code_credentials_path() -> Result<PathBuf> {
    if let Ok(dir) = env::var("CLAUDE_CONFIG_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Ok(Path::new(dir).join(".credentials.json"));
        }
    }
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(Path::new(&home).join(".claude/.credentials.json"))
}

/// Read the Claude Code credential file, tolerating both the nested
/// `{"claudeAiOauth":{...}}` shape and a flat object.
fn read_claude_code_file(path: &Path) -> Result<OAuthCredentials> {
    let raw = fs::read_to_string(path).with_context(|| {
        format!(
            "failed to read Claude Code credentials at {}",
            path.display()
        )
    })?;
    let value: Value = serde_json::from_str(&raw).with_context(|| {
        format!(
            "failed to parse Claude Code credentials at {}",
            path.display()
        )
    })?;
    parse_claude_code_credentials(&value)
}

fn parse_claude_code_credentials(value: &Value) -> Result<OAuthCredentials> {
    let oauth = value.get("claudeAiOauth").unwrap_or(value);
    let access = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Claude Code credentials missing accessToken"))?;
    let refresh = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Claude Code credentials missing refreshToken"))?;
    let expires = oauth
        .get("expiresAt")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("Claude Code credentials missing expiresAt"))?;
    Ok(OAuthCredentials {
        access: access.to_string(),
        refresh: refresh.to_string(),
        expires: u128::from(expires),
        extra: Map::new(),
    })
}

/// Write the rotated token back into the Claude Code file, updating only the
/// three credential fields IN PLACE so every other key the user has (nested
/// `claudeAiOauth` siblings like scopes/subscriptionType, or unrelated root
/// keys) is preserved, and the file's existing shape (nested vs flat) is kept.
/// Atomic (tmp + rename) and 0600 -- a stale refresh token here would lock the
/// user out of Claude Code, so this must never drop or reshape their config.
fn write_claude_code_file(path: &Path, credentials: &OAuthCredentials) -> Result<()> {
    // Preserve the whole existing document; default to a nested envelope only
    // when the file is absent or not a JSON object.
    let mut document = match fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    {
        Some(value) if value.is_object() => value,
        _ => json!({ "claudeAiOauth": {} }),
    };
    let root = document
        .as_object_mut()
        .expect("document is a JSON object by construction");
    // Update the credential fields wherever they live: under `claudeAiOauth`
    // when that envelope exists, otherwise at the (flat) root.
    let target = if root.contains_key("claudeAiOauth") {
        root.get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| anyhow!("Claude Code claudeAiOauth field is not an object"))?
    } else {
        root
    };
    target.insert("accessToken".to_string(), json!(credentials.access));
    target.insert("refreshToken".to_string(), json!(credentials.refresh));
    target.insert("expiresAt".to_string(), json!(credentials.expires as u64));
    let raw = serde_json::to_string_pretty(&document)?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp_path = unique_tmp_path(path);
    write_secret_file(&tmp_path, &format!("{raw}\n"))?;
    fs::rename(&tmp_path, path).with_context(|| format!("failed to replace {}", path.display()))
}

fn unique_tmp_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "tmp-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ))
}

/// Create (or truncate) a file containing secret material and write `contents`.
/// On Unix the file is created with 0600 from the start, closing the TOCTOU
/// window where a default-umask (0644) temp file briefly exposes the token to
/// other local users before a later chmod.
fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))
}

fn refresh_access_token(client: &Client, refresh_token: &str) -> Result<OAuthCredentials> {
    let response = client
        .post(TOKEN_URL)
        .header("anthropic-beta", "oauth-2025-04-20")
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
            "scope": REFRESH_SCOPE,
        }))
        .send()
        .context("failed to refresh Anthropic token")?;

    let status = response.status();
    if !status.is_success() {
        // Token-endpoint bodies are the highest-risk surface; omit entirely.
        let _ = response.text();
        bail!("Anthropic token refresh failed ({status})");
    }
    let body: Value = response
        .json()
        .context("failed to parse Anthropic token refresh response")?;
    parse_refresh_response(&body, refresh_token)
}

/// Parse the refresh response; reuse the old refresh token when the server
/// omits a rotated one. Pure so the parsing is unit-tested without network.
fn parse_refresh_response(body: &Value, old_refresh: &str) -> Result<OAuthCredentials> {
    let access = body
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("token refresh response missing access_token"))?;
    let expires_in = body
        .get("expires_in")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("token refresh response missing expires_in"))?;
    let refresh = body
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| old_refresh.to_string());
    Ok(OAuthCredentials {
        access: access.to_string(),
        refresh,
        expires: now_millis() + u128::from(expires_in) * 1000,
        extra: Map::new(),
    })
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
    fn parses_nested_claude_code_credentials() -> Result<()> {
        let value = json!({
            "claudeAiOauth": {
                "accessToken": "acc",
                "refreshToken": "ref",
                "expiresAt": 1_700_000_000_000_u64,
                "scopes": [],
            }
        });
        let creds = parse_claude_code_credentials(&value)?;
        assert_eq!(creds.access, "acc");
        assert_eq!(creds.refresh, "ref");
        assert_eq!(creds.expires, 1_700_000_000_000);
        Ok(())
    }

    #[test]
    fn refresh_response_yields_future_expiry() -> Result<()> {
        let body = json!({
            "access_token": "new-access",
            "refresh_token": "new-refresh",
            "expires_in": 3600,
        });
        let creds = parse_refresh_response(&body, "old-refresh")?;
        assert_eq!(creds.access, "new-access");
        assert_eq!(creds.refresh, "new-refresh");
        assert!(
            creds.expires > now_millis(),
            "expiry should be in the future"
        );
        Ok(())
    }

    #[test]
    fn refresh_response_reuses_old_refresh_when_omitted() -> Result<()> {
        let body = json!({
            "access_token": "new-access",
            "expires_in": 3600,
        });
        let creds = parse_refresh_response(&body, "old-refresh")?;
        assert_eq!(creds.refresh, "old-refresh");
        Ok(())
    }

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("iris-cc-cred-{tag}-{nanos}.json"))
    }

    fn new_creds() -> OAuthCredentials {
        OAuthCredentials {
            access: "new-acc".to_string(),
            refresh: "new-ref".to_string(),
            expires: 1_800_000_000_000,
            extra: Map::new(),
        }
    }

    #[test]
    fn unique_tmp_path_is_not_the_static_tmp_sibling() {
        let path = Path::new("/tmp/.credentials.json");
        let tmp = unique_tmp_path(path);
        assert_ne!(tmp, path.with_extension("tmp"));
        assert!(
            tmp.extension()
                .unwrap()
                .to_string_lossy()
                .starts_with("tmp-")
        );
    }

    #[test]
    fn write_back_preserves_nested_siblings_and_root_keys() -> Result<()> {
        let path = temp_path("nested");
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "claudeAiOauth": {
                    "accessToken": "old",
                    "refreshToken": "old",
                    "expiresAt": 1_u64,
                    "scopes": ["user:inference"],
                    "subscriptionType": "max",
                },
                "otherTool": { "keep": true },
            }))?,
        )?;

        write_claude_code_file(&path, &new_creds())?;

        let back: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        let oauth = &back["claudeAiOauth"];
        assert_eq!(oauth["accessToken"], json!("new-acc"));
        assert_eq!(oauth["refreshToken"], json!("new-ref"));
        assert_eq!(oauth["expiresAt"], json!(1_800_000_000_000_u64));
        // Sibling fields and unrelated root keys must survive the rewrite.
        assert_eq!(oauth["scopes"], json!(["user:inference"]));
        assert_eq!(oauth["subscriptionType"], json!("max"));
        assert_eq!(back["otherTool"], json!({ "keep": true }));
        fs::remove_file(&path).ok();
        Ok(())
    }

    #[test]
    fn write_back_keeps_flat_shape() -> Result<()> {
        let path = temp_path("flat");
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "accessToken": "old",
                "refreshToken": "old",
                "expiresAt": 1_u64,
                "scopes": ["x"],
            }))?,
        )?;

        write_claude_code_file(&path, &new_creds())?;

        let back: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
        // Flat stays flat (no claudeAiOauth envelope introduced) and siblings survive.
        assert!(
            back.get("claudeAiOauth").is_none(),
            "shape preserved as flat"
        );
        assert_eq!(back["accessToken"], json!("new-acc"));
        assert_eq!(back["refreshToken"], json!("new-ref"));
        assert_eq!(back["scopes"], json!(["x"]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        }
        fs::remove_file(&path).ok();
        Ok(())
    }
}
