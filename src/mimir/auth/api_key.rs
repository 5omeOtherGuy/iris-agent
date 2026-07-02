use anyhow::Result;

use crate::mimir::auth::storage::{AuthStore, CredentialKind};
use crate::mimir::selection::ProviderId;

/// Resolve an API key for an API-key-backed provider.
///
/// Stored credentials win over environment variables. The generic
/// `openai-compatible` lane deliberately does not reuse `OPENAI_API_KEY`: a
/// custom base URL can be local or third-party, so only a dedicated custom env
/// var (or a stored key for that provider) is safe to send there.
pub(crate) fn api_key_for_provider(
    provider: ProviderId,
    auth: &AuthStore,
) -> Result<Option<String>> {
    let provider_id = provider.as_str();
    match auth.credential_kind(provider_id)? {
        Some(CredentialKind::ApiKey) => {
            return Ok(Some(auth.api_key_credentials(provider_id)?.key));
        }
        Some(_) => return Ok(None),
        None => {}
    }

    Ok(env_api_key(provider))
}

fn env_api_key(provider: ProviderId) -> Option<String> {
    let names: &[&str] = match provider {
        ProviderId::OpenAi => &["OPENAI_API_KEY"],
        ProviderId::Anthropic => &["ANTHROPIC_API_KEY"],
        ProviderId::OpenAiCompatible => &[
            "OPENAI_COMPATIBLE_API_KEY",
            "IRIS_OPENAI_COMPATIBLE_API_KEY",
        ],
        ProviderId::OpenAiCodex | ProviderId::Antigravity => &[],
    };
    names.iter().find_map(|name| non_empty_env(name))
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mimir::auth::storage::AuthStore;
    use crate::mimir::selection::ProviderId;
    use anyhow::Result;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_auth() -> Result<(AuthStore, PathBuf)> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "iris-api-key-auth-test-{nanos}-{}-{seq}/auth.json",
            std::process::id()
        ));
        std::fs::create_dir_all(path.parent().unwrap())?;
        Ok((AuthStore::from_path(path.clone()), path))
    }

    #[test]
    fn stored_api_key_wins_over_env_without_leaking_to_custom_provider() -> Result<()> {
        let _env = crate::mimir::test_support::env_lock();
        let (auth, path) = temp_auth()?;
        auth.set_api_key_credentials("openai", "sk-stored")?;
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "sk-env");
            std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-env");
            std::env::remove_var("OPENAI_COMPATIBLE_API_KEY");
            std::env::remove_var("IRIS_OPENAI_COMPATIBLE_API_KEY");
        }

        assert_eq!(
            api_key_for_provider(ProviderId::OpenAi, &auth)?.as_deref(),
            Some("sk-stored")
        );
        assert_eq!(
            api_key_for_provider(ProviderId::Anthropic, &auth)?.as_deref(),
            Some("sk-ant-env")
        );
        assert_eq!(
            api_key_for_provider(ProviderId::OpenAiCompatible, &auth)?.as_deref(),
            None,
            "OPENAI_API_KEY must not be sent to arbitrary OpenAI-compatible base URLs"
        );

        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        std::fs::remove_dir_all(path.parent().unwrap())?;
        Ok(())
    }

    #[test]
    fn custom_provider_uses_only_its_dedicated_env_or_stored_key() -> Result<()> {
        let _env = crate::mimir::test_support::env_lock();
        let (auth, path) = temp_auth()?;
        unsafe { std::env::set_var("OPENAI_COMPATIBLE_API_KEY", "sk-compatible-env") };
        assert_eq!(
            api_key_for_provider(ProviderId::OpenAiCompatible, &auth)?.as_deref(),
            Some("sk-compatible-env")
        );

        auth.set_api_key_credentials("openai-compatible", "sk-compatible-stored")?;
        assert_eq!(
            api_key_for_provider(ProviderId::OpenAiCompatible, &auth)?.as_deref(),
            Some("sk-compatible-stored")
        );

        unsafe {
            std::env::remove_var("OPENAI_COMPATIBLE_API_KEY");
            std::env::remove_var("IRIS_OPENAI_COMPATIBLE_API_KEY");
        };
        std::fs::remove_dir_all(path.parent().unwrap())?;
        Ok(())
    }
}
