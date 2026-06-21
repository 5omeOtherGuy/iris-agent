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

use crate::mimir::auth::anthropic;
use crate::mimir::auth::storage::AuthStore;
use crate::mimir::selection::ProviderId;

/// UI id of the model hidden behind the [`FABLE_5_OPT_IN_ENV`] opt-in.
const FABLE_5_MODEL_ID: &str = "claude-fable-5";

/// Hidden opt-in for Claude Fable 5. The model stays fully defined in the
/// subscription matrix (so a deliberate selection still builds a correct
/// request), but it is omitted from the `/model` candidate set unless this env
/// var is set to `1`. It is an undocumented 0->1 switch: Fable 5 is gated by
/// Anthropic (`404 not_found` on accounts without access), so it stays off by
/// default and is only surfaced for accounts that opt in.
const FABLE_5_OPT_IN_ENV: &str = "IRIS_ENABLE_FABLE_5";

/// Whether the Fable 5 opt-in is switched on (`IRIS_ENABLE_FABLE_5=1`). Unset,
/// `0`, or any other value keeps it hidden.
fn fable_5_enabled() -> bool {
    std::env::var(FABLE_5_OPT_IN_ENV)
        .map(|value| value.trim() == "1")
        .unwrap_or(false)
}

/// The hand-maintained set of (provider, model id, display name, context-window
/// label) tuples Iris supports. New models are added here in one place; the list
/// intentionally stays small.
///
// ponytail: the context-window labels are hand-maintained display strings for
// the `/model` picker badge, not enforced limits. Verify against provider docs
// when adding models; the upgrade path is a generated registry (declined for
// now, see model_catalog module docs).
// Anthropic rows are the Claude Code subscription matrix; their wire facts
// (native id, output cap, thinking mode, fallback) live in
// `crate::mimir::anthropic_models`. The display id set here must stay in sync
// with that matrix -- `catalog_anthropic_ids_match_subscription_matrix` enforces
// it. The context-window label is the soft routing cap shown in the picker
// badge (e.g. `claude-opus-4-7-300k` is the 300k soft-cap alias of the 1M
// `claude-opus-4-7`).
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
    (ProviderId::Anthropic, "claude-opus-4-8", "Opus 4.8", "1M"),
    (ProviderId::Anthropic, "claude-opus-4-7", "Opus 4.7", "1M"),
    (
        ProviderId::Anthropic,
        "claude-opus-4-7-300k",
        "Opus 4.7 300k",
        "300k",
    ),
    (ProviderId::Anthropic, "claude-opus-4-6", "Opus 4.6", "1M"),
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
            AuthStatus::ClaudeCode => "✓ Claude Code login",
            AuthStatus::Unconfigured => "unconfigured",
        }
    }
}

/// The full catalog, in registry order. Fable 5 is filtered out unless its
/// hidden opt-in ([`FABLE_5_OPT_IN_ENV`]) is switched on, so it never appears in
/// the `/model` picker or exact-match candidate set by default.
pub(crate) fn all() -> Vec<CatalogModel> {
    let fable_enabled = fable_5_enabled();
    ENTRIES
        .iter()
        .filter(|(_, id, ..)| fable_enabled || *id != FABLE_5_MODEL_ID)
        .map(|(provider, id, _name, _ctx)| CatalogModel {
            provider: *provider,
            id: (*id).to_string(),
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
    if auth.has_credentials(provider.as_str()).unwrap_or(false) {
        return AuthStatus::StoredOAuth;
    }
    if provider == ProviderId::Anthropic && anthropic::claude_code_credentials_available() {
        return AuthStatus::ClaudeCode;
    }
    AuthStatus::Unconfigured
}

/// Catalog models whose provider is authenticated, in registry order. This is
/// the candidate set the `/model` picker shows and `/model <exact>` matches
/// against when no scope is active.
pub(crate) fn available_models(auth: &AuthStore) -> Vec<CatalogModel> {
    // Resolve auth once per provider (a handful) rather than re-reading auth.json
    // for every catalog entry.
    let configured: Vec<ProviderId> = ProviderId::ALL
        .iter()
        .copied()
        .filter(|&provider| provider_status(auth, provider).is_configured())
        .collect();
    all()
        .into_iter()
        .filter(|model| configured.contains(&model.provider))
        .collect()
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
        assert_eq!(display_name("anthropic/claude-haiku-4-5"), "Haiku 4.5");
        // Not in the catalog -> show the bare model id.
        assert_eq!(display_name("openai-codex/gpt-9-mystery"), "gpt-9-mystery");
        assert_eq!(display_name("no-slash"), "no-slash");
    }

    #[test]
    fn ctx_label_returns_catalog_value_or_none() {
        assert_eq!(ctx_label("openai-codex/gpt-5.5"), Some("300k"));
        assert_eq!(ctx_label("openai-codex/gpt-5.4"), Some("300k"));
        assert_eq!(ctx_label("openai-codex/gpt-5.3-codex-spark"), Some("300k"));
        assert_eq!(ctx_label("antigravity/gemini-3.1-pro"), Some("1M"));
        assert_eq!(ctx_label("anthropic/claude-sonnet-4-6"), Some("200k"));
        assert_eq!(ctx_label("anthropic/claude-haiku-4-5"), Some("200k"));
        assert_eq!(ctx_label("anthropic/claude-opus-4-8"), Some("1M"));
        assert_eq!(ctx_label("anthropic/claude-opus-4-7-300k"), Some("300k"));
        assert_eq!(ctx_label("anthropic/claude-haiku-4-5"), Some("200k"));
        assert_eq!(ctx_label("anthropic/claude-fable-5"), Some("1M"));
        assert_eq!(ctx_label("openai-codex/gpt-9-mystery"), None);
    }

    #[test]
    fn fable_5_is_hidden_unless_the_opt_in_is_switched_on() {
        // SAFETY: env mutation is process-global; this is the only test that reads
        // IRIS_ENABLE_FABLE_5, and it restores the var before returning.
        let has = |models: &[CatalogModel]| models.iter().any(|m| m.id == FABLE_5_MODEL_ID);
        unsafe { std::env::remove_var(FABLE_5_OPT_IN_ENV) };
        assert!(!has(&all()), "hidden by default (unset)");
        unsafe { std::env::set_var(FABLE_5_OPT_IN_ENV, "0") };
        assert!(!has(&all()), "0 keeps it hidden");
        unsafe { std::env::set_var(FABLE_5_OPT_IN_ENV, "1") };
        assert!(has(&all()), "1 surfaces it");
        // Other Anthropic models are unaffected by the toggle.
        assert!(all().iter().any(|m| m.id == "claude-opus-4-8"));
        unsafe { std::env::remove_var(FABLE_5_OPT_IN_ENV) };
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
        assert_eq!(AuthStatus::Unconfigured.badge(), "unconfigured");
        assert_eq!(AuthStatus::StoredOAuth.badge(), "✓ configured");
    }
}
