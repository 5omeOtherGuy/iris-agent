//! Normalized model selection (Tier 3 Mimir): the single place that owns the
//! user's provider + model + reasoning choice and the precedence rules that
//! resolve it.
//!
//! Before this layer each adapter re-resolved its own model/base-url with a
//! private `DEFAULT_MODEL` + `resolve_setting` and a private `IRIS_*` env read.
//! That duplication is gone: [`ModelSelection::resolve`] centralizes the
//! precedence (env > settings file > built-in default) and the per-provider
//! defaults live here, so an adapter just receives the resolved strings plus an
//! optional [`ReasoningEffort`].
//!
//! Conceptually ported from pi-mono's normalized effort enum
//! (`packages/ai/src/types.ts` `ThinkingLevel`) and selection precedence
//! (`packages/coding-agent/src/core/model-resolver.ts`); the fuzzy matching,
//! alias resolution, and generated registry are intentionally not adopted --
//! switching accepts exact ids only.

use std::env;

use anyhow::Result;

use crate::config::Settings;
use crate::errors::UsageError;

/// The providers Iris supports today. Parsing keeps the "unsupported provider"
/// error close to its only authority, so `build_provider` no longer matches on
/// raw strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderId {
    OpenAiCodex,
    Anthropic,
    Antigravity,
}

impl ProviderId {
    /// Provider used when the settings file selects none. Stays `openai-codex`
    /// for backward compatibility.
    pub(crate) const DEFAULT: ProviderId = ProviderId::OpenAiCodex;

    /// Every supported provider, in display/registry order. Used by the model
    /// catalog and the `/login` provider list so a new provider is added in one
    /// place.
    pub(crate) const ALL: [ProviderId; 3] = [
        ProviderId::OpenAiCodex,
        ProviderId::Anthropic,
        ProviderId::Antigravity,
    ];

    /// Human-facing provider name for selectors and status lines. Today this is
    /// just the wire id; kept as a separate accessor so a friendlier label can
    /// be added without touching call sites.
    pub(crate) fn display_name(self) -> &'static str {
        self.as_str()
    }

    /// Parse a provider id string. The error mirrors the message and exit-code
    /// classification (`UsageError`) `build_provider` used to emit, so an
    /// unsupported value still fails loudly with the usage exit code.
    pub(crate) fn parse(value: &str) -> Result<ProviderId> {
        match value.trim() {
            "openai-codex" => Ok(ProviderId::OpenAiCodex),
            "anthropic" => Ok(ProviderId::Anthropic),
            "antigravity" => Ok(ProviderId::Antigravity),
            other => Err(UsageError::new(format!(
                "unsupported provider '{other}'; supported: openai-codex, anthropic, antigravity"
            ))
            .into()),
        }
    }

    /// The wire id, used for display and the recorded `modelSelection` event.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ProviderId::OpenAiCodex => "openai-codex",
            ProviderId::Anthropic => "anthropic",
            ProviderId::Antigravity => "antigravity",
        }
    }

    /// Built-in default model for this provider. Inherited placeholders already
    /// present in the per-adapter constants; centralized here so selection owns
    /// the model default.
    pub(crate) fn default_model(self) -> &'static str {
        match self {
            ProviderId::OpenAiCodex => "gpt-5.5",
            ProviderId::Anthropic => "claude-sonnet-4-6",
            ProviderId::Antigravity => "gemini-3.5-flash",
        }
    }

    /// Built-in default base URL for this provider's API endpoint.
    fn default_base_url(self) -> &'static str {
        match self {
            ProviderId::OpenAiCodex => "https://chatgpt.com/backend-api",
            ProviderId::Anthropic => "https://api.anthropic.com",
            ProviderId::Antigravity => "https://daily-cloudcode-pa.googleapis.com",
        }
    }

    /// Provider-specific base-url env override, if any. Only Codex exposes one
    /// today (`IRIS_CODEX_BASE_URL`); the others have no env override.
    fn base_url_env(self) -> Option<&'static str> {
        match self {
            ProviderId::OpenAiCodex => Some("IRIS_CODEX_BASE_URL"),
            ProviderId::Anthropic | ProviderId::Antigravity => None,
        }
    }
}

/// Normalized reasoning/thinking effort. Mirrors pi-mono's `ThinkingLevel`
/// (`minimal|low|medium|high|xhigh`) plus an explicit `off`. Each adapter maps a
/// `Some(level)` into its own wire shape; `None` means "no preference -> omit
/// all reasoning fields -> today's wire".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReasoningEffort {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

impl ReasoningEffort {
    /// Default thinking/effort level (`medium`), matching pi-mono. Used when a
    /// picker needs a starting level and the session has no explicit preference.
    pub(crate) const DEFAULT: ReasoningEffort = ReasoningEffort::Medium;

    /// Every level, in increasing order. Used to round-trip parsing in tests.
    #[cfg(test)]
    pub(crate) const ALL: [ReasoningEffort; 6] = [
        ReasoningEffort::Off,
        ReasoningEffort::Minimal,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
    ];

    /// Parse a level from a string (case-insensitive). Exact tokens only. A bad
    /// value is a usage/config error (`UsageError`), so a misconfigured
    /// `defaultReasoning` fails at startup with the usage exit code.
    pub(crate) fn parse(value: &str) -> Result<ReasoningEffort> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(ReasoningEffort::Off),
            "minimal" => Ok(ReasoningEffort::Minimal),
            "low" => Ok(ReasoningEffort::Low),
            "medium" => Ok(ReasoningEffort::Medium),
            "high" => Ok(ReasoningEffort::High),
            "xhigh" => Ok(ReasoningEffort::XHigh),
            other => Err(UsageError::new(format!(
                "unsupported reasoning level '{other}'; supported: off, minimal, low, medium, high, xhigh"
            ))
            .into()),
        }
    }

    /// The wire/display token for this level.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Off => "off",
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::XHigh => "xhigh",
        }
    }

    /// Short human description shown in the effort picker, matching pi-mono's
    /// thinking-level descriptions.
    pub(crate) fn description(self) -> &'static str {
        match self {
            ReasoningEffort::Off => "No reasoning",
            ReasoningEffort::Minimal => "Very brief reasoning (~1k tokens)",
            ReasoningEffort::Low => "Light reasoning (~2k tokens)",
            ReasoningEffort::Medium => "Moderate reasoning (~8k tokens)",
            ReasoningEffort::High => "Deep reasoning (~16k tokens)",
            ReasoningEffort::XHigh => "Maximum reasoning (~32k tokens)",
        }
    }
}

/// The resolved user choice: provider + model + base URL + optional reasoning.
/// `reasoning: None` means no preference, so adapters omit every reasoning field
/// and emit byte-identical requests to today's behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelSelection {
    pub(crate) provider: ProviderId,
    pub(crate) model: String,
    pub(crate) base_url: String,
    pub(crate) reasoning: Option<ReasoningEffort>,
}

impl ModelSelection {
    /// Resolve the selection from settings, centralizing precedence:
    /// - provider: `settings.default_provider` -> `openai-codex`
    /// - model: `IRIS_MODEL` -> `settings.default_model` -> per-provider default
    /// - base_url: provider env (`IRIS_CODEX_BASE_URL` only today) ->
    ///   `settings.base_url` -> per-provider default
    /// - reasoning: `settings.default_reasoning` -> else `None`
    ///
    /// `settings.base_url` is already global-only (the security invariant is
    /// enforced in `Settings::merged_with`), so resolve never re-derives it from
    /// untrusted project config.
    pub(crate) fn resolve(settings: &Settings) -> Result<ModelSelection> {
        let provider = match trimmed_non_empty(settings.default_provider.as_deref()) {
            Some(value) => ProviderId::parse(value)?,
            None => ProviderId::DEFAULT,
        };
        let model = non_empty_env("IRIS_MODEL")
            .or_else(|| trimmed_non_empty(settings.default_model.as_deref()).map(str::to_string))
            .unwrap_or_else(|| provider.default_model().to_string());
        let base_url = base_url_for(provider, settings.base_url.as_deref());
        let reasoning = match trimmed_non_empty(settings.default_reasoning.as_deref()) {
            Some(value) => Some(ReasoningEffort::parse(value)?),
            None => None,
        };
        Ok(ModelSelection {
            provider,
            model,
            base_url,
            reasoning,
        })
    }
}

/// Resolve a base URL for a provider with precedence `env > settings > default`.
/// Pass `settings_base_url = None` to ignore the settings value (used on a
/// runtime `/model` provider switch: the configured `base_url` binds to the
/// originally selected provider and must not silently redirect a different one).
pub(crate) fn base_url_for(provider: ProviderId, settings_base_url: Option<&str>) -> String {
    provider
        .base_url_env()
        .and_then(non_empty_env)
        .or_else(|| trimmed_non_empty(settings_base_url).map(str::to_string))
        .unwrap_or_else(|| provider.default_base_url().to_string())
}

/// Trim a settings value and drop it when blank, so an empty `""` falls back to
/// the next precedence layer instead of overriding with an invalid value.
fn trimmed_non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// Read an env var, returning `None` when unset or blank/whitespace-only.
fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build settings with explicit fields; mirrors how `Settings::load`
    /// produces a value without touching disk.
    fn settings(
        provider: Option<&str>,
        model: Option<&str>,
        base_url: Option<&str>,
        reasoning: Option<&str>,
    ) -> Settings {
        Settings {
            default_provider: provider.map(str::to_string),
            default_model: model.map(str::to_string),
            base_url: base_url.map(str::to_string),
            context_token_budget: None,
            default_reasoning: reasoning.map(str::to_string),
            enabled_models: None,
        }
    }

    /// Env vars are process-global; serialize the env-sensitive cases through one
    /// test so concurrent test threads never observe each other's `IRIS_MODEL`.
    #[test]
    fn resolve_precedence_env_over_settings_over_default() {
        // Clean slate: no env overrides -> settings, then defaults.
        unsafe {
            env::remove_var("IRIS_MODEL");
            env::remove_var("IRIS_CODEX_BASE_URL");
        }

        // Defaults when nothing is set.
        let s = settings(None, None, None, None);
        let resolved = ModelSelection::resolve(&s).unwrap();
        assert_eq!(resolved.provider, ProviderId::OpenAiCodex);
        assert_eq!(resolved.model, "gpt-5.5");
        assert_eq!(resolved.base_url, "https://chatgpt.com/backend-api");
        assert_eq!(resolved.reasoning, None);

        // Settings values win over defaults.
        let s = settings(
            Some("anthropic"),
            Some("settings-model"),
            None,
            Some("high"),
        );
        let resolved = ModelSelection::resolve(&s).unwrap();
        assert_eq!(resolved.provider, ProviderId::Anthropic);
        assert_eq!(resolved.model, "settings-model");
        assert_eq!(resolved.base_url, "https://api.anthropic.com");
        assert_eq!(resolved.reasoning, Some(ReasoningEffort::High));

        // Blank settings fall back to the default.
        let s = settings(None, Some("   "), None, None);
        assert_eq!(ModelSelection::resolve(&s).unwrap().model, "gpt-5.5");

        // Env wins over settings (model + codex base url).
        unsafe {
            env::set_var("IRIS_MODEL", "env-model");
            env::set_var("IRIS_CODEX_BASE_URL", "https://env.example");
        }
        let s = settings(Some("openai-codex"), Some("settings-model"), None, None);
        let resolved = ModelSelection::resolve(&s).unwrap();
        assert_eq!(resolved.model, "env-model");
        assert_eq!(resolved.base_url, "https://env.example");
        unsafe {
            env::remove_var("IRIS_MODEL");
            env::remove_var("IRIS_CODEX_BASE_URL");
        }
    }

    #[test]
    fn resolve_rejects_unknown_provider_and_reasoning() {
        let s = settings(Some("bogus"), None, None, None);
        let err = ModelSelection::resolve(&s).unwrap_err().to_string();
        assert!(err.contains("unsupported provider"), "{err}");

        let s = settings(None, None, None, Some("ultra"));
        let err = ModelSelection::resolve(&s).unwrap_err().to_string();
        assert!(err.contains("unsupported reasoning level"), "{err}");
    }

    #[test]
    fn provider_and_reasoning_parse_round_trip() {
        for provider in [
            ProviderId::OpenAiCodex,
            ProviderId::Anthropic,
            ProviderId::Antigravity,
        ] {
            assert_eq!(ProviderId::parse(provider.as_str()).unwrap(), provider);
        }
        for level in ReasoningEffort::ALL {
            assert_eq!(ReasoningEffort::parse(level.as_str()).unwrap(), level);
        }
        // Case-insensitive.
        assert_eq!(
            ReasoningEffort::parse("HIGH").unwrap(),
            ReasoningEffort::High
        );
    }

    #[test]
    fn base_url_for_ignores_settings_on_provider_switch() {
        // Anthropic/Antigravity expose no base-url env, so these cases are
        // env-independent (no IRIS_CODEX_BASE_URL interaction). With
        // settings_base_url=None (the provider-switch case) only the per-provider
        // default applies, never a prior provider's configured base url.
        assert_eq!(
            base_url_for(ProviderId::Anthropic, None),
            "https://api.anthropic.com"
        );
        assert_eq!(
            base_url_for(ProviderId::Antigravity, Some("https://stale.example")),
            "https://stale.example",
            "explicit settings url is honored when provided"
        );
    }
}
