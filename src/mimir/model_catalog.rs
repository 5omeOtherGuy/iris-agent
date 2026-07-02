//! Model catalog + auth availability (Tier 3 Mimir): the small hand-maintained
//! list of models Iris actually supports, plus a no-network, no-secret view of
//! which providers are authenticated.
//!
//! Iris deliberately does not adopt pi-mono's generated model registry or
//! `models.json` catalog (see the task report). Instead this module enumerates
//! the handful of provider/model pairs Iris can route to today, keyed off the
//! same provider/model facts `model_capabilities` already encodes. The `/model`
//! picker shows only [`available_models`]: catalog entries whose provider has a
//! credential Iris can use, so an unauthenticated model is never offered.
//!
//! Availability is decided by credential *presence* only -- a stored OAuth
//! record in `auth.json` (or, for Anthropic, an existing Claude Code login). It
//! never reads, refreshes, or exposes the secret material.

use crate::config::Settings;
use crate::mimir::auth::anthropic;
use crate::mimir::auth::api_key;
use crate::mimir::auth::storage::{AuthStore, CredentialKind};
use crate::mimir::selection::{ModelSelection, OpenAiCompatibleConfig, ProviderId};

/// The hand-maintained set of (provider, model id, display name, context-window
/// label) tuples Iris supports. New models are added here in one place; the list
/// intentionally stays small.
///
// ponytail: the context-window labels are hand-maintained display strings for
// the `/model` picker badge, not enforced limits. Verify against provider docs
// when adding models; the upgrade path is a generated registry (declined for
// now, see model_catalog module docs).
// Anthropic rows are the Claude Code subscription matrix; their wire facts
// (model id, output cap, thinking mode, fallback) live in
// `crate::mimir::anthropic_models`. The display id set here must stay in sync
// with that matrix -- `catalog_anthropic_ids_match_subscription_matrix` enforces
// it. The context-window label is the soft routing cap shown in the picker
// badge.
const ENTRIES: &[(ProviderId, &str, &str, &str)] = &[
    (ProviderId::OpenAiCodex, "gpt-5.5", "GPT 5.5", "300k"),
    (ProviderId::OpenAiCodex, "gpt-5.4", "GPT 5.4", "300k"),
    (
        ProviderId::OpenAiCodex,
        "gpt-5.4-mini",
        "GPT 5.4 Mini",
        "300k",
    ),
    (
        ProviderId::OpenAiCodex,
        "gpt-5.3-codex-spark",
        "GPT 5.3 Codex Spark",
        "300k",
    ),
    (ProviderId::OpenAi, "gpt-4.1", "GPT 4.1", "1M"),
    (ProviderId::OpenAi, "gpt-4.1-mini", "GPT 4.1 Mini", "1M"),
    (ProviderId::OpenAi, "gpt-4o", "GPT 4o", "128k"),
    (ProviderId::OpenAi, "gpt-4o-mini", "GPT 4o Mini", "128k"),
    (ProviderId::Anthropic, "claude-opus-4-8", "Opus 4.8", "1M"),
    (ProviderId::Anthropic, "claude-opus-4-7", "Opus 4.7", "1M"),
    (ProviderId::Anthropic, "claude-opus-4-6", "Opus 4.6", "1M"),
    (ProviderId::Anthropic, "claude-sonnet-5", "Sonnet 5", "1M"),
    (
        ProviderId::Anthropic,
        "claude-sonnet-4-6",
        "Sonnet 4.6",
        "200k",
    ),
    (
        ProviderId::Anthropic,
        "claude-haiku-4-5",
        "Haiku 4.5",
        "200k",
    ),
    (ProviderId::Anthropic, "claude-fable-5", "Fable 5", "1M"),
    (
        ProviderId::Antigravity,
        "gemini-3.5-flash",
        "Gemini 3.5 Flash",
        "1M",
    ),
    (
        ProviderId::Antigravity,
        "gemini-3.1-pro",
        "Gemini 3.1 Pro",
        "1M",
    ),
];

/// One known model: its provider and id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogModel {
    pub(crate) provider: ProviderId,
    pub(crate) id: String,
    pub(crate) ctx_label: Option<String>,
}

impl CatalogModel {
    /// Canonical `provider/model` id, used for exact `/model` matching and for
    /// the persisted scoped-model list.
    pub(crate) fn qualified(&self) -> String {
        format!("{}/{}", self.provider.as_str(), self.id)
    }
}

/// Whether a provider has a credential Iris can use, and where it comes from.
/// Carries no secret material -- only enough to render a `/login` status badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthStatus {
    /// Stored OAuth credential in Iris's `auth.json`.
    StoredOAuth,
    /// Stored API-key credential in Iris's `auth.json`.
    StoredApiKey,
    /// API-key credential supplied by the provider-specific environment var.
    EnvApiKey,
    /// Anthropic only: bootstrapped from an existing Claude Code login.
    ClaudeCode,
    /// No credential Iris can use.
    Unconfigured,
}

impl AuthStatus {
    pub(crate) fn is_configured(self) -> bool {
        !matches!(self, AuthStatus::Unconfigured)
    }

    /// Short status badge for the `/login` provider selector. Never a secret.
    pub(crate) fn badge(self) -> &'static str {
        match self {
            AuthStatus::StoredOAuth => "✓ configured",
            AuthStatus::StoredApiKey => "✓ API key",
            AuthStatus::EnvApiKey => "✓ env API key",
            AuthStatus::ClaudeCode => "✓ Claude Code login",
            AuthStatus::Unconfigured => "unconfigured",
        }
    }
}

/// The full catalog, in registry order.
pub(crate) fn all() -> Vec<CatalogModel> {
    ENTRIES
        .iter()
        .map(|(provider, id, _name, ctx)| CatalogModel {
            provider: *provider,
            id: (*id).to_string(),
            ctx_label: Some((*ctx).to_string()),
        })
        .collect()
}

/// Context-window label for a `provider/modelId` (e.g. "200k", "1M"), shown as the
/// `/model` picker's `[ctx:...]` badge. `None` for anything not in the catalog.
pub(crate) fn ctx_label(qualified: &str) -> Option<&'static str> {
    ENTRIES
        .iter()
        .find(|(provider, id, _, _)| format!("{}/{}", provider.as_str(), id) == qualified)
        .map(|(_, _, _, ctx)| *ctx)
}

#[cfg(test)]
pub(crate) fn ctx_label_for(model: &CatalogModel) -> Option<String> {
    model
        .ctx_label
        .clone()
        .or_else(|| ctx_label(&model.qualified()).map(str::to_string))
}

/// Human-friendly display name for a `provider/modelId`, shown in the `/model`
/// picker footer ("Model Name: ..."). Falls back to the bare model id for
/// anything not in the catalog.
pub(crate) fn display_name(qualified: &str) -> String {
    ENTRIES
        .iter()
        .find(|(provider, id, _, _)| format!("{}/{}", provider.as_str(), id) == qualified)
        .map(|(_, _, name, _)| (*name).to_string())
        .unwrap_or_else(|| {
            qualified
                .split_once('/')
                .map(|(_, id)| id)
                .unwrap_or(qualified)
                .to_string()
        })
}

/// Auth status for one provider, by credential presence only.
pub(crate) fn provider_status(auth: &AuthStore, provider: ProviderId) -> AuthStatus {
    match auth.credential_kind(provider.as_str()).ok().flatten() {
        Some(CredentialKind::OAuth) => return AuthStatus::StoredOAuth,
        Some(CredentialKind::ApiKey) => return AuthStatus::StoredApiKey,
        Some(CredentialKind::Unknown) | None => {}
    }
    if api_key::api_key_for_provider(provider, auth)
        .ok()
        .flatten()
        .is_some()
    {
        return AuthStatus::EnvApiKey;
    }
    if provider == ProviderId::Anthropic && anthropic::claude_code_credentials_available() {
        return AuthStatus::ClaudeCode;
    }
    AuthStatus::Unconfigured
}

/// Catalog models whose provider is authenticated, in registry order. This is
/// the candidate set the `/model` picker shows and `/model <exact>` matches
/// against when no scope is active.
pub(crate) fn available_models(auth: &AuthStore, settings: &Settings) -> Vec<CatalogModel> {
    // Resolve auth once per provider (a handful) rather than re-reading auth.json
    // for every catalog entry.
    let configured: Vec<ProviderId> = ProviderId::ALL
        .iter()
        .copied()
        .filter(|&provider| provider_status(auth, provider).is_configured())
        .collect();
    let mut models: Vec<CatalogModel> = all()
        .into_iter()
        .filter(|model| configured.contains(&model.provider))
        .collect();

    if let Some(model) = openai_compatible_model(auth, settings)
        && !models
            .iter()
            .any(|entry| entry.qualified() == model.qualified())
    {
        models.push(model);
    }
    models
}

fn openai_compatible_model(auth: &AuthStore, settings: &Settings) -> Option<CatalogModel> {
    let configured_default = settings
        .default_provider
        .as_deref()
        .and_then(|provider| ProviderId::parse(provider).ok())
        == Some(ProviderId::OpenAiCompatible);
    let has_key = provider_status(auth, ProviderId::OpenAiCompatible).is_configured();
    if !configured_default && settings.open_ai_compatible.is_none() && !has_key {
        return None;
    }

    let config = OpenAiCompatibleConfig::from_settings(settings.open_ai_compatible.as_ref());
    if config.api_key_required && !has_key {
        return None;
    }
    let id = if configured_default {
        ModelSelection::resolve(settings)
            .ok()
            .map(|selection| selection.model)
            .unwrap_or_else(|| ProviderId::OpenAiCompatible.default_model().to_string())
    } else {
        ProviderId::OpenAiCompatible.default_model().to_string()
    };
    Some(CatalogModel {
        provider: ProviderId::OpenAiCompatible,
        id,
        ctx_label: config.context_window.map(context_window_label),
    })
}

pub(crate) fn context_window_label(tokens: u64) -> String {
    if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1000 {
        format!("{}k", tokens / 1000)
    } else {
        tokens.to_string()
    }
}

/// Result of resolving a `/model <search>` argument against a candidate set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExactMatch {
    /// Exactly one candidate matched: switch to it without opening the picker.
    One(CatalogModel),
    /// A bare model id matched more than one candidate: fall back to the picker.
    Ambiguous,
    /// No candidate matched: open the picker with the search pre-filled.
    None,
}

/// Resolve `query` against `candidates`, matching pi-mono's exact-match rules:
/// case-insensitive; `provider/modelId` matches canonically; a bare `modelId`
/// matches only when exactly one candidate has that id (otherwise ambiguous).
pub(crate) fn exact_match(candidates: &[CatalogModel], query: &str) -> ExactMatch {
    let query = query.trim();
    if query.is_empty() {
        return ExactMatch::None;
    }
    if let Some((provider, model)) = query.split_once('/') {
        let provider = provider.trim();
        let model = model.trim();
        let mut hits = candidates.iter().filter(|candidate| {
            candidate.provider.as_str().eq_ignore_ascii_case(provider)
                && candidate.id.eq_ignore_ascii_case(model)
        });
        return match hits.next() {
            Some(found) => ExactMatch::One(found.clone()),
            None => ExactMatch::None,
        };
    }
    let mut hits = candidates
        .iter()
        .filter(|candidate| candidate.id.eq_ignore_ascii_case(query));
    match (hits.next(), hits.next()) {
        (Some(found), None) => ExactMatch::One(found.clone()),
        (Some(_), Some(_)) => ExactMatch::Ambiguous,
        _ => ExactMatch::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: ProviderId, id: &str) -> CatalogModel {
        CatalogModel {
            provider,
            id: id.to_string(),
            ctx_label: None,
        }
    }

    fn settings(
        provider: Option<&str>,
        model: Option<&str>,
        base_url: Option<&str>,
    ) -> crate::config::Settings {
        crate::config::Settings {
            default_provider: provider.map(str::to_string),
            default_model: model.map(str::to_string),
            base_url: base_url.map(str::to_string),
            context_token_budget: None,
            default_reasoning: None,
            prompt_cache_retention: None,
            anthropic_context_management: None,
            enabled_models: None,
            max_tool_roundtrips: None,
            retry: None,
            open_ai_compatible: Some(crate::config::OpenAiCompatibleSettings {
                context_window: Some(131_072),
                reasoning: Some(true),
                api_key_required: Some(false),
            }),
        }
    }

    #[test]
    fn qualified_id_is_provider_slash_model() {
        assert_eq!(
            model(ProviderId::Anthropic, "claude-sonnet-4-6").qualified(),
            "anthropic/claude-sonnet-4-6"
        );
    }

    #[test]
    fn display_name_uses_catalog_then_falls_back_to_id() {
        assert_eq!(display_name("openai-codex/gpt-5.5"), "GPT 5.5");
        assert_eq!(display_name("openai-codex/gpt-5.4"), "GPT 5.4");
        assert_eq!(display_name("openai-codex/gpt-5.4-mini"), "GPT 5.4 Mini");
        assert_eq!(
            display_name("openai-codex/gpt-5.3-codex-spark"),
            "GPT 5.3 Codex Spark"
        );
        assert_eq!(display_name("antigravity/gemini-3.1-pro"), "Gemini 3.1 Pro");
        assert_eq!(display_name("anthropic/claude-opus-4-7"), "Opus 4.7");
        assert_eq!(display_name("anthropic/claude-sonnet-5"), "Sonnet 5");
        assert_eq!(display_name("anthropic/claude-haiku-4-5"), "Haiku 4.5");
        // Not in the catalog -> show the bare model id.
        assert_eq!(display_name("openai-codex/gpt-9-mystery"), "gpt-9-mystery");
        assert_eq!(display_name("openai-compatible/llama3.1"), "llama3.1");
        assert_eq!(display_name("no-slash"), "no-slash");
    }

    #[test]
    fn ctx_label_returns_catalog_value_or_none() {
        assert_eq!(ctx_label("openai-codex/gpt-5.5"), Some("300k"));
        assert_eq!(ctx_label("openai-codex/gpt-5.4"), Some("300k"));
        assert_eq!(ctx_label("openai-codex/gpt-5.3-codex-spark"), Some("300k"));
        assert_eq!(ctx_label("antigravity/gemini-3.1-pro"), Some("1M"));
        assert_eq!(ctx_label("anthropic/claude-sonnet-5"), Some("1M"));
        assert_eq!(ctx_label("anthropic/claude-sonnet-4-6"), Some("200k"));
        assert_eq!(ctx_label("anthropic/claude-haiku-4-5"), Some("200k"));
        assert_eq!(ctx_label("anthropic/claude-opus-4-8"), Some("1M"));
        assert_eq!(ctx_label("anthropic/claude-opus-4-7"), Some("1M"));
        assert_eq!(ctx_label("anthropic/claude-haiku-4-5"), Some("200k"));
        assert_eq!(ctx_label("anthropic/claude-fable-5"), Some("1M"));
        assert_eq!(ctx_label("openai-codex/gpt-9-mystery"), None);
        assert_eq!(ctx_label("openai-compatible/llama3.1"), None);
    }

    #[test]
    fn api_key_status_and_available_models_use_stored_or_env_keys() {
        let _env = crate::mimir::test_support::env_lock();
        let dir = std::env::temp_dir().join(format!("iris-catalog-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let auth = AuthStore::from_path(dir.join("auth.json"));
        auth.set_api_key_credentials("openai", "sk-openai").unwrap();
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "sk-anthropic");
            std::env::remove_var("OPENAI_COMPATIBLE_API_KEY");
        }

        assert_eq!(
            provider_status(&auth, ProviderId::OpenAi),
            AuthStatus::StoredApiKey
        );
        assert_eq!(
            provider_status(&auth, ProviderId::Anthropic),
            AuthStatus::EnvApiKey
        );
        assert_eq!(AuthStatus::StoredApiKey.badge(), "✓ API key");
        assert_eq!(AuthStatus::EnvApiKey.badge(), "✓ env API key");

        let models = available_models(&auth, &settings(None, None, None));
        assert!(models.iter().any(|m| m.qualified() == "openai/gpt-4.1"));
        assert!(
            models
                .iter()
                .any(|m| m.qualified() == "anthropic/claude-sonnet-4-6")
        );

        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn openai_compatible_catalog_synthesizes_configured_model_without_key() {
        let dir =
            std::env::temp_dir().join(format!("iris-catalog-custom-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let auth = AuthStore::from_path(dir.join("auth.json"));
        let models = available_models(
            &auth,
            &settings(
                Some("openai-compatible"),
                Some("llama3.1"),
                Some("http://localhost:11434/v1"),
            ),
        );

        let custom = models
            .iter()
            .find(|m| m.qualified() == "openai-compatible/llama3.1")
            .expect("configured custom model");
        assert_eq!(ctx_label_for(custom), Some("131k".to_string()));

        let models = available_models(&auth, &settings(Some("openai"), Some("gpt-4.1"), None));
        assert!(
            models
                .iter()
                .any(|m| m.qualified() == "openai-compatible/llama3.1"),
            "configured custom provider stays discoverable when another provider is active"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fable_5_is_in_the_catalog() {
        assert!(all().iter().any(|m| m.id == "claude-fable-5"));
        assert!(all().iter().any(|m| m.id == "claude-opus-4-8"));
    }

    #[test]
    fn catalog_anthropic_ids_match_subscription_matrix() {
        use crate::mimir::anthropic_models;
        let mut catalog_ids: Vec<&str> = ENTRIES
            .iter()
            .filter(|(provider, ..)| *provider == ProviderId::Anthropic)
            .map(|(_, id, ..)| *id)
            .collect();
        catalog_ids.sort_unstable();
        let mut matrix_ids: Vec<&str> = anthropic_models::MODELS.iter().map(|m| m.ui_id).collect();
        matrix_ids.sort_unstable();
        assert_eq!(
            catalog_ids, matrix_ids,
            "catalog Anthropic ids must match the subscription model matrix"
        );
    }

    #[test]
    fn exact_match_resolves_qualified_and_bare_ids() {
        let candidates = vec![
            model(ProviderId::OpenAiCodex, "gpt-5.5"),
            model(ProviderId::Anthropic, "claude-sonnet-4-6"),
        ];
        // Qualified id, case-insensitive.
        assert_eq!(
            exact_match(&candidates, "ANTHROPIC/claude-sonnet-4-6"),
            ExactMatch::One(model(ProviderId::Anthropic, "claude-sonnet-4-6"))
        );
        // Bare unique id.
        assert_eq!(
            exact_match(&candidates, "gpt-5.5"),
            ExactMatch::One(model(ProviderId::OpenAiCodex, "gpt-5.5"))
        );
        // Unknown -> none (caller opens the picker pre-filled).
        assert_eq!(exact_match(&candidates, "bad-prefix"), ExactMatch::None);
        // Empty -> none.
        assert_eq!(exact_match(&candidates, "   "), ExactMatch::None);
    }

    #[test]
    fn bare_id_shared_by_two_providers_is_ambiguous() {
        let candidates = vec![
            model(ProviderId::OpenAiCodex, "shared"),
            model(ProviderId::Anthropic, "shared"),
        ];
        assert_eq!(exact_match(&candidates, "shared"), ExactMatch::Ambiguous);
        // The qualified form still resolves unambiguously.
        assert_eq!(
            exact_match(&candidates, "anthropic/shared"),
            ExactMatch::One(model(ProviderId::Anthropic, "shared"))
        );
    }

    #[test]
    fn unconfigured_status_is_not_configured_and_has_no_secret_badge() {
        assert!(!AuthStatus::Unconfigured.is_configured());
        assert!(AuthStatus::StoredOAuth.is_configured());
        assert!(AuthStatus::StoredApiKey.is_configured());
        assert!(AuthStatus::EnvApiKey.is_configured());
        assert_eq!(AuthStatus::Unconfigured.badge(), "unconfigured");
        assert_eq!(AuthStatus::StoredOAuth.badge(), "✓ configured");
        assert_eq!(AuthStatus::StoredApiKey.badge(), "✓ API key");
        assert_eq!(AuthStatus::EnvApiKey.badge(), "✓ env API key");
    }
}
