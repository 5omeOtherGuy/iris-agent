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
        match self {
            ProviderId::OpenAiCodex => "OpenAI",
            ProviderId::Anthropic => "Anthropic",
            ProviderId::Antigravity => "Antigravity",
        }
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

/// Prompt-cache retention preference shared by provider adapters. `Short` opts
/// into the provider default ephemeral cache (Anthropic 5-minute `cache_control`
/// / OpenAI `prompt_cache_key`); `Long` opts into the provider's longer-lived
/// cache marker (Anthropic `ttl: "1h"` / OpenAI `prompt_cache_retention:
/// "24h"`); `None` (the default) disables every provider request cache hint so
/// the request stays byte-identical to pre-cache behavior unless a user opts in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptCacheRetention {
    None,
    Short,
    Long,
}

impl PromptCacheRetention {
    /// Default cache retention: `None` (off). Prompt caching is a public
    /// server-side opt-in, so the default emits no cache request fields and
    /// preserves existing request behavior and cost for users who did not
    /// enable it.
    pub(crate) const DEFAULT: PromptCacheRetention = PromptCacheRetention::None;

    #[cfg(test)]
    pub(crate) const ALL: [PromptCacheRetention; 3] = [
        PromptCacheRetention::None,
        PromptCacheRetention::Short,
        PromptCacheRetention::Long,
    ];

    pub(crate) fn parse(value: &str) -> Result<PromptCacheRetention> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(PromptCacheRetention::None),
            "short" => Ok(PromptCacheRetention::Short),
            "long" => Ok(PromptCacheRetention::Long),
            other => Err(UsageError::new(format!(
                "unsupported prompt cache retention '{other}'; supported: none, short, long"
            ))
            .into()),
        }
    }

    #[cfg(test)]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PromptCacheRetention::None => "none",
            PromptCacheRetention::Short => "short",
            PromptCacheRetention::Long => "long",
        }
    }

    pub(crate) fn caching_enabled(self) -> bool {
        self != PromptCacheRetention::None
    }
}

/// Anthropic server-side context-management opt-in (`context_management.edits`),
/// deserialized from the global `anthropicContextManagement` setting. An empty
/// value (the default) is disabled, so no `context_management` is emitted and
/// the request and betas stay byte-identical unless a user explicitly enables
/// an edit. Each present edit maps to a documented Anthropic edit type; the
/// required betas are derived from the emitted payload by the Anthropic adapter.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextManagement {
    /// `clear_tool_uses_20250919`: drop old tool-use/result pairs past a token
    /// trigger, keeping the most recent.
    pub(crate) clear_tool_uses: Option<ClearToolUses>,
    /// `clear_thinking_20251015`: drop extended-thinking blocks from older turns.
    pub(crate) clear_thinking: Option<ClearThinking>,
    /// `compact_20260112`: parsed only so Iris can reject it until the provider
    /// response `compaction` block can be persisted and replayed safely.
    pub(crate) compact: Option<Compact>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClearToolUses {
    pub(crate) trigger_input_tokens: Option<u64>,
    pub(crate) keep_tool_uses: Option<u64>,
    pub(crate) clear_at_least_input_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClearThinking {
    pub(crate) trigger_input_tokens: Option<u64>,
    /// Recent thinking turns to keep. When unset, Iris omits `keep` and lets the
    /// Anthropic beta use its API default rather than sending a Claude-Code-only
    /// `"all"` sentinel.
    pub(crate) keep_thinking_turns: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Compact {
    pub(crate) trigger_input_tokens: Option<u64>,
    pub(crate) instructions: Option<String>,
}

impl ContextManagement {
    /// Whether any supported context-management edit is configured. When false
    /// the adapter emits no `context_management` field and no extra betas.
    /// Compact is intentionally excluded: Iris rejects it until compaction
    /// blocks can be represented in the transcript and replayed on later turns.
    pub(crate) fn is_enabled(&self) -> bool {
        self.clear_tool_uses.is_some() || self.clear_thinking.is_some()
    }

    pub(crate) fn validate_supported(&self) -> Result<()> {
        if self.compact.is_some() {
            return Err(UsageError::new(
                "anthropicContextManagement.compact is not supported yet; compact responses require transcript replay support",
            )
            .into());
        }
        Ok(())
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
    pub(crate) cache_retention: PromptCacheRetention,
    /// Anthropic-only context-management opt-in; empty/default for other
    /// providers and when unconfigured.
    pub(crate) context_management: ContextManagement,
}

impl ModelSelection {
    /// Resolve the selection from settings, centralizing precedence:
    /// - provider: `settings.default_provider` -> `openai-codex`
    /// - model: `IRIS_MODEL` -> `settings.default_model` -> per-provider default
    /// - base_url: provider env (`IRIS_CODEX_BASE_URL` only today) ->
    ///   `settings.base_url` -> per-provider default
    /// - reasoning: `settings.default_reasoning` -> else `None`
    /// - cache retention: `settings.prompt_cache_retention` -> `none` (off)
    /// - context management: `settings.anthropic_context_management` -> empty
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
        let cache_retention = match trimmed_non_empty(settings.prompt_cache_retention.as_deref()) {
            Some(value) => PromptCacheRetention::parse(value)?,
            None => PromptCacheRetention::DEFAULT,
        };
        let context_management = match &settings.anthropic_context_management {
            Some(value) => {
                serde_json::from_value::<ContextManagement>(value.clone()).map_err(|error| {
                    UsageError::new(format!(
                        "invalid anthropicContextManagement config: {error}"
                    ))
                })?
            }
            None => ContextManagement::default(),
        };
        context_management.validate_supported()?;
        Ok(ModelSelection {
            provider,
            model,
            base_url,
            reasoning,
            cache_retention,
            context_management,
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
            prompt_cache_retention: None,
            anthropic_context_management: None,
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
        assert_eq!(resolved.cache_retention, PromptCacheRetention::None);
        assert!(!resolved.context_management.is_enabled());

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
    fn context_management_parses_typed_edits_and_rejects_malformed_or_unsupported() {
        let mut s = settings(None, None, None, None);
        assert!(
            !ModelSelection::resolve(&s)
                .unwrap()
                .context_management
                .is_enabled(),
            "unconfigured context management stays disabled"
        );

        s.anthropic_context_management = Some(serde_json::json!({
            "clearToolUses": { "triggerInputTokens": 100000, "keepToolUses": 3 },
            "clearThinking": { "triggerInputTokens": 90000, "keepThinkingTurns": 2 },
        }));
        let cm = ModelSelection::resolve(&s).unwrap().context_management;
        assert!(cm.is_enabled());
        let clear = cm.clear_tool_uses.expect("clear_tool_uses");
        assert_eq!(clear.trigger_input_tokens, Some(100000));
        assert_eq!(clear.keep_tool_uses, Some(3));
        let thinking = cm.clear_thinking.expect("clear_thinking");
        assert_eq!(thinking.trigger_input_tokens, Some(90000));
        assert_eq!(thinking.keep_thinking_turns, Some(2));
        assert!(cm.compact.is_none());

        s.anthropic_context_management = Some(serde_json::json!({
            "compact": { "triggerInputTokens": 150000, "instructions": "preserve decisions" }
        }));
        let err = ModelSelection::resolve(&s).unwrap_err().to_string();
        assert!(err.contains("compact is not supported yet"), "{err}");

        s.anthropic_context_management = Some(serde_json::json!({ "clearToolUses": 7 }));
        let err = ModelSelection::resolve(&s).unwrap_err().to_string();
        assert!(err.contains("invalid anthropicContextManagement"), "{err}");
    }

    #[test]
    fn cache_retention_parses_defaults_and_rejects_unknown_values() {
        let mut s = settings(None, None, None, None);
        // Default is off: prompt caching is opt-in, so an unconfigured session
        // emits no cache request fields.
        assert_eq!(
            ModelSelection::resolve(&s).unwrap().cache_retention,
            PromptCacheRetention::None
        );

        s.prompt_cache_retention = Some("short".to_string());
        assert_eq!(
            ModelSelection::resolve(&s).unwrap().cache_retention,
            PromptCacheRetention::Short
        );

        s.prompt_cache_retention = Some("none".to_string());
        assert_eq!(
            ModelSelection::resolve(&s).unwrap().cache_retention,
            PromptCacheRetention::None
        );

        s.prompt_cache_retention = Some("long".to_string());
        assert_eq!(
            ModelSelection::resolve(&s).unwrap().cache_retention,
            PromptCacheRetention::Long
        );

        s.prompt_cache_retention = Some("forever".to_string());
        let err = ModelSelection::resolve(&s).unwrap_err().to_string();
        assert!(err.contains("unsupported prompt cache retention"), "{err}");
    }

    #[test]
    fn provider_reasoning_and_cache_retention_parse_round_trip() {
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
        for retention in PromptCacheRetention::ALL {
            assert_eq!(
                PromptCacheRetention::parse(retention.as_str()).unwrap(),
                retention
            );
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
