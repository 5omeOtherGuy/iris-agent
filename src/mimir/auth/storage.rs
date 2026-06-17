use std::env;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone)]
pub(crate) struct AuthStore {
    path: PathBuf,
}

impl AuthStore {
    pub(crate) fn from_env() -> Result<Self> {
        let home = env::var("HOME").context("HOME is not set")?;
        let path = env::var("IRIS_AUTH_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| Path::new(&home).join(".iris/auth.json"));
        Ok(Self { path })
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

    pub(crate) fn set_oauth_credentials(
        &self,
        provider_id: &str,
        credentials: OAuthCredentials,
    ) -> Result<()> {
        let mut auth = AuthFile::read_or_default(&self.path)?;
        auth.set_oauth_credentials(provider_id, credentials)?;
        auth.write(&self.path)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct OAuthCredentials {
    pub(crate) access: String,
    pub(crate) refresh: String,
    pub(crate) expires: u128,
    #[serde(flatten)]
    pub(crate) extra: serde_json::Map<String, Value>,
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
        .with_context(|| format!("failed to write {}", path.display()))
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

    fn unique_test_dir() -> Result<PathBuf> {
        let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let path = env::temp_dir().join(format!("iris-auth-test-{millis}"));
        fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn jwt(account_id: &str) -> String {
        let payload =
            format!(r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#);
        format!("aaa.{}.bbb", URL_SAFE_NO_PAD.encode(payload.as_bytes()))
    }
}
