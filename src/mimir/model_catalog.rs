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

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Settings;
use crate::errors::UsageError;
use crate::mimir::auth::anthropic;
use crate::mimir::auth::api_key;
use crate::mimir::auth::storage::{AuthStore, CredentialKind};
use crate::mimir::selection::{ModelSelection, OpenAiCompatibleConfig, ProviderId};

/// The hand-maintained set of (provider, model id, display name, context-window
/// label) tuples Iris supports. The label also resolves to the numeric trigger
/// window below. New models are added here in one place; the list stays small.
///
// ponytail: context-window labels are hand-maintained catalog facts used by the
// `/model` picker and the compaction trigger. Verify them with output reserves
// when adding models; the upgrade path is a generated registry (declined for
// now, see model_catalog module docs).
// Anthropic rows are the Claude Code subscription matrix; their wire facts
// (model id, output cap, thinking mode, fallback) live in
// `crate::mimir::anthropic_models`. The display id set here must stay in sync
// with that matrix -- `catalog_anthropic_ids_match_subscription_matrix` enforces
// it. The context-window label is the soft routing cap shown in the picker
// badge.
const ENTRIES: &[(ProviderId, &str, &str, &str)] = &[
    (
        ProviderId::OpenAiCodex,
        "gpt-5.6-sol",
        "GPT 5.6 Sol",
        "372k",
    ),
    (
        ProviderId::OpenAiCodex,
        "gpt-5.6-terra",
        "GPT 5.6 Terra",
        "372k",
    ),
    (
        ProviderId::OpenAiCodex,
        "gpt-5.6-luna",
        "GPT 5.6 Luna",
        "372k",
    ),
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

/// Provider vendor used to decide whether a credential-lane selector is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ProviderVendor {
    OpenAi,
    Anthropic,
    Google,
    Custom,
}

/// Authentication mechanism for one active model route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CredentialLaneKind {
    OAuth,
    Api,
    Configured,
}

/// One active, non-secret credential lane for a model route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentCredentialLane {
    pub(crate) id: String,
    pub(crate) vendor: ProviderVendor,
    pub(crate) provider: ProviderId,
    pub(crate) kind: CredentialLaneKind,
}

/// One model/lane pair captured for delegated-worker schema and routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentCatalogEntry {
    pub(crate) model: CatalogModel,
    pub(crate) lane: Option<SubagentCredentialLane>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SubagentSchemaChoices {
    pub(crate) models: Vec<String>,
    pub(crate) providers: Option<Vec<String>>,
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

/// Capture active credential lanes for delegated-worker schema and routing.
pub(crate) fn subagent_catalog(auth: &AuthStore, settings: &Settings) -> Vec<SubagentCatalogEntry> {
    available_models(auth, settings)
        .into_iter()
        .flat_map(|model| {
            active_lanes(auth, settings, model.provider)
                .into_iter()
                .map(move |lane| SubagentCatalogEntry {
                    model: model.clone(),
                    lane: Some(lane),
                })
        })
        .collect()
}

fn active_lanes(
    auth: &AuthStore,
    settings: &Settings,
    provider: ProviderId,
) -> Vec<SubagentCredentialLane> {
    let stored = auth.credential_kind(provider.as_str()).ok().flatten();
    let oauth = stored == Some(CredentialKind::OAuth)
        || (provider == ProviderId::Anthropic && anthropic::claude_code_credentials_available());
    let api = stored == Some(CredentialKind::ApiKey) || api_key::env_api_key_available(provider);
    let (vendor, oauth_id, api_id) = match provider {
        ProviderId::OpenAiCodex => (ProviderVendor::OpenAi, Some("openai-codex"), None),
        ProviderId::OpenAi => (ProviderVendor::OpenAi, None, Some("openai")),
        ProviderId::Anthropic => (
            ProviderVendor::Anthropic,
            Some("anthropic-oauth"),
            Some("anthropic-api"),
        ),
        ProviderId::Antigravity => (ProviderVendor::Google, Some("antigravity"), None),
        ProviderId::OpenAiCompatible => (ProviderVendor::Custom, None, Some("openai-compatible")),
    };
    let mut lanes = Vec::new();
    if oauth && let Some(id) = oauth_id {
        lanes.push(SubagentCredentialLane {
            id: id.to_string(),
            vendor,
            provider,
            kind: CredentialLaneKind::OAuth,
        });
    }
    if api && let Some(id) = api_id {
        lanes.push(SubagentCredentialLane {
            id: id.to_string(),
            vendor,
            provider,
            kind: CredentialLaneKind::Api,
        });
    }
    if provider == ProviderId::OpenAiCompatible
        && lanes.is_empty()
        && settings
            .open_ai_compatible
            .as_ref()
            .is_some_and(|config| config.api_key_required == Some(false))
    {
        lanes.push(SubagentCredentialLane {
            id: "openai-compatible".to_string(),
            vendor,
            provider,
            kind: CredentialLaneKind::Configured,
        });
    }
    lanes
}

/// Model enum values and the optional credential-lane selector.
pub(crate) fn subagent_schema_choices_from(
    entries: &[SubagentCatalogEntry],
) -> SubagentSchemaChoices {
    let active: Vec<&SubagentCatalogEntry> = entries
        .iter()
        .filter(|entry| entry.lane.is_some())
        .collect();
    let models = distinct_ids(
        &active
            .iter()
            .map(|entry| entry.model.clone())
            .collect::<Vec<_>>(),
    );
    let mut lane_ids = Vec::new();
    let mut vendor_lanes: BTreeMap<ProviderVendor, BTreeSet<String>> = BTreeMap::new();
    for lane in active.iter().filter_map(|entry| entry.lane.as_ref()) {
        if !lane_ids.contains(&lane.id) {
            lane_ids.push(lane.id.clone());
        }
        vendor_lanes
            .entry(lane.vendor)
            .or_default()
            .insert(lane.id.clone());
    }
    let providers = vendor_lanes
        .values()
        .any(|lanes| lanes.len() >= 2)
        .then_some(lane_ids);
    SubagentSchemaChoices { models, providers }
}

/// Resolve an active delegated-worker model, preferring OAuth when no lane is named.
pub(crate) fn resolve_subagent_model_in(
    entries: &[SubagentCatalogEntry],
    model: &str,
    provider: Option<&str>,
) -> Result<SubagentCatalogEntry> {
    let model = model.trim();
    let provider = provider.map(str::trim).filter(|value| !value.is_empty());
    let mut matches: Vec<&SubagentCatalogEntry> = entries
        .iter()
        .filter(|entry| {
            entry.model.id == model
                && entry
                    .lane
                    .as_ref()
                    .is_some_and(|lane| provider.is_none_or(|requested| lane.id == requested))
        })
        .collect();
    if matches.is_empty() {
        let available = subagent_schema_choices_from(entries).models.join(", ");
        let message = if available.is_empty() {
            format!(
                "model '{model}' is not available; no active credential lane offers a subagent model"
            )
        } else if let Some(provider) = provider {
            format!("model '{model}' is not available from provider lane '{provider}'")
        } else {
            format!("unknown or unauthenticated model '{model}'; available models: {available}")
        };
        return Err(UsageError::new(message).into());
    }
    matches.sort_by_key(|entry| match entry.lane.as_ref().map(|lane| lane.kind) {
        Some(CredentialLaneKind::OAuth) => 0,
        Some(CredentialLaneKind::Configured) => 1,
        Some(CredentialLaneKind::Api) => 2,
        None => 3,
    });
    let preferred = matches[0];
    if provider.is_none()
        && matches.get(1).is_some_and(|next| {
            next.lane.as_ref().map(|lane| lane.kind)
                == preferred.lane.as_ref().map(|lane| lane.kind)
                && next.lane.as_ref().map(|lane| lane.vendor)
                    != preferred.lane.as_ref().map(|lane| lane.vendor)
        })
    {
        return Err(UsageError::new(format!(
            "model '{model}' is offered by multiple provider vendors; select a provider lane"
        ))
        .into());
    }
    Ok(preferred.clone())
}

/// Distinct model ids and provider wire ids in `models`, in registry order.
/// Backs the `spawn_subagent` schema `enum`s so the delegating model is offered
/// only exact, authenticated identifiers instead of guessing spelling.
pub(crate) fn schema_choices_from(models: &[CatalogModel]) -> (Vec<String>, Vec<String>) {
    let mut providers = Vec::new();
    for model in models {
        let provider = model.provider.as_str().to_string();
        if !providers.contains(&provider) {
            providers.push(provider);
        }
    }
    (distinct_ids(models), providers)
}

/// Resolve a `(model, optional provider)` subagent request against an
/// authenticated catalog snapshot. `provider` disambiguates a model id offered
/// by more than one authenticated provider; a unique id needs no provider.
pub(crate) fn resolve_model_in(
    models: &[CatalogModel],
    model: &str,
    provider: Option<&str>,
) -> Result<CatalogModel> {
    let model = model.trim();
    let provider = provider.map(str::trim).filter(|value| !value.is_empty());
    let matches: Vec<&CatalogModel> = models
        .iter()
        .filter(|entry| {
            entry.id == model && provider.is_none_or(|want| entry.provider.as_str() == want)
        })
        .collect();
    match matches.as_slice() {
        [only] => Ok((*only).clone()),
        [] => Err(unresolved_model_error(models, model, provider)),
        many => {
            let providers = many
                .iter()
                .map(|entry| entry.provider.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(UsageError::new(format!(
                "model '{model}' is offered by multiple providers; set provider to one of: {providers}"
            ))
            .into())
        }
    }
}

fn unresolved_model_error(
    models: &[CatalogModel],
    model: &str,
    provider: Option<&str>,
) -> anyhow::Error {
    if let Some(want) = provider {
        let offered: Vec<&str> = models
            .iter()
            .filter(|entry| entry.id == model)
            .map(|entry| entry.provider.as_str())
            .collect();
        if !offered.is_empty() {
            return UsageError::new(format!(
                "model '{model}' is not available from provider '{want}'; it is offered by: {}",
                offered.join(", ")
            ))
            .into();
        }
    }
    let available = distinct_ids(models).join(", ");
    if available.is_empty() {
        UsageError::new(format!(
            "model '{model}' is not available; no authenticated provider offers a subagent model"
        ))
        .into()
    } else {
        UsageError::new(format!(
            "unknown or unauthenticated model '{model}'; available models: {available}"
        ))
        .into()
    }
}

fn distinct_ids(models: &[CatalogModel]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for model in models {
        if !ids.iter().any(|id| id == &model.id) {
            ids.push(model.id.clone());
        }
    }
    ids
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextPolicyAuthority {
    OfficialCli,
    CatalogFallback,
    ConfiguredEndpoint,
}

/// Provider-neutral model facts used to resolve Iris's context policy. The
/// displayed window and hard application threshold are deliberately separate:
/// official CLIs do not necessarily compact at the capacity they display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffectiveContextWindow {
    pub(crate) raw: u64,
    pub(crate) displayed: u64,
    pub(crate) model_max_output_tokens: u64,
    pub(crate) output_reserve: u64,
    pub(crate) summary_reserve: u64,
    pub(crate) hard_compaction_threshold: u64,
    pub(crate) authority: ContextPolicyAuthority,
}

/// Tier-3 owns the conversion into the provider-neutral facts the runtime
/// carries: inner tiers depend on `metrics`, never on this catalog type.
impl From<EffectiveContextWindow> for crate::metrics::ContextWindowFacts {
    fn from(policy: EffectiveContextWindow) -> Self {
        Self {
            raw: policy.raw,
            displayed: policy.displayed,
            model_max_output_tokens: policy.model_max_output_tokens,
            output_reserve: policy.output_reserve,
            summary_reserve: policy.summary_reserve,
            hard_compaction_threshold: policy.hard_compaction_threshold,
            official_cli: policy.authority == ContextPolicyAuthority::OfficialCli,
            configured_endpoint: policy.authority == ContextPolicyAuthority::ConfiguredEndpoint,
        }
    }
}

/// Resolve provider/model facts into the context policy exposed by the
/// corresponding official CLI. For providers without an authoritative CLI,
/// preserve Iris's catalog/default reserve behavior and mark it as fallback.
pub(crate) fn effective_context_window(
    selection: &ModelSelection,
    fallback_summary_reserve: u64,
) -> Option<EffectiveContextWindow> {
    let (raw, model_max_output_tokens, authority) =
        if selection.provider == ProviderId::OpenAiCompatible {
            (
                selection.open_ai_compatible.context_window?,
                0,
                ContextPolicyAuthority::ConfiguredEndpoint,
            )
        } else {
            let qualified = format!("{}/{}", selection.provider.as_str(), selection.model);
            (
                catalog_context_window(&qualified)?,
                catalog_max_output_tokens(selection.provider, &selection.model),
                if matches!(
                    selection.provider,
                    ProviderId::OpenAiCodex | ProviderId::Anthropic
                ) {
                    ContextPolicyAuthority::OfficialCli
                } else {
                    ContextPolicyAuthority::CatalogFallback
                },
            )
        };
    let output_reserve = model_max_output_tokens.min(20_000);
    let (displayed, summary_reserve, hard_compaction_threshold) = match selection.provider {
        ProviderId::OpenAiCodex => {
            let displayed = raw.saturating_mul(95) / 100;
            let hard = raw.saturating_mul(90) / 100;
            (
                displayed,
                raw.saturating_sub(output_reserve).saturating_sub(hard),
                hard,
            )
        }
        ProviderId::Anthropic => {
            let summary = 13_000;
            (
                raw,
                summary,
                raw.saturating_sub(output_reserve).saturating_sub(summary),
            )
        }
        _ => (
            raw,
            fallback_summary_reserve,
            raw.saturating_sub(output_reserve)
                .saturating_sub(fallback_summary_reserve),
        ),
    };
    Some(EffectiveContextWindow {
        raw,
        displayed,
        model_max_output_tokens,
        output_reserve,
        summary_reserve,
        hard_compaction_threshold,
        authority,
    })
}

pub(crate) fn catalog_context_window(qualified: &str) -> Option<u64> {
    ctx_label(qualified).and_then(|label| match label {
        "128k" => Some(128_000),
        "200k" => Some(200_000),
        "300k" => Some(300_000),
        "372k" => Some(372_000),
        "1M" => Some(1_000_000),
        _ => None,
    })
}

fn catalog_max_output_tokens(provider: ProviderId, model: &str) -> u64 {
    if provider == ProviderId::Anthropic {
        return crate::mimir::anthropic_models::find(model)
            .map(|model| u64::from(model.output_cap))
            .unwrap_or(64_000);
    }
    match provider {
        // These are selection/catalog reserves, not request limits. Provider
        // adapters may choose a smaller response for an individual request.
        ProviderId::OpenAiCodex => 128_000,
        ProviderId::OpenAi if model.starts_with("gpt-4.1") => 32_768,
        ProviderId::OpenAi => 16_384,
        ProviderId::Antigravity => 65_536,
        ProviderId::OpenAiCompatible | ProviderId::Anthropic => 0,
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
            microcompaction_watermark: None,
            default_reasoning: None,
            compaction_summarizer: None,
            microcompaction: None,
            tool_result_compaction: None,
            mutation_safety: None,
            tasks: None,
            bash_tool_mode: None,
            web_search_backend: None,
            read_web_page_backend: None,
            searxng_url: None,
            search_timeout_ms: None,
            read_timeout_ms: None,
            max_search_results: None,
            max_search_response_bytes: None,
            max_read_response_bytes: None,
            max_read_output_bytes: None,
            prompt_cache_retention: None,
            anthropic_context_management: None,
            enabled_models: None,
            max_tool_roundtrips: None,
            retry: None,
            codex_transport: None,
            codex_stream_idle_timeout_ms: None,
            open_ai_compatible: Some(crate::config::OpenAiCompatibleSettings {
                context_window: Some(131_072),
                reasoning: Some(true),
                api_key_required: Some(false),
            }),
            verify: None,
            tui: None,
            default_approval: None,
            worktree_root: None,
            compaction: None,
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
    fn official_cli_context_policies_reserve_output_and_summary_headroom() {
        let haiku = settings(Some("anthropic"), Some("claude-haiku-4-5"), None);
        let haiku = ModelSelection::resolve(&haiku).unwrap();
        assert_eq!(
            effective_context_window(&haiku, 8_192),
            Some(EffectiveContextWindow {
                raw: 200_000,
                displayed: 200_000,
                model_max_output_tokens: 64_000,
                output_reserve: 20_000,
                summary_reserve: 13_000,
                hard_compaction_threshold: 167_000,
                authority: ContextPolicyAuthority::OfficialCli,
            })
        );

        let codex = settings(Some("openai-codex"), Some("gpt-5.6-sol"), None);
        let codex = ModelSelection::resolve(&codex).unwrap();
        assert_eq!(
            effective_context_window(&codex, 8_192),
            Some(EffectiveContextWindow {
                raw: 372_000,
                displayed: 353_400,
                model_max_output_tokens: 128_000,
                output_reserve: 20_000,
                summary_reserve: 17_200,
                hard_compaction_threshold: 334_800,
                authority: ContextPolicyAuthority::OfficialCli,
            })
        );
    }

    #[test]
    fn openai_compatible_window_is_numeric_and_unknown_models_degrade() {
        let custom = settings(Some("openai-compatible"), Some("llama3.1"), None);
        let custom = ModelSelection::resolve(&custom).unwrap();
        assert_eq!(
            effective_context_window(&custom, 8_192),
            Some(EffectiveContextWindow {
                raw: 131_072,
                displayed: 131_072,
                model_max_output_tokens: 0,
                output_reserve: 0,
                summary_reserve: 8_192,
                hard_compaction_threshold: 122_880,
                authority: ContextPolicyAuthority::ConfiguredEndpoint,
            })
        );

        let mut unknown_settings = settings(Some("openai"), Some("gpt-unknown"), None);
        unknown_settings.open_ai_compatible = None;
        let unknown = ModelSelection::resolve(&unknown_settings).unwrap();
        assert_eq!(effective_context_window(&unknown, 8_192), None);
    }

    #[test]
    fn display_name_uses_catalog_then_falls_back_to_id() {
        assert_eq!(display_name("openai-codex/gpt-5.6-sol"), "GPT 5.6 Sol");
        assert_eq!(display_name("openai-codex/gpt-5.6-terra"), "GPT 5.6 Terra");
        assert_eq!(display_name("openai-codex/gpt-5.6-luna"), "GPT 5.6 Luna");
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
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            assert_eq!(
                ctx_label(&format!("openai-codex/{model}")),
                Some("372k"),
                "{model} must expose its real Codex context window to the picker and meter"
            );
        }
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
    fn unconfigured_status_is_not_configured() {
        assert!(!AuthStatus::Unconfigured.is_configured());
        assert!(AuthStatus::StoredOAuth.is_configured());
        assert!(AuthStatus::StoredApiKey.is_configured());
        assert!(AuthStatus::EnvApiKey.is_configured());
    }

    #[test]
    fn schema_choices_are_distinct_and_registry_ordered() {
        let catalog = [
            model(ProviderId::OpenAiCodex, "gpt-5.4-mini"),
            model(ProviderId::OpenAi, "gpt-4.1"),
            model(ProviderId::Anthropic, "shared"),
            model(ProviderId::OpenAi, "shared"),
        ];
        let (models, providers) = schema_choices_from(&catalog);
        assert_eq!(models, vec!["gpt-5.4-mini", "gpt-4.1", "shared"]);
        assert_eq!(providers, vec!["openai-codex", "openai", "anthropic"]);
    }

    #[test]
    fn resolve_model_matches_a_unique_id_without_a_provider() {
        let catalog = [
            model(ProviderId::OpenAiCodex, "gpt-5.4-mini"),
            model(ProviderId::Anthropic, "claude-opus-4-6"),
        ];
        let resolved = resolve_model_in(&catalog, "gpt-5.4-mini", None).unwrap();
        assert_eq!(resolved.provider, ProviderId::OpenAiCodex);
        assert_eq!(resolved.id, "gpt-5.4-mini");
        // Surrounding whitespace is tolerated.
        assert_eq!(
            resolve_model_in(&catalog, "  gpt-5.4-mini  ", None)
                .unwrap()
                .provider,
            ProviderId::OpenAiCodex
        );
    }

    #[test]
    fn resolve_model_rejects_unknown_ids_and_lists_available_ones() {
        let catalog = [model(ProviderId::OpenAiCodex, "gpt-5.4-mini")];
        let error = resolve_model_in(&catalog, "gpt-9", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("gpt-9"), "{error}");
        assert!(error.contains("gpt-5.4-mini"), "{error}");

        let empty = resolve_model_in(&[], "gpt-9", None)
            .unwrap_err()
            .to_string();
        assert!(empty.contains("no authenticated provider"), "{empty}");
    }

    #[test]
    fn resolve_model_rejects_a_wrong_provider_for_a_known_id() {
        let catalog = [model(ProviderId::OpenAiCodex, "gpt-5.4-mini")];
        let error = resolve_model_in(&catalog, "gpt-5.4-mini", Some("openai"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("not available from provider 'openai'"),
            "{error}"
        );
        assert!(error.contains("openai-codex"), "{error}");
    }

    #[test]
    fn resolve_model_disambiguates_a_shared_id_by_provider() {
        let catalog = [
            model(ProviderId::OpenAi, "shared"),
            model(ProviderId::Anthropic, "shared"),
        ];
        // Ambiguous without a provider: refuse and name the candidates.
        let error = resolve_model_in(&catalog, "shared", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("multiple providers"), "{error}");
        assert!(
            error.contains("openai") && error.contains("anthropic"),
            "{error}"
        );

        // A provider selects the intended one.
        assert_eq!(
            resolve_model_in(&catalog, "shared", Some("anthropic"))
                .unwrap()
                .provider,
            ProviderId::Anthropic
        );
    }

    fn subagent_entry(
        provider: ProviderId,
        model_id: &str,
        lane: Option<SubagentCredentialLane>,
    ) -> SubagentCatalogEntry {
        SubagentCatalogEntry {
            model: model(provider, model_id),
            lane,
        }
    }

    fn lane(
        id: &str,
        vendor: ProviderVendor,
        provider: ProviderId,
        kind: CredentialLaneKind,
    ) -> SubagentCredentialLane {
        SubagentCredentialLane {
            id: id.to_string(),
            vendor,
            provider,
            kind,
        }
    }

    #[test]
    fn subagent_schema_excludes_models_without_an_active_credential_lane() {
        let catalog = [
            subagent_entry(
                ProviderId::OpenAiCodex,
                "gpt-authenticated",
                Some(lane(
                    "openai-codex",
                    ProviderVendor::OpenAi,
                    ProviderId::OpenAiCodex,
                    CredentialLaneKind::OAuth,
                )),
            ),
            subagent_entry(ProviderId::Anthropic, "claude-unconfigured", None),
        ];

        let choices = subagent_schema_choices_from(&catalog);

        assert_eq!(choices.models, vec!["gpt-authenticated"]);
        assert_eq!(choices.providers, None);
    }

    #[test]
    fn subagent_provider_schema_is_only_exposed_for_multiple_lanes_per_vendor() {
        let single_lane_vendors = [
            subagent_entry(
                ProviderId::OpenAiCodex,
                "gpt-oauth",
                Some(lane(
                    "openai-codex",
                    ProviderVendor::OpenAi,
                    ProviderId::OpenAiCodex,
                    CredentialLaneKind::OAuth,
                )),
            ),
            subagent_entry(
                ProviderId::Anthropic,
                "claude-oauth",
                Some(lane(
                    "anthropic-oauth",
                    ProviderVendor::Anthropic,
                    ProviderId::Anthropic,
                    CredentialLaneKind::OAuth,
                )),
            ),
        ];
        assert_eq!(
            subagent_schema_choices_from(&single_lane_vendors).providers,
            None
        );

        let multiple_openai_lanes = [
            subagent_entry(
                ProviderId::OpenAiCodex,
                "shared",
                Some(lane(
                    "openai-codex",
                    ProviderVendor::OpenAi,
                    ProviderId::OpenAiCodex,
                    CredentialLaneKind::OAuth,
                )),
            ),
            subagent_entry(
                ProviderId::OpenAi,
                "shared",
                Some(lane(
                    "openai",
                    ProviderVendor::OpenAi,
                    ProviderId::OpenAi,
                    CredentialLaneKind::Api,
                )),
            ),
        ];
        assert_eq!(
            subagent_schema_choices_from(&multiple_openai_lanes).providers,
            Some(vec!["openai-codex".to_string(), "openai".to_string()])
        );
    }

    #[test]
    fn subagent_resolution_prefers_oauth_and_accepts_an_explicit_active_lane() {
        let catalog = [
            subagent_entry(
                ProviderId::OpenAi,
                "shared",
                Some(lane(
                    "openai",
                    ProviderVendor::OpenAi,
                    ProviderId::OpenAi,
                    CredentialLaneKind::Api,
                )),
            ),
            subagent_entry(
                ProviderId::OpenAiCodex,
                "shared",
                Some(lane(
                    "openai-codex",
                    ProviderVendor::OpenAi,
                    ProviderId::OpenAiCodex,
                    CredentialLaneKind::OAuth,
                )),
            ),
        ];

        let preferred = resolve_subagent_model_in(&catalog, "shared", None).unwrap();
        assert_eq!(preferred.lane.unwrap().id, "openai-codex");
        let explicit = resolve_subagent_model_in(&catalog, "shared", Some("openai")).unwrap();
        assert_eq!(explicit.lane.unwrap().id, "openai");
    }
}
