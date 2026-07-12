use std::env;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Auth-file id for the Brave Search service key (a web-tool service
/// credential, not a chat provider).
pub(crate) const BRAVE_SERVICE_ID: &str = "brave-search";
/// Auth-file id for the Jina service key.
pub(crate) const JINA_SERVICE_ID: &str = "jina";
/// Environment fallback for the Brave Search key.
pub(crate) const BRAVE_ENV_VAR: &str = "BRAVE_API_KEY";
/// Environment fallback for the Jina key.
pub(crate) const JINA_ENV_VAR: &str = "JINA_API_KEY";

#[derive(Debug, Clone)]
pub(crate) struct AuthStore {
    path: PathBuf,
}

impl AuthStore {
    pub(crate) fn from_env() -> Result<Self> {
        // An explicit IRIS_AUTH_PATH wins and must not require HOME (mirrors
        // config::global_path), so resolve it before falling back to ~/.iris.
        let path = match env::var("IRIS_AUTH_PATH") {
            Ok(path) => PathBuf::from(path),
            Err(_) => {
                let home = env::var("HOME").context("HOME is not set")?;
                Path::new(&home).join(".iris/auth.json")
            }
        };
        Ok(Self { path })
    }

    /// Construct a store over an explicit auth-file path (used by tests and any
    /// caller that already resolved the path).
    #[cfg(test)]
    pub(crate) fn from_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// The auth-file path. Used to key the per-source refresh lock so concurrent
    /// refreshes of the same store coalesce; carries no secret material.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn oauth_credentials(&self, provider_id: &str) -> Result<OAuthCredentials> {
        AuthFile::read_or_default(&self.path)?
            .oauth_credentials(provider_id)
            .with_context(|| {
                format!(
                    "failed to load {provider_id} credentials from {}",
                    self.path.display()
                )
            })
    }

    pub(crate) fn api_key_credentials(&self, provider_id: &str) -> Result<ApiKeyCredentials> {
        AuthFile::read_or_default(&self.path)?
            .api_key_credentials(provider_id)
            .with_context(|| {
                format!(
                    "failed to load {provider_id} API-key credentials from {}",
                    self.path.display()
                )
            })
    }

    pub(crate) fn set_api_key_credentials(&self, provider_id: &str, key: &str) -> Result<()> {
        let mut auth = AuthFile::read_or_default(&self.path)?;
        auth.set_api_key_credentials(provider_id, key)?;
        auth.write(&self.path)
    }

    /// Resolve a web-tool SERVICE API key (Brave/Jina), NOT a chat provider.
    /// The stored key wins over the environment fallback (same precedence as
    /// `api_key_for_provider`); `None` when neither is set or both are blank.
    /// `service_id` is one of [`BRAVE_SERVICE_ID`] / [`JINA_SERVICE_ID`]; these
    /// live in the same auth file under a plain string id (never a
    /// `ProviderId`) so they cannot masquerade as a chat provider.
    pub(crate) fn service_api_key(&self, service_id: &str, env_var: &str) -> Option<String> {
        if let Ok(creds) = self.api_key_credentials(service_id) {
            let key = creds.key.trim();
            if !key.is_empty() {
                return Some(key.to_string());
            }
        }
        std::env::var(env_var).ok().and_then(|value| {
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        })
    }

    pub(crate) fn credential_kind(&self, provider_id: &str) -> Result<Option<CredentialKind>> {
        Ok(AuthFile::read_or_default(&self.path)?
            .providers
            .get(provider_id)
            .map(CredentialKind::from_value))
    }

    pub(crate) fn set_oauth_credentials(
        &self,
        provider_id: &str,
        credentials: OAuthCredentials,
    ) -> Result<()> {
        let mut auth = AuthFile::read_or_default(&self.path)?;
        auth.set_oauth_credentials(provider_id, credentials)?;
        auth.write(&self.path)
    }

    /// Whether a credential of any kind is stored for `provider_id` in the auth
    /// file. Used by tests to assert removal semantics without reading secret material.
    #[cfg(test)]
    pub(crate) fn has_credentials(&self, provider_id: &str) -> Result<bool> {
        Ok(AuthFile::read_or_default(&self.path)?
            .providers
            .contains_key(provider_id))
    }

    /// List the providers with stored credentials and the credential kind, for
    /// the `/logout` selector. Returns no secret values -- only the provider id
    /// and whether the stored entry is an OAuth or API-key credential, so the
    /// caller can phrase the right "logged out" vs "removed API key" message.
    pub(crate) fn stored_credentials(&self) -> Result<Vec<StoredCredential>> {
        let auth = AuthFile::read_or_default(&self.path)?;
        let mut stored: Vec<StoredCredential> = auth
            .providers
            .iter()
            .map(|(provider_id, value)| StoredCredential {
                provider_id: provider_id.clone(),
                kind: CredentialKind::from_value(value),
            })
            .collect();
        stored.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
        Ok(stored)
    }

    /// Remove the stored credential for `provider_id`, returning whether an entry
    /// existed. Only the auth-file entry is touched; environment variables and
    /// any external (e.g. Claude Code) credentials are left untouched. A missing
    /// auth file is treated as "nothing to remove".
    pub(crate) fn remove_credentials(&self, provider_id: &str) -> Result<bool> {
        let mut auth = AuthFile::read_or_default(&self.path)?;
        if auth.providers.remove(provider_id).is_none() {
            return Ok(false);
        }
        auth.write(&self.path)?;
        Ok(true)
    }
}

/// A stored credential's provider and kind, with no secret material attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredCredential {
    pub(crate) provider_id: String,
    pub(crate) kind: CredentialKind,
}

/// The kind of a stored credential, inferred from its `type` field. Used only to
/// pick the right user-facing message; never carries the secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialKind {
    OAuth,
    ApiKey,
    Unknown,
}

impl CredentialKind {
    fn from_value(value: &Value) -> Self {
        match value.get("type").and_then(Value::as_str) {
            Some("oauth") => CredentialKind::OAuth,
            Some("api_key") => CredentialKind::ApiKey,
            _ => CredentialKind::Unknown,
        }
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct OAuthCredentials {
    pub(crate) access: String,
    pub(crate) refresh: String,
    pub(crate) expires: u128,
    #[serde(flatten)]
    pub(crate) extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ApiKeyCredentials {
    pub(crate) key: String,
}

impl std::fmt::Debug for OAuthCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthCredentials")
            .field("access", &"<redacted>")
            .field("refresh", &"<redacted>")
            .field("expires", &self.expires)
            .field("extra", &self.extra)
            .finish()
    }
}

impl std::fmt::Debug for ApiKeyCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyCredentials")
            .field("key", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AuthFile {
    #[serde(flatten)]
    providers: serde_json::Map<String, Value>,
}

impl AuthFile {
    fn read_or_default(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let raw = serde_json::to_string_pretty(self)?;
        let tmp_path = unique_tmp_path(path);
        write_secret_file(&tmp_path, &format!("{raw}\n"))?;
        fs::rename(&tmp_path, path).with_context(|| format!("failed to replace {}", path.display()))
    }

    fn oauth_credentials(&self, provider_id: &str) -> Result<OAuthCredentials> {
        let value = self
            .providers
            .get(provider_id)
            .ok_or_else(|| anyhow!("missing {provider_id} credentials"))?
            .clone();
        if value.get("type").and_then(Value::as_str) != Some("oauth") {
            bail!("{provider_id} credentials are not OAuth credentials");
        }
        serde_json::from_value(value)
            .with_context(|| format!("malformed {provider_id} OAuth credentials"))
    }

    fn api_key_credentials(&self, provider_id: &str) -> Result<ApiKeyCredentials> {
        let value = self
            .providers
            .get(provider_id)
            .ok_or_else(|| anyhow!("missing {provider_id} credentials"))?
            .clone();
        if value.get("type").and_then(Value::as_str) != Some("api_key") {
            bail!("{provider_id} credentials are not API-key credentials");
        }
        serde_json::from_value(value)
            .with_context(|| format!("malformed {provider_id} API-key credentials"))
    }

    fn set_api_key_credentials(&mut self, provider_id: &str, key: &str) -> Result<()> {
        let key = key.trim();
        if key.is_empty() {
            bail!("API key is blank");
        }
        let mut value = serde_json::to_value(ApiKeyCredentials {
            key: key.to_string(),
        })
        .context("failed to serialize API-key credentials")?;
        if let Value::Object(object) = &mut value {
            object.insert("type".to_string(), Value::String("api_key".to_string()));
        }
        self.providers.insert(provider_id.to_string(), value);
        Ok(())
    }

    fn set_oauth_credentials(
        &mut self,
        provider_id: &str,
        credentials: OAuthCredentials,
    ) -> Result<()> {
        let mut value =
            serde_json::to_value(credentials).context("failed to serialize OAuth credentials")?;
        if let Value::Object(object) = &mut value {
            object.insert("type".to_string(), Value::String("oauth".to_string()));
        }
        self.providers.insert(provider_id.to_string(), value);
        Ok(())
    }
}

fn unique_tmp_path(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "tmp-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ))
}

fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(contents.as_bytes())
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn reads_provider_keyed_oauth_credentials() -> Result<()> {
        let auth: AuthFile = serde_json::from_str(&format!(
            r#"{{"openai-codex":{{"type":"oauth","access":"{}","refresh":"r","expires":9999999999999,"accountId":"acc_test"}}}}"#,
            jwt("acc_test")
        ))?;
        let credentials = auth.oauth_credentials("openai-codex")?;
        assert_eq!(credentials.refresh, "r");
        Ok(())
    }

    #[test]
    fn reports_malformed_oauth_credentials() -> Result<()> {
        let auth: AuthFile = serde_json::from_str(
            r#"{"openai-codex":{"type":"oauth","access":"aaa.bbb.ccc","expires":1}}"#,
        )?;
        let error = auth
            .oauth_credentials("openai-codex")
            .unwrap_err()
            .to_string();
        assert!(error.contains("malformed openai-codex OAuth credentials"));
        Ok(())
    }

    #[test]
    fn unique_tmp_path_is_not_the_static_tmp_sibling() {
        let path = Path::new("/tmp/auth.json");
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
    fn writes_auth_file_atomically_with_restricted_permissions() -> Result<()> {
        let dir = unique_test_dir()?;
        let path = dir.join("auth.json");
        let mut auth = AuthFile::default();
        auth.set_oauth_credentials(
            "openai-codex",
            OAuthCredentials {
                access: jwt("acc_test"),
                refresh: "refresh".to_string(),
                expires: 9999999999999,
                extra: serde_json::Map::new(),
            },
        )?;

        auth.write(&path)?;

        let written = fs::read_to_string(&path)?;
        assert!(written.contains("openai-codex"));
        assert!(!path.with_extension("tmp").exists());
        #[cfg(unix)]
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn stores_reads_and_rejects_api_key_credentials() -> Result<()> {
        let dir = unique_test_dir()?;
        let path = dir.join("auth.json");
        let store = AuthStore { path: path.clone() };

        store.set_api_key_credentials("openai", "sk-live-secret")?;
        assert_eq!(store.api_key_credentials("openai")?.key, "sk-live-secret");
        assert_eq!(
            store.credential_kind("openai")?,
            Some(CredentialKind::ApiKey)
        );

        let written = fs::read_to_string(&path)?;
        assert!(written.contains(r#""type": "api_key""#));
        assert!(written.contains(r#""key": "sk-live-secret""#));
        #[cfg(unix)]
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);

        let error = store
            .set_api_key_credentials("openai", "   ")
            .unwrap_err()
            .to_string();
        assert!(error.contains("API key is blank"), "{error}");

        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn stored_oauth_credentials_are_not_api_key_credentials() -> Result<()> {
        let mut auth = AuthFile::default();
        auth.set_oauth_credentials(
            "openai",
            OAuthCredentials {
                access: "access".to_string(),
                refresh: "refresh".to_string(),
                expires: 9,
                extra: serde_json::Map::new(),
            },
        )?;
        let error = auth.api_key_credentials("openai").unwrap_err().to_string();
        assert!(error.contains("not API-key credentials"), "{error}");
        Ok(())
    }

    #[test]
    fn lists_and_removes_only_stored_credentials_without_secrets() -> Result<()> {
        let dir = unique_test_dir()?;
        let path = dir.join("auth.json");
        fs::write(
            &path,
            r#"{
              "openai-codex": {"type": "oauth", "access": "a", "refresh": "r", "expires": 9},
              "some-provider": {"type": "api_key", "key": "sk-secret"}
            }"#,
        )?;
        let store = AuthStore { path: path.clone() };

        // Listing reports provider + kind, sorted, with no secret material.
        let stored = store.stored_credentials()?;
        assert_eq!(
            stored,
            vec![
                StoredCredential {
                    provider_id: "openai-codex".to_string(),
                    kind: CredentialKind::OAuth,
                },
                StoredCredential {
                    provider_id: "some-provider".to_string(),
                    kind: CredentialKind::ApiKey,
                },
            ]
        );
        assert!(store.has_credentials("openai-codex")?);
        assert!(!store.has_credentials("anthropic")?);

        // Removing an entry rewrites the file and reports it existed; the other
        // provider's credential is preserved.
        assert!(store.remove_credentials("openai-codex")?);
        assert!(!store.remove_credentials("openai-codex")?);
        assert!(!store.has_credentials("openai-codex")?);
        let remaining = fs::read_to_string(&path)?;
        assert!(remaining.contains("some-provider"));
        assert!(!remaining.contains("openai-codex"));
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    #[test]
    fn missing_auth_file_lists_nothing_and_removes_nothing() -> Result<()> {
        let dir = unique_test_dir()?;
        let store = AuthStore {
            path: dir.join("missing.json"),
        };
        assert!(store.stored_credentials()?.is_empty());
        assert!(!store.has_credentials("openai-codex")?);
        assert!(!store.remove_credentials("openai-codex")?);
        fs::remove_dir_all(dir)?;
        Ok(())
    }

    fn unique_test_dir() -> Result<PathBuf> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "iris-auth-test-{nanos}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn jwt(account_id: &str) -> String {
        let payload =
            format!(r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#);
        format!("aaa.{}.bbb", URL_SAFE_NO_PAD.encode(payload.as_bytes()))
    }
}
